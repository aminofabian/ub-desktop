//! Local HTTP bridge for ESC/POS printers and cash drawers (DESKTOP_INSTALLATION.md §5.6).
//!
//! Listens on `127.0.0.1:19500`. Accepts raw ESC/POS on `POST /print` and forwards to
//! CUPS (`X-Printer-Cups-Name`), network :9100, or a spool file. Cloud cashier in a
//! browser can also POST here (CORS enabled).
//!
//! If the port is busy (typically a stray instance from a previous session), the
//! thread keeps retrying for [`BIND_RETRY_WINDOW`] and shares the outcome through
//! a [`BridgeStatus`] handle so the boot path can warn the user instead of the
//! bridge dying silently.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;

pub const DEVICE_PORT: u16 = 19500;

/// How long the bridge keeps retrying to bind before giving up. A previous
/// instance that is still winding down (or a stray process) holds the port;
/// killing it mid-retry lets this instance take over without a restart.
const BIND_RETRY_WINDOW: Duration = Duration::from_secs(30);
/// Delay between bind attempts while the port is busy.
const BIND_RETRY_INTERVAL: Duration = Duration::from_millis(750);

/// Bind outcome of the device bridge, shared with the boot path.
#[derive(Clone, PartialEq)]
pub enum BridgeStatus {
    /// First bind attempt hasn't settled yet (or the port is busy and the
    /// thread is still retrying in the background).
    Starting,
    /// The bridge is listening.
    Ready,
    /// The bridge gave up; the string explains why (and what to do).
    Failed(String),
}

/// Standard ESC/POS pulse — drawer kick pin 2 then pin 5.
const DRAWER_KICK: &[u8] = &[
    0x1b, 0x70, 0x00, 0x19, 0xfa, // pin 2
    0x1b, 0x70, 0x01, 0x19, 0xfa, // pin 5
];

#[derive(Debug, Deserialize)]
struct PrinterConfig {
    #[serde(default = "default_mode")]
    mode: String,
    #[serde(default)]
    host: String,
    #[serde(default = "default_port")]
    port: u16,
    #[serde(default)]
    path: String,
    /// CUPS / system queue name when `mode` is `cups`.
    #[serde(default, alias = "cupsName")]
    cups_name: String,
}

fn default_mode() -> String {
    "file".to_string()
}

fn default_port() -> u16 {
    9100
}

/// Spawn the ESC/POS bridge thread and return a handle to its bind status.
/// Non-blocking: the boot path calls [`wait_for_bridge_bind`] with a short
/// timeout instead of blocking on the full retry window.
///
/// `restart_flag` is shared with the shell's watchdog: `POST /restart` sets it,
/// and the supervisor then tears the stack down and boots it again.
pub fn start_device_server(
    app_data: PathBuf,
    restart_flag: Arc<AtomicBool>,
) -> Arc<Mutex<BridgeStatus>> {
    let status = Arc::new(Mutex::new(BridgeStatus::Starting));
    let status_for_thread = Arc::clone(&status);
    thread::spawn(move || {
        if let Err(e) = run_server(&app_data, &status_for_thread, &restart_flag) {
            log::error!("device server exited: {e}");
        }
    });
    status
}

/// Wait up to `timeout` for the first definite bind outcome. Returns
/// [`BridgeStatus::Starting`] when the port is busy and the thread is still
/// retrying in the background.
pub fn wait_for_bridge_bind(status: &Mutex<BridgeStatus>, timeout: Duration) -> BridgeStatus {
    let deadline = Instant::now() + timeout;
    loop {
        let state = status.lock().unwrap().clone();
        if state != BridgeStatus::Starting {
            return state;
        }
        if Instant::now() >= deadline {
            return BridgeStatus::Starting;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn run_server(
    app_data: &Path,
    status: &Mutex<BridgeStatus>,
    restart_flag: &AtomicBool,
) -> std::io::Result<()> {
    let addr = format!("127.0.0.1:{DEVICE_PORT}");
    let listener = bind_with_retry(&addr, status)?;
    log::info!("Device bridge listening on http://{addr}");

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                if let Err(e) = handle_client(s, app_data, restart_flag) {
                    log::warn!("device request failed: {e}");
                }
            }
            Err(e) => log::warn!("device accept error: {e}"),
        }
    }
    Ok(())
}

