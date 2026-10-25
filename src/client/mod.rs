//! The client: bootstrap over SSH, connect over QUIC, and keep the session alive
//! across network changes, sleep and roaming.

pub mod clipboard;
pub mod net;
pub mod rpc;
pub mod term;

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow, bail};
use tokio::sync::mpsc;

use crate::proto::{self, Control, Hello, Welcome, kind};
use net::{Link, PathKind, SshSpec, Transport};
use term::{Event, Out};

pub struct Opts {
    pub new_session: bool,
    pub ssh: Option<SshSpec>,
    /// For share links: address, fingerprint and token, without SSH.
    pub direct: Option<(SocketAddr, String, String)>,
    pub session: String,
    pub transport: Transport,
    pub command: Vec<String>,
    pub download_dir: PathBuf,
    pub policy: rpc::Policy,
    pub osc52: bool,
    pub verbose: bool,
}

pub struct Shared {
    pub conn: Mutex<Option<quinn::Connection>>,
    pub offset: AtomicU64,
    pub out: mpsc::Sender<Out>,
    pub policy: rpc::Policy,
    pub download_dir: PathBuf,
}

enum Outcome {
    Exit(i32),
    Detached(String),
    Quit,
    Lost(String),
}

enum Fail {
    Retry(anyhow::Error),
    Fatal(anyhow::Error),
    /// The server forgot this client (for example, after a long time away).
    Unknown,
}

type Streams = (quinn::Connection, quinn::SendStream, quinn::RecvStream);

const HELP: &str = "qsh keys after Ctrl-^:  . quit (session stays)  d detach  c new session  n/p next/previous  0-9 select  w list  Ctrl-^ send Ctrl-^";

fn term_env() -> (String, Option<String>, Vec<(String, String)>) {
    let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into());
    let common = ["xterm", "xterm-256color", "screen", "screen-256color", "tmux", "tmux-256color", "vt100", "linux", "dumb"];
    let terminfo = if common.contains(&term.as_str()) {
        None
    } else {
        std::process::Command::new("infocmp")
            .args(["-x", &term])
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let mut env = Vec::new();
    for (k, v) in std::env::vars() {
        if matches!(k.as_str(), "COLORTERM" | "TERM_PROGRAM" | "TERM_PROGRAM_VERSION" | "LANG") || k.starts_with("LC_") {
            env.push((k, v));
        }
    }
    (term, terminfo, env)
}

fn set_size(h: &mut Hello, s: term::Size) {
    h.cols = s.cols;
    h.rows = s.rows;
    h.xpix = s.xpix;
    h.ypix = s.ypix;
}

async fn handshake(conn: &quinn::Connection, hello: &Hello) -> std::result::Result<(quinn::SendStream, quinn::RecvStream, Welcome), Fail> {
    let (mut s, mut r) = conn.open_bi().await.map_err(|e| Fail::Retry(e.into()))?;
    proto::write_msg(&mut s, hello).await.map_err(Fail::Retry)?;
    let w: Welcome = tokio::time::timeout(Duration::from_secs(10), proto::read_msg(&mut r))
        .await
        .map_err(|_| Fail::Retry(anyhow!("no answer")))?
        .map_err(Fail::Retry)?;
    if !w.ok {
        let e = w.error.unwrap_or_default();
        return Err(if e.contains("unknown client") { Fail::Unknown } else { Fail::Fatal(anyhow!(e)) });
    }
    Ok((s, r, w))
}

