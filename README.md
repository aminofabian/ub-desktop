# Palmart Desktop — Tauri shell

The thin Rust + WebView shell that wraps the desktop SKU's Spring Boot JAR + bundled MariaDB. See `../DESKTOP_INSTALLATION.md` §§5, 7, 16.

## Layout

```
desktop/
├── README.md
├── scripts/
│   └── prepare-tauri-resources.sh   # jlink JRE + MariaDB + bootJar → src-tauri/Resources/
├── dist/                          stub frontend (Tauri requires frontendDist)
└── src-tauri/
    ├── Resources/                 # bundled into .app / .dmg (jre, mariadb, jar, config)
    └── src/
        ├── bundle.rs              # resolve bundled JRE / MariaDB paths
        ├── supervisor.rs
        ├── mariadb.rs
        └── backend.rs
```

## Local dev (without full bundle)

Prerequisites:

- Rust + `cargo install tauri-cli --version "^2.0" --locked`
- JDK 21+ with `jlink`
- Optional: Homebrew `mariadb@10.11` and `openjdk@21` as fallbacks when `Resources/` is empty

```bash
# Populate Resources/ (recommended — matches production .app)
bash desktop/scripts/prepare-tauri-resources.sh

cd backend && ./gradlew bootJar -Pdesktop=true
cd ../desktop/src-tauri && cargo run --release
```

The shell prefers **bundled** binaries under `Resources/`:

- `Resources/jre/bin/java`
- `Resources/mariadb/bin/mariadbd` (or flat `Resources/mariadb/mariadbd`)
- `Resources/jar/kiosk.jar`

If those are missing, it falls back to Homebrew MariaDB and `java` on `PATH`.

### Environment overrides

| var | use |
|-----|-----|
| `APP_DATA` | Per-install data dir (default `~/Library/Application Support/Palmart`) |
| `APP_DESKTOP_RESOURCES` | Force bundle `Resources/` path |
| `APP_JAVA_BIN` | Override JVM |
| `APP_MARIADB_BIN_DIR` | Override MariaDB `bin/` |
| `APP_DESKTOP_JAR` | Override bootJar path |
| `APP_DESKTOP_SKIP_MARIADB` | Attach to external `mariadbd` (`1`) |

## Production bundle

`cargo tauri build` runs `prepare-tauri-resources.sh` automatically (`beforeBuildCommand`), then produces:

```bash
cd desktop/src-tauri
cargo tauri build
# → target/release/bundle/macos/*.app and *.dmg
```

Unsigned macOS builds may need:

```bash
xattr -dr com.apple.quarantine "target/release/bundle/macos/Kiosk Desktop.app"
```

## Windows installer (cross-built from macOS/Linux)

The NSIS installer bundles a prebuilt Temurin 21 JRE (Windows x64), MariaDB
10.11 winx64, and `kiosk.jar` under `resources/` next to the exe.

One-time toolchain setup:

```bash
rustup target add x86_64-pc-windows-msvc
cargo install cargo-xwin --locked
brew install nsis llvm lld
```

Offline WebView2 runtime (required — `webviewInstallMode: offlineInstaller`):

```bash
# Download the Evergreen Standalone Installer (~127 MB) and place it exactly here:
#   https://developer.microsoft.com/microsoft-edge/webview2/ → "Evergreen Standalone Installer"
#   src-tauri/bundle/windows/MicrosoftEdgeWebview2Setup.exe
#
# The installer embeds it, so Windows installs never touch the network. The
# Tauri bundler fails the build if this file is missing.
mkdir -p src-tauri/bundle/windows
curl -L -o src-tauri/bundle/windows/MicrosoftEdgeWebview2Setup.exe \
  https://go.microsoft.com/fwlink/p/?LinkId=2124703
```

Build:

```bash
# Stage Resources-windows/ (downloads are cached in desktop/.cache/)
bash desktop/scripts/prepare-windows-resources.sh

cd desktop/src-tauri
export PATH="$(brew --prefix llvm)/bin:$(brew --prefix lld)/bin:$PATH"
cargo tauri build --runner cargo-xwin --target x86_64-pc-windows-msvc
# → target/x86_64-pc-windows-msvc/release/bundle/nsis/Kiosk_<version>_x64-setup.exe
```

Config lives in `src-tauri/tauri.windows.conf.json` (merged over
`tauri.conf.json` for Windows targets). The installer is unsigned — Windows
SmartScreen will show "More info → Run anyway" until a code-signing
certificate is configured via `bundle > windows > signCommand`.

Publish to the web download page with `cd frontend && bun run
pack:desktop-downloads` (picks up the newest installer per platform from
`desktop/` or the Tauri bundle output).

### One-command release

`bash desktop/scripts/release.sh` does the whole ship path in one go:
rebuilds the UI + JAR, stages Windows resources, cross-builds the NSIS
installer (optionally the macOS dmg with `--macos`), uploads it to a GitHub
Release (`desktop-v<version>` on the frontend repo), and republishes the site
manifest so the homepage download button serves the new build. Run
`bash desktop/scripts/release.sh --no-upload` to build without publishing.

