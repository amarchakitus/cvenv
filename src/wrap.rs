//! Wrapping things cvenv did not build:
//! - `cvenv wrap NAME IMAGE`: an existing container (local .sif or docker://, oras://, ...)
//! - `cvenv wrap-install NAME DIR`: an existing on-disk installation (e.g. a conda env with
//!   100k files), squashed into one file and mounted back at its original path.

use crate::build::{build_tmp_root, parse_base_image};
use crate::config;
use crate::env::{
    load_registry, update_registry, valid_name, Backend, Env, Kind, Meta, Record, WRAP_CFG,
};
use crate::runtime::{
    find_apptainer, find_pixi, pixi_version, select_backend, write_layer, write_shims,
    HOST_ENV_DROP, INSTALL_SQFS,
};
use crate::util::{
    expand, human_size, info, is_executable, is_under, lock_file, now_iso, realpath, remove_tree,
    warn, which, write_atomic, write_json,
};
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

/// Container directories whose commands would shadow the host's basic tools; from these
/// only interpreter/launcher-like names are exposed unless asked for explicitly.
const SYSTEM_DIRS: &[&str] = &["/usr/bin", "/bin", "/usr/sbin", "/sbin"];
const SYSTEM_ALLOW: &[&str] = &[
    "python",
    "python3",
    "python3.*",
    "pip",
    "pip3",
    "pip3.*",
    "ipython*",
    "jupyter*",
    "torchrun",
    "accelerate",
    "deepspeed",
    "huggingface-cli",
    "nvidia-smi",
    "nvcc",
    "Rscript",
    "R",
    "julia",
];
/// Marker written into a replaced installation directory (see `wrap-install --replace`).
const STUB_MARKER: &str = ".cvenv-wrapped";

const WRAP_CFG_TEMPLATE: &str = "\
# cvenv settings for this wrapped env (all optional; runtime only, no rebuild needed)
# gpu = \"auto\"                       # auto | true | false | rocm
# binds = [\"/data\", \"/in:/mnt:ro\"]   # extra bind mounts
# env = { OMP_NUM_THREADS = \"8\" }    # extra environment variables
# apptainer-args = []
# isolate-user-site = true            # PYTHONNOUSERSITE=1
# host-valid-paths = true             # sys.executable valid on the host too
# shim-exclude = []                   # glob patterns of commands not to expose
# shim-include = []
";

fn is_remote(r: &str) -> bool {
    [
        "docker://",
        "docker-daemon://",
        "oras://",
        "library://",
        "shub://",
        "http://",
        "https://",
    ]
    .iter()
    .any(|p| r.starts_with(p))
}

