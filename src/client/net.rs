//! Network paths for the client: direct UDP, or QUIC datagrams through an SSH
//! channel. Both are sockets of one quinn endpoint, so a connection can move from
//! one path to the other (QUIC connection migration).

use std::{
    fmt,
    io::{BufRead, BufReader, BufWriter, Read, Write},
    net::SocketAddr,
    pin::Pin,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow, bail};
use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};
use tokio::sync::mpsc;

#[derive(Clone, Debug)]
pub struct SshSpec {
    /// The ssh program and its options, for example ["ssh", "-i", "key"].
    pub argv: Vec<String>,
    pub dest: String,
    pub server_path: Option<String>,
    pub port: Option<u16>,
}

impl SshSpec {
    /// The command that ssh runs on the server: find qsh, then run it with `args`.
    pub fn remote_command(&self, args: &str) -> String {
        match &self.server_path {
            Some(p) => format!("{p} {args}"),
            // ~/.qsh/current is the build that qsh clients install. Without
            // qsh, report the platform, so that the client can install it.
            None => format!(
                "sh -c 'Q=; for c in \"$HOME/.qsh/current\" \"$(command -v qsh 2>/dev/null)\" \"$HOME/.local/bin/qsh\" \"$HOME/.cargo/bin/qsh\" \"$HOME/.qsh/bin/qsh\"; do [ -n \"$c\" ] && [ -x \"$c\" ] && {{ Q=$c; break; }}; done; [ -n \"$Q\" ] || {{ echo \"QSH-MISSING $(uname -s) $(uname -m)\"; exit 0; }}; exec \"$Q\" {args}'"
            ),
        }
    }
}

pub struct ConnectLine {
    pub port: u16,
    pub fp: String,
    pub token: Option<String>,
    /// Build and platform of the server's qsh (None from old versions).
    pub build: Option<crate::version::Build>,
    pub platform: Option<String>,
}

/// The server has no qsh. The value is its platform ("linux-x86_64").
#[derive(Debug)]
pub struct Missing(pub String);

impl std::fmt::Display for Missing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "qsh is not installed on the server ({})", self.0)
    }
}

impl std::error::Error for Missing {}

/// A running `ssh host qsh helper` process.
pub struct Helper {
    _child: SshChild,
    pub line: ConnectLine,
    to_ssh: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
    from_ssh: Option<mpsc::Receiver<Vec<u8>>>,
    alive: Arc<AtomicBool>,
}

/// An ssh process that stops when this value goes away: also when the start
/// is cancelled before the helper answered (fast start won the race). A
/// `Child` alone keeps running.
struct SshChild(Child);

impl Drop for SshChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Helper {
    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
}

