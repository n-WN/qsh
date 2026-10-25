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

const HELP: &str = "qsh: Ctrl-^ .  quit (session stays)   Ctrl-^ d  detach   Ctrl-^ Ctrl-^  send Ctrl-^";

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
    let (cols, rows) = term::size();
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
        cols,
        rows,
        term: term_name,
        terminfo,
        env,
        command: opts.command.clone(),
        server_addr: link.addr.to_string(),
    };
    let (send, recv, w) = match handshake(&conn, &hello).await {
        Ok(x) => x,
        Err(Fail::Retry(e)) | Err(Fail::Fatal(e)) => bail!("{e:#}"),
        Err(Fail::Unknown) => bail!("unknown client"),
    };
    hello.client_id = Some(w.client_id);
    hello.token = w.resume_token.clone();
    hello.terminfo = None;
    hello.command.clear();
    if w.readonly {
        eprintln!("qsh: read-only view of session {}", w.session);
    }

    let (out_tx, out_rx) = mpsc::channel::<Out>(64);
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
    tokio::spawn(async move {
        let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()) else { return };
        while s.recv().await.is_some() {
            let (c, r) = term::size();
            if ev_tx.send(Event::Resize(c, r)).is_err() {
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
            Outcome::Detached(r) => break Ok((0, Some(format!("[qsh: {r}; session {} stays on the server]", w.session)))),
            Outcome::Quit => break Ok((0, Some(format!("[qsh: quit; session {} stays on the server]", w.session)))),
            Outcome::Lost(why) => match {
                crate::paths::log(format!("connection lost: {why}"));
                reconnect(&sh, &mut link, &mut ev_rx, &mut hello, &mut pending).await
            } {
                Ok(Some(s)) => streams = s,
                Ok(None) => break Ok((0, Some(format!("[qsh: quit; session {} stays on the server]", w.session)))),
                Err(e) => break Err(e),
            },
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
                eprintln!("{m}");
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
    let mut resize = Vec::with_capacity(4);
    resize.extend_from_slice(&hello.cols.to_be_bytes());
    resize.extend_from_slice(&hello.rows.to_be_bytes());
    if send.write_all(&proto::frame(kind::RESIZE, &resize)).await.is_err() {
        return Outcome::Lost("write".into());
    }
    if !pending.is_empty() {
        if send.write_all(&proto::frame(kind::INPUT, pending)).await.is_err() {
            return Outcome::Lost("write".into());
        }
        pending.clear();
    }

    let (net_tx, mut net_rx) = mpsc::unbounded_channel::<Outcome>();
    let sh2 = sh.clone();
    let reader = tokio::spawn(async move {
        let o = read_loop(&sh2, recv).await;
        let _ = net_tx.send(o);
    });
    let c2 = conn.clone();
    let sh3 = sh.clone();
    let acceptor = tokio::spawn(async move {
        while let Ok((s, r)) = c2.accept_bi().await {
            tokio::spawn(rpc::serve(sh3.clone(), s, r));
        }
    });
    let (probe_tx, mut probe_rx) = mpsc::unbounded_channel::<bool>();

    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut last_rx_count = conn.stats().udp_rx.datagrams;
    let mut last_rx = Instant::now();
    let mut rebound = false;
    let mut shown: Option<u64> = None;
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
                    Event::Resize(c, r) => {
                        hello.cols = c;
                        hello.rows = r;
                        let mut p = Vec::with_capacity(4);
                        p.extend_from_slice(&c.to_be_bytes());
                        p.extend_from_slice(&r.to_be_bytes());
                        if send.write_all(&proto::frame(kind::RESIZE, &p)).await.is_err() {
                            break Outcome::Lost("write".into());
                        }
                    }
                    Event::Detach => {
                        let _ = send.write_all(&proto::control_frame(&Control::Detach)).await;
                        match tokio::time::timeout(Duration::from_secs(2), net_rx.recv()).await {
                            Ok(Some(o @ Outcome::Detached(_))) => break o,
                            _ => break Outcome::Detached("detached".into()),
                        }
                    }
                    Event::Quit => break Outcome::Quit,
                    Event::Help => {
                        let _ = sh.out.send(Out::Message(HELP.into())).await;
                    }
                }
            }
            o = net_rx.recv() => break o.unwrap_or(Outcome::Lost("reader".into())),
            Some(ok) = probe_rx.recv() => {
                probing = false;
                // UDP works again: move this connection off the SSH tunnel.
                if ok && link.kind == PathKind::Tunnel && link.use_direct().is_ok() {
                    rebound = false;
                }
            }
            _ = tick.tick() => {
                let n = conn.stats().udp_rx.datagrams;
                if n != last_rx_count {
                    last_rx_count = n;
                    last_rx = Instant::now();
                }
                let idle = last_rx.elapsed();
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
    reader.abort();
    acceptor.abort();
    if shown.is_some() {
        let _ = sh.out.try_send(Out::Status(None));
    }
    conn.close(0u32.into(), b"");
    outcome
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

async fn read_loop(sh: &Shared, mut recv: quinn::RecvStream) -> Outcome {
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
            }
            kind::SNAPSHOT => {
                let Ok((off, data)) = proto::split_offset(&p) else { continue };
                if sh.out.send(Out::Data(data.to_vec())).await.is_err() {
                    return Outcome::Quit;
                }
                sh.offset.store(off, Ordering::SeqCst);
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
            Transport::Auto => (attempt % 2 == 0) != prefer_tunnel,
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
                        Some(Event::Resize(c, r)) => { hello.cols = c; hello.rows = r; }
                        Some(Event::Help) => {}
                        Some(Event::Quit) | Some(Event::Detach) | None => break None,
                    }
                }
            }
        };
        match res {
            None => return Ok(None),
            Some(Ok((conn, s, r, w))) => {
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
                match net::spawn_helper(&spec, false, true, true).await {
                    Ok(hp) => fresh_token = hp.line.token.clone(),
                    Err(_) => {}
                }
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
                    Some(Event::Resize(c, r)) => { hello.cols = c; hello.rows = r; }
                    Some(Event::Help) => {}
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
