//! Wire protocol: framing helpers and message types shared by client, daemon and tools.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const ALPN: &[u8] = b"qsh/1";
pub const VERSION: u32 = 1;
const MAX_MSG: usize = 64 << 20;

/// Frame kinds on the main (terminal) stream.
pub mod kind {
    // client -> server
    pub const INPUT: u8 = 1;
    pub const RESIZE: u8 = 2;
    pub const PING: u8 = 3;
    /// Output offset that the client gave to its terminal (u64).
    pub const ACK: u8 = 4;
    // server -> client
    pub const OUTPUT: u8 = 10;
    pub const SNAPSHOT: u8 = 11;
    pub const PONG: u8 = 12;
    // both directions, JSON payload
    pub const CONTROL: u8 = 20;
}

pub fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + payload.len());
    v.push(kind);
    v.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    v.extend_from_slice(payload);
    v
}

/// Frame with an u64 offset prefix (OUTPUT and SNAPSHOT).
pub fn frame_at(kind: u8, offset: u64, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(13 + payload.len());
    v.push(kind);
    v.extend_from_slice(&((payload.len() + 8) as u32).to_be_bytes());
    v.extend_from_slice(&offset.to_be_bytes());
    v.extend_from_slice(payload);
    v
}

pub fn control_frame(c: &Control) -> Vec<u8> {
    frame(kind::CONTROL, &serde_json::to_vec(c).unwrap())
}

/// Read one frame. Returns `None` on a clean end of stream.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<(u8, Vec<u8>)>> {
    let mut hdr = [0u8; 5];
    match r.read_exact(&mut hdr).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(hdr[1..5].try_into().unwrap()) as usize;
    if len > MAX_MSG {
        bail!("frame too large: {len}");
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(Some((hdr[0], buf)))
}

pub fn split_offset(p: &[u8]) -> Result<(u64, &[u8])> {
    if p.len() < 8 {
        bail!("short frame");
    }
    Ok((u64::from_be_bytes(p[..8].try_into().unwrap()), &p[8..]))
}

/// Length-prefixed JSON message.
pub async fn write_msg<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, m: &T) -> Result<()> {
    let body = serde_json::to_vec(m)?;
    let mut v = Vec::with_capacity(4 + body.len());
    v.extend_from_slice(&(body.len() as u32).to_be_bytes());
    v.extend_from_slice(&body);
    w.write_all(&v).await?;
    Ok(())
}

pub async fn read_msg<R: AsyncRead + Unpin, T: DeserializeOwned>(r: &mut R) -> Result<T> {
    let mut l = [0u8; 4];
    r.read_exact(&mut l).await.context("read message length")?;
    let len = u32::from_be_bytes(l) as usize;
    if len > MAX_MSG {
        bail!("message too large: {len}");
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(serde_json::from_slice(&buf)?)
}

/// First message of a connection, on the main stream.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Hello {
    pub version: u32,
    pub token: String,
    pub session: String,
    /// Set to resume an earlier client.
    pub client_id: Option<u64>,
    /// Bytes of output this client has already received.
    pub offset: u64,
    pub cols: u16,
    pub rows: u16,
    pub term: String,
    pub terminfo: Option<String>,
    pub env: Vec<(String, String)>,
    pub command: Vec<String>,
    /// The server address as the client sees it (used for share links).
    pub server_addr: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Welcome {
    pub ok: bool,
    pub error: Option<String>,
    pub client_id: u64,
    pub resume_token: String,
    pub session: String,
    pub created: bool,
    pub resumed: bool,
    pub readonly: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "t", rename_all = "kebab-case")]
pub enum Control {
    /// client -> server: forget this client and close.
    Detach,
    /// server -> client: the session ended.
    Exit { code: i32 },
    /// server -> client: this client was detached.
    Detached { reason: String },
    /// server -> client: show a short message.
    Notice { text: String },
}

/// Requests carried on their own bidirectional stream (in both directions).
/// After the header, the request body follows until end of stream. The reply is
/// a `Reply` header, followed by the reply body until end of stream.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Rpc {
    ClipList { primary: bool },
    ClipGet { mime: String, primary: bool },
    ClipSet { mime: String, primary: bool },
    /// Body: archive. To the client: save into the download folder (and open it).
    /// To the server: write into `dest` (relative to the shell directory).
    Push {
        dest: String,
        #[serde(default)]
        open: bool,
    },
    /// Reply body: archive. `picker`: let the user select files on the client.
    Pull { paths: Vec<String>, picker: bool },
    Open { url: String },
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Reply {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub mime: Option<String>,
    #[serde(default)]
    pub types: Vec<String>,
    #[serde(default)]
    pub saved: Vec<String>,
}

impl Reply {
    pub fn ok() -> Self {
        Reply { ok: true, ..Default::default() }
    }
    pub fn err(e: impl std::fmt::Display) -> Self {
        Reply { ok: false, error: Some(e.to_string()), ..Default::default() }
    }
}

/// Requests on the daemon's local unix socket (from `qsh helper` and remote tools).
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum LocalReq {
    Token,
    Info,
    List,
    Rpc { session: String, rpc: Rpc },
    Detach { session: String },
    Share { session: String, readonly: bool, ttl: u64 },
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct LocalReply {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub fp: String,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub sessions: Vec<SessionInfo>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct SessionInfo {
    pub name: String,
    pub pid: i32,
    pub created: u64,
    pub command: String,
    pub cols: u16,
    pub rows: u16,
    pub clients: usize,
    pub connected: usize,
}
