//! Running commands inside an env's image: tool discovery, binds, GPUs, shims, activation.

use crate::config::{self, exe_path};
use crate::env::{Backend, Env, Kind, SHIM_LINK};
use crate::util::{expand, is_executable, is_under, q, qp, realpath, warn, which, write_atomic};
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where things live inside every image.
pub const IMG_ROOT: &str = "/opt/cvenv";
pub const IMG_PIXI: &str = "/opt/cvenv/bin/pixi";
pub const IMG_ENVS: &str = "/opt/cvenv/envs";
/// `<env>/.cvenv/inner-bin`: symlinks to the real executables inside the image. Bound over
/// the shim directory inside the container, so `<env>/.cvenv/bin/python` is the shim on the
/// host and the real interpreter in the container - `sys.executable` is valid on both sides.
pub const LAYER_DIR: &str = "inner-bin";
/// `<env>/.cvenv/install.sqfs`: the squashed tree of a wrap-install env.
pub const INSTALL_SQFS: &str = "install.sqfs";

/// Host variables removed before entering the container: they would point the
/// containerised interpreter at host installations.
pub const HOST_ENV_DROP: &[&str] = &[
    "PYTHONHOME",
    "VIRTUAL_ENV",
    "CONDA_PREFIX",
    "CONDA_DEFAULT_ENV",
    "CONDA_SHLVL",
    "CONDA_PROMPT_MODIFIER",
    "PIXI_PROJECT_ROOT",
    "PIXI_PROJECT_MANIFEST",
    "PIXI_ENVIRONMENT_NAME",
    "PIXI_IN_SHELL",
];

/// Executables that are not shimmed by default: launching a container for these
/// would only slow down or confuse the host shell.
const DEFAULT_SHIM_EXCLUDE: &[&str] = &[
    "clear",
    "reset",
    "tput",
    "tabs",
    "tset",
    "tic",
    "toe",
    "infocmp",
    "captoinfo",
    "infotocap",
    "ncurses*-config",
    "ncursesw*-config",
    "xz*",
    "unxz",
    "lz*",
    "unlz*",
    "bz*",
    "bunzip2",
    "zstd*",
    "unzstd",
    "pzstd",
    "gzip",
    "gunzip",
    "zcat",
    "zcmp",
    "zdiff",
    "zgrep",
    "zless",
    "zmore",
    "znew",
    "c_rehash",
    "pixi",
    "cvenv",
    "activate",
    "deactivate",
    "conda",
    "mamba",
    "micromamba",
];

// ---------------------------------------------------------------------------
// tool discovery
// ---------------------------------------------------------------------------

fn is_shim(p: &Path) -> bool {
    p.parent()
        .map(|d| d.join(SHIM_LINK).exists())
        .unwrap_or(false)
}

pub fn find_apptainer() -> Result<PathBuf> {
    if let Some(v) = config::value("apptainer") {
        let p = expand(&v);
        if is_executable(&p) {
            return Ok(p);
        }
        warn(&format!(
            "configured apptainer {} is not executable; searching PATH",
            p.display()
        ));
    }
    for n in ["apptainer", "singularity"] {
        if let Some(p) = which(n) {
            return Ok(p);
        }
    }
    bail!(
        "apptainer not found. Load it (e.g. `module load apptainer`) or run \
         `cvenv config set apptainer /path/to/apptainer` (installation.sh does this)"
    )
}

