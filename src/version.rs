//! Build identity (see build.rs).

use std::fmt;

pub const BUILD_ID: &str = env!("QSH_BUILD_ID");
const BUILD_TIME: &str = env!("QSH_BUILD_TIME");

/// The source of this build (a tar file).
pub static SOURCE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/source.tar"));

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Build {
    pub id: String,
    /// Time of the newest source file: it orders builds.
    pub time: u64,
}

impl Build {
    pub fn current() -> Build {
        Build { id: BUILD_ID.to_string(), time: BUILD_TIME.parse().unwrap_or(0) }
    }

    /// "ID:TIME", as in the helper's connect line and the daemon's replies.
    pub fn parse(s: &str) -> Option<Build> {
        let (id, t) = s.split_once(':')?;
        (!id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit())).then(|| Build { id: id.to_string(), time: t.parse().unwrap_or(0) })
    }

    /// True if `self` should replace `other`: a different build from newer source.
    pub fn replaces(&self, other: &Build) -> bool {
        self.id != other.id && self.time > other.time
    }
}

impl fmt::Display for Build {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.id, self.time)
    }
}

/// "os-arch" as Rust names them, for example "linux-x86_64".
pub fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// The platform name for the output of `uname -s` and `uname -m`.
pub fn platform_of_uname(sys: &str, machine: &str) -> String {
    let os = match sys {
        "Linux" => "linux",
        "Darwin" => "macos",
        "FreeBSD" => "freebsd",
        other => return format!("{}-{machine}", other.to_lowercase()),
    };
    let arch = match machine {
        "x86_64" | "amd64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        other => other,
    };
    format!("{os}-{arch}")
}

/// A short text for people: "0.1.0 (1a2b3c4d)".
pub fn describe() -> String {
    format!("{} ({})", env!("CARGO_PKG_VERSION"), &BUILD_ID[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order() {
        let a = Build { id: "aa".into(), time: 10 };
        let b = Build { id: "bb".into(), time: 20 };
        assert!(b.replaces(&a));
        assert!(!a.replaces(&b));
        assert!(!a.replaces(&a.clone()));
        assert_eq!(Build::parse(&b.to_string()), Some(b));
        assert_eq!(Build::parse("zz:1"), None);
        assert_eq!(platform_of_uname("Linux", "amd64"), "linux-x86_64");
        assert_eq!(platform_of_uname("Darwin", "arm64"), "macos-aarch64");
        assert!(!SOURCE.is_empty());
    }
}
