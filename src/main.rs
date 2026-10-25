//! qsh: a durable remote terminal over QUIC.

mod client;
mod files;
mod paths;
mod proto;
mod server;
mod tls;
mod tools;

use std::{path::PathBuf, time::Duration};

use anyhow::{Result, anyhow, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    name = "qsh",
    version,
    about = "A durable remote terminal over QUIC: roaming, native scrollback, kitty graphics, clipboard and files.",
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    #[command(flatten)]
    connect: ConnectArgs,
}

#[derive(Args)]
struct SshArgs {
    /// The ssh command to use, with options (for example "ssh -i ~/.ssh/key -p 2222").
    #[arg(long, default_value = "ssh")]
    ssh: String,
    /// Path of qsh on the server.
    #[arg(long, value_name = "PATH")]
    server: Option<String>,
    /// UDP port for the server daemon (only used when it starts).
    #[arg(long)]
    port: Option<u16>,
}

#[derive(Args)]
struct ConnectArgs {
    /// [user@]host, or a qsh:// link from `qsh share`.
    destination: Option<String>,
    /// Session name: attach to it, or make it.
    #[arg(short, long, default_value = "main")]
    session: String,
    /// Make a new session with a free name.
    #[arg(short = 'n', long = "new", conflicts_with = "session")]
    new_session: bool,
    #[command(flatten)]
    ssh: SshArgs,
    /// How QUIC packets travel: direct UDP, through the SSH channel, or auto.
    #[arg(long, value_enum, default_value_t = TransportArg::Auto)]
    transport: TransportArg,
    /// Where files from the server go (default: ~/Downloads).
    #[arg(long)]
    download_dir: Option<PathBuf>,
    /// Do not let the server read or write the local clipboard.
    #[arg(long)]
    no_clipboard: bool,
    /// Do not let the server open links on this machine.
    #[arg(long)]
    no_open: bool,
    /// Do not accept files from the server.
    #[arg(long)]
    no_receive: bool,
    /// Server reads of local files by path (`qsh get PATH`).
    #[arg(long, value_enum, default_value_t = AskArg::Ask)]
    read_paths: AskArg,
    /// Do not copy OSC 52 clipboard writes into the local clipboard.
    #[arg(long)]
    no_osc52: bool,
    #[arg(short, long)]
    verbose: bool,
    /// Command to run in a new session (instead of the login shell).
    #[arg(last = true)]
    command: Vec<String>,
}

#[derive(Clone, Copy, ValueEnum)]
enum TransportArg {
    Auto,
    Udp,
    Ssh,
}

#[derive(Clone, Copy, ValueEnum)]
enum AskArg {
    Ask,
    Always,
    Never,
}

