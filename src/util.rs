//! Small helpers: output, paths, hashing, atomic writes, file locks.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Component, Path, PathBuf};

fn stderr_is_tty() -> bool {
    unsafe { libc::isatty(2) == 1 }
}

pub fn stdout_is_tty() -> bool {
    unsafe { libc::isatty(1) == 1 }
}

pub fn stdin_is_tty() -> bool {
    unsafe { libc::isatty(0) == 1 }
}

fn paint(code: &str, text: &str) -> String {
    if stderr_is_tty() && std::env::var_os("NO_COLOR").is_none() {
        format!("\x1b[{}m{}\x1b[0m", code, text)
    } else {
        text.to_string()
    }
}

pub fn info(msg: &str) {
    eprintln!("{} {}", paint("1;34", "cvenv:"), msg);
}

pub fn warn(msg: &str) {
    eprintln!("{} {}", paint("1;33", "cvenv: warning:"), msg);
}

pub fn error_line(msg: &str) {
    eprintln!("{} {}", paint("1;31", "cvenv: error:"), msg);
}

/// Print an error and exit with `code` (for user-facing usage errors).
pub fn die(msg: &str, code: i32) -> ! {
    error_line(msg);
    std::process::exit(code)
}

/// POSIX shell quoting.
pub fn q(s: &str) -> String {
    if !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\"'\"'"))
    }
}

pub fn qp(p: &Path) -> String {
    q(&p.to_string_lossy())
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// Lexically normalise a path (no symlink resolution).
pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

fn expand_vars(s: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '$' && i + 1 < chars.len() {
            let (name, next) = if chars[i + 1] == '{' {
                match chars[i + 2..].iter().position(|&c| c == '}') {
                    Some(end) => (chars[i + 2..i + 2 + end].iter().collect::<String>(), i + 3 + end),
                    None => (String::new(), i),
                }
            } else {
                let mut j = i + 1;
                while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
                (chars[i + 1..j].iter().collect::<String>(), j)
            };
            if !name.is_empty() {
                if let Ok(v) = std::env::var(&name) {
                    out.push_str(&v);
                    i = next;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Expand `~` and `$VARS`, make absolute and normalise.
pub fn expand(s: &str) -> PathBuf {
    let s = expand_vars(s);
    let p = if s == "~" {
        home()
    } else if let Some(rest) = s.strip_prefix("~/") {
        home().join(rest)
    } else {
        PathBuf::from(&s)
    };
    absolute(&p)
}

pub fn absolute(p: &Path) -> PathBuf {
    if p.is_absolute() {
        normalize(p)
    } else {
        normalize(&std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")).join(p))
    }
}

pub fn is_under(path: &Path, parent: &Path) -> bool {
    normalize(path).starts_with(normalize(parent))
}

pub fn realpath(p: &Path) -> PathBuf {
    fs::canonicalize(p).unwrap_or_else(|_| absolute(p))
}

pub fn sha256_bytes(b: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(b);
    hex(&h.finalize())
}

pub fn sha256_file(p: &Path) -> Result<String> {
    let mut f = File::open(p).with_context(|| format!("reading {}", p.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

pub fn human_size(n: u64) -> String {
    let mut v = n as f64;
    for unit in ["B", "K", "M", "G", "T"] {
        if v < 1024.0 || unit == "T" {
            return if unit == "B" { format!("{}B", n) } else { format!("{:.1}{}", v, unit) };
        }
        v /= 1024.0;
    }
    unreachable!()
}

/// Write a file atomically (temp file in the same directory + rename).
pub fn write_atomic(path: &Path, data: &[u8], mode: Option<u32>) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
    ));
    {
        let mut f = File::create(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
        f.write_all(data)?;
        f.flush()?;
    }
    if let Some(m) = mode {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(m))?;
    }
    fs::rename(&tmp, path).with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let s = fs::read_to_string(path).ok()?;
    serde_json::from_str(&s).ok()
}

pub fn write_json<T: serde::Serialize>(path: &Path, v: &T) -> Result<()> {
    let mut s = serde_json::to_string_pretty(v)?;
    s.push('\n');
    write_atomic(path, s.as_bytes(), None)
}

/// An exclusive `flock`, released on drop.
pub struct FileLock {
    _f: File,
}

pub fn lock_file(path: &Path, blocking: bool, what: &str) -> Result<FileLock> {
    if let Some(d) = path.parent() {
        fs::create_dir_all(d)?;
    }
    let f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening lock {}", path.display()))?;
    let flags = libc::LOCK_EX | if blocking { 0 } else { libc::LOCK_NB };
    let rc = unsafe { libc::flock(f.as_raw_fd(), flags) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EWOULDBLOCK) | Some(libc::EACCES) => {
                bail!("{} is locked by another cvenv process ({})", what, path.display())
            }
            // filesystems without flock support: carry on unlocked
            _ => {}
        }
    }
    Ok(FileLock { _f: f })
}

/// Remove a directory tree, fixing permissions on the way (fakeroot builds can
/// leave read-only directories behind).
pub fn remove_tree(p: &Path) {
    if fs::remove_dir_all(p).is_ok() || !p.exists() {
        return;
    }
    let _ = std::process::Command::new("chmod").arg("-R").arg("u+rwX").arg(p).status();
    let _ = fs::remove_dir_all(p);
}

pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for d in std::env::split_paths(&path) {
        let c = d.join(name);
        if is_executable(&c) {
            return Some(c);
        }
    }
    None
}

pub fn is_writable(p: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    CString::new(p.as_os_str().as_bytes()).map(|c| unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }).unwrap_or(false)
}

pub fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

pub fn now_iso() -> String {
    chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%z").to_string()
}

pub fn username() -> String {
    std::env::var("USER").unwrap_or_else(|_| unsafe { libc::getuid() }.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(q("plain/path-1.2"), "plain/path-1.2");
        assert_eq!(q(""), "''");
        assert_eq!(q("a b"), "'a b'");
        assert_eq!(q("it's"), "'it'\"'\"'s'");
    }

    #[test]
    fn paths() {
        assert_eq!(normalize(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
        assert!(is_under(Path::new("/a/b/c"), Path::new("/a/b")));
        assert!(is_under(Path::new("/a/b"), Path::new("/a/b")));
        assert!(!is_under(Path::new("/a/bc"), Path::new("/a/b")));
        std::env::set_var("CVENV_TEST_VAR", "/x/y");
        assert_eq!(expand("$CVENV_TEST_VAR/z"), PathBuf::from("/x/y/z"));
        assert_eq!(expand("${CVENV_TEST_VAR}/../w"), PathBuf::from("/x/w"));
    }

    #[test]
    fn sizes() {
        assert_eq!(human_size(512), "512B");
        assert_eq!(human_size(1536), "1.5K");
    }
}
