# Installs the latest bracket release for Windows:
#
#   irm https://raw.githubusercontent.com/xav-db/bracket-cli/main/install/install.ps1 | iex
#
# Set BRACKET_INSTALL_DIR to install somewhere other than %LOCALAPPDATA%\Programs\bracket.
$ErrorActionPreference = 'Stop'

$repo = 'xav-db/bracket-cli'
$dir = if ($env:BRACKET_INSTALL_DIR) { $env:BRACKET_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\bracket' }
if (-not [Environment]::Is64BitOperatingSystem) { throw 'bracket: only 64-bit Windows is supported' }

$url = "https://github.com/$repo/releases/latest/download/bracket-cli-windows-amd64.exe"
$exe = Join-Path $dir 'bracket.exe'
New-Item -ItemType Directory -Force -Path $dir | Out-Null

Write-Host "Downloading $url"
Invoke-WebRequest -Uri $url -OutFile "$exe.download" -UseBasicParsing
# Windows won't overwrite a running exe, but it will rename one. This is what `bracket update` relies on.
Remove-Item -Force "$exe.old" -ErrorAction SilentlyContinue
if (Test-Path $exe) { Move-Item -Force $exe "$exe.old" }
Move-Item -Force "$exe.download" $exe

Write-Host "Installed $(& $exe --version) to $dir"

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($userPath -split ';') -notcontains $dir) {
  [Environment]::SetEnvironmentVariable('Path', "$userPath;$dir", 'User')
  Write-Host "Added $dir to your PATH. Open a new terminal to use bracket."
}
