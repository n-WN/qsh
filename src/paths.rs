//! Local state directories.

use std::{
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

use anyhow::Result;

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// `$QSH_HOME`, or `~/.qsh`.
pub fn base() -> PathBuf {
    std::env::var_os("QSH_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".qsh"))
}

pub fn ensure_dir(p: &Path) -> Result<()> {
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(p)?;
    Ok(())
}

pub fn run_dir() -> Result<PathBuf> {
    let d = base().join("run");
    ensure_dir(&d)?;
    Ok(d)
}

pub fn daemon_sock() -> Result<PathBuf> {
    Ok(run_dir()?.join("daemon.sock"))
}

pub fn shim_dir() -> PathBuf {
    base().join("bin")
}

pub fn client_dir() -> Result<PathBuf> {
    let d = base().join("client");
    ensure_dir(&d)?;
    Ok(d)
}

pub fn write_private(p: &Path, data: &[u8]) -> Result<()> {
    if let Some(d) = p.parent() {
        ensure_dir(d)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).truncate(true).write(true).mode(0o600).open(p)?;
    f.write_all(data)?;
    Ok(())
}

pub fn expand_tilde(s: &str) -> PathBuf {
    if s == "~" {
        home()
    } else if let Some(rest) = s.strip_prefix("~/") {
        home().join(rest)
    } else {
        PathBuf::from(s)
    }
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn random_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("random");
    hex::encode(b)
}

/// Debug log: set QSH_LOG to a file path.
pub fn log(msg: impl AsRef<str>) {
    if let Some(p) = std::env::var_os("QSH_LOG")
        && let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
            let _ = writeln!(f, "{}.{:03} [{}] {}", t.as_secs(), t.subsec_millis(), std::process::id(), msg.as_ref());
        }
}

/// Debug dump of raw bytes: set QSH_DUMP to a file path. Session output goes
/// there, input that goes to the server into the same path with ".in".
pub fn dump(input: bool, data: &[u8]) {
    use std::sync::OnceLock;
    static PATH: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    let Some(p) = PATH.get_or_init(|| std::env::var_os("QSH_DUMP").map(PathBuf::from)) else { return };
    let p = if input { p.with_extension("in") } else { p.clone() };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
        let _ = f.write_all(data);
    }
}
