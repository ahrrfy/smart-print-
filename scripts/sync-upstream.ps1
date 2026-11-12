<#
.SYNOPSIS
    PdfCraft Enterprise Multi-Printer Edition — Conflict-Free Upstream Sync & Builder
.DESCRIPTION
    Fetches the latest updates/releases from the official upstream channel
    (https://github.com/storytold/pdfcraft.git), merges them cleanly on top of our
    isolated Enterprise Multi-Printer modules, verifies architectural layer rules,
    builds the release binary, and packages the Portable distribution with SHA-256 checksums.

    Can be run directly by the developer/admin:
        powershell -ExecutionPolicy Bypass -File .\scripts\sync-upstream.ps1
    Or invoked headlessly by PdfCraft's built-in Update Manager ("Update and Restart"):
        powershell -ExecutionPolicy Bypass -File .\scripts\sync-upstream.ps1 -TargetRef v0.22.0 -StageZipPath C:\...\update.zip
#>
param(
    [string]$TargetRef = "upstream/main",
    [string]$StageZipPath = "",
    [switch]$SkipTests
)

$ErrorActionPreference = "Stop"

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
Set-Location -Path $RepoRoot

Write-Host "=== [1/5] Preparing Upstream Channel (storytold/pdfcraft) ===" -ForegroundColor Cyan
$UpstreamUrl = "https://github.com/storytold/pdfcraft.git"
$Remotes = git remote
if ($Remotes -notcontains "upstream") {
    git remote add upstream $UpstreamUrl
} else {
    git remote set-url upstream $UpstreamUrl
}

git fetch upstream --tags --prune

# Ensure git identity exists for automated merge commits
$GitName = git config user.name
if ([string]::IsNullOrWhiteSpace($GitName)) {
    git config user.name "PdfCraft Enterprise Sync"
    git config user.email "sync@pdfcraft.local"
}

# Resolve target ref: if a tag like v0.21.1 was requested and upstream/main already contains it,
# merge upstream/main (or the exact tag if preferred) cleanly.
$MergeTarget = $TargetRef
git rev-parse --verify $MergeTarget *> $null
if ($LASTEXITCODE -ne 0) {
    throw "Upstream reference '$MergeTarget' was not found after fetching $UpstreamUrl."
}

$HeadSha = (git rev-parse HEAD).Trim()
$TargetSha = (git rev-parse $MergeTarget).Trim()

$Stashed = $false
$DirtyStatus = git status --porcelain
if (![string]::IsNullOrWhiteSpace($DirtyStatus)) {
    Write-Host "Saving uncommitted local changes to temporary stash before upstream merge..." -ForegroundColor Yellow
    git stash push -u -m "auto-stash-before-upstream-sync-$(Get-Date -Format 'yyyyMMdd-HHmmss')"
    $Stashed = $true
}

try {
    git merge-base --is-ancestor $TargetSha HEAD
    if ($LASTEXITCODE -eq 0) {
        Write-Host "Already up to date with $MergeTarget ($TargetSha)." -ForegroundColor Green
    } else {
        Write-Host "=== [2/5] Merging $MergeTarget into Custom Enterprise Branch ===" -ForegroundColor Cyan
        git merge $MergeTarget --no-edit -m "chore(sync): merge upstream $MergeTarget into Enterprise Multi-Printer Edition"
        if ($LASTEXITCODE -ne 0) {
            git merge --abort
            throw "Automatic merge encountered a conflict with $MergeTarget. Merge was safely aborted so no files were harmed."
        }
        Write-Host "Successfully merged $MergeTarget without conflicts!" -ForegroundColor Green
    }
} finally {
    if ($Stashed) {
        Write-Host "Restoring local stashed changes..." -ForegroundColor Yellow
        git stash pop
    }
}

# Set up ASCII drive mapping if workspace path contains non-ASCII characters (MinGW linker compatibility on Windows)
$BuildWorkDir = $RepoRoot
if ($RepoRoot -match "[^\x00-\x7F]") {
    $ParentDir = Split-Path -Parent $RepoRoot
    $LeafName = Split-Path -Leaf $RepoRoot
    if (!(Get-PSDrive -Name X -ErrorAction SilentlyContinue)) {
        subst X: $ParentDir
    }
    if (Test-Path "X:\$LeafName") {
        $BuildWorkDir = "X:\$LeafName"
    }
}

# Configure local Rust / MinGW toolchain paths if installed in user profile
$UserCargo = Join-Path $env:USERPROFILE ".cargo\bin"
$UserW64 = Join-Path $env:USERPROFILE "w64devkit\bin"
$UserW64Libexec = Join-Path $env:USERPROFILE "w64devkit\libexec\gcc\x86_64-w64-mingw32\16.2.0"
$UserRustSelfContained = Join-Path $env:USERPROFILE ".rustup\toolchains\1.95.0-x86_64-pc-windows-gnu\lib\rustlib\x86_64-pc-windows-gnu\bin\self-contained"

$ExtraPaths = @($UserCargo, $UserW64, $UserW64Libexec, $UserRustSelfContained) | Where-Object { Test-Path $_ }
if ($ExtraPaths.Count -gt 0) {
    $env:PATH = ($ExtraPaths -join ";") + ";" + $env:PATH
}
if ((Test-Path (Join-Path $UserW64 "gcc.exe")) -and [string]::IsNullOrWhiteSpace($env:CC)) {
    $env:CC = Join-Path $UserW64 "gcc.exe"
    $env:AR = Join-Path $UserW64 "ar.exe"
    $env:RUSTFLAGS = "-C link-arg=-Wl,--disable-high-entropy-va -C link-arg=-Wl,--image-base,0x140000000"
}
if ([string]::IsNullOrWhiteSpace($env:CARGO_TARGET_DIR) -and ($RepoRoot -match "[^\x00-\x7F]")) {
    $env:CARGO_TARGET_DIR = Join-Path $env:USERPROFILE "target"
}

Set-Location -Path $BuildWorkDir

Write-Host "=== [3/5] Verifying Architecture Layer Boundaries (xtask layers) ===" -ForegroundColor Cyan
cargo run -p xtask -- layers
if ($LASTEXITCODE -ne 0) {
    throw "xtask layers check failed after upstream sync."
}

if (-not $SkipTests -and [string]::IsNullOrWhiteSpace($StageZipPath)) {
    Write-Host "=== [3b/5] Running Core Print & Coordinator Regression Tests ===" -ForegroundColor Cyan
    cargo test -p pdfcraft-print --lib
    if ($LASTEXITCODE -ne 0) { throw "pdfcraft-print unit tests failed." }
}

Write-Host "=== [4/5] Building Combined Release Binary (Upstream + Multi-Printer) ===" -ForegroundColor Cyan
cargo build --release -p pdfcraft
if ($LASTEXITCODE -ne 0) {
    throw "Release build failed."
}

$TargetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $BuildWorkDir "target" }
$BuiltExe = Join-Path $TargetDir "release\pdfcraft.exe"
if (!(Test-Path $BuiltExe)) {
    throw "Compiled binary not found at $BuiltExe"
}