pub async fn spawn_helper(spec: &SshSpec, tunnel: bool, token: bool, background: bool) -> Result<Helper> {
    let mut args = String::from("helper");
    if tunnel {
        args.push_str(" --tunnel");
    }
    if !token {
        args.push_str(" --no-token");
    }
    if let Some(p) = spec.port {
        args.push_str(&format!(" --port {p}"));
    }
    let mut cmd = Command::new(&spec.argv[0]);
    cmd.args(&spec.argv[1..]);
    if background {
        cmd.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"]);
    }
    cmd.args(["-T", "-o", "ServerAliveInterval=5", "-o", "ServerAliveCountMax=3"]);
    cmd.arg(&spec.dest).arg(spec.remote_command(&args));
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = SshChild(cmd.spawn().with_context(|| format!("start {}", spec.argv[0]))?);
    let stdin = child.0.stdin.take().unwrap();
    let stdout = child.0.stdout.take().unwrap();
    let stderr = child.0.stderr.take().unwrap();
    let alive = Arc::new(AtomicBool::new(true));

    // Messages of ssh (and of the server's shell) go to the terminal only
    // while the user waits for the login. Later they would write into the
    // session's screen, or after qsh has exited; they go to the log.
    let answered = Arc::new(AtomicBool::new(background));
    let last_err = Arc::new(Mutex::new(String::new()));
    {
        let (answered, last_err) = (answered.clone(), last_err.clone());
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if answered.load(Ordering::SeqCst) {
                    crate::paths::log(format!("ssh: {line}"));
                } else {
                    eprintln!("{line}");
                }
                *last_err.lock().unwrap() = line;
            }
        });
    }

    let (line_tx, line_rx) = tokio::sync::oneshot::channel::<Option<String>>();
    let (pkt_tx, pkt_rx) = mpsc::channel::<Vec<u8>>(4096);
    let a = alive.clone();
    std::thread::spawn(move || {
        let mut r = BufReader::with_capacity(256 * 1024, stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match r.read_line(&mut line) {
                Ok(0) | Err(_) => {
                    a.store(false, Ordering::SeqCst);
                    let _ = line_tx.send(None);
                    return;
                }
                Ok(_) => {}
            }
            // Shell startup files can print text first; skip it.
            if line.starts_with("QSH-CONNECT ") || line.starts_with("QSH-MISSING ") {
                let _ = line_tx.send(Some(line.trim().to_string()));
                break;
            }
        }
        if tunnel {
            let mut buf = vec![0u8; 65536];
            loop {
                let mut l = [0u8; 2];
                if r.read_exact(&mut l).is_err() {
                    break;
                }
                let n = u16::from_be_bytes(l) as usize;
                if r.read_exact(&mut buf[..n]).is_err() {
                    break;
                }
                if pkt_tx.blocking_send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        }
        a.store(false, Ordering::SeqCst);
    });

    let (to_ssh, from_ssh) = if tunnel {
        let (wtx, wrx) = std::sync::mpsc::sync_channel::<Vec<u8>>(4096);
        let a = alive.clone();
        std::thread::spawn(move || {
            let mut w = BufWriter::with_capacity(256 * 1024, stdin);
            while let Ok(p) = wrx.recv() {
                let mut ok = w.write_all(&(p.len() as u16).to_be_bytes()).is_ok() && w.write_all(&p).is_ok();
                while ok {
                    match wrx.try_recv() {
                        Ok(p) => ok = w.write_all(&(p.len() as u16).to_be_bytes()).is_ok() && w.write_all(&p).is_ok(),
                        Err(_) => break,
                    }
                }
                if !ok || w.flush().is_err() {
                    break;
                }
            }
            a.store(false, Ordering::SeqCst);
        });
        (Some(wtx), Some(pkt_rx))
    } else {
        drop(stdin);
        (None, None)
    };

    // Cancel safe: if this future is dropped, `child` stops ssh.
    let timeout = Duration::from_secs(if background { 20 } else { 120 });
    let line = match tokio::time::timeout(timeout, line_rx).await {
        Ok(Ok(Some(line))) => line,
        Ok(_) => {
            let e = last_err.lock().unwrap().clone();
            bail!("ssh exited before the qsh server answered{}", if background && !e.is_empty() { format!(": {e}") } else { String::new() })
        }
        Err(_) => bail!("no answer from the qsh server over ssh"),
    };
    answered.store(true, Ordering::SeqCst);
    let f: Vec<&str> = line.split_whitespace().collect();
    if f[0] == "QSH-MISSING" && f.len() >= 3 {
        return Err(Missing(crate::version::platform_of_uname(f[1], f[2])).into());
    }
    if f.len() < 5 || f[1] != "1" {
        bail!("unexpected answer from the qsh server: {line}");
    }
    let line = ConnectLine {
        port: f[2].parse().context("bad port")?,
        fp: f[3].to_string(),
        token: (f[4] != "-").then(|| f[4].to_string()),
        build: f.get(5).and_then(|b| crate::version::Build::parse(b)),
        platform: f.get(6).map(|p| p.to_string()),
    };
    Ok(Helper { _child: child, line, to_ssh, from_ssh, alive })
}

/// Find the address of the host, as ssh would (`ssh -G` applies the ssh config).
pub async fn resolve(spec: &SshSpec, port: u16) -> Result<SocketAddr> {
    let out = Command::new(&spec.argv[0]).args(&spec.argv[1..]).arg("-G").arg(&spec.dest).output();
    let mut host = spec.dest.rsplit('@').next().unwrap_or(&spec.dest).to_string();
    if let Ok(o) = out {
        for l in String::from_utf8_lossy(&o.stdout).lines() {
            if let Some(h) = l.strip_prefix("hostname ") {
                host = h.trim().to_string();
            }
        }
    }
    let host = host.trim_start_matches('[').trim_end_matches(']').to_string();
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port)).await.with_context(|| format!("resolve {host}"))?.collect();
    addrs.iter().find(|a| a.is_ipv4()).or(addrs.first()).copied().ok_or_else(|| anyhow!("no address for {host}"))
}

/// A quinn socket whose datagrams go through the SSH helper's stdio.
pub struct TunnelSocket {
    tx: std::sync::mpsc::SyncSender<Vec<u8>>,
    rx: Mutex<mpsc::Receiver<Vec<u8>>>,
    peer: SocketAddr,
    local: SocketAddr,
}

