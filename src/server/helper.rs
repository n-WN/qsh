//! `qsh helper`: started by the client over SSH. It makes sure the daemon runs,
//! gets connection details, prints them on one line, and (with `--tunnel`)
//! then relays QUIC datagrams between SSH stdio and the daemon's UDP port.

use std::{
    net::{IpAddr, SocketAddr},
    os::unix::process::CommandExt,
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};

use crate::{
    paths,
    proto::{self, LocalReply, LocalReq},
    version::{self, Build},
};

pub struct HelperArgs {
    pub port: Option<u16>,
    pub tunnel: bool,
    pub token: bool,
    pub list: bool,
}

pub async fn connect_daemon(port: Option<u16>, start: bool) -> Result<UnixStream> {
    let sock = paths::daemon_sock()?;
    if let Ok(s) = UnixStream::connect(&sock).await {
        return Ok(s);
    }
    if !start {
        bail!("the qsh daemon is not running");
    }
    start_daemon(port)?;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if let Ok(s) = UnixStream::connect(&sock).await {
            return Ok(s);
        }
    }
    bail!("the qsh daemon did not start; see {}", paths::run_dir()?.join("daemon.log").display())
}

fn start_daemon(port: Option<u16>) -> Result<()> {
    let exe = std::env::current_exe()?;
    let log_path = paths::run_dir()?.join("daemon.log");
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon");
    if let Some(p) = port {
        cmd.args(["--port", &p.to_string()]);
    }
    cmd.current_dir(paths::home()).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("start daemon")?;
    Ok(())
}

pub async fn local_request(port: Option<u16>, start: bool, req: &LocalReq) -> Result<LocalReply> {
    let mut s = connect_daemon(port, start).await?;
    proto::write_msg(&mut s, req).await?;
    let rep: LocalReply = proto::read_msg(&mut s).await?;
    if !rep.ok {
        bail!("{}", rep.error.unwrap_or_else(|| "request failed".into()));
    }
    Ok(rep)
}

pub async fn run(a: HelperArgs) -> Result<()> {
    if a.list {
        let rep = local_request(a.port, false, &LocalReq::List).await.unwrap_or_default();
        println!("{}", serde_json::to_string(&rep.sessions)?);
        return Ok(());
    }
    // A daemon of an older build gives its sessions to this program.
    if let Ok(rep) = local_request(a.port, true, &LocalReq::Info).await
        && let Some(running) = Build::parse(&rep.build)
        && Build::current().replaces(&running)
    {
        upgrade_daemon(a.port).await;
    }
    let rep = local_request(a.port, true, if a.token { &LocalReq::Token } else { &LocalReq::Info }).await?;
    let token = if rep.token.is_empty() { "-".to_string() } else { rep.token.clone() };
    let mut out = tokio::io::stdout();
    let line = format!("QSH-CONNECT 1 {} {} {} {} {}\n", rep.port, rep.fp, token, Build::current(), version::platform());
    out.write_all(line.as_bytes()).await?;
    out.flush().await?;
    if a.tunnel {
        relay(rep.port, out).await?;
    }
    Ok(())
}

/// Ask the daemon to continue with this program, then wait until it does.
async fn upgrade_daemon(port: Option<u16>) {
    let Ok(exe) = std::env::current_exe() else { return };
    let req = LocalReq::Upgrade { exe: exe.display().to_string() };
    if let Ok(mut s) = connect_daemon(port, false).await
        && proto::write_msg(&mut s, &req).await.is_ok()
    {
        // On success the daemon does not answer: the stream closes at exec.
        if let Ok(rep) = proto::read_msg::<_, LocalReply>(&mut s).await {
            eprintln!("qsh: cannot upgrade the daemon: {}", rep.error.unwrap_or_default());
            return;
        }
    }
    let want = Build::current();
    for _ in 0..60 {
        if let Ok(rep) = local_request(port, false, &LocalReq::Info).await
            && Build::parse(&rep.build).as_ref() == Some(&want)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A routable local address for the relay, not loopback. QUIC keeps the local
/// address where a connection started; a connection that started on loopback
/// cannot later move to a real network path (the kernel does not send from
/// 127.0.0.1 to a remote host).
fn relay_ip() -> IpAddr {
    if let Some(ip) = std::env::var("SSH_CONNECTION")
        .ok()
        .and_then(|s| s.split_whitespace().nth(2).and_then(|x| x.parse::<IpAddr>().ok()))
        .filter(|ip| !ip.is_loopback() && !ip.is_unspecified())
    {
        return ip;
    }
    if let Ok(s) = std::net::UdpSocket::bind("0.0.0.0:0")
        && s.connect("192.0.2.1:9").is_ok()
            && let Ok(a) = s.local_addr()
                && !a.ip().is_unspecified() {
                    return a.ip();
                }
    IpAddr::from([127, 0, 0, 1])
}

/// Frames on stdio: `[u16 length][datagram]`.
async fn relay(port: u16, mut out: tokio::io::Stdout) -> Result<()> {
    let ip = relay_ip();
    let udp = match tokio::net::UdpSocket::bind(SocketAddr::new(if ip.is_ipv4() { [0, 0, 0, 0].into() } else { std::net::Ipv6Addr::UNSPECIFIED.into() }, 0)).await {
        Ok(u) if u.connect((ip, port)).await.is_ok() => u,
        _ => {
            let u = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
            u.connect(("127.0.0.1", port)).await?;
            u
        }
    };
    let udp = Arc::new(udp);
    let u2 = udp.clone();
    let up = async move {
        let mut inp = tokio::io::stdin();
        let mut buf = vec![0u8; 65536];
        // The input ends when the client closes the tunnel: a normal end.
        loop {
            let mut l = [0u8; 2];
            if inp.read_exact(&mut l).await.is_err() {
                return Ok(());
            }
            let n = u16::from_be_bytes(l) as usize;
            if inp.read_exact(&mut buf[..n]).await.is_err() {
                return Ok(());
            }
            let _ = u2.send(&buf[..n]).await;
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };
    let down = async move {
        let mut buf = vec![0u8; 65536];
        let mut f = Vec::with_capacity(65538);
        loop {
            let n = udp.recv(&mut buf).await?;
            f.clear();
            f.extend_from_slice(&(n as u16).to_be_bytes());
            f.extend_from_slice(&buf[..n]);
            // The client is gone.
            if out.write_all(&f).await.is_err() || out.flush().await.is_err() {
                return Ok(());
            }
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };
    tokio::select! {
        r = up => r,
        r = down => r,
    }
}

