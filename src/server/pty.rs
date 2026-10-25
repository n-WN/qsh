//! Pseudo terminal and child process.

use std::{
    os::fd::{AsRawFd, OwnedFd},
    os::unix::process::CommandExt,
    path::Path,
    process::{Child, Command, Stdio},
};

use anyhow::{Context, Result};

pub struct Pty {
    pub master: OwnedFd,
    pub child: Child,
}

fn set_cloexec(fd: &OwnedFd) {
    unsafe {
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFD);
        libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC);
    }
}

pub fn winsize(cols: u16, rows: u16) -> libc::winsize {
    libc::winsize { ws_row: rows.max(1), ws_col: cols.max(1), ws_xpixel: 0, ws_ypixel: 0 }
}

pub fn resize(master: &OwnedFd, cols: u16, rows: u16) {
    let ws = winsize(cols, rows);
    unsafe {
        libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ as _, &ws);
    }
}

/// Start `argv` on a new pty. `login` makes argv[0] start with '-' (a login shell).
pub fn spawn(argv: &[String], login: bool, env: &[(String, String)], cwd: &Path, cols: u16, rows: u16) -> Result<Pty> {
    let ws = winsize(cols, rows);
    let r = nix::pty::openpty(Some(&ws), None).context("openpty")?;
    set_cloexec(&r.master);
    set_cloexec(&r.slave);
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    if login {
        let base = Path::new(&argv[0]).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        cmd.arg0(format!("-{base}"));
    }
    cmd.env_clear().envs(env.iter().map(|(k, v)| (k, v))).current_dir(cwd);
    cmd.stdin(Stdio::from(r.slave.try_clone()?));
    cmd.stdout(Stdio::from(r.slave.try_clone()?));
    cmd.stderr(Stdio::from(r.slave));
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Give the child default signal handling.
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
            for s in [libc::SIGPIPE, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM, libc::SIGHUP, libc::SIGCHLD] {
                libc::signal(s, libc::SIG_DFL);
            }
            Ok(())
        });
    }
    let child = cmd.spawn().with_context(|| format!("start {}", argv[0]))?;
    Ok(Pty { master: r.master, child })
}

/// The current directory of a process, if it is known.
pub fn process_cwd(pid: i32) -> Option<std::path::PathBuf> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }
    #[cfg(target_os = "macos")]
    {
        // proc_pidinfo(PROC_PIDVNODEPATHINFO)
        unsafe {
            let mut info: libc::proc_vnodepathinfo = std::mem::zeroed();
            let sz = std::mem::size_of::<libc::proc_vnodepathinfo>() as i32;
            let n = libc::proc_pidinfo(pid, libc::PROC_PIDVNODEPATHINFO, 0, &mut info as *mut _ as *mut libc::c_void, sz);
            if n != sz {
                return None;
            }
            let p = std::ffi::CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr() as *const libc::c_char);
            Some(std::path::PathBuf::from(p.to_string_lossy().into_owned()))
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}
