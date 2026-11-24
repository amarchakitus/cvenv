//! The squashfuse backend: an env is one SquashFS image, mounted with squashfuse (no root,
//! no Apptainer, no user namespaces); its commands run directly on the host from the
//! mount point.
//!
//! Installed conda packages hard-code their prefix, so pixi installs into the mount point
//! itself: during a build `<mount-root>/<env-id>/<build-id>` is a symlink to node-local
//! scratch. pixi installs through it (every recorded path is the mount point, every file
//! lands in scratch), the tree is squashed, and the symlink is replaced by the empty mount
//! point. Each build gets a new id, so jobs still running from the previous image keep working.
//!
//! Mount points never move and are never removed while they may be in use: renaming or
//! deleting a directory that is mounted on another node would leave a dead mount entry
//! there that only root can clear. So they live outside the env directory (`cvenv move`
//! doesn't touch them), and a replaced build's mount point is only marked retired; it is
//! removed `mount-retention` days later, if it is empty and not mounted on this node. A
//! missing mount point is simply recreated (it is just an empty directory).
//!
//! Mounts belong to one user on one node. Shims mount on demand under a node-local lock;
//! squashfuse unmounts by itself after `mount-timeout` idle seconds (never while files are
//! open), and stale mounts left by killed squashfuse processes are cleaned up. Under Slurm
//! a job's processes are killed when it ends, which would break other jobs sharing the
//! mount, so when the user's systemd instance runs, squashfuse is started through
//! `systemd-run --user` (`mount-mode`).

use crate::build::{build_tmp_root, run_logged};
use crate::config;
use crate::env::{compute_fingerprint, cuda_requirement, stat_sig, toml_to_string, Backend, Env, Kind, Meta};
use crate::runtime::{find_pixi, pixi_version, write_shims, HOST_ENV_DROP, LAYER_DIR};
use crate::util::{expand, home, human_size, info, is_executable, lock_file, now_iso, q, realpath, remove_tree, sha256_bytes, warn, which, write_json};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const DEFAULT_MOUNT_TIMEOUT: u64 = 600;
/// Days a replaced build's mount point is kept (jobs on other nodes may still use it).
pub const DEFAULT_MOUNT_RETENTION_DAYS: u64 = 14;
/// Suffix of the marker that retires a mount point: `<id-dir>/<build>.retired`.
const RETIRED_SUFFIX: &str = ".retired";
/// PATH while capturing the pixi activation; stripped from the captured PATH.
const BASE_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// Activation variables that describe the capturing process, not the env.
const ACTIVATION_SKIP: &[&str] = &[
    "PATH", "PWD", "OLDPWD", "SHLVL", "_", "HOME", "USER", "PS1", "PIXI_IN_SHELL", "CONDA_OVERRIDE_CUDA",
    "PIXI_HOME", "PIXI_CACHE_DIR", "RATTLER_CACHE_DIR", "UV_CACHE_DIR", "PIXI_NO_PROGRESS", "TMPDIR",
];
/// Guards against a command resolving back to a shim (e.g. a stub's bin/ when unmounted).
const DEPTH_VAR: &str = "_CVENV_SQUASH_DEPTH";
/// Marker in a directory replaced by `wrap-install --replace` (see wrap.rs).
const STUB_MARKER: &str = ".cvenv-wrapped";

// ---------------------------------------------------------------------------
// tools
// ---------------------------------------------------------------------------

pub fn find_squashfuse() -> Result<PathBuf> {
    if let Some(v) = config::value("squashfuse") {
        let p = expand(&v);
        if is_executable(&p) {
            return Ok(p);
        }
        warn(&format!("configured squashfuse {} is not executable; searching PATH", p.display()));
    }
    for n in ["squashfuse_ll", "squashfuse"] {
        if let Some(p) = which(n) {
            return Ok(p);
        }
    }
    bail!(
        "squashfuse not found. Install it (installation.sh can fetch it on Debian/Ubuntu; \
         conda-forge has it too) or run `cvenv config set squashfuse /path/to/squashfuse_ll`"
    )
}

pub fn find_fusermount() -> Option<PathBuf> {
    for n in ["fusermount3", "fusermount"] {
        if let Some(p) = which(n) {
            return Some(p);
        }
    }
    ["/usr/bin/fusermount3", "/bin/fusermount3", "/usr/bin/fusermount", "/bin/fusermount"]
        .iter()
        .map(PathBuf::from)
        .find(|p| is_executable(p))
}

