//! The client: bootstrap over SSH, connect over QUIC, and keep the session alive
//! across network changes, sleep and roaming.

pub mod clipboard;
pub mod drop;
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
use net::{Candidate, Link, PathKind, SshSpec, Transport};
use term::{Event, Out, Overlay};

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
    /// The session continues on a new connection.
    Moved(Streams),
}

enum Fail {
    Retry(anyhow::Error),
    Fatal(anyhow::Error),
    /// The server forgot this client (for example, after a long time away).
    Unknown,
}

type Streams = (quinn::Connection, quinn::SendStream, quinn::RecvStream);

const HELP: &[&str] = &[
    "qsh · keys after Ctrl-^",
    "  .     quit (the session stays)",
    "  d     detach",
    "  c     new session",
    "  n  p  next / previous session",
    "  0-9   select a session",
    "  w     list the sessions",
    "  i     connection info",
    "  C-^   send Ctrl-^",
];

/// What the overlay shows: a connection badge and a panel.
#[derive(Default)]
struct Ui {
    badge: Option<String>,
    panel: Vec<(String, bool)>,
    until: Option<Instant>,
    dismiss_on_input: bool,
    sent: Option<Option<Overlay>>,
}

impl Ui {
    fn badge(&mut self, b: Option<&str>) {
        self.badge = b.map(str::to_string);
    }

    fn panel(&mut self, lines: Vec<(String, bool)>, secs: u64, dismiss_on_input: bool) {
        self.panel = lines;
        self.until = Some(Instant::now() + Duration::from_secs(secs));
        self.dismiss_on_input = dismiss_on_input;
    }

    /// A short message. It goes away after `secs`, or when the user types.
    fn note(&mut self, text: impl Into<String>, warn: bool, secs: u64) {
        self.panel(vec![(text.into(), warn)], secs, true);
    }

    fn clear_panel(&mut self) {
        self.panel.clear();
        self.until = None;
    }

    fn on_input(&mut self) {
        if self.dismiss_on_input && !self.panel.is_empty() {
            self.clear_panel();
        }
    }

    fn tick(&mut self) {
        if self.until.is_some_and(|t| Instant::now() >= t) {
            self.clear_panel();
        }
    }

    fn overlay(&self) -> Option<Overlay> {
        let mut lines = Vec::new();
        if let Some(b) = &self.badge {
            lines.push((b.clone(), true));
        }
        lines.extend(self.panel.iter().cloned());
        (!lines.is_empty()).then_some(Overlay { lines })
    }

    /// Send the overlay to the output thread if it changed. When the channel is
    /// full (much output), the next tick tries again.
    fn push(&mut self, out: &mpsc::Sender<Out>) {
        let o = self.overlay();
        if self.sent.as_ref() != Some(&o) && out.try_send(Out::Overlay(o.clone())).is_ok() {
            self.sent = Some(o);
        }
    }
}

