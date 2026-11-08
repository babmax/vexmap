#!/usr/bin/env python3
"""Phase 1 bench test (ADR-007): admission through Velocity, with the real vexmap mod.

1. A client with the mod joins server a through the proxy and is admitted (`[vexmap] admit`), then is moved to b and
   back: each server admits it again (the `Probe` path, since the client cannot see a proxy switch).
2. The launcher's stock Horizon client (same version, no vexmap mod) joins and must be kicked with our message.
3. Phase 3: each server takes its region's modules from the sidecar (tiles on in a, off in b), and when the sidecar dies
   a server keeps its last modules and still admits players.
4. Phase 3: an admitted player reaches the sidecar: recorded in the database, located in the region they are on and
   gone from the region they left. Joining a country brings the client its markers, leaving clears them.

Local only, everything under .deps/run/admission. Reuses the S5 harness (two servers, Velocity, horizon-ctl).

    python tools/dev/stage.py build mods/vexmap
    cargo build -p vexmap-sidecar
    python tools/dev/admission_test.py
"""
import json
import os
from pathlib import Path
import re
import secrets
import subprocess
import sys
import time
import urllib.request

sys.path.insert(0, str(Path(__file__).resolve().parent))
import s5_measure as s5  # noqa: E402

s5.RUN = s5.DEPS / "run" / "admission"
MODDED = s5.BIN / "horizon-client.exe"
STOCK = s5.VERSION / "horizon-client.exe"
CLIENT_LOG = Path(os.environ["TEMP"]) / "horizon-control.log"
SIDECAR = s5.ROOT / "target" / "debug" / "vexmap-sidecar.exe"
SIDECAR_ADDR = "127.0.0.1:25620"
FEED_ADDR = "127.0.0.1:25621"
REALTIME_BIT = 1 << 0
TILES_BIT = 1 << 1


def lines(path, needle):
    if not path.is_file():
        return []
    return [l for l in path.read_text(encoding="utf-8", errors="replace").splitlines() if needle in l]


def wait_for(path, needle, before, seconds):
    """Seconds until `path` has more than `before` lines with `needle`, or None."""
    started = time.monotonic()
    while time.monotonic() - started < seconds:
        if len(lines(path, needle)) > before:
            return round(time.monotonic() - started, 2)
        time.sleep(0.2)
    return None


def admitted_region_mask(path):
    """The module mask in the newest `[vexmap] admit` line of a server log, or None."""
    found = re.findall(r"region (0x[0-9a-f]+)", "\n".join(lines(path, "[vexmap] admit")))
    return int(found[-1], 16) if found else None


def api(method, path, key=None, body=None):
    request = urllib.request.Request(f"http://{SIDECAR_ADDR}{path}", method=method,
                                     data=None if body is None else json.dumps(body).encode())
    request.add_header("X-VexMap-Client", "bench")
    request.add_header("Content-Type", "application/json")
    if key:
        request.add_header("Idempotency-Key", key)
    with urllib.request.urlopen(request, timeout=5) as response:
        return json.load(response)


def start_sidecar():
    """A sidecar on a fresh database with tiles on everywhere except region b."""
    s5.RUN.mkdir(parents=True, exist_ok=True)
    database = s5.RUN / "sidecar.db"
    for path in (database, Path(f"{database}-wal"), Path(f"{database}-shm")):
        path.unlink(missing_ok=True)
    env = dict(os.environ, VEXMAP_API_ADDR=SIDECAR_ADDR, VEXMAP_FEED_ADDR=FEED_ADDR,
               VEXMAP_DATABASE_URL=f"sqlite:{database.as_posix()}?mode=rwc")
    process = subprocess.Popen([str(SIDECAR)], env=env, stdout=(s5.RUN / "sidecar.log").open("w", encoding="utf-8"),
                               stderr=subprocess.STDOUT)
    deadline = time.monotonic() + 20
    while True:
        try:
            with urllib.request.urlopen(f"http://{SIDECAR_ADDR}/healthz", timeout=2):
                break
        except OSError:
            if process.poll() is not None or time.monotonic() > deadline:
                raise SystemExit("the sidecar did not start, see sidecar.log")
            time.sleep(0.2)
    api("PUT", "/v1/features/tiles", "bench-1", {"enabled": True})
    api("PUT", "/v1/features/tiles", "bench-2", {"enabled": False, "region": "b"})
    api("PUT", "/v1/features/realtime", "bench-3", {"enabled": True})
    return process