## Device bridge (ESC/POS)

The shell starts an HTTP listener on `127.0.0.1:19500`:

- `POST /print` — raw ESC/POS bytes → network printer (9100) or `APP_DATA/receipts.log`
- `POST /drawer/kick` — cash drawer pulse

Configure via **Settings → Desktop & LAN → Receipt printer** (`APP_DATA/conf/printer.json`).

The JVM posts through `POST /api/v1/desktop/devices/print/sale/{saleId}`.

## Share on LAN & Windows Firewall

**Settings → Desktop & LAN → Share on LAN** writes `APP_DATA/conf/lan-enabled`;
next boot binds the JVM to `0.0.0.0` instead of `127.0.0.1` so other devices
on the same network can open the POS at `http://<lan-ip>:5050` (backend
`DesktopLanService` / `DesktopLanController`).

Other devices will still be **blocked by Windows Firewall** unless port 5050
has an inbound allow rule. The backend tries to add one automatically
(`netsh advfirewall firewall add rule name="Palmart Kiosk 5050" …`) when LAN
is toggled on, but that requires elevation, which a normal (non-admin) till
won't have — the Settings page then shows the one-liner to run once in an
**admin** PowerShell:

```bat
netsh advfirewall firewall add rule name="Palmart Kiosk 5050" dir=in action=allow protocol=TCP localport=5050
```

Verify from another device with `curl http://<lan-ip>:5050/api/v1/license/status`
(a timeout = firewall/router blocking; a refused connection = wrong IP/port).

## Licensing (vendor workflow)

Kiosk Desktop runs a 30‑day trial until an Ed25519‑signed license token is
pasted in **Settings → License**. Tokens are issued by the vendor, offline:

1. **One‑command setup** — generates the key pair once (idempotent), stores it
   outside the repo at `~/.palmart-license/`, bakes the public key into
   `application-desktop.properties` automatically, and prints the env var line
   (or paste it into the console's License issuer key section):

   ```bash
   bash backend/scripts/generate-license.sh bootstrap
   ```

   - Private key: `~/.palmart-license/private.pem` (mode 600 — keep it safe, back it up,
     never commit or send it). Regenerate only before any licenses exist:
     `bootstrap --force`.
   - Public key: `~/.palmart-license/public.pem`, baked into
     `app.desktop.license.public-key` as a default so `APP_DESKTOP_LICENSE_PUBLIC_KEY`
     can still override at runtime.
   - Cloud: set `APP_DESKTOP_LICENSE_PRIVATE_KEY=<…>` on the deployment so
     the Super Admin console can issue licenses — **or** skip the env var and
     paste the PRIVATE_KEY (and PUBLIC_KEY) into the Super Admin console's
     **Platform → Desktop licenses → License issuer key** section: it is stored
     encrypted in the platform database and picked up immediately, no restart.
   - `bash backend/scripts/generate-license.sh pubkey` prints the baked public key.
   - `release.sh` warns at preflight if the public key isn't baked (tills would
     run trial-only).

2. **Issue a license per customer** — the Super Admin console
   (Platform → Desktop licenses) or the CLI, then send the token manually
   (WhatsApp/email):

   ```bash
   LICENSE_PRIVATE_KEY=$(cat ~/.palmart-license/private.pem) \
     bash backend/scripts/generate-license.sh issue \
       --business "Exact Shop Name" --plan shop --days 365
   # prints one line: the token the customer pastes into Settings → License
   ```

   - `--business` must match the shop name entered in the first‑run wizard
     **exactly** (case‑sensitive runtime check).
   - `--plan` is `counter | shop | lan` (informational today — nothing gates
     on it yet).
   - Validity: `--days N`, `--expires <ISO‑8601>`, or `--perpetual`.
   - `--fingerprint <machine-id>` **required** — the till's Machine ID
     (Settings → License → Machine ID, copy button). The runtime rejects any
     token whose fingerprint doesn't match the machine it runs on, so a key
     issued for one shop/till can't be pasted into another.

4. **Sanity‑check before sending**: `verify` runs the same check the till runs:

   ```bash
   LICENSE_PUBLIC_KEY=<PUBLIC_KEY> bash backend/scripts/generate-license.sh verify --token <TOKEN>
   ```

The token is `base64url(payload).base64url(ed25519-sig)` where payload =
`{businessName, plan, issuedAt, expiresAt, machineFingerprint}` — signed by
`LicenseService.encodeToken` (see `backend/…/desktop/license/LicenseService.java`).
Expired/invalid licenses flip the app to read‑only (`DesktopLicenseReadOnlyFilter`).

## Signing & updates (§12–§13)

```bash
export APPLE_SIGNING_IDENTITY="Developer ID Application: …"
./desktop/scripts/sign-and-notarize-macos.sh path/to/*.dmg
```

See `desktop/updates.example.json` for the Tauri updater manifest template.

## Still optional / polish

- mDNS discovery for LAN tills (backend JMDNS)
- Encrypted backups + USB detection (§14)
- Windows/Linux installer CI
