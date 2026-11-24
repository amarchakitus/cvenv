//! Global configuration: env vars > ~/.config/cvenv/config.json > <repo>/etc/install.conf.

use crate::util::{expand, home, lock_file, read_json, write_json};
use anyhow::Result;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;

/// (key, environment override, description)
pub const KEYS: &[(&str, &str, &str)] = &[
    ("store", "CVENV_STORE", "default directory in which new envs are created"),
    ("backend", "CVENV_BACKEND", "image backend for builds: auto | apptainer | squashfuse"),
    ("apptainer", "CVENV_APPTAINER", "apptainer (or singularity) executable"),
    ("apptainer-conf", "CVENV_APPTAINER_CONF", "apptainer.conf (to detect site bind paths)"),
    ("squashfuse", "CVENV_SQUASHFUSE", "squashfuse_ll (or squashfuse) executable"),
    ("mount-timeout", "CVENV_MOUNT_TIMEOUT", "seconds an unused squashfuse mount stays (default 600)"),
    ("mount-mode", "CVENV_MOUNT_MODE", "squashfuse mounts: auto (systemd if running) | systemd | direct"),
    ("mount-root", "CVENV_MOUNT_ROOT", "where squashfuse images are mounted (default <store>/.cvenv-mnt)"),
    ("mount-retention", "CVENV_MOUNT_RETENTION", "days before an old build's mount directory is removed (default 14)"),
    ("pixi", "CVENV_PIXI", "pixi executable (also copied into images)"),
    ("build-tmpdir", "CVENV_BUILD_TMPDIR", "node-local scratch dir for builds (default /tmp)"),
    ("pixi-cache", "CVENV_PIXI_CACHE", "persistent package cache for builds"),
    ("base-image", "CVENV_BASE_IMAGE", "default base image for builds"),
    ("rebuild-mode", "CVENV_REBUILD_MODE", "where `cvenv rebuild` builds: local | slurm"),
    ("slurm-args", "CVENV_SLURM_ARGS", "sbatch arguments for `cvenv rebuild --slurm`"),
    ("repo", "CVENV_REPO", "GitHub repo that `cvenv self-update` installs releases from"),
];

pub const PATH_KEYS: &[&str] = &["store", "apptainer", "apptainer-conf", "squashfuse", "mount-root", "pixi", "build-tmpdir", "pixi-cache"];

pub fn key_env(key: &str) -> Option<&'static str> {
    KEYS.iter().find(|k| k.0 == key).map(|k| k.1)
}

pub fn config_dir() -> PathBuf {
    if let Ok(d) = std::env::var("CVENV_CONFIG_DIR") {
        if !d.is_empty() {
            return expand(&d);
        }
    }
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"));
    base.join("cvenv")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

pub fn registry_path() -> PathBuf {
    config_dir().join("registry.json")
}

/// The running cvenv binary (resolved).
pub fn exe_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(p).ok())
        .unwrap_or_else(|| PathBuf::from("cvenv"))
}

/// Repository checkout containing the binary (`<repo>/bin/cvenv`), if any.
pub fn repo_dir() -> Option<PathBuf> {
    let exe = exe_path();
    let repo = exe.parent()?.parent()?.to_path_buf();
    if repo.join("etc").is_dir() || repo.join("Cargo.toml").is_file() {
        Some(repo)
    } else {
        None
    }
}

fn unquote_shell(v: &str) -> String {
    let v = v.trim();
    if v.len() >= 2 && ((v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\''))) {
        return v[1..v.len() - 1].to_string();
    }
    // printf %q style backslash escapes
    let mut out = String::new();
    let mut esc = false;
    for c in v.chars() {
        if esc {
            out.push(c);
            esc = false;
        } else if c == '\\' {
            esc = true;
        } else {
            out.push(c);
        }
    }
    out
}

pub fn install_conf() -> &'static BTreeMap<String, String> {
    static CONF: OnceLock<BTreeMap<String, String>> = OnceLock::new();
    CONF.get_or_init(|| {
        let mut m = BTreeMap::new();
        if let Some(repo) = repo_dir() {
            if let Ok(s) = std::fs::read_to_string(repo.join("etc").join("install.conf")) {
                for line in s.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    let line = line.strip_prefix("export ").unwrap_or(line);
                    if let Some((k, v)) = line.split_once('=') {
                        m.insert(k.trim().to_string(), unquote_shell(v));
                    }
                }
            }
        }
        m
    })
}

fn install_key(key: &str) -> String {
    format!("CVENV_INSTALL_{}", key.to_uppercase().replace('-', "_"))
}

pub fn load_config() -> Map<String, Value> {
    read_json(&config_path()).unwrap_or_default()
}

pub enum Source {
    Env(&'static str),
    Config,
    InstallConf,
}

pub fn value_with_source(key: &str) -> Option<(String, Source)> {
    if let Some(env) = key_env(key) {
        if let Ok(v) = std::env::var(env) {
            if !v.is_empty() {
                return Some((v, Source::Env(env)));
            }
        }
    }
    if let Some(Value::String(v)) = load_config().get(key) {
        if !v.is_empty() {
            return Some((v.clone(), Source::Config));
        }
    }
    install_conf()
        .get(&install_key(key))
        .filter(|v| !v.is_empty())
        .map(|v| (v.clone(), Source::InstallConf))
}

pub fn value(key: &str) -> Option<String> {
    value_with_source(key).map(|(v, _)| v)
}

pub fn store_dir() -> PathBuf {
    value("store").map(|v| expand(&v)).unwrap_or_else(|| home().join(".cvenv").join("envs"))
}

pub fn set_value(key: &str, val: Option<&str>) -> Result<()> {
    let _l = lock_file(&config_dir().join("config.json.lock"), true, "the config")?;
    let mut cfg = load_config();
    match val {
        Some(v) => {
            cfg.insert(key.to_string(), Value::String(v.to_string()));
        }
        None => {
            cfg.remove(key);
        }
    }
    write_json(&config_path(), &cfg)
}
