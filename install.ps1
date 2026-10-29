# Install the gitjet binary on Windows (no installer, no admin rights).
#   irm https://github.com/kashari/gitjet/releases/latest/download/install.ps1 | iex
# Options (environment variables):
#   GITJET_INSTALL_DIR  where to put gitjet.exe (default: %LOCALAPPDATA%\Programs\gitjet)
#   GITJET_VERSION      tag to install, e.g. v0.1.0 (default: latest)
$ErrorActionPreference = 'Stop'

$repo = if ($env:GITJET_REPO) { $env:GITJET_REPO } else { 'kashari/gitjet' }
$installDir = if ($env:GITJET_INSTALL_DIR) { $env:GITJET_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\gitjet' }

$arch = if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -eq 'Arm64') { 'aarch64' } else { 'x86_64' }
$target = "$arch-pc-windows-msvc"
$base = if ($env:GITJET_VERSION) { "https://github.com/$repo/releases/download/$($env:GITJET_VERSION)" } else { "https://github.com/$repo/releases/latest/download" }
$url = "$base/gitjet-$target.zip"

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("gitjet-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    Write-Host "Downloading $url"
    $zip = Join-Path $tmp 'gitjet.zip'
    Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
    $expected = $null
    try {
        $expected = ((Invoke-WebRequest -Uri "$url.sha256" -UseBasicParsing).Content -split '\s+')[0]
    } catch { }
    if ($expected) {
        $actual = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
        if ($expected.ToLower() -ne $actual) { throw "gitjet: checksum mismatch, aborting" }
    }

    Expand-Archive -Path $zip -DestinationPath $tmp -Force
    New-Item -ItemType Directory -Force -Path $installDir | Out-Null
    Copy-Item (Join-Path $tmp 'gitjet.exe') (Join-Path $installDir 'gitjet.exe') -Force
    Write-Host "Installed gitjet to $installDir\gitjet.exe"

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not ($userPath -split ';' | Where-Object { $_ -eq $installDir })) {
        [Environment]::SetEnvironmentVariable('Path', ($userPath.TrimEnd(';') + ';' + $installDir), 'User')
        Write-Host "Added $installDir to your user PATH. Open a new terminal to use 'gitjet'."
    }
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