pub async fn run(opts: Opts) -> Result<i32> {
    if !term::is_tty() {
        bail!("qsh needs a terminal on stdin and stdout");
    }
    let size = term::size();
    let (term_name, terminfo, env) = term_env();

    // Bootstrap: SSH gives the port, the certificate fingerprint and a one-time token.
    let (mut link, fp, token) = if let Some(spec) = &opts.ssh {
        let want_tunnel = opts.transport != Transport::Udp;
        let helper = net::spawn_helper(spec, want_tunnel, true, false).await?;
        let addr = net::resolve(spec, helper.line.port).await?;
        let fp = helper.line.fp.clone();
        let token = helper.line.token.clone().ok_or_else(|| anyhow!("no token from the server"))?;
        let mut link = Link::new(addr, &fp, opts.transport, Some(spec.clone()))?;
        let conn = first_connect(&mut link, helper, opts.verbose).await?;
        (link, fp, (token, conn))
    } else if let Some((addr, fp, token)) = &opts.direct {
        let link = Link::new(*addr, fp, Transport::Udp, None)?;
        let conn = link.connect(Duration::from_secs(8)).await.map_err(|e| anyhow!("cannot reach {addr}: {e:#}"))?;
        (link, fp.clone(), (token.clone(), conn))
    } else {
        bail!("no destination");
    };
    let (token, conn) = token;

    let mut hello = Hello {
        version: proto::VERSION,
        token,
        session: opts.session.clone(),
        client_id: None,
        offset: 0,
        cols: size.cols,
        rows: size.rows,
        xpix: size.xpix,
        ypix: size.ypix,
        term: term_name,
        terminfo,
        env,
        command: opts.command.clone(),
        server_addr: link.addr.to_string(),
        new: opts.new_session,
    };
    let (send, recv, w) = match handshake(&conn, &hello).await {
        Ok(x) => x,
        Err(Fail::Retry(e)) | Err(Fail::Fatal(e)) => bail!("{e:#}"),
        Err(Fail::Unknown) => bail!("unknown client"),
    };
    hello.client_id = Some(w.client_id);
    hello.token = w.resume_token.clone();
    hello.session = w.session.clone();
    hello.new = false;
    hello.terminfo = None;
    hello.command.clear();
    if w.readonly {
        eprintln!("qsh: read-only view of session {}", w.session);
    }

    let (out_tx, out_rx) = mpsc::channel::<Out>(16);
    let sh = Arc::new(Shared {
        conn: Mutex::new(None),
        offset: AtomicU64::new(0),
        out: out_tx,
        policy: opts.policy.clone(),
        download_dir: opts.download_dir.clone(),
    });
    let ctl_path = rpc::spawn_ctl(sh.clone()).ok();

    term::enter_raw()?;
    let out_thread = term::spawn_stdout(out_rx, opts.osc52);
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<Event>();
    term::spawn_stdin(ev_tx.clone());
    for (sig, kind) in [(libc::SIGUSR1, PathKind::Direct), (libc::SIGUSR2, PathKind::Tunnel)] {
        let tx = ev_tx.clone();
        tokio::spawn(async move {
            let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(sig)) else { return };
            while s.recv().await.is_some() {
                if tx.send(Event::Migrate(kind)).is_err() {
                    break;
                }
            }
        });
    }
    tokio::spawn(async move {
        let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()) else { return };
        while s.recv().await.is_some() {
            if ev_tx.send(Event::Resize(term::size())).is_err() {
                break;
            }
        }
    });

    let mut pending = Vec::new();
    let mut streams: Streams = (conn, send, recv);
    let result = loop {
        let (conn, send, recv) = streams;
        *sh.conn.lock().unwrap() = Some(conn.clone());
        let o = drive(&sh, &mut link, &fp, conn, send, recv, &mut ev_rx, &mut hello, &mut pending).await;
        *sh.conn.lock().unwrap() = None;
        match o {
            Outcome::Exit(code) => break Ok((code, None)),
            Outcome::Detached(r) => break Ok((0, Some(format!("[qsh: {r}; session {} stays on the server]", hello.session)))),
            Outcome::Quit => break Ok((0, Some(format!("[qsh: quit; session {} stays on the server]", hello.session)))),
            Outcome::Lost(why) => {
                crate::paths::log(format!("connection lost: {why}"));
                match reconnect(&sh, &mut link, &mut ev_rx, &mut hello, &mut pending).await {
                    Ok(Some(s)) => streams = s,
                    Ok(None) => break Ok((0, Some(format!("[qsh: quit; session {} stays on the server]", hello.session)))),
                    Err(e) => break Err(e),
                }
            }
        }
    };

    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let _ = sh.out.send(Out::Flush(done_tx)).await;
    let _ = done_rx.recv_timeout(Duration::from_secs(2));
    drop(out_thread);
    term::restore();
    if let Some(p) = ctl_path {
        let _ = std::fs::remove_file(p);
    }
    link.ep.close(0u32.into(), b"bye");
    match result {
        Ok((code, msg)) => {
            if let Some(m) = msg {
                eprintln!("\r\n{m}");
            }
            Ok(code)
        }
        Err(e) => {
            eprintln!("qsh: {e:#}");
            Ok(255)
        }
    }
}