/// Library directories of a squashfuse unpacked from distro packages into a prefix
/// (`<root>/usr/bin/squashfuse_ll`, libraries in `<root>/usr/lib/<triple>`, `<root>/lib/<triple>`).
fn squashfuse_lib_dirs(bin: &Path) -> Vec<PathBuf> {
    let real = realpath(bin);
    if ["/usr", "/bin", "/sbin"].iter().any(|p| real.starts_with(p)) {
        return vec![];
    }
    let Some(bindir) = real.parent() else { return vec![] };
    let triple = format!("{}-linux-gnu", std::env::consts::ARCH);
    let mut c = vec![bindir.join("../lib"), bindir.join("../lib64"), bindir.join("../lib").join(&triple)];
    if let Some(root) = bindir.parent().and_then(Path::parent) {
        c.push(root.join("lib").join(&triple));
    }
    let mut out: Vec<PathBuf> = vec![];
    for p in c.into_iter().map(|p| crate::util::normalize(&p)) {
        if p.is_dir() && !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// LD_LIBRARY_PATH squashfuse needs (None: the default is fine).
fn squashfuse_ld_path(bin: &Path) -> Option<String> {
    let libs = squashfuse_lib_dirs(bin);
    if libs.is_empty() {
        return None;
    }
    let mut v: Vec<String> = libs.iter().map(|p| p.display().to_string()).collect();
    if let Ok(old) = std::env::var("LD_LIBRARY_PATH") {
        if !old.is_empty() {
            v.push(old);
        }
    }
    Some(v.join(":"))
}

fn squashfuse_env(c: &mut Command, bin: &Path) {
    if let Some(ld) = squashfuse_ld_path(bin) {
        c.env("LD_LIBRARY_PATH", ld);
    }
}

/// Whether this squashfuse build knows `-o timeout=N` (idle auto-unmount).
fn supports_timeout(bin: &Path) -> bool {
    let mut c = Command::new(bin);
    c.arg("--help").stdin(Stdio::null());
    squashfuse_env(&mut c, bin);
    c.output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("timeout=") || String::from_utf8_lossy(&o.stderr).contains("timeout="))
        .unwrap_or(false)
}

fn mount_timeout() -> u64 {
    config::value("mount-timeout").and_then(|v| v.trim().parse().ok()).unwrap_or(DEFAULT_MOUNT_TIMEOUT)
}

// ---------------------------------------------------------------------------
// mount state
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum MountState {
    NotMounted,
    Mounted,
    /// listed, but the squashfuse process is gone ("Transport endpoint is not connected")
    Stale,
    /// mounted by another user (FUSE mounts are private to their owner)
    Foreign,
}

impl MountState {
    pub fn label(self) -> &'static str {
        match self {
            MountState::NotMounted => "not mounted",
            MountState::Mounted => "mounted",
            MountState::Stale => "stale mount",
            MountState::Foreign => "mounted by another user",
        }
    }
}

/// /proc/mounts escapes space, tab, newline and backslash as \ooo.
fn unescape_mount_field(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            let v = (b[i + 1] - b'0') as u32 * 64 + (b[i + 2] - b'0') as u32 * 8 + (b[i + 3] - b'0') as u32;
            out.push(v as u8);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A mount point as the kernel lists it: parents resolved, the last component kept (a
/// stale FUSE mount can't be stat'ed).
fn canon(mnt: &Path) -> PathBuf {
    match (mnt.parent(), mnt.file_name()) {
        (Some(p), Some(n)) => realpath(p).join(n),
        _ => crate::util::absolute(mnt),
    }
}

fn mount_count_in(table: &str, mnt: &Path) -> usize {
    table
        .lines()
        .filter(|l| l.split(' ').nth(1).map(|f| Path::new(&unescape_mount_field(f)) == mnt).unwrap_or(false))
        .count()
}

pub fn state(mnt: &Path) -> MountState {
    let table = fs::read_to_string("/proc/self/mounts").unwrap_or_default();
    if mount_count_in(&table, &canon(mnt)) == 0 {
        return MountState::NotMounted;
    }
    // opendir always reaches squashfuse; stat may be answered from the kernel's attribute
    // cache even when the squashfuse process is gone
    match fs::read_dir(mnt) {
        Ok(_) => MountState::Mounted,
        Err(e) => match e.raw_os_error() {
            Some(libc::EACCES) | Some(libc::EPERM) => MountState::Foreign,
            _ => MountState::Stale,
        },
    }
}

pub fn unmount(mnt: &Path, lazy: bool) -> bool {
    let Some(fm) = find_fusermount() else { return false };
    let mut c = Command::new(fm);
    c.arg("-u");
    if lazy {
        c.arg("-z");
    }
    c.arg(mnt).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// mounting
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MountMode {
    Auto,
    Systemd,
    Direct,
}

fn mount_mode() -> Result<MountMode> {
    match config::value("mount-mode").map(|v| v.trim().to_lowercase()).as_deref() {
        None | Some("") | Some("auto") => Ok(MountMode::Auto),
        Some("systemd") => Ok(MountMode::Systemd),
        Some("direct") => Ok(MountMode::Direct),
        Some(o) => bail!("invalid `mount-mode` setting '{}' (auto, systemd or direct)", o),
    }
}

/// The runtime directory of this user's systemd instance, if one is running here.
fn systemd_user_runtime() -> Option<PathBuf> {
    which("systemd-run")?;
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })));
    if dir.join("systemd/private").exists() || dir.join("bus").exists() {
        Some(dir)
    } else {
        None
    }
}

