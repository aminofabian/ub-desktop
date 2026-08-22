//! Paths to runtime assets shipped inside the Tauri bundle (`Resources/` on
//! macOS, `resources/` on Linux/Windows).

use std::env;
use std::path::{Path, PathBuf};

/// Platform executable name: `mariadbd` → `mariadbd.exe` on Windows.
pub fn exe_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

/// Directory containing `jre/`, `mariadb/`, `jar/`, and `config/`.
pub fn bundle_resources_dir() -> Option<PathBuf> {
    if let Ok(dir) = env::var("APP_DESKTOP_RESOURCES") {
        let p = PathBuf::from(dir);
        if p.exists() {
            return p.canonicalize().ok();
        }
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(exe_dir) = env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        candidates.push(exe_dir.join("../Resources"));
        candidates.push(exe_dir.join("../resources"));
        candidates.push(exe_dir.join("resources"));
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Resources"));

    for c in candidates {
        if !c.exists() {
            continue;
        }
        let has_marker = c.join("jar").exists()
            || c.join("jre").exists()
            || c.join("mariadb").exists();
        if has_marker {
            return c.canonicalize().ok().or(Some(c));
        }
    }
    None
}

/// `Resources/jre/bin/java` (or `.exe` on Windows), else `java` on PATH.
pub fn resolve_java_bin() -> PathBuf {
    if let Ok(p) = env::var("APP_JAVA_BIN") {
        return PathBuf::from(p);
    }
    if let Some(bundle) = bundle_resources_dir() {
        let java = java_executable_in(bundle.join("jre"));
        if java.exists() {
            log::info!("Using bundled JRE: {}", java.display());
            return java;
        }
    }
    PathBuf::from("java")
}

/// Directory containing `mariadbd`, `mariadb`, `mariadb-install-db`, etc.
pub fn resolve_mariadb_bin_dir() -> PathBuf {
    if let Ok(p) = env::var("APP_MARIADB_BIN_DIR") {
        return PathBuf::from(p);
    }
    if let Some(bundle) = bundle_resources_dir() {
        let root = bundle.join("mariadb");
        if let Some(bin) = mariadb_bin_dir_under(&root) {
            log::info!("Using bundled MariaDB: {}", bin.display());
            return bin;
        }
    }
    default_system_mariadb_bin_dir()
}

/// `lib/` next to bundled `mariadbd` for dynamic loader paths.
pub fn mariadb_lib_dir(bin_dir: &Path) -> Option<PathBuf> {
    if bin_dir.ends_with("bin") {
        let lib = bin_dir.parent()?.join("lib");
        if lib.is_dir() {
            return Some(lib);
        }
    }
    let sibling = bin_dir.join("../lib");
    if sibling.is_dir() {
        return sibling.canonicalize().ok();
    }
    None
}

fn mariadb_bin_dir_under(root: &Path) -> Option<PathBuf> {
    let daemon = exe_name("mariadbd");
    let in_bin = root.join("bin");
    if in_bin.join(&daemon).exists() {
        return in_bin.canonicalize().ok().or(Some(in_bin));
    }
    if root.join(&daemon).exists() {
        return root.canonicalize().ok().or(Some(root.to_path_buf()));
    }
    None
}

fn java_executable_in(jre_root: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let exe = jre_root.join("bin/java.exe");
        if exe.exists() {
            return exe;
        }
    }
    jre_root.join("bin/java")
}

#[cfg(target_os = "macos")]
fn default_system_mariadb_bin_dir() -> PathBuf {
    let apple_silicon = PathBuf::from("/opt/homebrew/opt/mariadb@10.11/bin");
    if apple_silicon.join("mariadbd").exists() {
        return apple_silicon;
    }
    PathBuf::from("/usr/local/opt/mariadb@10.11/bin")
}

#[cfg(all(unix, not(target_os = "macos")))]
fn default_system_mariadb_bin_dir() -> PathBuf {
    PathBuf::from("/usr/bin")
}

#[cfg(windows)]
fn default_system_mariadb_bin_dir() -> PathBuf {
    PathBuf::from("mariadb/bin")
}
