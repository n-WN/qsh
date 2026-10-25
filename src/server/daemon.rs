//! The per-user daemon: one UDP port (QUIC), many sessions, many clients.

use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use tokio::{
    io::AsyncWriteExt,
    net::{UnixListener, UnixStream},
    sync::mpsc,
};

use super::session::{self, ClientSlot, Session};
use crate::{
    files, paths,
    proto::{self, Control, Hello, LocalReply, LocalReq, Reply, Rpc, SessionInfo, Welcome, kind},
    tls,
};

/// A client further behind than this gets a screen snapshot instead of the backlog.
const LAG_LIMIT: u64 = 4 << 20;
const CHUNK: usize = 64 * 1024;
const BOOTSTRAP_TTL: Duration = Duration::from_secs(120);
const STALE_CLIENT: Duration = Duration::from_secs(7 * 24 * 3600);

enum Grant {
    Bootstrap,
    Share { session: String, readonly: bool },
}

struct GrantEntry {
    grant: Grant,
    expires: Instant,
}

pub struct Daemon {
    port: u16,
    fp: String,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    grants: Mutex<HashMap<String, GrantEntry>>,
    next_id: AtomicU64,
    conns: AtomicUsize,
    last_active: Mutex<Instant>,
}

enum Start {
    Offset(u64),
    Snapshot(bool),
}

struct Attached {
    session: Arc<Session>,
    id: u64,
    epoch: u64,
    start: Start,
    welcome: Welcome,
}

pub async fn run(port: Option<u16>, idle_exit: Duration) -> Result<()> {
    let run_dir = paths::run_dir()?;
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(run_dir.join("daemon.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("another qsh daemon is running");
    }
    let id = tls::load_or_create(&paths::base())?;
    let (sock, port) = bind_udp(port)?;
    let endpoint = quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(tls::server_config(&id)?),
        sock,
        Arc::new(quinn::TokioRuntime),
    )?;
    let sock_path = paths::daemon_sock()?;
    let _ = std::fs::remove_file(&sock_path);
    let listener = UnixListener::bind(&sock_path).context("bind daemon socket")?;
    if let Err(e) = install_shims() {
        eprintln!("qsh daemon: cannot install shims: {e:#}");
    }
    paths::write_private(
        &run_dir.join("daemon.json"),
        serde_json::json!({"port": port, "fp": id.fp, "pid": std::process::id()}).to_string().as_bytes(),
    )?;
    eprintln!("qsh daemon: pid {} udp port {port} fp {}", std::process::id(), id.fp);

    let d = Arc::new(Daemon {
        port,
        fp: id.fp.clone(),
        sessions: Mutex::new(HashMap::new()),
        grants: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        conns: AtomicUsize::new(0),
        last_active: Mutex::new(Instant::now()),
    });

    let dl = d.clone();
    tokio::spawn(async move {
        while let Ok((s, _)) = listener.accept().await {
            let d = dl.clone();
            tokio::spawn(async move {
                if let Err(e) = d.serve_local(s).await {
                    eprintln!("qsh daemon: local request: {e:#}");
                }
            });
        }
    });

    let di = d.clone();
    let sp = sock_path.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            di.sweep();
            let idle = di.sessions.lock().unwrap().is_empty()
                && di.conns.load(Ordering::SeqCst) == 0
                && di.last_active.lock().unwrap().elapsed() > idle_exit;
            if idle {
                let _ = std::fs::remove_file(&sp);
                eprintln!("qsh daemon: idle, exit");
                std::process::exit(0);
            }
        }
    });

    while let Some(inc) = endpoint.accept().await {
        let d = d.clone();
        tokio::spawn(async move {
            if let Err(e) = d.clone().handle_conn(inc).await {
                eprintln!("qsh daemon: connection: {e:#}");
            }
        });
    }
    Ok(())
}

fn bind_udp(pref: Option<u16>) -> Result<(std::net::UdpSocket, u16)> {
    let ports: Vec<u16> = match pref {
        Some(p) => vec![p],
        None => (60001..=60999).collect(),
    };
    for p in ports {
        if let Ok(s) = bind_one(p) {
            return Ok((s, p));
        }
    }
    bail!("no free UDP port")
}

