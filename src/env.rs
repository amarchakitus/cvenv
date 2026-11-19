//! Registry of envs, the `Env` model, manifest access, fingerprints and staleness.

use crate::config::{registry_path, config_dir};
use crate::util::{absolute, is_under, lock_file, read_json, realpath, sha256_bytes, sha256_file, warn, write_json};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as J};
use std::cell::OnceCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const MANIFEST_NAMES: &[&str] = &["pixi.toml", "pyproject.toml"];
pub const ART_DIR: &str = ".cvenv";
pub const IMAGE_NAME: &str = "image.sif";
pub const META_NAME: &str = "image.json";
pub const SHIM_LINK: &str = ".cvenv-shim";
/// Per-env descriptor next to the image, so an env can be used from its directory
/// alone (shared envs, other users, no registry entry).
pub const ENV_FILE: &str = "env.json";
/// Settings file of wrapped envs (they have no pixi manifest).
pub const WRAP_CFG: &str = "cvenv.toml";

/// Workspace metadata that does not influence what gets installed.
const NON_DEP_WORKSPACE_KEYS: &[&str] = &[
    "description", "authors", "version", "license", "license-file", "readme",
    "homepage", "repository", "documentation",
];
/// `[tool.cvenv]` keys that change the image (everything else is runtime-only).
pub const BUILD_CFG_KEYS: &[&str] = &["base-image", "post-commands"];

fn default_pixi_env() -> String {
    "default".into()
}

fn is_pixi(k: &Kind) -> bool {
    *k == Kind::Pixi
}

