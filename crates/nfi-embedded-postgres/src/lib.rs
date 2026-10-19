//! Run a private PostgreSQL server as a supervised child process.
//!
//! The server keeps its cluster in `<data_dir>/pgdata`, listens only on a
//! Unix socket inside `<data_dir>` (no TCP), and uses trust authentication
//! for the `nfi` role. On first start the cluster is created with `initdb`
//! and a database is created for the service.
//!
//! The child is stopped when [`EmbeddedPostgres::stop`] is called or the
//! handle is dropped. On Linux it is also signalled if the parent dies
//! (`PR_SET_PDEATHSIG`); for that to be reliable, call [`EmbeddedPostgres::start`]
//! from a thread that lives as long as the process (e.g. the main thread).
//!
//! PostgreSQL refuses to run as root, so starting as root is an error.

use anyhow::{Context, Result, bail};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const USER: &str = "nfi";
/// Port number; only used to name the Unix socket (`.s.PGSQL.<port>`).
pub const PORT: u16 = 5432;

#[derive(Clone, Debug)]
pub struct Options {
    /// Directory holding the cluster and the socket. Created if missing.
    pub data_dir: PathBuf,
    /// Directory containing `initdb`, `postgres`, `pg_isready`, `createdb` and `psql`.
    /// Defaults to `pg_config --bindir`, then the directory of `postgres` on
    /// the PATH, then the newest `/usr/lib/postgresql/*/bin`.
    pub bin_dir: Option<PathBuf>,
    /// Database to create and connect to.
    pub database: String,
    /// How long to wait for the server to accept connections.
    pub start_timeout: Duration,
}

impl Options {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            bin_dir: None,
            database: "nfi".into(),
            start_timeout: Duration::from_secs(60),
        }
    }
}

pub struct EmbeddedPostgres {
    child: Option<Child>,
    socket_dir: PathBuf,
    database: String,
    /// Whether the cluster was created by this start.
    pub created: bool,
    /// Server major version.
    pub major_version: u32,
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .map(|d| Path::new(d).join(name))
        .find(|p| p.is_file())
}

/// Locate the PostgreSQL server binaries.
pub fn find_bin_dir(explicit: Option<&Path>) -> Result<PathBuf> {
    let has_all = |d: &Path| {
        ["initdb", "postgres", "pg_isready", "createdb", "psql"]
            .iter()
            .all(|b| d.join(b).is_file())
    };
    if let Some(d) = explicit {
        if has_all(d) {
            return Ok(d.to_path_buf());
        }
        bail!(
            "{} does not contain initdb, postgres, pg_isready, createdb and psql",
            d.display()
        );
    }
    if let Ok(out) = Command::new("pg_config").arg("--bindir").output() {
        let d = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        if out.status.success() && has_all(&d) {
            return Ok(d);
        }
    }
    if let Some(d) = which("postgres").and_then(|p| p.parent().map(Path::to_path_buf))
        && has_all(&d)
    {
        return Ok(d);
    }
    if let Ok(entries) = std::fs::read_dir("/usr/lib/postgresql") {
        let mut dirs: Vec<(u32, PathBuf)> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| Some((e.file_name().to_str()?.parse().ok()?, e.path().join("bin"))))
            .filter(|(_, d)| has_all(d))
            .collect();
        dirs.sort();
        if let Some((_, d)) = dirs.pop() {
            return Ok(d);
        }
    }
    bail!("PostgreSQL server binaries not found; install PostgreSQL or pass the bin directory explicitly")
}

/// Major version of the `postgres` binary, from `postgres --version`
/// (e.g. `postgres (PostgreSQL) 16.10` -> 16).
pub fn binary_major_version(bin_dir: &Path) -> Result<u32> {
    let out = Command::new(bin_dir.join("postgres"))
        .arg("--version")
        .output()
        .context("running postgres --version")?;
    let text = String::from_utf8_lossy(&out.stdout);
    parse_major(&text).with_context(|| format!("unexpected postgres --version output {text:?}"))
}

