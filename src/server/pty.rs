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

pub fn winsize(cols: u16, rows: u16, xpix: u16, ypix: u16) -> libc::winsize {
    libc::winsize { ws_row: rows.max(1), ws_col: cols.max(1), ws_xpixel: xpix, ws_ypixel: ypix }
}

pub fn resize(master: &OwnedFd, cols: u16, rows: u16, xpix: u16, ypix: u16) {
    let ws = winsize(cols, rows, xpix, ypix);
    unsafe {
        libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ as _, &ws);
    }
}

/// Start `argv` on a new pty. `login` makes argv[0] start with '-' (a login shell).
pub fn spawn(argv: &[String], login: bool, env: &[(String, String)], cwd: &Path, size: [u16; 4]) -> Result<Pty> {
    let ws = winsize(size[0], size[1], size[2], size[3]);
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

/// The pty mode as `proto::mode` bits: echo and line mode of the terminal,
/// and whether the foreground program is a shell.
pub fn mode(master: &OwnedFd) -> u8 {
    use crate::proto::mode;
    let mut m = 0;
    if let Ok(t) = nix::sys::termios::tcgetattr(master) {
        use nix::sys::termios::LocalFlags;
        m |= mode::KNOWN;
        if t.local_flags.contains(LocalFlags::ECHO) {
            m |= mode::ECHO;
        }
        if t.local_flags.contains(LocalFlags::ICANON) {
            m |= mode::ICANON;
        }
    }
    let pgrp = unsafe { libc::tcgetpgrp(master.as_raw_fd()) };
    if pgrp > 0 && process_name(pgrp).is_some_and(|n| is_shell(&n)) {
        m |= mode::SHELL;
    }
    m
}

fn is_shell(name: &str) -> bool {
    let n = name.trim_start_matches('-');
    ["bash", "zsh", "fish", "sh", "dash", "ash", "ksh", "mksh", "oksh", "yash", "tcsh", "csh", "nu", "elvish", "xonsh", "pwsh"].contains(&n)
}

fn process_name(pid: i32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string(format!("/proc/{pid}/comm")).ok().map(|s| s.trim().to_string())
    }
    #[cfg(target_os = "macos")]
    {
        let mut buf = [0u8; 256];
        let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32) };
        (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
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
