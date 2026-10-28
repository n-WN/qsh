//! Switches for ablation studies (tests/netlab/ablate.py). QSH_ABLATE is a
//! list of mechanisms to turn off, for example `QSH_ABLATE=floor,skip`, so
//! that the effect of each one can be measured, and of values to try
//! (`initwin=76800`). Not for normal use.

use std::sync::OnceLock;

/// The mechanisms that can be turned off.
const NAMES: &[&str] = &[
    "bbr", "floor", "initwin", "win32k", "skip", "ping4", "knownrtt", "fastretry", "reconnect", "copies", "fastrace", "prewarm",
];
/// The values that can be set.
const VALUES: &[&str] = &["initwin", "winmin"];

fn set() -> &'static [String] {
    static SET: OnceLock<Vec<String>> = OnceLock::new();
    SET.get_or_init(|| {
        std::env::var("QSH_ABLATE")
            .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
            .unwrap_or_default()
    })
}

/// Stops at start for a wrong name: it must not give a measurement of nothing.
pub fn check() -> anyhow::Result<()> {
    for s in set() {
        let ok = match s.split_once('=') {
            Some((k, v)) => VALUES.contains(&k) && v.parse::<u64>().is_ok(),
            None => NAMES.contains(&s.as_str()),
        };
        if !ok {
            anyhow::bail!("QSH_ABLATE: unknown entry {s:?} (names: {}; values: {}=N)", NAMES.join(","), VALUES.join("=N,"));
        }
    }
    Ok(())
}

/// True if `name` is turned off.
pub fn off(name: &str) -> bool {
    set().iter().any(|s| s == name)
}

/// The value set for `name`, if any.
pub fn value(name: &str) -> Option<u64> {
    set().iter().find_map(|s| s.split_once('=').filter(|(k, _)| *k == name).and_then(|(_, v)| v.parse().ok()))
}
