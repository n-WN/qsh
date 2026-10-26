//! Put qsh on a server: the same build as this client. The binary comes from
//! the local cache, from this program (same platform), or is built on the
//! server from the source inside this program. Builds made on a server go into
//! the local cache for other servers of that platform.

use std::{
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Command, Stdio},
    time::Instant,
};

use anyhow::{Context, Result, anyhow, bail};

use super::net::SshSpec;
use crate::version::{self, BUILD_ID};

fn cache_path(platform: &str) -> PathBuf {
    crate::paths::base().join("cache").join(BUILD_ID).join(platform).join("qsh")
}

fn local_binary(platform: &str) -> Option<PathBuf> {
    let c = cache_path(platform);
    if c.is_file() {
        return Some(c);
    }
    (platform == version::platform()).then(|| std::env::current_exe().ok()).flatten()
}

fn ssh(spec: &SshSpec, script: &str) -> Command {
    let mut cmd = Command::new(&spec.argv[0]);
    cmd.args(&spec.argv[1..]).arg("-T").arg(&spec.dest).arg(format!("sh -c '{script}'"));
    cmd
}

/// Shell lines (no single quotes) that install "$v/qsh.new" as the current build.
fn activate_lines() -> String {
    format!(
        r#"chmod 755 "$v/qsh.new"
"$v/qsh.new" version >/dev/null
mv -f "$v/qsh.new" "$v/qsh"
ln -sfn "versions/{BUILD_ID}/qsh" "$HOME/.qsh/current.new"
mv -f "$HOME/.qsh/current.new" "$HOME/.qsh/current"
ls -1dt "$HOME"/.qsh/versions/*/ 2>/dev/null | tail -n +4 | xargs rm -rf 2>/dev/null || true
echo QSH-INSTALLED"#
    )
}

/// Install this build on the server (`platform` as in `version::platform`).
pub async fn install(spec: &SshSpec, platform: &str, why: &str) -> Result<()> {
    eprintln!("qsh: {why}: installing qsh {} on {} ({platform})", version::describe(), spec.dest);
    let t0 = Instant::now();
    let spec2 = spec.clone();
    let platform2 = platform.to_string();
    tokio::task::spawn_blocking(move || match local_binary(&platform2) {
        Some(bin) => upload(&spec2, &bin),
        None => build_on_server(&spec2, &platform2),
    })
    .await??;
    eprintln!("qsh: installed in {:.1}s", t0.elapsed().as_secs_f32());
    Ok(())
}

fn upload(spec: &SshSpec, bin: &PathBuf) -> Result<()> {
    let file = std::fs::File::open(bin).with_context(|| format!("open {}", bin.display()))?;
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    eprint!("qsh: sending {} …", super::rpc::human(size));
    let mut gz = Command::new("gzip").arg("-c").stdin(file).stdout(Stdio::piped()).spawn().context("start gzip")?;
    let script = format!(
        r#"set -e
v="$HOME/.qsh/versions/{BUILD_ID}"
mkdir -p "$v"
gzip -dc > "$v/qsh.new"
{}"#,
        activate_lines()
    );
    let out = ssh(spec, &script).stdin(Stdio::from(gz.stdout.take().unwrap())).stderr(Stdio::piped()).output()?;
    let _ = gz.wait();
    eprintln!();
    if !String::from_utf8_lossy(&out.stdout).contains("QSH-INSTALLED") {
        bail!("install failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

fn build_on_server(spec: &SshSpec, platform: &str) -> Result<()> {
    eprintln!("qsh: no qsh binary for {platform} here; building on the server (the first build takes a few minutes)");
    let script = format!(
        r#"set -e
c=$(command -v cargo 2>/dev/null || true)
[ -n "$c" ] || c="$HOME/.cargo/bin/cargo"
[ -x "$c" ] || {{ echo QSH-NO-CARGO; exit 0; }}
PATH="$(dirname "$c"):$PATH"
b="$HOME/.qsh/build"
rm -rf "$b/{BUILD_ID}"
mkdir -p "$b/{BUILD_ID}"
cd "$b/{BUILD_ID}"
tar xf -
cd qsh
# Low priority: the server may have other work.
CARGO_TARGET_DIR="$b/target" nice -n 10 "$c" build --release --locked 2>&1
v="$HOME/.qsh/versions/{BUILD_ID}"
mkdir -p "$v"
cp "$b/target/release/qsh" "$v/qsh.new"
cd "$HOME"
rm -rf "$b/{BUILD_ID}"
{}"#,
        activate_lines()
    );
    let mut child = ssh(spec, &script).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let mut stdin = child.stdin.take().unwrap();
    std::thread::spawn(move || {
        let _ = stdin.write_all(version::SOURCE);
    });
    let mut tail: Vec<String> = Vec::new();
    let (mut installed, mut no_cargo) = (false, false);
    for line in BufReader::new(child.stdout.take().unwrap()).lines() {
        let line = line?;
        let t = line.trim();
        if t == "QSH-INSTALLED" {
            installed = true;
        } else if t == "QSH-NO-CARGO" {
            no_cargo = true;
        } else if let Some(what) = t.strip_prefix("Compiling ") {
            eprint!("\r\x1b[Kqsh: building on the server · {what}");
        }
        tail.push(line);
        if tail.len() > 30 {
            tail.remove(0);
        }
    }
    let status = child.wait()?;
    eprint!("\r\x1b[K");
    if no_cargo {
        bail!(
            "the server has no Rust toolchain, and there is no qsh binary for {platform} here.\n\
             Install Rust on the server (https://rustup.rs), or put a qsh binary for {platform} at {}",
            cache_path(platform).display()
        );
    }
    if !installed || !status.success() {
        let mut err = String::new();
        let _ = std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut err);
        bail!("the build on the server failed:\n{}\n{}", tail.join("\n"), err.trim());
    }
    // Keep the binary for other servers of this platform.
    let out = ssh(spec, &format!(r#"cat "$HOME/.qsh/versions/{BUILD_ID}/qsh""#)).stderr(Stdio::null()).output()?;
    if out.status.success() && out.stdout.len() > 1 << 20 {
        let p = cache_path(platform);
        if let Some(d) = p.parent() {
            crate::paths::ensure_dir(d)?;
        }
        std::fs::write(&p, &out.stdout)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// The platform of the server, from `uname` (for servers with an old qsh).
pub async fn remote_platform(spec: &SshSpec) -> Result<String> {
    let mut cmd = ssh(spec, "uname -s; uname -m");
    let out = tokio::task::spawn_blocking(move || cmd.stderr(Stdio::null()).output()).await??;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut l = text.lines().map(str::trim).filter(|l| !l.is_empty());
    match (l.next(), l.next()) {
        (Some(s), Some(m)) => Ok(version::platform_of_uname(s, m)),
        _ => Err(anyhow!("cannot find the platform of {}", spec.dest)),
    }
}