/// Bind the listener, retrying while the port is busy (a previous instance
/// may be winding down and about to release it). Gives up after
/// [`BIND_RETRY_WINDOW`] and records the failure on the shared status.
fn bind_with_retry(addr: &str, status: &Mutex<BridgeStatus>) -> std::io::Result<TcpListener> {
    let deadline = Instant::now() + BIND_RETRY_WINDOW;
    let mut warned = false;
    loop {
        match TcpListener::bind(addr) {
            Ok(listener) => {
                *status.lock().unwrap() = BridgeStatus::Ready;
                return Ok(listener);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                if !warned {
                    log::warn!(
                        "{addr} is busy — another Kiosk instance may be running; \
                         retrying for up to {}s",
                        BIND_RETRY_WINDOW.as_secs()
                    );
                    warned = true;
                }
                if Instant::now() >= deadline {
                    let msg = format!(
                        "could not bind {addr}: port already in use. Another Kiosk instance \
                         is likely running — close it, or kill the stray kiosk-desktop \
                         process in Task Manager, then relaunch. Printing and cash-drawer \
                         control stay unavailable until the port is free."
                    );
                    *status.lock().unwrap() = BridgeStatus::Failed(msg.clone());
                    return Err(std::io::Error::new(std::io::ErrorKind::AddrInUse, msg));
                }
                thread::sleep(BIND_RETRY_INTERVAL);
            }
            Err(e) => {
                *status.lock().unwrap() = BridgeStatus::Failed(e.to_string());
                return Err(e);
            }
        }
    }
}

fn handle_client(
    mut stream: TcpStream,
    app_data: &Path,
    restart_flag: &AtomicBool,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let (method, path, body, headers) = read_http_request(&mut stream)?;

    if method == "OPTIONS" {
        return write_response(&mut stream, 204, "");
    }

    match (method.as_str(), path.as_str()) {
        ("GET", "/health") | ("GET", "/health/") => {
            let platform = health_platform();
            let has_lpstat = lpstat_bin().is_some();
            let json = format!(
                r#"{{"ok":true,"cups":{cups},"lpstat":{lpstat},"platform":"{platform}","port":{port}}}"#,
                cups = if has_lpstat { "true" } else { "false" },
                lpstat = if has_lpstat { "true" } else { "false" },
                platform = platform,
                port = DEVICE_PORT,
            );
            write_json_response(&mut stream, 200, &json)
        }
        ("GET", "/printers") | ("GET", "/printers/") => match list_cups_printers_json() {
            Ok(json) => write_json_response(&mut stream, 200, &json),
            Err(e) => {
                log::warn!("list printers failed: {e}");
                write_response(&mut stream, 500, &e.to_string())
            }
        },
        ("POST", "/print") | ("POST", "/print/") => {
            if body.is_empty() {
                return write_response(&mut stream, 400, "empty body");
            }
            if let Some(cups) = header_ci(&headers, "x-printer-cups-name") {
                if !is_valid_cups_name(&cups) {
                    return write_response(&mut stream, 400, "invalid CUPS printer name");
                }
                match send_to_cups(&cups, &body) {
                    Ok(()) => write_response(&mut stream, 200, "printed"),
                    Err(e) => {
                        log::warn!("cups print failed: {e}");
                        write_response(&mut stream, 500, &e.to_string())
                    }
                }
            } else {
                match send_to_printer(app_data, &body) {
                    Ok(()) => write_response(&mut stream, 200, "printed"),
                    Err(e) => {
                        log::warn!("print failed: {e}");
                        write_response(&mut stream, 500, &e.to_string())
                    }
                }
            }
        }
        ("POST", "/drawer/kick") | ("POST", "/drawer/kick/") => {
            if let Some(cups) = header_ci(&headers, "x-printer-cups-name") {
                if !is_valid_cups_name(&cups) {
                    return write_response(&mut stream, 400, "invalid CUPS printer name");
                }
                match send_to_cups(&cups, DRAWER_KICK) {
                    Ok(()) => write_response(&mut stream, 200, "drawer"),
                    Err(e) => {
                        log::warn!("cups drawer kick failed: {e}");
                        write_response(&mut stream, 500, &e.to_string())
                    }
                }
            } else {
                match send_to_printer(app_data, DRAWER_KICK) {
                    Ok(()) => write_response(&mut stream, 200, "drawer"),
                    Err(e) => write_response(&mut stream, 500, &e.to_string()),
                }
            }
        }
        // Settings → Desktop → "Restart backend". The flag is consumed by the
        // shell's health watchdog, which tears the stack down and boots it
        // again; the bridge itself keeps listening across the restart.
        ("POST", "/restart") | ("POST", "/restart/") => {
            restart_flag.store(true, Ordering::SeqCst);
            log::info!("Restart requested via the device bridge.");
            write_json_response(&mut stream, 202, r#"{"restarting":true}"#)
        }
        // Settings → Desktop → "Open data folder". Opens APP_DATA in the OS file
        // manager (the JVM is headless, so it cannot do this itself).
        ("POST", "/open-data-folder") | ("POST", "/open-data-folder/") => {
            match open_in_file_manager(app_data) {
                Ok(()) => write_json_response(&mut stream, 200, r#"{"opened":true}"#),
                Err(e) => {
                    log::warn!("open data folder failed: {e}");
                    write_json_response(
                        &mut stream,
                        500,
                        &format!(r#"{{"opened":false,"error":"{}"}}"#, e),
                    )
                }
            }
        }
        _ => write_response(&mut stream, 404, "not found"),
    }
}

/// Open `path` in the OS file manager. Detaches so a long-lived opener never
/// blocks the bridge thread; a failure to spawn is surfaced to the caller.
fn open_in_file_manager(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(target_os = "windows")]
    let program = "explorer";
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let program = "xdg-open";

    let mut cmd = Command::new(program);
    cmd.arg(path);
    // Windows: don't flash a console window for the opener.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd.spawn().map(|_| ())
}

fn header_ci(headers: &HashMap<String, String>, key: &str) -> Option<String> {
    headers.get(key).cloned()
}

fn health_platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "win32"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "unknown"
    }
}