impl fmt::Debug for TunnelSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TunnelSocket({})", self.peer)
    }
}

impl TunnelSocket {
    /// `peer` must be the remote address of the connections that use this socket.
    pub fn take_from(h: &mut Helper, peer: SocketAddr) -> Result<Arc<TunnelSocket>> {
        let tx = h.to_ssh.take().ok_or_else(|| anyhow!("helper has no tunnel"))?;
        let rx = h.from_ssh.take().ok_or_else(|| anyhow!("helper has no tunnel"))?;
        let local: SocketAddr = if peer.is_ipv4() { "0.0.0.0:0".parse().unwrap() } else { "[::]:0".parse().unwrap() };
        Ok(Arc::new(TunnelSocket { tx, rx: Mutex::new(rx), peer, local }))
    }
}

#[derive(Debug)]
struct Ready;

impl UdpPoller for Ready {
    fn poll_writable(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncUdpSocket for TunnelSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(Ready)
    }

    fn try_send(&self, t: &Transmit) -> std::io::Result<()> {
        let seg = t.segment_size.unwrap_or(t.contents.len()).max(1);
        for chunk in t.contents.chunks(seg) {
            // Like UDP: when the pipe is full, drop the datagram.
            let _ = self.tx.try_send(chunk.to_vec());
        }
        Ok(())
    }