/// Run apptainer with all temporary/cache files on node-local scratch.
fn apptainer_cmd(app: &Path) -> Result<Command> {
    let tmp = build_tmp_root()?;
    let work = tmp.join(format!("pull-{}", std::process::id()));
    fs::create_dir_all(&work)?;
    let mut c = Command::new(app);
    c.env("APPTAINER_TMPDIR", &work)
        .env("SINGULARITY_TMPDIR", &work)
        .env("TMPDIR", &work)
        .env(
            "APPTAINER_CACHEDIR",
            std::env::var("APPTAINER_CACHEDIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| tmp.join("apptainer-cache")),
        );
    for k in HOST_ENV_DROP {
        c.env_remove(k);
    }
    Ok(c)
}

fn cleanup_pull_dir() {
    if let Ok(tmp) = build_tmp_root() {
        remove_tree(&tmp.join(format!("pull-{}", std::process::id())));
    }
}

/// Put `source` at `dest`: pull a remote image, copy a local one, or symlink it (`link`).
fn fetch_image(source: &str, dest: &Path, link: bool) -> Result<()> {
    let partial = PathBuf::from(format!("{}.partial", dest.display()));
    let _ = fs::remove_file(&partial);
    if is_remote(source) {
        if link {
            bail!("--link only works with local image files");
        }
        let app = find_apptainer()?;
        info(&format!("pulling {}", source));
        let r = apptainer_cmd(&app)?
            .arg("pull")
            .arg("--force")
            .arg(&partial)
            .arg(source)
            .status();
        cleanup_pull_dir();
        if !r?.success() {
            let _ = fs::remove_file(&partial);
            bail!("`apptainer pull {}` failed", source);
        }
    } else {
        let src = realpath(&expand(
            source.strip_prefix("localimage://").unwrap_or(source),
        ));
        if !src.is_file() {
            bail!("image not found: {}", src.display());
        }
        if link {
            let _ = fs::remove_file(dest);
            std::os::unix::fs::symlink(&src, &partial)?;
        } else {
            info(&format!("copying {}", src.display()));
            fs::copy(&src, &partial).with_context(|| format!("copying {}", src.display()))?;
        }
    }
    fs::rename(&partial, dest)?;
    Ok(())
}

/// Executables visible in the container: name -> absolute path inside the container.
fn scan_container(
    image: &Path,
    bin_dirs: &[String],
    commands: &[String],
) -> Result<BTreeMap<String, String>> {
    let app = find_apptainer()?;
    let dirs = if bin_dirs.is_empty() {
        "$PATH".to_string()
    } else {
        bin_dirs.join(":")
    };
    let script = format!(
        "IFS=:; for d in {dirs}; do [ -d \"$d\" ] || continue; for f in \"$d\"/*; do \
         [ -f \"$f\" ] && [ -x \"$f\" ] && printf '%s\\t%s\\n' \"$d\" \"${{f##*/}}\"; done; done; \
         for c in {cmds}; do p=$(command -v \"$c\" 2>/dev/null) && printf '@\\t%s\\t%s\\n' \"$c\" \"$p\"; done; true",
        dirs = dirs,
        cmds = commands.iter().map(|c| crate::util::q(c)).collect::<Vec<_>>().join(" ")
    );
    let out = apptainer_cmd(&app)?
        .arg("exec")
        .arg(image)
        .arg("/bin/sh")
        .arg("-c")
        .arg(&script)
        .stderr(Stdio::inherit())
        .output()
        .context("listing executables in the container")?;
    cleanup_pull_dir();
    if !out.status.success() {
        bail!("could not run the container to list its executables");
    }
    let allow = |n: &str| {
        SYSTEM_ALLOW
            .iter()
            .any(|p| glob::Pattern::new(p).map(|g| g.matches(n)).unwrap_or(false))
    };
    let mut map = BTreeMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        match parts.as_slice() {
            ["@", name, path] => {
                map.insert(name.to_string(), path.to_string());
            }
            [dir, name] => {
                let system = bin_dirs.is_empty() && SYSTEM_DIRS.contains(dir);
                if (!system || allow(name)) && !map.contains_key(*name) {
                    map.insert(
                        name.to_string(),
                        format!("{}/{}", dir.trim_end_matches('/'), name),
                    );
                }
            }
            _ => {}
        }
    }
    Ok(map)
}

fn new_env_dir(name: &str, path: Option<String>) -> Result<PathBuf> {
    if !valid_name(name) {
        bail!(
            "invalid env name '{}' (use letters, digits, '.', '_' and '-')",
            name
        );
    }
    let reg = load_registry();
    if let Some(r) = reg.envs.get(name) {
        bail!("env '{}' already exists at {}", name, r.path.display());
    }
    let dir = path
        .map(|p| expand(&p))
        .unwrap_or_else(|| config::store_dir().join(name));
    for (n, r) in &reg.envs {
        if crate::util::normalize(&r.path) == crate::util::normalize(&dir) {
            bail!("{} is already used by env '{}'", dir.display(), n);
        }
    }
    fs::create_dir_all(dir.join(crate::env::ART_DIR))?;
    Ok(dir)
}

fn register(env: &Env) -> Result<()> {
    let mut rec = env.record();
    rec.created = Some(now_iso());
    update_registry(|r| {
        r.envs.insert(env.name.clone(), rec);
        Ok(())
    })?;
    env.write_env_file()
}

// ---------------------------------------------------------------------------
// cvenv wrap
// ---------------------------------------------------------------------------

pub struct WrapOpts {
    pub link: bool,
    pub bin_dirs: Vec<String>,
    pub commands: Vec<String>,
}

