# Hoard CLI installer - Windows (PowerShell)
#
#   irm https://hoard.services/install.ps1 | iex
#
# Detects your architecture, downloads the matching `hoard` tarball from the
# latest GitHub release, verifies its SHA-256, installs hoard.exe to
# %LOCALAPPDATA%\hoard\bin, and adds that folder to your user PATH.
#
# Override with environment variables before running:
#   $env:HOARD_VERSION     = '1.0.2'   # pin a version instead of "latest"
#   $env:HOARD_INSTALL_DIR = 'C:\tools' # install somewhere else
#
# After install:  hoard login ; hoard sync start

$ErrorActionPreference = 'Stop'
# Invoke-WebRequest's progress bar makes large downloads crawl on Windows
# PowerShell 5.1 — turning it off speeds the tarball fetch up by orders of
# magnitude and avoids rendering glitches in non-interactive hosts.
$ProgressPreference = 'SilentlyContinue'
$Repo = 'rleeon/hoard'

# Windows PowerShell 5.1 defaults to TLS 1.0/1.1, which GitHub rejects. Opt into
# TLS 1.2 so the API + download calls below succeed. Harmless on PowerShell 7+.
try {
  [Net.ServicePointManager]::SecurityProtocol =
    [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
} catch {}

function Info($m) { Write-Host "==> $m" -ForegroundColor Green }
function Warn($m) { Write-Host "warning: $m" -ForegroundColor Yellow }
function Die($m)  { Write-Host "error: $m" -ForegroundColor Red; exit 1 }

# tar.exe ships with Windows 10 (1803+) / 11.
if (-not (Get-Command tar.exe -ErrorAction SilentlyContinue)) {
  Die "tar.exe not found. Windows 10 1803+ or Windows 11 is required."
}

# ---- detect architecture ---------------------------------------------------
# Use the OS env vars (always set on Windows) rather than RuntimeInformation,
# which returns empty in some Windows PowerShell 5.1 hosts. PROCESSOR_ARCHITEW6432
# is set when a 32-bit or emulated shell runs on a 64-bit/ARM64 OS, and holds the
# *real* machine arch — so it wins when present.
$rawArch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
switch ("$rawArch".ToUpper()) {
  'AMD64' { $arch = 'x86_64' }
  'X64'   { $arch = 'x86_64' }
  'ARM64' { $arch = 'aarch64' }
  default {
    Die "could not detect a supported architecture (got '$rawArch'). Download manually from https://hoard.services/cli"
  }
}
$platform = "windows-$arch"

# ---- resolve version -------------------------------------------------------
$ver = $env:HOARD_VERSION
if ([string]::IsNullOrWhiteSpace($ver)) {
  Info "Looking up the latest release..."
  try {
    $rel = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest" `
             -UseBasicParsing -Headers @{ 'User-Agent' = 'hoard-installer' }
    $ver = $rel.tag_name
  } catch {
    Die "could not reach the GitHub API (rate limited?). Set `$env:HOARD_VERSION and retry."
  }
}
$ver = $ver.TrimStart('v')

$asset = "hoard-$ver-$platform.tar.gz"
$url   = "https://github.com/$Repo/releases/download/v$ver/$asset"

# ---- download --------------------------------------------------------------
$tmp = Join-Path $env:TEMP ("hoard-" + [System.Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp -Force | Out-Null
try {
  $pkg = Join-Path $tmp 'pkg.tar.gz'
  Info "Downloading $asset"
  try {
    Invoke-WebRequest $url -OutFile $pkg -UseBasicParsing -Headers @{ 'User-Agent' = 'hoard-installer' }
  } catch {
    Die "download failed: $url"
  }

  # ---- verify sha256 -------------------------------------------------------
  # Download the .sha256 to a file and read it as text. GitHub serves it as
  # application/octet-stream, and on Windows PowerShell 5.1 `.Content` of a
  # non-text response is a byte[], not a string — splitting that yielded the
  # first byte's decimal value (0x65 = 101) instead of the hash.
  $shaFile = Join-Path $tmp 'pkg.sha256'
  $verified = $false
  try {
    Invoke-WebRequest "$url.sha256" -OutFile $shaFile -UseBasicParsing `
      -Headers @{ 'User-Agent' = 'hoard-installer' }
    $shaText  = (Get-Content $shaFile -Raw).Trim()
    $expected = ($shaText -split '\s+')[0].ToLower()   # "<hash> *<file>" -> hash
    $verified = $true
  } catch {
    Warn "could not download the checksum file - skipping verification."
  }
  if ($verified) {
    $actual = (Get-FileHash $pkg -Algorithm SHA256).Hash.ToLower()
    if ($expected.Length -ne 64) {
      Warn "unexpected checksum format - skipping verification."
    } elseif ($expected -ne $actual) {
      Die "checksum mismatch! expected $expected, got $actual. Aborting."
    } else {
      Info "Checksum verified."
    }
  }

  # ---- extract -------------------------------------------------------------
  tar.exe -xzf $pkg -C $tmp
  if ($LASTEXITCODE -ne 0) { Die "failed to extract the archive." }
  $src = Join-Path $tmp "hoard-$ver-$platform\hoard.exe"
  if (-not (Test-Path $src)) { Die "the archive did not contain hoard.exe." }

  # ---- install -------------------------------------------------------------
  $dir = $env:HOARD_INSTALL_DIR
  if ([string]::IsNullOrWhiteSpace($dir)) {
    $dir = Join-Path $env:LOCALAPPDATA 'hoard\bin'
  }
  New-Item -ItemType Directory -Path $dir -Force | Out-Null
  Copy-Item $src (Join-Path $dir 'hoard.exe') -Force
  Info "Installed hoard $ver -> $dir\hoard.exe"

  # ---- PATH (user scope) ---------------------------------------------------
  $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
  $parts = @()
  if ($userPath) { $parts = $userPath -split ';' }
  if ($parts -notcontains $dir) {
    $newPath = if ($userPath) { "$userPath;$dir" } else { $dir }
    [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
    $env:Path = "$env:Path;$dir"   # current session
    Warn "$dir was added to your user PATH. Open a new terminal for it to take effect everywhere."
  }
} finally {
  Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host ""
Info "Done. Next steps:"
Write-Host "  hoard login       # sign in (Cloud or self-hosted)"
Write-Host "  hoard sync start  # run the background sync service"
Write-Host ""
Write-Host "Docs: https://hoard.services/cli" -ForegroundColor DarkGray
