//! The per-user daemon: one UDP port (QUIC), many sessions, many clients.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    net::SocketAddr,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
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
    copies,
    files, paths,
    proto::{self, Control, Hello, LocalReply, LocalReq, Reply, Rpc, SessionInfo, Welcome, kind},
    tls,
    version::Build,
    vt::Prep,
};

/// Time that a program has to echo input before the input is acknowledged.
const ECHO_WAIT: Duration = Duration::from_millis(50);
/// QUIC close code: the daemon restarts with a new program; connect again now.
pub const CLOSE_UPGRADE: u32 = 2;
/// Close code: the device of this client was revoked.
pub const CLOSE_REVOKED: u32 = 3;
/// Format of the state that a daemon gives to its successor.
pub const STATE_VERSION: u32 = 1;

/// A client further behind than this gets a screen snapshot instead of the backlog.
const LAG_LIMIT: u64 = 4 << 20;
/// After an interrupt, a backlog larger than this is replaced by the screen.
const SKIP_MIN: u64 = 128 << 10;
/// Time for the program to stop after an interrupt (the kernel then also
/// drops the pty's output queue).
const SKIP_WAIT: Duration = Duration::from_millis(40);
const CHUNK: usize = 64 * 1024;
const BOOTSTRAP_TTL: Duration = Duration::from_secs(120);
const STALE_CLIENT: Duration = Duration::from_secs(7 * 24 * 3600);

#[derive(serde::Serialize, serde::Deserialize, Clone)]
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
    /// Sockets and the lock that a new daemon program takes over.
    udp_fd: RawFd,
    unix_fd: RawFd,
    lock_fd: RawFd,
    /// An upgrade is in progress: new connections wait for the new program.
    upgrading: AtomicBool,
    devices: Mutex<super::devices::Devices>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    grants: Mutex<HashMap<String, GrantEntry>>,
    next_id: AtomicU64,
    conns: AtomicUsize,
    last_active: Mutex<Instant>,
}

enum Start {
    Offset(u64),
    /// history: lines of scrollback to send (see Mirror::snapshot).
    Snapshot { history: Option<usize>, prep: Prep },
}

/// State of one QUIC connection.
struct ConnCtx {
    conn: quinn::Connection,
    /// Set after a successful read-write attach: later streams of this
    /// connection can attach to (or make) other sessions without a token.
    trusted: AtomicBool,
    /// Set when the connection logged in with a one-time token from the SSH
    /// helper. Only such a connection can register a device: a device token
    /// cannot make more device tokens.
    ssh_login: AtomicBool,
    /// The device token (its hash) that this connection logged in with.
    device: Mutex<Option<String>>,
    /// The session that the client shows now (for `qsh cp` paths).
    current: Mutex<Option<Arc<Session>>>,
    /// The terminal stream now, for input copies in datagrams (`copies`).
    term: Mutex<Option<TermRef>>,
}

#[derive(Clone)]
struct TermRef {
    sid: u64,
    sess: Arc<Session>,
    id: u64,
    epoch: u64,
}

struct Attached {
    session: Arc<Session>,
    id: u64,
    epoch: u64,
    owner: bool,
    /// Logged in with a one-time token from the SSH helper.
    ssh_login: bool,
    device: Option<String>,
    start: Start,
    welcome: Welcome,
}

pub async fn run(port: Option<u16>, idle_exit: Duration, resume: Option<PathBuf>) -> Result<()> {
    let run_dir = paths::run_dir()?;
    let id = tls::load_or_create(&paths::base())?;
    let saved = match &resume {
        Some(p) => Some(load_state(p)?),
        None => None,
    };
    let (lock_fd, sock, port, listener) = match &saved {
        Some((st, _)) => {
            // Taken over from the previous daemon program (same process).
            let sock = unsafe { std::net::UdpSocket::from_raw_fd(st.udp_fd) };
            let l = unsafe { std::os::unix::net::UnixListener::from_raw_fd(st.unix_fd) };
            l.set_nonblocking(true)?;
            for fd in [st.lock_fd, st.udp_fd, st.unix_fd] {
                set_cloexec(fd);
            }
            (st.lock_fd, sock, st.port, UnixListener::from_std(l)?)
        }
        None => {
            let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(run_dir.join("daemon.lock"))?;
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                bail!("another qsh daemon is running");
            }
            let (sock, port) = bind_udp(port)?;
            let sock_path = paths::daemon_sock()?;
            let _ = std::fs::remove_file(&sock_path);
            let listener = UnixListener::bind(&sock_path).context("bind daemon socket")?;
            // The lock lives as long as the process (and its successors).
            let fd = std::os::fd::IntoRawFd::into_raw_fd(lock);
            (fd, sock, port, listener)
        }
    };
    let udp_fd = sock.as_raw_fd();
    let unix_fd = listener.as_raw_fd();
    let endpoint = quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(tls::server_config(&id)?),
        sock,
        Arc::new(quinn::TokioRuntime),
    )?;
    let sock_path = paths::daemon_sock()?;
    if let Err(e) = install_shims() {
        eprintln!("qsh daemon: cannot install shims: {e:#}");
    }
    paths::write_private(
        &run_dir.join("daemon.json"),
        serde_json::json!({"port": port, "fp": id.fp, "pid": std::process::id(), "build": Build::current().to_string()})
            .to_string()
            .as_bytes(),
    )?;

    let d = Arc::new(Daemon {
        port,
        fp: id.fp.clone(),
        udp_fd,
        unix_fd,
        lock_fd,
        upgrading: AtomicBool::new(false),
        devices: Mutex::new(super::devices::Devices::load()),
        sessions: Mutex::new(HashMap::new()),
        grants: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        conns: AtomicUsize::new(0),
        last_active: Mutex::new(Instant::now()),
    });
    match saved {
        Some((st, blobs)) => {
            let n = d.restore(st, &blobs);
            eprintln!("qsh daemon: pid {} build {} took over {n} session(s)", std::process::id(), crate::version::describe());
        }
        None => eprintln!("qsh daemon: pid {} build {} udp port {port} fp {}", std::process::id(), crate::version::describe(), id.fp),
    }

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

    // Dropped files are for the moment: remove them after a week.
    tokio::task::spawn_blocking(|| clean_drops(Duration::from_secs(7 * 24 * 3600)));

    let di = d.clone();
    let sp = sock_path.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
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
        if d.upgrading.load(Ordering::SeqCst) {
            // No answer: the client sends its first packet again, and the
            // new program (same UDP socket) answers it.
            inc.ignore();
            continue;
        }
        let d = d.clone();
        tokio::spawn(async move {
            if let Err(e) = d.clone().handle_conn(inc).await {
                eprintln!("qsh daemon: connection: {e:#}");
            }
        });
    }
    Ok(())
}