/// The backend a build uses: `--backend` > `backend` setting > the env's current backend >
/// auto-detection (Apptainer if installed, otherwise squashfuse).
pub fn select_backend(
    env: Option<&Env>,
    requested: Option<Backend>,
    kind: Kind,
) -> Result<Backend> {
    if kind == Kind::Wrap {
        if requested == Some(Backend::Squashfuse) {
            bail!("`cvenv wrap` runs existing container images and needs Apptainer; the squashfuse backend can't run them");
        }
        return Ok(Backend::Apptainer);
    }
    if let Some(b) = requested {
        return Ok(b);
    }
    if let Some(v) = config::value("backend") {
        match v.trim().to_lowercase().as_str() {
            "" | "auto" => {}
            other => {
                return Backend::parse(other).ok_or_else(|| {
                    anyhow::anyhow!(
                        "invalid `backend` setting '{}' (auto, apptainer or squashfuse)",
                        other
                    )
                })
            }
        }
    }
    if let Some(e) = env {
        if e.built() {
            return Ok(e.backend());
        }
    }
    if find_apptainer().is_ok() {
        return Ok(Backend::Apptainer);
    }
    if crate::squash::find_squashfuse().is_ok() {
        return Ok(Backend::Squashfuse);
    }
    bail!(
        "neither apptainer nor squashfuse was found. Install one of them (installation.sh can fetch \
         squashfuse on Debian/Ubuntu), or point cvenv at it: `cvenv config set apptainer PATH` / \
         `cvenv config set squashfuse PATH`"
    )
}

pub fn find_pixi() -> Result<PathBuf> {
    let mut cands = vec![];
    if let Some(v) = config::value("pixi") {
        cands.push(expand(&v));
    }
    if let Some(p) = which("pixi") {
        cands.push(p);
    }
    let pixi_home = std::env::var("PIXI_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| crate::util::home().join(".pixi"));
    cands.push(pixi_home.join("bin").join("pixi"));
    for c in cands {
        if is_executable(&c) && !is_shim(&c) {
            return Ok(realpath(&c));
        }
    }
    bail!(
        "pixi not found. Install it (installation.sh can) or `cvenv config set pixi /path/to/pixi`"
    )
}

pub fn pixi_version(pixi: &Path) -> Option<String> {
    let out = Command::new(pixi).arg("--version").output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    s.split_whitespace()
        .find(|w| {
            w.chars()
                .next()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false)
        })
        .map(|w| w.to_string())
}

fn apptainer_conf(app: &Path) -> Option<PathBuf> {
    if let Some(v) = config::value("apptainer-conf") {
        let p = expand(&v);
        if p.is_file() {
            return Some(p);
        }
    }
    let real = realpath(app);
    let base = real.parent()?.parent()?.to_path_buf();
    let arch = std::env::consts::ARCH;
    [
        base.join("etc/apptainer/apptainer.conf"),
        base.join(arch).join("etc/apptainer/apptainer.conf"),
        PathBuf::from("/etc/apptainer/apptainer.conf"),
        PathBuf::from("/usr/local/etc/apptainer/apptainer.conf"),
        PathBuf::from("/etc/singularity/singularity.conf"),
        PathBuf::from("/usr/local/etc/singularity/singularity.conf"),
    ]
    .into_iter()
    .find(|c| c.is_file())
}

/// Directories apptainer already binds on this machine.
fn site_bind_paths(app: &Path) -> Vec<PathBuf> {
    let mut v = vec![
        crate::util::home(),
        PathBuf::from("/tmp"),
        PathBuf::from("/var/tmp"),
    ];
    if let Ok(cwd) = std::env::current_dir() {
        v.push(cwd);
    }
    if let Some(conf) = apptainer_conf(app) {
        if let Ok(s) = fs::read_to_string(conf) {
            for line in s.lines() {
                let l = line.trim();
                if let Some(rest) = l.strip_prefix("bind path") {
                    if let Some(val) = rest.trim().strip_prefix('=') {
                        let src = val.trim().split(':').next().unwrap_or("").to_string();
                        if !src.is_empty() {
                            v.push(PathBuf::from(src));
                        }
                    }
                }
            }
        }
    }
    v
}

// ---------------------------------------------------------------------------
// container command line
// ---------------------------------------------------------------------------

