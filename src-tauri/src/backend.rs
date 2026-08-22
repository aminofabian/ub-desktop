//! JVM child-process management for the Spring Boot bootJar.

use std::io;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use crate::supervisor::{BackendConfig, ChildProcess};

const HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(750);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(45);
/// First-launch cap: MariaDB datadir init + 220+ Flyway migrations + JVM boot
/// can take several minutes on a modest Windows box (antivirus real-time
/// scanning slows the first run a lot). 90 s was routinely too short and made
/// fresh installs fail with "Backend never reported healthy".
const HEALTH_TIMEOUT: Duration = Duration::from_secs(300);

pub fn start(cfg: &BackendConfig) -> io::Result<ChildProcess> {
    // Windows: `Path::canonicalize()` returns `\\?\C:\…` extended-length
    // paths, which Spring Boot's nested-jar loader (JarLauncher) cannot open —
    // the JVM dies with ClassNotFoundException before main() runs. Normalize
    // to plain `C:\…` paths for everything the JVM sees (the exe itself and
    // the -jar argument). No-op on macOS/Linux.
    let java_bin = dunce::simplified(&cfg.java_bin).to_path_buf();
    let jar_path = dunce::simplified(&cfg.jar_path).to_path_buf();

    if !jar_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("desktop bootJar missing at {}", jar_path.display()),
        ));
    }

    log::info!(
        "Starting JVM ({} -jar {})",
        java_bin.display(),
        jar_path.display()
    );

    // LAN binding is controlled via a marker file written by
    // `DesktopLanService` (see DESKTOP_INSTALLATION.md §11). Spring cannot
    // rebind its server socket at runtime, so we pick the bind address on
    // startup.
    let lan_enabled_file = cfg.app_data.join("conf").join("lan-enabled");
    let bind_env = match std::env::var("APP_DESKTOP_BIND") {
        Ok(v) => Some(v),
        Err(_) => Some(if lan_enabled_file.exists() {
            "0.0.0.0".to_string()
        } else {
            "127.0.0.1".to_string()
        }),
    };
    let mut command = Command::new(&java_bin);
    // Optional remote log reporting for support (Super Admin → Platform →
    // Logs): forward the shell's ingest key / URL to the JVM reporter so an
    // operator can enable it without code changes. Blank = reporter stays
    // inert inside the JVM.
    if let Ok(v) = std::env::var("APP_DESKTOP_LOG_INGEST_KEY") {
        command.env("APP_DESKTOP_LOG_INGEST_KEY", v);
    }
    if let Ok(v) = std::env::var("APP_DESKTOP_LOG_REPORTING_URL") {
        command.env("APP_DESKTOP_LOG_REPORTING_URL", v);
    }
    // Windows: java.exe is a console-subsystem binary; without this the GUI
    // shell (no console to inherit) would pop a blank console window per JVM
    // launch. javaw.exe is GUI, but java.exe keeps the log redirection below.
    #[cfg(windows)]
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let child = command
        // Cap heap so an undertested feature can't OOM the cashier's box.
        // 512 MiB is plenty for single-store usage; tune in §10 if needed.
        .arg("-Xmx1024m")
        .arg("-jar")
        .arg(&jar_path)
        // Honour the desktop profile + bind the configured port.
        .arg(format!("--spring.profiles.active={}", cfg.profile))
        .arg(format!("--server.port={}", cfg.port))
        // DB creds go through env vars, not CLI args: a POS password on the
        // process command line is visible to any local user via Task Manager
        // (`wmic process get commandline`). Spring's relaxed binding maps
        // SPRING_DATASOURCE_* onto spring.datasource.*, overriding the values
        // baked into application-desktop.properties.
        //
        // connectionCollation must match the `ub` database collation
        // (`CREATE DATABASE ub … COLLATE utf8mb4_unicode_ci` in mariadb.rs).
        // Without it the MariaDB Connector/J default (utf8mb4_general_ci)
        // collides with the utf8mb4_unicode_ci columns on any comparison
        // against a session variable (e.g. migration V123's `= @catalog_id`)
        // and Flyway dies with error 1267.
        .env(
            "SPRING_DATASOURCE_URL",
            format!(
                "jdbc:mariadb://127.0.0.1:{}/ub?connectionCollation=utf8mb4_unicode_ci",
                cfg.mariadb_port
            ),
        )
        .env("SPRING_DATASOURCE_USERNAME", &cfg.mariadb_user)
        .env("SPRING_DATASOURCE_PASSWORD", &cfg.mariadb_password)
        // Make APP_DATA explicit so the JAR's
        // `app.media.local.dir=${APP_DATA:...}/media` resolves to the same
        // dir the shell uses. Set as env (not CLI) because property
        // expansion in `application.properties` reads env, not args.
        .env("APP_DATA", &cfg.app_data)
        .env("APP_DESKTOP_BIND", bind_env.unwrap())
        .env("APP_DESKTOP_DB_PORT", cfg.mariadb_port.to_string())
        .env("APP_DESKTOP_DB_USER", &cfg.mariadb_user)
        .env("APP_DESKTOP_DB_PASSWORD", &cfg.mariadb_password)
        .env("APP_JWT_SECRET", &cfg.jwt_secret)
        .env("APP_PAYMENTS_ENCRYPTION_KEY", &cfg.payment_encryption_key)
        // Drop stdio so the shell stays quiet; redirect to the log file
        // under APP_DATA so a user can grep when something goes sideways.
        .stdout(open_log(cfg, "backend.out.log")?)
        .stderr(open_log(cfg, "backend.err.log")?)
        .spawn()
        .map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "spawn java ({}) failed: {e}. Set APP_JAVA_BIN if java \
                     isn't on PATH.",
                    cfg.java_bin.display()
                ),
            )
        })?;

    log::info!("JVM pid={}", child.id());
    Ok(ChildProcess::new("backend (JVM)", child))
}