async fn first_connect(link: &mut Link, helper: net::Helper, verbose: bool) -> Result<quinn::Connection> {
    match link.transport {
        Transport::Udp => link.connect(Duration::from_secs(6)).await.map_err(|e| anyhow!("UDP to {} failed: {e:#}", link.addr)),
        Transport::Ssh => {
            link.use_tunnel(helper)?;
            link.connect(Duration::from_secs(20)).await
        }
        Transport::Auto => match link.connect(Duration::from_millis(2500)).await {
            Ok(c) => {
                if verbose {
                    eprintln!("qsh: QUIC over UDP to {}", link.addr);
                }
                drop(helper);
                Ok(c)
            }
            Err(_) => {
                eprintln!("qsh: no UDP path to {}; QUIC goes through the SSH channel", link.addr);
                link.use_tunnel(helper)?;
                link.connect(Duration::from_secs(20)).await
            }
        },
    }
}

#[allow(clippy::too_many_arguments)]
async fn drive(
    sh: &Arc<Shared>,
    link: &mut Link,
    fp: &str,
    conn: quinn::Connection,
    mut send: quinn::SendStream,
    recv: quinn::RecvStream,
    ev_rx: &mut mpsc::UnboundedReceiver<Event>,
    hello: &mut Hello,
    pending: &mut Vec<u8>,
) -> Outcome {
    let size = term::Size { cols: hello.cols, rows: hello.rows, xpix: hello.xpix, ypix: hello.ypix };
    if send.write_all(&proto::frame(kind::RESIZE, &size.encode())).await.is_err() {
        return Outcome::Lost("write".into());
    }
    if !pending.is_empty() {
        if send.write_all(&proto::frame(kind::INPUT, pending)).await.is_err() {
            return Outcome::Lost("write".into());
        }
        pending.clear();
    }

    let mut reader = Reader::spawn(sh, recv);
    let c2 = conn.clone();
    let sh3 = sh.clone();
    let acceptor = tokio::spawn(async move {
        while let Ok((s, r)) = c2.accept_bi().await {
            tokio::spawn(rpc::serve(sh3.clone(), s, r));
        }
    });
    let (probe_tx, mut probe_rx) = mpsc::unbounded_channel::<bool>();
    // Liveness: the server echoes our datagrams. Received packets alone are not
    // enough: when only the upstream direction fails, data still comes in.
    let last_echo = Arc::new(Mutex::new(Instant::now()));
    let le = last_echo.clone();
    let c4 = conn.clone();
    let echo = tokio::spawn(async move {
        while c4.read_datagram().await.is_ok() {
            *le.lock().unwrap() = Instant::now();
        }
    });

    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut last_ping = Instant::now() - Duration::from_secs(10);
    let mut seq: u32 = 0;
    let mut rebound = false;
    let mut shown: Option<u64> = None;
    let mut status_until: Option<Instant> = None;
    let mut last_probe = Instant::now();
    let mut probing = false;

    let outcome = loop {
        tokio::select! {
            ev = ev_rx.recv() => {
                let Some(ev) = ev else { break Outcome::Quit };
                match ev {
                    Event::Input(b) => {
                        if send.write_all(&proto::frame(kind::INPUT, &b)).await.is_err() {
                            pending.extend_from_slice(&b);
                            break Outcome::Lost("write".into());
                        }
                    }
                    Event::Resize(sz) => {
                        set_size(hello, sz);
                        if send.write_all(&proto::frame(kind::RESIZE, &sz.encode())).await.is_err() {
                            break Outcome::Lost("write".into());
                        }
                    }
                    Event::Detach => {
                        let _ = send.write_all(&proto::control_frame(&Control::Detach)).await;
                        match tokio::time::timeout(Duration::from_secs(2), reader.net_rx.recv()).await {
                            Ok(Some(o @ Outcome::Detached(_))) => break o,
                            _ => break Outcome::Detached("detached".into()),
                        }
                    }
                    Event::Quit => break Outcome::Quit,
                    Event::Help => {
                        let _ = sh.out.send(Out::Status(Some(HELP.into()))).await;
                        status_until = Some(Instant::now() + Duration::from_secs(6));
                    }
                    Event::Key(k) => {
                        let list = match rpc_list(&conn).await {
                            Ok(l) => l,
                            Err(_) => continue,
                        };
                        if k == 'w' {
                            let names: Vec<String> = list
                                .iter()
                                .enumerate()
                                .map(|(i, s)| if s.name == hello.session { format!("{i}:[{}]", s.name) } else { format!("{i}:{}", s.name) })
                                .collect();
                            let _ = sh.out.send(Out::Status(Some(format!("qsh sessions  {}", names.join("  "))))).await;
                            status_until = Some(Instant::now() + Duration::from_secs(4));
                            continue;
                        }
                        let Some(target) = pick_session(k, &list, &hello.session) else { continue };
                        match switch_session(sh, &conn, hello, target).await {
                            Ok((s2, r2)) => {
                                reader.task.abort();
                                let _ = send.write_all(&proto::control_frame(&Control::Detach)).await;
                                let _ = send.finish();
                                send = s2;
                                reader = Reader::spawn(sh, r2);
                                let _ = sh.out.send(Out::Status(Some(format!("qsh: session {}", hello.session)))).await;
                                status_until = Some(Instant::now() + Duration::from_secs(2));
                            }
                            Err(e) => {
                                let _ = sh.out.send(Out::Status(Some(format!("qsh: {e:#}")))).await;
                                status_until = Some(Instant::now() + Duration::from_secs(4));
                            }
                        }
                    }
                    Event::Migrate(PathKind::Direct) => {
                        crate::paths::log("signal: move to a new UDP socket");
                        let _ = link.use_direct();
                    }
                    Event::Migrate(PathKind::Tunnel) => {
                        if let Some(spec) = link.spec.clone() {
                            crate::paths::log("signal: move to the SSH tunnel");
                            match net::spawn_helper(&spec, true, false, true).await {
                                Ok(h) => { let _ = link.use_tunnel(h); }
                                Err(e) => crate::paths::log(format!("tunnel: {e:#}")),
                            }
                        }
                    }
                }
            }
            o = reader.net_rx.recv() => {
                let o = o.unwrap_or(Outcome::Lost("reader".into()));
                if let Outcome::Exit(code) = o {
                    // The shell of this session ended. Show another session, if any.
                    let others = rpc_list(&conn).await.unwrap_or_default();
                    let ended = hello.session.clone();
                    if let Some(next) = others.iter().find(|s| s.name != ended)
                        && let Ok((s2, r2)) = switch_session(sh, &conn, hello, SessionTarget::Name(next.name.clone())).await {
                            send = s2;
                            reader = Reader::spawn(sh, r2);
                            let _ = sh.out.send(Out::Status(Some(format!("qsh: session {ended} ended; now in session {}", hello.session)))).await;
                            status_until = Some(Instant::now() + Duration::from_secs(3));
                            continue;
                        }
                    break Outcome::Exit(code);
                }
                break o;
            }
            Some(mut v) = reader.ack_rx.recv() => {
                while let Ok(x) = reader.ack_rx.try_recv() {
                    v = x;
                }
                if send.write_all(&proto::frame(kind::ACK, &v.to_be_bytes())).await.is_err() {
                    break Outcome::Lost("write".into());
                }
            }
            Some(ok) = probe_rx.recv() => {
                probing = false;
                // UDP works again: move this connection off the SSH tunnel.
                if ok && link.kind == PathKind::Tunnel && link.use_direct().is_ok() {
                    crate::paths::log("UDP works again: connection moved to direct UDP");
                    rebound = false;
                }
            }
            _ = tick.tick() => {
                if last_ping.elapsed() >= Duration::from_millis(1000) {
                    last_ping = Instant::now();
                    seq = seq.wrapping_add(1);
                    let mut d = vec![b'P'];
                    d.extend_from_slice(&seq.to_be_bytes());
                    let _ = conn.send_datagram(d.into());
                }
                if status_until.is_some_and(|t| Instant::now() > t) {
                    status_until = None;
                    let _ = sh.out.try_send(Out::Status(None));
                }
                let idle = last_echo.lock().unwrap().elapsed();
                if idle > Duration::from_millis(2500) {
                    let secs = idle.as_secs();
                    if shown != Some(secs) {
                        shown = Some(secs);
                        let _ = sh.out.try_send(Out::Status(Some(format!("[qsh: no contact for {secs}s]"))));
                    }
                } else if shown.take().is_some() {
                    let _ = sh.out.try_send(Out::Status(None));
                }
                if idle > Duration::from_secs(3) && !rebound && link.kind == PathKind::Direct {
                    // Maybe the network changed: a new socket gives a new path.
                    rebound = true;
                    crate::paths::log("no echo for 3s: rebind to a new UDP socket");
                    let _ = link.use_direct();
                }
                if idle > Duration::from_secs(8) {
                    break Outcome::Lost("timeout".into());
                }
                if link.tunnel_dead() {
                    break Outcome::Lost("tunnel closed".into());
                }
                if link.kind == PathKind::Tunnel && link.transport == Transport::Auto && !probing && last_probe.elapsed() > Duration::from_secs(30) {
                    probing = true;
                    last_probe = Instant::now();
                    let probe_link_addr = link.addr;
                    let fp = fp.to_string();
                    let tx = probe_tx.clone();
                    let ep_probe = ProbeTarget { addr: probe_link_addr, fp };
                    tokio::spawn(async move { let _ = tx.send(ep_probe.run().await); });
                }
            }
        }
    };
    reader.task.abort();
    acceptor.abort();
    echo.abort();
    if shown.is_some() {
        let _ = sh.out.try_send(Out::Status(None));
    }
    conn.close(0u32.into(), b"");
    outcome
}

