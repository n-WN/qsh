//! Hosts that this client can reach without SSH (fast start). One file per
//! destination in ~/.qsh/known/ (mode 0600): the address and certificate
//! fingerprint of the daemon, a device token, the daemon's build and the
//! round-trip time.

use std::{net::SocketAddr, path::PathBuf};

use serde::{Deserialize, Serialize};

use super::net::SshSpec;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Known {
    pub addr: SocketAddr,
    pub fp: String,
    pub token: String,
    pub build: String,
    pub rtt_ms: u64,
    /// The last login went through the SSH tunnel (no UDP path then): SSH
    /// starts at once next time, next to the direct attempt.
    #[serde(default)]
    pub tunnel: bool,
}

/// The file for a destination: the ssh command and the port option are part
/// of the key, as they can lead to another account or daemon.
fn path(spec: &SshSpec) -> PathBuf {
    let key = format!("{}\0{}\0{:?}", spec.dest, spec.argv.join(" "), spec.port);
    let h = hex::encode(&ring::digest::digest(&ring::digest::SHA256, key.as_bytes()).as_ref()[..16]);
    crate::paths::base().join("known").join(format!("{h}.json"))
}

pub fn load(spec: &SshSpec) -> Option<Known> {
    serde_json::from_slice(&std::fs::read(path(spec)).ok()?).ok()
}

pub fn save(spec: &SshSpec, k: &Known) {
    if let Ok(b) = serde_json::to_vec(k) {
        let _ = crate::paths::write_private(&path(spec), &b);
    }
}

pub fn forget(spec: &SshSpec) {
    let _ = std::fs::remove_file(path(spec));
}
