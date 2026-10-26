//! Device credentials: after a login over SSH, a client gets a token with which
//! it can connect again without SSH (fast start). Only a hash of each token is
//! stored (`~/.qsh/devices.json`, mode 0600). A token that is not used for
//! `MAX_IDLE` stops working.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::paths;

pub const MAX_IDLE: u64 = 30 * 24 * 3600;
/// `last_used` is written to disk at most this often.
const TOUCH_EVERY: u64 = 3600;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Device {
    /// SHA-256 of the token, hex.
    pub hash: String,
    /// Where the client runs (its host name), for people.
    pub label: String,
    pub created: u64,
    pub last_used: u64,
}

pub struct Devices {
    path: PathBuf,
    list: Vec<Device>,
}

fn hash(token: &str) -> String {
    hex::encode(ring::digest::digest(&ring::digest::SHA256, token.as_bytes()))
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl Devices {
    pub fn load() -> Devices {
        let path = paths::base().join("devices.json");
        let list: Vec<Device> = std::fs::read(&path).ok().and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or_default();
        let mut d = Devices { path, list };
        let now = paths::now_secs();
        let n = d.list.len();
        d.list.retain(|x| now.saturating_sub(x.last_used) < MAX_IDLE);
        if d.list.len() != n {
            d.save();
        }
        d
    }

    fn save(&self) {
        if let Ok(b) = serde_json::to_vec_pretty(&self.list) {
            let _ = paths::write_private(&self.path, &b);
        }
    }

    /// Make a token for a device.
    pub fn add(&mut self, label: &str) -> String {
        let token = paths::random_hex(32);
        let now = paths::now_secs();
        let label: String = label.chars().filter(|c| !c.is_control()).take(64).collect();
        self.list.push(Device { hash: hash(&token), label, created: now, last_used: now });
        self.save();
        token
    }

    /// True if the token belongs to a device that may connect.
    pub fn check(&mut self, token: &str) -> bool {
        if token.is_empty() {
            return false;
        }
        let h = hash(token);
        let now = paths::now_secs();
        let mut found = false;
        let mut dirty = false;
        for d in &mut self.list {
            if ct_eq(d.hash.as_bytes(), h.as_bytes()) && now.saturating_sub(d.last_used) < MAX_IDLE {
                found = true;
                if now.saturating_sub(d.last_used) > TOUCH_EVERY {
                    d.last_used = now;
                    dirty = true;
                }
            }
        }
        if dirty {
            self.save();
        }
        found
    }

    pub fn list(&self) -> Vec<Device> {
        self.list.clone()
    }

    /// Remove devices whose hash starts with `prefix` ("" removes all).
    pub fn revoke(&mut self, prefix: &str) -> usize {
        let n = self.list.len();
        self.list.retain(|d| !d.hash.starts_with(prefix));
        self.save();
        n - self.list.len()
    }
}