struct Reader {
    task: tokio::task::JoinHandle<()>,
    net_rx: mpsc::UnboundedReceiver<Outcome>,
    ack_rx: mpsc::UnboundedReceiver<u64>,
}

impl Reader {
    fn spawn(sh: &Arc<Shared>, recv: quinn::RecvStream) -> Reader {
        let (net_tx, net_rx) = mpsc::unbounded_channel::<Outcome>();
        let (ack_tx, ack_rx) = mpsc::unbounded_channel::<u64>();
        let sh = sh.clone();
        let task = tokio::spawn(async move {
            let o = read_loop(&sh, recv, ack_tx).await;
            let _ = net_tx.send(o);
        });
        Reader { task, net_rx, ack_rx }
    }
}

enum SessionTarget {
    New,
    Name(String),
}

fn pick_session(key: char, list: &[proto::SessionInfo], current: &str) -> Option<SessionTarget> {
    if key == 'c' {
        return Some(SessionTarget::New);
    }
    if list.is_empty() {
        return None;
    }
    let i = list.iter().position(|s| s.name == current).unwrap_or(0);
    let n = list.len();
    let j = match key {
        'n' => (i + 1) % n,
        'p' => (i + n - 1) % n,
        d @ '0'..='9' => d.to_digit(10)? as usize,
        _ => return None,
    };
    list.get(j).filter(|s| s.name != current).map(|s| SessionTarget::Name(s.name.clone()))
}