$VersionOutput = (& $BuiltExe --version | Out-String).Trim()
$VersionNumber = ($VersionOutput -replace "^pdfcraft\s+", "").Split(" ")[0]
Write-Host "Built version: $VersionOutput" -ForegroundColor Green

Write-Host "=== [5/5] Packaging Portable Distribution ===" -ForegroundColor Cyan
if (![string]::IsNullOrWhiteSpace($StageZipPath)) {
    $StageParent = Split-Path -Parent $StageZipPath
    if ($StageParent -and !(Test-Path $StageParent)) {
        New-Item -ItemType Directory -Path $StageParent -Force | Out-Null
    }
    $TempPackDir = Join-Path ([System.IO.Path]::GetTempPath()) ("pdfcraft-stage-" + [System.Guid]::NewGuid().ToString("N"))
    $InnerDir = Join-Path $TempPackDir "pdfcraft-$VersionNumber-windows-x64-portable"
    New-Item -ItemType Directory -Path $InnerDir -Force | Out-Null
    Copy-Item -Path $BuiltExe -Destination (Join-Path $InnerDir "pdfcraft.exe") -Force
    Set-Content -Path (Join-Path $InnerDir "portable.txt") -Value "Portable mode marker - settings stored in PdfCraftData" -Encoding UTF8
    if (Test-Path $StageZipPath) { Remove-Item -Path $StageZipPath -Force }
    Compress-Archive -Path $InnerDir -DestinationPath $StageZipPath -Force
    Remove-Item -Path $TempPackDir -Recurse -Force -ErrorAction SilentlyContinue
    Write-Host "Staged portable update ZIP at: $StageZipPath" -ForegroundColor Green
}

# Always keep dist\PdfCraft-Portable up to date as well when not locked by a running instance
$DistDir = Join-Path $RepoRoot "dist"
$PortableDir = Join-Path $DistDir "PdfCraft-Portable"
New-Item -ItemType Directory -Path $PortableDir -Force | Out-Null

try {
    Copy-Item -Path $BuiltExe -Destination (Join-Path $PortableDir "pdfcraft.exe") -Force -ErrorAction Stop
    if (!(Test-Path (Join-Path $PortableDir "portable.txt"))) {
        Set-Content -Path (Join-Path $PortableDir "portable.txt") -Value "Portable mode marker - settings stored in PdfCraftData" -Encoding UTF8
    }
    $ChannelConfigPath = Join-Path $PortableDir "update_channel.json"
    if (!(Test-Path $ChannelConfigPath)) {
        $DefaultChannel = @{
            upstream_repo    = "storytold/pdfcraft"
            custom_repo      = ""
            source_workspace = $RepoRoot
        } | ConvertTo-Json -Depth 4
        Set-Content -Path $ChannelConfigPath -Value $DefaultChannel -Encoding UTF8
    }
    $ZipPath = Join-Path $DistDir "PdfCraft-Portable.zip"
    $VersionedZip = Join-Path $DistDir "pdfcraft-$VersionNumber-windows-x64-portable.zip"
    if (Test-Path $ZipPath) { Remove-Item $ZipPath -Force }
    if (Test-Path $VersionedZip) { Remove-Item $VersionedZip -Force }
    # Exclude runtime user data/cache (PdfCraftData) from distributable ZIP archives
    $CleanItems = Get-ChildItem -Path $PortableDir | Where-Object { $_.Name -ne "PdfCraftData" } | Select-Object -ExpandProperty FullName
    Compress-Archive -Path $CleanItems -DestinationPath $ZipPath -Force
    Copy-Item -Path $ZipPath -Destination $VersionedZip -Force

    $Hash = (Get-FileHash -Path $VersionedZip -Algorithm SHA256).Hash.ToLowerInvariant()
    $SumsLine = "$Hash  pdfcraft-$VersionNumber-windows-x64-portable.zip"
    Set-Content -Path (Join-Path $DistDir "SHA256SUMS.txt") -Value $SumsLine -Encoding ASCII
    Write-Host "Updated $PortableDir and $VersionedZip (SHA-256: $Hash)" -ForegroundColor Green
} catch {
    if ([string]::IsNullOrWhiteSpace($StageZipPath)) {
        throw $_
    }
    Write-Host "Note: dist\PdfCraft-Portable\pdfcraft.exe is currently running; helper will replace it on exit." -ForegroundColor Yellow
}
