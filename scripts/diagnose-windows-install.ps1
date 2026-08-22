# ── Kiosk Desktop — Windows install diagnostics ────────────────────────────
# Run in PowerShell (right-click Start → Windows PowerShell, or run with:
#   powershell -ExecutionPolicy Bypass -File desktop\scripts\diagnose-windows-install.ps1
# Prints the evidence a support engineer needs: log tails, port owners,
# the JVM command line (normal vs \\?\ path), and the health endpoint.
$ErrorActionPreference = "Continue"
$d = Join-Path $env:APPDATA "Palmart"

Write-Host "== Kiosk Desktop install version =="
$exe = Get-Item (Join-Path $d "..") -ErrorAction SilentlyContinue
Get-ChildItem "$env:LOCALAPPDATA" -Filter "kiosk-desktop*.exe" -Recurse -ErrorAction SilentlyContinue |
    Select-Object FullName, Length, LastWriteTime | Format-List

Write-Host "== tail kiosk.log =="
Get-Content (Join-Path $d "kiosk.log") -Tail 40 -ErrorAction SilentlyContinue

Write-Host "== tail backend.err.log =="
Get-Content (Join-Path $d "backend.err.log") -Tail 40 -ErrorAction SilentlyContinue

Write-Host "== tail backend.out.log =="
Get-Content (Join-Path $d "backend.out.log") -Tail 40 -ErrorAction SilentlyContinue

Write-Host "== ports 19500 / 5050 / 33306 =="
netstat -ano | Select-String -Pattern "19500|5050|33306"

Write-Host "== owning processes =="
$pids = netstat -ano | Select-String -Pattern "19500|5050|33306" |
    ForEach-Object { ($_ -split "\s+")[-1] } | Sort-Object -Unique
foreach ($pid in $pids) {
    Get-Process -Id $pid -ErrorAction SilentlyContinue |
        Select-Object Id, ProcessName, Path | Format-List
}

Write-Host "== JVM command line (check for \\?\\ prefix on -jar) =="
Get-CimInstance Win32_Process -Filter "Name like 'java%' or Name like 'javaw%'" |
    Select-Object ProcessId, CommandLine | Format-List

Write-Host "== Flyway history (last 5) =="
$maria = Get-ChildItem $d -Filter "mariadb.exe" -Recurse -ErrorAction SilentlyContinue | Select-Object -First 1
if ($maria) { "mariadb client available at $($maria.FullName)" } else { "mariadb client not in APP_DATA" }

Write-Host "== health endpoint =="
try {
    $r = Invoke-WebRequest "http://127.0.0.1:5050/actuator/health" -UseBasicParsing -TimeoutSec 5
    "HTTP $($r.StatusCode)  $($r.Content)"
} catch {
    "UNREACHABLE: $($_.Exception.Message)"
}