async fn rpc_list(conn: &quinn::Connection) -> Result<Vec<proto::SessionInfo>> {
    let (mut s, mut r) = conn.open_bi().await?;
    proto::write_msg(&mut s, &proto::Rpc::List).await?;
    let _ = s.finish();
    let rep: proto::Reply = tokio::time::timeout(Duration::from_secs(5), proto::read_msg(&mut r)).await??;
    if !rep.ok {
        bail!("{}", rep.error.unwrap_or_default());
    }
    Ok(rep.sessions)
}

/// Attach to another session on a new stream of the same connection. On
/// success, `hello` describes the new session, and the local screen and
/// scrollback are cleared: the new session brings its own.
async fn switch_session(
    sh: &Arc<Shared>,
    conn: &quinn::Connection,
    hello: &mut Hello,
    target: SessionTarget,
) -> Result<(quinn::SendStream, quinn::RecvStream)> {
    let mut h = hello.clone();
    h.client_id = None;
    h.token = String::new();
    h.offset = 0;
    match target {
        SessionTarget::New => {
            h.new = true;
            h.session.clear();
        }
        SessionTarget::Name(n) => {
            h.new = false;
            h.session = n;
        }
    }
    let (mut s, mut r) = conn.open_bi().await?;
    proto::write_msg(&mut s, &proto::Rpc::Attach { hello: h }).await?;
    let w: Welcome = tokio::time::timeout(Duration::from_secs(10), proto::read_msg(&mut r)).await??;
    if !w.ok {
        bail!("{}", w.error.unwrap_or_default());
    }
    hello.session = w.session;
    hello.client_id = Some(w.client_id);
    hello.token = w.resume_token;
    hello.new = false;
    sh.offset.store(0, Ordering::SeqCst);
    let _ = sh.out.send(Out::Data(b"\x1b[H\x1b[2J\x1b[3J".to_vec())).await;
    Ok((s, r))
}

