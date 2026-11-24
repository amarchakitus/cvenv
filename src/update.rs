//! `cvenv self-update`: replace the running binary with a GitHub release.
//!
//! Downloads go through curl (or wget), like installation.sh, so the static binary needs no
//! HTTPS client. The new binary is verified (sha256 published next to it, and it must run
//! and report the expected version) before it atomically replaces the current one; shims are
//! symlinks to the binary, so they follow automatically.

use crate::config;
use crate::util::{info, is_writable, sha256_file, warn, which};
use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const DEFAULT_REPO: &str = "amarchakitus/cvenv";

/// Base URL of the releases: `$CVENV_RELEASES_URL`, or `https://github.com/<repo>/releases`.
fn releases_url() -> String {
    if let Ok(u) = std::env::var("CVENV_RELEASES_URL") {
        if !u.is_empty() {
            return u.trim_end_matches('/').to_string();
        }
    }
    let repo = config::value("repo").unwrap_or_else(|| DEFAULT_REPO.into());
    format!("https://github.com/{}/releases", repo.trim_matches('/'))
}

/// Release asset name suffix for this machine.
fn triple() -> Option<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Some("x86_64-unknown-linux-gnu"),
        "aarch64" => Some("aarch64-unknown-linux-gnu"),
        _ => None,
    }
}

/// "v0.3.0", "0.3.0" or "cvenv 0.3.0" -> [0, 3, 0]
pub fn parse_version(s: &str) -> Option<Vec<u64>> {
    let word = s.split_whitespace().last()?;
    let v = word.strip_prefix('v').unwrap_or(word);
    let parts: Option<Vec<u64>> = v.split('.').map(|p| p.parse().ok()).collect();
    parts.filter(|p| !p.is_empty())
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

fn normalize_tag(v: &str) -> String {
    let v = v.trim();
    if v.starts_with('v') { v.to_string() } else { format!("v{}", v) }
}

/// The tag in a `.../releases/tag/<tag>` URL.
fn tag_from_url(url: &str) -> Option<String> {
    let (_, tag) = url.trim().trim_end_matches('/').rsplit_once("/tag/")?;
    (!tag.is_empty() && !tag.contains('/')).then(|| tag.to_string())
}

/// Download `url` to `dest` with curl or wget.
fn download(url: &str, dest: &Path) -> Result<()> {
    let ok = if let Some(curl) = which("curl") {
        // -s without -S: cvenv reports failures itself
        Command::new(curl).args(["-fsL", "--retry", "3", "-o"]).arg(dest).arg(url).stdin(Stdio::null()).stderr(Stdio::null()).status()?.success()
    } else if let Some(wget) = which("wget") {
        Command::new(wget).args(["-q", "-O"]).arg(dest).arg(url).stdin(Stdio::null()).stderr(Stdio::null()).status()?.success()
    } else {
        bail!("curl or wget is needed to download updates");
    };
    if !ok {
        let _ = fs::remove_file(dest);
        bail!("download failed: {}", url);
    }
    Ok(())
}

/// The newest release tag: follow the `releases/latest` redirect (no API rate limit), or
/// ask the GitHub API when curl isn't available.
fn latest_tag(base: &str) -> Result<String> {
    if let Some(curl) = which("curl") {
        let out = Command::new(curl)
            .args(["-fsSLI", "--retry", "3", "-o", "/dev/null", "-w", "%{url_effective}"])
            .arg(format!("{}/latest", base))
            .stdin(Stdio::null())
            .output()?;
        if let Some(tag) = out.status.success().then(|| tag_from_url(&String::from_utf8_lossy(&out.stdout))).flatten() {
            return Ok(tag);
        }
    }
    if let Some(repo) = base.strip_prefix("https://github.com/").and_then(|r| r.strip_suffix("/releases")) {
        let tmp = std::env::temp_dir().join(format!("cvenv-latest-{}.json", std::process::id()));
        let res = download(&format!("https://api.github.com/repos/{}/releases/latest", repo), &tmp);
        let text = fs::read_to_string(&tmp).unwrap_or_default();
        let _ = fs::remove_file(&tmp);
        res?;
        let v: serde_json::Value = serde_json::from_str(&text).context("parsing the GitHub API response")?;
        if let Some(t) = v.get("tag_name").and_then(|t| t.as_str()) {
            return Ok(t.to_string());
        }
    }
    bail!("could not find the latest release at {}", base)
}

/// Removes the file on drop unless disarmed.
struct TempFile(Option<PathBuf>);
impl Drop for TempFile {
    fn drop(&mut self) {
        if let Some(p) = &self.0 {
            let _ = fs::remove_file(p);
        }
    }
}

/// Download release `tag` from `base`, verify it and atomically replace `target` with it.
/// Returns the new binary's version string. On any error `target` is left untouched.
pub fn install_release(base: &str, tag: &str, triple: &str, target: &Path) -> Result<String> {
    let dir = target.parent().ok_or_else(|| anyhow!("{} has no parent directory", target.display()))?;
    let url = format!("{}/download/{}/cvenv-{}", base, tag, triple);
    let new = TempFile(Some(dir.join(format!(".cvenv-update-{}", std::process::id()))));
    let sum = TempFile(Some(dir.join(format!(".cvenv-update-{}.sha256", std::process::id()))));
    let (new_path, sum_path) = (new.0.clone().unwrap(), sum.0.clone().unwrap());

    download(&url, &new_path).with_context(|| format!("release {} has no binary for {}", tag, triple))?;
    download(&format!("{}.sha256", url), &sum_path).context("the release publishes no checksum; refusing to install it")?;
    let want = fs::read_to_string(&sum_path)?.split_whitespace().next().unwrap_or_default().to_lowercase();
    let got = sha256_file(&new_path)?;
    if want != got {
        bail!("checksum mismatch for {} (expected {}, got {})", url, want, got);
    }

    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&new_path, fs::Permissions::from_mode(0o755))?;
    let out = Command::new(&new_path).arg("--version").stdin(Stdio::null()).output();
    let version = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => bail!("the downloaded binary does not run on this machine"),
    };
    if parse_version(&version) != parse_version(tag) {
        bail!("the downloaded binary reports '{}', not {}", version, tag);
    }
    // rename(2) over the old binary: atomic; running commands keep the old file
    fs::rename(&new_path, target).with_context(|| format!("replacing {}", target.display()))?;
    let mut new = new;
    new.0 = None;
    Ok(version)
}

