//! Commands for use inside a qsh session on the server. They reach the client
//! through the daemon: tool -> unix socket -> daemon -> QUIC stream -> client.

use std::{
    io::{IsTerminal, Read, Write},
    path::PathBuf,
};

use anyhow::{Context, Result, anyhow, bail};
use tokio::{
    io::AsyncWriteExt,
    net::{
        UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
};

use crate::{
    files, paths,
    proto::{self, LocalReply, LocalReq, Reply, Rpc},
};

fn session() -> Result<String> {
    std::env::var("QSH_SESSION").map_err(|_| anyhow!("not inside a qsh session (QSH_SESSION is not set)"))
}

fn sock() -> Result<PathBuf> {
    match std::env::var_os("QSH_SOCK") {
        Some(s) => Ok(PathBuf::from(s)),
        None => paths::daemon_sock(),
    }
}

async fn open_rpc(rpc: Rpc) -> Result<(OwnedReadHalf, OwnedWriteHalf)> {
    let s = UnixStream::connect(sock()?).await.context("connect to the qsh daemon")?;
    let (r, mut w) = s.into_split();
    proto::write_msg(&mut w, &LocalReq::Rpc { session: session()?, rpc }).await?;
    Ok((r, w))
}

async fn reply(r: &mut OwnedReadHalf) -> Result<Reply> {
    let rep: Reply = proto::read_msg(r).await?;
    if !rep.ok {
        bail!("{}", rep.error.unwrap_or_else(|| "failed".into()));
    }
    Ok(rep)
}

async fn local(req: LocalReq) -> Result<LocalReply> {
    let mut s = UnixStream::connect(sock()?).await.context("connect to the qsh daemon")?;
    proto::write_msg(&mut s, &req).await?;
    let rep: LocalReply = proto::read_msg(&mut s).await?;
    if !rep.ok {
        bail!("{}", rep.error.unwrap_or_else(|| "failed".into()));
    }
    Ok(rep)
}

pub async fn clip_list() -> Result<Vec<String>> {
    let (mut r, mut w) = open_rpc(Rpc::ClipList { primary: false }).await?;
    w.shutdown().await?;
    Ok(reply(&mut r).await?.types)
}

pub async fn clip_get(mime: &str) -> Result<Vec<u8>> {
    let (mut r, mut w) = open_rpc(Rpc::ClipGet { mime: mime.to_string(), primary: false }).await?;
    w.shutdown().await?;
    reply(&mut r).await?;
    let mut data = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut r, &mut data).await?;
    Ok(data)
}

pub async fn clip_set(mime: &str, data: &[u8]) -> Result<()> {
    let (mut r, mut w) = open_rpc(Rpc::ClipSet { mime: mime.to_string(), primary: false }).await?;
    w.write_all(data).await?;
    w.shutdown().await?;
    reply(&mut r).await?;
    Ok(())
}

pub async fn send(files: Vec<PathBuf>, open: bool) -> Result<()> {
    let (mut r, mut w) = open_rpc(Rpc::Push { dest: String::new(), open }).await?;
    let (n, bytes) = files::write_archive(&mut w, &files).await?;
    w.shutdown().await?;
    let rep = reply(&mut r).await?;
    eprintln!("qsh: sent {n} file(s), {}", crate::client::rpc::human(bytes));
    for s in rep.saved {
        println!("{s}");
    }
    Ok(())
}

pub async fn get(dir: Option<PathBuf>, paths: Vec<String>) -> Result<()> {
    let picker = paths.is_empty();
    let (mut r, mut w) = open_rpc(Rpc::Pull { paths, picker }).await?;
    w.shutdown().await?;
    if picker {
        eprintln!("qsh: choose files in the dialog on your local machine");
    }
    reply(&mut r).await?;
    let dir = dir.unwrap_or(std::env::current_dir()?);
    let got = files::read_archive(&mut r, files::Place::Unique(dir)).await?;
    for g in got {
        println!("{}", g.display());
    }
    Ok(())
}

pub async fn open(target: &str) -> Result<()> {
    let is_url = target.find(':').is_some_and(|i| {
        i > 1 && target[..i].chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c)) && !std::path::Path::new(target).exists()
    });
    if is_url {
        let (mut r, mut w) = open_rpc(Rpc::Open { url: target.to_string() }).await?;
        w.shutdown().await?;
        reply(&mut r).await?;
    } else {
        let p = target.strip_prefix("file://").unwrap_or(target);
        send(vec![PathBuf::from(p)], true).await?;
    }
    Ok(())
}

pub async fn detach() -> Result<()> {
    local(LocalReq::Detach { session: session()? }).await?;
    Ok(())
}