struct ProbeTarget {
    addr: SocketAddr,
    fp: String,
}

impl ProbeTarget {
    async fn run(self) -> bool {
        let Ok(l) = Link::new(self.addr, &self.fp, Transport::Udp, None) else { return false };
        let ok = l.probe_direct(&self.fp).await;
        l.ep.close(0u32.into(), b"");
        ok
    }
}

async fn read_loop(sh: &Shared, mut recv: quinn::RecvStream, ack: mpsc::UnboundedSender<u64>) -> Outcome {
    loop {
        let (k, p) = match proto::read_frame(&mut recv).await {
            Ok(Some(f)) => f,
            Ok(None) => return Outcome::Lost("stream closed".into()),
            Err(e) => return Outcome::Lost(e.to_string()),
        };
        match k {
            kind::OUTPUT => {
                let Ok((off, data)) = proto::split_offset(&p) else { continue };
                let cur = sh.offset.load(Ordering::SeqCst);
                let end = off + data.len() as u64;
                if end <= cur {
                    continue;
                }
                let skip = cur.saturating_sub(off) as usize;
                if sh.out.send(Out::Data(data[skip..].to_vec())).await.is_err() {
                    return Outcome::Quit;
                }
                sh.offset.store(end, Ordering::SeqCst);
                let _ = ack.send(end);
            }
            kind::SNAPSHOT => {
                let Ok((off, data)) = proto::split_offset(&p) else { continue };
                if sh.out.send(Out::Data(data.to_vec())).await.is_err() {
                    return Outcome::Quit;
                }
                sh.offset.store(off, Ordering::SeqCst);
                let _ = ack.send(off);
            }
            kind::CONTROL => match serde_json::from_slice::<Control>(&p) {
                Ok(Control::Exit { code }) => return Outcome::Exit(code),
                Ok(Control::Detached { reason }) => return Outcome::Detached(reason),
                Ok(Control::Notice { text }) => {
                    let _ = sh.out.send(Out::Message(text)).await;
                }
                _ => {}
            },
            _ => {}
        }
    }
}