fn bind_one(port: u16) -> std::io::Result<std::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    if let Ok(s) = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP)) {
        let _ = s.set_only_v6(false);
        let a: SocketAddr = (std::net::Ipv6Addr::UNSPECIFIED, port).into();
        if s.bind(&a.into()).is_ok() {
            return Ok(s.into());
        }
    }
    std::net::UdpSocket::bind(("0.0.0.0", port))
}

pub const SHIMS: &[&str] = &["xclip", "xsel", "wl-copy", "wl-paste", "pbcopy", "pbpaste", "xdg-open"];

fn install_shims() -> Result<()> {
    let dir = paths::shim_dir();
    paths::ensure_dir(&dir)?;
    let exe = std::env::current_exe()?.canonicalize()?;
    for name in std::iter::once(&"qsh").chain(SHIMS.iter()) {
        let p = dir.join(name);
        let _ = std::fs::remove_file(&p);
        std::os::unix::fs::symlink(&exe, &p)?;
    }
    Ok(())
}

struct ConnGuard(Arc<Daemon>);

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.conns.fetch_sub(1, Ordering::SeqCst);
        *self.0.last_active.lock().unwrap() = Instant::now();
    }
}

fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl Daemon {
    fn sweep(&self) {
        self.grants.lock().unwrap().retain(|_, g| g.expires > Instant::now());
        for s in self.sessions.lock().unwrap().values() {
            let mut st = s.st.lock().unwrap();
            let stale: Vec<u64> = st
                .clients
                .iter()
                .filter(|(_, c)| c.gone_since.is_some_and(|t| t.elapsed() > STALE_CLIENT))
                .map(|(k, _)| *k)
                .collect();
            for id in stale {
                s.remove_client(&mut st, id);
            }
        }
    }

    async fn handle_conn(self: Arc<Self>, inc: quinn::Incoming) -> Result<()> {
        let conn = inc.await?;
        self.conns.fetch_add(1, Ordering::SeqCst);
        let _guard = ConnGuard(self.clone());
        let (mut send, mut recv) = tokio::time::timeout(Duration::from_secs(15), conn.accept_bi()).await??;
        let hello: Hello = tokio::time::timeout(Duration::from_secs(15), proto::read_msg(&mut recv)).await??;
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(16);
        let att = match self.attach(&conn, &hello, tx.clone()) {
            Ok(a) => a,
            Err(e) => {
                let w = Welcome { ok: false, error: Some(format!("{e:#}")), ..Default::default() };
                proto::write_msg(&mut send, &w).await?;
                let _ = send.finish();
                let _ = tokio::time::timeout(Duration::from_secs(3), conn.closed()).await;
                return Ok(());
            }
        };
        proto::write_msg(&mut send, &att.welcome).await?;

        let writer = tokio::spawn(async move {
            while let Some(b) = rx.recv().await {
                if send.write_all(&b).await.is_err() {
                    break;
                }
            }
            let _ = send.finish();
        });

        // Echo datagrams: the client uses them to check the path end to end.
        let c3 = conn.clone();
        tokio::spawn(async move {
            while let Ok(d) = c3.read_datagram().await {
                let _ = c3.send_datagram(d);
            }
        });

        let sess = att.session.clone();
        let c2 = conn.clone();
        tokio::spawn(async move {
            while let Ok((s, r)) = c2.accept_bi().await {
                let sess = sess.clone();
                tokio::spawn(async move {
                    if let Err(e) = serve_rpc(sess, s, r).await {
                        eprintln!("qsh daemon: rpc: {e:#}");
                    }
                });
            }
        });

        let out = tokio::spawn(output_loop(att.session.clone(), conn.clone(), att.id, att.epoch, tx.clone(), att.start));
        let _ = input_loop(&att.session, att.id, att.epoch, &mut recv, &tx).await;
        {
            let mut st = att.session.st.lock().unwrap();
            if let Some(slot) = st.clients.get_mut(&att.id) {
                if slot.epoch == att.epoch {
                    slot.conn = None;
                    slot.ctl = None;
                    slot.gone_since = Some(Instant::now());
                }
            }
        }
        out.abort();
        att.session.cv.notify_all();
        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(3), writer).await;
        let _ = tokio::time::timeout(Duration::from_secs(3), conn.closed()).await;
        Ok(())
    }

    fn attach(self: &Arc<Self>, conn: &quinn::Connection, h: &Hello, ctl: mpsc::Sender<Vec<u8>>) -> Result<Attached> {
        if h.version != proto::VERSION {
            bail!("protocol version mismatch (client {}, server {})", h.version, proto::VERSION);
        }
        let name = if h.session.is_empty() { "main".to_string() } else { h.session.clone() };
        if !valid_name(&name) {
            bail!("bad session name {name:?}");
        }

        if let Some(id) = h.client_id {
            let sess = self.sessions.lock().unwrap().get(&name).cloned().ok_or_else(|| anyhow!("session ended"))?;
            let mut st = sess.st.lock().unwrap();
            let slot = st.clients.get_mut(&id).ok_or_else(|| anyhow!("unknown client"))?;
            if !ct_eq(&slot.resume_token, &h.token) {
                bail!("bad resume token");
            }
            slot.epoch += 1;
            let epoch = slot.epoch;
            if let Some(old) = slot.conn.replace(conn.clone()) {
                if old.stable_id() != conn.stable_id() {
                    old.close(1u32.into(), b"superseded");
                }
            }
            slot.ctl = Some(ctl);
            slot.size = [h.cols, h.rows, h.xpix, h.ypix];
            slot.gone_since = None;
            slot.acked = h.offset;
            slot.ack_at = Instant::now();
            if !h.server_addr.is_empty() {
                slot.server_addr = h.server_addr.clone();
            }
            let readonly = slot.readonly;
            let token = slot.resume_token.clone();
            if st.active == Some(id) {
                sess.resize(&mut st, [h.cols, h.rows, h.xpix, h.ypix]);
            }
            drop(st);
            return Ok(Attached {
                session: sess,
                id,
                epoch,
                start: Start::Offset(h.offset),
                welcome: Welcome { ok: true, client_id: id, resume_token: token, session: name, resumed: true, readonly, ..Default::default() },
            });
        }

        let grant = {
            let mut g = self.grants.lock().unwrap();
            g.retain(|_, e| e.expires > Instant::now());
            let found = g.iter().find(|(k, _)| ct_eq(k, &h.token)).map(|(k, _)| k.clone());
            let Some(key) = found else { bail!("invalid or expired token") };
            match &g[&key].grant {
                Grant::Bootstrap => {
                    g.remove(&key);
                    Grant::Bootstrap
                }
                Grant::Share { session, readonly } => Grant::Share { session: session.clone(), readonly: *readonly },
            }
        };
        let (readonly, may_create) = match &grant {
            Grant::Bootstrap => (false, true),
            Grant::Share { session, readonly } => {
                if *session != name {
                    bail!("this link is for session {session}");
                }
                (*readonly, false)
            }
        };
        let (sess, created) = {
            let mut ss = self.sessions.lock().unwrap();
            match ss.get(&name) {
                Some(s) => (s.clone(), false),
                None => {
                    if !may_create {
                        bail!("session {name} does not exist");
                    }
                    let s = self.create_session(&name, h)?;
                    ss.insert(name.clone(), s.clone());
                    (s, true)
                }
            }
        };
        if created {
            let d = self.clone();
            let s = sess.clone();
            tokio::spawn(async move {
                let mut rx = s.exit.subscribe();
                while rx.borrow_and_update().is_none() {
                    if rx.changed().await.is_err() {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
                let mut ss = d.sessions.lock().unwrap();
                if ss.get(&s.name).is_some_and(|x| Arc::ptr_eq(x, &s)) {
                    ss.remove(&s.name);
                }
                *d.last_active.lock().unwrap() = Instant::now();
            });
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let token = paths::random_hex(32);
        let mut st = sess.st.lock().unwrap();
        let acked = if created { 0 } else { st.ring.head() };
        st.clients.insert(
            id,
            ClientSlot {
                size: [h.cols, h.rows, h.xpix, h.ypix],
                readonly,
                resume_token: token.clone(),
                conn: Some(conn.clone()),
                ctl: Some(ctl),
                epoch: 1,
                gone_since: None,
                server_addr: h.server_addr.clone(),
                acked,
                ack_at: Instant::now(),
            },
        );
        if !readonly {
            sess.activate(&mut st, id);
        }
        drop(st);
        Ok(Attached {
            session: sess,
            id,
            epoch: 1,
            start: if created { Start::Offset(0) } else { Start::Snapshot(true) },
            welcome: Welcome { ok: true, client_id: id, resume_token: token, session: name, created, readonly, ..Default::default() },
        })
    }

    fn create_session(&self, name: &str, h: &Hello) -> Result<Arc<Session>> {
        let shell = user_shell();
        let (argv, login) = if h.command.is_empty() {
            (vec![shell.clone()], true)
        } else {
            (vec![shell.clone(), "-c".to_string(), h.command.join(" ")], false)
        };
        let env = build_env(name, h, &shell)?;
        Session::start(name, &argv, login, &env, &paths::home(), [h.cols, h.rows, h.xpix, h.ypix])
    }

    /// The connection of the client that should serve a request from a tool.
    fn target(&self, session: &str) -> Result<quinn::Connection> {
        let sess = self.sessions.lock().unwrap().get(session).cloned().ok_or_else(|| anyhow!("no session {session:?}"))?;
        let st = sess.st.lock().unwrap();
        if let Some(c) = st.active.and_then(|id| st.clients.get(&id)).and_then(|c| c.conn.clone()) {
            return Ok(c);
        }
        st.clients
            .values()
            .filter(|c| !c.readonly)
            .find_map(|c| c.conn.clone())
            .ok_or_else(|| anyhow!("no client is connected to this session"))
    }

    async fn serve_local(self: &Arc<Self>, s: UnixStream) -> Result<()> {
        let (mut r, mut w) = s.into_split();
        let req: LocalReq = proto::read_msg(&mut r).await?;
        match req {
            LocalReq::Token => {
                let token = paths::random_hex(32);
                self.grants.lock().unwrap().insert(token.clone(), GrantEntry { grant: Grant::Bootstrap, expires: Instant::now() + BOOTSTRAP_TTL });
                let rep = LocalReply { ok: true, port: self.port, fp: self.fp.clone(), token, ..Default::default() };
                proto::write_msg(&mut w, &rep).await?;
            }
            LocalReq::Info => {
                let rep = LocalReply { ok: true, port: self.port, fp: self.fp.clone(), ..Default::default() };
                proto::write_msg(&mut w, &rep).await?;
            }
            LocalReq::List => {
                let mut sessions: Vec<SessionInfo> = self
                    .sessions
                    .lock()
                    .unwrap()
                    .values()
                    .map(|s| {
                        let st = s.st.lock().unwrap();
                        SessionInfo {
                            name: s.name.clone(),
                            pid: s.pid,
                            created: s.created,
                            command: s.command.clone(),
                            cols: st.size[0],
                            rows: st.size[1],
                            clients: st.clients.len(),
                            connected: st.clients.values().filter(|c| c.conn.is_some()).count(),
                        }
                    })
                    .collect();
                sessions.sort_by_key(|s| s.created);
                proto::write_msg(&mut w, &LocalReply { ok: true, sessions, port: self.port, fp: self.fp.clone(), ..Default::default() }).await?;
            }
            LocalReq::Rpc { session, rpc } => {
                let conn = match self.target(&session) {
                    Ok(c) => c,
                    Err(e) => return proto::write_msg(&mut w, &Reply::err(e)).await,
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
            }
            LocalReq::Detach { session } => {
                let rep = match self.sessions.lock().unwrap().get(&session).cloned() {
                    None => LocalReply { ok: false, error: Some("no such session".into()), ..Default::default() },
                    Some(s) => {
                        let mut st = s.st.lock().unwrap();
                        match st.active {
                            None => LocalReply { ok: false, error: Some("no active client".into()), ..Default::default() },
                            Some(id) => {
                                if let Some(slot) = s.remove_client(&mut st, id) {
                                    if let Some(ctl) = slot.ctl {
                                        let _ = ctl.try_send(proto::control_frame(&Control::Detached { reason: "detached by qsh detach".into() }));
                                    }
                                }
                                LocalReply { ok: true, ..Default::default() }
                            }
                        }
                    }
                };
                proto::write_msg(&mut w, &rep).await?;
            }
            LocalReq::Share { session, readonly, ttl } => {
                let addr = {
                    let ss = self.sessions.lock().unwrap();
                    ss.get(&session).map(|s| {
                        let st = s.st.lock().unwrap();
                        st.active
                            .and_then(|id| st.clients.get(&id))
                            .map(|c| c.server_addr.clone())
                            .filter(|a| !a.is_empty())
                            .unwrap_or_else(|| format!("{}:{}", hostname(), self.port))
                    })
                };
                let Some(addr) = addr else {
                    return proto::write_msg(&mut w, &LocalReply { ok: false, error: Some("no such session".into()), ..Default::default() }).await;
                };
                let token = paths::random_hex(24);
                let ttl = Duration::from_secs(ttl.max(60));
                self.grants.lock().unwrap().insert(
                    token.clone(),
                    GrantEntry { grant: Grant::Share { session: session.clone(), readonly }, expires: Instant::now() + ttl },
                );
                let ro = if readonly { "&ro=1" } else { "" };
                let url = format!("qsh://{addr}/{session}?fp={}{ro}#{token}", self.fp);
                proto::write_msg(&mut w, &LocalReply { ok: true, url, token, ..Default::default() }).await?;
            }
        }
        Ok(())
    }
}

enum Next {
    Frame(Vec<u8>),
    Wait,
    Exit(i32),
}

/// Flow window for one connection: about two congestion windows, so the
/// network stays busy, but little output waits in buffers.
fn window(conn: &quinn::Connection) -> u64 {
    (2 * conn.stats().path.cwnd).clamp(256 << 10, 4 << 20)
}

async fn output_loop(sess: Arc<Session>, conn: quinn::Connection, id: u64, epoch: u64, tx: mpsc::Sender<Vec<u8>>, start: Start) {
    let mut head_rx = sess.head.subscribe();
    let mut ack_rx = sess.ack.subscribe();
    let mut exit_rx = sess.exit.subscribe();
    let (mut off, mut snap) = match start {
        Start::Offset(o) => (o, None),
        Start::Snapshot(full) => (0, Some(full)),
    };
    loop {
        head_rx.borrow_and_update();
        ack_rx.borrow_and_update();
        let win = window(&conn);
        let next = {
            let mut st = sess.st.lock().unwrap();
            let acked = match st.clients.get(&id) {
                Some(c) if c.epoch == epoch => c.acked,
                _ => return,
            };
            let head = st.ring.head();
            if let Some(full) = snap.take() {
                let b = st.snapshot(full);
                off = head;
                Next::Frame(proto::frame_at(kind::SNAPSHOT, head, &b))
            } else if off < st.ring.base() || off > head || head - off > LAG_LIMIT {
                let b = st.snapshot(false);
                off = head;
                Next::Frame(proto::frame_at(kind::SNAPSHOT, head, &b))
            } else if off < head {
                let inflight = off.saturating_sub(acked);
                if inflight >= win {
                    Next::Wait
                } else {
                    let n = (CHUNK as u64).min(win - inflight) as usize;
                    let d = st.ring.read(off, n).unwrap_or_default();
                    let f = proto::frame_at(kind::OUTPUT, off, &d);
                    off += d.len() as u64;
                    Next::Frame(f)
                }
            } else if let Some(code) = *exit_rx.borrow_and_update() {
                Next::Exit(code)
            } else {
                Next::Wait
            }
        };
        match next {
            Next::Frame(f) => {
                if tx.send(f).await.is_err() {
                    return;
                }
            }
            Next::Exit(code) => {
                let _ = tx.send(proto::control_frame(&Control::Exit { code })).await;
                return;
            }
            Next::Wait => {
                tokio::select! {
                    r = head_rx.changed() => if r.is_err() { return },
                    r = ack_rx.changed() => if r.is_err() { return },
                    _ = exit_rx.changed() => {},
                    _ = tx.closed() => return,
                    // The congestion window can grow without an event.
                    _ = tokio::time::sleep(Duration::from_millis(200)) => {},
                }
            }
        }
    }
}

async fn input_loop(sess: &Arc<Session>, id: u64, epoch: u64, recv: &mut quinn::RecvStream, tx: &mpsc::Sender<Vec<u8>>) -> Result<()> {
    while let Some((k, p)) = proto::read_frame(recv).await? {
        match k {
            kind::INPUT => {
                let ok = {
                    let mut st = sess.st.lock().unwrap();
                    let Some(slot) = st.clients.get(&id) else { return Ok(()) };
                    if slot.epoch != epoch {
                        return Ok(());
                    }
                    if slot.readonly {
                        false
                    } else if st.active != Some(id) {
                        if session::looks_like_report(&p) {
                            false
                        } else {
                            sess.activate(&mut st, id);
                            true
                        }
                    } else {
                        true
                    }
                };
                if ok {
                    sess.write_input(p);
                }
            }
            kind::RESIZE if p.len() >= 4 => {
                let v = |i: usize| if p.len() >= i + 2 { u16::from_be_bytes([p[i], p[i + 1]]) } else { 0 };
                let size = [v(0), v(2), v(4), v(6)];
                let mut st = sess.st.lock().unwrap();
                if let Some(slot) = st.clients.get_mut(&id) {
                    slot.size = size;
                }
                if st.active == Some(id) {
                    sess.resize(&mut st, size);
                }
            }
            kind::ACK if p.len() >= 8 => {
                let v = u64::from_be_bytes(p[..8].try_into().unwrap());
                {
                    let mut st = sess.st.lock().unwrap();
                    if let Some(slot) = st.clients.get_mut(&id) {
                        if v > slot.acked {
                            slot.acked = v;
                            slot.ack_at = Instant::now();
                        }
                    }
                }
                sess.cv.notify_all();
                sess.ack.send_modify(|x| *x = x.wrapping_add(1));
            }
            kind::PING => {
                let _ = tx.try_send(proto::frame(kind::PONG, &p));
            }
            kind::CONTROL => {
                if let Ok(Control::Detach) = serde_json::from_slice(&p) {
                    {
                        let mut st = sess.st.lock().unwrap();
                        sess.remove_client(&mut st, id);
                    }
                    let _ = tx.send(proto::control_frame(&Control::Detached { reason: "detached".into() })).await;
                    return Ok(());
                }
            }
            _ => {}
        }
    }
    Ok(())
}

pub fn resolve(cwd: &Path, p: &str) -> PathBuf {
    if p.is_empty() || p == "." {
        return cwd.to_path_buf();
    }
    let e = paths::expand_tilde(p);
    if e.is_absolute() { e } else { cwd.join(e) }
}

/// Requests that a client sends to the server (from `qsh cp` on the client side).
async fn serve_rpc(sess: Arc<Session>, mut s: quinn::SendStream, mut r: quinn::RecvStream) -> Result<()> {
    let rpc: Rpc = proto::read_msg(&mut r).await?;
    match rpc {
        Rpc::Push { dest, .. } => {
            let target = resolve(&sess.cwd(), &dest);
            let place = if target.is_dir() { files::Place::Into(target) } else { files::Place::As(target) };
            let rep = match files::read_archive(&mut r, place).await {
                Ok(saved) => Reply { ok: true, saved: saved.iter().map(|p| p.display().to_string()).collect(), ..Default::default() },
                Err(e) => Reply::err(format!("{e:#}")),
            };
            proto::write_msg(&mut s, &rep).await?;
        }
        Rpc::Pull { paths, .. } => {
            let cwd = sess.cwd();
            let roots: Vec<PathBuf> = paths.iter().map(|p| resolve(&cwd, p)).collect();
            if let Some(missing) = roots.iter().find(|p| !p.exists()) {
                proto::write_msg(&mut s, &Reply::err(format!("{}: no such file", missing.display()))).await?;
            } else {
                proto::write_msg(&mut s, &Reply::ok()).await?;
                files::write_archive(&mut s, &roots).await?;
            }
        }
        _ => proto::write_msg(&mut s, &Reply::err("not supported by the server")).await?,
    }
    let _ = s.finish();
    let _ = tokio::time::timeout(Duration::from_secs(30), s.stopped()).await;
    Ok(())
}

fn hostname() -> String {
    nix::unistd::gethostname().map(|h| h.to_string_lossy().into_owned()).unwrap_or_else(|_| "localhost".into())
}

pub fn user_shell() -> String {
    if let Ok(s) = std::env::var("SHELL") {
        if !s.is_empty() && Path::new(&s).exists() {
            return s;
        }
    }
    nix::unistd::User::from_uid(nix::unistd::getuid())
        .ok()
        .flatten()
        .map(|u| u.shell.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/sh".to_string())
}

fn set_env(env: &mut Vec<(String, String)>, k: &str, v: impl Into<String>) {
    let v = v.into();
    if let Some(e) = env.iter_mut().find(|(ek, _)| ek == k) {
        e.1 = v;
    } else {
        env.push((k.to_string(), v));
    }
}

fn build_env(name: &str, h: &Hello, shell: &str) -> Result<Vec<(String, String)>> {
    let mut env = Vec::new();
    for k in [
        "HOME", "USER", "LOGNAME", "PATH", "MAIL", "TMPDIR", "TZ", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME", "XDG_DATA_HOME",
        "XDG_CACHE_HOME", "XDG_STATE_HOME",
    ] {
        if let Ok(v) = std::env::var(k) {
            env.push((k.to_string(), v));
        }
    }
    let user = nix::unistd::User::from_uid(nix::unistd::getuid()).ok().flatten();
    if let Some(u) = &user {
        for (k, v) in [("USER", u.name.clone()), ("LOGNAME", u.name.clone()), ("HOME", u.dir.display().to_string())] {
            if !env.iter().any(|(ek, _)| ek == k) {
                env.push((k.to_string(), v));
            }
        }
    }
    set_env(&mut env, "SHELL", shell);
    if !env.iter().any(|(k, _)| k == "PATH") {
        env.push(("PATH".into(), "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into()));
    }
    set_env(&mut env, "TERM", resolve_term(&h.term, h.terminfo.as_deref()));

    let locales = available_locales();
    let mut have_lang = false;
    for (k, v) in &h.env {
        match k.as_str() {
            "COLORTERM" | "TERM_PROGRAM" | "TERM_PROGRAM_VERSION" => set_env(&mut env, k, v.clone()),
            "LANG" | "LC_ALL" | "LC_CTYPE" | "LC_MESSAGES" | "LC_COLLATE" | "LC_NUMERIC" | "LC_TIME" | "LC_MONETARY" => {
                if locale_ok(&locales, v) {
                    set_env(&mut env, k, v.clone());
                    have_lang |= k == "LANG" || k == "LC_ALL";
                }
            }
            _ => {}
        }
    }
    if !have_lang && locale_ok(&locales, "C.UTF-8") {
        set_env(&mut env, "LANG", "C.UTF-8");
    }

    let shims = paths::shim_dir();
    let path = env.iter().find(|(k, _)| k == "PATH").map(|(_, v)| v.clone()).unwrap_or_default();
    set_env(&mut env, "PATH", format!("{}:{path}", shims.display()));
    set_env(&mut env, "QSH", "1");
    set_env(&mut env, "QSH_SESSION", name);
    set_env(&mut env, "QSH_SOCK", paths::daemon_sock()?.display().to_string());
    set_env(&mut env, "QSH_BIN", shims.display().to_string());
    set_env(&mut env, "BROWSER", shims.join("xdg-open").display().to_string());
    // A login profile can reset PATH. For bash, put the shim folder back at the first prompt.
    set_env(
        &mut env,
        "PROMPT_COMMAND",
        r#"case ":$PATH:" in *":$QSH_BIN:"*) ;; *) PATH="$QSH_BIN:$PATH";; esac"#,
    );
    Ok(env)
}

fn has_terminfo(term: &str) -> bool {
    Command::new("infocmp").arg(term).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
}

/// Use the client's TERM. Compile the client's terminfo entry if the server lacks it.
fn resolve_term(term: &str, terminfo: Option<&str>) -> String {
    let fallback = "xterm-256color".to_string();
    if term.is_empty() || !term.chars().all(|c| c.is_ascii_alphanumeric() || "-._+".contains(c)) {
        return fallback;
    }
    if has_terminfo(term) {
        return term.to_string();
    }
    if let Some(src) = terminfo {
        if let Ok(dir) = paths::run_dir() {
            let f = dir.join(format!("terminfo-{term}.src"));
            if std::fs::write(&f, src).is_ok() {
                let out = paths::home().join(".terminfo");
                let ok = Command::new("tic")
                    .args(["-x", "-o"])
                    .arg(&out)
                    .arg(&f)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                let _ = std::fs::remove_file(&f);
                if ok && has_terminfo(term) {
                    return term.to_string();
                }
            }
        }
    }
    fallback
}

fn norm_locale(s: &str) -> String {
    s.to_lowercase().replace("utf-8", "utf8")
}

fn available_locales() -> Option<HashSet<String>> {
    let out = Command::new("locale").arg("-a").stderr(Stdio::null()).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).lines().map(norm_locale).collect())
}

fn locale_ok(set: &Option<HashSet<String>>, v: &str) -> bool {
    if v == "C" || v == "POSIX" {
        return true;
    }
    match set {
        Some(s) => s.contains(&norm_locale(v)),
        None => true,
    }
}
