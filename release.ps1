# Builds dist\LocalAI-Setup.exe for publishing: the manager, then the installer that embeds it.
# Run: powershell -NoProfile -ExecutionPolicy Bypass -File release.ps1
# Panic messages carry source paths, so the cargo home (inside your user folder) and this folder are
# remapped out of both exes, and the result is checked for them before it is copied to dist\.
$ErrorActionPreference = 'Stop'
$repo = $PSScriptRoot
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
$cargo = (Get-Command cargo -ErrorAction SilentlyContinue).Source
if (-not $cargo) { $cargo = Join-Path $cargoHome 'bin\cargo.exe' }

# One argument per 0x1F-separated field, so folder names with spaces stay whole (RUSTFLAGS splits on spaces).
$env:CARGO_ENCODED_RUSTFLAGS = "--remap-path-prefix=$cargoHome=cargo" + [char]0x1F + "--remap-path-prefix=$repo=localai-manager"
& $cargo build --release --manifest-path "$repo\Cargo.toml"
if ($LASTEXITCODE) { exit $LASTEXITCODE }
& $cargo build --release --manifest-path "$repo\installer\Cargo.toml"
if ($LASTEXITCODE) { exit $LASTEXITCODE }

$private = @($env:USERPROFILE, $cargoHome, $repo, $env:USERNAME, $env:COMPUTERNAME) | Where-Object { $_ -and $_.Length -ge 4 }
foreach ($exe in @("$repo\target\release\localai-manager.exe", "$repo\installer\target\release\localai-setup.exe")) {
    $bytes = [IO.File]::ReadAllBytes($exe)
    # Byte text plus UTF-16 text at both alignments.
    $texts = @([Text.Encoding]::GetEncoding(28591).GetString($bytes), [Text.Encoding]::Unicode.GetString($bytes), [Text.Encoding]::Unicode.GetString($bytes, 1, $bytes.Length - 1))
    foreach ($p in $private) {
        foreach ($t in $texts) {
            if ($t.IndexOf($p, [StringComparison]::OrdinalIgnoreCase) -ge 0) { throw "$exe still contains '$p'; not copied to dist" }
        }
    }
}

New-Item -ItemType Directory -Force "$repo\dist" | Out-Null
Copy-Item "$repo\installer\target\release\localai-setup.exe" "$repo\dist\LocalAI-Setup.exe" -Force
$out = Get-Item "$repo\dist\LocalAI-Setup.exe"
"{0} ({1:N1} MB)" -f $out.FullName, ($out.Length / 1MB)
"SHA-256 {0}" -f (Get-FileHash $out.FullName -Algorithm SHA256).Hash
