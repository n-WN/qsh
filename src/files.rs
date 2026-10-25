//! A small streaming archive format for file transfer:
//! repeated `[u32 len][json Entry][file bytes]`, ended by a zero length.

use std::{
    collections::HashMap,
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Dir,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Entry {
    pub path: String,
    pub kind: Kind,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub mode: u32,
}

async fn write_entry<W: AsyncWrite + Unpin>(w: &mut W, e: &Entry) -> Result<()> {
    let j = serde_json::to_vec(e)?;
    w.write_all(&(j.len() as u32).to_be_bytes()).await?;
    w.write_all(&j).await?;
    Ok(())
}

fn root_name(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .or_else(|| std::fs::canonicalize(p).ok().and_then(|c| c.file_name().map(|s| s.to_string_lossy().into_owned())))
        .unwrap_or_else(|| "root".to_string())
}

/// Write files and directory trees. Symbolic links inside trees are skipped.
/// Returns (files, bytes).
pub async fn write_archive<W: AsyncWrite + Unpin>(w: &mut W, roots: &[PathBuf]) -> Result<(u64, u64)> {
    let (mut files, mut bytes) = (0u64, 0u64);
    for root in roots {
        let meta = tokio::fs::metadata(root).await.with_context(|| format!("{}", root.display()))?;
        let mut stack = vec![(root.clone(), root_name(root), meta)];
        while let Some((path, rel, meta)) = stack.pop() {
            let mode = meta.permissions().mode() & 0o7777;
            if meta.is_dir() {
                write_entry(w, &Entry { path: rel.clone(), kind: Kind::Dir, size: 0, mode }).await?;
                let mut rd = tokio::fs::read_dir(&path).await?;
                let mut kids = Vec::new();
                while let Some(de) = rd.next_entry().await? {
                    let m = tokio::fs::symlink_metadata(de.path()).await?;
                    if m.is_dir() || m.is_file() {
                        kids.push((de.path(), format!("{rel}/{}", de.file_name().to_string_lossy()), m));
                    }
                }
                kids.sort_by(|a, b| b.1.cmp(&a.1));
                stack.extend(kids);
            } else if meta.is_file() {
                let size = meta.len();
                write_entry(w, &Entry { path: rel, kind: Kind::File, size, mode }).await?;
                let f = tokio::fs::File::open(&path).await?;
                let n = tokio::io::copy(&mut f.take(size), w).await?;
                if n < size {
                    // The file got shorter while we read it: pad to keep the stream valid.
                    let zeros = vec![0u8; 64 * 1024];
                    let mut left = size - n;
                    while left > 0 {
                        let k = left.min(zeros.len() as u64) as usize;
                        w.write_all(&zeros[..k]).await?;
                        left -= k as u64;
                    }
                }
                files += 1;
                bytes += size;
            }
        }
    }
    w.write_all(&0u32.to_be_bytes()).await?;
    w.flush().await?;
    Ok((files, bytes))
}

/// Where to put the top-level entries of an archive.
pub enum Place {
    /// Into this directory; replace files that exist.
    Into(PathBuf),
    /// Into this directory; give new names when a name exists ("a (1).txt").
    Unique(PathBuf),
    /// The single top-level entry gets this path.
    As(PathBuf),
}

pub fn unique_name(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    if std::fs::symlink_metadata(&p).is_err() {
        return p;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    for i in 1.. {
        let p = dir.join(format!("{stem} ({i}){ext}"));
        if std::fs::symlink_metadata(&p).is_err() {
            return p;
        }
    }
    unreachable!()
}

fn safe_parts(path: &str) -> Result<Vec<String>> {
    let mut v = Vec::new();
    for c in Path::new(path).components() {
        match c {
            Component::Normal(s) => v.push(s.to_string_lossy().into_owned()),
            _ => bail!("unsafe path in archive: {path:?}"),
        }
    }
    if v.is_empty() {
        bail!("empty path in archive");
    }
    Ok(v)
}

/// Read an archive and write it to disk. Returns the top-level paths written.
pub async fn read_archive<R: AsyncRead + Unpin>(r: &mut R, place: Place) -> Result<Vec<PathBuf>> {
    let mut tops: HashMap<String, PathBuf> = HashMap::new();
    let mut order = Vec::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let mut l = [0u8; 4];
        r.read_exact(&mut l).await.context("archive ended early")?;
        let len = u32::from_be_bytes(l) as usize;
        if len == 0 {
            break;
        }
        if len > 1 << 20 {
            bail!("bad archive header");
        }
        let mut h = vec![0u8; len];
        r.read_exact(&mut h).await?;
        let e: Entry = serde_json::from_slice(&h)?;
        let parts = safe_parts(&e.path)?;
        let top = match tops.get(&parts[0]) {
            Some(t) => t.clone(),
            None => {
                let t = match &place {
                    Place::Into(d) => d.join(&parts[0]),
                    Place::Unique(d) => unique_name(d, &parts[0]),
                    Place::As(p) => {
                        if !tops.is_empty() {
                            bail!("more than one item: the destination must be a directory");
                        }
                        p.clone()
                    }
                };
                tops.insert(parts[0].clone(), t.clone());
                order.push(t.clone());
                t
            }
        };
        let mut path = top;
        for p in &parts[1..] {
            path.push(p);
        }
        match e.kind {
            Kind::Dir => {
                tokio::fs::create_dir_all(&path).await.with_context(|| format!("mkdir {}", path.display()))?;
                if e.mode != 0 {
                    let _ = tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode((e.mode & 0o777) | 0o700)).await;
                }
            }
            Kind::File => {
                if let Some(parent) = path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                if let Ok(m) = std::fs::symlink_metadata(&path) {
                    if m.file_type().is_symlink() {
                        tokio::fs::remove_file(&path).await?;
                    }
                }
                let mut opts = tokio::fs::OpenOptions::new();
                opts.write(true).create(true).truncate(true).custom_flags(libc::O_NOFOLLOW);
                opts.mode(if e.mode != 0 { e.mode & 0o777 } else { 0o644 });
                let mut f = opts.open(&path).await.with_context(|| format!("create {}", path.display()))?;
                let mut left = e.size;
                while left > 0 {
                    let k = left.min(buf.len() as u64) as usize;
                    r.read_exact(&mut buf[..k]).await.context("archive ended early")?;
                    f.write_all(&buf[..k]).await?;
                    left -= k as u64;
                }
                f.flush().await?;
                if e.mode != 0 {
                    let _ = tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(e.mode & 0o777)).await;
                }
            }
        }
    }
    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roundtrip() {
        let t = std::env::temp_dir().join(format!("qsh-test-{}", std::process::id()));
        let src = t.join("src");
        std::fs::create_dir_all(src.join("d/e")).unwrap();
        std::fs::write(src.join("a.txt"), b"hello").unwrap();
        std::fs::write(src.join("d/e/b.bin"), vec![7u8; 300_000]).unwrap();
        let mut buf = Vec::new();
        let (files, bytes) = write_archive(&mut buf, &[src.join("a.txt"), src.join("d")]).await.unwrap();
        assert_eq!((files, bytes), (2, 300_005));
        let dst = t.join("dst");
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(dst.join("a.txt"), b"old").unwrap();
        let got = read_archive(&mut buf.as_slice(), Place::Unique(dst.clone())).await.unwrap();
        assert_eq!(got, vec![dst.join("a (1).txt"), dst.join("d")]);
        assert_eq!(std::fs::read(dst.join("a (1).txt")).unwrap(), b"hello");
        assert_eq!(std::fs::read(dst.join("d/e/b.bin")).unwrap().len(), 300_000);
        let mut bad = Vec::new();
        write_entry(&mut bad, &Entry { path: "../x".into(), kind: Kind::File, size: 0, mode: 0 }).await.unwrap();
        bad.extend_from_slice(&0u32.to_be_bytes());
        assert!(read_archive(&mut bad.as_slice(), Place::Into(dst.clone())).await.is_err());
        std::fs::remove_dir_all(&t).unwrap();
    }
}