pub fn cmd_wrap(name: &str, source: &str, path: Option<String>, opts: WrapOpts) -> Result<()> {
    let dir = new_env_dir(name, path)?;
    let cfg_file = dir.join(WRAP_CFG);
    if !cfg_file.exists() {
        fs::write(&cfg_file, WRAP_CFG_TEMPLATE)?;
    }
    let env = Env::new(
        name,
        &Record {
            path: dir.clone(),
            manifest: cfg_file,
            kind: Kind::Wrap,
            pixi_env: "default".into(),
            created: None,
        },
    );
    // keep the registry clean if pulling/scanning fails
    if let Err(e) = wrap_image(&env, source, &opts) {
        warn(&format!(
            "wrapping failed; nothing was registered ({} can be removed)",
            dir.display()
        ));
        return Err(e);
    }
    register(&env)?;
    println!(
        "Next steps:\n    cvenv activate {n}          # or: cvenv run --env {n} CMD\n    edit {cfg}   # optional runtime settings",
        n = name,
        cfg = env.manifest.display()
    );
    Ok(())
}

fn wrap_image(env: &Env, source: &str, opts: &WrapOpts) -> Result<()> {
    let _lock = lock_file(
        &env.art().join(".build.lock"),
        false,
        &format!("env '{}'", env.name),
    )?;
    let t0 = Instant::now();
    fetch_image(source, &env.image(), opts.link)?;
    let exec_paths = scan_container(&env.image(), &opts.bin_dirs, &opts.commands)?;
    if exec_paths.is_empty() {
        warn("found no executables to expose; use --bin-dir DIR or --command NAME");
    }
    let bins: Vec<String> = exec_paths.keys().cloned().collect();
    let meta = Meta {
        kind: Kind::Wrap,
        name: env.name.clone(),
        manifest: env.manifest.clone(),
        pixi_env: env.pixi_env.clone(),
        built_at: now_iso(),
        build_seconds: (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0,
        base_image: source.to_string(),
        source: Some(source.to_string()),
        apptainer: find_apptainer()?.display().to_string(),
        cvenv_version: env!("CARGO_PKG_VERSION").into(),
        image_size: fs::metadata(env.image())?.len(),
        bins: bins.clone(),
        exec_paths: exec_paths.clone(),
        bin_dirs: opts.bin_dirs.clone(),
        commands: opts.commands.clone(),
        link: opts.link,
        ..Default::default()
    };
    write_json(&env.meta_path(), &meta)?;
    write_layer(env, &exec_paths)?;
    let n = write_shims(env, &bins)?;
    info(&format!(
        "wrapped {} as '{}' ({}{}, {} commands exposed: {})",
        source,
        env.name,
        human_size(meta.image_size),
        if opts.link { ", linked" } else { "" },
        n,
        preview(&exposed(env, &bins))
    ));
    Ok(())
}

/// The commands that actually got a shim (some names are excluded by default).
fn exposed(env: &Env, bins: &[String]) -> Vec<String> {
    bins.iter()
        .filter(|b| env.shim_dir().join(b.as_str()).exists())
        .cloned()
        .collect()
}

fn preview(bins: &[String]) -> String {
    let mut v: Vec<&str> = bins.iter().map(String::as_str).take(12).collect();
    if bins.len() > 12 {
        v.push("...");
    }
    v.join(" ")
}

/// `cvenv rebuild` for a wrapped container: re-pull/re-copy the source and rescan.
pub fn refresh_wrap(env: &Env) -> Result<()> {
    let meta = env
        .meta()
        .ok_or_else(|| anyhow::anyhow!("no metadata for wrapped env '{}'", env.name))?;
    let source = meta
        .source
        .clone()
        .ok_or_else(|| anyhow::anyhow!("wrapped env '{}' has no recorded source", env.name))?;
    info(&format!(
        "refreshing wrapped env '{}' from {}",
        env.name, source
    ));
    wrap_image(
        env,
        &source,
        &WrapOpts {
            link: meta.link,
            bin_dirs: meta.bin_dirs.clone(),
            commands: meta.commands.clone(),
        },
    )?;
    env.write_env_file()
}

// ---------------------------------------------------------------------------
// cvenv wrap-install
// ---------------------------------------------------------------------------

pub fn find_mksquashfs() -> Result<PathBuf> {
    if let Some(p) = which("mksquashfs") {
        return Ok(p);
    }
    for c in [
        "/usr/sbin/mksquashfs",
        "/sbin/mksquashfs",
        "/usr/local/sbin/mksquashfs",
    ] {
        if is_executable(Path::new(c)) {
            return Ok(PathBuf::from(c));
        }
    }
    // apptainer installations usually ship one
    if let Ok(app) = find_apptainer() {
        let base = realpath(&app);
        if let Some(root) = base.parent().and_then(Path::parent) {
            for c in [
                root.join("utils/bin/mksquashfs"),
                root.join(std::env::consts::ARCH)
                    .join("utils/bin/mksquashfs"),
                root.join("libexec/apptainer/bin/mksquashfs"),
            ] {
                if is_executable(&c) {
                    return Ok(c);
                }
            }
        }
    }
    bail!("mksquashfs not found (install squashfs-tools or put it on PATH)")
}

fn squash(dir: &Path, out: &Path) -> Result<()> {
    let mk = find_mksquashfs()?;
    let partial = PathBuf::from(format!("{}.partial", out.display()));
    let _ = fs::remove_file(&partial);
    info(&format!(
        "squashing {} (this reads every file once; big trees take a while)",
        dir.display()
    ));
    let ok = Command::new(&mk)
        .arg(dir)
        .arg(&partial)
        .args(["-noappend", "-no-progress", "-no-xattrs"])
        .stdout(Stdio::null())
        .status()
        .with_context(|| format!("running {}", mk.display()))?
        .success();
    if !ok {
        let _ = fs::remove_file(&partial);
        bail!("mksquashfs failed for {}", dir.display());
    }
    fs::rename(&partial, out)?;
    Ok(())
}

fn list_host_bins(root: &Path, bin_dirs: &[String]) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for d in bin_dirs {
        let dir = root.join(d);
        if let Ok(rd) = fs::read_dir(&dir) {
            let mut names: Vec<String> = rd
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect();
            names.sort();
            for n in names {
                let p = dir.join(&n);
                // symlinks inside the tree may point to targets only valid when mounted; keep them
                let exe = is_executable(&p)
                    || fs::symlink_metadata(&p)
                        .map(|m| m.file_type().is_symlink())
                        .unwrap_or(false);
                if exe && !map.contains_key(&n) {
                    map.insert(n.clone(), p.display().to_string());
                }
            }
        }
    }
    map
}

