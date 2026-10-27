//! Run a command on a pseudo-terminal, as if from an interactive shell.

use std::io::Read;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub struct PtyOutput {
    /// Exit code; `None` if killed (e.g. on timeout).
    pub code: Option<i32>,
    /// Everything written to the terminal, with escape sequences removed.
    pub text: String,
    pub timed_out: bool,
}

/// Run `cmd` with stdin, stdout and stderr on a new `cols`×`rows`
/// pseudo-terminal that is its controlling terminal, killing it after
/// `timeout`. Blocks; call it from `spawn_blocking` in async tests.
pub fn run(mut cmd: Command, cols: u16, rows: u16, timeout: Duration) -> PtyOutput {
    let (mut master, mut slave) = (0, 0);
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: openpty writes two new descriptors, which we take ownership of.
    let r = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &ws,
        )
    };
    assert_eq!(r, 0, "openpty: {}", std::io::Error::last_os_error());
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    cmd.stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave.try_clone().unwrap()));
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().expect("spawning on the pty");
    // Only the child holds the terminal now, so reads end (EIO) when it exits.
    drop(slave);
    drop(cmd);
    let mut reader = std::fs::File::from(master);
    let collector = std::thread::spawn(move || {
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        out
    });
    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if start.elapsed() > timeout {
            timed_out = true;
            let _ = child.kill();
            break child.wait().unwrap();
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let raw = collector.join().unwrap();
    PtyOutput {
        code: status.code(),
        text: strip_ansi(&String::from_utf8_lossy(&raw)),
        timed_out,
    }
}

/// Remove ANSI escape sequences (CSI, OSC and two-character escapes).
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\x1b' {
            if c != '\r' {
                out.push(c);
            }
            continue;
        }
        match it.next() {
            Some('[') => {
                for c in it.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(c) = it.next() {
                    if c == '\x07' || (c == '\x1b' && it.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_on_a_terminal() {
        let mut c = Command::new("sh");
        c.args([
            "-c",
            "test -t 0 && test -t 2 && stty size && printf '\\033[1mbold\\033[0m\\n'; exit 3",
        ]);
        let o = run(c, 90, 30, Duration::from_secs(10));
        assert_eq!(o.code, Some(3), "{}", o.text);
        assert!(o.text.contains("30 90"), "{}", o.text);
        assert!(o.text.contains("bold\n"), "{:?}", o.text);
        assert_eq!(strip_ansi("a\x1b]0;t\x07b\x1b[2Jc"), "abc");
    }
}
