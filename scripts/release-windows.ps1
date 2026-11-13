# The Windows half of a release, on Windows: cairn.exe and cairn-tray.exe built, each signed with
# Authenticode by CAIRN_SIGN_COMMAND, a script given the file's path, and copied to dist\ for updates as
#   cairn-<version>-windows-x86_64.exe and cairn-<version>-windows-x86_64-tray.exe,
# named as release.sh names its files; installed, they are cairn.exe and cairn-tray.exe.
# The signature is what keeps SmartScreen and antivirus from taking a program that downloads and replaces
# programs for something worse; permissions on Windows go by path, so nothing else rests on it.
# CAIRN_SIGN=off skips it, for a build that is only tried. Also needs what release.sh does
# (scripts/update-env.sh): CAIRN_UPDATE_FEED and CAIRN_UPDATE_KEYS, or CAIRN_UPDATE=off.
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')
if ($env:CAIRN_UPDATE -eq 'off') {
    Remove-Item Env:CAIRN_UPDATE_FEED, Env:CAIRN_UPDATE_KEYS -ErrorAction SilentlyContinue
} elseif (-not $env:CAIRN_UPDATE_FEED -or -not $env:CAIRN_UPDATE_KEYS) {
    throw "set CAIRN_UPDATE_FEED and CAIRN_UPDATE_KEYS, the feed and keys this release's programs update from, or CAIRN_UPDATE=off"
}
$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.*)"' | Select-Object -First 1).Matches[0].Groups[1].Value
python scripts/tray-version.py
if ($LASTEXITCODE) { throw 'tray-version.py failed' }
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $HOME '.cargo' }
$rustupHome = if ($env:RUSTUP_HOME) { $env:RUSTUP_HOME } else { Join-Path $HOME '.rustup' }
$env:RUSTFLAGS = "$env:RUSTFLAGS --remap-path-prefix=$cargoHome=/cargo --remap-path-prefix=$rustupHome=/rustup --remap-path-prefix=$PWD=/cairn".Trim()
# -p, never --workspace: the harness turns failpoints on for whatever is built with it.
cargo build --release --locked -p cairn-cli
if ($LASTEXITCODE) { throw 'cargo build failed' }
Push-Location tray
cargo build --release --locked --bin cairn-tray
if ($LASTEXITCODE) { throw 'cargo build of the tray failed' }
Pop-Location
New-Item -ItemType Directory -Force dist | Out-Null
foreach ($p in @(@('cairn', '', 'target\release\cairn.exe'), @('cairn-tray', '-tray', 'tray\target\release\cairn-tray.exe'))) {
    $name, $program, $built = $p
    if (Select-String -Path $built -Pattern 'CAIRN_FP_' -SimpleMatch -Quiet) { throw "$name was built with failpoints" }
    $said = (& $built --version | Out-String).Trim()
    if ($said -ne "$name $version") { throw "$built --version said '$said'" }
    $out = "dist\cairn-$version-windows-x86_64$program.exe"
    Copy-Item -Force $built $out
    if ($env:CAIRN_SIGN -ne 'off') {
        if (-not $env:CAIRN_SIGN_COMMAND) { throw 'set CAIRN_SIGN_COMMAND, the script that signs a file, or CAIRN_SIGN=off' }
        & $env:CAIRN_SIGN_COMMAND (Resolve-Path $out).Path
        if ($LASTEXITCODE) { throw "signing $out failed" }
        $sig = Get-AuthenticodeSignature $out
        if ($sig.Status -ne 'Valid') { throw "$out is not validly signed: $($sig.Status) $($sig.StatusMessage)" }
        Write-Output "$out signed by $($sig.SignerCertificate.Subject)"
    }
    Get-Item $out | Format-Table -AutoSize Name, Length
    Write-Output "$((Get-FileHash -Algorithm SHA256 $out).Hash.ToLower())  $(Split-Path -Leaf $out)"
}