async fn reconnect(
    sh: &Arc<Shared>,
    link: &mut Link,
    ev_rx: &mut mpsc::UnboundedReceiver<Event>,
    hello: &mut Hello,
    pending: &mut Vec<u8>,
) -> Result<Option<Streams>> {
    let start = Instant::now();
    let mut attempt: u32 = 0;
    let mut fresh_token: Option<String> = None;
    let prefer_tunnel = link.kind == PathKind::Tunnel;
    loop {
        attempt += 1;
        let _ = sh.out.try_send(Out::Status(Some(format!("[qsh: reconnecting, {}s]", start.elapsed().as_secs()))));
        let use_tunnel = match link.transport {
            Transport::Udp => false,
            Transport::Ssh => true,
            Transport::Auto => attempt.is_multiple_of(2) != prefer_tunnel,
        };
        let mut h = hello.clone();
        h.offset = sh.offset.load(Ordering::SeqCst);
        if let Some(t) = &fresh_token {
            h.client_id = None;
            h.token = t.clone();
        }
        let res = {
            let fut = attempt_once(link, &h, use_tunnel);
            tokio::pin!(fut);
            loop {
                tokio::select! {
                    r = &mut fut => break Some(r),
                    ev = ev_rx.recv() => match ev {
                        Some(Event::Input(b)) => if pending.len() < 1 << 20 { pending.extend_from_slice(&b) },
                        Some(Event::Resize(sz)) => set_size(hello, sz),
                        Some(Event::Help) | Some(Event::Migrate(_)) | Some(Event::Key(_)) => {}
                        Some(Event::Quit) | Some(Event::Detach) | None => break None,
                    }
                }
            }
        };
        match res {
            None => return Ok(None),
            Some(Ok((conn, s, r, w))) => {
                crate::paths::log(format!("reconnected via {:?} after {:?}, resume at {}", link.kind, start.elapsed(), h.offset));
                if fresh_token.is_some() {
                    hello.client_id = Some(w.client_id);
                    hello.token = w.resume_token;
                }
                let _ = sh.out.send(Out::Status(None)).await;
                return Ok(Some((conn, s, r)));
            }
            Some(Err(Fail::Fatal(e))) => return Err(e),
            Some(Err(Fail::Unknown)) => {
                // The server forgot us: get a new token over SSH and attach again.
                let Some(spec) = link.spec.clone() else { bail!("the server forgot this client") };
                if let Ok(hp) = net::spawn_helper(&spec, false, true, true).await { fresh_token = hp.line.token.clone() }
                continue;
            }
            Some(Err(Fail::Retry(_))) => {}
        }
        let wait = Duration::from_millis((250 * attempt as u64).min(2000));
        let sleep = tokio::time::sleep(wait);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => break,
                ev = ev_rx.recv() => match ev {
                    Some(Event::Input(b)) => if pending.len() < 1 << 20 { pending.extend_from_slice(&b) },
                    Some(Event::Resize(sz)) => set_size(hello, sz),
                    Some(Event::Help) | Some(Event::Migrate(_)) | Some(Event::Key(_)) => {}
                    Some(Event::Quit) | Some(Event::Detach) | None => return Ok(None),
                }
            }
        }
    }
}

async fn attempt_once(
    link: &mut Link,
    h: &Hello,
    use_tunnel: bool,
) -> std::result::Result<(quinn::Connection, quinn::SendStream, quinn::RecvStream, Welcome), Fail> {
    if use_tunnel {
        let spec = link.spec.clone().ok_or_else(|| Fail::Retry(anyhow!("no ssh")))?;
        let helper = net::spawn_helper(&spec, true, false, true).await.map_err(Fail::Retry)?;
        link.use_tunnel(helper).map_err(Fail::Retry)?;
        let conn = link.connect(Duration::from_secs(10)).await.map_err(Fail::Retry)?;
        let (s, r, w) = handshake(&conn, h).await?;
        Ok((conn, s, r, w))
    } else {
        link.use_direct().map_err(Fail::Retry)?;
        let conn = link.connect(Duration::from_secs(3)).await.map_err(Fail::Retry)?;
        let (s, r, w) = handshake(&conn, h).await?;
        Ok((conn, s, r, w))
    }
}
