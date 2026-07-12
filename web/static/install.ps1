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
$Repo = 'rleeon/hoard'

function Info($m) { Write-Host "==> $m" -ForegroundColor Green }
function Warn($m) { Write-Host "warning: $m" -ForegroundColor Yellow }
function Die($m)  { Write-Host "error: $m" -ForegroundColor Red; exit 1 }

# tar.exe ships with Windows 10 (1803+) / 11.
if (-not (Get-Command tar.exe -ErrorAction SilentlyContinue)) {
  Die "tar.exe not found. Windows 10 1803+ or Windows 11 is required."
}

# ---- detect architecture ---------------------------------------------------
switch ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture) {
  'X64'   { $arch = 'x86_64' }
  'Arm64' { $arch = 'aarch64' }
  default { Die "unsupported architecture: $_" }
}
$platform = "windows-$arch"

# ---- resolve version -------------------------------------------------------
$ver = $env:HOARD_VERSION
if ([string]::IsNullOrWhiteSpace($ver)) {
  Info "Looking up the latest release..."
  try {
    $rel = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest" `
             -Headers @{ 'User-Agent' = 'hoard-installer' }
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
  try {
    $sha = (Invoke-WebRequest "$url.sha256" -UseBasicParsing `
              -Headers @{ 'User-Agent' = 'hoard-installer' }).Content
    $expected = ($sha -split '\s+')[0].Trim().ToLower()
    $actual   = (Get-FileHash $pkg -Algorithm SHA256).Hash.ToLower()
    if ($expected -and $expected -ne $actual) {
      Die "checksum mismatch! expected $expected, got $actual. Aborting."
    }
    Info "Checksum verified."
  } catch {
    Warn "could not verify checksum - continuing."
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
