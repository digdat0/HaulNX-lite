<#
Store-submission build for the desktop companion: HaulNX App Utility Lite.

Same reasoning and mechanics as build-public.ps1 (local-only extras physically
moved aside during the build, then restored -- Tauri embeds the whole src/
folder as webview assets regardless of any Rust cfg, so a flag alone can't
exclude them), plus `--features lite`, which:
  - makes app_info() report "lite": true, so index.html hides the Archive
    Collections tab, the archive.org half of Credentials, and the
    archive.org quick-test tool (see IS_LITE in index.html);
  - makes download_file() refuse an archive.org URL outright (defense in
    depth -- the real removal is index.html never constructing one).

Also strips index.html for the build: every block between "@lite-strip-begin"
and "@lite-strip-end" marker lines (the Find Collections tab -- archive.org
search -- is physically removed, not just hidden), and "// @lite-only: "
lines become live code. The full page is parked outside src/ during the build
and restored afterwards. release-lite.sh applies the same rule to the Lite
repo's exported source.

Builds to its own target-lite/ dir so it never clobbers (or mixes stale
objects with) a normal `cargo build --release` or build-public.ps1's
target-public/.

Output: target-lite/dist/haulnx-app-utility.exe copied to
desktop/HaulNX-AppUtility-Lite.exe -- ship only this exe to the store that
rejected the archive.org/downloader feature; never the plain
HaulNX-AppUtility.exe (that one still has it).
#>

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$extOps = Join-Path $root 'src-tauri\src\ext_ops.rs'
$localExt = Join-Path $root 'src\local-ext.js'
# Park the extras OUTSIDE src/ and src-tauri/src/. They used to be renamed in
# place (src/local-ext.js -> src/local-ext.js.lite-build-aside), but Tauri
# embeds every file under src/ whatever its name, so local-ext.js shipped
# inside the exe under the renamed name in every public/Lite build.
$asideDir = Join-Path $root '.build-aside-lite'
$extOpsAway = Join-Path $asideDir 'ext_ops.rs'
$localExtAway = Join-Path $asideDir 'local-ext.js'

$indexHtml = Join-Path $root 'src\index.html'
$indexAway = Join-Path $asideDir 'index.html'

$movedExtOps = $false
$movedLocalExt = $false
$movedIndex = $false

try {
    New-Item -ItemType Directory -Force $asideDir | Out-Null
    if (Test-Path $extOps) {
        Move-Item $extOps $extOpsAway -Force
        $movedExtOps = $true
    }
    if (Test-Path $localExt) {
        Move-Item $localExt $localExtAway -Force
        $movedLocalExt = $true
    }

    # Lite strip (see the header). Read before parking, write after, so a
    # failure in between leaves the original in place.
    $lines = [IO.File]::ReadAllLines($indexHtml)
    $outLines = New-Object System.Collections.Generic.List[string]
    $skip = $false
    foreach ($l in $lines) {
        if ($l.Contains('@lite-strip-begin')) { $skip = $true; continue }
        if ($l.Contains('@lite-strip-end')) { $skip = $false; continue }
        if ($skip) { continue }
        $outLines.Add(($l -replace '^\s*// @lite-only: ', ''))
    }
    if ($skip) { throw "index.html: @lite-strip-begin without a matching @lite-strip-end" }
    $stripped = [string]::Join("`n", $outLines) + "`n"
    foreach ($needle in @('findCollections', 'Find Collections', 'navFind', 'advancedsearch', '@lite-')) {
        if ($stripped.Contains($needle)) { throw "Lite strip left '$needle' in index.html" }
    }
    Move-Item $indexHtml $indexAway -Force
    $movedIndex = $true
    [IO.File]::WriteAllText($indexHtml, $stripped, (New-Object Text.UTF8Encoding $false))

    Push-Location (Join-Path $root 'src-tauri')
    try {
        $env:CARGO_TARGET_DIR = Join-Path $root 'src-tauri\target-lite'
        cargo build --profile dist --features lite   # full LTO -- see [profile.dist] in Cargo.toml
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }
    } finally {
        Remove-Item Env:\CARGO_TARGET_DIR -ErrorAction SilentlyContinue
        Pop-Location
    }

    $exe = Join-Path $root 'src-tauri\target-lite\dist\haulnx-app-utility.exe'
    if (-not (Test-Path $exe)) { throw "build succeeded but $exe is missing" }
    # Refuse to publish an exe that still carries either extra (or anything
    # parked by an older version of this script): Tauri keeps asset names as
    # plain text in the binary, and a compiled ext_ops.rs leaves its path in
    # panic locations.
    $bytes = [IO.File]::ReadAllBytes($exe)
    $text = [Text.Encoding]::ASCII.GetString($bytes)
    foreach ($needle in @('local-ext', 'build-aside', 'ext_ops.rs')) {
        if ($text.Contains($needle)) {
            throw "refusing to publish: $exe contains '$needle' (a local-only file leaked into the build)"
        }
    }
    $out = Join-Path $root 'HaulNX-AppUtility-Lite.exe'
    Copy-Item $exe $out -Force
    Write-Host "Lite exe written to $out"
} finally {
    if ($movedIndex) { Move-Item $indexAway $indexHtml -Force }
    if ($movedExtOps) { Move-Item $extOpsAway $extOps -Force }
    if ($movedLocalExt) { Move-Item $localExtAway $localExt -Force }
}