fn wait_mounted(mnt: &Path, timeout: Duration) -> bool {
    let t0 = Instant::now();
    loop {
        if state(mnt) == MountState::Mounted {
            return true;
        }
        if t0.elapsed() > timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn squashfuse_args(sq: &Path) -> Vec<String> {
    let t = mount_timeout();
    if t > 0 && supports_timeout(sq) {
        vec!["-o".into(), format!("timeout={}", t)]
    } else {
        vec![]
    }
}

/// Mount through `systemd-run --user`: the squashfuse process then belongs to the user's
/// systemd instance instead of the current Slurm job, so it outlives the job.
fn mount_systemd(sq: &Path, runtime: &Path, image: &Path, mnt: &Path, errf: &Path) -> Result<()> {
    let unit = format!("cvenv-mount-{}-{}", &sha256_bytes(canon(mnt).as_os_str().as_encoded_bytes())[..12], std::process::id());
    let mut c = Command::new(which("systemd-run").unwrap_or_else(|| PathBuf::from("systemd-run")));
    c.args(["--user", "--collect", "--quiet", "--unit", &unit])
        .arg("--property=StandardOutput=null")
        .arg(format!("--property=StandardError=file:{}", errf.display()));
    if let Some(ld) = squashfuse_ld_path(sq) {
        c.arg(format!("--setenv=LD_LIBRARY_PATH={}", ld));
    }
    c.arg(sq).arg("-f").args(squashfuse_args(sq)).arg(image).arg(mnt);
    c.env("XDG_RUNTIME_DIR", runtime).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    let out = c.output().context("running systemd-run")?;
    if !out.status.success() {
        bail!("systemd-run failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    if wait_mounted(mnt, Duration::from_secs(10)) {
        return Ok(());
    }
    let _ = Command::new("systemctl").args(["--user", "stop", &unit]).env("XDG_RUNTIME_DIR", runtime).stderr(Stdio::null()).status();
    bail!("the systemd unit {} did not mount the image: {}", unit, fs::read_to_string(errf).unwrap_or_default().trim());
}

/// Mount from this process: squashfuse daemonizes, but stays in the current Slurm job and
/// is killed when the job ends.
fn mount_direct(sq: &Path, image: &Path, mnt: &Path, errf: &Path) -> Result<()> {
    let mut c = Command::new(sq);
    c.args(squashfuse_args(sq)).arg(image).arg(mnt).current_dir("/");
    c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(fs::File::create(errf)?);
    squashfuse_env(&mut c, sq);
    let st = c.status().with_context(|| format!("running {}", sq.display()))?;
    // squashfuse returns once the mount is in place
    if st.success() && wait_mounted(mnt, Duration::from_secs(5)) {
        return Ok(());
    }
    bail!("{}", fs::read_to_string(errf).unwrap_or_default().trim());
}

/// Make sure `image` is mounted at `mnt` on this node (mounting it if needed). With
/// `create`, a missing mount point is created (cvenv-managed mount points are just empty
/// directories; wrap-install mount points are the user's own directories and are not).
pub fn ensure_mounted(image: &Path, mnt: &Path, create: bool) -> Result<()> {
    if state(mnt) == MountState::Mounted {
        return Ok(());
    }
    let key = &sha256_bytes(canon(mnt).as_os_str().as_encoded_bytes())[..16];
    let _l = lock_file(&build_tmp_root()?.join("mounts").join(format!("{}.lock", key)), true, "the mount lock")?;
    let mut repairs = 0;
    loop {
        match state(mnt) {
            MountState::Mounted => return Ok(()),
            MountState::NotMounted => break,
            MountState::Foreign => bail!(
                "{} is mounted by another user on this node. FUSE mounts are private to the user who made \
                 them (this node does not allow sharing), so use another node, or wait until it is \
                 unmounted (automatically, once it has been unused for a while)",
                mnt.display()
            ),
            MountState::Stale => {
                // left behind by a killed squashfuse (e.g. at the end of a Slurm job)
                repairs += 1;
                if repairs > 4 || !unmount(mnt, true) {
                    bail!("{} is a stale mount that could not be removed; try `fusermount3 -uz {}`", mnt.display(), q(&mnt.display().to_string()));
                }
            }
        }
    }
    if !image.is_file() {
        bail!("image {} is missing", image.display());
    }
    if !mnt.is_dir() {
        if !create || fs::symlink_metadata(mnt).is_ok() {
            bail!("mount point {} is missing", mnt.display());
        }
        fs::create_dir_all(mnt).with_context(|| format!("recreating the mount point {}", mnt.display()))?;
    }
    if !Path::new("/dev/fuse").exists() {
        bail!("/dev/fuse does not exist on this node: FUSE mounts are not possible here");
    }
    if find_fusermount().is_none() {
        bail!("fusermount3 (or fusermount) not found: FUSE mounts need it (package fuse3)");
    }
    let sq = find_squashfuse()?;
    let errf = build_tmp_root()?.join("mounts").join(format!("{}-{}.err", key, std::process::id()));
    let mode = mount_mode()?;
    let runtime = if mode == MountMode::Direct { None } else { systemd_user_runtime() };
    let debug = std::env::var_os("CVENV_DEBUG").is_some();
    let res = match (mode, runtime) {
        (MountMode::Systemd, None) => Err(anyhow!("mount-mode is 'systemd' but no systemd user instance is running on this node")),
        (_, Some(rt)) => match mount_systemd(&sq, &rt, image, mnt, &errf) {
            Ok(()) => {
                if debug {
                    info(&format!("mounted {} at {} (systemd user unit)", image.display(), mnt.display()));
                }
                Ok(())
            }
            Err(e) if mode == MountMode::Auto => {
                if debug {
                    warn(&format!("mounting through systemd failed ({:#}); mounting directly", e));
                }
                mount_direct(&sq, image, mnt, &errf)
            }
            Err(e) => Err(e),
        },
        (_, None) => mount_direct(&sq, image, mnt, &errf),
    };
    let _ = fs::remove_file(&errf);
    res.with_context(|| format!("mounting {} at {} failed", image.display(), mnt.display()))?;
    if debug {
        info(&format!("mounted {} at {}", image.display(), mnt.display()));
    }
    Ok(())
}

/// pixi leaves `<workspace>/.pixi/envs` as a symlink to the install location. While the
/// image is mounted, host pixi (`cvenv add`, ...) would follow it, find an env it thinks was
/// moved and refuse to work, so the link is removed after builds and before host pixi runs.
pub fn drop_pixi_envs_link(env: &Env) {
    let link = env.manifest_dir().join(".pixi").join("envs");
    let Some(id_dir) = env.meta().and_then(|m| m.mount_dir).and_then(|m| m.parent().map(Path::to_path_buf)) else { return };
    if let Ok(target) = fs::read_link(&link) {
        let target = if target.is_absolute() { target } else { link.parent().unwrap_or(Path::new("/")).join(target) };
        if crate::util::is_under(&target, &id_dir) {
            let _ = fs::remove_file(&link);
        }
    }
}

/// Where a wrap-install env's squashed tree must be mounted, or None when the original
/// tree is still in place (then commands run from it directly).
fn wrap_install_mount(env: &Env, meta: &Meta) -> Result<Option<PathBuf>> {
    let ip = meta.install_path.clone().ok_or_else(|| anyhow!("env '{}' has no recorded installation path", env.name))?;
    let replaced = ip.join(STUB_MARKER).exists() || fs::read_dir(&ip).map(|mut d| d.next().is_none()).unwrap_or(false);
    if replaced || state(&ip) != MountState::NotMounted {
        Ok(Some(ip))
    } else if ip.is_dir() {
        Ok(None)
    } else {
        bail!("{} (the mount point of env '{}') does not exist; recreate the directory", ip.display(), env.name)
    }
}

/// The mount point of a built env (None: wrap-install whose original tree is in place).
pub fn mount_point(env: &Env) -> Result<Option<PathBuf>> {
    let meta = env.meta().ok_or_else(|| anyhow!("env '{}' has no build metadata - run `{}`", env.name, env.rebuild_hint()))?;
    if env.kind == Kind::WrapInstall {
        return wrap_install_mount(env, &meta);
    }
    match meta.mount_dir {
        Some(m) => Ok(Some(m)),
        None => bail!("env '{}' has no mount point recorded - run `{}`", env.name, env.rebuild_hint()),
    }
}

// ---------------------------------------------------------------------------
// running commands
// ---------------------------------------------------------------------------

/// Replace the current process with `args` running from the env's mounted image.
pub fn exec(env: &Env, args: &[String], mode: &str) -> Result<()> {
    let depth: u32 = std::env::var(DEPTH_VAR).ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    if depth >= 3 {
        bail!("env '{}': the command keeps resolving back to a cvenv shim (is the image mounted? see `cvenv info`)", env.name);
    }
    let meta = env.meta().ok_or_else(|| anyhow!("env '{}' has no build metadata - run `{}`", env.name, env.rebuild_hint()))?;
    if let Some(mnt) = mount_point(env)? {
        ensure_mounted(&env.image(), &mnt, env.kind != Kind::WrapInstall)?;
    }
    let cfg = env.cfg();
    let mut penv: BTreeMap<String, String> = std::env::vars().collect();
    for k in HOST_ENV_DROP {
        penv.remove(*k);
    }
    let mut prepend: Vec<String> = vec![];
    let mut conda_activate: Option<PathBuf> = None;
    if env.kind == Kind::WrapInstall {
        let ip = meta.install_path.clone().unwrap_or_default();
        prepend.extend(meta.bin_dirs.iter().map(|d| ip.join(d).display().to_string()));
        if meta.conda {
            penv.insert("CONDA_PREFIX".into(), ip.display().to_string());
            conda_activate = Some(ip);
        }
    } else {
        penv.extend(meta.activation.clone());
        prepend.extend(meta.path_prepend.clone());
    }
    if let Some(t) = cfg.get("env").and_then(|v| v.as_table()) {
        for (k, v) in t {
            penv.insert(k.clone(), toml_to_string(v));
        }
    }
    penv.insert("CVENV_ENV".into(), env.name.clone());
    penv.insert("CVENV_IMAGE".into(), env.image().display().to_string());
    penv.insert(DEPTH_VAR.into(), (depth + 1).to_string());
    if cfg.get("isolate-user-site").and_then(|v| v.as_bool()).unwrap_or(true) {
        penv.insert("PYTHONNOUSERSITE".into(), "1".into());
    }
    penv.entry("PIP_USER".into()).or_insert_with(|| "0".into());
    let host_path = penv.get("PATH").cloned().unwrap_or_default();
    let mut path: Vec<String> = prepend.clone();
    path.extend(host_path.split(':').filter(|p| !p.is_empty() && !prepend.iter().any(|x| x == p)).map(String::from));
    penv.insert("PATH".into(), path.join(":"));

    let mut argv: Vec<String> = if mode == "shell" {
        penv.insert("PS1".into(), format!("({}) \\w \\$ ", env.name));
        let mut v = vec!["bash".to_string(), "--norc".into()];
        v.extend(args.iter().cloned());
        v
    } else {
        args.to_vec()
    };
    if argv.is_empty() {
        bail!("no command given");
    }
    if !argv[0].contains('/') {
        if let Some(p) = meta.exec_paths.get(&argv[0]) {
            argv[0] = p.clone();
        }
    }
    if let Some(ip) = conda_activate {
        // conda envs: run the packages' activate.d scripts like `conda activate` would
        let mut w = vec![
            "/bin/bash".to_string(),
            "-c".into(),
            format!("for f in {}/etc/conda/activate.d/*.sh; do [ -f \"$f\" ] && . \"$f\"; done; exec \"$@\"", q(&ip.display().to_string())),
            "cvenv-activate".into(),
        ];
        w.extend(argv);
        argv = w;
    }
    if std::env::var_os("CVENV_DEBUG").is_some() {
        info(&format!("exec (squashfuse): {}", argv.iter().map(|a| q(a)).collect::<Vec<_>>().join(" ")));
    }
    let err = Command::new(&argv[0]).args(&argv[1..]).env_clear().envs(&penv).exec();
    Err(err).with_context(|| format!("failed to run {}", argv[0]))
}

/// A PIXI_HOME whose config.toml (the user's global pixi config plus
/// `detached-environments`) makes pixi install into / find envs under `detached`.
fn pixi_home_for(dir: &Path, detached: &Path) -> Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let user_home = std::env::var("PIXI_HOME").ok().filter(|s| !s.is_empty()).map(PathBuf::from).unwrap_or_else(|| home().join(".pixi"));
    let mut cfg: toml::Table = if user_home != dir {
        fs::read_to_string(user_home.join("config.toml")).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default()
    } else {
        toml::Table::new()
    };
    cfg.insert("detached-environments".into(), toml::Value::String(detached.display().to_string()));
    crate::util::write_atomic(&dir.join("config.toml"), toml::to_string(&cfg)?.as_bytes(), Some(0o644))?;
    Ok(dir.to_path_buf())
}

/// PIXI_HOME for running pixi tasks of a squashfuse env on the host.
pub fn task_pixi_home(env: &Env) -> Result<PathBuf> {
    let meta = env.meta().ok_or_else(|| anyhow!("env '{}' has no build metadata", env.name))?;
    let mnt = meta.mount_dir.ok_or_else(|| anyhow!("env '{}' has no mount point recorded - run `{}`", env.name, env.rebuild_hint()))?;
    let key = &sha256_bytes(mnt.as_os_str().as_encoded_bytes())[..16];
    pixi_home_for(&build_tmp_root()?.join("pixi-home").join(key), &mnt)
}

// ---------------------------------------------------------------------------
// building
// ---------------------------------------------------------------------------

/// Split `pixi shell-hook --json` variables into (variables to set, PATH entries to prepend).
fn split_activation(vars: &serde_json::Map<String, serde_json::Value>, base_path: &str) -> (BTreeMap<String, String>, Vec<String>) {
    let mut set = BTreeMap::new();
    let mut prepend = vec![];
    let base: Vec<&str> = base_path.split(':').collect();
    for (k, v) in vars {
        let v = match v {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        if k == "PATH" {
            prepend = v.split(':').filter(|p| !p.is_empty() && !base.contains(p)).map(String::from).collect();
        } else if !ACTIVATION_SKIP.contains(&k.as_str()) {
            set.insert(k.clone(), v);
        }
    }
    (set, prepend)
}

/// pixi must record the mount point, not the scratch directory behind the build symlink.
fn check_no_stage_leak(prefix: &Path, stage: &Path) -> Result<()> {
    let needle = stage.display().to_string();
    let mut files: Vec<PathBuf> = fs::read_dir(prefix.join("bin")).map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect()).unwrap_or_default();
    files.truncate(300);
    files.push(prefix.join("conda-meta").join("history"));
    let mut buf = vec![0u8; 4096];
    for f in files {
        if !fs::symlink_metadata(&f).map(|m| m.is_file()).unwrap_or(false) {
            continue;
        }
        if let Ok(mut fh) = fs::File::open(&f) {
            let n = fh.read(&mut buf).unwrap_or(0);
            if String::from_utf8_lossy(&buf[..n]).contains(&needle) {
                bail!(
                    "{} refers to the build directory {} instead of the mount point; this pixi version \
                     resolves the build symlink, which the squashfuse backend can't handle",
                    f.display(),
                    needle
                );
            }
        }
    }
    Ok(())
}

pub fn build(env: &Env, update: bool, fresh_cache: bool, keep_build_dir: bool, incremental: bool) -> Result<()> {
    let pixi = find_pixi()?;
    let mksquashfs = crate::wrap::find_mksquashfs()?;
    if let Err(e) = find_squashfuse() {
        warn(&format!("{:#}. The image will be built, but can't be used until squashfuse is available", e));
    }
    if !env.manifest.is_file() {
        bail!("manifest not found: {}", env.manifest.display());
    }
    fs::create_dir_all(env.art())?;
    let _lock = lock_file(&env.art().join(".build.lock"), false, &format!("env '{}' (another build is running?)", env.name))?;
    if incremental {
        info("--incremental is not needed with the squashfuse backend (packages are hard-linked from the cache); doing a normal build");
    }

    let tmp_root = build_tmp_root()?;
    let work = tmp_root.join(format!("build-{}-{}", env.name, std::process::id()));
    remove_tree(&work);
    let stage = work.join("root");
    fs::create_dir_all(&stage)?;
    let cache = if fresh_cache {
        work.join("pixi-cache")
    } else {
        config::value("pixi-cache").map(|v| expand(&v)).unwrap_or_else(|| tmp_root.join("pixi-cache"))
    };
    fs::create_dir_all(&cache)?;

    let id = format!("{}-{}", chrono::Local::now().format("%Y%m%d%H%M%S"), std::process::id());
    let id_dir = mount_root().join(env.ensure_env_id()?);
    let mnt = id_dir.join(&id);
    if mnt.as_os_str().len() > 180 {
        bail!(
            "the mount path is too long for the squashfuse backend ({} characters in {}; conda packages \
             allow about 200 for the install location). Use a shorter one: `cvenv config set mount-root DIR`",
            mnt.as_os_str().len(),
            mnt.display()
        );
    }
    fs::create_dir_all(&id_dir).with_context(|| format!("creating {}", id_dir.display()))?;
    let _ = fs::remove_file(&mnt);
    std::os::unix::fs::symlink(&stage, &mnt).with_context(|| format!("creating {}", mnt.display()))?;
    let image_name = format!("image-{}.sqfs", id);
    let image = env.art().join(&image_name);
    let partial = PathBuf::from(format!("{}.partial", image.display()));
    let log_path = env.art().join("build.log");

    info(&format!("building env '{}' (pixi env '{}', squashfuse backend)", env.name, env.pixi_env));
    info(&format!("  manifest   : {}", env.manifest.display()));
    info(&format!("  build dir  : {}", work.display()));
    info(&format!("  image      : {}", image.display()));
    let t0 = Instant::now();

    let res = (|| -> Result<Meta> {
        let mut log = fs::File::create(&log_path).with_context(|| format!("creating {}", log_path.display()))?;
        writeln!(log, "# {}\n# squashfuse build of '{}': installs into {} (staged in {})\n", now_iso(), env.name, mnt.display(), stage.display())?;
        let home_dir = pixi_home_for(&work.join("pixi-home"), &mnt)?;
        let cuda = std::env::var("CONDA_OVERRIDE_CUDA").ok().filter(|s| !s.is_empty()).or_else(|| cuda_requirement(env));
        let manifest = env.manifest.display().to_string();
        let pixi_cmd = |args: &[&str]| -> Command {
            let mut c = Command::new(&pixi);
            c.args(args)
                .current_dir(env.manifest_dir())
                .env("PIXI_HOME", &home_dir)
                .env("PIXI_CACHE_DIR", &cache)
                .env("RATTLER_CACHE_DIR", &cache)
                .env("UV_CACHE_DIR", cache.join("uv"))
                .env("PIXI_NO_PROGRESS", "true")
                .env("TMPDIR", &work);
            for k in HOST_ENV_DROP {
                c.env_remove(k);
            }
            if let Some(cu) = &cuda {
                // pixi checks [system-requirements] against the build machine, which usually has no GPU
                c.env("CONDA_OVERRIDE_CUDA", cu);
            }
            c
        };
        if update && !run_logged(&mut pixi_cmd(&["update", "--manifest-path", &manifest]), &mut log)?.success() {
            bail!("`pixi update` failed");
        }
        if !run_logged(&mut pixi_cmd(&["install", "--manifest-path", &manifest, "-e", &env.pixi_env]), &mut log)?.success() {
            bail!("`pixi install` failed");
        }
        let proj = fs::read_dir(&mnt)?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .find(|n| mnt.join(n).join("envs").join(&env.pixi_env).join("conda-meta").is_dir())
            .ok_or_else(|| anyhow!("pixi did not install the env under {}", mnt.display()))?;
        let prefix = mnt.join(proj).join("envs").join(&env.pixi_env);
        check_no_stage_leak(&prefix, &stage)?;

        // the pixi activation (incl. activate.d scripts), captured as variables
        let mut hook = Command::new(&pixi);
        hook.args(["shell-hook", "--json", "--frozen", "--manifest-path", &manifest, "-e", &env.pixi_env])
            .current_dir(env.manifest_dir())
            .env_clear()
            .env("HOME", home())
            .env("USER", crate::util::username())
            .env("PATH", BASE_PATH)
            .env("PIXI_HOME", &home_dir)
            .env("PIXI_CACHE_DIR", &cache)
            .env("TMPDIR", &work)
            .stdin(Stdio::null());
        if let Some(cu) = &cuda {
            hook.env("CONDA_OVERRIDE_CUDA", cu);
        }
        let out = hook.output().context("running `pixi shell-hook`")?;
        log.write_all(&out.stderr)?;
        if !out.status.success() {
            bail!("capturing the pixi activation failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).context("parsing `pixi shell-hook --json` output")?;
        let vars = v.get("environment_variables").and_then(|x| x.as_object()).ok_or_else(|| anyhow!("unexpected `pixi shell-hook --json` output"))?;
        let (activation, path_prepend) = split_activation(vars, BASE_PATH);
        writeln!(log, "# activation: {} variables, PATH += {}", activation.len(), path_prepend.join(":"))?;

        let mut bins: Vec<String> = fs::read_dir(prefix.join("bin"))
            .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string()).collect())
            .unwrap_or_default();
        bins.sort();
        let pkgs = pixi_cmd(&["list", "--frozen", "--manifest-path", &manifest, "-e", &env.pixi_env]).stdin(Stdio::null()).output()?;
        fs::write(env.art().join("packages.txt"), &pkgs.stdout)?;
        // provenance inside the image, like /opt/cvenv/src in apptainer images
        let src = stage.join(".cvenv-src");
        fs::create_dir_all(&src)?;
        fs::copy(&env.manifest, src.join(env.manifest.file_name().unwrap_or_default()))?;
        if env.lock_path().is_file() {
            fs::copy(env.lock_path(), src.join("pixi.lock"))?;
        }
        fs::write(src.join("packages.txt"), &pkgs.stdout)?;

        info(&format!("squashing {} (single file for the whole env)", prefix.display()));
        let mut mk = Command::new(&mksquashfs);
        mk.arg(&stage).arg(&partial).args(["-noappend", "-no-progress", "-no-xattrs", "-quiet"]);
        if !run_logged(&mut mk, &mut log)?.success() {
            bail!("mksquashfs failed");
        }
        fs::rename(&partial, &image)?;
        // the build symlink becomes the (empty) mount point
        fs::remove_file(&mnt)?;
        fs::create_dir(&mnt)?;

        let fresh = Env::new(&env.name, &env.record()); // re-read the manifest: pixi may have touched it
        let image_size = fs::metadata(&image)?.len();
        Ok(Meta {
            kind: Kind::Pixi,
            backend: Backend::Squashfuse,
            name: env.name.clone(),
            pixi_env: env.pixi_env.clone(),
            manifest: env.manifest.clone(),
            built_at: now_iso(),
            build_seconds: (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0,
            base_image: "none (runs on the host OS)".into(),
            pixi_version: pixi_version(&pixi),
            cvenv_version: env!("CARGO_PKG_VERSION").into(),
            image_size,
            fingerprint: compute_fingerprint(&fresh)?,
            stat: stat_sig(&fresh),
            exec_paths: bins.iter().map(|b| (b.clone(), prefix.join("bin").join(b).display().to_string())).collect(),
            bins,
            prefix: Some(prefix.display().to_string()),
            squash_image: Some(image_name.clone()),
            mount_dir: Some(mnt.clone()),
            activation,
            path_prepend,
            ..Default::default()
        })
    })();

    let meta = match res {
        Ok(m) => m,
        Err(e) => {
            if fs::symlink_metadata(&mnt).map(|m| m.file_type().is_symlink()).unwrap_or(false) {
                let _ = fs::remove_file(&mnt);
            }
            let _ = fs::remove_file(&partial);
            let _ = fs::remove_file(&image); // new id: nothing refers to it yet
            if !keep_build_dir {
                remove_tree(&work);
            }
            bail!("build failed: {:#}\nThe previous image (if any) is untouched. Log: {}", e, log_path.display());
        }
    };
    let previous = env.meta().filter(|m| m.backend == Backend::Squashfuse && m.kind == Kind::Pixi);
    // a shared env (`cvenv share`) keeps its mount point writable for the group
    if let Some(old) = previous.as_ref().and_then(|m| m.mount_dir.clone()) {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(md) = fs::metadata(&old) {
            let extra = md.permissions().mode() & 0o077;
            let _ = fs::set_permissions(&mnt, fs::Permissions::from_mode(0o700 | extra));
        }
    }
    // switch: new processes use the new image from here on; running ones keep the old mount
    write_json(&env.meta_path(), &meta)?;
    env.write_env_file()?;
    let n = write_shims(env, &meta.bins)?;
    drop_pixi_envs_link(env);
    if let Some(old) = previous.as_ref().and_then(|m| m.mount_dir.as_ref()) {
        if *old != mnt {
            retire_mount(old);
        }
    }
    remove_artifacts(env, Some(&meta));
    gc_mount_root(&mount_root());
    if !keep_build_dir {
        remove_tree(&work);
    }
    info(&format!(
        "built '{}' in {:.0}s: {} ({}, {} commands shimmed)",
        env.name,
        t0.elapsed().as_secs_f64(),
        env.image().display(),
        human_size(meta.image_size),
        n
    ));
    Ok(())
}

/// Where squashfuse images are mounted: `<mount-root>/<env-id>/<build-id>`.
pub fn mount_root() -> PathBuf {
    config::value("mount-root").map(|v| expand(&v)).unwrap_or_else(|| config::store_dir().join(".cvenv-mnt"))
}

fn retention() -> Duration {
    let days = config::value("mount-retention").and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(DEFAULT_MOUNT_RETENTION_DAYS);
    Duration::from_secs(days * 86400)
}

fn retired_marker(mnt: &Path) -> Option<PathBuf> {
    let name = mnt.file_name()?.to_string_lossy().to_string();
    Some(mnt.with_file_name(format!("{}{}", name, RETIRED_SUFFIX)))
}

/// Mark a mount point as no longer current; `gc_mount_root` removes it after the retention
/// period. Never removes anything itself.
pub fn retire_mount(mnt: &Path) {
    if let Some(m) = retired_marker(mnt) {
        if !m.exists() && mnt.parent().map(Path::is_dir).unwrap_or(false) {
            let _ = fs::write(&m, format!("retired {}\n", now_iso()));
        }
    }
}

/// Remove retired mount points older than the retention period. Only ever removes empty
/// directories (rmdir) that are not mounted on this node, plus leftover build symlinks whose
/// scratch directory is gone. Safe to run at any time.
pub fn gc_mount_root(root: &Path) {
    let keep = retention();
    let Ok(ids) = fs::read_dir(root) else { return };
    for id in ids.filter_map(|e| e.ok()) {
        if !id.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let id_dir = id.path();
        let Ok(entries) = fs::read_dir(&id_dir) else { continue };
        for e in entries.filter_map(|e| e.ok()) {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            let ft = match e.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_symlink() {
                if !p.exists() {
                    let _ = fs::remove_file(&p); // build symlink of a crashed build
                }
            } else if let Some(build) = name.strip_suffix(RETIRED_SUFFIX) {
                let age = e.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.elapsed().ok()).unwrap_or_default();
                if age < keep {
                    continue;
                }
                let d = id_dir.join(build);
                let gone = fs::symlink_metadata(&d).is_err();
                if gone || (state(&d) == MountState::NotMounted && fs::remove_dir(&d).is_ok()) {
                    let _ = fs::remove_file(&p);
                }
            }
        }
        let _ = fs::remove_dir(&id_dir); // only if nothing is left
    }
}

/// An env stops using the squashfuse image described by `meta` (deleted, or switched to
/// Apptainer): retire its mount point; `gc_mount_root` removes it later.
pub fn retire_image(meta: Option<&Meta>) {
    if let Some(m) = meta {
        if m.backend == Backend::Squashfuse && m.kind == Kind::Pixi {
            if let Some(mnt) = &m.mount_dir {
                retire_mount(mnt);
            }
        }
    }
    gc_mount_root(&mount_root());
}

/// Remove squashfuse images of `env` other than `current`'s (None: the env switched to
/// Apptainer). That is safe at any time: running mounts keep their image open. With
/// `current`, Apptainer leftovers (image.sif, host-path layer) are removed too.
pub fn remove_artifacts(env: &Env, current: Option<&Meta>) {
    let art = env.art();
    let keep_image = current.and_then(|m| m.squash_image.clone());
    if let Ok(rd) = fs::read_dir(&art) {
        for e in rd.filter_map(|e| e.ok()) {
            let n = e.file_name().to_string_lossy().to_string();
            if n.starts_with("image-") && (n.ends_with(".sqfs") || n.ends_with(".sqfs.partial")) && Some(&n) != keep_image.as_ref() {
                let _ = fs::remove_file(e.path());
            }
        }
    }
    if current.is_some() && env.kind == Kind::Pixi {
        let _ = fs::remove_file(art.join(crate::env::IMAGE_NAME));
        let _ = fs::remove_file(art.join("activate.sh"));
        remove_tree(&art.join(LAYER_DIR));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_table_parsing() {
        assert_eq!(unescape_mount_field("/a\\040b/c"), "/a b/c");
        assert_eq!(unescape_mount_field("/plain"), "/plain");
        assert_eq!(unescape_mount_field("/trailing\\04"), "/trailing\\04");
        let table = "proc /proc proc rw 0 0\nsquashfuse_ll /x/my\\040env/.cvenv/mnt/1 fuse.squashfuse_ll ro 0 0\n\
                     squashfuse_ll /x/my\\040env/.cvenv/mnt/1 fuse.squashfuse_ll ro 0 0\n";
        assert_eq!(mount_count_in(table, Path::new("/x/my env/.cvenv/mnt/1")), 2);
        assert_eq!(mount_count_in(table, Path::new("/x/my env/.cvenv/mnt")), 0);
    }

    #[test]
    fn activation_split() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"PATH": "/e/bin:/e/sbin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                "CONDA_PREFIX": "/e", "PIXI_IN_SHELL": "1", "HOME": "/h", "CONDA_SHLVL": 1}"#,
        )
        .unwrap();
        let (set, pre) = split_activation(v.as_object().unwrap(), BASE_PATH);
        assert_eq!(pre, vec!["/e/bin", "/e/sbin"]);
        assert_eq!(set.get("CONDA_PREFIX").map(String::as_str), Some("/e"));
        assert_eq!(set.get("CONDA_SHLVL").map(String::as_str), Some("1"));
        assert!(!set.contains_key("PIXI_IN_SHELL") && !set.contains_key("HOME") && !set.contains_key("PATH"));
    }

    #[test]
    fn mount_root_gc() {
        let root = std::env::temp_dir().join(format!("cvenv-test-gc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let id = root.join("0123456789abcdef");
        let old = std::time::SystemTime::now() - Duration::from_secs(30 * 86400);
        let mk = |build: &str, retired_at: Option<std::time::SystemTime>| {
            fs::create_dir_all(id.join(build)).unwrap();
            if let Some(t) = retired_at {
                let m = id.join(format!("{}{}", build, RETIRED_SUFFIX));
                fs::write(&m, "retired\n").unwrap();
                fs::File::options().write(true).open(&m).unwrap().set_modified(t).unwrap();
            }
        };
        mk("old-retired", Some(old)); // removed
        mk("new-retired", Some(std::time::SystemTime::now())); // too recent: kept
        mk("old-busy", Some(old)); // not empty: kept
        fs::write(id.join("old-busy").join("f"), "x").unwrap();
        mk("current", None); // never retired: kept
        fs::write(id.join("gone").with_extension("retired"), "").unwrap(); // marker without dir
        fs::File::options().write(true).open(id.join("gone.retired")).unwrap().set_modified(old).unwrap();
        std::os::unix::fs::symlink(root.join("no-such-scratch"), id.join("crashed-build")).unwrap();
        std::os::unix::fs::symlink(&root, id.join("running-build")).unwrap(); // target exists: kept

        gc_mount_root(&root);
        assert!(!id.join("old-retired").exists() && !id.join("old-retired.retired").exists());
        assert!(id.join("new-retired").is_dir() && id.join("new-retired.retired").exists());
        assert!(id.join("old-busy").is_dir() && id.join("old-busy.retired").exists());
        assert!(id.join("current").is_dir());
        assert!(!id.join("gone.retired").exists());
        assert!(fs::symlink_metadata(id.join("crashed-build")).is_err());
        assert!(fs::symlink_metadata(id.join("running-build")).is_ok());

        // once everything is gone, the env's directory goes too
        let empty = root.join("fedcba9876543210");
        fs::create_dir_all(&empty).unwrap();
        gc_mount_root(&root);
        assert!(!empty.exists() && id.exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn distro_lib_dirs() {
        let root = std::env::temp_dir().join(format!("cvenv-test-sqfs-{}", std::process::id()));
        let triple = format!("{}-linux-gnu", std::env::consts::ARCH);
        fs::create_dir_all(root.join("usr/bin")).unwrap();
        fs::create_dir_all(root.join("usr/lib").join(&triple)).unwrap();
        fs::create_dir_all(root.join("lib").join(&triple)).unwrap();
        let bin = root.join("usr/bin/squashfuse_ll");
        fs::write(&bin, "").unwrap();
        let dirs = squashfuse_lib_dirs(&bin);
        let root = realpath(&root);
        assert!(dirs.contains(&root.join("usr/lib").join(&triple)));
        assert!(dirs.contains(&root.join("lib").join(&triple)));
        assert!(squashfuse_lib_dirs(Path::new("/usr/bin/squashfuse_ll")).is_empty());
        let _ = fs::remove_dir_all(&root);
    }
}