fn truthy(v: &str) -> bool {
    matches!(
        v.trim().to_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn falsy(v: &str) -> bool {
    matches!(
        v.trim().to_lowercase().as_str(),
        "0" | "false" | "no" | "off" | "none" | ""
    )
}

pub fn gpu_flags(env: &Env) -> Vec<String> {
    let v = std::env::var("CVENV_GPU").ok().unwrap_or_else(|| {
        env.cfg()
            .get("gpu")
            .map(crate::env::toml_to_string)
            .unwrap_or_else(|| "auto".into())
    });
    let v = v.trim().to_lowercase();
    if ["nv", "nvidia", "cuda"].contains(&v.as_str()) || truthy(&v) {
        return vec!["--nv".into()];
    }
    if v == "rocm" || v == "amd" {
        return vec!["--rocm".into()];
    }
    if falsy(&v) {
        return vec![];
    }
    let mut flags = vec![];
    let has_nvidia = fs::read_dir("/dev")
        .map(|rd| {
            rd.filter_map(|e| e.ok()).any(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                n.strip_prefix("nvidia")
                    .map(|r| {
                        r.chars()
                            .next()
                            .map(|c| c.is_ascii_digit())
                            .unwrap_or(false)
                    })
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    if has_nvidia {
        flags.push("--nv".into());
    }
    if Path::new("/dev/kfd").exists() {
        flags.push("--rocm".into());
    }
    flags
}

pub struct ContainerCmd {
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
}

pub fn container_command(env: &Env, args: &[String], mode: &str) -> Result<ContainerCmd> {
    let app = find_apptainer()?;
    let cfg = env.cfg();
    let mut argv = vec![app.to_string_lossy().to_string(), mode.to_string()];
    argv.extend(gpu_flags(env));

    let covered = site_bind_paths(&app);
    let mut binds = vec![];
    let mdir = env.manifest_dir();
    if !covered.iter().any(|p| is_under(&mdir, p)) {
        binds.push(mdir.to_string_lossy().to_string());
    }
    let mut extra = env.cfg_str_list("binds");
    if let Ok(b) = std::env::var("CVENV_BINDS") {
        extra.extend(
            b.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
        );
    }
    for b in extra {
        let (src, rest) = match b.split_once(':') {
            Some((s, r)) => (s.to_string(), Some(r.to_string())),
            None => (b.clone(), None),
        };
        let src = expand(&src);
        if !src.exists() {
            warn(&format!(
                "bind source {} does not exist; skipping",
                src.display()
            ));
            continue;
        }
        binds.push(match rest {
            Some(r) => format!("{}:{}", src.display(), r),
            None => src.to_string_lossy().to_string(),
        });
    }
    // host-valid paths / wrapped installation mounts
    let meta = env.meta().unwrap_or_default();
    let mut prepend: Vec<String> = vec![];
    let mut cmd_path: Option<String> = None;
    let first = args.first().filter(|a| !a.contains('/')).cloned();
    if env.kind == Kind::WrapInstall {
        if let Some(ip) = &meta.install_path {
            binds.push(format!(
                // `ro`: the image is read-only anyway; without it Apptainer tries to open
                // it writable and warns on every command
                "{}:{}:ro,image-src=/",
                env.art().join(INSTALL_SQFS).display(),
                ip.display()
            ));
            prepend.extend(
                meta.bin_dirs
                    .iter()
                    .map(|d| ip.join(d).display().to_string()),
            );
            if meta.conda {
                argv.push("--env".into());
                argv.push(format!("CONDA_PREFIX={}", ip.display()));
            }
            cmd_path = first.as_ref().and_then(|f| meta.exec_paths.get(f).cloned());
        }
    } else {
        let layer = env.art().join(LAYER_DIR);
        let enabled = cfg
            .get("host-valid-paths")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        if enabled && layer.is_dir() && !meta.exec_paths.is_empty() {
            binds.push(format!("{}:{}", layer.display(), env.shim_dir().display()));
            prepend.push(env.shim_dir().display().to_string());
            cmd_path = first
                .as_ref()
                .filter(|f| meta.exec_paths.contains_key(*f))
                .map(|f| env.shim_dir().join(f).display().to_string());
        }
    }
    for b in binds {
        argv.push("--bind".into());
        argv.push(b);
    }
    if let Some(t) = cfg.get("env").and_then(|v| v.as_table()) {
        for (k, v) in t {
            argv.push("--env".into());
            argv.push(format!("{}={}", k, crate::env::toml_to_string(v)));
        }
    }
    argv.extend(env.cfg_str_list("apptainer-args"));
    if let Ok(a) = std::env::var("CVENV_APPTAINER_ARGS") {
        argv.extend(a.split_whitespace().map(String::from));
    }
    if mode == "shell" {
        argv.push("--env".into());
        argv.push(format!("PS1=({}) [container] \\w \\$ ", env.name));
    }
    argv.push(env.image().to_string_lossy().to_string());
    let mut args: Vec<String> = args.to_vec();
    if let (Some(p), false) = (&cmd_path, args.is_empty()) {
        args[0] = p.clone(); // run via the host-valid path so sys.executable etc. are valid on the host
    }
    if env.kind == Kind::WrapInstall && meta.conda && !args.is_empty() {
        if let Some(ip) = &meta.install_path {
            // conda envs: run the packages' activate.d scripts like `conda activate` would
            let mut wrapped = vec![
                "/bin/bash".to_string(),
                "-c".into(),
                format!(
                    "for f in {}/etc/conda/activate.d/*.sh; do [ -f \"$f\" ] && . \"$f\"; done; exec \"$@\"",
                    q(&ip.display().to_string())
                ),
                "cvenv-activate".into(),
            ];
            wrapped.extend(args);
            args = wrapped;
        }
    }
    argv.extend(args);

    let mut penv: BTreeMap<String, String> = std::env::vars().collect();
    for k in HOST_ENV_DROP {
        penv.remove(*k);
    }
    penv.insert("CVENV_ENV".into(), env.name.clone());
    penv.insert(
        "CVENV_IMAGE".into(),
        env.image().to_string_lossy().to_string(),
    );
    let isolate = cfg
        .get("isolate-user-site")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    if isolate {
        // don't let ~/.local/lib/pythonX.Y/site-packages leak into the env
        penv.insert("PYTHONNOUSERSITE".into(), "1".into());
    }
    // pip can't write to the read-only image; never let it fall back to ~/.local
    penv.entry("PIP_USER".into()).or_insert_with(|| "0".into());
    if !prepend.is_empty() {
        if let Some(user) = penv
            .get("APPTAINERENV_PREPEND_PATH")
            .filter(|v| !v.is_empty())
        {
            prepend.push(user.clone());
        }
        penv.insert("APPTAINERENV_PREPEND_PATH".into(), prepend.join(":"));
    }
    Ok(ContainerCmd { argv, env: penv })
}

/// Replace the current process with the command running in the env: `apptainer <mode>
/// ... image args`, or (squashfuse backend) the command itself from the mounted image.
pub fn exec_in_container(env: &Env, args: &[String], mode: &str) -> Result<()> {
    if !env.built() {
        bail!(
            "env '{}' has not been built yet - run `{}`",
            env.name,
            env.rebuild_hint()
        );
    }
    if env.backend() == Backend::Squashfuse {
        return crate::squash::exec(env, args, mode);
    }
    let cc = container_command(env, args, mode)?;
    if std::env::var_os("CVENV_DEBUG").is_some() {
        crate::util::info(&format!(
            "exec: {}",
            cc.argv.iter().map(|a| q(a)).collect::<Vec<_>>().join(" ")
        ));
    }
    let err = Command::new(&cc.argv[0])
        .args(&cc.argv[1..])
        .env_clear()
        .envs(&cc.env)
        .exec();
    Err(err).with_context(|| format!("failed to run {}", cc.argv[0]))
}

// ---------------------------------------------------------------------------
// shims
// ---------------------------------------------------------------------------

fn shim_excluded(name: &str, env: &Env) -> bool {
    let matches = |pats: &[String]| {
        pats.iter().any(|p| {
            glob::Pattern::new(p)
                .map(|g| g.matches(name))
                .unwrap_or(false)
        })
    };
    if matches(&env.cfg_str_list("shim-include")) {
        return false;
    }
    let mut ex: Vec<String> = DEFAULT_SHIM_EXCLUDE.iter().map(|s| s.to_string()).collect();
    ex.extend(env.cfg_str_list("shim-exclude"));
    matches(&ex)
}

/// `<shim_dir>/.cvenv-shim` -> the cvenv binary. Every shim is a relative symlink
/// to it, so re-pointing this one link updates them all.
pub fn write_shim_link(env: &Env) -> Result<()> {
    let d = env.shim_dir();
    fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
    let link = d.join(SHIM_LINK);
    let exe = exe_path();
    if fs::read_link(&link).map(|t| t == exe).unwrap_or(false) {
        return Ok(());
    }
    let tmp = d.join(format!(".{}.tmp", SHIM_LINK));
    let _ = fs::remove_file(&tmp);
    std::os::unix::fs::symlink(&exe, &tmp)?;
    fs::rename(&tmp, &link)?;
    Ok(())
}

pub fn write_activate_script(env: &Env) -> Result<()> {
    let p = env.art().join("activate");
    let body = format!(
        "# Source this file to activate cvenv env '{name}' (like a venv):\n\
         #     source {path}\n\
         eval \"$({exe} activate {qname})\"\n",
        name = env.name,
        path = p.display(),
        exe = qp(&exe_path()),
        qname = qp(&env.path)
    );
    write_atomic(&p, body.as_bytes(), Some(0o644))
}

/// (Re)create the host-valid path layer: `<art>/inner-bin/NAME -> <path inside the image>`.
pub fn write_layer(env: &Env, exec_paths: &BTreeMap<String, String>) -> Result<()> {
    let d = env.art().join(LAYER_DIR);
    let tmp = env.art().join(format!(".{}.tmp", LAYER_DIR));
    crate::util::remove_tree(&tmp);
    fs::create_dir_all(&tmp)?;
    for (name, target) in exec_paths {
        if !name.is_empty() && !name.starts_with('.') && !name.contains('/') {
            std::os::unix::fs::symlink(target, tmp.join(name))?;
        }
    }
    crate::util::remove_tree(&d);
    fs::rename(&tmp, &d)?;
    Ok(())
}

/// Recreate the shim symlinks for `bins`; returns how many were created.
pub fn write_shims(env: &Env, bins: &[String]) -> Result<usize> {
    write_shim_link(env)?;
    let d = env.shim_dir();
    for e in fs::read_dir(&d)? {
        let e = e?;
        let n = e.file_name().to_string_lossy().to_string();
        if n != SHIM_LINK && e.file_type()?.is_symlink() {
            fs::remove_file(e.path())?;
        }
    }
    // pip guards: while an env is active `pip` must never fall through to a host
    // pip (which would install into ~/.local - the file explosion cvenv avoids).
    let mut all: Vec<String> = bins.to_vec();
    all.push("pip".into());
    all.push("pip3".into());
    all.sort();
    all.dedup();
    let mut n = 0;
    for b in all {
        if b.is_empty() || b.starts_with('.') || b.contains('/') || shim_excluded(&b, env) {
            continue;
        }
        std::os::unix::fs::symlink(SHIM_LINK, d.join(&b))?;
        n += 1;
    }
    write_activate_script(env)?;
    Ok(n)
}

/// If `argv0` is a shim (a symlink in a shim directory), return (env, command name).
pub fn shim_target(argv0: &str) -> Option<(PathBuf, String)> {
    let name = Path::new(argv0).file_name()?.to_string_lossy().to_string();
    if name == "cvenv" || name == SHIM_LINK {
        return None;
    }
    let path = if argv0.contains('/') {
        crate::util::absolute(Path::new(argv0))
    } else {
        // invoked by bare name (e.g. `exec -a python`): look it up on PATH
        let p = std::env::var_os("PATH")?;
        std::env::split_paths(&p)
            .map(|d| d.join(&name))
            .find(|c| is_shim(c))?
    };
    let dir = path.parent()?.to_path_buf();
    if dir.join(SHIM_LINK).exists() {
        Some((dir, name))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// shell integration
// ---------------------------------------------------------------------------

fn path_without(path: &str, remove: &[PathBuf]) -> String {
    path.split(':')
        .filter(|p| !p.is_empty())
        .filter(|p| {
            !remove
                .iter()
                .any(|r| crate::util::normalize(Path::new(p)) == crate::util::normalize(r))
        })
        .collect::<Vec<_>>()
        .join(":")
}

const RESTORE_PS1: &str =
    "if [ -n \"${_CVENV_OLD_PS1+x}\" ]; then PS1=\"$_CVENV_OLD_PS1\"; unset _CVENV_OLD_PS1; fi";

pub fn activation_code(env: &Env) -> String {
    let mut remove = vec![env.shim_dir()];
    if let Ok(old) = std::env::var("CVENV_SHIM_DIR") {
        remove.push(PathBuf::from(old));
    }
    let path = path_without(&std::env::var("PATH").unwrap_or_default(), &remove);
    let new_path = if path.is_empty() {
        env.shim_dir().to_string_lossy().to_string()
    } else {
        format!("{}:{}", env.shim_dir().display(), path)
    };
    let mut lines = vec![
        RESTORE_PS1.to_string(),
        format!("export PATH={}", q(&new_path)),
        format!("export CVENV_ACTIVE={}", q(&env.name)),
        format!("export CVENV_ACTIVE_DIR={}", qp(&env.path)),
        format!("export CVENV_SHIM_DIR={}", qp(&env.shim_dir())),
        format!("export CVENV_MANIFEST={}", qp(&env.manifest)),
    ];
    if std::env::var_os("CVENV_NO_PROMPT").is_none() {
        lines.push(format!(
            "_CVENV_OLD_PS1=\"${{PS1-}}\"; PS1={}\"${{PS1-}}\"",
            q(&format!("({}) ", env.name))
        ));
    }
    lines.push("hash -r 2>/dev/null || true".into());
    lines.join("\n") + "\n"
}

pub fn deactivation_code() -> String {
    let mut remove = vec![];
    if let Ok(old) = std::env::var("CVENV_SHIM_DIR") {
        remove.push(PathBuf::from(old));
    }
    let path = path_without(&std::env::var("PATH").unwrap_or_default(), &remove);
    [
        RESTORE_PS1.to_string(),
        format!("export PATH={}", q(&path)),
        "unset CVENV_ACTIVE CVENV_ACTIVE_DIR CVENV_SHIM_DIR CVENV_MANIFEST".into(),
        "hash -r 2>/dev/null || true".into(),
    ]
    .join("\n")
        + "\n"
}

pub fn shell_init() -> String {
    let exe = qp(&exe_path());
    format!(
        r#"# cvenv shell integration (generated by `cvenv shell-init`)
cvenv() {{
    local __cvenv_code __cvenv_rc
    case "${{1-}}" in
        activate|deactivate)
            __cvenv_code="$(command {exe} "$@")" || return $?
            eval "$__cvenv_code"
            ;;
        move|mv)
            command {exe} "$@" || return $?
            if [ -n "${{CVENV_ACTIVE-}}" ]; then
                __cvenv_code="$(command {exe} activate "$CVENV_ACTIVE" 2>/dev/null)" && eval "$__cvenv_code"
            fi
            ;;
        *)
            command {exe} "$@"
            __cvenv_rc=$?
            hash -r 2>/dev/null
            return $__cvenv_rc
            ;;
    esac
}}
"#,
        exe = exe
    )
}