fn parse_major(version_output: &str) -> Option<u32> {
    let mut words = version_output.split_whitespace();
    words.find(|w| *w == "(PostgreSQL)")?;
    let digits: String = words.next()?.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// PID recorded in `postmaster.pid`, if any.
fn postmaster_pid(pgdata: &Path) -> Option<u32> {
    std::fs::read_to_string(pgdata.join("postmaster.pid"))
        .ok()?
        .lines()
        .next()?
        .trim()
        .parse()
        .ok()
}

/// PID of a live postmaster for this cluster (a stale pid file is ignored).
fn running_postmaster(pgdata: &Path) -> Option<u32> {
    let pid = postmaster_pid(pgdata)?;
    // SAFETY: signal 0 only checks for existence.
    (unsafe { libc::kill(pid as libc::pid_t, 0) } == 0).then_some(pid)
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// Configure the child to receive `signal` when its parent thread exits and
/// to run in its own process group (so terminal signals reach only us).
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

/// Recursively set mode 0700 on `path` and everything below it (symlinks are
/// left alone). PostgreSQL refuses to start unless the data directory is
/// owner-only, and volumes or restored backups often arrive with wider modes.
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::symlink_metadata(path).with_context(|| format!("reading {}", path.display()))?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    if meta.permissions().mode() & 0o7777 != 0o700 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting the mode of {}", path.display()))?;
    }
    if meta.is_dir() {
        for entry in std::fs::read_dir(path).with_context(|| format!("reading {}", path.display()))? {
            restrict_permissions(&entry?.path())?;
        }
    }
    Ok(())
}

