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
# Windows won't overwrite or delete a running exe, but it will rename one, and a long-lived
# `bracket ssh-proxy` may still be running an old binary. So park the current one under a unique
# name, and sweep up earlier ones that are no longer in use. This is what `bracket update` relies on.
Get-ChildItem -Path $dir -Filter 'bracket.exe.old-*' -ErrorAction SilentlyContinue |
  Remove-Item -Force -ErrorAction SilentlyContinue
if (Test-Path $exe) { Move-Item -Force $exe "$exe.old-$([guid]::NewGuid().ToString('N'))" }
Move-Item -Force "$exe.download" $exe

Write-Host "Installed $(& $exe --version) to $dir"

# Edit the raw registry value so entries like %USERPROFILE%\bin stay unexpanded.
$key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
$userPath = $key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
if (($userPath -split ';') -notcontains $dir) {
  $key.SetValue('Path', ((@($userPath, $dir) | Where-Object { $_ }) -join ';'), [Microsoft.Win32.RegistryValueKind]::ExpandString)
  # Setting a user variable through .NET broadcasts the change, so new terminals see the new PATH.
  [Environment]::SetEnvironmentVariable('BRACKET_INSTALL_REFRESH', '1', 'User')
  [Environment]::SetEnvironmentVariable('BRACKET_INSTALL_REFRESH', $null, 'User')
  Write-Host "Added $dir to your PATH. Open a new terminal to use bracket."
}
$key.Close()
