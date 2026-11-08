#!/usr/bin/env python3
"""Phase 5 bench test: tiles from real region files reach a real client and show in all three map modes.

    python tools/dev/stage.py build mods/vexmap
    cargo build -p vexmap-sidecar -p vexmap-resourcepack
    python tools/dev/phase5_test.py

Local only, everything under .deps/run/phase5. Screenshots stay there: they show Minecraft's textures.
"""
import json
import os
from pathlib import Path
import re
import secrets
import statistics
import struct
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))
import s5_measure as s5  # noqa: E402

s5.RUN = s5.DEPS / "run" / "phase5"
import admission_test as admission  # noqa: E402
import phase4_test as phase4  # noqa: E402

s5.RUN = s5.DEPS / "run" / "phase5"
phase4.SHOTS = s5.RUN / "shots"
phase4.CONFIG = s5.RUN / "client-game" / "config" / "vexmap.cfg"
TILES = s5.RUN / "tiles"


def gui_size():
    """The GUI's size right now, from a screenshot: the window can be resized while the test runs."""
    probe = s5.RUN / "probe.png"
    s5.ctl("screenshot", str(probe))
    time.sleep(0.6)
    width, height = struct.unpack(">II", probe.read_bytes()[16:24])
    scale = 1
    while width // (scale + 1) >= 320 and height // (scale + 1) >= 240:
        scale += 1
    return width // scale, height // scale


def click_view_button():
    """148 from the right edge, under the settings button."""
    width, _ = gui_size()
    s5.ctl("click-at", str(width - 148 + 45), "29")


def map_mode_saved():
    deadline = time.monotonic() + 3
    while time.monotonic() < deadline:
        found = re.search(r"map_mode=(\w+)", phase4.CONFIG.read_text(encoding="utf-8"))
        if found:
            return found.group(1)
        time.sleep(0.1)
    return None


def tile_files():
    return sum(1 for _ in TILES.rglob("*.vxt"))


def fps_reports(state):
    """The rates the mod logged while the map was `state` (open or closed), first report of a run dropped."""
    text = admission.CLIENT_LOG.read_text(encoding="utf-8", errors="replace")
    found = [int(value) for value in re.findall(rf"\[vexmap\] fps map={state} (\d+)", text)]
    return found[1:]


def uncap_frame_rate():
    """The game's limiter hides what the map costs. Only this test's own client folder is touched."""
    options = s5.RUN / "client-game" / "options.txt"
    if options.exists():
        text = options.read_text(encoding="utf-8")
        text = re.sub(r"^enableVsync:.*$", "enableVsync:false", text, flags=re.M)
        text = re.sub(r"^maxFps:.*$", "maxFps:260", text, flags=re.M)
        options.write_text(text, encoding="utf-8")


def person_touched():
    """Horizon notes input from a person during an agent's run. Such a run says nothing about the steps it disturbed."""
    text = admission.CLIENT_LOG.read_text(encoding="utf-8", errors="replace")
    return text.count("input from a person while an agent was driving")


def wait_for_tiles(server, seconds=120):
    """Saves the world until the sidecar's scan has drawn something."""
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        server.stdin.write("save-all flush\n")
        server.stdin.flush()
        time.sleep(8)
        if tile_files() > 0:
            return True
    return False