/// A drag-and-drop upload in progress. Input waits until it is done, so that
/// the order of keys stays correct.
struct Upload {
    task: tokio::task::JoinHandle<()>,
    original: Vec<u8>,
    queued: Vec<u8>,
    done: Arc<std::sync::atomic::AtomicU64>,
    total: u64,
    name: String,
}

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

    // Bootstrap: SSH gives the port, the certificate fingerprint and a one-time
    // token. Then QUIC over UDP and QUIC through the same SSH connection race.
    let (mut link, token, conn) = if let Some(spec) = &opts.ssh {
        let want_tunnel = opts.transport != Transport::Udp;
        let helper = net::spawn_helper(spec, want_tunnel, true, false).await?;
        let addr = net::resolve(spec, helper.line.port).await?;
        let fp = helper.line.fp.clone();
        let token = helper.line.token.clone().ok_or_else(|| anyhow!("no token from the server"))?;
        let mut link = Link::new(addr, &fp, opts.transport, Some(spec.clone()))?;
        let cand = net::race(link.race_spec(), want_tunnel.then_some(helper), Duration::ZERO).await?;
        let conn = link.adopt(cand);
        if link.kind == PathKind::Tunnel && opts.transport == Transport::Auto {
            eprintln!("qsh: no UDP path to {addr}; QUIC goes through the SSH connection (qsh moves to UDP when it works)");
        } else if opts.verbose {
            eprintln!("qsh: {}", link.path_name());
        }
        (link, token, conn)
    } else if let Some((addr, fp, token)) = &opts.direct {
        let link = Link::new(*addr, fp, Transport::Udp, None)?;
        let conn = link.connect(Duration::from_secs(8)).await.map_err(|e| anyhow!("cannot reach {addr}: {e:#}"))?;
        (link, token.clone(), conn)
    } else {
        bail!("no destination");
    };

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
        // The screen is made blank below, before any session output.
        clean: true,
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
    hello.clean = false;
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
    // Scroll only the lines with content into the scrollback, then start from
    // a blank screen: the output thread's mirror starts blank too.
    let (pos, typed) = term::query_cursor(Duration::from_millis(400));
    {
        use std::io::Write;
        let mut o = std::io::stdout().lock();
        let _ = o.write_all(&term::prepare_screen(size.rows, pos));
        let _ = o.flush();
    }
    let out_thread = term::spawn_stdout(out_rx, opts.osc52, size.cols, size.rows);
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

    let mut pending = typed;
    let mut ui = Ui::default();
    let mut streams: Streams = (conn, send, recv);
    let result = loop {
        let (conn, send, recv) = streams;
        *sh.conn.lock().unwrap() = Some(conn.clone());
        let o = drive(&sh, &mut link, conn, send, recv, &mut ev_rx, &mut hello, &mut pending, &mut ui).await;
        *sh.conn.lock().unwrap() = None;
        match o {
            Outcome::Moved(s) => streams = s,
            Outcome::Exit(code) => break Ok((code, None)),
            Outcome::Detached(r) => break Ok((0, Some(format!("[qsh: {r}; session {} stays on the server]", hello.session)))),
            Outcome::Quit => break Ok((0, Some(format!("[qsh: quit; session {} stays on the server]", hello.session)))),
            Outcome::Lost(why) => {
                crate::paths::log(format!("connection lost: {why}"));
                match reconnect(&sh, &mut link, &mut ev_rx, &mut hello, &mut pending, &mut ui).await {
                    Ok(Some(s)) => streams = s,
                    Ok(None) => break Ok((0, Some(format!("[qsh: quit; session {} stays on the server]", hello.session)))),
                    Err(e) => break Err(e),
                }
            }
        }
    };

    let _ = sh.out.send(Out::Overlay(None)).await;
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
            eprintln!("\r\nqsh: {e:#}");
            Ok(255)
        }
    }
}

fn info_lines(conn: &quinn::Connection, link: &Link, hello: &Hello) -> Vec<(String, bool)> {
    let st = conn.stats().path;
    let loss = if st.sent_packets > 0 { 100.0 * st.lost_packets as f64 / st.sent_packets as f64 } else { 0.0 };
    vec![
        (format!("qsh · session {}", hello.session), false),
        (format!("  path    {}", link.path_name()), false),
        (format!("  rtt     {} ms", conn.rtt().as_millis()), false),
        (format!("  loss    {loss:.1}% of {} packets", st.sent_packets), false),
        (format!("  mtu     {} bytes", st.current_mtu), false),
    ]
}

fn human(n: u64) -> String {
    rpc::human(n)
}

