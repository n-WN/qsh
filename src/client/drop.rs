//! Drag and drop. A terminal pastes the paths of dropped files. When every
//! pasted word is a path of a local file, the files go to the server first,
//! and the paste then contains the remote paths. So a screenshot dropped into
//! a remote program (for example Claude Code) arrives as a real file.

use std::{
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
};

use anyhow::{Result, bail};
use tokio::io::AsyncWrite;

use crate::{
    files,
    proto::{self, Reply, Rpc},
};

pub const PASTE_START: &[u8] = b"\x1b[200~";
pub const PASTE_END: &[u8] = b"\x1b[201~";

pub fn wrap(p: &[u8]) -> Vec<u8> {
    [PASTE_START, p, PASTE_END].concat()
}

/// Split shell words with backslash escapes, single and double quotes.
fn words(s: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                cur.push(chars.next()?);
                in_word = true;
            }
            '\'' => {
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    cur.push(c);
                }
                in_word = true;
            }
            '"' => {
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => cur.push(chars.next()?),
                        c => cur.push(c),
                    }
                }
                in_word = true;
            }
            c if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    Some(out)
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(h, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Places that only exist on this machine. Paths in /tmp, /etc and similar
/// can also name files on the server, so they stay text.
fn local_only(p: &Path) -> bool {
    let home = crate::paths::home();
    (p.starts_with(&home) && p != home)
        || ["/Volumes", "/private/var/folders", "/var/folders", "/media", "/mnt", "/run/media"].iter().any(|r| p.starts_with(r))
}

/// The local files in a paste, if the paste is only paths of local files.
pub fn local_paths(paste: &[u8]) -> Option<Vec<PathBuf>> {
    let s = std::str::from_utf8(paste).ok()?.trim();
    if s.is_empty() || s.len() > 64 * 1024 {
        return None;
    }
    let ws = words(s)?;
    if ws.is_empty() || ws.len() > 100 {
        return None;
    }
    let mut out = Vec::new();
    for w in ws {
        let w = match w.strip_prefix("file://") {
            Some(r) => percent_decode(r)?,
            None => w,
        };
        let p = PathBuf::from(w);
        if !p.is_absolute() || !local_only(&p) || std::fs::symlink_metadata(&p).is_err() {
            return None;
        }
        out.push(p);
    }
    Some(out)
}

/// Quote a remote path in the style of the original paste.
fn quote(original: &[u8], path: &str) -> String {
    if original.contains(&b'\'') {
        format!("'{}'", path.replace('\'', "'\\''"))
    } else {
        let mut s = String::new();
        for c in path.chars() {
            if !(c.is_ascii_alphanumeric() || "/._-+,:@%".contains(c)) {
                s.push('\\');
            }
            s.push(c);
        }
        s
    }
}

pub fn rewrite(original: &[u8], remote: &[String]) -> Vec<u8> {
    remote.iter().map(|p| quote(original, p)).collect::<Vec<_>>().join(" ").into_bytes()
}

pub fn total_size(paths: &[PathBuf]) -> u64 {
    fn walk(p: &Path) -> u64 {
        match std::fs::symlink_metadata(p) {
            Ok(m) if m.is_file() => m.len(),
            Ok(m) if m.is_dir() => std::fs::read_dir(p).map(|rd| rd.flatten().map(|e| walk(&e.path())).sum()).unwrap_or(0),
            _ => 0,
        }
    }
    paths.iter().map(|p| walk(p)).sum()
}

struct Counting<'a> {
    inner: &'a mut quinn::SendStream,
    done: Arc<AtomicU64>,
}

impl AsyncWrite for Counting<'_> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        let r = AsyncWrite::poll_write(Pin::new(&mut *self.inner), cx, buf);
        if let Poll::Ready(Ok(n)) = &r {
            self.done.fetch_add(*n as u64, Ordering::Relaxed);
        }
        r
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        AsyncWrite::poll_flush(Pin::new(&mut *self.inner), cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        AsyncWrite::poll_shutdown(Pin::new(&mut *self.inner), cx)
    }
}

fn stamp() -> String {
    let t = crate::paths::now_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}-{}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        crate::paths::random_hex(2)
    )
}

/// Send the files into a new folder under ~/.qsh/drop on the server. Returns
/// the remote paths of the top-level items.
pub async fn upload(conn: quinn::Connection, paths: Vec<PathBuf>, done: Arc<AtomicU64>) -> Result<Vec<String>> {
    let (mut s, mut r) = conn.open_bi().await?;
    proto::write_msg(&mut s, &Rpc::Push { dest: format!("~/.qsh/drop/{}/", stamp()), open: false }).await?;
    files::write_archive(&mut Counting { inner: &mut s, done }, &paths).await?;
    let _ = s.finish();
    let rep: Reply = proto::read_msg(&mut r).await?;
    if !rep.ok {
        bail!("{}", rep.error.unwrap_or_default());
    }
    Ok(rep.saved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_words() {
        assert_eq!(words(r"/a/b\ c.png '/d e/f' x").unwrap(), vec!["/a/b c.png", "/d e/f", "x"]);
        assert_eq!(words("\"/q \\\"x\\\"\"").unwrap(), vec!["/q \"x\""]);
        assert!(words("'open").is_some());
        assert!(words("\"open").is_none());
    }

    #[test]
    fn detect_and_rewrite() {
        let home = crate::paths::home();
        let dir = home.join(format!(".qsh-drop-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("my shot.png");
        std::fs::write(&f, b"x").unwrap();
        let paste = f.display().to_string().replace(' ', "\\ ");
        assert_eq!(local_paths(paste.as_bytes()), Some(vec![f.clone()]));
        assert_eq!(local_paths(b"/etc/hosts"), None);
        assert_eq!(local_paths(b"hello world"), None);
        let missing = format!("{}/nope.png", dir.display());
        assert_eq!(local_paths(missing.as_bytes()), None);
        let url = format!("file://{}", f.display().to_string().replace(' ', "%20"));
        assert_eq!(local_paths(url.as_bytes()), Some(vec![f.clone()]));
        assert_eq!(rewrite(paste.as_bytes(), &["/root/.qsh/drop/1/my shot.png".into()]), b"/root/.qsh/drop/1/my\\ shot.png");
        assert_eq!(rewrite(b"'x y'", &["/r/x y".into()]), b"'/r/x y'");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
