//! Lifecycle wiring + environment discovery.
//!
//! `Supervisor` is the orchestrator: it owns the resolved paths, ports and
//! credentials and exposes start/stop primitives for each child. Splitting
//! the concrete `mariadb` / `backend` modules behind this façade keeps `lib.rs`
//! readable and makes the contracts explicit.

use std::env;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::AtomicBool;

use base64::{engine::general_purpose::STANDARD, Engine};
use rand::distributions::{Alphanumeric, DistString};
use rand::RngCore;

use crate::{backend, bundle, mariadb};

/// Default backend HTTP port — matches the value baked into the JAR's
/// `application-desktop.properties` `server.address` so the webview URL is
/// predictable. Overridable via `APP_DESKTOP_BACKEND_PORT`.
const DEFAULT_BACKEND_PORT: u16 = 5050;
/// Default MariaDB port — matches DESKTOP_INSTALLATION.md §7.
const DEFAULT_MARIADB_PORT: u16 = 33306;

/// Wraps a [`Child`] alongside a label so the shutdown path can log who's
/// being terminated without re-stating the pid in every caller.
pub struct ChildProcess {
    pub label: &'static str,
    pub child: std::sync::Mutex<Child>,
}

impl ChildProcess {
    pub fn new(label: &'static str, child: Child) -> Self {
        Self {
            label,
            child: std::sync::Mutex::new(child),
        }
    }

    pub fn pid(&self) -> u32 {
        self.child.lock().unwrap().id()
    }
}

/// Snapshot of everything the supervisor needs to talk to MariaDB. Kept
/// `Clone` so callers (start, shutdown, ensure_schema) can pass it around
/// without re-deriving paths.
#[derive(Clone)]
pub struct MariadbConfig {
    pub bin_dir: PathBuf,
    /// Shared libraries for bundled `mariadbd` (DYLD / LD_LIBRARY_PATH);
    /// unused on Windows, which loads DLLs from the exe's own directory.
    #[cfg_attr(windows, allow(dead_code))]
    pub lib_dir: Option<PathBuf>,
    pub datadir: PathBuf,
    /// Unix socket for the root admin channel; unused on Windows (TCP there).
    #[cfg_attr(windows, allow(dead_code))]
    pub socket: PathBuf,
    pub pid_file: PathBuf,
    pub log_file: PathBuf,
    pub port: u16,
    pub user: String,
    pub password: String,
    /// `true` when the user opted out via `APP_DESKTOP_SKIP_MARIADB=1` and
    /// we're attaching to an externally-managed `mariadbd` (no extraction,
    /// no datadir init, no shutdown on exit).
    pub external: bool,
}

#[derive(Clone)]
pub struct BackendConfig {
    pub jar_path: PathBuf,
    pub java_bin: PathBuf,
    pub port: u16,
    pub profile: String,
    pub app_data: PathBuf,
    pub mariadb_port: u16,
    pub mariadb_user: String,
    pub mariadb_password: String,
    pub jwt_secret: String,
    pub payment_encryption_key: String,
}

pub struct Supervisor {
    app_data: PathBuf,
    mariadb: MariadbConfig,
    backend: BackendConfig,
}

impl Supervisor {
    /// Resolve everything from env vars + sensible defaults. Pure function —
    /// no I/O until [`Self::start_mariadb`] / [`Self::start_backend`] are
    /// called.
    pub fn discover() -> io::Result<Self> {
        let app_data = resolve_app_data()?;
        std::fs::create_dir_all(&app_data)?;

        let mariadb = MariadbConfig::resolve(&app_data)?;
        let backend = BackendConfig::resolve(&app_data, &mariadb)?;

        Ok(Self {
            app_data,
            mariadb,
            backend,
        })
    }

    pub fn app_data(&self) -> &Path {
        &self.app_data
    }

    pub fn backend_url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.backend.port)
    }

    pub fn backend_port(&self) -> u16 {
        self.backend.port
    }

    pub fn mariadb_config(&self) -> &MariadbConfig {
        &self.mariadb
    }

    pub fn start_mariadb(&self) -> io::Result<ChildProcess> {
        mariadb::start(&self.mariadb)
    }

    pub fn start_backend(&self) -> io::Result<ChildProcess> {
        backend::start(&self.backend)
    }

    pub fn wait_backend_healthy(
        &self,
        backend_state: &std::sync::Mutex<Option<ChildProcess>>,
        abort: &AtomicBool,
    ) -> io::Result<()> {
        backend::wait_healthy(
            self.backend.port,
            &self.app_data,
            backend_state,
            abort,
        )
    }
}

// ---------------------------------------------------------------------------
// env / path resolution
// ---------------------------------------------------------------------------