pub async fn share(readonly: bool, ttl: u64) -> Result<()> {
    let rep = local(LocalReq::Share { session: session()?, readonly, ttl }).await?;
    println!("{}", rep.url);
    eprintln!("qsh: on another machine run:  qsh '{}'", rep.url);
    eprintln!("qsh: the link works for {} minute(s){}", ttl.max(60) / 60, if readonly { ", read-only" } else { "" });
    Ok(())
}

/// List (or revoke) the devices that can connect without SSH.
pub async fn devices(revoke: Option<String>) -> Result<()> {
    let revoke = revoke.map(|r| if r == "all" { String::new() } else { r });
    let rep = local(LocalReq::Devices { revoke: revoke.clone() }).await?;
    if revoke.is_some() {
        println!("removed {} device(s)", rep.removed);
    }
    if rep.devices.is_empty() {
        println!("no devices");
        return Ok(());
    }
    let now = paths::now_secs();
    println!("{:<14} {:<24} {:>10} {:>10}", "ID", "CLIENT", "CREATED", "LAST USE");
    for d in &rep.devices {
        let days = |t: u64| format!("{}d ago", now.saturating_sub(t) / 86400);
        println!("{:<14} {:<24} {:>10} {:>10}", d.id, d.label, days(d.created), days(d.last_used));
    }
    Ok(())
}

pub fn print_sessions(sessions: &[proto::SessionInfo]) {
    if sessions.is_empty() {
        println!("no sessions");
        return;
    }
    let now = paths::now_secs();
    println!("{:<16} {:>8} {:>9} {:>8} {:>10}  COMMAND", "SESSION", "PID", "SIZE", "CLIENTS", "AGE");
    for s in sessions {
        let age = now.saturating_sub(s.created);
        let age = if age >= 86400 {
            format!("{}d", age / 86400)
        } else if age >= 3600 {
            format!("{}h", age / 3600)
        } else {
            format!("{}m", age / 60)
        };
        println!(
            "{:<16} {:>8} {:>9} {:>8} {:>10}  {}",
            s.name,
            s.pid,
            format!("{}x{}", s.cols, s.rows),
            format!("{}/{}", s.connected, s.clients),
            age,
            s.command
        );
    }
}

pub async fn ls_local() -> Result<()> {
    let rep = local(LocalReq::List).await?;
    print_sessions(&rep.sessions);
    Ok(())
}

// ---- shims: xclip, xsel, wl-copy, wl-paste, pbcopy, pbpaste, xdg-open ----

fn read_input(files: &[String]) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    if files.is_empty() {
        std::io::stdin().read_to_end(&mut data)?;
    } else {
        for f in files {
            data.extend(std::fs::read(f).with_context(|| f.clone())?);
        }
    }
    Ok(data)
}

fn write_out(data: &[u8]) -> Result<()> {
    let mut o = std::io::stdout().lock();
    o.write_all(data)?;
    o.flush()?;
    Ok(())
}

fn strip_nl(mut d: Vec<u8>) -> Vec<u8> {
    if d.last() == Some(&b'\n') {
        d.pop();
    }
    d
}

pub fn shim(name: &str, args: &[String]) -> i32 {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let r = rt.block_on(async {
        match name {
            "xclip" => xclip(args).await,
            "xsel" => xsel(args).await,
            "wl-copy" => wl_copy(args).await,
            "wl-paste" => wl_paste(args).await,
            "pbcopy" => clip_set("text/plain", &read_input(&[])?).await,
            "pbpaste" => write_out(&clip_get("text/plain").await?),
            "xdg-open" => match args.first() {
                Some(t) => open(t).await,
                None => bail!("usage: xdg-open URL|FILE"),
            },
            _ => bail!("unknown shim {name}"),
        }
    });
    match r {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{name} (qsh): {e:#}");
            1
        }
    }
}

fn prefix_of(flag: &str, word: &str, min: usize) -> bool {
    flag.len() >= min && word.starts_with(flag)
}

async fn xclip(args: &[String]) -> Result<()> {
    let (mut out, mut target, mut files, mut rmlastnl, mut filter) = (false, None::<String>, Vec::new(), false, false);
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a.len() > 1 && a.starts_with('-') {
            let f = a.trim_start_matches('-');
            if prefix_of(f, "out", 1) {
                out = true;
            } else if prefix_of(f, "in", 1) {
                out = false;
            } else if prefix_of(f, "selection", 2) || f == "s" {
                i += 1;
            } else if prefix_of(f, "target", 1) {
                i += 1;
                target = args.get(i).cloned();
            } else if prefix_of(f, "display", 1) || prefix_of(f, "loops", 1) {
                i += 1;
            } else if prefix_of(f, "rmlastnl", 1) {
                rmlastnl = true;
            } else if prefix_of(f, "filter", 1) {
                filter = true;
            } else if f == "version" {
                println!("xclip version 0.13 (qsh shim)");
                return Ok(());
            } else if prefix_of(f, "help", 1) {
                println!("usage: xclip [-selection clipboard] [-t TYPE] [-o | -i] [FILE...]  (qsh shim)");
                return Ok(());
            }
        } else {
            files.push(a.clone());
        }
        i += 1;
    }
    let t = target.unwrap_or_else(|| "text/plain".into());
    if out {
        if t == "TARGETS" {
            let types = clip_list().await?;
            write_out(format!("{}\n", types.join("\n")).as_bytes())
        } else {
            let d = clip_get(&t).await?;
            write_out(&if rmlastnl { strip_nl(d) } else { d })
        }
    } else {
        let d = read_input(&files)?;
        if filter {
            write_out(&d)?;
        }
        clip_set(&t, &if rmlastnl { strip_nl(d) } else { d }).await
    }
}