pub struct InstallOpts {
    pub base_image: Option<String>,
    pub bin_dirs: Vec<String>,
    pub replace: bool,
    pub backend: Option<Backend>,
}

pub fn cmd_wrap_install(
    name: &str,
    tree: &str,
    path: Option<String>,
    opts: InstallOpts,
) -> Result<()> {
    let tree = realpath(&expand(tree));
    if !tree.is_dir() {
        bail!("{} is not a directory", tree.display());
    }
    if tree.join(STUB_MARKER).exists() {
        bail!("{} is already a cvenv wrap-install stub", tree.display());
    }
    if tree == Path::new("/") || tree == crate::util::home() {
        bail!("refusing to wrap {}", tree.display());
    }
    let env_dir_planned = path
        .clone()
        .map(|p| expand(&p))
        .unwrap_or_else(|| config::store_dir().join(name));
    if is_under(&env_dir_planned, &tree) || is_under(&tree, &env_dir_planned) {
        bail!(
            "the env directory ({}) and the wrapped tree must not contain each other",
            env_dir_planned.display()
        );
    }
    let bin_dirs = if opts.bin_dirs.is_empty() {
        if tree.join("bin").is_dir() {
            vec!["bin".to_string()]
        } else {
            vec![]
        }
    } else {
        opts.bin_dirs.clone()
    };
    let dir = new_env_dir(name, path)?;
    let cfg_file = dir.join(WRAP_CFG);
    if !cfg_file.exists() {
        fs::write(&cfg_file, WRAP_CFG_TEMPLATE)?;
    }
    let env = Env::new(
        name,
        &Record {
            path: dir.clone(),
            manifest: cfg_file,
            kind: Kind::WrapInstall,
            pixi_env: "default".into(),
            created: None,
        },
    );
    let backend = select_backend(None, opts.backend, Kind::WrapInstall)?;
    if backend == Backend::Squashfuse && opts.base_image.is_some() {
        warn("--base-image is ignored by the squashfuse backend (the tree runs on the host OS)");
    }
    let base = match &opts.base_image {
        Some(b) => b.clone(),
        None => config::value("base-image").unwrap_or_else(|| {
            let ver = find_pixi()
                .ok()
                .and_then(|p| pixi_version(&p))
                .unwrap_or_else(|| "latest".into());
            format!("docker://ghcr.io/prefix-dev/pixi:{}", ver)
        }),
    };
    if let Err(e) = install_image(&env, &tree, &base, &bin_dirs, backend) {
        warn(&format!(
            "wrap-install failed; nothing was registered ({} can be removed)",
            dir.display()
        ));
        return Err(e);
    }
    register(&env)?;

    if opts.replace {
        replace_with_stub(&env, &tree, &bin_dirs)?;
    } else {
        println!(
            "The original files in {t} are untouched (and still used outside the container).\n\
             To free their quota, check the env works, then re-run with --replace,\n\
             or move the tree away and leave an empty directory at {t}.",
            t = tree.display()
        );
    }
    Ok(())
}