fn lpstat_bin() -> Option<&'static str> {
    if std::path::Path::new("/usr/bin/lpstat").exists() {
        Some("/usr/bin/lpstat")
    } else if std::path::Path::new("/bin/lpstat").exists() {
        Some("/bin/lpstat")
    } else {
        None
    }
}

fn is_valid_cups_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 120
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

fn is_likely_thermal(name: &str, uri: &str) -> bool {
    let hay = format!("{name} {uri}").to_ascii_lowercase();
    const HINTS: &[&str] = &[
        "caysn",
        "xprinter",
        "x-printer",
        "epson",
        "tm-",
        "tm_",
        "star",
        "bixolon",
        "citizen",
        "pos-80",
        "pos80",
        "receipt",
        "thermal",
        "rongta",
        "gprinter",
        "munbyn",
        "rp58",
        "rp80",
        "xp-",
    ];
    HINTS.iter().any(|h| hay.contains(h))
}

fn is_noise_printer(name: &str, uri: &str) -> bool {
    let hay = format!("{name} {uri}").to_ascii_lowercase();
    const NOISE: &[&str] = &[
        "pdf",
        "fax",
        "airprint",
        "microsoft print to pdf",
        "onenote",
        "send to onenote",
        "microsoft xps",
    ];
    NOISE.iter().any(|n| hay.contains(n))
}