/// Resolve the per-user app-data directory.
///
/// - `APP_DATA` env var (highest precedence) — used for throw-away tests.
/// - Per-OS app-support dir (macOS: `~/Library/Application Support/Palmart`,
///   Windows: `%APPDATA%/Palmart`, Linux: `$XDG_DATA_HOME/palmart`).
/// - Fall back to `~/.palmart` if the OS doesn't expose one.
///
/// <p>Backward compatible with earlier development builds that used
/// `Kiosk`/`~/.kiosk`.
fn resolve_app_data() -> io::Result<PathBuf> {
    if let Ok(p) = env::var("APP_DATA") {
        return Ok(PathBuf::from(p));
    }
    if let Some(base) = dirs::data_dir() {
        let primary = base.join("Palmart");
        let legacy = base.join("Kiosk");
        if primary.exists() {
            return Ok(primary);
        }
        if legacy.exists() {
            return Ok(legacy);
        }
        return Ok(primary);
    }
    if let Some(home) = dirs::home_dir() {
        let primary = home.join(".palmart");
        let legacy = home.join(".kiosk");
        if primary.exists() {
            return Ok(primary);
        }
        if legacy.exists() {
            return Ok(legacy);
        }
        return Ok(primary);
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no app-data dir candidate available",
    ))
}

impl MariadbConfig {
    fn resolve(app_data: &Path) -> io::Result<Self> {
        let bin_dir = bundle::resolve_mariadb_bin_dir();
        let lib_dir = bundle::mariadb_lib_dir(&bin_dir);

        if !bin_dir.join(bundle::exe_name("mariadbd")).exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "mariadbd not found under {}. Run \
                     desktop/scripts/prepare-tauri-resources.sh, install \
                     `brew install mariadb@10.11`, or set APP_MARIADB_BIN_DIR.",
                    bin_dir.display()
                ),
            ));
        }

        let port: u16 = parse_env_port("APP_DESKTOP_DB_PORT", DEFAULT_MARIADB_PORT)?;
        let external = env::var("APP_DESKTOP_SKIP_MARIADB")
            .ok()
            .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE"))
            .unwrap_or(false);

        let user = env::var("APP_DESKTOP_DB_USER").unwrap_or_else(|_| "ub_local".to_string());
        let password = resolve_or_generate_password(app_data)?;

        Ok(Self {
            bin_dir,
            lib_dir,
            datadir: app_data.join("db"),
            socket: app_data.join("mariadb.sock"),
            pid_file: app_data.join("mariadb.pid"),
            log_file: app_data.join("mariadb.log"),
            port,
            user,
            password,
            external,
        })
    }
}

impl BackendConfig {
    fn resolve(app_data: &Path, mariadb: &MariadbConfig) -> io::Result<Self> {
        let port: u16 = parse_env_port("APP_DESKTOP_BACKEND_PORT", DEFAULT_BACKEND_PORT)?;
        let profile = env::var("APP_DESKTOP_SPRING_PROFILE")
            .unwrap_or_else(|_| "desktop".to_string());
        let java_bin = bundle::resolve_java_bin();
        let jar_path = resolve_jar_path()?;

        let (jwt_secret, payment_encryption_key) = resolve_desktop_secrets(app_data)?;

        Ok(Self {
            jar_path,
            java_bin,
            port,
            profile,
            app_data: app_data.to_path_buf(),
            mariadb_port: mariadb.port,
            mariadb_user: mariadb.user.clone(),
            mariadb_password: mariadb.password.clone(),
            jwt_secret,
            payment_encryption_key,
        })
    }
}

fn parse_env_port(name: &str, default: u16) -> io::Result<u16> {
    match env::var(name) {
        Ok(v) => v.parse().map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name}={v:?} is not a valid u16: {e}"),
            )
        }),
        Err(_) => Ok(default),
    }
}

/// Resolve the desktop bootJar:
///
/// 1. `APP_DESKTOP_JAR` env var (highest precedence).
/// 2. Sibling `kiosk-desktop-*.jar` next to the executable (bundled .app).
/// 3. `../../../backend/build/libs/kiosk-desktop-*.jar` relative to the
///    cargo target dir (dev / `cargo run`).
fn resolve_jar_path() -> io::Result<PathBuf> {
    if let Ok(p) = env::var("APP_DESKTOP_JAR") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Ok(p);
        }
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("APP_DESKTOP_JAR={} does not exist", p.display()),
        ));
    }

    let exe_dir = env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));

    if let Some(bundle) = bundle::bundle_resources_dir() {
        let kiosk = bundle.join("jar/kiosk.jar");
        if kiosk.exists() {
            return Ok(kiosk);
        }
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(d) = &exe_dir {
        // Bundled .app: Contents/MacOS/<exe>  →  ../Resources/jar/*.jar.
        candidates.push(d.join("../Resources/jar"));
        // Side-by-side: <exe>/../jar (Linux / portable archive layout).
        candidates.push(d.join("jar"));
        // Dev: cargo's default target/<profile>/  →  ../../../backend/build/libs.
        candidates.push(d.join("../../../backend/build/libs"));
    }
    // Compile-time fallback. `CARGO_MANIFEST_DIR` is embedded at build time
    // and survives cargo's `target-dir` redirections (e.g. the cursor sandbox
    // puts target/ under /var/folders, breaking exe-relative resolution).
    // From `desktop/src-tauri/`, the backend libs sit two dirs up.
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../backend/build/libs"));

    for dir in &candidates {
        if let Some(found) = first_jar_in(dir, "kiosk-desktop") {
            return Ok(found);
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "kiosk-desktop-*.jar not found. Build it with `cd backend && \
             ./gradlew bootJar -Pdesktop=true`, or set APP_DESKTOP_JAR. \
             Searched: {}",
            candidates
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    ))
}

