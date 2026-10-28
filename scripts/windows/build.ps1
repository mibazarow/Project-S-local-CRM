# Windows release build for Project-S (CROSPLATFORM.md §3.1).
#
# Runs on the Windows build host only: pull, install deps, build the NSIS
# installer with the Windows bundle config. The host never edits code — all
# commits are made on macOS.
#
#   ssh win11 'powershell -NoProfile -ExecutionPolicy Bypass -File C:\Users\<user>\Projects\Project-S-local-CRM\scripts\windows\build.ps1'
#
# Artifacts land in src-tauri\target\release\bundle\nsis.

$ErrorActionPreference = 'Stop'

$repo = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
Set-Location $repo

Write-Host "== repo: $repo"
git pull
if ($LASTEXITCODE -ne 0) { throw "git pull failed ($LASTEXITCODE)" }
git log --oneline -1

npm ci
if ($LASTEXITCODE -ne 0) { throw "npm ci failed ($LASTEXITCODE)" }

npx tauri build --config src-tauri/tauri.windows.conf.json
if ($LASTEXITCODE -ne 0) { throw "tauri build failed ($LASTEXITCODE)" }

Write-Host "== artifacts"
Get-ChildItem src-tauri\target\release\bundle\nsis | Format-Table Name, Length, LastWriteTime