/// JSON shape matches the cloud Till Print Bridge `/printers` response.
fn list_cups_printers_json() -> std::io::Result<String> {
    let platform = health_platform();
    let Some(lpstat) = lpstat_bin() else {
        return Ok(format!(
            r#"{{"ok":true,"platform":"{platform}","printers":[],"suggested":null,"defaultName":null}}"#
        ));
    };

    let devices = Command::new(lpstat).arg("-v").output()?;
    let default_out = Command::new(lpstat).arg("-d").output().ok();
    let devices_text = String::from_utf8_lossy(&devices.stdout);
    let default_text = default_out
        .as_ref()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();

    let default_name = default_text
        .lines()
        .find_map(|line| {
            let lower = line.to_ascii_lowercase();
            let idx = lower.find("system default destination:")?;
            Some(line[idx + "system default destination:".len()..].trim().to_string())
        })
        .filter(|n| !n.is_empty());

    let mut printers: Vec<(String, String, bool, bool)> = Vec::new();
    for line in devices_text.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed
            .strip_prefix("device for ")
            .or_else(|| trimmed.strip_prefix("Device for "))
        else {
            continue;
        };
        let Some((name, uri)) = rest.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if !is_valid_cups_name(name) {
            continue;
        }
        let uri = uri.trim().to_string();
        let is_default = default_name.as_deref() == Some(name);
        let likely = is_likely_thermal(name, &uri) && !is_noise_printer(name, &uri);
        printers.push((name.to_string(), uri, is_default, likely));
    }

    printers.sort_by(|a, b| {
        b.3.cmp(&a.3)
            .then_with(|| {
                let a_noise = is_noise_printer(&a.0, &a.1);
                let b_noise = is_noise_printer(&b.0, &b.1);
                a_noise.cmp(&b_noise)
            })
            .then_with(|| b.2.cmp(&a.2))
            .then_with(|| a.0.cmp(&b.0))
    });

    let suggested = printers
        .iter()
        .find(|p| p.3)
        .or_else(|| {
            printers
                .iter()
                .find(|p| p.2 && !is_noise_printer(&p.0, &p.1))
        })
        .or_else(|| {
            printers
                .iter()
                .find(|p| !is_noise_printer(&p.0, &p.1))
        })
        .or_else(|| {
            if printers.len() == 1 {
                printers.first()
            } else {
                None
            }
        })
        .map(|p| p.0.clone());

    let mut arr = String::from("[");
    for (i, (name, uri, is_default, likely)) in printers.iter().enumerate() {
        if i > 0 {
            arr.push(',');
        }
        arr.push_str(&format!(
            r#"{{"name":{},"uri":{},"isDefault":{},"likelyThermal":{}}}"#,
            serde_json::to_string(name).unwrap_or_else(|_| "\"\"".into()),
            serde_json::to_string(uri).unwrap_or_else(|_| "\"\"".into()),
            if *is_default { "true" } else { "false" },
            if *likely { "true" } else { "false" },
        ));
    }
    arr.push(']');

    let suggested_json = suggested
        .as_ref()
        .map(|s| serde_json::to_string(s).unwrap_or_else(|_| "null".into()))
        .unwrap_or_else(|| "null".into());
    let default_json = default_name
        .as_ref()
        .map(|s| serde_json::to_string(s).unwrap_or_else(|_| "null".into()))
        .unwrap_or_else(|| "null".into());

    Ok(format!(
        r#"{{"ok":true,"platform":"{platform}","printers":{arr},"suggested":{suggested_json},"defaultName":{default_json}}}"#
    ))
}

fn send_to_cups(name: &str, data: &[u8]) -> std::io::Result<()> {
    let lp = if std::path::Path::new("/usr/bin/lp").exists() {
        "/usr/bin/lp"
    } else if std::path::Path::new("/bin/lp").exists() {
        "/bin/lp"
    } else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "CUPS lp not found (expected /usr/bin/lp)",
        ));
    };

    let file = std::env::temp_dir().join(format!(
        "palmart-escpos-{}-{}.bin",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    fs::write(&file, data)?;

    let output = Command::new(lp)
        .args([
            "-d",
            name,
            "-o",
            "raw",
            "-o",
            "document-format=application/vnd.cups-raw",
            &file.to_string_lossy(),
        ])
        .output();

    let _ = fs::remove_file(&file);

    match output {
        Ok(out) if out.status.success() => {
            log::info!("sent {} bytes to CUPS queue {name}", data.len());
            Ok(())
        }
        Ok(out) => Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        )),
        Err(e) => Err(e),
    }
}

