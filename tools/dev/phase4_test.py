#!/usr/bin/env python3
"""Phase 4 bench test: the required resource pack through Velocity, the map screen, the minimap, and the tick's cost.

    python tools/dev/stage.py build mods/vexmap
    cargo build -p vexmap-sidecar -p vexmap-resourcepack
    python tools/dev/phase4_test.py            # pack and interface, one real client
    python tools/dev/phase4_test.py ticks      # what the mod costs the tick, with bots

Local only, everything under .deps/run/phase4. Screenshots stay there: they show Minecraft's textures.
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

s5.RUN = s5.DEPS / "run" / "phase4"
import admission_test as admission  # noqa: E402

s5.RUN = s5.DEPS / "run" / "phase4"
PACK_TOOL = s5.ROOT / "target" / "debug" / "vexmap-resourcepack.exe"
CONFIG = s5.RUN / "client-game" / "config" / "vexmap.cfg"
SHOTS = s5.RUN / "shots"
GUI = (427, 240)  # 1280x720 at GUI scale 3, the automatic scale for this window.


def lines(path, needle):
    return admission.lines(path, needle)


def make_pack():
    directory = s5.RUN / "pack"
    subprocess.run([str(PACK_TOOL), str(directory)], check=True, capture_output=True)
    return directory, (directory / "vexmap-pack.sha1").read_text().strip()


def shot(name):
    SHOTS.mkdir(parents=True, exist_ok=True)
    s5.ctl("screenshot", str(SHOTS / f"{name}.png"))
    time.sleep(0.6)


def accept_prompt_until(log, needle, before, seconds):
    """Clicks the pack prompt whenever it is up. Returns (seconds until the server logged `needle`, prompts clicked)."""
    started, clicked = time.monotonic(), 0
    while time.monotonic() - started < seconds:
        if s5.answer_pack_prompt():
            clicked += 1
        if len(lines(log, needle)) > before:
            return round(time.monotonic() - started, 2), clicked
        time.sleep(0.2)
    return None, clicked


def screen_id():
    try:
        return json.loads(s5.ctl("screen").stdout).get("screen")
    except (ValueError, AttributeError):
        return None


def wait_screen(wanted, seconds=5):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if screen_id() == wanted:
            return True
        time.sleep(0.15)
    return False


def move_between_servers(logs, results):
    moves = []
    for target in ("b", "a"):
        before = len(lines(logs[target], "Loaded"))
        started = time.monotonic()
        s5.ctl("command", f"server {target}")
        loaded, clicked = accept_prompt_until(logs[target], "Loaded", before, 60)
        moves.append({"target": target, "pack_loaded_s": loaded, "prompts": clicked,
                      "total_s": round(time.monotonic() - started, 2)})
        s5.ctl("wait-ready", "60", timeout=90)
    results["moves"] = moves


def interface(results):
    s5.ctl("wait-ready", "60", timeout=90)
    shot("01-hud-minimap")
    results["first_run_no_config"] = not CONFIG.exists()

    s5.ctl("key", "m", "tap")
    results["key_opens_map"] = wait_screen("mod")
    time.sleep(0.5)
    shot("02-map-first-run")
    # "No" of the auto-tune offer: the second button, centred at the top of the full-screen map.
    s5.ctl("click-at", str(GUI[0] // 2 + 44), "49")
    time.sleep(1.2)
    results["config_saved_after_answer"] = CONFIG.exists()
    shot("03-map")
    s5.ctl("scroll-at", "200", "120", "6")
    time.sleep(0.4)
    s5.ctl("drag-at", "200", "120", "260", "160")
    time.sleep(0.4)
    shot("04-map-zoom-pan")
    s5.ctl("click-at", str(GUI[0] - 148 + 40), "10")  # Settings.
    time.sleep(0.5)
    shot("05-settings")
    s5.ctl("click-at", str(GUI[0] // 2 - 100), "36")  # The first row: language.
    time.sleep(0.8)
    shot("06-settings-language")
    s5.ctl("click-at", "20", "12")  # Back.
    time.sleep(0.3)
    s5.ctl("key", "m", "tap")
    results["key_closes_map"] = wait_screen("playing")

    s5.ctl("command", "map")
    results["command_opens_map"] = wait_screen("mod")
    s5.ctl("key", "esc", "tap")
    results["escape_closes_map"] = wait_screen("playing")

    s5.ctl("pause")
    time.sleep(0.5)
    shot("07-pause-button")
    s5.ctl("click-at", "40", "14")
    results["pause_button_opens_map"] = wait_screen("mod")
    time.sleep(0.4)
    s5.ctl("key", "esc", "tap")
    time.sleep(0.4)
    s5.ctl("key", "n", "tap")
    time.sleep(1.0)
    shot("08-minimap-off")
    saved = CONFIG.read_text(encoding="utf-8") if CONFIG.exists() else ""
    results["minimap_key_saved"] = "minimap.enabled=false" in saved
    results["language_saved"] = "language=en" in saved
    s5.ctl("key", "n", "tap")
    results["tree_of_settings"] = saved.count("\n")


def run_interface():
    secret = secrets.token_hex(16)
    processes, logs, results = [], {}, {}
    CONFIG.unlink(missing_ok=True)
    sidecar = admission.start_sidecar()
    pack_dir, sha1 = make_pack()
    server, hits = s5.serve(pack_dir)
    pack_env = {"VEXMAP_PACK_URL": f"http://127.0.0.1:{s5.PACK_PORT}/vexmap-pack.zip", "VEXMAP_PACK_SHA1": sha1}
    try:
        for name, port in s5.BACKENDS.items():
            env = {"VEXMAP_REGION": name, "VEXMAP_API_ADDR": admission.SIDECAR_ADDR,
                   "VEXMAP_FEED_ADDR": admission.FEED_ADDR, **pack_env}
            process, logfile = s5.start_backend(name, port, secret, None, extra_env=env)
            processes.append(process)
            logs[name] = logfile
        proxy, _ = s5.start_proxy(secret)
        processes.append(proxy)
        time.sleep(6)
        before = len(lines(logs["a"], "Loaded"))
        admission.launch(admission.MODDED)
        results["join_s"] = admission.join(logs)
        results["pack_loaded_s"], results["first_join_prompts"] = accept_prompt_until(logs["a"], "Loaded", before, 60)
        results["pack_requests_first_join"] = len(hits)
        move_between_servers(logs, results)
        results["pack_requests_after_moves"] = len(hits)
        shot("00-after-moves")
        interface(results)
        results["kicks"] = sum(len(lines(path, "[vexmap] kick")) for path in logs.values())
        results["tick_reports"] = [l[-60:] for l in lines(logs["a"], "tick us:")]
        s5.ctl("quit")
        time.sleep(5)

        # A second player's first join, declining the pack. `client-decline` keeps the first client's answer out.
        kicks = len(lines(logs["a"], "[vexmap] kick"))
        s5.ctl("launch", "--game", str(admission.MODDED), "--resources", str(s5.RESOURCES), "--game-dir",
               str(s5.RUN / "client-decline"), "--width", "1280", "--height", "720", "--no-pause",
               "--quickPlayMultiplayer", f"127.0.0.1:{s5.PROXY_PORT}", timeout=240)
        started, declined = time.monotonic(), False
        while time.monotonic() - started < 60 and not declined:
            for label in ("No", "Disconnect", "Нет", "Отключиться"):
                result = s5.ctl("click-button", label, timeout=20)
                if result.returncode == 0 and '"ok":true' in result.stdout:
                    declined = True
                    break
            time.sleep(0.3)
        results["decline_clicked"] = declined
        time.sleep(3)
        # A pack sent as required: the client refuses to stay by itself, before the server hears an answer.
        results["decline_screen"] = s5.ctl("screen", timeout=20).stdout[-400:]
        results["decline_disconnected"] = "requires a custom resource pack" in results["decline_screen"]
        shot("09-declined")
        s5.ctl("quit")
        time.sleep(4)

        # A pack host that is down: the client accepts, the download fails, and the server must say so.
        server.shutdown()
        server.server_close()
        s5.ctl("launch", "--game", str(admission.MODDED), "--resources", str(s5.RESOURCES), "--game-dir",
               str(s5.RUN / "client-nohost"), "--width", "1280", "--height", "720", "--no-pause",
               "--quickPlayMultiplayer", f"127.0.0.1:{s5.PROXY_PORT}", timeout=240)
        results["failed_download_kick_s"], _ = accept_prompt_until(logs["a"], "[vexmap] kick", kicks, 60)
        results["failed_download_kick"] = [l[-70:] for l in lines(logs["a"], "[vexmap] kick")][kicks:]
        time.sleep(3)
        results["failed_download_screen"] = s5.ctl("screen", timeout=20).stdout[-300:]
        s5.ctl("quit")
        time.sleep(4)
    finally:
        finish(sidecar, processes, server)
    keys = ("key_opens_map", "key_closes_map", "command_opens_map", "escape_closes_map", "pause_button_opens_map",
            "config_saved_after_answer", "minimap_key_saved", "language_saved")
    results["passed"] = bool(results.get("pack_loaded_s") is not None and all(results.get(key) for key in keys)
                             and all(m["pack_loaded_s"] is not None for m in results.get("moves", []))
                             and results.get("kicks") == 0 and results.get("decline_disconnected")
                             and results.get("failed_download_kick_s") is not None)
    report(results, "results.json")
    return 0 if results["passed"] else 1


def finish(sidecar, processes, server):
    if sidecar.poll() is None:
        sidecar.kill()
    server.shutdown()  # Harmless when the test already stopped it.
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


def report(results, name):
    (s5.RUN / name).write_text(json.dumps(results, indent=1, ensure_ascii=False), encoding="utf-8")
    print(json.dumps(results, indent=1, ensure_ascii=False))


def tick_reports(path):
    found = re.findall(r"tick us: n=(\d+) p50=(\d+) p99=(\d+) max=(\d+)", path.read_text(encoding="utf-8", errors="replace"))
    return [dict(zip(("n", "p50", "p99", "max"), map(int, row))) for row in found]


def run_ticks():
    """Bots with admission waived, so the whole path runs: list of players, samples, feed, and country frames to them."""
    secret = secrets.token_hex(16)
    results = {}
    sidecar = admission.start_sidecar()
    port = s5.BACKENDS["a"]
    bench = {"VEXMAP_REGION": "a", "VEXMAP_API_ADDR": admission.SIDECAR_ADDR, "VEXMAP_FEED_ADDR": admission.FEED_ADDR,
             "VEXMAP_BENCH_ADMIT_ALL": "1"}
    # Writes server.properties and the EULA answer; the runs below connect directly, without the proxy.
    process, logfile = s5.start_backend("a", port, secret, None, extra_env=bench)
    process.stdin.write("stop\n")
    process.stdin.flush()
    process.wait(timeout=60)
    (s5.RUN / "a" / "forwarding.secret").unlink(missing_ok=True)
    process = None
    try:
        for bots in (1, 20):
            process, logfile = start_direct(bench, bots)
            command = [str(s5.BIN / "horizon-bots.exe"), "--server", f"127.0.0.1:{port}", "--bots", str(bots),
                       "--behaviour", "walk", "--join-every", "0.25", "--seconds", "60", "--brand", "horizon",
                       "--report", str(s5.RUN / f"bots-{bots}.json")]
            swarm = subprocess.Popen(command, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            time.sleep(25)
            put_players_in_a_country()
            swarm.wait(timeout=120)
            process.stdin.write("stop\n")
            process.stdin.flush()
            process.wait(timeout=90)
            process = None
            results[f"bots_{bots}"] = tick_reports(logfile)
    finally:
        if process is not None:
            process.kill()
        if sidecar.poll() is None:
            sidecar.kill()
    report(results, "ticks.json")
    return 0


def start_direct(bench, bots):
    directory = s5.RUN / "a"
    properties = (directory / "server.properties").read_text()
    if "max-players=" not in properties:
        (directory / "server.properties").write_text(properties + "max-players=200\n")
    env = dict(os.environ, HORIZON_NO_AUDIO="1", HORIZON_HOME=str(directory / "home"), **bench)
    logfile = directory / f"server-{bots}.log"
    process = subprocess.Popen([str(s5.BIN / "horizon-client.exe"), "--resources", str(s5.RESOURCES), "--dedicated", "--nogui"],
                               cwd=directory, env=env, stdin=subprocess.PIPE, stdout=logfile.open("w", encoding="utf-8"),
                               stderr=subprocess.STDOUT, text=True)
    s5.wait_port(s5.BACKENDS["a"], process, "server a")
    return process, logfile


def put_players_in_a_country():
    seen = admission.positions().get("a", [])
    for index, player in enumerate(seen):
        admission.api("PUT", f"/v1/players/{player['uuid']}/country", f"tick-{player['uuid']}-{index}", {"country": "north"})


if __name__ == "__main__":
    raise SystemExit(run_ticks() if sys.argv[1:] == ["ticks"] else run_interface())
