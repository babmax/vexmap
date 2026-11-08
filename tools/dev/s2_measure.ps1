# Starts the S2 spike in throwaway folders and compares frame times with background panorama capture off and on.
# Never touches the player's own saves: everything lives under .deps\run\s2.
$ErrorActionPreference = 'Continue'
$root = 'E:\vexmap\.deps'
$version = Join-Path $env:APPDATA 'horizon-launcher\versions\nightly-2026-11-07-6560641'
$ctl = Join-Path $version 'horizon-ctl.exe'
$resources = Join-Path $version '.local\horizon-client\26.3'
$run = Join-Path $root 'run\s2'
$env:VEXMAP_S2_DIR = Join-Path $run 'faces'
$env:HORIZON_HOME = Join-Path $run 'home'
Remove-Item -Recurse -Force $env:VEXMAP_S2_DIR -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $env:VEXMAP_S2_DIR | Out-Null

function ctl { & $ctl @args 2>&1 | ForEach-Object { "$_" } }
function frametimes($label, $seconds) {
    ctl stats reset | Out-Null
    ctl wait ($seconds * 20) | Out-Null
    $s = (ctl stats) -join '' | ConvertFrom-Json
    "{0,-22} frames={1,5} fps={2,6:N1} p50={3,5:N2} p95={4,5:N2} p99={5,5:N2} max={6,6:N1} gpu_avg={7,5:N2} cpu_avg={8,5:N2} low1%={9,6:N1}" -f $label, $s.frames, $s.avg_fps, $s.p50_ms, $s.p95_ms, $s.p99_ms, $s.max_ms, $s.gpu.avg_ms, $s.cpu.avg_ms, $s.low_1pct_fps
}

ctl launch --game (Join-Path $root 'target\fast\horizon-client.exe') --world vexmap_s2 --seed 42 --creative --wait `
    --resources $resources --game-dir (Join-Path $run 'game') --width 1920 --height 1080 | Out-Null
ctl wait 400 | Out-Null
frametimes 'standing, off' 20
ctl key n tap | Out-Null
frametimes 'standing, capture on' 30
ctl key n tap | Out-Null
ctl wait 40 | Out-Null
ctl screenshot (Join-Path $run 'after-capture.png') | Out-Null
frametimes 'standing, off again' 15
ctl key w down | Out-Null
frametimes 'walking, off' 15
ctl key n tap | Out-Null
frametimes 'walking, capture on' 20
ctl key n tap | Out-Null
ctl key w up | Out-Null
ctl wait 40 | Out-Null
ctl screenshot (Join-Path $run 'after-walk.png') | Out-Null
ctl quit | Out-Null
Start-Sleep -Seconds 3
'--- mod log'
Get-Content (Join-Path $env:TEMP 'horizon-control.log') | Select-String 'vexmap_s2' | Select-Object -Last 8 | ForEach-Object { $_.Line }
'--- faces'
$faces = Get-ChildItem $env:VEXMAP_S2_DIR -Filter *.png
"count=$($faces.Count) total_kb=$([int](($faces | Measure-Object Length -Sum).Sum / 1KB))"
$faces | Group-Object { $_.Name.Substring(0, 3) } | Select-Object -First 3 | ForEach-Object { "$($_.Name): $($_.Count) faces, $([int](($_.Group | Measure-Object Length -Sum).Sum / 1KB)) KB" }
'done'