fn first_jar_in(dir: &Path, prefix: &str) -> Option<PathBuf> {
    let canonical = dir.canonicalize().ok()?;
    let read = std::fs::read_dir(&canonical).ok()?;
    let mut matches: Vec<_> = read
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            name.starts_with(prefix) && name.ends_with(".jar")
        })
        .collect();
    matches.sort();
    matches.last().cloned()
}

/// Read the per-install MariaDB password from `APP_DATA/db_password`, or
/// generate one on first run. Mirrors the doc's recipe (random 32-char base
/// + write to `APP_DATA/conf/db.env`), but stored as a single bare file so
/// the dev / smoke-test story stays simple. The file is `chmod 600` on Unix.
/// Per-install JWT signing secret and payment-field encryption key (§15).
fn resolve_desktop_secrets(app_data: &Path) -> io::Result<(String, String)> {
    let conf = app_data.join("conf");
    std::fs::create_dir_all(&conf)?;

    let jwt = resolve_or_generate_secret_file(
        &conf.join("jwt.key"),
        64,
        "APP_JWT_SECRET",
    )?;

    // Spring expects `app.payments.encryption-key` to be a base64-encoded
    // 256-bit key (32 bytes) for AES-256-GCM.
    let payment = resolve_or_generate_payment_encryption_key(&conf.join("payment.key"))?;
    Ok((jwt, payment))
}

fn resolve_or_generate_secret_file(
    path: &Path,
    len: usize,
    env_name: &str,
) -> io::Result<String> {
    if let Ok(v) = env::var(env_name) {
        if !v.is_empty() {
            return Ok(v);
        }
    }
    if path.exists() {
        return Ok(std::fs::read_to_string(path)?.trim().to_string());
    }
    let secret = Alphanumeric.sample_string(&mut rand::thread_rng(), len);
    std::fs::write(path, &secret)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(secret)
}

fn resolve_or_generate_payment_encryption_key(path: &Path) -> io::Result<String> {
    // Validate env var first so ops can pin a deterministic key.
    if let Ok(v) = env::var("APP_PAYMENTS_ENCRYPTION_KEY") {
        if is_valid_base64_256_key(&v) {
            return Ok(v.trim().to_string());
        }
        log::warn!(
            "Ignoring invalid APP_PAYMENTS_ENCRYPTION_KEY (must be base64(32 bytes)). Regenerating."
        );
    }

    // If we already generated a key, validate it; older dev builds stored
    // an alphanumeric string here, which crashes Spring on startup.
    if path.exists() {
        let existing = std::fs::read_to_string(path)?.trim().to_string();
        if is_valid_base64_256_key(&existing) {
            return Ok(existing);
        }
        log::warn!(
            "Overwriting invalid {} (must be base64(32 bytes)). Regenerating.",
            path.display()
        );
    }

    let mut key_bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut key_bytes);
    let encoded = STANDARD.encode(key_bytes);
    std::fs::write(path, &encoded)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }

    Ok(encoded)
}

fn is_valid_base64_256_key(s: &str) -> bool {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return false;
    }
    // Rust base64 decoder tolerates both padded/unpadded input; we only
    // care about decoded length (32 bytes).
    match STANDARD.decode(trimmed) {
        Ok(bytes) => bytes.len() == 32,
        Err(_) => false,
    }
}

fn resolve_or_generate_password(app_data: &Path) -> io::Result<String> {
    if let Ok(p) = env::var("APP_DESKTOP_DB_PASSWORD") {
        return Ok(p);
    }
    let pw_file = app_data.join("db_password");
    if pw_file.exists() {
        return Ok(std::fs::read_to_string(&pw_file)?.trim().to_string());
    }
    let pw = Alphanumeric.sample_string(&mut rand::thread_rng(), 32);
    std::fs::write(&pw_file, &pw)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&pw_file)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&pw_file, perms)?;
    }
    Ok(pw)
}