def positions():
    """Region name to its list of players, for every region connected to the sidecar."""
    return {r["region"]: r["players"] for r in api("GET", "/v1/positions")["regions"]}


def check_feed(results):
    """After the last switch the player is on b. Two sample intervals and a poll are enough to settle."""
    deadline = time.monotonic() + 10
    while True:
        seen = positions()
        if len(seen.get("b", [])) == 1 and not seen.get("a") or time.monotonic() > deadline:
            break
        time.sleep(0.2)
    results["feed_regions"] = {name: len(players) for name, players in seen.items()}
    on_b = seen.get("b", [])
    if len(on_b) != 1:
        return
    player = on_b[0]
    results["feed_position"] = {key: player[key] for key in ("dimension", "x", "y", "z", "yaw")}
    results["feed_recorded_name"] = api("GET", f"/v1/players/{player['uuid']}")["name"] == player["name"]
    check_markers(results, player["uuid"])


def check_markers(results, uuid):
    """No country, no markers. Joining one brings the client its own marker, and leaving clears the map."""
    time.sleep(2)
    results["client_markers_without_country"] = len(lines(CLIENT_LOG, "positions: 1 markers")) > 0
    one = len(lines(CLIENT_LOG, "positions: 1 markers"))
    zero = len(lines(CLIENT_LOG, "positions: 0 markers"))
    api("PUT", f"/v1/players/{uuid}/country", "bench-4", {"country": "north"})
    results["client_marker_after_joining_country_s"] = wait_for(CLIENT_LOG, "positions: 1 markers", one, 10)
    api("PUT", f"/v1/players/{uuid}/country", "bench-5", {"country": None})
    results["client_cleared_after_leaving_country_s"] = wait_for(CLIENT_LOG, "positions: 0 markers", zero, 10)


def launch(game):
    return s5.ctl("launch", "--game", str(game), "--resources", str(s5.RESOURCES), "--game-dir",
                  str(s5.RUN / "client-game"), "--width", "1280", "--height", "720", "--no-pause",
                  "--quickPlayMultiplayer", f"127.0.0.1:{s5.PROXY_PORT}", timeout=240)


def join(logs, server="a"):
    before = len(lines(logs[server], "logged in"))
    return wait_for(logs[server], "logged in", before, 120)


def modded(logs, results, sidecar):
    s5.log("modded client")
    admits = {name: len(lines(path, "[vexmap] admit")) for name, path in logs.items()}
    launch(MODDED)
    # After the launch: each launch starts this log again, so an earlier count would be stale.
    welcomes = len(lines(CLIENT_LOG, "[vexmap] welcomed"))
    results["modded_join_s"] = join(logs)
    s5.ctl("wait-ready", "60", timeout=90)
    results["modded_admit_a_s"] = wait_for(logs["a"], "[vexmap] admit", admits["a"], 20)
    results["modded_welcomed_client"] = wait_for(CLIENT_LOG, "[vexmap] welcomed", welcomes, 10) is not None
    moves = []
    for target in ("b", "a", "b"):
        before_admit = len(lines(logs[target], "[vexmap] admit"))
        before_login = len(lines(logs[target], "logged in"))
        started = time.monotonic()
        s5.ctl("command", f"server {target}")
        login = wait_for(logs[target], "logged in", before_login, 90)
        admit = wait_for(logs[target], "[vexmap] admit", before_admit, 20)
        moves.append({"target": target, "login_s": login,
                      "admit_after_switch_s": None if admit is None else round(time.monotonic() - started, 2)})
        s5.log(json.dumps(moves[-1]))
    results["modded_switches"] = moves
    results["region_a_mask"] = admitted_region_mask(logs["a"])
    results["region_b_mask"] = admitted_region_mask(logs["b"])
    check_feed(results)

    sidecar.kill()
    sidecar.wait(timeout=10)
    s5.log("sidecar killed")
    time.sleep(6)  # at least one failed poll on each server
    before_admit = len(lines(logs["a"], "[vexmap] admit"))
    s5.ctl("command", "server a")
    results["admit_without_sidecar_s"] = wait_for(logs["a"], "[vexmap] admit", before_admit, 30)
    results["region_a_mask_without_sidecar"] = admitted_region_mask(logs["a"])
    results["sidecar_loss_logged"] = bool(lines(logs["a"], "sidecar unavailable"))

    time.sleep(15)  # past the 10 s deadline: no server may kick an admitted player
    results["modded_kicks"] = sum(len(lines(path, "[vexmap] kick")) for path in logs.values())
    s5.ctl("quit")
    time.sleep(5)


