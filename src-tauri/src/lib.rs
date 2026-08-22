//! Kiosk Desktop — Tauri shell.
//!
//! Lifecycle (see `DESKTOP_INSTALLATION.md` §5, §7, §16 step 5):
//!
//! 1. Resolve `APP_DATA` (per-OS app-support dir, overridable via env var).
//! 2. Open the window immediately with an offline splash page (`dist/`), so
//!    startup is never a silent, window-less wait — first launch (datadir
//!    init + 112 Flyway migrations) can take minutes.
//! 3. On a background thread: start MariaDB on `127.0.0.1:<APP_DESKTOP_DB_PORT>`
//!    (default 33306), then spawn the Spring Boot JAR with
//!    `SPRING_PROFILES_ACTIVE=desktop` and the discovered DB creds, polling
//!    `/actuator/health` until 200 (90 s timeout).
//! 4. Once healthy, navigate the window to
//!    `http://127.0.0.1:<APP_DESKTOP_BACKEND_PORT>/`; on failure, the splash
//!    page renders the error in place (Windows also gets a native alert).
//! 5. On window close, ask Spring's actuator to shut down, `mysqladmin
//!    shutdown` MariaDB, wait briefly for each, then exit.
//!
//! A second launch of the app is intercepted by tauri-plugin-single-instance
//! and forwarded to the running instance instead of dying on the fixed ports.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Manager, RunEvent, Url, WebviewUrl, WebviewWindowBuilder};

mod backend;
mod bundle;
mod devices;
mod mariadb;
mod supervisor;

use supervisor::{ChildProcess, Supervisor};

/// Shared lifecycle state stashed in Tauri's managed-state container so the
/// `RunEvent::ExitRequested` handler can reach the child processes. Shared
/// with the startup thread via `Arc`, so the boot path stores each child in
/// here the moment it is spawned and the exit path shuts down whatever is
/// present (no double-shutdown: both sides use `take()`).
pub struct AppState {
    /// `None` until the JVM is spawned, then `Some(_)` until shutdown.
    pub backend: Mutex<Option<ChildProcess>>,
    /// `None` until `mariadbd` is up, then `Some(_)` until shutdown.
    pub mariadb: Mutex<Option<ChildProcess>>,
    /// Set when the user closes the app while startup is still in flight so
    /// the boot thread bails out at its next checkpoint instead of spawning
    /// more processes into a dying app.
    pub abort: AtomicBool,
}

