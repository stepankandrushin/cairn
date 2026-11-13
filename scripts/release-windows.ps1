# The Windows half of a release, on Windows: cairn.exe and cairn-tray.exe built, each signed with
# Authenticode by CAIRN_SIGN_COMMAND, a script given the file's path, and copied to dist\ for updates as
#   cairn-<version>-windows-x86_64.exe and cairn-<version>-windows-x86_64-tray.exe,
# named as release.sh names its files; installed, they are cairn.exe and cairn-tray.exe. Before them, the DLL
# of Explorer's badges (crates/explorer), built and signed, for cairn.exe to carry (crates/cli/build.rs):
# `cairn explorer on` writes it out, and Explorer loads it, so it is signed as the programs are.
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
function Sign($file) {
    if ($env:CAIRN_SIGN -eq 'off') { return }
    if (-not $env:CAIRN_SIGN_COMMAND) { throw 'set CAIRN_SIGN_COMMAND, the script that signs a file, or CAIRN_SIGN=off' }
    & $env:CAIRN_SIGN_COMMAND (Resolve-Path $file).Path
    if ($LASTEXITCODE) { throw "signing $file failed" }
    $sig = Get-AuthenticodeSignature $file
    if ($sig.Status -ne 'Valid') { throw "$file is not validly signed: $($sig.Status) $($sig.StatusMessage)" }
    Write-Output "$file signed by $($sig.SignerCertificate.Subject)"
}
# Its own profile, whose panics unwind to the entry point Explorer called rather than end Explorer.
cargo build --profile explorer --locked -p cairn-explorer
if ($LASTEXITCODE) { throw 'cargo build of the Explorer handler failed' }
$dll = 'target\explorer\cairn-explorer-signed.dll'
Copy-Item -Force target\explorer\cairn_explorer.dll $dll
Sign $dll
$env:CAIRN_EXPLORER_DLL = (Resolve-Path $dll).Path
# -p, never --workspace: the harness turns failpoints on for whatever is built with it.
cargo build --release --locked -p cairn-cli
if ($LASTEXITCODE) { throw 'cargo build failed' }
$carried = (& target\release\cairn.exe explorer status | Select-String 'carries handler (\w+)').Matches[0].Groups[1].Value
if (-not $carried -or $carried -eq 'none') { throw 'cairn.exe was built without the Explorer handler' }
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
    Sign $out
    Get-Item $out | Format-Table -AutoSize Name, Length
    Write-Output "$((Get-FileHash -Algorithm SHA256 $out).Hash.ToLower())  $(Split-Path -Leaf $out)"
}