async fn xsel(args: &[String]) -> Result<()> {
    let (mut input, mut output, mut clear, mut append) = (false, false, false, false);
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "--input" => input = true,
            "--output" => output = true,
            "--clear" | "--delete" => clear = true,
            "--append" => append = true,
            "--logfile" | "--display" | "--selectionTimeout" => i += 1,
            s if s.starts_with("--") => {}
            s if s.starts_with('-') => {
                for c in s[1..].chars() {
                    match c {
                        'i' => input = true,
                        'o' => output = true,
                        'c' | 'd' => clear = true,
                        'a' => append = true,
                        't' | 'l' => i += 1,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    if clear {
        return clip_set("text/plain", b"").await;
    }
    if !input && !output {
        if !std::io::stdin().is_terminal() {
            input = true;
        } else {
            output = true;
        }
    }
    if input {
        let mut d = if append { clip_get("text/plain").await.unwrap_or_default() } else { Vec::new() };
        d.extend(read_input(&[])?);
        clip_set("text/plain", &d).await?;
    }
    if output {
        write_out(&clip_get("text/plain").await?)?;
    }
    Ok(())
}

async fn wl_copy(args: &[String]) -> Result<()> {
    let (mut mime, mut trim, mut clear, mut words) = (None::<String>, false, false, Vec::new());
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "-t" | "--type" => {
                i += 1;
                mime = args.get(i).cloned();
            }
            s if s.starts_with("--type=") => mime = Some(s[7..].to_string()),
            "-n" | "--trim-newline" => trim = true,
            "-c" | "--clear" => clear = true,
            "-s" | "--seat" => i += 1,
            "-h" | "--help" => {
                println!("usage: wl-copy [-t TYPE] [-n] [TEXT...]  (qsh shim)");
                return Ok(());
            }
            "-v" | "--version" => {
                println!("wl-clipboard 2.2.1 (qsh shim)");
                return Ok(());
            }
            s if s.starts_with('-') && s.len() > 1 && words.is_empty() => {}
            _ => words.push(args[i].clone()),
        }
        i += 1;
    }
    if clear {
        return clip_set("text/plain", b"").await;
    }
    let data = if words.is_empty() { read_input(&[])? } else { words.join(" ").into_bytes() };
    let data = if trim { strip_nl(data) } else { data };
    let mime = mime.unwrap_or_else(|| sniff(&data).to_string());
    clip_set(&mime, &data).await
}

/// Guess a MIME type for data without a type (as wl-copy does).
fn sniff(d: &[u8]) -> &'static str {
    if d.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if d.starts_with(&[0xff, 0xd8, 0xff]) {
        "image/jpeg"
    } else if d.starts_with(b"GIF8") {
        "image/gif"
    } else {
        "text/plain"
    }
}

async fn wl_paste(args: &[String]) -> Result<()> {
    let (mut list, mut mime, mut no_nl) = (false, None::<String>, false);
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "-l" | "--list-types" => list = true,
            "-t" | "--type" => {
                i += 1;
                mime = args.get(i).cloned();
            }
            s if s.starts_with("--type=") => mime = Some(s[7..].to_string()),
            "-n" | "--no-newline" => no_nl = true,
            "-s" | "--seat" => i += 1,
            "-w" | "--watch" => bail!("--watch is not supported"),
            "-h" | "--help" => {
                println!("usage: wl-paste [-l] [-t TYPE] [-n]  (qsh shim)");
                return Ok(());
            }
            "-v" | "--version" => {
                println!("wl-clipboard 2.2.1 (qsh shim)");
                return Ok(());
            }
            _ => {}
        }
        i += 1;
    }
    if list {
        let types = clip_list().await?;
        let types: Vec<&String> = types.iter().filter(|t| t.contains('/')).collect();
        let s: Vec<&str> = types.iter().map(|s| s.as_str()).collect();
        return write_out(format!("{}\n", s.join("\n")).as_bytes());
    }
    let mime = mime.unwrap_or_else(|| "text/plain".into());
    let mut d = clip_get(&mime).await?;
    if crate::client::clipboard::is_text(&mime) && !no_nl && d.last() != Some(&b'\n') {
        d.push(b'\n');
    }
    write_out(&d)
}
