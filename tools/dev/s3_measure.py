#!/usr/bin/env python3
"""Spike S3/S4: runs the modded engine as a dedicated server against bots and compares the cost of mod messages.

Local only: the server binds 127.0.0.1, everything lives under .deps/run/s3. Results: .deps/run/s3/results.json.

    python tools/dev/s3_measure.py gate      # who gets in with horizon-clients-only
    python tools/dev/s3_measure.py matrix    # the traffic scenarios against 20 and 50 bots
"""
import ctypes
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[2]
DEPS = ROOT / ".deps"
BIN = DEPS / "target" / "fast"
VERSION = Path(os.environ["APPDATA"]) / "horizon-launcher" / "versions" / "nightly-2026-11-07-6560641"
RESOURCES = VERSION / ".local" / "horizon-client" / "26.3"
RUN = DEPS / "run" / "s3"
SERVER_DIR = RUN / "server"
PORT = 25599
SERVER_CPUS = 0b0000_1111_1111  # 4 cores, 8 threads
BOT_CPUS = 0b1111_0000_0000  # 2 cores, 4 threads
SECONDS = 100
KEEP_REPORTS = 4

PROPERTIES = {
    "server-ip": "127.0.0.1",
    "server-port": str(PORT),
    "online-mode": "false",
    "level-seed": "12345",
    "view-distance": "10",
    "simulation-distance": "10",
    "gamemode": "creative",
    "force-gamemode": "true",
    "max-players": "200",
    "spawn-protection": "0",
    "white-list": "false",
    "horizon-clients-only": "true",
}

LINE = re.compile(r"\[vexmap_s3\] server (.*)")


def pin(process, mask):
    if os.name != "nt":
        return
    kernel32 = ctypes.windll.kernel32
    handle = kernel32.OpenProcess(0x0200 | 0x0400, False, process.pid)  # SET_INFORMATION | QUERY_INFORMATION
    if handle:
        kernel32.SetProcessAffinityMask(handle, mask)
        kernel32.CloseHandle(handle)


def prepare():
    SERVER_DIR.mkdir(parents=True, exist_ok=True)
    text = "".join(f"{key}={value}\n" for key, value in PROPERTIES.items())
    (SERVER_DIR / "server.properties").write_text(text)
    # The owner accepted Mojang's EULA in chat on 2026-11-07, for this local test server only.
    (SERVER_DIR / "eula.txt").write_text("eula=true\n")


def start_server(size, msgs, log):
    env = dict(os.environ, VEXMAP_S3_SIZE=str(size), VEXMAP_S3_MSGS=str(msgs), HORIZON_NO_AUDIO="1",
               HORIZON_HOME=str(RUN / "home"))
    output = log.open("w", encoding="utf-8")
    process = subprocess.Popen(
        [str(BIN / "horizon-client.exe"), "--resources", str(RESOURCES), "--dedicated", "--nogui"],
        cwd=SERVER_DIR, env=env, stdin=subprocess.PIPE, stdout=output, stderr=subprocess.STDOUT, text=True)
    pin(process, SERVER_CPUS)
    deadline = time.monotonic() + 240
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise SystemExit(f"the server stopped with {process.returncode}, see {log}")
        try:
            socket.create_connection(("127.0.0.1", PORT), timeout=1).close()
            return process
        except OSError:
            time.sleep(1)
    process.kill()
    raise SystemExit(f"the server did not open its port, see {log}")


def stop_server(process):
    try:
        process.stdin.write("stop\n")
        process.stdin.flush()
        process.wait(timeout=120)
    except (OSError, subprocess.TimeoutExpired):
        process.kill()
        process.wait()


def run_bots(count, seconds, report, brand="horizon", behaviour="mixed", name="Bot"):
    command = [str(BIN / "horizon-bots.exe"), "--server", f"127.0.0.1:{PORT}", "--bots", str(count),
               "--behaviour", behaviour, "--join-every", "0.25", "--seconds", str(seconds), "--name", name,
               "--report", str(report)]
    if brand:
        command += ["--brand", brand]
    process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
                               encoding="utf-8", errors="replace")
    pin(process, BOT_CPUS)
    output, _ = process.communicate(timeout=seconds + 180)
    return process.returncode, output


def parse(log):
    rows = []
    for line in log.read_text(encoding="utf-8", errors="replace").splitlines():
        match = LINE.search(line)
        if match:
            rows.append({k: float(v) for k, v in (pair.split("=") for pair in match.group(1).split())})
    return rows