fn run(cmd: &mut Command, what: &str) -> Result<()> {
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {what}"))?;
    if !out.status.success() {
        bail!(
            "{what} failed ({}): {}{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

impl EmbeddedPostgres {
    /// Initialise (if needed) and start the server, waiting until it accepts
    /// connections.
    pub fn start(opts: &Options) -> Result<Self> {
        if is_root() {
            bail!(
                "PostgreSQL refuses to run as root; run as an unprivileged user to use embedded PostgreSQL"
            );
        }
        let bin = find_bin_dir(opts.bin_dir.as_deref())?;
        let major = binary_major_version(&bin)?;
        std::fs::create_dir_all(&opts.data_dir)
            .with_context(|| format!("creating {}", opts.data_dir.display()))?;
        let data_dir = opts.data_dir.canonicalize()?;
        let pgdata = data_dir.join("pgdata");
        // Unix socket paths are limited to ~107 bytes.
        let socket_len = data_dir.as_os_str().len() + format!("/.s.PGSQL.{PORT}").len();
        if socket_len > 100 {
            bail!(
                "data directory path {} is too long for a Unix socket; use a shorter path",
                data_dir.display()
            );
        }

        let version_file = pgdata.join("PG_VERSION");
        let created = if version_file.exists() {
            let existing: u32 = std::fs::read_to_string(&version_file)?
                .trim()
                .parse()
                .with_context(|| format!("parsing {}", version_file.display()))?;
            if existing != major {
                bail!(
                    "the cluster in {} was created by PostgreSQL {existing}, but the available server is PostgreSQL {major}. \
                     Upgrade it with pg_upgrade, or delete the directory and re-ingest the interchange bundles.",
                    pgdata.display()
                );
            }
            false
        } else {
            tracing::info!(dir = %pgdata.display(), version = major, "initialising embedded PostgreSQL cluster");
            run(
                Command::new(bin.join("initdb")).arg("-D").arg(&pgdata).args([
                    "-U",
                    USER,
                    "--auth=trust",
                    "-E",
                    "UTF8",
                    "--locale=C",
                    "--no-instructions",
                ]),
                "initdb",
            )?;
            true
        };

        if let Some(pid) = running_postmaster(&pgdata) {
            bail!(
                "PostgreSQL (pid {pid}) is already running on {}; is another nfi-service using it?",
                pgdata.display()
            );
        }
        restrict_permissions(&pgdata)?;

        let mut cmd = Command::new(bin.join("postgres"));
        cmd.arg("-D")
            .arg(&pgdata)
            .args(["-p", &PORT.to_string(), "-c", "listen_addresses="])
            .arg("-k")
            .arg(&data_dir)
            .args([
                "-c",
                "unix_socket_permissions=0700",
                "-c",
                "log_min_messages=warning",
                "-c",
                "log_checkpoints=off",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        // SIGINT is PostgreSQL's "fast shutdown".
        supervise(&mut cmd, libc::SIGINT);
        let child = cmd.spawn().context("starting postgres")?;
        let mut pg = EmbeddedPostgres {
            child: Some(child),
            socket_dir: data_dir,
            database: opts.database.clone(),
            created,
            major_version: major,
        };

        let deadline = Instant::now() + opts.start_timeout;
        loop {
            if let Some(status) = pg
                .child
                .as_mut()
                .and_then(|c| c.try_wait().transpose())
                .transpose()?
            {
                pg.child = None;
                bail!("postgres exited during startup ({status}); see its log output above");
            }
            // Only our own server counts (another one could answer on the socket).
            let ours = pg.child.as_ref().map(Child::id) == postmaster_pid(&pgdata);
            let ready = ours
                && Command::new(bin.join("pg_isready"))
                    .arg("-q")
                    .arg("-h")
                    .arg(&pg.socket_dir)
                    .args(["-p", &PORT.to_string(), "-U", USER, "-d", "postgres"])
                    .status()
                    .is_ok_and(|s| s.success());
            if ready {
                break;
            }
            if Instant::now() > deadline {
                bail!("postgres did not become ready within {:?}", opts.start_timeout);
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        // Create the service database (first start, or if it was dropped).
        let out = Command::new(bin.join("psql"))
            .arg("-h")
            .arg(&pg.socket_dir)
            .args(["-p", &PORT.to_string(), "-U", USER, "-d", "postgres", "-tAc"])
            .arg(format!(
                "SELECT 1 FROM pg_database WHERE datname = '{}'",
                pg.database.replace('\'', "''")
            ))
            .stdin(Stdio::null())
            .output()
            .context("running psql")?;
        if !out.status.success() {
            bail!(
                "checking for database {}: {}",
                pg.database,
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let exists = String::from_utf8_lossy(&out.stdout).trim() == "1";
        if !exists {
            run(
                Command::new(bin.join("createdb"))
                    .arg("-h")
                    .arg(&pg.socket_dir)
                    .args(["-p", &PORT.to_string(), "-U", USER, &pg.database]),
                "createdb",
            )?;
        }
        tracing::info!(socket = %pg.socket_dir.display(), version = major, "embedded PostgreSQL ready");
        Ok(pg)
    }

    /// Directory containing the server's Unix socket.
    pub fn socket_dir(&self) -> &Path {
        &self.socket_dir
    }

    /// Connection URL (libpq/sqlx style, Unix socket via the `host` parameter).
    pub fn url(&self) -> String {
        format!(
            "postgres://{USER}@localhost:{PORT}/{}?host={}",
            self.database,
            percent_encode(&self.socket_dir.to_string_lossy())
        )
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    /// Fast shutdown (SIGINT), escalating to SIGKILL after `timeout`.
    pub fn stop_with_timeout(&mut self, timeout: Duration) -> Result<()> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        // SAFETY: signalling our own child process.
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if child.try_wait()?.is_some() {
                tracing::info!("embedded PostgreSQL stopped");
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        tracing::warn!("embedded PostgreSQL did not stop in time; killing it");
        child.kill()?;
        child.wait()?;
        Ok(())
    }

    pub fn stop(mut self) -> Result<()> {
        self.stop_with_timeout(Duration::from_secs(60))
    }
}

impl Drop for EmbeddedPostgres {
    fn drop(&mut self) {
        if let Err(e) = self.stop_with_timeout(Duration::from_secs(60)) {
            tracing::warn!(error = %e, "stopping embedded PostgreSQL");
        }
    }
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(
            parse_major("postgres (PostgreSQL) 16.10 (Ubuntu 16.10-1)\n"),
            Some(16)
        );
        assert_eq!(parse_major("postgres (PostgreSQL) 16.10"), Some(16));
        assert_eq!(parse_major("postgres (PostgreSQL) 18beta1"), Some(18));
        assert_eq!(parse_major("postgres (PostgreSQL) 17rc1"), Some(17));
    }

    #[test]
    fn restricts_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("pgdata");
        std::fs::create_dir_all(root.join("base/1")).unwrap();
        std::fs::write(root.join("base/1/file"), b"x").unwrap();
        std::fs::write(root.join("PG_VERSION"), b"16").unwrap();
        for p in [&root, &root.join("base"), &root.join("base/1")] {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o777)).unwrap();
        }
        std::fs::set_permissions(root.join("PG_VERSION"), std::fs::Permissions::from_mode(0o644)).unwrap();
        restrict_permissions(&root).unwrap();
        for p in ["", "base", "base/1", "base/1/file", "PG_VERSION"] {
            let mode = std::fs::metadata(root.join(p)).unwrap().permissions().mode() & 0o7777;
            assert_eq!(mode, 0o700, "{p}");
        }
    }

    #[test]
    fn encodes() {
        assert_eq!(percent_encode("/tmp/a b"), "%2Ftmp%2Fa%20b");
    }
}