fn install_image(
    env: &Env,
    tree: &Path,
    base: &str,
    bin_dirs: &[String],
    backend: Backend,
) -> Result<()> {
    let _lock = lock_file(
        &env.art().join(".build.lock"),
        false,
        &format!("env '{}'", env.name),
    )?;
    let t0 = Instant::now();
    let sif = env.art().join(crate::env::IMAGE_NAME);
    if backend == Backend::Apptainer {
        let (kind, _) = parse_base_image(base);
        let base_ref = if kind == "localimage" || base.contains("://") {
            base.to_string()
        } else {
            format!("docker://{}", base)
        };
        fetch_image(&base_ref, &sif, false)?;
    }
    squash(tree, &env.art().join(INSTALL_SQFS))?;
    let exec_paths = list_host_bins(tree, bin_dirs);
    if exec_paths.is_empty() {
        warn("no executables found to expose; use --bin-dir DIR (relative to the tree)");
    }
    let bins: Vec<String> = exec_paths.keys().cloned().collect();
    let sqfs_size = fs::metadata(env.art().join(INSTALL_SQFS))?.len();
    let (size, files, apptainer, base) = if backend == Backend::Apptainer {
        (
            fs::metadata(&sif)?.len() + sqfs_size,
            2,
            find_apptainer()?.display().to_string(),
            base.to_string(),
        )
    } else {
        // switched from apptainer: the base image is not needed any more
        let _ = fs::remove_file(&sif);
        (
            sqfs_size,
            1,
            String::new(),
            "none (runs on the host OS)".to_string(),
        )
    };
    let meta = Meta {
        kind: Kind::WrapInstall,
        backend,
        name: env.name.clone(),
        manifest: env.manifest.clone(),
        pixi_env: env.pixi_env.clone(),
        built_at: now_iso(),
        build_seconds: (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0,
        base_image: base,
        source: Some(tree.display().to_string()),
        install_path: Some(tree.to_path_buf()),
        bin_dirs: bin_dirs.to_vec(),
        conda: tree.join("conda-meta").is_dir(),
        apptainer,
        cvenv_version: env!("CARGO_PKG_VERSION").into(),
        image_size: size,
        bins: bins.clone(),
        exec_paths,
        ..Default::default()
    };
    write_json(&env.meta_path(), &meta)?;
    let n = write_shims(env, &bins)?;
    info(&format!(
        "wrapped {} as '{}' ({} in {} file{}, {} backend, {} commands exposed: {})",
        tree.display(),
        env.name,
        human_size(size),
        files,
        if files == 1 { "" } else { "s" },
        backend.label(),
        n,
        preview(&exposed(env, &bins))
    ));
    Ok(())
}

/// Move the original tree aside and leave a stub at its path whose bin dirs point to the
/// shims: old absolute paths (scripts, kernels, shebangs) keep working through cvenv.
fn replace_with_stub(env: &Env, tree: &Path, bin_dirs: &[String]) -> Result<()> {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let backup = PathBuf::from(format!("{}.cvenv-orig-{}", tree.display(), stamp));
    fs::rename(tree, &backup).with_context(|| format!("moving {} aside", tree.display()))?;
    let res = (|| -> Result<()> {
        fs::create_dir_all(tree)?;
        write_atomic(
            &tree.join(STUB_MARKER),
            format!(
                "This directory is a stub: its content lives in {} (cvenv env '{}').\n",
                env.art().join(INSTALL_SQFS).display(),
                env.name
            )
            .as_bytes(),
            Some(0o644),
        )?;
        for d in bin_dirs {
            let link = tree.join(d);
            if let Some(parent) = link.parent() {
                fs::create_dir_all(parent)?;
            }
            std::os::unix::fs::symlink(env.shim_dir(), &link)?;
        }
        Ok(())
    })();
    if let Err(e) = res {
        remove_tree(tree);
        let _ = fs::rename(&backup, tree);
        return Err(e.context("creating the stub failed; the original tree was restored"));
    }
    info(&format!(
        "replaced {} with a stub; the original is at {}",
        tree.display(),
        backup.display()
    ));
    println!(
        "Check that everything works, e.g.\n    cvenv run --env {n} {cmd}\n\
         then delete the original files to free the quota:\n    rm -rf {b}",
        n = env.name,
        cmd = if env
            .meta()
            .map(|m| m.bins.iter().any(|b| b == "python"))
            .unwrap_or(false)
        {
            "python -c 'import sys; print(sys.prefix)'"
        } else {
            "<command>"
        },
        b = crate::util::q(&backup.display().to_string())
    );
    Ok(())
}

/// Re-point a stub's bin dirs after the env directory moved.
pub fn relink_stub(env: &Env) {
    if let Some(meta) = env.meta() {
        if let Some(tree) = &meta.install_path {
            if tree.join(STUB_MARKER).exists() {
                for d in &meta.bin_dirs {
                    let link = tree.join(d);
                    if fs::symlink_metadata(&link)
                        .map(|m| m.file_type().is_symlink())
                        .unwrap_or(false)
                    {
                        let _ = fs::remove_file(&link);
                        let _ = std::os::unix::fs::symlink(env.shim_dir(), &link);
                    }
                }
            }
        }
    }
}

/// `cvenv rebuild` for a wrap-install env: re-squash the tree (if it still exists).
pub fn refresh_install(env: &Env, backend: Option<Backend>) -> Result<()> {
    let meta = env
        .meta()
        .ok_or_else(|| anyhow::anyhow!("no metadata for env '{}'", env.name))?;
    let tree = meta
        .install_path
        .clone()
        .ok_or_else(|| anyhow::anyhow!("env '{}' has no recorded tree", env.name))?;
    if tree.join(STUB_MARKER).exists() || !tree.is_dir() {
        bail!(
            "the original tree of '{}' was replaced, so there is nothing to re-squash. \
             The env keeps working; to change it, wrap a new tree with `cvenv wrap-install`",
            env.name
        );
    }
    let backend = select_backend(Some(env), backend, Kind::WrapInstall)?;
    let base = if meta.backend == Backend::Apptainer {
        meta.base_image.clone()
    } else {
        config::value("base-image").unwrap_or_else(|| {
            let ver = find_pixi()
                .ok()
                .and_then(|p| pixi_version(&p))
                .unwrap_or_else(|| "latest".into());
            format!("docker://ghcr.io/prefix-dev/pixi:{}", ver)
        })
    };
    info(&format!(
        "re-squashing {} for '{}'",
        tree.display(),
        env.name
    ));
    install_image(env, &tree, &base, &meta.bin_dirs, backend)?;
    env.write_env_file()
}