def summarize(rows):
    kept = [row for row in rows if row["players"] > 0][-KEEP_REPORTS:]
    if not kept:
        return {"reports": 0}
    mean = lambda key: round(sum(row[key] for row in kept) / len(kept), 2)
    return {
        "reports": len(kept),
        "players": kept[-1]["players"],
        "tps": mean("tps"),
        "mspt": mean("mspt_avg"),
        "hook_us_p50": mean("hook_us_p50"),
        "hook_us_p99": mean("hook_us_p99"),
        "hook_us_max": max(row["hook_us_max"] for row in kept),
        "sent_kb_s_per_player": mean("sent_kb_s_per_player"),
        "send_errors": sum(row["send_errors"] for row in kept),
    }


def gate():
    prepare()
    log = RUN / "gate.log"
    server = start_server(16, 0, log)
    results = {}
    for label, brand, name in (("no brand", None, "NoBrand"), ("vanilla brand", "vanilla", "Vanilla"),
                               ("horizon brand", "horizon", "Hori")):
        code, output = run_bots(2, 15, RUN / f"gate-{name}.json", brand=brand, behaviour="idle", name=name)
        results[label] = {"exit": code, "tail": output.strip().splitlines()[-3:]}
        print(label, json.dumps(results[label], ensure_ascii=False), flush=True)
    stop_server(server)
    text = log.read_text(encoding="utf-8", errors="replace").splitlines()
    print("--- server log")
    for line in text:
        if re.search(r"NoBrand|Vanilla|Hori|client|Horizon client", line):
            print(line[:200])


def matrix():
    prepare()
    scenarios = [("baseline", 16, 0), ("map", 800, 1), ("peak", 800, 4), ("heavy", 30000, 1), ("ceiling", 30000, 32),
                 ("baseline-again", 16, 0)]
    results = []
    out = RUN / "results.json"
    for bots in (20, 50):
        for name, size, msgs in scenarios:
            log = RUN / f"server-{bots}-{name}.log"
            started = time.monotonic()
            print(f"--- {bots} bots, {name}: {size} bytes x {msgs}", flush=True)
            server = start_server(size, msgs, log)
            code, output = run_bots(bots, SECONDS, RUN / f"bots-{bots}-{name}.json")
            stop_server(server)
            row = {"bots": bots, "scenario": name, "size": size, "msgs": msgs, "bots_exit": code,
                   "seconds": round(time.monotonic() - started), **summarize(parse(log))}
            results.append(row)
            print(json.dumps(row), flush=True)
            out.write_text(json.dumps(results, indent=1))
    print("done", flush=True)


def client():
    """A real Horizon client on the server: what arrives, in what order, and the round trip."""
    prepare()
    ctl = VERSION / "horizon-ctl.exe"
    env = dict(os.environ, HORIZON_HOME=str(RUN / "client-home"))
    client_log = Path(os.environ["TEMP"]) / "horizon-control.log"
    results = []
    for name, size, msgs in (("map", 800, 1), ("peak", 800, 4), ("heavy", 30000, 1), ("ceiling", 30000, 32)):
        log = RUN / f"server-client-{name}.log"
        print(f"--- client, {name}: {size} bytes x {msgs}", flush=True)
        server = start_server(size, msgs, log)
        launch = subprocess.run(
            [str(ctl), "launch", "--game", str(BIN / "horizon-client.exe"), "--resources", str(RESOURCES),
             "--game-dir", str(RUN / "client-game"), "--width", "1280", "--height", "720", "--no-pause",
             "--quickPlayMultiplayer", f"127.0.0.1:{PORT}"],
            env=env, capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=240)
        if launch.returncode:
            print("launch failed:", launch.stdout[-300:], launch.stderr[-300:], flush=True)
        time.sleep(70)
        state = subprocess.run([str(ctl), "state"], env=env, capture_output=True, text=True, encoding="utf-8",
                               errors="replace", timeout=30).stdout
        stats = subprocess.run([str(ctl), "stats"], env=env, capture_output=True, text=True, encoding="utf-8",
                               errors="replace", timeout=30).stdout
        subprocess.run([str(ctl), "quit"], env=env, capture_output=True, timeout=60)
        time.sleep(5)
        lines = client_log.read_text(encoding="utf-8", errors="replace").splitlines()
        client_lines = [line for line in lines if "[vexmap_s3] client" in line][-3:]
        stop_server(server)
        server_lines = [line for line in log.read_text(encoding="utf-8", errors="replace").splitlines()
                        if "[vexmap_s3] server" in line and "players=1 " in line][-3:]
        row = {"scenario": name, "connected": '"dimension"' in state, "client": client_lines, "server": server_lines,
               "client_fps": re.findall(r'"avg_fps":([0-9.]+)', stats)[:1],
               "disconnected": [line[:160] for line in lines if "isconnect" in line or "Timed out" in line][-2:]}
        results.append(row)
        print(json.dumps(row, ensure_ascii=False), flush=True)
    (RUN / "client-results.json").write_text(json.dumps(results, indent=1, ensure_ascii=False))
    print("done", flush=True)


if __name__ == "__main__":
    {"gate": gate, "matrix": matrix, "client": client}[sys.argv[1]]()
