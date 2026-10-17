//! Run a private Meilisearch server as a supervised child process.
//!
//! The database lives in `<data_dir>/data.ms` (dumps in `<data_dir>/dumps`).
//! The server listens on a random loopback port and is protected by a random
//! master key generated on every start.
//!
//! Search data is expected to be derived from another source of truth, so if
//! the on-disk database was written by a different Meilisearch version (which
//! Meilisearch cannot open), it is discarded and [`EmbeddedMeilisearch::reset`]
//! is set so the caller can rebuild it. `reset` is also set for a brand-new
//! database.
//!
//! The child is stopped when [`EmbeddedMeilisearch::stop`] is called or the
//! handle is dropped. On Linux it is also signalled if the parent dies
//! (`PR_SET_PDEATHSIG`); call [`EmbeddedMeilisearch::start`] from a thread that
//! lives as long as the process (e.g. the main thread).

use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct Options {
    /// Directory holding the database. Created if missing.
    pub data_dir: PathBuf,
    /// The `meilisearch` binary; defaults to `meilisearch` on the PATH.
    pub binary: Option<PathBuf>,
    /// How long to wait for the server to become healthy.
    pub start_timeout: Duration,
}

impl Options {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            binary: None,
            start_timeout: Duration::from_secs(60),
        }
    }
}

pub struct EmbeddedMeilisearch {
    child: Option<Child>,
    /// Base URL, e.g. `http://127.0.0.1:41234`.
    pub url: String,
    /// Master key required by the server.
    pub master_key: String,
    /// The database was created empty by this start (new, or discarded
    /// because of a version change); the caller should rebuild it.
    pub reset: bool,
    pub version: String,
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .map(|d| Path::new(d).join(name))
        .find(|p| p.is_file())
}

/// Version reported by `meilisearch --version` (e.g. `meilisearch 1.54.3` -> `1.54.3`).
pub fn binary_version(binary: &Path) -> Result<String> {
    let out = Command::new(binary)
        .arg("--version")
        .output()
        .with_context(|| format!("running {}", binary.display()))?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace()
        .find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .map(str::to_string)
        .with_context(|| format!("unexpected meilisearch --version output {text:?}"))
}

fn random_key() -> Result<String> {
    let mut buf = [0u8; 24];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

fn free_port() -> Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

/// Minimal blocking `GET /health` probe.
fn healthy(port: u16) -> bool {
    let Ok(mut s) =
        std::net::TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(500))
    else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    if s.write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut buf = [0u8; 64];
    let n = s.read(&mut buf).unwrap_or(0);
    std::str::from_utf8(&buf[..n]).is_ok_and(|r| r.starts_with("HTTP/1.1 200"))
}

fn supervise(cmd: &mut Command, signal: libc::c_int) {
    cmd.process_group(0);
    #[cfg(target_os = "linux")]
    unsafe {
        cmd.pre_exec(move || {
            // SAFETY: prctl is async-signal-safe.
            if libc::prctl(libc::PR_SET_PDEATHSIG, signal) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    #[cfg(not(target_os = "linux"))]
    let _ = signal;
}

impl EmbeddedMeilisearch {
    pub fn start(opts: &Options) -> Result<Self> {
        let binary = match &opts.binary {
            Some(b) => b.clone(),
            None => which("meilisearch").context("meilisearch not found on the PATH")?,
        };
        let version = binary_version(&binary)?;
        std::fs::create_dir_all(&opts.data_dir)
            .with_context(|| format!("creating {}", opts.data_dir.display()))?;
        let data_dir = opts.data_dir.canonicalize()?;
        let db = data_dir.join("data.ms");

        let reset = match std::fs::read_to_string(db.join("VERSION")) {
            Ok(v) if v.trim() == version => false,
            Ok(v) => {
                tracing::warn!(
                    old = v.trim(),
                    new = %version,
                    "embedded Meilisearch database was written by another version; discarding it for a rebuild"
                );
                std::fs::remove_dir_all(&db).with_context(|| format!("removing {}", db.display()))?;
                true
            }
            Err(_) => {
                if db.exists() {
                    // Incomplete/unknown database: start over.
                    std::fs::remove_dir_all(&db)?;
                }
                true
            }
        };

        let port = free_port()?;
        let master_key = random_key()?;
        let mut cmd = Command::new(&binary);
        cmd.arg("--db-path")
            .arg(&db)
            .arg("--dump-dir")
            .arg(data_dir.join("dumps"))
            .args(["--http-addr", &format!("127.0.0.1:{port}"), "--env", "production"])
            .args(["--no-analytics", "--log-level", "ERROR"])
            .env("MEILI_MASTER_KEY", &master_key)
            .current_dir(&data_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        supervise(&mut cmd, libc::SIGTERM);
        let child = cmd
            .spawn()
            .with_context(|| format!("starting {}", binary.display()))?;
        let mut m = EmbeddedMeilisearch {
            child: Some(child),
            url: format!("http://127.0.0.1:{port}"),
            master_key,
            reset,
            version,
        };
        let deadline = Instant::now() + opts.start_timeout;
        loop {
            if let Some(status) = m
                .child
                .as_mut()
                .and_then(|c| c.try_wait().transpose())
                .transpose()?
            {
                m.child = None;
                bail!("meilisearch exited during startup ({status}); see its log output above");
            }
            if healthy(port) {
                break;
            }
            if Instant::now() > deadline {
                bail!(
                    "meilisearch did not become healthy within {:?}",
                    opts.start_timeout
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        tracing::info!(url = %m.url, version = %m.version, reset = m.reset, "embedded Meilisearch ready");
        Ok(m)
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    /// Graceful shutdown (SIGTERM), escalating to SIGKILL after `timeout`.
    pub fn stop_with_timeout(&mut self, timeout: Duration) -> Result<()> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        // SAFETY: signalling our own child process.
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if child.try_wait()?.is_some() {
                tracing::info!("embedded Meilisearch stopped");
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        tracing::warn!("embedded Meilisearch did not stop in time; killing it");
        child.kill()?;
        child.wait()?;
        Ok(())
    }

    pub fn stop(mut self) -> Result<()> {
        self.stop_with_timeout(Duration::from_secs(30))
    }
}

impl Drop for EmbeddedMeilisearch {
    fn drop(&mut self) {
        if let Err(e) = self.stop_with_timeout(Duration::from_secs(30)) {
            tracing::warn!(error = %e, "stopping embedded Meilisearch");
        }
    }
}