#[allow(clippy::too_many_arguments)]
async fn drive(
    sh: &Arc<Shared>,
    link: &mut Link,
    conn: quinn::Connection,
    mut send: quinn::SendStream,
    recv: quinn::RecvStream,
    ev_rx: &mut mpsc::UnboundedReceiver<Event>,
    hello: &mut Hello,
    pending: &mut Vec<u8>,
    ui: &mut Ui,
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
    let (probe_tx, mut probe_rx) = mpsc::unbounded_channel::<bool>();
    let (race_tx, mut race_rx) = mpsc::unbounded_channel::<Result<Candidate>>();
    let (up_tx, mut up_rx) = mpsc::unbounded_channel::<Result<Vec<String>>>();
    let mut upload: Option<Upload> = None;

    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut last_ping = Instant::now() - Duration::from_secs(10);
    let mut seq: u32 = 0;
    let mut rebound = false;
    let mut racing = false;
    let mut last_probe = Instant::now() - Duration::from_secs(25);
    let mut probing = false;

    let outcome = loop {
        tokio::select! {
            ev = ev_rx.recv() => {
                let Some(ev) = ev else { break Outcome::Quit };
                match ev {
                    Event::Input(b) => {
                        ui.on_input();
                        if let Some(u) = &mut upload {
                            if b.contains(&0x03) {
                                // Ctrl-C stops the upload; the keys go on.
                                u.task.abort();
                                let mut q = std::mem::take(&mut u.queued);
                                q.extend_from_slice(&b);
                                upload = None;
                                ui.note("qsh ⇪ upload stopped", true, 3);
                                if send.write_all(&proto::frame(kind::INPUT, &q)).await.is_err() {
                                    pending.extend_from_slice(&q);
                                    break Outcome::Lost("write".into());
                                }
                            } else {
                                u.queued.extend_from_slice(&b);
                            }
                        } else if send.write_all(&proto::frame(kind::INPUT, &b)).await.is_err() {
                            pending.extend_from_slice(&b);
                            break Outcome::Lost("write".into());
                        }
                    }
                    Event::Paste(p) => {
                        ui.on_input();
                        let files = if sh.policy.drop && upload.is_none() { drop::local_paths(&p) } else { None };
                        if let Some(files) = files {
                            let total = drop::total_size(&files);
                            let name = match files.len() {
                                1 => files[0].file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                                n => format!("{n} items"),
                            };
                            let done = Arc::new(std::sync::atomic::AtomicU64::new(0));
                            let (c, d, tx) = (conn.clone(), done.clone(), up_tx.clone());
                            let task = tokio::spawn(async move {
                                let _ = tx.send(drop::upload(c, files, d).await);
                            });
                            upload = Some(Upload { task, original: p, queued: Vec::new(), done, total, name });
                        } else {
                            let b = drop::wrap(&p);
                            if let Some(u) = &mut upload {
                                u.queued.extend_from_slice(&b);
                            } else if send.write_all(&proto::frame(kind::INPUT, &b)).await.is_err() {
                                pending.extend_from_slice(&b);
                                break Outcome::Lost("write".into());
                            }
                        }
                    }
                    Event::Resize(sz) => {
                        set_size(hello, sz);
                        let _ = sh.out.send(Out::Resize(sz.cols, sz.rows)).await;
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
                    Event::Help => ui.panel(HELP.iter().map(|l| (l.to_string(), false)).collect(), 15, true),
                    Event::Key('i') => ui.panel(info_lines(&conn, link, hello), 15, true),
                    Event::Key(k) => {
                        let list = match rpc_list(&conn).await {
                            Ok(l) => l,
                            Err(e) => {
                                ui.note(format!("qsh: {e:#}"), true, 4);
                                ui.push(&sh.out);
                                continue;
                            }
                        };
                        if k == 'w' {
                            let mut lines = vec![("qsh · sessions".to_string(), false)];
                            for (i, s) in list.iter().enumerate() {
                                let mark = if s.name == hello.session { "▸" } else { " " };
                                lines.push((format!(" {mark}{i}  {:<12} {}x{}  {} client(s)", s.name, s.cols, s.rows, s.connected), false));
                            }
                            ui.panel(lines, 8, true);
                            ui.push(&sh.out);
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
                                ui.note(format!("qsh → session {}", hello.session), false, 2);
                            }
                            Err(e) => ui.note(format!("qsh: {e:#}"), true, 4),
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
                ui.push(&sh.out);
            }
            o = reader.net_rx.recv() => {
                let o = o.unwrap_or(Outcome::Lost("reader".into()));
                if let Outcome::Exit(code) = o {
                    // The shell of this session ended. Show another session, if any.
                    let others = rpc_list(&conn).await.unwrap_or_default();
                    let ended = hello.session.clone();
                    if let Some(next) = others.iter().find(|s| s.name != ended)
                        && let Ok((s2, r2)) = switch_session(sh, &conn, hello, SessionTarget::Name(next.name.clone())).await
                    {
                        send = s2;
                        reader = Reader::spawn(sh, r2);
                        ui.note(format!("qsh · session {ended} ended; now in session {}", hello.session), false, 3);
                        ui.push(&sh.out);
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
            Some(r) = race_rx.recv() => {
                // A new connection is ready while the old one does not answer.
                let still_dead = last_echo.lock().unwrap().elapsed() > Duration::from_secs(1);
                match r {
                    Ok(cand) if still_dead => {
                        let mut h = hello.clone();
                        h.offset = sh.offset.load(Ordering::SeqCst);
                        match handshake(&cand.conn, &h).await {
                            Ok((s2, r2, _)) => {
                                crate::paths::log(format!("moved to a new connection ({:?}) at offset {}", cand.kind, h.offset));
                                let c2 = link.adopt(cand);
                                ui.badge(None);
                                ui.note(format!("qsh ✓ back · {}", link.path_name()), false, 2);
                                ui.push(&sh.out);
                                break Outcome::Moved((c2, s2, r2));
                            }
                            Err(Fail::Retry(_)) => {}
                            Err(_) => break Outcome::Lost("new connection refused".into()),
                        }
                    }
                    Ok(cand) => cand.conn.close(0u32.into(), b""),
                    Err(_) => {}
                }
            }
            Some(r) = up_rx.recv() => {
                if let Some(u) = upload.take() {
                    let mut b = match r {
                        Ok(remote) => {
                            ui.note(format!("qsh ⇪ {} → {}", u.name, remote.first().map(String::as_str).unwrap_or("")), false, 3);
                            drop::wrap(&drop::rewrite(&u.original, &remote))
                        }
                        Err(e) => {
                            ui.note(format!("qsh ⇪ upload failed: {e:#}"), true, 5);
                            drop::wrap(&u.original)
                        }
                    };
                    b.extend_from_slice(&u.queued);
                    ui.push(&sh.out);
                    if send.write_all(&proto::frame(kind::INPUT, &b)).await.is_err() {
                        pending.extend_from_slice(&b);
                        break Outcome::Lost("write".into());
                    }
                }
            }
            _ = tick.tick() => {
                ui.tick();
                if last_ping.elapsed() >= Duration::from_millis(1000) {
                    last_ping = Instant::now();
                    seq = seq.wrapping_add(1);
                    let mut d = vec![b'P'];
                    d.extend_from_slice(&seq.to_be_bytes());
                    let _ = conn.send_datagram(d.into());
                }
                if let Some(u) = &upload {
                    let done = u.done.load(Ordering::Relaxed).min(u.total);
                    let pct = (done * 100).checked_div(u.total).unwrap_or(100);
                    ui.panel(vec![(format!("qsh ⇪ {} · {pct}% of {} · Ctrl-C stops", u.name, human(u.total)), false)], 2, false);
                }
                let idle = last_echo.lock().unwrap().elapsed();
                if idle > Duration::from_millis(2500) {
                    ui.badge(Some("qsh ↻ no reply from the server"));
                } else if idle < Duration::from_secs(1) {
                    ui.badge(None);
                    rebound = false;
                    racing = false;
                }
                if idle > Duration::from_secs(3) && !rebound && link.kind == PathKind::Direct {
                    // Maybe the network changed: a new socket gives a new path.
                    rebound = true;
                    crate::paths::log("no echo for 3s: rebind to a new UDP socket");
                    let _ = link.use_direct();
                }
                if idle > Duration::from_secs(3) && !racing && link.spec.is_some() {
                    // At the same time, make a new connection. The first that works wins.
                    racing = true;
                    let (rs, tx) = (link.race_spec(), race_tx.clone());
                    tokio::spawn(async move { let _ = tx.send(net::race(rs, None, Duration::from_millis(300)).await); });
                }
                if idle > Duration::from_secs(10) {
                    break Outcome::Lost("timeout".into());
                }
                if link.tunnel_dead() {
                    break Outcome::Lost("tunnel closed".into());
                }
                if link.kind == PathKind::Tunnel && link.transport == Transport::Auto && !probing && last_probe.elapsed() > Duration::from_secs(30) {
                    probing = true;
                    last_probe = Instant::now();
                    let probe = ProbeTarget { addr: link.addr, fp: link.race_spec().fp };
                    let tx = probe_tx.clone();
                    tokio::spawn(async move { let _ = tx.send(probe.run().await); });
                }
                ui.push(&sh.out);
            }
        }
    };
    reader.task.abort();
    acceptor.abort();
    echo.abort();
    if let Some(u) = upload {
        u.task.abort();
        pending.extend_from_slice(&drop::wrap(&u.original));
        pending.extend_from_slice(&u.queued);
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
    // The screen is made blank below.
    h.clean = true;
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
    // Leave the alternate screen first: the snapshot starts on the main screen.
    let _ = sh.out.send(Out::Data(b"\x1b[?1049l\x1b[0m\x1b[H\x1b[2J\x1b[3J".to_vec())).await;
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
                Ok(Control::Notice { text }) => crate::paths::log(format!("notice: {text}")),
                _ => {}
            },
            _ => {}
        }
    }
}

/// Wait for an event while disconnected. Returns false when the user quits.
fn offline_event(ev: Option<Event>, sh: &Shared, hello: &mut Hello, pending: &mut Vec<u8>) -> bool {
    match ev {
        Some(Event::Input(b)) => {
            if pending.len() < 1 << 20 {
                pending.extend_from_slice(&b);
            }
        }
        Some(Event::Paste(p)) => pending.extend_from_slice(&drop::wrap(&p)),
        Some(Event::Resize(sz)) => {
            set_size(hello, sz);
            let _ = sh.out.try_send(Out::Resize(sz.cols, sz.rows));
        }
        Some(Event::Help) | Some(Event::Migrate(_)) | Some(Event::Key(_)) => {}
        Some(Event::Quit) | Some(Event::Detach) | None => return false,
    }
    true
}

async fn reconnect(
    sh: &Arc<Shared>,
    link: &mut Link,
    ev_rx: &mut mpsc::UnboundedReceiver<Event>,
    hello: &mut Hello,
    pending: &mut Vec<u8>,
    ui: &mut Ui,
) -> Result<Option<Streams>> {
    let start = Instant::now();
    let mut attempt: u32 = 0;
    let mut fresh_token: Option<String> = None;
    loop {
        attempt += 1;
        let secs = start.elapsed().as_secs() / 5 * 5;
        ui.badge(Some(&if secs == 0 { "qsh ↻ reconnecting".to_string() } else { format!("qsh ↻ reconnecting · {secs}s") }));
        ui.push(&sh.out);
        let mut h = hello.clone();
        h.offset = sh.offset.load(Ordering::SeqCst);
        h.clean = false;
        if let Some(t) = &fresh_token {
            h.client_id = None;
            h.token = t.clone();
        }
        let rs = link.race_spec();
        let res = {
            let fut = async {
                let cand = net::race(rs, None, Duration::from_millis(300)).await.map_err(Fail::Retry)?;
                let (s, r, w) = handshake(&cand.conn, &h).await?;
                Ok::<_, Fail>((cand, s, r, w))
            };
            tokio::pin!(fut);
            loop {
                tokio::select! {
                    r = &mut fut => break Some(r),
                    ev = ev_rx.recv() => if !offline_event(ev, sh, hello, pending) { break None },
                }
            }
        };
        match res {
            None => return Ok(None),
            Some(Ok((cand, s, r, w))) => {
                crate::paths::log(format!("reconnected via {:?} after {:?}, resume at {}", cand.kind, start.elapsed(), h.offset));
                if fresh_token.is_some() {
                    hello.client_id = Some(w.client_id);
                    hello.token = w.resume_token;
                }
                let conn = link.adopt(cand);
                ui.badge(None);
                ui.note(format!("qsh ✓ reconnected · {} · {:.1}s", link.path_name(), start.elapsed().as_secs_f32()), false, 3);
                ui.push(&sh.out);
                return Ok(Some((conn, s, r)));
            }
            Some(Err(Fail::Fatal(e))) => {
                ui.badge(None);
                return Err(e);
            }
            Some(Err(Fail::Unknown)) => {
                // The server forgot us: get a new token over SSH and attach again.
                let Some(spec) = link.spec.clone() else { bail!("the server forgot this client") };
                if let Ok(hp) = net::spawn_helper(&spec, false, true, true).await {
                    fresh_token = hp.line.token.clone();
                }
                continue;
            }
            Some(Err(Fail::Retry(_))) => {}
        }
        let sleep = tokio::time::sleep(Duration::from_millis((250 * attempt as u64).min(2000)));
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => break,
                ev = ev_rx.recv() => if !offline_event(ev, sh, hello, pending) { return Ok(None) },
            }
        }
    }
}
