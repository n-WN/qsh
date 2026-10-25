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
            None => format!(
                "sh -c 'Q=$(command -v qsh 2>/dev/null); for c in \"$HOME/.local/bin/qsh\" \"$HOME/.cargo/bin/qsh\" \"$HOME/.qsh/bin/qsh\"; do [ -n \"$Q\" ] && break; [ -x \"$c\" ] && Q=$c; done; [ -n \"$Q\" ] || {{ echo \"qsh: qsh is not installed on the server\" >&2; exit 127; }}; exec \"$Q\" {args}'"
            ),
        }
    }
}

pub struct ConnectLine {
    pub port: u16,
    pub fp: String,
    pub token: Option<String>,
}

/// A running `ssh host qsh helper` process.
pub struct Helper {
    child: Child,
    pub line: ConnectLine,
    to_ssh: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
    from_ssh: Option<mpsc::Receiver<Vec<u8>>>,
    alive: Arc<AtomicBool>,
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
    cmd.stderr(if background { Stdio::null() } else { Stdio::inherit() });
    let mut child = cmd.spawn().with_context(|| format!("start {}", spec.argv[0]))?;
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let alive = Arc::new(AtomicBool::new(true));

    let (line_tx, line_rx) = std::sync::mpsc::channel::<Result<String>>();
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
                    let _ = line_tx.send(Err(anyhow!("ssh exited before the qsh server answered")));
                    return;
                }
                Ok(_) => {}
            }
            // Shell startup files can print text first; skip it.
            if line.starts_with("QSH-CONNECT ") {
                let _ = line_tx.send(Ok(line.trim().to_string()));
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

    let timeout = if background { 20 } else { 120 };
    let line = tokio::task::spawn_blocking(move || line_rx.recv_timeout(Duration::from_secs(timeout)))
        .await?
        .map_err(|_| anyhow!("no answer from the qsh server over ssh"))??;
    let f: Vec<&str> = line.split_whitespace().collect();
    if f.len() < 5 || f[1] != "1" {
        bail!("unexpected answer from the qsh server: {line}");
    }
    let line = ConnectLine {
        port: f[2].parse().context("bad port")?,
        fp: f[3].to_string(),
        token: (f[4] != "-").then(|| f[4].to_string()),
    };
    Ok(Helper { child, line, to_ssh, from_ssh, alive })
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
    tunnel: Option<Helper>,
}

fn bind_any(addr: &SocketAddr) -> std::io::Result<std::net::UdpSocket> {
    std::net::UdpSocket::bind(if addr.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" })
}

impl Link {
    pub fn new(addr: SocketAddr, fp: &str, transport: Transport, spec: Option<SshSpec>) -> Result<Link> {
        let mut ep = quinn::Endpoint::new(quinn::EndpointConfig::default(), None, bind_any(&addr)?, Arc::new(quinn::TokioRuntime))?;
        ep.set_default_client_config(crate::tls::client_config(fp)?);
        Ok(Link { ep, addr, kind: PathKind::Direct, transport, spec, tunnel: None })
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
