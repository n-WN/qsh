//! Build identity and embedded source.
//!
//! - QSH_BUILD_ID: a hash of the source files. Two builds of the same source
//!   have the same id, on any machine and with any compiler.
//! - QSH_BUILD_TIME: the time of the newest source file (or SOURCE_DATE_EPOCH,
//!   or the `qsh-build-time` file of an unpacked source archive). It orders
//!   builds: a client only upgrades a server that is older.
//! - source.tar: the source, so that a client can build qsh on a server for
//!   which it has no binary.

use std::{
    fs,
    path::{Path, PathBuf},
};

fn walk(d: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(d) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

fn sources(root: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = ["Cargo.toml", "Cargo.lock", "build.rs"].iter().map(|f| root.join(f)).filter(|p| p.exists()).collect();
    walk(&root.join("src"), &mut v);
    v.sort();
    v
}

/// FNV-1a, 64 bit: small and the same everywhere.
fn fnv(h: &mut u64, data: &[u8]) {
    for &b in data {
        *h ^= b as u64;
        *h = h.wrapping_mul(0x100000001b3);
    }
}

fn octal(field: &mut [u8], v: u64) {
    let s = format!("{:0w$o}", v, w = field.len() - 1);
    field[..s.len()].copy_from_slice(s.as_bytes());
}

fn tar_entry(out: &mut Vec<u8>, name: &str, data: &[u8], mtime: u64) {
    let mut h = [0u8; 512];
    h[..name.len()].copy_from_slice(name.as_bytes());
    octal(&mut h[100..108], 0o644);
    octal(&mut h[108..116], 0);
    octal(&mut h[116..124], 0);
    octal(&mut h[124..136], data.len() as u64);
    octal(&mut h[136..148], mtime);
    h[148..156].copy_from_slice(b"        ");
    h[156] = b'0';
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    let sum: u32 = h.iter().map(|&b| b as u32).sum();
    let s = format!("{sum:06o}\0 ");
    h[148..156].copy_from_slice(s.as_bytes());
    out.extend_from_slice(&h);
    out.extend_from_slice(data);
    out.resize(out.len().div_ceil(512) * 512, 0);
}

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let files = sources(&root);
    let mut id: u64 = 0xcbf29ce484222325;
    let mut newest = 0u64;
    let mut tar = Vec::new();
    for f in &files {
        let rel = f.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
        let data = fs::read(f).unwrap();
        fnv(&mut id, rel.as_bytes());
        fnv(&mut id, &[0]);
        fnv(&mut id, &data);
        fnv(&mut id, &[0]);
        let mtime = fs::metadata(f)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        newest = newest.max(mtime);
        tar_entry(&mut tar, &format!("qsh/{rel}"), &data, mtime);
    }
    let time = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .or_else(|| fs::read_to_string(root.join("qsh-build-time")).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(newest);
    tar_entry(&mut tar, "qsh/qsh-build-time", format!("{time}\n").as_bytes(), time);
    tar.extend_from_slice(&[0u8; 1024]);

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("source.tar");
    fs::write(&out, &tar).unwrap();

    println!("cargo:rustc-env=QSH_BUILD_ID={id:016x}");
    println!("cargo:rustc-env=QSH_BUILD_TIME={time}");
    for f in ["src", "Cargo.toml", "Cargo.lock", "build.rs", "qsh-build-time"] {
        println!("cargo:rerun-if-changed={f}");
    }
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
}