/// What installing `tag` over `current` does: (message, is it a downgrade?).
fn action_message(current: &str, tag: &str) -> (String, bool) {
    let target = tag.trim_start_matches('v');
    if is_newer(tag, current) {
        (format!("updating cvenv {} -> {}", current, target), false)
    } else if parse_version(tag) == parse_version(current) {
        (format!("reinstalling cvenv {}", current), false)
    } else {
        (format!("downgrading cvenv {} -> {}", current, target), true)
    }
}

pub fn self_update(check: bool, version: Option<String>, force: bool) -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    let exe = config::exe_path();
    let base = releases_url();
    let triple = triple().ok_or_else(|| anyhow!("no prebuilt binaries for {}; build from source", std::env::consts::ARCH))?;

    let explicit = version.is_some();
    let tag = match version {
        Some(v) => normalize_tag(&v),
        None => latest_tag(&base)?,
    };
    let newer = is_newer(&tag, current);
    let same = parse_version(&tag) == parse_version(current);
    if check {
        if newer {
            println!("cvenv {} is available (installed: {}). Update with `cvenv self-update`.", tag.trim_start_matches('v'), current);
        } else {
            println!("cvenv {} is up to date.", current);
        }
        return Ok(());
    }
    if !explicit && !newer && !force {
        info(&format!("cvenv {} is up to date (latest release: {})", current, tag));
        return Ok(());
    }
    if explicit && same && !force {
        info(&format!("cvenv {} is already installed (use --force to reinstall it)", current));
        return Ok(());
    }
    if let Some(repo) = config::repo_dir() {
        if repo.join("Cargo.toml").is_file() && repo.join(".git").exists() && !force {
            bail!(
                "{} was built from the checkout {}; update it with `git pull && pixi run build` \
                 (or pass --force to replace it with the release binary)",
                exe.display(),
                repo.display()
            );
        }
    }
    let dir = exe.parent().unwrap_or(Path::new("/"));
    if !is_writable(dir) {
        bail!(
            "can't write to {}: cvenv is installed where you have no write access. Ask its owner \
             to update it, or install your own copy with installation.sh",
            dir.display()
        );
    }
    match action_message(current, &tag) {
        (msg, false) => info(&msg),
        (msg, true) => warn(&msg),
    }
    let new_version = install_release(&base, &tag, triple, &exe)?;
    info(&format!("installed {} at {}", new_version, exe.display()));
    info("pixi, squashfuse and your settings are unchanged; re-run installation.sh to refresh those too");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("v0.3.0"), Some(vec![0, 3, 0]));
        assert_eq!(parse_version("cvenv 0.10.1"), Some(vec![0, 10, 1]));
        assert_eq!(parse_version("latest"), None);
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(is_newer("v1.0.0", "0.3.0"));
        assert!(!is_newer("v0.3.0", "0.3.0"));
        assert!(!is_newer("v0.2.0", "0.3.0"));
        assert!(!is_newer("garbage", "0.3.0"));
        assert_eq!(normalize_tag("0.3.0"), "v0.3.0");
        assert_eq!(normalize_tag("v0.3.0"), "v0.3.0");
    }

    #[test]
    fn action_messages() {
        assert_eq!(action_message("0.4.0", "v0.5.0"), ("updating cvenv 0.4.0 -> 0.5.0".to_string(), false));
        assert_eq!(action_message("0.4.0", "v0.4.0"), ("reinstalling cvenv 0.4.0".to_string(), false));
        assert_eq!(action_message("0.4.0", "v0.3.0"), ("downgrading cvenv 0.4.0 -> 0.3.0".to_string(), true));
    }

    #[test]
    fn latest_redirect() {
        assert_eq!(tag_from_url("https://github.com/o/r/releases/tag/v0.3.0").as_deref(), Some("v0.3.0"));
        assert_eq!(tag_from_url("https://github.com/o/r/releases/tag/v0.3.0/\n").as_deref(), Some("v0.3.0"));
        assert_eq!(tag_from_url("https://github.com/o/r/releases"), None);
    }

    /// A fake release directory served through file:// URLs (curl supports them).
    struct FakeRelease {
        root: PathBuf,
    }

    impl FakeRelease {
        fn new(name: &str) -> FakeRelease {
            let root = std::env::temp_dir().join(format!("cvenv-test-update-{}-{}", name, std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("install")).unwrap();
            FakeRelease { root }
        }
        fn base(&self) -> String {
            format!("file://{}", self.root.join("releases").display())
        }
        /// Publish `script` as the binary of `tag` (with a correct checksum unless `bad_sum`).
        fn publish(&self, tag: &str, script: &str, bad_sum: bool) {
            let d = self.root.join("releases/download").join(tag);
            fs::create_dir_all(&d).unwrap();
            let bin = d.join("cvenv-test-triple");
            fs::write(&bin, script).unwrap();
            let sum = if bad_sum { "0".repeat(64) } else { sha256_file(&bin).unwrap() };
            fs::write(d.join("cvenv-test-triple.sha256"), format!("{}  cvenv-test-triple\n", sum)).unwrap();
        }
        fn target(&self) -> PathBuf {
            let t = self.root.join("install/cvenv");
            if !t.exists() {
                fs::write(&t, "old binary").unwrap();
            }
            t
        }
    }

    impl Drop for FakeRelease {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn no_leftovers(dir: &Path) -> bool {
        fs::read_dir(dir).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().starts_with(".cvenv-update"))
    }

    #[test]
    fn install_replaces_binary() {
        if which("curl").is_none() {
            return;
        }
        let r = FakeRelease::new("ok");
        r.publish("v9.9.9", "#!/bin/sh\necho 'cvenv 9.9.9'\n", false);
        let t = r.target();
        let v = install_release(&r.base(), "v9.9.9", "test-triple", &t).unwrap();
        assert_eq!(v, "cvenv 9.9.9");
        assert!(fs::read_to_string(&t).unwrap().contains("9.9.9"));
        assert!(no_leftovers(t.parent().unwrap()));
    }

    #[test]
    fn install_rejects_bad_releases() {
        if which("curl").is_none() {
            return;
        }
        let r = FakeRelease::new("bad");
        let t = r.target();
        r.publish("v1.0.0", "#!/bin/sh\necho 'cvenv 1.0.0'\n", true); // wrong checksum
        r.publish("v2.0.0", "#!/bin/sh\nexit 1\n", false); // doesn't run
        r.publish("v3.0.0", "#!/bin/sh\necho 'cvenv 2.9.0'\n", false); // wrong version
        for (tag, why) in [("v1.0.0", "checksum"), ("v2.0.0", "does not run"), ("v3.0.0", "reports"), ("v4.0.0", "no binary")] {
            let e = install_release(&r.base(), tag, "test-triple", &t).unwrap_err();
            assert!(format!("{:#}", e).contains(why), "{}: {:#}", tag, e);
            assert_eq!(fs::read_to_string(&t).unwrap(), "old binary", "{} touched the binary", tag);
        }
        assert!(no_leftovers(t.parent().unwrap()));
    }
}