def stock(logs, results):
    s5.log("stock client, no vexmap mod")
    kicks = len(lines(logs["a"], "[vexmap] kick"))
    launch(STOCK)
    results["stock_join_s"] = join(logs)
    results["stock_kick_after_join_s"] = wait_for(logs["a"], "[vexmap] kick", kicks, 30)
    results["stock_kick_lines"] = [l[-120:] for l in lines(logs["a"], "[vexmap] kick")][kicks:]
    time.sleep(3)
    results["stock_screen"] = s5.ctl("screen", timeout=20).stdout[-600:]
    s5.ctl("screenshot", str(s5.RUN / "stock-kicked.png"))
    s5.ctl("quit")
    time.sleep(5)


def main():
    if not MODDED.is_file() or not STOCK.is_file() or not SIDECAR.is_file():
        raise SystemExit(f"need {MODDED} (stage.py build mods/vexmap), {SIDECAR} (cargo build) and {STOCK}")
    secret = secrets.token_hex(16)
    processes, logs, results = [], {}, {}
    sidecar = start_sidecar()
    try:
        for name, port in s5.BACKENDS.items():
            region_env = {"VEXMAP_REGION": name, "VEXMAP_API_ADDR": SIDECAR_ADDR, "VEXMAP_FEED_ADDR": FEED_ADDR}
            process, logfile = s5.start_backend(name, port, secret, None, extra_env=region_env)
            processes.append(process)
            logs[name] = logfile
        proxy, proxy_log = s5.start_proxy(secret)
        processes.append(proxy)
        s5.log("servers and proxy up")
        time.sleep(6)  # one poll of the sidecar by each server
        modded(logs, results, sidecar)
        stock(logs, results)
        results["proxy_notable"] = [l[-160:] for l in proxy_log.read_text(encoding="utf-8", errors="replace")
                                    .splitlines() if re.search(r"WARN|ERROR|disconnected|kicked", l)][-10:]
    finally:
        if sidecar.poll() is None:
            sidecar.kill()
        for process, command in zip(processes, ("stop\n", "stop\n", "shutdown\n")):
            try:
                process.stdin.write(command)
                process.stdin.flush()
            except OSError:
                pass
        for process in processes:
            try:
                process.wait(timeout=90)
            except subprocess.TimeoutExpired:
                process.kill()
    passed = bool(results.get("modded_admit_a_s") is not None and results.get("modded_welcomed_client")
                  and all(m["admit_after_switch_s"] is not None for m in results.get("modded_switches", []))
                  and results.get("modded_kicks") == 0 and results.get("stock_kick_after_join_s") is not None
                  and results.get("region_a_mask") == REALTIME_BIT | TILES_BIT
                  and results.get("region_b_mask") == REALTIME_BIT
                  and results.get("admit_without_sidecar_s") is not None
                  and results.get("region_a_mask_without_sidecar") == REALTIME_BIT | TILES_BIT
                  and results.get("sidecar_loss_logged")
                  and results.get("feed_regions") == {"a": 0, "b": 1}
                  and results.get("feed_recorded_name") is True
                  and results.get("client_marker_after_joining_country_s") is not None
                  and results.get("client_cleared_after_leaving_country_s") is not None)
    results["passed"] = passed
    (s5.RUN / "results.json").write_text(json.dumps(results, indent=1, ensure_ascii=False), encoding="utf-8")
    print(json.dumps(results, indent=1, ensure_ascii=False))
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