pub fn run() {
    // ---- 1. Resolve APP_DATA first so logging has a home ------------------
    // (Supervisor::discover also creates the directory on first run.)
    let supervisor = Supervisor::discover()
        .unwrap_or_else(|e| fatal(&format!("Failed to resolve APP_DATA: {e}")));

    // File logger: stderr alone is invisible in Windows release builds (no
    // console), so every startup failure looked like "the app won't open".
    // Everything is mirrored to kiosk.log under APP_DATA for support.
    init_logging(supervisor.app_data());

    log::info!("Kiosk Desktop starting (v{}).", env!("CARGO_PKG_VERSION"));
    log::info!("APP_DATA: {}", supervisor.app_data().display());

    // ---- 2. Build the app + splash window FIRST ---------------------------
    // The window shows a loading page immediately; MariaDB/JVM boot runs on
    // a background thread so the event loop (and the splash animation) keep
    // running while first launch does its minutes of database setup.
    let supervisor = Arc::new(supervisor);
    let state = Arc::new(AppState {
        backend: Mutex::new(None),
        mariadb: Mutex::new(None),
        abort: AtomicBool::new(false),
    });

    let supervisor_for_boot = Arc::clone(&supervisor);
    let state_for_boot = Arc::clone(&state);
    let supervisor_for_event = Arc::clone(&supervisor);

    tauri::Builder::default()
        // Second launch → focus the running instance instead of double-booting.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_focus();
            }
        }))
        .manage(Arc::clone(&state))
        .setup(move |app| {
            // Main window starts on the offline splash page (`desktop/dist/`);
            // the boot thread navigates it to the backend URL when healthy.
            let window = WebviewWindowBuilder::new(
                app,
                "main",
                WebviewUrl::App("index.html".into()),
            )
            .title("Kiosk")
            .inner_size(1280.0, 800.0)
            .min_inner_size(1024.0, 700.0)
            .resizable(true)
            .decorations(true)
            .visible(true)
            .build()?;
            window.set_focus().ok();

            let handle = app.handle().clone();
            std::thread::spawn(move || {
                boot_stack(supervisor_for_boot, state_for_boot, handle)
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(move |app_handle, event| {
            if let RunEvent::ExitRequested { .. } = event {
                let state = app_handle.state::<Arc<AppState>>();
                state.abort.store(true, Ordering::SeqCst);
                shutdown_children(state.inner(), &supervisor_for_event);
            }
        });
}

/// Start MariaDB + the JVM off the main thread. Stores each child in
/// [`AppState`] the moment it is spawned; on success navigates the window to
/// the backend URL, on failure cleans up and surfaces the error on the splash
/// page (plus a native alert on Windows).
fn boot_stack(supervisor: Arc<Supervisor>, state: Arc<AppState>, app: AppHandle) {
    // ESC/POS bridge runs on a sidecar thread. Wait briefly for its first
    // bind outcome so a busy port (typically a stray instance from a previous
    // session) is reported loudly instead of a silently missing printer
    // bridge. The thread keeps retrying in the background while the stack
    // boots (devices.rs `BIND_RETRY_WINDOW`).
    let device_status = devices::start_device_server(supervisor.app_data().to_path_buf());
    match devices::wait_for_bridge_bind(&device_status, Duration::from_secs(2)) {
        devices::BridgeStatus::Ready => {}
        devices::BridgeStatus::Failed(msg) => log::error!("Device bridge unavailable: {msg}"),
        devices::BridgeStatus::Starting => log::warn!(
            "Device bridge (port {}) is busy — another instance may be running; \
             retrying in the background.",
            devices::DEVICE_PORT
        ),
    }

    let boot = (|| -> Result<(), BootError> {
        // Fail fast instead of the 90-second death march: if the backend port
        // is already answering, a previous/stray instance is alive and holds
        // the ports. Booting MariaDB + another JVM then just cascades (second
        // mariadbd can't bind 33306, the JVM exits on 5050, and the shell
        // waits for a health check that never comes before tearing everything
        // down again).
        if port_is_open("127.0.0.1", supervisor.backend_port()) {
            return Err(BootError::failed(format!(
                "Port {} is already in use — another Kiosk Desktop instance is running. \
                 Close it (system tray / Task Manager), or kill stray kiosk-desktop, \
                 java and mariadbd processes, then relaunch.",
                supervisor.backend_port()
            )));
        }
        let mariadb = supervisor
            .start_mariadb()
            .map_err(|e| BootError::failed(format!("MariaDB failed to start: {e}")))?;
        if aborted(&state) {
            mariadb::shutdown(&mariadb, supervisor.mariadb_config());
            return Err(BootError::cancelled());
        }
        state.mariadb.lock().unwrap().replace(mariadb);

        let backend = supervisor
            .start_backend()
            .map_err(|e| BootError::failed(format!("Backend JVM failed to start: {e}")))?;
        if aborted(&state) {
            if let Some(mariadb) = state.mariadb.lock().unwrap().take() {
                mariadb::shutdown(&mariadb, supervisor.mariadb_config());
            }
            backend::terminate(&backend, supervisor.backend_port());
            return Err(BootError::cancelled());
        }
        state.backend.lock().unwrap().replace(backend);

        supervisor
            .wait_backend_healthy(&state.backend, &state.abort)
            .map_err(|e| {
                if aborted(&state) {
                    BootError::cancelled()
                } else {
                    BootError::failed(format!("Backend never reported healthy: {e}"))
                }
            })?;
        Ok(())
    })();

    match boot {
        Ok(()) => {
            let url = supervisor.backend_url();
            log::info!("Backend healthy at {url}. Navigating main window.");
            if let Some(window) = app.get_webview_window("main") {
                if let Ok(parsed) = Url::parse(&url) {
                    let _ = window.navigate(parsed);
                }
            }
        }
        Err(err) => {
            // Clean up whatever is still under our control (no-op for things
            // the exit handler already `take()`n).
            if let Some(backend) = state.backend.lock().unwrap().take() {
                backend::terminate(&backend, supervisor.backend_port());
            }
            if let Some(mariadb) = state.mariadb.lock().unwrap().take() {
                mariadb::shutdown(&mariadb, supervisor.mariadb_config());
            }

            if err.cancelled {
                log::info!("Startup cancelled by exit request.");
                return;
            }

            log::error!("Startup failed: {}", err.message);
            let mut display = format!(
                "{}\n\nLog file: {}/kiosk.log",
                err.message,
                supervisor.app_data().display()
            );
            // If the printer/cash-drawer bridge also failed to come up, the
            // most likely cause is a stray instance from a previous session —
            // surface that next to the boot error so it can't be missed.
            if let Some(note) = device_bridge_note(&device_status) {
                display.push_str(&format!("\n\nDevice bridge: {note}"));
            }
            // Navigate the splash back to itself with `?error=…` so the page
            // renders the failure (works on every platform — no IPC needed).
            if let Some(window) = app.get_webview_window("main") {
                if let Ok(mut url) = window.url() {
                    url.query_pairs_mut()
                        .clear()
                        .append_pair("error", &display);
                    let _ = window.navigate(url);
                }
            }
            // Windows: also pop a native alert so the failure can't be missed.
            #[cfg(windows)]
            show_error_dialog("Kiosk Desktop", &display);
        }
    }
}

fn aborted(state: &AppState) -> bool {
    state.abort.load(Ordering::SeqCst)
}

/// Human-readable note about the device bridge for the splash error surface;
/// `None` when the bridge is up (no need to distract the user).
fn device_bridge_note(status: &Mutex<devices::BridgeStatus>) -> Option<String> {
    let state = status.lock().unwrap();
    match &*state {
        devices::BridgeStatus::Ready => None,
        devices::BridgeStatus::Failed(msg) => Some(msg.clone()),
        devices::BridgeStatus::Starting => Some(
            "Port 19500 (printer / cash-drawer bridge) is busy — another Kiosk \
             instance may be running. Close it, or kill the stray kiosk-desktop \
             process in Task Manager, then relaunch."
                .to_string(),
        ),
    }
}

struct BootError {
    message: String,
    cancelled: bool,
}

impl BootError {
    fn failed(message: String) -> Self {
        Self {
            message,
            cancelled: false,
        }
    }

    fn cancelled() -> Self {
        Self {
            message: String::new(),
            cancelled: true,
        }
    }
}

fn shutdown_children(state: &Arc<AppState>, supervisor: &Supervisor) {
    log::info!("ExitRequested — shutting down children.");
    if let Some(backend) = state.backend.lock().unwrap().take() {
        backend::shutdown(&backend, supervisor.backend_port());
    }
    if let Some(mariadb) = state.mariadb.lock().unwrap().take() {
        mariadb::shutdown(&mariadb, supervisor.mariadb_config());
    }
    log::info!("Goodbye.");
}

fn fatal(msg: &str) -> ! {
    log::error!("{msg}");
    eprintln!("[kiosk-desktop] FATAL: {msg}");
    // Windows release builds have no console, so the user would otherwise see
    // nothing — show a native message box before exiting.
    #[cfg(windows)]
    show_error_dialog("Kiosk Desktop", msg);
    std::process::exit(1);
}

/// Native error dialog (Windows only). Called before the Tauri app is built,
/// so tauri-plugin-dialog can't help here — plain MessageBoxW is enough.
#[cfg(windows)]
fn show_error_dialog(title: &str, message: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONERROR, MB_OK,
    };
    let title_wide: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
    let message_wide: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
    // SAFETY: valid wide strings + a null owner HWND; the call is synchronous
    // and doesn't touch our memory after returning.
    unsafe {
        MessageBoxW(
            0, // null owner window handle (windows-sys 0.52 uses isize handles)
            message_wide.as_ptr(),
            title_wide.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

// ---------------------------------------------------------------------------
// Logging — timestamped lines to APP_DATA/kiosk.log, mirrored to stderr so
// `cargo run` terminals keep working unchanged.
// ---------------------------------------------------------------------------

struct FileLogger {
    file: Mutex<Option<File>>,
}

impl FileLogger {
    fn new(app_data: &Path) -> Self {
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(app_data.join("kiosk.log"))
        {
            Ok(file) => Self {
                file: Mutex::new(Some(file)),
            },
            Err(e) => {
                // Never hard-fail logging setup — a broken log file shouldn't
                // stop the app from starting. stderr mirror still works.
                eprintln!("[kiosk-desktop] could not open kiosk.log: {e}");
                Self {
                    file: Mutex::new(None),
                }
            }
        }
    }
}

impl log::Log for FileLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!(
            "[{} {:5} {}] {}\n",
            format_unix_ts(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            ),
            record.level(),
            record.target(),
            record.args()
        );
        if let Ok(mut file) = self.file.lock() {
            if let Some(f) = file.as_mut() {
                let _ = f.write_all(line.as_bytes());
                let _ = f.flush();
            }
        }
        eprint!("{line}");
    }

    fn flush(&self) {
        if let Ok(mut file) = self.file.lock() {
            if let Some(f) = file.as_mut() {
                let _ = f.flush();
            }
        }
    }
}

fn init_logging(app_data: &Path) {
    log::set_boxed_logger(Box::new(FileLogger::new(app_data))).expect("set logger");
    log::set_max_level(max_log_level());
}

/// Rough RUST_LOG compat: "error"/"warn"/"debug"/"trace" pick a level,
/// anything else defaults to INFO (same default the old env_logger config had).
fn max_log_level() -> log::LevelFilter {
    match std::env::var("RUST_LOG").unwrap_or_default().to_lowercase() {
        s if s.contains("trace") => log::LevelFilter::Trace,
        s if s.contains("debug") => log::LevelFilter::Debug,
        s if s.contains("warn") => log::LevelFilter::Warn,
        s if s.contains("error") => log::LevelFilter::Error,
        _ => log::LevelFilter::Info,
    }
}

/// Whether anything is already accepting TCP connections on the given port.
/// Used as a fail-fast boot pre-flight: when the backend port answers before
/// we have started anything, a previous/stray instance is holding it —
/// starting MariaDB + the JVM again would just cascade.
fn port_is_open(host: &str, port: u16) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    let Ok(mut addrs) = format!("{host}:{port}").to_socket_addrs() else {
        return false;
    };
    let Some(addr) = addrs.next() else {
        return false;
    };
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// UTC "YYYY-MM-DDTHH:MM:SSZ" from a unix timestamp, without pulling in a
/// chrono/time dependency. Civil-from-days is Howard Hinnant's public-domain
/// algorithm.
fn format_unix_ts(secs: u64) -> String {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (hh, mm, ss) = (rem / 3_600, (rem % 3_600) / 60, rem % 60);
    let z = days as i64 + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}