#[derive(Subcommand)]
enum Cmd {
    /// List sessions (on HOST, or on this server inside a session).
    Ls {
        destination: Option<String>,
        #[command(flatten)]
        ssh: SshArgs,
    },
    /// Copy files through the running qsh client. Remote paths start with ':'.
    /// Example: qsh cp report.pdf :~/docs/   or   qsh cp :/var/log/syslog .
    Cp {
        /// Pid of the qsh client to use (default: the newest).
        #[arg(short = 'c', long)]
        client: Option<u32>,
        #[arg(required = true, num_args = 2..)]
        paths: Vec<String>,
    },
    /// (In a session) send files to the local machine's download folder.
    Send {
        /// Open the file on the local machine after the transfer.
        #[arg(long)]
        open: bool,
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
    /// (In a session) get local files: a file picker opens, or give local paths.
    Get {
        /// Folder for the files (default: current folder).
        #[arg(short = 'C', long)]
        dir: Option<PathBuf>,
        paths: Vec<String>,
    },
    /// (In a session) use the local clipboard.
    Clip {
        #[command(subcommand)]
        op: ClipOp,
    },
    /// (In a session) open a link on the local machine, or send a file and open it.
    Open { target: String },
    /// (In a session) detach the active client.
    Detach,
    /// (In a session) make a link that lets another person join this session.
    Share {
        #[arg(long)]
        readonly: bool,
        /// Link lifetime in seconds.
        #[arg(long, default_value_t = 3600)]
        ttl: u64,
    },
    #[command(hide = true)]
    Daemon {
        #[arg(long)]
        port: Option<u16>,
        /// Exit after this many seconds with no sessions and no clients.
        #[arg(long, default_value_t = 300)]
        idle_exit: u64,
    },
    #[command(hide = true)]
    Helper {
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        tunnel: bool,
        #[arg(long)]
        no_token: bool,
        #[arg(long)]
        list: bool,
    },
}

#[derive(Subcommand)]
enum ClipOp {
    /// Copy stdin (or a file) into the local clipboard.
    Copy {
        #[arg(short, long, default_value = "text/plain")]
        r#type: String,
        file: Option<PathBuf>,
    },
    /// Write the local clipboard to stdout.
    Paste {
        #[arg(short, long, default_value = "text/plain")]
        r#type: String,
    },
    /// List the types in the local clipboard.
    List,
}

fn ssh_spec(dest: &str, a: &SshArgs) -> Result<client::net::SshSpec> {
    let argv: Vec<String> = a.ssh.split_whitespace().map(|w| paths::expand_tilde(w).display().to_string()).collect();
    if argv.is_empty() {
        bail!("empty --ssh");
    }
    Ok(client::net::SshSpec { argv, dest: dest.to_string(), server_path: a.server.clone(), port: a.port })
}

/// qsh://HOST:PORT/SESSION?fp=HEX[&ro=1]#TOKEN
fn parse_link(s: &str) -> Result<(std::net::SocketAddr, String, String, String)> {
    let rest = s.strip_prefix("qsh://").ok_or_else(|| anyhow!("not a qsh:// link"))?;
    let (rest, token) = rest.split_once('#').ok_or_else(|| anyhow!("link has no token"))?;
    let (rest, query) = rest.split_once('?').ok_or_else(|| anyhow!("link has no fingerprint"))?;
    let (hostport, session) = rest.split_once('/').unwrap_or((rest, "main"));
    let fp = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("fp="))
        .ok_or_else(|| anyhow!("link has no fingerprint"))?
        .to_string();
    use std::net::ToSocketAddrs;
    let addr = hostport.to_socket_addrs()?.next().ok_or_else(|| anyhow!("cannot resolve {hostport}"))?;
    Ok((addr, fp, token.to_string(), session.to_string()))
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio runtime")
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let argv0 = std::path::Path::new(&args[0]).file_name().and_then(|s| s.to_str()).unwrap_or("qsh").to_string();
    if server::daemon::SHIMS.contains(&argv0.as_str()) {
        std::process::exit(tools::shim(&argv0, &args[1..]));
    }
    let cli = Cli::parse();
    if let Ok(filter) = std::env::var("QSH_TRACE") {
        let w = std::env::var("QSH_LOG").ok().and_then(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).ok());
        let b = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::new(filter)).with_ansi(false);
        match w {
            Some(f) => b.with_writer(std::sync::Mutex::new(f)).init(),
            None => b.with_writer(std::io::stderr).init(),
        }
    }
    let code = match real_main(cli) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("qsh: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

fn real_main(cli: Cli) -> Result<i32> {
    let rt = runtime();
    match cli.cmd {
        Some(Cmd::Daemon { port, idle_exit }) => rt.block_on(server::daemon::run(port, Duration::from_secs(idle_exit))).map(|_| 0),
        Some(Cmd::Helper { port, tunnel, no_token, list }) => {
            rt.block_on(server::helper::run(server::helper::HelperArgs { port, tunnel, token: !no_token, list })).map(|_| 0)
        }
        Some(Cmd::Ls { destination: None, .. }) => rt.block_on(tools::ls_local()).map(|_| 0),
        Some(Cmd::Ls { destination: Some(dest), ssh }) => {
            let spec = ssh_spec(&dest, &ssh)?;
            let mut cmd = std::process::Command::new(&spec.argv[0]);
            cmd.args(&spec.argv[1..]).arg("-T").arg(&dest);
            cmd.arg(match &spec.server_path {
                Some(p) => format!("{p} helper --list"),
                None => "sh -c 'Q=$(command -v qsh || echo \"$HOME/.local/bin/qsh\"); exec \"$Q\" helper --list'".to_string(),
            });
            let out = cmd.output()?;
            if !out.status.success() {
                bail!("ssh failed: {}", String::from_utf8_lossy(&out.stderr).trim());
            }
            let text = String::from_utf8_lossy(&out.stdout);
            let line = text.lines().rev().find(|l| l.starts_with('[')).unwrap_or("[]");
            let sessions: Vec<proto::SessionInfo> = serde_json::from_str(line)?;
            tools::print_sessions(&sessions);
            Ok(0)
        }
        Some(Cmd::Cp { client, paths }) => rt.block_on(client::rpc::cp(paths, client)).map(|_| 0),
        Some(Cmd::Send { open, files }) => rt.block_on(tools::send(files, open)).map(|_| 0),
        Some(Cmd::Get { dir, paths }) => rt.block_on(tools::get(dir, paths)).map(|_| 0),
        Some(Cmd::Clip { op }) => rt
            .block_on(async {
                use std::io::{Read, Write};
                match op {
                    ClipOp::Copy { r#type, file } => {
                        let data = match file {
                            Some(f) => std::fs::read(f)?,
                            None => {
                                let mut d = Vec::new();
                                std::io::stdin().read_to_end(&mut d)?;
                                d
                            }
                        };
                        tools::clip_set(&r#type, &data).await
                    }
                    ClipOp::Paste { r#type } => {
                        let d = tools::clip_get(&r#type).await?;
                        std::io::stdout().write_all(&d)?;
                        Ok(())
                    }
                    ClipOp::List => {
                        for t in tools::clip_list().await? {
                            println!("{t}");
                        }
                        Ok(())
                    }
                }
            })
            .map(|_| 0),
        Some(Cmd::Open { target }) => rt.block_on(tools::open(&target)).map(|_| 0),
        Some(Cmd::Detach) => rt.block_on(tools::detach()).map(|_| 0),
        Some(Cmd::Share { readonly, ttl }) => rt.block_on(tools::share(readonly, ttl)).map(|_| 0),
        None => {
            let c = cli.connect;
            let Some(dest) = c.destination.clone() else {
                use clap::CommandFactory;
                Cli::command().print_help()?;
                return Ok(2);
            };
            let policy = client::rpc::Policy {
                clipboard: !c.no_clipboard,
                open: !c.no_open,
                receive: !c.no_receive,
                read_paths: match c.read_paths {
                    AskArg::Ask => client::rpc::Ask::Ask,
                    AskArg::Always => client::rpc::Ask::Always,
                    AskArg::Never => client::rpc::Ask::Never,
                },
            };
            let transport = match c.transport {
                TransportArg::Auto => client::net::Transport::Auto,
                TransportArg::Udp => client::net::Transport::Udp,
                TransportArg::Ssh => client::net::Transport::Ssh,
            };
            let download_dir = c.download_dir.clone().unwrap_or_else(|| paths::home().join("Downloads"));
            let mut opts = client::Opts {
                new_session: c.new_session,
                ssh: None,
                direct: None,
                session: c.session.clone(),
                transport,
                command: c.command.clone(),
                download_dir,
                policy,
                osc52: !c.no_osc52,
                verbose: c.verbose,
            };
            if dest.starts_with("qsh://") {
                let (addr, fp, token, session) = parse_link(&dest)?;
                opts.direct = Some((addr, fp, token));
                opts.session = session;
            } else {
                opts.ssh = Some(ssh_spec(&dest, &c.ssh)?);
            }
            rt.block_on(client::run(opts))
        }
    }
}