/// What an env is built from.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// a pixi workspace installed into an image by `cvenv rebuild`
    #[default]
    Pixi,
    /// an existing container image (`cvenv wrap`)
    Wrap,
    /// an existing on-disk installation squashed into an image (`cvenv wrap-install`)
    WrapInstall,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Pixi => "pixi",
            Kind::Wrap => "wrap",
            Kind::WrapInstall => "wrap-install",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Record {
    pub path: PathBuf,
    /// pixi manifest (pixi envs) or `<env>/cvenv.toml` (wrapped envs)
    pub manifest: PathBuf,
    #[serde(default, skip_serializing_if = "is_pixi")]
    pub kind: Kind,
    #[serde(default = "default_pixi_env")]
    pub pixi_env: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct Registry {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub envs: BTreeMap<String, Record>,
}

pub fn load_registry() -> Registry {
    let mut r: Registry = read_json(&registry_path()).unwrap_or_default();
    r.version = 1;
    r
}

/// Locked read-modify-write of the registry.
pub fn update_registry<F: FnOnce(&mut Registry) -> Result<()>>(f: F) -> Result<()> {
    let _l = lock_file(&config_dir().join("registry.json.lock"), true, "the registry")?;
    let mut reg = load_registry();
    f(&mut reg)?;
    write_json(&registry_path(), &reg)
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Fingerprint {
    pub deps: String,
    pub lock: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct StatSig {
    pub manifest: Option<[i64; 2]>,
    pub lock: Option<[i64; 2]>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Meta {
    pub kind: Kind,
    pub name: String,
    pub pixi_env: String,
    pub manifest: PathBuf,
    pub built_at: String,
    pub build_seconds: f64,
    pub base_image: String,
    pub pixi_version: Option<String>,
    pub apptainer: String,
    pub cvenv_version: String,
    pub image_size: u64,
    pub fingerprint: Fingerprint,
    pub stat: StatSig,
    pub bins: Vec<String>,
}

/// `<env>/.cvenv/env.json`
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct EnvFile {
    pub name: String,
    #[serde(default)]
    pub kind: Kind,
    /// relative to the env directory when inside it
    pub manifest: PathBuf,
    #[serde(default = "default_pixi_env")]
    pub pixi_env: String,
}

pub struct Env {
    pub name: String,
    pub path: PathBuf,
    pub manifest: PathBuf,
    pub pixi_env: String,
    pub kind: Kind,
    /// found via its directory (env.json), not this user's registry
    pub unregistered: bool,
    data: OnceCell<toml::Table>,
}

impl Env {
    pub fn new(name: &str, rec: &Record) -> Env {
        Env {
            name: name.to_string(),
            path: rec.path.clone(),
            manifest: rec.manifest.clone(),
            pixi_env: rec.pixi_env.clone(),
            kind: rec.kind,
            unregistered: false,
            data: OnceCell::new(),
        }
    }

    /// Load an env from its directory via `.cvenv/env.json` (no registry needed).
    pub fn from_dir(dir: &Path) -> Result<Env> {
        let dir = realpath(dir);
        let f = dir.join(ART_DIR).join(ENV_FILE);
        let ef: EnvFile = read_json(&f).ok_or_else(|| {
            anyhow!("{} is not a cvenv env (no readable {})", dir.display(), f.display())
        })?;
        let manifest = if ef.manifest.is_absolute() { ef.manifest.clone() } else { dir.join(&ef.manifest) };
        let rec = Record { path: dir, manifest, kind: ef.kind, pixi_env: ef.pixi_env, created: None };
        let mut e = Env::new(&ef.name, &rec);
        e.unregistered = load_registry().envs.get(&ef.name).map(|r| realpath(&r.path) != rec.path).unwrap_or(true);
        Ok(e)
    }

    pub fn record(&self) -> Record {
        Record {
            path: self.path.clone(),
            manifest: self.manifest.clone(),
            kind: self.kind,
            pixi_env: self.pixi_env.clone(),
            created: None,
        }
    }

    /// Write `.cvenv/env.json` (best effort: shared envs may not be writable by us).
    pub fn write_env_file(&self) -> Result<()> {
        let manifest = self.manifest.strip_prefix(&self.path).map(Path::to_path_buf).unwrap_or_else(|_| self.manifest.clone());
        let ef = EnvFile { name: self.name.clone(), kind: self.kind, manifest, pixi_env: self.pixi_env.clone() };
        write_json(&self.art().join(ENV_FILE), &ef)
    }

    pub fn is_pixi(&self) -> bool {
        self.kind == Kind::Pixi
    }

    /// The `--env` value that selects this env for the current user.
    pub fn selector(&self) -> String {
        if self.unregistered {
            self.path.display().to_string()
        } else {
            self.name.clone()
        }
    }

    pub fn art(&self) -> PathBuf {
        self.path.join(ART_DIR)
    }
    pub fn image(&self) -> PathBuf {
        self.art().join(IMAGE_NAME)
    }
    pub fn meta_path(&self) -> PathBuf {
        self.art().join(META_NAME)
    }
    pub fn shim_dir(&self) -> PathBuf {
        self.art().join("bin")
    }
    pub fn manifest_dir(&self) -> PathBuf {
        self.manifest.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/"))
    }
    pub fn lock_path(&self) -> PathBuf {
        self.manifest_dir().join("pixi.lock")
    }
    pub fn is_pyproject(&self) -> bool {
        self.manifest.file_name().map(|n| n == "pyproject.toml").unwrap_or(false)
    }
    pub fn manifest_inside(&self) -> bool {
        is_under(&self.manifest, &self.path)
    }
    pub fn built(&self) -> bool {
        self.image().is_file()
    }
    pub fn meta(&self) -> Option<Meta> {
        read_json(&self.meta_path())
    }
    pub fn is_active(&self) -> bool {
        std::env::var("CVENV_ACTIVE").map(|a| a == self.name).unwrap_or(false)
    }
    /// `cvenv rebuild` or `cvenv rebuild --env NAME`, depending on what is active.
    pub fn rebuild_hint(&self) -> String {
        if self.is_active() {
            "cvenv rebuild".into()
        } else {
            format!("cvenv rebuild --env {}", self.selector())
        }
    }

    pub fn data(&self) -> Result<&toml::Table> {
        if let Some(d) = self.data.get() {
            return Ok(d);
        }
        if !self.is_pixi() && !self.manifest.is_file() {
            // wrapped envs: the settings file is optional
            let _ = self.data.set(toml::Table::new());
            return Ok(self.data.get().unwrap());
        }
        let text = fs::read_to_string(&self.manifest)
            .with_context(|| format!("manifest of env '{}' not found: {}", self.name, self.manifest.display()))?;
        let t: toml::Table =
            toml::from_str(&text).map_err(|e| anyhow!("could not parse {}: {}", self.manifest.display(), e))?;
        let _ = self.data.set(t);
        Ok(self.data.get().unwrap())
    }

    /// The `[tool.cvenv]` table (pixi envs) or the whole `cvenv.toml` (wrapped envs).
    pub fn cfg(&self) -> toml::Table {
        if !self.is_pixi() {
            return self.data().cloned().unwrap_or_default();
        }
        self.data()
            .ok()
            .and_then(|d| d.get("tool"))
            .and_then(|t| t.get("cvenv"))
            .and_then(|c| c.as_table())
            .cloned()
            .unwrap_or_default()
    }

    /// The table holding pixi keys (whole file for pixi.toml, `[tool.pixi]` for pyproject).
    pub fn pixi_table(&self) -> toml::Table {
        let d = match self.data() {
            Ok(d) => d,
            Err(_) => return toml::Table::new(),
        };
        if self.is_pyproject() {
            d.get("tool").and_then(|t| t.get("pixi")).and_then(|p| p.as_table()).cloned().unwrap_or_default()
        } else {
            d.clone()
        }
    }

    pub fn cfg_str_list(&self, key: &str) -> Vec<String> {
        match self.cfg().get(key) {
            Some(toml::Value::Array(a)) => a.iter().map(toml_to_string).collect(),
            Some(toml::Value::String(s)) => vec![s.clone()],
            _ => vec![],
        }
    }
}

pub fn toml_to_string(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

pub fn valid_name(n: &str) -> bool {
    let mut chars = n.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

/// Pick the env to operate on: explicit name > active env > env containing the cwd.
pub fn resolve_env(name: Option<&str>, allow_active: bool, allow_cwd: bool) -> Result<Env> {
    let reg = load_registry();
    if let Some(n) = name {
        // a path selects an env by its directory (works for envs shared by other users)
        if n.contains('/') || n == "." || n == ".." {
            return Env::from_dir(&crate::util::expand(n));
        }
        return match reg.envs.get(n) {
            Some(rec) => Ok(Env::new(n, rec)),
            None => {
                let known: Vec<&String> = reg.envs.keys().collect();
                bail!(
                    "unknown env '{}'. Registered envs: {}",
                    n,
                    if known.is_empty() { "(none)".to_string() } else { known.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ") }
                )
            }
        };
    }
    if allow_active {
        if let Ok(n) = std::env::var("CVENV_ACTIVE") {
            if !n.is_empty() {
                if let Ok(dir) = std::env::var("CVENV_ACTIVE_DIR") {
                    if let Ok(e) = Env::from_dir(Path::new(&dir)) {
                        if e.name == n {
                            return Ok(e);
                        }
                    }
                }
                if let Some(rec) = reg.envs.get(&n) {
                    return Ok(Env::new(&n, rec));
                }
                warn(&format!("active env '{}' is not registered any more", n));
            }
        }
    }
    if allow_cwd {
        if let Ok(cwd) = std::env::current_dir() {
            if let Some(e) = env_for_dir(&cwd, &reg) {
                return Ok(e);
            }
        }
    }
    bail!("no env selected: activate one (`cvenv activate NAME`), pass --env NAME (or --env /path/to/env), or run from inside an env directory")
}

pub fn env_for_dir(d: &Path, reg: &Registry) -> Option<Env> {
    let d = realpath(d);
    let mut best: Option<(&String, usize)> = None;
    for (n, rec) in &reg.envs {
        let mdir = rec.manifest.parent().map(Path::to_path_buf).unwrap_or_default();
        for root in [&rec.path, &mdir] {
            let r = realpath(root);
            if is_under(&d, &r) {
                let len = r.as_os_str().len();
                if best.map(|b| len > b.1).unwrap_or(true) {
                    best = Some((n, len));
                }
            }
        }
    }
    best.map(|(n, _)| Env::new(n, &reg.envs[n]))
}

/// Find the env owning a shim directory (`<env>/.cvenv/bin`, possibly reached through
/// symlinks): this user's registry first, then the env's own `env.json`.
pub fn env_for_shim_dir(shim_dir: &Path) -> Option<Env> {
    let real = realpath(shim_dir);
    let env_dir = real.parent()?.parent()?.to_path_buf();
    let reg = load_registry();
    for (n, rec) in &reg.envs {
        if realpath(&rec.path) == env_dir {
            return Some(Env::new(n, rec));
        }
    }
    Env::from_dir(&env_dir).ok()
}

pub fn find_manifest(p: &Path) -> Option<PathBuf> {
    let p = absolute(p);
    if p.is_file() {
        return Some(p);
    }
    MANIFEST_NAMES.iter().map(|n| p.join(n)).find(|c| c.is_file())
}

// ---------------------------------------------------------------------------
// fingerprint / staleness
// ---------------------------------------------------------------------------

fn strip_tasks(v: &mut J) {
    if let J::Object(m) = v {
        m.remove("tasks");
        if let Some(J::Object(targets)) = m.get_mut("target") {
            for t in targets.values_mut() {
                strip_tasks(t);
            }
        }
    }
}

/// The parts of the manifest that determine what ends up in the image.
pub fn deps_view(env: &Env) -> Result<J> {
    let data = serde_json::to_value(env.data()?)?;
    let pyproject = env.is_pyproject();
    let mut view = if pyproject {
        let proj = data.get("project").cloned().unwrap_or(J::Null);
        let mut sub = serde_json::Map::new();
        for k in ["name", "dependencies", "optional-dependencies", "requires-python"] {
            if let Some(v) = proj.get(k) {
                sub.insert(k.into(), v.clone());
            }
        }
        json!({
            "pixi": data.get("tool").and_then(|t| t.get("pixi")).cloned().unwrap_or(json!({})),
            "project": sub,
            "dependency-groups": data.get("dependency-groups").cloned().unwrap_or(J::Null),
        })
    } else {
        let mut d = data.clone();
        if let J::Object(m) = &mut d {
            m.remove("tool");
        }
        d
    };
    {
        let pix: &mut J = if pyproject { view.get_mut("pixi").unwrap() } else { &mut view };
        for ws in ["workspace", "project"] {
            if ws == "workspace" || !pyproject {
                if let Some(J::Object(w)) = pix.get_mut(ws) {
                    for k in NON_DEP_WORKSPACE_KEYS {
                        w.remove(*k);
                    }
                }
            }
        }
        strip_tasks(pix);
        if let Some(J::Object(features)) = pix.get_mut("feature") {
            for f in features.values_mut() {
                strip_tasks(f);
            }
        }
    }
    let cfg = serde_json::to_value(env.cfg())?;
    let mut build = serde_json::Map::new();
    for k in BUILD_CFG_KEYS {
        if let Some(v) = cfg.get(*k) {
            build.insert((*k).into(), v.clone());
        }
    }
    if let J::Object(m) = &mut view {
        m.insert("__cvenv_build__".into(), J::Object(build));
        m.insert("__pixi_env__".into(), J::String(env.pixi_env.clone()));
    }
    Ok(view)
}

pub fn compute_fingerprint(env: &Env) -> Result<Fingerprint> {
    // serde_json maps are ordered (BTreeMap), so this serialisation is canonical.
    let blob = serde_json::to_string(&deps_view(env)?)?;
    let lock = env.lock_path();
    Ok(Fingerprint {
        deps: sha256_bytes(blob.as_bytes()),
        lock: if lock.is_file() { Some(sha256_file(&lock)?) } else { None },
    })
}

fn stat_one(p: &Path) -> Option<[i64; 2]> {
    use std::os::unix::fs::MetadataExt;
    let m = fs::metadata(p).ok()?;
    Some([m.mtime() * 1_000_000 + m.mtime_nsec() / 1000, m.size() as i64])
}

pub fn stat_sig(env: &Env) -> StatSig {
    StatSig { manifest: stat_one(&env.manifest), lock: stat_one(&env.lock_path()) }
}

#[derive(Default)]
pub struct Staleness {
    pub reasons: Vec<String>,
    /// The image content no longer matches the manifest/lock.
    pub changed: bool,
    pub not_built: bool,
    pub moved: bool,
}

pub fn staleness(env: &Env) -> Staleness {
    let mut s = Staleness::default();
    if !env.built() {
        s.not_built = true;
        s.reasons.push(format!("env '{}' has not been built yet - run `{}`", env.name, env.rebuild_hint()));
        return s;
    }
    if !env.is_pixi() {
        return s;
    }
    let mut meta = match env.meta() {
        Some(m) => m,
        None => {
            s.changed = true;
            s.reasons.push(format!("no build metadata found next to the image - run `{}`", env.rebuild_hint()));
            return s;
        }
    };
    if meta.pixi_env != env.pixi_env {
        s.changed = true;
        s.reasons.push(format!(
            "image was built for pixi environment '{}' but the env now uses '{}'",
            meta.pixi_env, env.pixi_env
        ));
    }
    if crate::util::normalize(&meta.manifest) != crate::util::normalize(&env.manifest) {
        s.moved = true;
        s.reasons.push(format!(
            "env was moved since it was built (from {}); it still works, but pixi tasks and \
             editable/path dependencies need `{}`",
            meta.manifest.parent().unwrap_or(Path::new("?")).display(),
            env.rebuild_hint()
        ));
    }
    let sig = stat_sig(env);
    if sig == meta.stat {
        return s;
    }
    if !env.manifest.is_file() {
        s.changed = true;
        s.reasons.push(format!("manifest {} is missing", env.manifest.display()));
        return s;
    }
    let fp = match compute_fingerprint(env) {
        Ok(f) => f,
        Err(e) => {
            s.changed = true;
            s.reasons.push(format!("could not check the manifest: {:#}", e));
            return s;
        }
    };
    let manifest_name = env.manifest.file_name().unwrap_or_default().to_string_lossy().to_string();
    if fp.deps != meta.fingerprint.deps {
        s.changed = true;
        s.reasons.push(format!("dependencies in {} changed since the image was built", manifest_name));
    } else if fp.lock != meta.fingerprint.lock {
        s.changed = true;
        s.reasons.push("pixi.lock changed since the image was built".into());
    } else {
        // only irrelevant edits (tasks, description, ...): refresh the stat cache
        meta.stat = sig;
        let _ = write_json(&env.meta_path(), &meta);
    }
    s
}

pub fn warn_if_stale(env: &Env) {
    if std::env::var_os("CVENV_NO_STALE_WARNING").is_some() {
        return;
    }
    let s = staleness(env);
    for r in &s.reasons {
        warn(&format!("[{}] {}", env.name, r));
    }
    if s.changed && env.built() {
        warn(&format!(
            "[{}] the image does not match the manifest; run `{}` to apply the changes",
            env.name,
            env.rebuild_hint()
        ));
    }
}

/// Task names defined anywhere in the manifest (top level, targets, features).
pub fn task_names(env: &Env) -> Vec<String> {
    fn collect(t: &toml::Table, out: &mut Vec<String>) {
        if let Some(tasks) = t.get("tasks").and_then(|v| v.as_table()) {
            out.extend(tasks.keys().cloned());
        }
        if let Some(targets) = t.get("target").and_then(|v| v.as_table()) {
            for tv in targets.values() {
                if let Some(tt) = tv.as_table() {
                    if let Some(tasks) = tt.get("tasks").and_then(|v| v.as_table()) {
                        out.extend(tasks.keys().cloned());
                    }
                }
            }
        }
    }
    let pix = env.pixi_table();
    let mut out = vec![];
    collect(&pix, &mut out);
    if let Some(features) = pix.get("feature").and_then(|v| v.as_table()) {
        for f in features.values() {
            if let Some(ft) = f.as_table() {
                collect(ft, &mut out);
            }
        }
    }
    out
}

/// Highest CUDA version declared in `[system-requirements]` (top level or features).
pub fn cuda_requirement(env: &Env) -> Option<String> {
    fn grab(t: &toml::Table, out: &mut Vec<String>) {
        if let Some(sr) = t.get("system-requirements").and_then(|v| v.as_table()) {
            match sr.get("cuda") {
                Some(toml::Value::String(s)) => out.push(s.clone()),
                Some(toml::Value::Table(c)) => {
                    if let Some(v) = c.get("version") {
                        out.push(toml_to_string(v));
                    }
                }
                Some(other) => out.push(toml_to_string(other)),
                None => {}
            }
        }
    }
    let pix = env.pixi_table();
    let mut vals = vec![];
    grab(&pix, &mut vals);
    // newer pixi: platforms = [{ platform = "linux-64", cuda = "12.0" }, ...]
    for ws in ["workspace", "project"] {
        if let Some(plats) = pix.get(ws).and_then(|w| w.get("platforms")).and_then(|p| p.as_array()) {
            for p in plats {
                if let Some(c) = p.as_table().and_then(|t| t.get("cuda")) {
                    vals.push(toml_to_string(c));
                }
            }
        }
    }
    if let Some(features) = pix.get("feature").and_then(|v| v.as_table()) {
        for f in features.values() {
            if let Some(ft) = f.as_table() {
                grab(ft, &mut vals);
            }
        }
    }
    let key = |v: &String| -> Vec<u64> { v.split(|c: char| !c.is_ascii_digit()).filter_map(|x| x.parse().ok()).collect() };
    vals.into_iter().max_by(|a, b| key(a).cmp(&key(b)))
}
