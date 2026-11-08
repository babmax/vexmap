# Measures what the player sees: the time between shown frames with background panorama capture off and on.
# Runs the S2 spike in throwaway folders under .deps\run\s2, three spacings between faces.
$ErrorActionPreference = 'Continue'
$root = 'E:\vexmap\.deps'
$version = Join-Path $env:APPDATA 'horizon-launcher\versions\nightly-2026-11-07-6560641'
$ctl = Join-Path $version 'horizon-ctl.exe'
$resources = Join-Path $version '.local\horizon-client\26.3'
$run = Join-Path $root 'run\s2'
$env:VEXMAP_S2_DIR = Join-Path $run 'faces-pacing'
$env:HORIZON_HOME = Join-Path $run 'home'
$log = Join-Path $env:TEMP 'horizon-control.log'
function ctl { & $ctl @args 2>&1 | ForEach-Object { "$_" } }

foreach ($gap in 10, 3, 0) {
    $env:VEXMAP_S2_GAP = "$gap"
    "===== gap $gap"
    ctl launch --game (Join-Path $root 'target\fast\horizon-client.exe') --world vexmap_s2 --seed 42 --creative --wait `
        --resources $resources --game-dir (Join-Path $run 'game') --width 1920 --height 1080 | Out-Null
    ctl wait 300 | Out-Null
    ctl key w down | Out-Null
    ctl wait 420 | Out-Null
    ctl key n tap | Out-Null
    ctl wait 640 | Out-Null
    ctl key n tap | Out-Null
    ctl key w up | Out-Null
    ctl wait 60 | Out-Null
    ctl quit | Out-Null
    Start-Sleep -Seconds 3
    Get-Content $log | Select-String 'pacing' | Select-Object -Last 7 | ForEach-Object { $_.Line }
}