def run():
    secret = secrets.token_hex(16)
    processes, logs, results = [], {}, {}
    phase4.CONFIG.unlink(missing_ok=True)
    os.environ["VEXMAP_WORLD_DIRS"] = str(s5.RUN / "a" / "world")
    os.environ["VEXMAP_TILE_DIR"] = str(TILES)
    os.environ["VEXMAP_DIAG"] = "1"
    uncap_frame_rate()
    sidecar = admission.start_sidecar()
    pack_dir, sha1 = phase4.make_pack()
    pack_server, _ = s5.serve(pack_dir)
    pack_env = {"VEXMAP_PACK_URL": f"http://127.0.0.1:{s5.PACK_PORT}/vexmap-pack.zip", "VEXMAP_PACK_SHA1": sha1}
    try:
        for name, port in s5.BACKENDS.items():
            env = {"VEXMAP_REGION": name, "VEXMAP_API_ADDR": admission.SIDECAR_ADDR,
                   "VEXMAP_FEED_ADDR": admission.FEED_ADDR, "VEXMAP_MAP_RADIUS": "2000", **pack_env}
            process, logfile = s5.start_backend(name, port, secret, None, extra_env=env)
            processes.append(process)
            logs[name] = logfile
        proxy, _ = s5.start_proxy(secret)
        processes.append(proxy)
        time.sleep(6)
        before = len(admission.lines(logs["a"], "Loaded"))
        admission.launch(admission.MODDED)
        results["join_s"] = admission.join(logs)
        results["pack_loaded_s"], _ = phase4.accept_prompt_until(logs["a"], "Loaded", before, 60)
        s5.ctl("wait-ready", "60", timeout=90)
        time.sleep(12)  # The world around the player finishes generating.
        results["tiles_drawn"] = wait_for_tiles(processes[0])
        results["tile_files"] = tile_files()
        time.sleep(32)  # The next scan picks up what the save above left.
        results["tile_files_after_second_scan"] = tile_files()
        time.sleep(11)
        results["fps_closed"] = fps_reports("closed")

        s5.ctl("key", "m", "tap")
        results["key_opens_map"] = phase4.wait_screen("mod")
        results["screen_before_retry"] = phase4.screen_id()
        if not results["key_opens_map"]:  # The window may not have had focus for the first tap.
            s5.ctl("command", "map")
            results["command_opens_map"] = phase4.wait_screen("mod")
        time.sleep(0.5)
        s5.ctl("click-at", str(gui_size()[0] // 2 + 44), "49")  # "No" to the auto-tune offer.
        time.sleep(6)
        phase4.shot("01-globe")
        middle = [str(value // 2) for value in gui_size()]
        s5.ctl("scroll-at", *middle, "14")
        time.sleep(4)
        phase4.shot("02-globe-near")
        before_fps = len(fps_reports("open"))
        time.sleep(16)
        results["fps_open_globe"] = fps_reports("open")[before_fps:]
        click_view_button()
        time.sleep(4)
        results["mode_after_one_click"] = map_mode_saved()
        phase4.shot("03-round")
        click_view_button()
        time.sleep(4)
        results["mode_after_two_clicks"] = map_mode_saved()
        phase4.shot("04-flat")
        s5.ctl("scroll-at", *[str(value // 2) for value in gui_size()], "8")
        time.sleep(4)
        phase4.shot("05-flat-near")
        s5.ctl("key", "esc", "tap")
        phase4.wait_screen("playing")
        time.sleep(4)
        phase4.shot("06-minimap-tiles")
        s5.ctl("key", "m", "tap")
        phase4.wait_screen("mod")
        s5.ctl("key", "esc", "tap")
        results["escape_closes_map"] = phase4.wait_screen("playing")
        results["kicks"] = sum(len(admission.lines(path, "[vexmap] kick")) for path in logs.values())
        results["tick_reports"] = [line[-60:] for line in admission.lines(logs["a"], "tick us:")][-4:]
        results["render_errors"] = [line[-200:] for line in admission.CLIENT_LOG.read_text(
            encoding="utf-8", errors="replace").splitlines() if "wgpu" in line.lower() and "error" in line.lower()][:5]
        touched = person_touched()
        s5.ctl("quit")
        time.sleep(4)
    finally:
        phase4.finish(sidecar, processes, pack_server)
    for key in ("fps_closed", "fps_open_globe"):
        results[f"{key}_median"] = statistics.median(results[key]) if results.get(key) else None
    results["person_touched"] = touched
    results["passed"] = bool(results.get("tiles_drawn") and (results.get("key_opens_map") or results.get("command_opens_map")) and results.get("kicks") == 0
                             and not results.get("render_errors") and results.get("mode_after_one_click") == "round"
                             and results.get("mode_after_two_clicks") == "flat")
    (s5.RUN / "results.json").write_text(json.dumps(results, indent=1, ensure_ascii=False), encoding="utf-8")
    print(json.dumps(results, indent=1, ensure_ascii=False))
    return 0 if results["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(run())