fn read_http_request(
    stream: &mut TcpStream,
) -> std::io::Result<(String, String, Vec<u8>, HashMap<String, String>)> {
    let mut buf = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];

    let (body_start, content_length, method, path, headers) = loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed before HTTP headers completed",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);

        let header_end = match find_header_end(&buf) {
            Some(i) => i,
            None => {
                if buf.len() > 65536 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "HTTP headers too large",
                    ));
                }
                continue;
            }
        };

        let sep_len = if buf[header_end..].starts_with(b"\r\n\r\n") {
            4
        } else {
            2
        };
        let body_start = header_end + sep_len;
        let header_text = String::from_utf8_lossy(&buf[..header_end]);
        let mut lines = header_text.lines();
        let request_line = lines.next().unwrap_or("");
        let parts: Vec<&str> = request_line.split_whitespace().collect();
        if parts.len() < 2 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "bad request line",
            ));
        }
        let method = parts[0].to_string();
        let path = parts[1].to_string();

        let mut headers = HashMap::new();
        let mut content_length = 0usize;
        for line in lines {
            if line.is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                if k.trim().eq_ignore_ascii_case("content-length") {
                    content_length = v.trim().parse().unwrap_or(0);
                }
            }
        }

        break (body_start, content_length, method, path, headers);
    };

    while buf.len() < body_start + content_length {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!(
                    "connection closed with {}/{} body bytes",
                    buf.len().saturating_sub(body_start),
                    content_length
                ),
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    }

    let body = buf[body_start..body_start + content_length].to_vec();
    Ok((method, path, body, headers))
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .or_else(|| buf.windows(2).position(|w| w == b"\n\n"))
}

fn cors_headers() -> &'static str {
    "Access-Control-Allow-Origin: *\r\n\
     Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
     Access-Control-Allow-Headers: Content-Type, X-Printer-Cups-Name, X-Printer-Host, X-Printer-Port\r\n"
}

fn write_response(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         {cors}\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         Content-Type: text/plain\r\n\r\n{body}",
        body.len(),
        cors = cors_headers()
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()?;
    Ok(())
}

fn write_json_response(stream: &mut TcpStream, status: u16, json: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         {cors}\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         Content-Type: application/json\r\n\r\n{json}",
        json.len(),
        cors = cors_headers()
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()?;
    Ok(())
}

fn config_path(app_data: &Path) -> PathBuf {
    app_data.join("conf").join("printer.json")
}

fn load_config(app_data: &Path) -> PrinterConfig {
    let path = config_path(app_data);
    if !path.exists() {
        return PrinterConfig {
            mode: "file".to_string(),
            host: String::new(),
            port: 9100,
            path: app_data.join("receipts.log").display().to_string(),
            cups_name: String::new(),
        };
    }
    match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            log::warn!("invalid printer.json: {e}");
            PrinterConfig {
                mode: "file".to_string(),
                host: String::new(),
                port: 9100,
                path: app_data.join("receipts.log").display().to_string(),
                cups_name: String::new(),
            }
        }),
        Err(_) => PrinterConfig {
            mode: "none".to_string(),
            host: String::new(),
            port: 9100,
            path: String::new(),
            cups_name: String::new(),
        },
    }
}

fn send_to_printer(app_data: &Path, data: &[u8]) -> std::io::Result<()> {
    let cfg = load_config(app_data);
    match cfg.mode.as_str() {
        "none" => {
            log::info!("printer mode=none — dropped {} bytes", data.len());
            Ok(())
        }
        "network" => {
            if cfg.host.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "printer.json: host required for network mode",
                ));
            }
            let addr = format!("{}:{}", cfg.host, cfg.port);
            let mut stream = TcpStream::connect(&addr)?;
            stream.set_write_timeout(Some(Duration::from_secs(10)))?;
            stream.write_all(data)?;
            stream.flush()?;
            log::info!("sent {} bytes to printer at {addr}", data.len());
            Ok(())
        }
        "cups" => {
            if cfg.cups_name.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "printer.json: cupsName required for cups mode",
                ));
            }
            if !is_valid_cups_name(&cfg.cups_name) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "printer.json: invalid CUPS printer name",
                ));
            }
            send_to_cups(&cfg.cups_name, data)
        }
        "file" | _ => {
            let path = if cfg.path.is_empty() {
                app_data.join("receipts.log")
            } else {
                PathBuf::from(&cfg.path)
            };
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
            file.write_all(data)?;
            file.flush()?;
            log::info!("appended {} bytes to {}", data.len(), path.display());
            Ok(())
        }
    }
}
