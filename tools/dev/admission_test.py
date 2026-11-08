#!/usr/bin/env python3
"""Phase 1 bench test (ADR-007): admission through Velocity, with the real vexmap mod.

1. A client with the mod joins server a through the proxy and is admitted (`[vexmap] admit`), then is moved to b and
   back: each server admits it again (the `Probe` path, since the client cannot see a proxy switch).
2. The launcher's stock Horizon client (same version, no vexmap mod) joins and must be kicked with our message.

Local only, everything under .deps/run/admission. Reuses the S5 harness (two servers, Velocity, horizon-ctl).

    python tools/dev/stage.py build mods/vexmap
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

sys.path.insert(0, str(Path(__file__).resolve().parent))
import s5_measure as s5  # noqa: E402

s5.RUN = s5.DEPS / "run" / "admission"
MODDED = s5.BIN / "horizon-client.exe"
STOCK = s5.VERSION / "horizon-client.exe"
CLIENT_LOG = Path(os.environ["TEMP"]) / "horizon-control.log"


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


def launch(game):
    return s5.ctl("launch", "--game", str(game), "--resources", str(s5.RESOURCES), "--game-dir",
                  str(s5.RUN / "client-game"), "--width", "1280", "--height", "720", "--no-pause",
                  "--quickPlayMultiplayer", f"127.0.0.1:{s5.PROXY_PORT}", timeout=240)


def join(logs, server="a"):
    before = len(lines(logs[server], "logged in"))
    return wait_for(logs[server], "logged in", before, 120)


def modded(logs, results):
    s5.log("modded client")
    admits = {name: len(lines(path, "[vexmap] admit")) for name, path in logs.items()}
    welcomes = len(lines(CLIENT_LOG, "[vexmap] welcomed"))
    launch(MODDED)
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
                      "admit_after_switch_s": admit and round(time.monotonic() - started, 2)})
        s5.log(json.dumps(moves[-1]))
    results["modded_switches"] = moves
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
    if not MODDED.is_file() or not STOCK.is_file():
        raise SystemExit(f"need {MODDED} (stage.py build mods/vexmap) and {STOCK}")
    secret = secrets.token_hex(16)
    processes, logs, results = [], {}, {}
    try:
        for name, port in s5.BACKENDS.items():
            process, logfile = s5.start_backend(name, port, secret, None)
            processes.append(process)
            logs[name] = logfile
        proxy, proxy_log = s5.start_proxy(secret)
        processes.append(proxy)
        s5.log("servers and proxy up")
        modded(logs, results)
        stock(logs, results)
        results["proxy_notable"] = [l[-160:] for l in proxy_log.read_text(encoding="utf-8", errors="replace")
                                    .splitlines() if re.search(r"WARN|ERROR|disconnected|kicked", l)][-10:]
    finally:
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
                  and all(m["admit_after_switch_s"] for m in results.get("modded_switches", []))
                  and results.get("modded_kicks") == 0 and results.get("stock_kick_after_join_s") is not None)
    results["passed"] = passed
    (s5.RUN / "results.json").write_text(json.dumps(results, indent=1, ensure_ascii=False), encoding="utf-8")
    print(json.dumps(results, indent=1, ensure_ascii=False))
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