fn set_cloexec(fd: RawFd) {
    unsafe {
        let f = libc::fcntl(fd, libc::F_GETFD);
        libc::fcntl(fd, libc::F_SETFD, f | libc::FD_CLOEXEC);
    }
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

fn clean_drops(age: Duration) {
    let dir = crate::paths::home().join(".qsh/drop");
    let Ok(rd) = std::fs::read_dir(&dir) else { return };
    for e in rd.flatten() {
        let old = e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().unwrap_or_default() > age);
        if old {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
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
            // The client that set the size is gone: a connected client takes over.
            let gone = st.active.and_then(|a| st.clients.get(&a)).is_some_and(|c| c.gone_since.is_some_and(|t| t.elapsed() > Duration::from_secs(3)));
            if gone
                && let Some(next) = st.clients.iter().filter(|(_, c)| c.conn.is_some() && !c.readonly).map(|(k, _)| *k).max()
            {
                s.activate(&mut st, next);
            }
        }
    }

    async fn handle_conn(self: Arc<Self>, inc: quinn::Incoming) -> Result<()> {
        let conn = inc.await?;
        self.conns.fetch_add(1, Ordering::SeqCst);
        let _guard = ConnGuard(self.clone());
        let ctx = Arc::new(ConnCtx { conn: conn.clone(), trusted: AtomicBool::new(false), ssh_login: AtomicBool::new(false), device: Mutex::new(None), current: Mutex::new(None), term: Mutex::new(None) });
        let (send, mut recv) = tokio::time::timeout(Duration::from_secs(15), conn.accept_bi()).await??;
        let hello: Hello = tokio::time::timeout(Duration::from_secs(15), proto::read_msg(&mut recv)).await??;

        // Datagrams: copies of input, and pings to echo (the client uses
        // them to check the path end to end).
        let (c3, x3) = (conn.clone(), ctx.clone());
        tokio::spawn(async move {
            while let Ok(d) = c3.read_datagram().await {
                match copies::decode(&d) {
                    Some((copies::INPUT, sid, end, data)) => {
                        let t = x3.term.lock().unwrap().clone();
                        if let Some(t) = t.filter(|t| t.sid == sid)
                            && let Some(start) = end.checked_sub(data.len() as u64)
                        {
                            take_input(&t.sess, t.id, t.epoch, start, data);
                        }
                    }
                    _ => {
                        let _ = c3.send_datagram(d);
                    }
                }
            }
        });

        // Other streams: requests, and terminal streams after a session switch.
        let (d, c) = (self.clone(), ctx.clone());
        tokio::spawn(async move {
            while let Ok((s, r)) = c.conn.accept_bi().await {
                let (d, c) = (d.clone(), c.clone());
                tokio::spawn(async move {
                    if let Err(e) = d.serve_stream(c, s, r).await {
                        eprintln!("qsh daemon: stream: {e:#}");
                    }
                });
            }
        });

        self.terminal(&ctx, send, recv, hello).await;
        conn.closed().await;
        Ok(())
    }

    /// Run one terminal stream: attach, then send output and take input.
    async fn terminal(self: &Arc<Self>, ctx: &Arc<ConnCtx>, mut send: quinn::SendStream, mut recv: quinn::RecvStream, hello: Hello) {
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(16);
        let conn_device = ctx.device.lock().unwrap().clone();
        let att = match self.attach(&ctx.conn, &hello, ctx.trusted.load(Ordering::SeqCst), conn_device, tx.clone()) {
            Ok(a) => a,
            Err(e) => {
                let w = Welcome { ok: false, error: Some(format!("{e:#}")), ..Default::default() };
                let _ = proto::write_msg(&mut send, &w).await;
                let _ = send.finish();
                return;
            }
        };
        if att.owner {
            ctx.trusted.store(true, Ordering::SeqCst);
        }
        if att.ssh_login {
            ctx.ssh_login.store(true, Ordering::SeqCst);
        }
        if att.device.is_some() {
            *ctx.device.lock().unwrap() = att.device.clone();
        }
        *ctx.current.lock().unwrap() = Some(att.session.clone());
        let sid = u64::from(recv.id());
        *ctx.term.lock().unwrap() = Some(TermRef { sid, sess: att.session.clone(), id: att.id, epoch: att.epoch });
        // However this connection ends, the client is then gone (not a ghost
        // that looks connected and can hold the pty size).
        {
            let (sess, conn, id, epoch) = (att.session.clone(), ctx.conn.clone(), att.id, att.epoch);
            tokio::spawn(async move {
                conn.closed().await;
                let mut st = sess.st.lock().unwrap();
                if let Some(slot) = st.clients.get_mut(&id)
                    && slot.epoch == epoch
                    && slot.conn.is_some()
                {
                    slot.conn = None;
                    slot.ctl = None;
                    slot.gone_since = Some(Instant::now());
                }
                drop(st);
                sess.cv.notify_all();
            });
        }
        if proto::write_msg(&mut send, &att.welcome).await.is_err() {
            return;
        }

        let input_start = att.welcome.input_seen.unwrap_or(0);
        let (conn, sess, id, want) = (ctx.conn.clone(), att.session.clone(), att.id, hello.copies);
        let writer = tokio::spawn(async move {
            // Small output batches get copies in datagrams on a lossy path
            // (`copies`); `pos` is the place of a batch in this stream.
            let mut pos = 0u64;
            let mut watch = copies::LossWatch::new();
            let mut sched = copies::Schedule::new(8);
            loop {
                tokio::select! {
                    b = rx.recv() => {
                        let Some(b) = b else { break };
                        if want
                            && b.len() <= copies::MAX_DATA
                            && let Some(end) = copies::output_end(&b)
                            && watch.lossy(&conn)
                        {
                            // Only sparse output: with more in flight, QUIC
                            // finds a loss soon by itself.
                            let acked = sess.st.lock().unwrap().clients.get(&id).map_or(end, |c| c.acked);
                            if end.saturating_sub(acked) <= copies::SPARSE {
                                sched.add(copies::encode(copies::OUTPUT, sid, pos, &b), end, conn.rtt());
                            }
                        }
                        if send.write_all(&b).await.is_err() {
                            break;
                        }
                        pos += b.len() as u64;
                    }
                    _ = copies::sleep_until(sched.next()) => {
                        let acked = sess.st.lock().unwrap().clients.get(&id).map_or(u64::MAX, |c| c.acked);
                        sched.retain(|p| p.end > acked);
                        for d in sched.due(conn.rtt()) {
                            let _ = conn.send_datagram(d);
                        }
                    }
                }
            }
            let _ = send.finish();
        });

        let out = tokio::spawn(output_loop(self.clone(), att.session.clone(), ctx.conn.clone(), att.id, att.epoch, tx.clone(), att.start));
        let _ = input_loop(&att.session, att.id, att.epoch, &mut recv, &tx, input_start).await;
        {
            let mut st = att.session.st.lock().unwrap();
            if let Some(slot) = st.clients.get_mut(&att.id)
                && slot.epoch == att.epoch {
                    slot.conn = None;
                    slot.ctl = None;
                    slot.gone_since = Some(Instant::now());
                }
        }
        out.abort();
        att.session.cv.notify_all();
        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(3), writer).await;
    }

    async fn serve_stream(self: &Arc<Self>, ctx: Arc<ConnCtx>, mut s: quinn::SendStream, mut r: quinn::RecvStream) -> Result<()> {
        let rpc: Rpc = proto::read_msg(&mut r).await?;
        let current = ctx.current.lock().unwrap().clone();
        let cwd = current.map(|s| s.cwd()).unwrap_or_else(paths::home);
        match rpc {
            Rpc::Attach { hello } => {
                self.terminal(&ctx, s, r, hello).await;
                return Ok(());
            }
            Rpc::List => {
                proto::write_msg(&mut s, &Reply { ok: true, sessions: self.list(), ..Default::default() }).await?;
            }
            Rpc::Device { label } => {
                let rep = if ctx.ssh_login.load(Ordering::SeqCst) {
                    Reply { ok: true, token: Some(self.devices.lock().unwrap().add(&label)), ..Default::default() }
                } else {
                    Reply::err("only a login over SSH can register a device")
                };
                proto::write_msg(&mut s, &rep).await?;
            }
            Rpc::Push { dest, .. } => {
                let target = resolve(&cwd, &dest);
                // "dir/": a new folder (drag and drop). It goes away if the transfer fails.
                let created = dest.ends_with('/') && !target.exists() && tokio::fs::create_dir_all(&target).await.is_ok();
                let place = if target.is_dir() { files::Place::Into(target.clone()) } else { files::Place::As(target.clone()) };
                let rep = match files::read_archive(&mut r, place).await {
                    Ok(saved) => Reply { ok: true, saved: saved.iter().map(|p| p.display().to_string()).collect(), ..Default::default() },
                    Err(e) => {
                        if created {
                            let _ = tokio::fs::remove_dir_all(&target).await;
                        }
                        Reply::err(format!("{e:#}"))
                    }
                };
                proto::write_msg(&mut s, &rep).await?;
            }
            Rpc::Pull { paths, .. } => {
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

    fn list(&self) -> Vec<SessionInfo> {
        let mut sessions: Vec<SessionInfo> = self
            .sessions
            .lock()
            .unwrap()
            .values()
            .filter(|s| s.exit.borrow().is_none())
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
        sessions.sort_by(|a, b| (a.created, &a.name).cmp(&(b.created, &b.name)));
        sessions
    }

    /// End the clients of revoked devices: their slots go (so their resume
    /// tokens stop working), and their connections close (so no new stream
    /// can use the trust of the connection). A client of the real user then
    /// logs in again over SSH.
    fn end_device_clients(&self, gone: &[String]) {
        if gone.is_empty() {
            return;
        }
        let sessions: Vec<Arc<Session>> = self.sessions.lock().unwrap().values().cloned().collect();
        for s in sessions {
            let mut st = s.st.lock().unwrap();
            let ids: Vec<u64> = st.clients.iter().filter(|(_, c)| c.device.as_ref().is_some_and(|d| gone.contains(d))).map(|(id, _)| *id).collect();
            for id in ids {
                if let Some(c) = s.remove_client(&mut st, id)
                    && let Some(conn) = c.conn
                {
                    conn.close(CLOSE_REVOKED.into(), b"device revoked");
                }
            }
        }
    }

    fn attach(
        self: &Arc<Self>,
        conn: &quinn::Connection,
        h: &Hello,
        trusted: bool,
        conn_device: Option<String>,
        ctl: mpsc::Sender<Vec<u8>>,
    ) -> Result<Attached> {
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
            if let Some(old) = slot.conn.replace(conn.clone())
                && old.stable_id() != conn.stable_id() {
                    old.close(1u32.into(), b"superseded");
                }
            slot.ctl = Some(ctl);
            slot.size = [h.cols, h.rows, h.xpix, h.ypix];
            slot.gone_since = None;
            slot.acked = h.offset;
            slot.ack_at = Instant::now();
            slot.echo = None;
            let input_seen = slot.input_seen;
            if !h.server_addr.is_empty() {
                slot.server_addr = h.server_addr.clone();
            }
            let readonly = slot.readonly;
            let owner = slot.owner;
            let device = slot.device.clone();
            let token = slot.resume_token.clone();
            if st.active == Some(id) {
                sess.resize(&mut st, [h.cols, h.rows, h.xpix, h.ypix]);
            }
            drop(st);
            return Ok(Attached {
                session: sess,
                id,
                epoch,
                owner,
                ssh_login: false,
                device,
                start: Start::Offset(h.offset),
                welcome: Welcome {
                    ok: true,
                    client_id: id,
                    resume_token: token,
                    session: name,
                    resumed: true,
                    readonly,
                    input_seen: Some(input_seen),
                    build: Build::current().to_string(),
                    ..Default::default()
                },
            });
        }

        let mut ssh_login = false;
        let mut device = None;
        let grant = if h.token.is_empty() && trusted {
            device = conn_device;
            Grant::Bootstrap
        } else {
            let mut g = self.grants.lock().unwrap();
            g.retain(|_, e| e.expires > Instant::now());
            let found = g.iter().find(|(k, _)| ct_eq(k, &h.token)).map(|(k, _)| k.clone());
            match found {
                // A device token from an earlier login (fast start).
                None => match self.devices.lock().unwrap().check(&h.token) {
                    Some(hash) => {
                        device = Some(hash);
                        Grant::Bootstrap
                    }
                    None => bail!("invalid or expired token"),
                },
                Some(key) => match &g[&key].grant {
                    Grant::Bootstrap => {
                        g.remove(&key);
                        ssh_login = true;
                        Grant::Bootstrap
                    }
                    Grant::Share { session, readonly } => Grant::Share { session: session.clone(), readonly: *readonly },
                },
            }
        };
        let owner = matches!(grant, Grant::Bootstrap);
        let (readonly, may_create) = match &grant {
            Grant::Bootstrap => (false, true),
            Grant::Share { session, readonly } => {
                if *session != name {
                    bail!("this link is for session {session}");
                }
                (*readonly, false)
            }
        };
        let (sess, created, name) = {
            let mut ss = self.sessions.lock().unwrap();
            let name = if h.new && may_create {
                (1..).map(|i: u32| i.to_string()).find(|n| !ss.contains_key(n)).unwrap()
            } else {
                name
            };
            // A session whose program ended stays for 2 s, for the clients that
            // were in it (they get the exit code). A new client gets a new one.
            match ss.get(&name).filter(|s| s.exit.borrow().is_none()) {
                Some(s) => (s.clone(), false, name),
                None => {
                    if !may_create {
                        bail!("session {name} does not exist");
                    }
                    let s = self.create_session(&name, h)?;
                    ss.insert(name.clone(), s.clone());
                    (s, true, name)
                }
            }
        };
        if created {
            self.watch_exit(sess.clone());
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
                owner,
                device: device.clone(),
                resume_token: token.clone(),
                conn: Some(conn.clone()),
                ctl: Some(ctl),
                epoch: 1,
                gone_since: None,
                server_addr: h.server_addr.clone(),
                acked,
                ack_at: Instant::now(),
                input_seen: 0,
                echo: None,
                skip_at: None,
            },
        );
        let before = st.size;
        if !readonly {
            sess.activate(&mut st, id);
        }
        // A full-screen program redraws on a size change. With no size change,
        // make it redraw anyway.
        let nudge = !created && st.size == before && st.mirror.alt_screen();
        drop(st);
        if nudge {
            sess.nudge();
        }
        Ok(Attached {
            session: sess,
            id,
            epoch: 1,
            owner,
            ssh_login,
            device,
            start: if created {
                Start::Offset(0)
            } else {
                Start::Snapshot { history: Some(usize::MAX), prep: if h.clean { Prep::Blank } else { Prep::Scroll } }
            },
            welcome: Welcome {
                ok: true,
                client_id: id,
                resume_token: token,
                session: name,
                created,
                readonly,
                input_seen: Some(0),
                build: Build::current().to_string(),
                ..Default::default()
            },
        })
    }

    /// Forget a session 2 s after its program ends (clients get the exit code first).
    fn watch_exit(self: &Arc<Self>, s: Arc<Session>) {
        let d = self.clone();
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
                let rep = LocalReply { ok: true, port: self.port, fp: self.fp.clone(), token, build: Build::current().to_string(), ..Default::default() };
                proto::write_msg(&mut w, &rep).await?;
            }
            LocalReq::Info => {
                let rep = LocalReply { ok: true, port: self.port, fp: self.fp.clone(), build: Build::current().to_string(), ..Default::default() };
                proto::write_msg(&mut w, &rep).await?;
            }
            LocalReq::List => {
                let rep = LocalReply {
                    ok: true,
                    sessions: self.list(),
                    port: self.port,
                    fp: self.fp.clone(),
                    build: Build::current().to_string(),
                    ..Default::default()
                };
                proto::write_msg(&mut w, &rep).await?;
            }
            LocalReq::Devices { revoke } => {
                let (devices, gone) = {
                    let mut d = self.devices.lock().unwrap();
                    let gone = revoke.map_or_else(Vec::new, |p| d.revoke(&p));
                    let devices = d
                        .list()
                        .into_iter()
                        .map(|x| proto::DeviceInfo { id: x.hash[..12].to_string(), label: x.label, created: x.created, last_used: x.last_used })
                        .collect();
                    (devices, gone)
                };
                self.end_device_clients(&gone);
                proto::write_msg(&mut w, &LocalReply { ok: true, devices, removed: gone.len(), ..Default::default() }).await?;
            }
            LocalReq::Upgrade { exe } => {
                // Replies only on failure: on success, this process runs the new program.
                if let Err(e) = self.upgrade(&exe).await {
                    eprintln!("qsh daemon: upgrade: {e:#}");
                    proto::write_msg(&mut w, &LocalReply { ok: false, error: Some(format!("{e:#}")), ..Default::default() }).await?;
                }
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
                                if let Some(slot) = s.remove_client(&mut st, id)
                                    && let Some(ctl) = slot.ctl {
                                        let _ = ctl.try_send(proto::control_frame(&Control::Detached { reason: "detached by qsh detach".into() }));
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
/// Output that may wait unacknowledged: two congestion windows. The minimum
/// is small, because everything in it goes before a Ctrl-C takes effect:
/// 32 KB is 0.26 s on a 1 Mbit/s link (256 KB was 2 s).
fn window(conn: &quinn::Connection) -> u64 {
    let min = crate::ablate::value("winmin").unwrap_or(if crate::ablate::off("win32k") { 256 << 10 } else { 32 << 10 });
    (2 * conn.stats().path.cwnd).clamp(min, 4 << 20)
}

fn size_frame(s: (u16, u16, bool)) -> Vec<u8> {
    let mut p = Vec::with_capacity(5);
    p.extend_from_slice(&s.0.to_be_bytes());
    p.extend_from_slice(&s.1.to_be_bytes());
    p.push(if s.2 { proto::SIZE_MINE } else { 0 });
    proto::frame(kind::SIZE, &p)
}

fn input_ack_frame(n: u64, mode: u8) -> Vec<u8> {
    let mut p = n.to_be_bytes().to_vec();
    p.push(mode);
    proto::frame(kind::INPUT_ACK, &p)
}

async fn output_loop(d: Arc<Daemon>, sess: Arc<Session>, conn: quinn::Connection, id: u64, epoch: u64, tx: mpsc::Sender<Vec<u8>>, start: Start) {
    let mut head_rx = sess.head.subscribe();
    let mut ack_rx = sess.ack.subscribe();
    let mut exit_rx = sess.exit.subscribe();
    let (mut off, mut snap) = match start {
        Start::Offset(o) => (o, None),
        Start::Snapshot { history, prep } => (0, Some((history, prep))),
    };
    // What this client knows: the pty size (and if it sets it), the pty mode,
    // and the last input acknowledgement.
    let mut sent_size: Option<(u16, u16, bool)> = None;
    let mut sent_mode: Option<u8> = None;
    let mut last_ack = 0u64;
    loop {
        head_rx.borrow_and_update();
        ack_rx.borrow_and_update();
        let win = window(&conn);
        let next = {
            let mut st = sess.st.lock().unwrap();
            let (acked, echo, skip_at) = match st.clients.get(&id) {
                Some(c) if c.epoch == epoch => (c.acked, c.echo, c.skip_at),
                _ => return,
            };
            let head = st.ring.head();
            let size = (st.size[0], st.size[1], st.active == Some(id));
            let mut out = Vec::new();
            // After an interrupt, do not make the user wait for output that the
            // program wrote before it stopped: show the screen as it is now.
            let skip = skip_at.is_some_and(|t| Instant::now() >= t);
            if skip && let Some(c) = st.clients.get_mut(&id) {
                c.skip_at = None;
            }
            let snapshot = match snap.take() {
                Some(s) => Some(s),
                None if skip && head.saturating_sub(off) > SKIP_MIN => Some((None, Prep::Scroll)),
                None if off < st.ring.base() || off > head || head - off > LAG_LIMIT => Some((None, Prep::Scroll)),
                None => None,
            };
            if let Some((history, prep)) = snapshot {
                out.extend(size_frame(size));
                sent_size = Some(size);
                let b = st.mirror.snapshot(history, prep);
                off = head;
                out.extend(proto::frame_at(kind::SNAPSHOT, head, &b));
            } else {
                // A new size goes out at its place in the stream; a change of
                // who sets it goes out at once.
                let only_owner = sent_size.is_some_and(|s| (s.0, s.1) == (size.0, size.1));
                if sent_size != Some(size) && (off >= st.size_at || only_owner) {
                    out.extend(size_frame(size));
                    sent_size = Some(size);
                }
                let inflight = off.saturating_sub(acked);
                if off < head && inflight < win {
                    let mut n = (CHUNK as u64).min(win - inflight);
                    if sent_size != Some(size) && off < st.size_at {
                        n = n.min(st.size_at - off);
                    }
                    let d = st.ring.read(off, n as usize).unwrap_or_default();
                    out.extend(proto::frame_at(kind::OUTPUT, off, &d));
                    off += d.len() as u64;
                    if sent_size != Some(size) && off >= st.size_at {
                        out.extend(size_frame(size));
                        sent_size = Some(size);
                    }
                }
            }
            // The input acknowledgement follows the output that holds the echo.
            if let Some((n, h)) = echo
                && off >= h
            {
                out.extend(input_ack_frame(n, st.mode));
                last_ack = n;
                sent_mode = Some(st.mode);
                if let Some(c) = st.clients.get_mut(&id) {
                    c.echo = None;
                }
            } else if st.mode != 0 && sent_mode != Some(st.mode) {
                out.extend(input_ack_frame(last_ack, st.mode));
                sent_mode = Some(st.mode);
            }
            if !out.is_empty() {
                Next::Frame(out)
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
                // With the list, the client can move to another session at once.
                let owner = sess.st.lock().unwrap().clients.get(&id).is_some_and(|c| c.owner);
                let others = owner.then(|| d.list().into_iter().map(|s| s.name).collect());
                let _ = tx.send(proto::control_frame(&Control::Exit { code, others })).await;
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

/// Input from the stream or from a copy in a datagram; `start` is the input
/// count before `p`. Only the part that the session does not have yet goes to
/// the program. False if this client is gone.
fn take_input(sess: &Arc<Session>, id: u64, epoch: u64, start: u64, p: &[u8]) -> bool {
    let count = {
        let mut st = sess.st.lock().unwrap();
        let Some(slot) = st.clients.get_mut(&id) else { return false };
        if slot.epoch != epoch {
            return false;
        }
        let end = start + p.len() as u64;
        // Had already (a copy and the stream bring the same input), or after
        // a gap (the stream fills it first).
        if end <= slot.input_seen || start > slot.input_seen {
            return true;
        }
        let p = &p[(slot.input_seen - start) as usize..];
        slot.input_seen = end;
        let count = slot.input_seen;
        if !slot.readonly && session::has_interrupt(p) && !crate::ablate::off("skip") {
            slot.skip_at = Some(Instant::now() + SKIP_WAIT);
        }
        let ok = if slot.readonly {
            false
        } else if st.active != Some(id) {
            if session::looks_like_report(p) {
                false
            } else {
                sess.activate(&mut st, id);
                true
            }
        } else {
            true
        };
        // Under the lock: the stream and the datagrams take turns, and the
        // order of the input stays.
        if ok {
            sess.write_input(p.to_vec());
        }
        count
    };
    // Acknowledge the input when the program had time to echo it.
    let s = sess.clone();
    tokio::spawn(async move {
        tokio::time::sleep(ECHO_WAIT).await;
        s.update_mode();
        {
            let mut st = s.st.lock().unwrap();
            let head = st.ring.head();
            if let Some(c) = st.clients.get_mut(&id)
                && c.echo.is_none_or(|(n, _)| n < count)
            {
                c.echo = Some((count, head));
            }
        }
        s.ack.send_modify(|x| *x = x.wrapping_add(1));
    });
    true
}

/// `pos`: the input count at the start of this stream (the client sends
/// again what the server did not have).
async fn input_loop(sess: &Arc<Session>, id: u64, epoch: u64, recv: &mut quinn::RecvStream, tx: &mpsc::Sender<Vec<u8>>, mut pos: u64) -> Result<()> {
    while let Some((k, p)) = proto::read_frame(recv).await? {
        match k {
            kind::INPUT => {
                let start = pos;
                pos += p.len() as u64;
                if !take_input(sess, id, epoch, start, &p) {
                    return Ok(());
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
                    if let Some(slot) = st.clients.get_mut(&id)
                        && v > slot.acked {
                            slot.acked = v;
                            slot.ack_at = Instant::now();
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

#[derive(serde::Serialize, serde::Deserialize)]
struct SavedState {
    version: u32,
    port: u16,
    udp_fd: RawFd,
    unix_fd: RawFd,
    lock_fd: RawFd,
    next_id: u64,
    /// (token, grant, milliseconds left)
    grants: Vec<(String, Grant, u64)>,
    sessions: Vec<SavedSession>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SavedSession {
    name: String,
    command: String,
    pid: i32,
    created: u64,
    master_fd: RawFd,
    size: [u16; 4],
    active: Option<u64>,
    clients: Vec<SavedClient>,
    ring_base: u64,
    /// Blob lengths: the ring, then a snapshot of the mirror.
    ring_len: usize,
    snap_len: usize,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SavedClient {
    id: u64,
    size: [u16; 4],
    readonly: bool,
    owner: bool,
    #[serde(default)]
    device: Option<String>,
    resume_token: String,
    server_addr: String,
    acked: u64,
    input_seen: u64,
}

/// File layout: u64 length of the JSON header, the header, then the blobs.
fn load_state(p: &Path) -> Result<(SavedState, Vec<u8>)> {
    let data = std::fs::read(p).with_context(|| format!("read {}", p.display()))?;
    let _ = std::fs::remove_file(p);
    if data.len() < 8 {
        bail!("short state file");
    }
    let n = u64::from_be_bytes(data[..8].try_into().unwrap()) as usize;
    let st: SavedState = serde_json::from_slice(data.get(8..8 + n).ok_or_else(|| anyhow!("short state file"))?)?;
    if st.version != STATE_VERSION {
        bail!("state version {} is not {STATE_VERSION}", st.version);
    }
    Ok((st, data[8 + n..].to_vec()))
}

fn dup_fd(fd: RawFd, keep: &mut Vec<RawFd>) -> Result<RawFd> {
    // dup() gives a descriptor without close-on-exec: it survives exec.
    let n = unsafe { libc::dup(fd) };
    if n < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    keep.push(n);
    Ok(n)
}

impl Daemon {
    /// Replace this process with the program `exe` (same pid): the pty
    /// masters, the UDP socket, the local socket and the lock stay open, and
    /// the new program gets the sessions, rings, screens and clients. Clients
    /// see their connection close and connect again at once.
    async fn upgrade(self: &Arc<Self>, exe: &str) -> Result<()> {
        let exe = PathBuf::from(exe);
        let check = tokio::process::Command::new(&exe)
            .args(["daemon", "--check-upgrade", &STATE_VERSION.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let ok = tokio::time::timeout(Duration::from_secs(5), check).await.ok().and_then(|r| r.ok()).is_some_and(|s| s.success());
        if !ok {
            bail!("{} cannot take over this daemon", exe.display());
        }
        self.upgrading.store(true, Ordering::SeqCst);
        let sessions: Vec<Arc<Session>> = self.sessions.lock().unwrap().values().cloned().collect();
        let ss = sessions.clone();
        let quiet = tokio::task::spawn_blocking(move || ss.iter().all(|s| s.quiesce(Duration::from_secs(1)))).await?;
        let resume = || {
            sessions.iter().for_each(|s| s.resume_reading());
            self.upgrading.store(false, Ordering::SeqCst);
        };
        if !quiet {
            resume();
            bail!("the sessions did not stop");
        }
        // No more input: close the client connections first.
        for s in &sessions {
            for c in s.st.lock().unwrap().clients.values() {
                if let Some(conn) = &c.conn {
                    conn.close(CLOSE_UPGRADE.into(), b"upgrade");
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let ss = sessions.clone();
        tokio::task::spawn_blocking(move || ss.iter().all(|s| s.quiesce(Duration::from_millis(500)))).await?;

        let mut fds = Vec::new();
        let r = self.exec_with_state(&exe, &sessions, &mut fds);
        for fd in fds {
            unsafe { libc::close(fd) };
        }
        resume();
        r
    }

    fn exec_with_state(&self, exe: &Path, sessions: &[Arc<Session>], fds: &mut Vec<RawFd>) -> Result<()> {
        let mut blobs = Vec::new();
        let mut saved = Vec::new();
        for s in sessions {
            let mut st = s.st.lock().unwrap();
            let ring = st.ring.contents();
            let snap = st.mirror.snapshot(Some(usize::MAX), Prep::Blank);
            saved.push(SavedSession {
                name: s.name.clone(),
                command: s.command.clone(),
                pid: s.pid,
                created: s.created,
                master_fd: dup_fd(s.master_fd().as_raw_fd(), fds)?,
                size: st.size,
                active: st.active,
                clients: st
                    .clients
                    .iter()
                    .map(|(id, c)| SavedClient {
                        id: *id,
                        size: c.size,
                        readonly: c.readonly,
                        owner: c.owner,
                        device: c.device.clone(),
                        resume_token: c.resume_token.clone(),
                        server_addr: c.server_addr.clone(),
                        acked: c.acked,
                        input_seen: c.input_seen,
                    })
                    .collect(),
                ring_base: st.ring.base(),
                ring_len: ring.len(),
                snap_len: snap.len(),
            });
            blobs.extend_from_slice(&ring);
            blobs.extend_from_slice(&snap);
        }
        let now = Instant::now();
        let grants = self
            .grants
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, g)| g.expires > now)
            .map(|(k, g)| (k.clone(), g.grant.clone(), (g.expires - now).as_millis() as u64))
            .collect();
        let state = SavedState {
            version: STATE_VERSION,
            port: self.port,
            udp_fd: dup_fd(self.udp_fd, fds)?,
            unix_fd: dup_fd(self.unix_fd, fds)?,
            lock_fd: dup_fd(self.lock_fd, fds)?,
            next_id: self.next_id.load(Ordering::SeqCst),
            grants,
            sessions: saved,
        };
        let head = serde_json::to_vec(&state)?;
        let mut file = (head.len() as u64).to_be_bytes().to_vec();
        file.extend_from_slice(&head);
        file.extend_from_slice(&blobs);
        let path = paths::run_dir()?.join(format!("upgrade-{}.state", std::process::id()));
        paths::write_private(&path, &file)?;
        eprintln!("qsh daemon: upgrade to {}", exe.display());
        let err = Command::new(exe).arg("daemon").arg("--resume-from").arg(&path).exec();
        let _ = std::fs::remove_file(&path);
        Err(anyhow!("exec {}: {err}", exe.display()))
    }

    /// Take over the sessions of the previous daemon program.
    fn restore(self: &Arc<Self>, st: SavedState, blobs: &[u8]) -> usize {
        self.next_id.store(st.next_id, Ordering::SeqCst);
        let now = Instant::now();
        {
            let mut g = self.grants.lock().unwrap();
            for (k, grant, ms) in st.grants {
                g.insert(k, GrantEntry { grant, expires: now + Duration::from_millis(ms) });
            }
        }
        let mut pos = 0;
        let mut n = 0;
        for ss in st.sessions {
            let ring = &blobs[pos.min(blobs.len())..(pos + ss.ring_len).min(blobs.len())];
            pos += ss.ring_len;
            let snap = &blobs[pos.min(blobs.len())..(pos + ss.snap_len).min(blobs.len())];
            pos += ss.snap_len;
            set_cloexec(ss.master_fd);
            let master = unsafe { OwnedFd::from_raw_fd(ss.master_fd) };
            let mut mirror = crate::vt::Mirror::new(ss.size[0], ss.size[1], session::SCROLLBACK);
            mirror.feed(snap);
            let clients: BTreeMap<u64, ClientSlot> = ss
                .clients
                .into_iter()
                .map(|c| {
                    (
                        c.id,
                        ClientSlot {
                            size: c.size,
                            readonly: c.readonly,
                            owner: c.owner,
                            device: c.device,
                            resume_token: c.resume_token,
                            conn: None,
                            ctl: None,
                            epoch: 1,
                            gone_since: Some(now),
                            server_addr: c.server_addr,
                            acked: c.acked,
                            ack_at: now,
                            input_seen: c.input_seen,
                            echo: None,
                            skip_at: None,
                        },
                    )
                })
                .collect();
            let s = Session::launch(session::Parts {
                name: ss.name.clone(),
                command: ss.command,
                pid: ss.pid,
                created: ss.created,
                master,
                ring: session::Ring::restore(session::RING_CAP, ss.ring_base, ring),
                mirror,
                clients,
                active: ss.active,
                size: ss.size,
            });
            self.sessions.lock().unwrap().insert(ss.name, s.clone());
            self.watch_exit(s);
            n += 1;
        }
        n
    }
}

pub fn resolve(cwd: &Path, p: &str) -> PathBuf {
    if p.is_empty() || p == "." {
        return cwd.to_path_buf();
    }
    let e = paths::expand_tilde(p);
    if e.is_absolute() { e } else { cwd.join(e) }
}

fn hostname() -> String {
    nix::unistd::gethostname().map(|h| h.to_string_lossy().into_owned()).unwrap_or_else(|_| "localhost".into())
}

pub fn user_shell() -> String {
    if let Ok(s) = std::env::var("SHELL")
        && !s.is_empty() && Path::new(&s).exists() {
            return s;
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
            "LANG" | "LC_ALL" | "LC_CTYPE" | "LC_MESSAGES" | "LC_COLLATE" | "LC_NUMERIC" | "LC_TIME" | "LC_MONETARY"
                if locale_ok(&locales, v) => {
                    set_env(&mut env, k, v.clone());
                    have_lang |= k == "LANG" || k == "LC_ALL";
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
    if let Some(src) = terminfo
        && let Ok(dir) = paths::run_dir() {
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
