# Installs prbot for the current Windows user and runs its setup. In PowerShell:
#   irm https://raw.githubusercontent.com/al-noori/prbot/main/install.ps1 | iex

$ErrorActionPreference = 'Stop'
$dir = Join-Path $env:LOCALAPPDATA 'prbot'
$exe = Join-Path $dir 'prbot.exe'
New-Item -ItemType Directory -Force $dir | Out-Null

# Updating: stop the prbot installed here so its file can be replaced.
if (Test-Path $exe) {
    & $exe stop 2>$null | Out-Null
    Get-Process prbot -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe } | Stop-Process -Force
    Start-Sleep -Seconds 1
}

Write-Host 'Downloading prbot...'
$ProgressPreference = 'SilentlyContinue'
Invoke-WebRequest 'https://github.com/al-noori/prbot/releases/latest/download/prbot-windows-x86_64.exe' -OutFile $exe -UseBasicParsing

# Make `prbot` available in new terminals, and in this one.
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not (($userPath -split ';') -contains $dir)) {
    [Environment]::SetEnvironmentVariable('Path', (($userPath, $dir) | Where-Object { $_ }) -join ';', 'User')
}
if (-not (($env:Path -split ';') -contains $dir)) { $env:Path = "$env:Path;$dir" }

Write-Host "Installed $exe"
& $exe setup
