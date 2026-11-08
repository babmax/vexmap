# Starts the S1 spike build in throwaway folders, opens the globe, takes screenshots and compares frame times.
# Never touches the player's own saves: everything lives under .deps\run\s1.
$ErrorActionPreference = 'Continue'
$root = 'E:\vexmap\.deps'
$version = Join-Path $env:APPDATA 'horizon-launcher\versions\nightly-2026-11-07-6560641'
$ctl = Join-Path $version 'horizon-ctl.exe'
$resources = Join-Path $version '.local\horizon-client\26.3'
$run = Join-Path $root 'run\s1'
$shots = Join-Path $run 'shots'
New-Item -ItemType Directory -Force $shots | Out-Null
$env:HORIZON_HOME = Join-Path $run 'home'

function step($name) { "=== $name" }
function ctl { & $ctl @args 2>&1 | ForEach-Object { "$_" } }
function frametimes($seconds) {
    ctl stats reset | Out-Null
    ctl wait ($seconds * 20) | Out-Null
    ctl stats
}

step 'launch'
ctl launch --game (Join-Path $root 'target\fast\horizon-client.exe') --world vexmap_s1 --seed 42 --creative --wait `
    --resources $resources --game-dir (Join-Path $run 'game') --width 1920 --height 1080
step 'state'
ctl state | Select-Object -First 12
step 'world, map closed'
frametimes 15
ctl screenshot (Join-Path $shots '1-world.png')
step 'open map'
ctl key m tap
ctl wait 40
ctl status | Select-String -Pattern 'screen' | Select-Object -First 3
ctl screenshot (Join-Path $shots '2-globe.png')
step 'globe, map open'
frametimes 15
step 'drag and zoom'
ctl drag-at 500 300 620 340 700 360
ctl scroll-at 500 300 8
ctl wait 20
ctl screenshot (Join-Path $shots '3-globe-moved.png')
step 'close map'
ctl key esc tap
ctl wait 20
ctl screenshot (Join-Path $shots '4-closed.png')
step 'world again'
frametimes 10
step 'quit'
ctl quit
"done"
