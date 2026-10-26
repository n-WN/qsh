//! Requests from the server (remote tools) to this client, and the local control
//! socket that `qsh cp` uses to reach the server through this client.

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

use anyhow::{Result, anyhow, bail};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
};

use super::Shared;
use crate::{
    files,
    proto::{self, LocalReq, Reply, Rpc},
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Consent {
    Ask,
    Always,
    Never,
}

#[derive(Clone, Debug)]
pub struct Policy {
    pub clipboard: bool,
    pub open: bool,
    pub receive: bool,
    /// Remote reads of local files that the user did not pick.
    pub read_paths: Consent,
    /// Upload files that are dropped into the terminal.
    pub drop: bool,
}

pub async fn serve(sh: Arc<Shared>, mut s: quinn::SendStream, mut r: quinn::RecvStream) {
    let mut header_sent = false;
    let res = handle(&sh, &mut s, &mut r, &mut header_sent).await;
    if let Err(e) = res
        && !header_sent {
            let _ = proto::write_msg(&mut s, &Reply::err(format!("{e:#}"))).await;
        }
    let _ = s.finish();
}

async fn handle(sh: &Arc<Shared>, s: &mut quinn::SendStream, r: &mut quinn::RecvStream, header_sent: &mut bool) -> Result<()> {
    let rpc: Rpc = proto::read_msg(r).await?;
    let p = &sh.policy;
    match rpc {
        Rpc::ClipList { .. } => {
            if !p.clipboard {
                bail!("clipboard access is off for this client");
            }
            let types = tokio::task::spawn_blocking(super::clipboard::list).await??;
            proto::write_msg(s, &Reply { ok: true, types, ..Default::default() }).await?;
        }
        Rpc::ClipGet { mime, .. } => {
            if !p.clipboard {
                bail!("clipboard access is off for this client");
            }
            let (m, data) = tokio::task::spawn_blocking(move || super::clipboard::get(&mime)).await??;
            *header_sent = true;
            proto::write_msg(s, &Reply { ok: true, mime: Some(m), ..Default::default() }).await?;
            s.write_all(&data).await?;
        }
        Rpc::ClipSet { mime, .. } => {
            if !p.clipboard {
                bail!("clipboard access is off for this client");
            }
            let mut data = Vec::new();
            r.take(512 << 20).read_to_end(&mut data).await?;
            tokio::task::spawn_blocking(move || super::clipboard::set(&mime, &data)).await??;
            proto::write_msg(s, &Reply::ok()).await?;
        }
        Rpc::Push { open, .. } => {
            if !p.receive {
                bail!("file receive is off for this client");
            }
            crate::paths::ensure_dir(&sh.download_dir).ok();
            let saved = files::read_archive(r, files::Place::Unique(sh.download_dir.clone())).await?;
            if open
                && let Some(first) = saved.first() {
                    open_local(&first.display().to_string())?;
                }
            let saved = saved.iter().map(|p| p.display().to_string()).collect();
            proto::write_msg(s, &Reply { ok: true, saved, ..Default::default() }).await?;
        }
        Rpc::Pull { paths, picker } => {
            let chosen: Vec<PathBuf> = if picker {
                tokio::task::spawn_blocking(pick_files).await??
            } else {
                let paths: Vec<PathBuf> = paths.iter().map(|p| crate::paths::expand_tilde(p)).collect();
                let allowed = match p.read_paths {
                    Consent::Always => true,
                    Consent::Never => false,
                    Consent::Ask => {
                        let list = paths.clone();
                        tokio::task::spawn_blocking(move || confirm_read(&list)).await?
                    }
                };
                if !allowed {
                    bail!("the user did not allow this read");
                }
                paths
            };
            if chosen.is_empty() {
                bail!("no files selected");
            }
            *header_sent = true;
            proto::write_msg(s, &Reply::ok()).await?;
            files::write_archive(s, &chosen).await?;
        }
        Rpc::Open { url } => {
            if !p.open {
                bail!("open is off for this client");
            }
            let lower = url.to_ascii_lowercase();
            if !["http://", "https://", "mailto:"].iter().any(|s| lower.starts_with(s)) {
                bail!("only http, https and mailto links can be opened");
            }
            open_local(&url)?;
            proto::write_msg(s, &Reply::ok()).await?;
        }
        Rpc::List | Rpc::Attach { .. } | Rpc::Device { .. } => bail!("not supported by the client"),
    }
    Ok(())
}

pub fn open_local(target: &str) -> Result<()> {
    let prog = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let st = Command::new(prog).arg(target).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status()?;
    if !st.success() {
        bail!("{prog} failed");
    }
    Ok(())
}

fn osascript(script: &[&str]) -> Result<String> {
    let mut c = Command::new("osascript");
    for l in script {
        c.args(["-e", l]);
    }
    let out = c.output()?;
    if !out.status.success() {
        bail!("cancelled");
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Let the user pick files to send to the server.
fn pick_files() -> Result<Vec<PathBuf>> {
    if !cfg!(target_os = "macos") {
        bail!("the file picker needs macOS; give the file paths instead");
    }
    let out = osascript(&[
        "set fs to choose file with prompt \"qsh: choose files to send to the remote host\" with multiple selections allowed",
        "set out to \"\"",
        "repeat with f in fs",
        "set out to out & POSIX path of f & linefeed",
        "end repeat",
        "return out",
    ])?;
    Ok(out.lines().filter(|l| !l.is_empty()).map(PathBuf::from).collect())
}

fn confirm_read(paths: &[PathBuf]) -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    let list: Vec<String> = paths.iter().map(|p| p.display().to_string().replace('"', "'")).collect();
    let script = format!(
        "display dialog \"The remote host wants to read these local files:\" & return & return & \"{}\" buttons {{\"Deny\", \"Allow\"}} default button \"Deny\" with title \"qsh\" with icon caution",
        list.join("\" & return & \"")
    );
    osascript(&[&script]).map(|o| o.contains("Allow")).unwrap_or(false)
}

/// Socket for `qsh cp` on this machine.
pub fn spawn_ctl(sh: Arc<Shared>) -> Result<PathBuf> {
    let path = crate::paths::client_dir()?.join(format!("{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let l = UnixListener::bind(&path)?;
    tokio::spawn(async move {
        while let Ok((s, _)) = l.accept().await {
            let sh = sh.clone();
            tokio::spawn(async move {
                let _ = ctl_conn(sh, s).await;
            });
        }
    });
    Ok(path)
}

async fn ctl_conn(sh: Arc<Shared>, s: UnixStream) -> Result<()> {
    let (mut r, mut w) = s.into_split();
    let req: LocalReq = proto::read_msg(&mut r).await?;
    let LocalReq::Rpc { rpc, .. } = req else {
        return proto::write_msg(&mut w, &Reply::err("unsupported request")).await;
    };
    let conn = sh.conn.lock().unwrap().clone();
    let Some(conn) = conn else {
        return proto::write_msg(&mut w, &Reply::err("not connected to the server now; try again")).await;
    };
    let (mut qs, mut qr) = conn.open_bi().await?;
    proto::write_msg(&mut qs, &rpc).await?;
    let up = async {
        let _ = tokio::io::copy(&mut r, &mut qs).await;
        let _ = qs.finish();
    };
    let down = async {
        let _ = tokio::io::copy(&mut qr, &mut w).await;
        let _ = w.shutdown().await;
    };
    tokio::join!(up, down);
    Ok(())
}

/// Find the control socket of a running client (the newest one, or the given pid).
pub fn find_ctl(pid: Option<u32>) -> Result<PathBuf> {
    let dir = crate::paths::client_dir()?;
    if let Some(pid) = pid {
        let p = dir.join(format!("{pid}.sock"));
        return if p.exists() { Ok(p) } else { Err(anyhow!("no qsh client with pid {pid}")) };
    }
    let mut socks: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "sock"))
        .filter(|p| {
            let pid: i32 = p.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse().ok()).unwrap_or(0);
            let alive = pid > 0 && unsafe { libc::kill(pid, 0) } == 0;
            if !alive {
                let _ = std::fs::remove_file(p);
            }
            alive
        })
        .filter_map(|p| Some((std::fs::metadata(&p).ok()?.modified().ok()?, p)))
        .collect();
    socks.sort();
    socks.pop().map(|(_, p)| p).ok_or_else(|| anyhow!("no running qsh client on this machine"))
}

pub fn is_remote(s: &str) -> bool {
    s.starts_with(':')
}

/// `qsh cp`: copy between this machine and the server of a running client.
pub async fn cp(paths: Vec<String>, pid: Option<u32>) -> Result<()> {
    let (dst, srcs) = paths.split_last().ok_or_else(|| anyhow!("usage: qsh cp SRC... DST"))?;
    let sock = find_ctl(pid)?;
    let s = UnixStream::connect(&sock).await?;
    let (mut r, mut w) = s.into_split();
    if is_remote(dst) && srcs.iter().all(|s| !is_remote(s)) {
        let local: Vec<PathBuf> = srcs.iter().map(PathBuf::from).collect();
        let rpc = Rpc::Push { dest: dst[1..].to_string(), open: false };
        proto::write_msg(&mut w, &LocalReq::Rpc { session: String::new(), rpc }).await?;
        let (files, bytes) = files::write_archive(&mut w, &local).await?;
        w.shutdown().await?;
        let rep: Reply = proto::read_msg(&mut r).await?;
        if !rep.ok {
            bail!("{}", rep.error.unwrap_or_default());
        }
        eprintln!("qsh: sent {files} file(s), {} to {}", human(bytes), rep.saved.join(", "));
    } else if srcs.iter().all(|s| is_remote(s)) && !is_remote(dst) {
        let remote: Vec<String> = srcs.iter().map(|s| s[1..].to_string()).collect();
        let rpc = Rpc::Pull { paths: remote, picker: false };
        proto::write_msg(&mut w, &LocalReq::Rpc { session: String::new(), rpc }).await?;
        w.shutdown().await?;
        let rep: Reply = proto::read_msg(&mut r).await?;
        if !rep.ok {
            bail!("{}", rep.error.unwrap_or_default());
        }
        let dst = Path::new(dst);
        let place = if dst.is_dir() {
            files::Place::Into(dst.to_path_buf())
        } else if srcs.len() == 1 {
            files::Place::As(dst.to_path_buf())
        } else {
            bail!("{} is not a directory", dst.display());
        };
        let got = files::read_archive(&mut r, place).await?;
        for g in got {
            eprintln!("qsh: got {}", g.display());
        }
    } else {
        bail!("one side must be remote (a path that starts with ':'), the other side local");
    }
    Ok(())
}

pub fn human(n: u64) -> String {
    let f = n as f64;
    if f >= 1e9 {
        format!("{:.1} GB", f / 1e9)
    } else if f >= 1e6 {
        format!("{:.1} MB", f / 1e6)
    } else if f >= 1e3 {
        format!("{:.1} kB", f / 1e3)
    } else {
        format!("{n} B")
    }
}