pub fn wait_healthy(
    port: u16,
    app_data: &Path,
    backend_state: &Mutex<Option<ChildProcess>>,
    abort: &AtomicBool,
) -> io::Result<()> {
    let url = format!("http://127.0.0.1:{port}/actuator/health");
    let deadline = Instant::now() + HEALTH_TIMEOUT;
    let mut last_err: Option<String> = None;
    while Instant::now() < deadline {
        // The user closed the app while we were still waiting — bail out so
        // the boot thread can clean up instead of polling into a dying app.
        if abort.load(Ordering::SeqCst) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "startup cancelled",
            ));
        }
        // If the JVM has already exited, don't wait out the full timeout —
        // fail fast so the error message (and the log files) are useful.
        if backend_exited(backend_state) {
            // Surface the JVM's own stderr/stdout tail in the error — the
            // whole point of the redirected logs is that the real reason
            // (Flyway failure, port in use, OOM…) is in there, and asking
            // the user to fetch them by hand usually ends the conversation.
            let detail = backend_log_tail(app_data, 8 * 1024, 40);
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!("the backend JVM exited before reporting healthy.{detail}"),
            ));
        }
        match ureq::get(&url).timeout(Duration::from_secs(2)).call() {
            Ok(resp) if resp.status() == 200 => {
                log::info!("/actuator/health → 200");
                return Ok(());
            }
            Ok(resp) => {
                last_err = Some(format!("status {}", resp.status()));
            }
            Err(e) => {
                last_err = Some(e.to_string());
            }
        }
        std::thread::sleep(HEALTH_POLL_INTERVAL);
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "/actuator/health never returned 200 within {:?} (last error: {})",
            HEALTH_TIMEOUT,
            last_err.unwrap_or_else(|| "—".to_string())
        ),
    ))
}

/// Tail of the JVM's own stderr/stdout logs (the files the shell redirects
/// the backend into under APP_DATA). Inlined into the boot error when the JVM
/// dies before becoming healthy, so the user's error dialog (and kiosk.log)
/// carry the real reason instead of a pointer to files that never get sent.
fn backend_log_tail(app_data: &Path, max_bytes: u64, max_lines: usize) -> String {
    let mut sections = String::new();
    for name in ["backend.err.log", "backend.out.log"] {
        let tail = read_tail(&app_data.join(name), max_bytes, max_lines);
        if tail.is_empty() {
            continue;
        }
        sections.push_str(&format!("\n--- tail of {name} ---\n{tail}\n"));
    }
    sections
}

/// Read up to `max_bytes` from the end of a file, keeping at most `max_lines`
/// whole lines. Returns "" when the file is missing/unreadable/empty.
fn read_tail(path: &Path, max_bytes: u64, max_lines: usize) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(meta) = std::fs::metadata(path) else {
        return String::new();
    };
    let len = meta.len();
    if len == 0 {
        return String::new();
    }
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let start = len.saturating_sub(max_bytes);
    if start > 0 && file.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    if file.take(max_bytes).read_to_end(&mut buf).is_err() {
        return String::new();
    }
    let text = String::from_utf8_lossy(&buf).to_string();
    let lines: Vec<&str> = text.lines().collect();
    let skip = lines.len().saturating_sub(max_lines);
    lines[skip..].join("\n")
}

