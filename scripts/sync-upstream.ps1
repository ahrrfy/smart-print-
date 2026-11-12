<#
.SYNOPSIS
    PdfCraft Enterprise Multi-Printer Edition — Conflict-Free Upstream Sync & Full Package Builder
.DESCRIPTION
    Fetches the latest updates/releases from the official upstream channel
    (https://github.com/storytold/pdfcraft.git), merges them cleanly on top of our
    isolated Enterprise Multi-Printer modules, verifies architectural layer rules,
    builds the full release binaries (pdfcraft.exe + pdfcraft-cli.exe), fetches SHA-256-verified
    offline OCR models (cargo xtask models), and packages the complete POSIX-compliant Portable
    distribution with SHA256SUMS.txt configured for ahrrfy/smart-print-.

    Can be run directly by the developer/admin:
        powershell -ExecutionPolicy Bypass -File .\scripts\sync-upstream.ps1
    Or invoked headlessly by PdfCraft's built-in Update Manager ("Update and Restart"):
        powershell -ExecutionPolicy Bypass -File .\scripts\sync-upstream.ps1 -TargetRef v0.22.0 -StageZipPath C:\...\update.zip -SkipTests
#>
param(
    [string]$TargetRef = "upstream/main",
    [string]$StageZipPath = "",
    [string]$CustomRepo = "ahrrfy/smart-print-",
    [switch]$SkipTests
)

$ErrorActionPreference = "Stop"

function New-PosixPortableZip {
    param(
        [Parameter(Mandatory = $true)][string]$SourceDir,
        [Parameter(Mandatory = $true)][string]$ZipFilePath,
        [Parameter(Mandatory = $true)][string]$WrapperFolderName,
        [string[]]$ExcludeNames = @("PdfCraftData")
    )
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem

    if (Test-Path -LiteralPath $ZipFilePath) {
        Remove-Item -LiteralPath $ZipFilePath -Force
    }
    $Parent = Split-Path -Parent $ZipFilePath
    if ($Parent -and !(Test-Path -LiteralPath $Parent)) {
        New-Item -ItemType Directory -Path $Parent -Force | Out-Null
    }

    $ResolvedSource = (Resolve-Path -LiteralPath $SourceDir).Path.TrimEnd('\', '/')
    $Fs = [System.IO.File]::Open($ZipFilePath, [System.IO.FileMode]::CreateNew, [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
    try {
        $Archive = New-Object System.IO.Compression.ZipArchive($Fs, [System.IO.Compression.ZipArchiveMode]::Create, $false)
        try {
            $AllFiles = Get-ChildItem -LiteralPath $ResolvedSource -Recurse -File
            foreach ($File in $AllFiles) {
                $Rel = $File.FullName.Substring($ResolvedSource.Length).TrimStart('\', '/')
                $TopSeg = ($Rel -split '[\\/]')[0]
                if ($ExcludeNames -contains $TopSeg) {
                    continue
                }
                # Enforce forward slashes ('/') so craft_update::portable::unpack_over accepts every nested entry
                $EntryName = ($WrapperFolderName + "/" + ($Rel -replace '\\', '/'))
                [System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
                    $Archive,
                    $File.FullName,
                    $EntryName,
                    [System.IO.Compression.CompressionLevel]::Optimal
                ) | Out-Null
            }
        } finally {
            $Archive.Dispose()
        }
    } finally {
        $Fs.Dispose()
    }
}

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
Set-Location -Path $RepoRoot

Write-Host "=== [1/6] Preparing Upstream Channel (storytold/pdfcraft) ===" -ForegroundColor Cyan
$UpstreamUrl = "https://github.com/storytold/pdfcraft.git"
$Remotes = git remote
if ($Remotes -notcontains "upstream") {
    git remote add upstream $UpstreamUrl
} else {
    git remote set-url upstream $UpstreamUrl
}

git fetch upstream --tags --prune

$GitName = git config user.name
if ([string]::IsNullOrWhiteSpace($GitName)) {
    git config user.name "PdfCraft Enterprise Sync"
    git config user.email "sync@pdfcraft.local"
}

$MergeTarget = $TargetRef
git rev-parse --verify $MergeTarget *> $null
if ($LASTEXITCODE -ne 0) {
    throw "Upstream reference '$MergeTarget' was not found after fetching $UpstreamUrl."
}

$TargetSha = (git rev-parse $MergeTarget).Trim()

$Stashed = $false
$DirtyStatus = git status --porcelain
if (![string]::IsNullOrWhiteSpace($DirtyStatus)) {
    Write-Host "Saving uncommitted local changes to temporary stash before upstream merge..." -ForegroundColor Yellow
    git stash push -u -m "auto-stash-before-upstream-sync-$(Get-Date -Format 'yyyyMMdd-HHmmss')"
    $Stashed = $true
}

function Resolve-VersionOnlyMergeConflicts {
    $LocalCargo = Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe"
    $CargoExe = if (Test-Path $LocalCargo) { $LocalCargo } else { "cargo" }

    $PrevEap = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $Unmerged = @(git diff --name-only --diff-filter=U | Where-Object { ![string]::IsNullOrWhiteSpace($_) })
        if ($Unmerged.Count -eq 0) {
            return $false
        }
        foreach ($Path in $Unmerged) {
            if ($Path -notin @("Cargo.toml", "Cargo.lock")) {
                return $false
            }
        }

        if ($Unmerged -contains "Cargo.toml") {
            $RawToml = [System.IO.File]::ReadAllText((Join-Path $RepoRoot "Cargo.toml"))
            $ConflictPattern = '(?ms)^<<<<<<< [^\r\n]*\r?\n(.*?)^=======\r?\n(.*?)^>>>>>>> [^\r\n]*\r?\n?'
            $MatchesList = [regex]::Matches($RawToml, $ConflictPattern)
            if ($MatchesList.Count -eq 0) {
                return $false
            }
            $VerLinePattern = '^\s*version\s*=\s*"(\d+\.\d+\.\d+)"\s*$'
            foreach ($M in $MatchesList) {
                $OursBlock = $M.Groups[1].Value.TrimEnd("`r", "`n")
                $TheirsBlock = $M.Groups[2].Value.TrimEnd("`r", "`n")
                if (($OursBlock -notmatch $VerLinePattern) -or ($TheirsBlock -notmatch $VerLinePattern)) {
                    return $false
                }
            }
            $ResolvedToml = [regex]::Replace($RawToml, $ConflictPattern, {
                param($Match)
                $OursLine = $Match.Groups[1].Value.TrimEnd("`r", "`n")
                $TheirsLine = $Match.Groups[2].Value.TrimEnd("`r", "`n")
                $OursVer = [version]([regex]::Match($OursLine, '"(\d+\.\d+\.\d+)"').Groups[1].Value)
                $TheirsVer = [version]([regex]::Match($TheirsLine, '"(\d+\.\d+\.\d+)"').Groups[1].Value)
                $NewLine = if ($Match.Value.Contains("`r`n")) { "`r`n" } else { "`n" }
                if ($TheirsVer -ge $OursVer) {
                    return ($TheirsLine + $NewLine)
                } else {
                    return ($OursLine + $NewLine)
                }
            })
            [System.IO.File]::WriteAllText((Join-Path $RepoRoot "Cargo.toml"), $ResolvedToml, (New-Object System.Text.UTF8Encoding($false)))
            git add Cargo.toml
        }

        if ($Unmerged -contains "Cargo.lock") {
            git checkout --theirs Cargo.lock 2>&1 | Out-Null
            & $CargoExe metadata --format-version 1 2>&1 | Out-Null
            if ($LASTEXITCODE -ne 0) {
                return $false
            }
            git add Cargo.lock
        } elseif (Test-Path (Join-Path $RepoRoot "Cargo.lock")) {
            & $CargoExe metadata --format-version 1 2>&1 | Out-Null
            if ($LASTEXITCODE -eq 0) {
                git add Cargo.lock
            }
        }

        $Remaining = @(git diff --name-only --diff-filter=U | Where-Object { ![string]::IsNullOrWhiteSpace($_) })
        return ($Remaining.Count -eq 0)
    } finally {
        $ErrorActionPreference = $PrevEap
    }
}

try {
    git merge-base --is-ancestor $TargetSha HEAD
    if ($LASTEXITCODE -eq 0) {
        Write-Host "Already up to date with $MergeTarget ($TargetSha)." -ForegroundColor Green
    } else {
        Write-Host "=== [2/6] Merging $MergeTarget into Custom Enterprise Branch ===" -ForegroundColor Cyan
        git merge $MergeTarget --no-edit -m "chore(sync): merge upstream $MergeTarget into Enterprise Multi-Printer Edition"
        if ($LASTEXITCODE -ne 0) {
            if (Resolve-VersionOnlyMergeConflicts) {
                git commit --no-edit -m "chore(sync): merge upstream $MergeTarget into Enterprise Multi-Printer Edition (SemVer max)"
                if ($LASTEXITCODE -ne 0) {
                    git merge --abort
                    throw "Failed to finalize automatic version-conflict merge with $MergeTarget."
                }
                Write-Host "Automatically resolved Cargo.toml/Cargo.lock version conflict using SemVer max!" -ForegroundColor Green
            } else {
                git merge --abort
                throw "Automatic merge encountered a conflict with $MergeTarget. Merge was safely aborted so no files were harmed."
            }
        } else {
            Write-Host "Successfully merged $MergeTarget without conflicts!" -ForegroundColor Green
        }
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

Write-Host "=== [3/6] Verifying Architecture Layer Boundaries (xtask layers) ===" -ForegroundColor Cyan
cargo run -p xtask -- layers
if ($LASTEXITCODE -ne 0) {
    throw "xtask layers check failed after upstream sync."
}

if (-not $SkipTests -and [string]::IsNullOrWhiteSpace($StageZipPath)) {
    Write-Host "=== [3b/6] Running Core Print & Custom Update Regression Tests ===" -ForegroundColor Cyan
    cargo test -p pdfcraft-print --lib
    if ($LASTEXITCODE -ne 0) { throw "pdfcraft-print unit tests failed." }
    cargo test -p pdfcraft-ui-egui --lib platform::custom_update
    if ($LASTEXITCODE -ne 0) { throw "pdfcraft-ui-egui custom_update tests failed." }
}

Write-Host "=== [4/6] Building Full Release Binaries (pdfcraft.exe + pdfcraft-cli.exe) ===" -ForegroundColor Cyan
cargo build --release -p pdfcraft -p pdfcraft-cli
if ($LASTEXITCODE -ne 0) {
    throw "Release build failed."
}

$TargetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $BuildWorkDir "target" }
$BuiltExe = Join-Path $TargetDir "release\pdfcraft.exe"
$BuiltCli = Join-Path $TargetDir "release\pdfcraft-cli.exe"
if (!(Test-Path $BuiltExe) -or !(Test-Path $BuiltCli)) {
    throw "Compiled binaries not found in $TargetDir\release"
}

$VersionOutput = (& $BuiltExe --version | Out-String).Trim()
$VersionNumber = ($VersionOutput -replace "^pdfcraft\s+", "").Split(" ")[0]
Write-Host "Built version: $VersionOutput" -ForegroundColor Green

Write-Host "=== [5/6] Staging SHA-256-Verified Offline OCR Models (cargo xtask models) ===" -ForegroundColor Cyan
$ModelsCacheDir = Join-Path $TargetDir "models-stage"
New-Item -ItemType Directory -Path $ModelsCacheDir -Force | Out-Null
cargo run -p xtask -- models $ModelsCacheDir
if ($LASTEXITCODE -ne 0) {
    throw "cargo xtask models failed."
}
Remove-Item -Force (Join-Path $ModelsCacheDir "*.part") -ErrorAction SilentlyContinue

function Populate-PortableFolder([string]$DestFolder) {
    New-Item -ItemType Directory -Path $DestFolder -Force | Out-Null
    Copy-Item -LiteralPath $BuiltExe -Destination (Join-Path $DestFolder "pdfcraft.exe") -Force -ErrorAction Stop
    Copy-Item -LiteralPath $BuiltCli -Destination (Join-Path $DestFolder "pdfcraft-cli.exe") -Force -ErrorAction Stop

    $DestModels = Join-Path $DestFolder "models"
    New-Item -ItemType Directory -Path $DestModels -Force | Out-Null
    Copy-Item -Path (Join-Path $ModelsCacheDir "*") -Destination $DestModels -Recurse -Force

    $OfficialPortableTxt = Join-Path $RepoRoot "packaging\windows\portable.txt"
    if (Test-Path -LiteralPath $OfficialPortableTxt) {
        Copy-Item -LiteralPath $OfficialPortableTxt -Destination (Join-Path $DestFolder "portable.txt") -Force
    } else {
        Set-Content -LiteralPath (Join-Path $DestFolder "portable.txt") -Value "Portable mode marker - settings stored in PdfCraftData" -Encoding UTF8
    }

    $ArGuide = Join-Path $RepoRoot "docs\PORTABLE-README-AR.txt"
    if (Test-Path -LiteralPath $ArGuide) {
        Copy-Item -LiteralPath $ArGuide -Destination (Join-Path $DestFolder "README-AR.txt") -Force
    }

    foreach ($DocFile in @("README.md", "NOTICE", "LICENSE-MIT", "LICENSE-APACHE")) {
        $DocSrc = Join-Path $RepoRoot $DocFile
        if (Test-Path -LiteralPath $DocSrc) {
            Copy-Item -LiteralPath $DocSrc -Destination (Join-Path $DestFolder $DocFile) -Force
        }
    }
    $IconLic = Join-Path $RepoRoot "assets\app-icon\LICENSE-document.txt"
    if (Test-Path -LiteralPath $IconLic) {
        Copy-Item -LiteralPath $IconLic -Destination (Join-Path $DestFolder "LICENSE-document.txt") -Force
    }

    $ChannelJson = [ordered]@{
        upstream_repo = "storytold/pdfcraft"
        custom_repo   = $CustomRepo
    } | ConvertTo-Json -Depth 3
    Set-Content -LiteralPath (Join-Path $DestFolder "update_channel.json") -Value $ChannelJson -Encoding UTF8
}

$WrapperStem = "pdfcraft-$VersionNumber-windows-x64-portable"

Write-Host "=== [6/6] Packaging Full Portable Distribution ===" -ForegroundColor Cyan
if (![string]::IsNullOrWhiteSpace($StageZipPath)) {
    $TempPackDir = Join-Path ([System.IO.Path]::GetTempPath()) ("pdfcraft-stage-" + [System.Guid]::NewGuid().ToString("N"))
    try {
        Populate-PortableFolder -DestFolder $TempPackDir
        New-PosixPortableZip -SourceDir $TempPackDir -ZipFilePath $StageZipPath -WrapperFolderName $WrapperStem
        Write-Host "Staged full portable update ZIP at: $StageZipPath" -ForegroundColor Green
    } finally {
        Remove-Item -LiteralPath $TempPackDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}

$DistDir = Join-Path $RepoRoot "dist"
$PortableDir = Join-Path $DistDir "PdfCraft-Portable"

try {
    Populate-PortableFolder -DestFolder $PortableDir
    $ZipPath = Join-Path $DistDir "PdfCraft-Portable.zip"
    $VersionedZip = Join-Path $DistDir "$WrapperStem.zip"

    New-PosixPortableZip -SourceDir $PortableDir -ZipFilePath $VersionedZip -WrapperFolderName $WrapperStem -ExcludeNames @("PdfCraftData")
    Copy-Item -LiteralPath $VersionedZip -Destination $ZipPath -Force

    $Hash = (Get-FileHash -LiteralPath $VersionedZip -Algorithm SHA256).Hash.ToLowerInvariant()
    $SumsLines = @(
        "$Hash  $WrapperStem.zip",
        "$Hash  PdfCraft-Portable.zip"
    ) -join "`n"
    [System.IO.File]::WriteAllText((Join-Path $DistDir "SHA256SUMS.txt"), ($SumsLines + "`n"), [System.Text.Encoding]::ASCII)
    Write-Host "Updated $PortableDir, $VersionedZip, and PdfCraft-Portable.zip (SHA-256: $Hash)" -ForegroundColor Green
} catch {
    if ([string]::IsNullOrWhiteSpace($StageZipPath)) {
        throw $_
    }
    Write-Host "Note: dist\PdfCraft-Portable\pdfcraft.exe is currently running; helper will replace it on exit." -ForegroundColor Yellow
}