    fn poll_recv(&self, cx: &mut Context, bufs: &mut [std::io::IoSliceMut<'_>], meta: &mut [RecvMeta]) -> Poll<std::io::Result<usize>> {
        let mut rx = self.rx.lock().unwrap();
        match rx.poll_recv(cx) {
            Poll::Ready(Some(p)) => {
                let n = p.len().min(bufs[0].len());
                bufs[0][..n].copy_from_slice(&p[..n]);
                meta[0] = RecvMeta { addr: self.peer, len: n, stride: n, ecn: None, dst_ip: None };
                Poll::Ready(Ok(1))
            }
            // A closed tunnel never delivers again.
            Poll::Ready(None) | Poll::Pending => Poll::Pending,
        }
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        Ok(self.local)
    }

    fn may_fragment(&self) -> bool {
        false
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Transport {
    Auto,
    Udp,
    Ssh,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PathKind {
    Direct,
    Tunnel,
}

pub struct Link {
    pub ep: quinn::Endpoint,
    pub addr: SocketAddr,
    pub kind: PathKind,
    pub transport: Transport,
    pub spec: Option<SshSpec>,
    fp: String,
    tunnel: Option<Helper>,
    /// The last round-trip time of the session (for new handshakes).
    pub rtt: Duration,
}

/// What a path race needs.
#[derive(Clone)]
pub struct RaceSpec {
    pub addr: SocketAddr,
    pub fp: String,
    pub transport: Transport,
    pub spec: Option<SshSpec>,
    pub rtt: Duration,
}

/// The winner of a path race: a connection with its endpoint.
pub struct Candidate {
    pub ep: quinn::Endpoint,
    pub conn: quinn::Connection,
    pub kind: PathKind,
    helper: Option<Helper>,
}

/// A client config for a handshake when the round-trip time is known: a
/// lost handshake packet is sent again after about 3 RTT (the default
/// assumes 150 ms, about 0.5 s).
fn client_config_rtt(fp: &str, rtt: Duration) -> Result<quinn::ClientConfig> {
    let mut cfg = crate::tls::client_config(fp)?;
    let mut t = crate::tls::transport();
    t.initial_rtt(rtt.clamp(Duration::from_millis(25), Duration::from_millis(300)));
    cfg.transport_config(Arc::new(t));
    Ok(cfg)
}

fn udp_endpoint(addr: &SocketAddr, fp: &str) -> Result<quinn::Endpoint> {
    let mut ep = quinn::Endpoint::new(quinn::EndpointConfig::default(), None, bind_any(addr)?, Arc::new(quinn::TokioRuntime))?;
    ep.set_default_client_config(crate::tls::client_config(fp)?);
    Ok(ep)
}

fn tunnel_endpoint(h: &mut Helper, addr: &SocketAddr, fp: &str) -> Result<quinn::Endpoint> {
    let sock = TunnelSocket::take_from(h, *addr)?;
    let mut ep = quinn::Endpoint::new_with_abstract_socket(quinn::EndpointConfig::default(), None, sock, Arc::new(quinn::TokioRuntime))?;
    ep.set_default_client_config(crate::tls::client_config(fp)?);
    Ok(ep)
}

async fn handshake_on(ep: quinn::Endpoint, addr: SocketAddr, timeout: Duration) -> Result<(quinn::Endpoint, quinn::Connection)> {
    let c = ep.connect(addr, "qsh")?;
    let conn = tokio::time::timeout(timeout, c).await.map_err(|_| anyhow!("timeout"))??;
    Ok((ep, conn))
}

/// Make a QUIC connection on direct UDP and through the SSH channel at the same
/// time; the first handshake wins. UDP is preferred: when the tunnel wins, UDP
/// gets 150 ms more. `helper` is a running tunnel helper, if there is one;
/// otherwise a new helper starts after `tunnel_delay`.
pub async fn race(rs: RaceSpec, helper: Option<Helper>, tunnel_delay: Duration) -> Result<Candidate> {
    let direct = async {
        let ep = udp_endpoint(&rs.addr, &rs.fp)?;
        let (ep, conn) = handshake_on(ep, rs.addr, Duration::from_secs(5)).await?;
        Ok::<_, anyhow::Error>(Candidate { ep, conn, kind: PathKind::Direct, helper: None })
    };
    let tunnel = async {
        let mut h = match helper {
            Some(h) => h,
            None => {
                tokio::time::sleep(tunnel_delay).await;
                let spec = rs.spec.as_ref().ok_or_else(|| anyhow!("no ssh"))?;
                spawn_helper(spec, true, false, true).await?
            }
        };
        let ep = tunnel_endpoint(&mut h, &rs.addr, &rs.fp)?;
        let (ep, conn) = handshake_on(ep, rs.addr, Duration::from_secs(10)).await?;
        Ok::<_, anyhow::Error>(Candidate { ep, conn, kind: PathKind::Tunnel, helper: Some(h) })
    };
    match rs.transport {
        Transport::Udp => direct.await,
        Transport::Ssh => tunnel.await,
        Transport::Auto => {
            tokio::pin!(direct, tunnel);
            let (mut derr, mut terr) = (None, None);
            loop {
                tokio::select! {
                    r = &mut direct, if derr.is_none() => match r {
                        Ok(c) => return Ok(c),
                        Err(e) => derr = Some(e),
                    },
                    r = &mut tunnel, if terr.is_none() => match r {
                        Ok(c) => {
                            // UDP is better: give it a moment to win.
                            if derr.is_none()
                                && let Ok(Ok(d)) = tokio::time::timeout(Duration::from_millis(150), &mut direct).await
                            {
                                return Ok(d);
                            }
                            return Ok(c);
                        }
                        Err(e) => terr = Some(e),
                    },
                }
                if let (Some(d), Some(t)) = (&derr, &terr) {
                    bail!("no path to the server (UDP: {d:#}; SSH: {t:#})");
                }
            }
        }
    }
}

/// Like `race`, for a reconnect: it does not give up. A fresh UDP handshake
/// (new socket, last round-trip time) starts every second, and up to three
/// overlap, so the session continues soon after the network comes back. SSH
/// tunnel attempts run at the same time, one after the other.
pub async fn race_until_up(rs: RaceSpec) -> Candidate {
    let direct = async {
        let mut tries = tokio::task::JoinSet::new();
        let mut every = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = every.tick(), if tries.len() < 3 => {
                    let (addr, fp, rtt) = (rs.addr, rs.fp.clone(), rs.rtt);
                    tries.spawn(async move {
                        let ep = quinn::Endpoint::new(quinn::EndpointConfig::default(), None, bind_any(&addr)?, Arc::new(quinn::TokioRuntime))?;
                        let c = ep.connect_with(client_config_rtt(&fp, rtt)?, addr, "qsh")?;
                        let conn = tokio::time::timeout(Duration::from_secs(3), c).await.map_err(|_| anyhow!("timeout"))??;
                        Ok::<_, anyhow::Error>((ep, conn))
                    });
                }
                Some(r) = tries.join_next() => {
                    if let Ok(Ok((ep, conn))) = r {
                        return Candidate { ep, conn, kind: PathKind::Direct, helper: None };
                    }
                }
            }
        }
    };
    let tunnel = async {
        let Some(spec) = rs.spec.clone() else { return std::future::pending().await };
        tokio::time::sleep(Duration::from_millis(300)).await;
        loop {
            let r = async {
                let mut h = spawn_helper(&spec, true, false, true).await?;
                let ep = tunnel_endpoint(&mut h, &rs.addr, &rs.fp)?;
                let (ep, conn) = handshake_on(ep, rs.addr, Duration::from_secs(10)).await?;
                Ok::<_, anyhow::Error>(Candidate { ep, conn, kind: PathKind::Tunnel, helper: Some(h) })
            };
            match r.await {
                Ok(c) => return c,
                Err(e) => {
                    crate::paths::log(format!("reconnect over SSH: {e:#}"));
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    };
    match rs.transport {
        Transport::Udp => direct.await,
        Transport::Ssh => tunnel.await,
        Transport::Auto => {
            tokio::pin!(direct, tunnel);
            tokio::select! {
                c = &mut direct => c,
                c = &mut tunnel => {
                    // UDP is better: give it a moment to win.
                    match tokio::time::timeout(Duration::from_millis(150), &mut direct).await {
                        Ok(d) => d,
                        Err(_) => c,
                    }
                }
            }
        }
    }
}

fn bind_any(addr: &SocketAddr) -> std::io::Result<std::net::UdpSocket> {
    std::net::UdpSocket::bind(if addr.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" })
}

impl Link {
    pub fn new(addr: SocketAddr, fp: &str, transport: Transport, spec: Option<SshSpec>) -> Result<Link> {
        let ep = udp_endpoint(&addr, fp)?;
        Ok(Link { ep, addr, kind: PathKind::Direct, transport, spec, fp: fp.to_string(), tunnel: None, rtt: Duration::from_millis(150) })
    }

    /// Use the connection of a race winner (and its endpoint and tunnel).
    pub fn adopt(&mut self, c: Candidate) -> quinn::Connection {
        self.ep = c.ep;
        self.kind = c.kind;
        self.tunnel = c.helper;
        c.conn
    }

    pub fn race_spec(&self) -> RaceSpec {
        RaceSpec { addr: self.addr, fp: self.fp.clone(), transport: self.transport, spec: self.spec.clone(), rtt: self.rtt }
    }

    pub fn path_name(&self) -> String {
        match self.kind {
            PathKind::Direct => format!("UDP {}", self.addr),
            PathKind::Tunnel => "QUIC in SSH".to_string(),
        }
    }

    /// Move to a new UDP socket (new port, new NAT mapping, current interface).
    pub fn use_direct(&mut self) -> Result<()> {
        self.ep.rebind(bind_any(&self.addr)?)?;
        self.kind = PathKind::Direct;
        self.tunnel = None;
        Ok(())
    }

    pub fn use_tunnel(&mut self, mut h: Helper) -> Result<()> {
        let s = TunnelSocket::take_from(&mut h, self.addr)?;
        self.ep.rebind_abstract(s)?;
        self.kind = PathKind::Tunnel;
        self.tunnel = Some(h);
        Ok(())
    }

    pub fn tunnel_dead(&self) -> bool {
        self.kind == PathKind::Tunnel && self.tunnel.as_ref().is_none_or(|t| !t.alive())
    }

    pub async fn connect(&self, timeout: Duration) -> Result<quinn::Connection> {
        let c = self.ep.connect(self.addr, "qsh")?;
        Ok(tokio::time::timeout(timeout, c).await.map_err(|_| anyhow!("timeout"))??)
    }

    /// Connect when the round-trip time is known: a lost handshake packet is
    /// sent again after about 3 RTT (the default assumes 150 ms, about 0.5 s).
    pub async fn connect_rtt(&self, rtt: Duration, timeout: Duration) -> Result<quinn::Connection> {
        let c = self.ep.connect_with(client_config_rtt(&self.fp, rtt)?, self.addr, "qsh")?;
        Ok(tokio::time::timeout(timeout, c).await.map_err(|_| anyhow!("timeout"))??)
    }

    /// Try a QUIC handshake over direct UDP on a separate endpoint (path probe).
    pub async fn probe_direct(&self, fp: &str) -> bool {
        let Ok(sock) = bind_any(&self.addr) else { return false };
        let Ok(mut ep) = quinn::Endpoint::new(quinn::EndpointConfig::default(), None, sock, Arc::new(quinn::TokioRuntime)) else {
            return false;
        };
        let Ok(cfg) = crate::tls::client_config(fp) else { return false };
        ep.set_default_client_config(cfg);
        let ok = match ep.connect(self.addr, "qsh") {
            Ok(c) => match tokio::time::timeout(Duration::from_secs(2), c).await {
                Ok(Ok(conn)) => {
                    conn.close(0u32.into(), b"probe");
                    true
                }
                _ => false,
            },
            Err(_) => false,
        };
        ep.close(0u32.into(), b"");
        ok
    }
}