/// True when the JVM child has exited (or was never stored). Locks briefly;
/// the guard is dropped before the caller polls, so this can't deadlock with
/// the shutdown path.
fn backend_exited(backend_state: &Mutex<Option<ChildProcess>>) -> bool {
    let guard: MutexGuard<'_, Option<ChildProcess>> = match backend_state.lock() {
        Ok(g) => g,
        Err(_) => return false,
    };
    let Some(proc) = guard.as_ref() else {
        // Not yet stored — keep polling; the boot thread stores it right
        // after spawning.
        return false;
    };
    let mut child = match proc.child.lock() {
        Ok(c) => c,
        Err(_) => return false,
    };
    match child.try_wait() {
        Ok(Some(_)) => true,
        _ => false,
    }
}

/// Stop the JVM gracefully: ask Spring's actuator to shut down first (clean
/// — Flyway/Hibernate get to finish), then fall back to SIGTERM, then kill.
pub fn shutdown(proc: &ChildProcess, port: u16) {
    log::info!("Stopping {} (pid {})…", proc.label, proc.pid());

    if let Ok(mut guard) = proc.child.lock() {
        if !request_graceful_shutdown(port) {
            log::warn!("Falling back to signal for JVM pid {}…", proc.pid());
            let _ = sigterm(&mut guard);
        }
        if let Err(e) = wait_with_timeout(&mut guard, SHUTDOWN_TIMEOUT) {
            log::warn!("Wait for JVM exit failed: {e}");
        }
    }
}

/// Public for the failure path in `lib.rs` — `start` succeeded but the
/// health probe didn't, so we need to terminate the JVM before exiting.
/// Best-effort graceful shutdown first (the app may be up but slow), then
/// force.
pub fn terminate(proc: &ChildProcess, port: u16) {
    if let Ok(mut guard) = proc.child.lock() {
        if !request_graceful_shutdown(port) {
            let _ = sigterm(&mut guard);
        }
        let _ = wait_with_timeout(&mut guard, SHUTDOWN_TIMEOUT);
    }
}

/// POST /actuator/shutdown (enabled via `management.endpoint.shutdown.enabled`
/// in the desktop profile). Returns true when Spring accepted the request and
/// will exit on its own. On Windows this is the only clean JVM shutdown path
/// — TerminateProcess leaves the process with no chance to flush.
fn request_graceful_shutdown(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/actuator/shutdown");
    match ureq::post(&url).timeout(Duration::from_secs(5)).call() {
        Ok(resp) if resp.status() == 200 => {
            log::info!("POST {url} → 200; waiting for JVM to exit.");
            true
        }
        Ok(resp) => {
            log::warn!("POST {url} → {}; using signal fallback.", resp.status());
            false
        }
        Err(e) => {
            log::warn!("POST {url} failed ({e}); using signal fallback.");
            false
        }
    }
}

fn open_log(cfg: &BackendConfig, name: &str) -> io::Result<Stdio> {
    use std::fs::OpenOptions;
    let path = cfg.app_data.join(name);
    let file = OpenOptions::new().create(true).append(true).open(&path)?;
    Ok(Stdio::from(file))
}

#[cfg(unix)]
fn sigterm(child: &mut Child) -> io::Result<()> {
    let pid = child.id() as libc::pid_t;
    // SAFETY: SIGTERM to a known pid; no UB possible.
    let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn sigterm(child: &mut Child) -> io::Result<()> {
    // No SIGTERM on Windows. TerminateProcess is abrupt but the alternative
    // (waiting the full timeout for a JVM nobody told to stop) is worse.
    // TODO(Windows polish): GenerateConsoleCtrlEvent + a Job object so the
    // JVM gets a chance to flush before dying.
    child.kill()
}

fn wait_with_timeout(child: &mut Child, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait()? {
            Some(_) => return Ok(()),
            None => {
                if Instant::now() >= deadline {
                    log::warn!(
                        "Timeout waiting for JVM pid {} to exit; SIGKILL.",
                        child.id()
                    );
                    let _ = child.kill();
                    return child.wait().map(|_| ());
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    }
}
