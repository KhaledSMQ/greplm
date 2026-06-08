# Install greplm and greplm-mcp on Windows.
#
# Usage:
#   irm https://raw.githubusercontent.com/KhaledSMQ/greplm/main/install.ps1 | iex

$ErrorActionPreference = "Stop"

# GitHub requires TLS 1.2; Windows PowerShell 5.1 does not enable it by default.
try {
    [Net.ServicePointManager]::SecurityProtocol = `
        [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
} catch {}

$Repo = if ($env:GREPLM_REPO) { $env:GREPLM_REPO } else { "KhaledSMQ/greplm" }
$Version = $env:GREPLM_VERSION

$CargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $HOME ".cargo" }
if ($env:GREPLM_INSTALL) {
    $InstallDir = $env:GREPLM_INSTALL
} elseif ((Test-Path (Join-Path $CargoHome "bin")) -or (Get-Command cargo -ErrorAction SilentlyContinue)) {
    $InstallDir = Join-Path $CargoHome "bin"
} else {
    $InstallDir = Join-Path $HOME ".local\bin"
}

function Install-FromCargo {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        return $false
    }
    Write-Host "Building from source with cargo (this may take a few minutes)..."
    # cargo install --root <dir> always writes to <dir>\bin, so build into a
    # temp root and copy the binaries into InstallDir for a consistent layout.
    $CargoRoot = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid().ToString())
    New-Item -ItemType Directory -Path $CargoRoot | Out-Null
    try {
        cargo install --locked --root $CargoRoot --git "https://github.com/$Repo" greplm-cli greplm-mcp
        if ($LASTEXITCODE -ne 0) { return $false }

        New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
        foreach ($Name in @("greplm.exe", "greplm-mcp.exe")) {
            $Dest = Join-Path $InstallDir $Name
            Copy-Item (Join-Path $CargoRoot "bin\$Name") $Dest -Force
            Write-Host "  installed $Dest"
        }
        return $true
    } finally {
        Remove-Item -Recurse -Force $CargoRoot -ErrorAction SilentlyContinue
    }
}

function Install-FromRelease {
    $Target = "x86_64-pc-windows-msvc"
    if ($Version) {
        $Tag = if ($Version -match '^v') { $Version } else { "v$Version" }
        $Base = "https://github.com/$Repo/releases/download/$Tag/greplm-$Target"
    } else {
        $Base = "https://github.com/$Repo/releases/latest/download/greplm-$Target"
    }

    $Tmp = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid().ToString())
    New-Item -ItemType Directory -Path $Tmp | Out-Null
    try {
        $Zip = Join-Path $Tmp "greplm-$Target.zip"
        Invoke-WebRequest -Uri "$Base.zip" -OutFile $Zip -UseBasicParsing
        Expand-Archive -Path $Zip -DestinationPath $Tmp -Force

        New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
        foreach ($Name in @("greplm.exe", "greplm-mcp.exe")) {
            $Dest = Join-Path $InstallDir $Name
            Copy-Item (Join-Path $Tmp $Name) $Dest -Force
            Write-Host "  installed $Dest"
        }
        return $true
    } catch {
        Write-Warning "Release install failed: $_"
        return $false
    } finally {
        Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
    }
}

Write-Host "Installing greplm into $InstallDir..."

$Ok = $false
if (Install-FromRelease) { $Ok = $true }
elseif (Install-FromCargo) { $Ok = $true }

if (-not $Ok) {
    Write-Error @"
No prebuilt binary available and cargo is not installed.
Install Rust from https://rustup.rs then re-run this script, or run:
  cargo install --locked --git https://github.com/$Repo greplm-cli greplm-mcp
"@
}

$PathParts = $env:PATH -split ';'
if ($PathParts -notcontains $InstallDir) {
    Write-Host ""
    Write-Host "Add $InstallDir to your PATH if greplm is not found."
    Write-Host ""
}

Write-Host "Done. Run: greplm --help"
