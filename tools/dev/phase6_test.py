#!/usr/bin/env python3
"""Phase 6 bench test: a real client follows the route network and draws it with the map editor.

    python tools/dev/stage.py build mods/vexmap
    cargo build -p vexmap-sidecar -p vexmap-resourcepack
    python tools/dev/phase6_test.py

Local only, everything under .deps/run/phase6. Screenshots stay there: they show Minecraft's textures.
"""
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import struct
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))
import s5_measure as s5  # noqa: E402

s5.RUN = s5.DEPS / "run" / "phase6"
import admission_test as admission  # noqa: E402
import phase4_test as phase4  # noqa: E402

s5.RUN = s5.DEPS / "run" / "phase6"
phase4.SHOTS = s5.RUN / "shots"
phase4.CONFIG = s5.RUN / "client-game" / "config" / "vexmap.cfg"

BUTTON = (72, 14)
PANEL_TOP = 40
# The panel's buttons, in the order of `EDITOR_BUTTONS` in editor.rs.
TOGGLE, NODE, LINE, CLOSE, WALK, BOAT, TRAIN, ONE_WAY, STATION = range(9)
# The panel with the editor closed: route for everyone, then what only an admin gets.
ROUTE, OPEN_EDITOR, LOAD_MAP = range(3)
# Blocks per GUI pixel when the map opens.
BLOCKS_PER_PIXEL = 2.0


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


def panel_button(index, size):
    """The centre of a panel button, laid out as `Ui::panel_rects` does in a full-screen map."""
    left, x, y = 6, 6, PANEL_TOP
    for _ in range(index):
        x += BUTTON[0] + 2
        if x + BUTTON[0] > size[0] - 4:
            x, y = left, y + BUTTON[1] + 2
    return x + BUTTON[0] // 2, y + BUTTON[1] // 2


def press(index, size):
    x, y = panel_button(index, size)
    s5.ctl("click-at", str(x), str(y))
    time.sleep(0.4)


def screen_of(size, center, world):
    """Where a world point shows on a flat map centred on `center`."""
    return (round(size[0] / 2 + (world[0] - center[0]) / BLOCKS_PER_PIXEL),
            round(size[1] / 2 + (world[1] - center[1]) / BLOCKS_PER_PIXEL))


def click_world(size, center, world):
    x, y = screen_of(size, center, world)
    s5.ctl("click-at", str(x), str(y))
    time.sleep(0.8)


def network():
    return admission.api("GET", "/v1/network")


def wait_network(check, seconds=8):
    deadline = time.monotonic() + seconds
    current = network()
    while time.monotonic() < deadline:
        current = network()
        if check(current):
            return True, current
        time.sleep(0.15)
    return check(current), current


def client_network_lines():
    """`(nodes, lines, may_edit)` from each `[vexmap] network:` line of the client's log."""
    text = admission.CLIENT_LOG.read_text(encoding="utf-8", errors="replace")
    found = re.findall(r"\[vexmap\] network: nodes=(\d+) lines=(\d+) may_edit=(true|false)", text)
    return [(int(nodes), int(lines), may == "true") for nodes, lines, may in found]


def wait_client(wanted, seconds=10):
    deadline = time.monotonic() + seconds
    started = time.monotonic()
    while time.monotonic() < deadline:
        seen = client_network_lines()
        if seen and seen[-1] == wanted:
            return round(time.monotonic() - started, 2)
        time.sleep(0.1)
    return None


def write_settings():
    """A flat map in English with the auto-tune question answered, so the first click lands where it should."""
    phase4.CONFIG.parent.mkdir(parents=True, exist_ok=True)
    phase4.CONFIG.write_text("map_mode=flat\nautotuned=true\nlanguage=en\n", encoding="utf-8")


def player_state():
    state = json.loads(s5.ctl("state").stdout)
    player = state.get("player", state)
    position = player.get("position") or player.get("pos")
    if isinstance(position, dict):
        return position["x"], position["z"]
    return position[0], position[2]


def player_uuid():
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        for players in admission.positions().values():
            if players:
                return players[0]["uuid"]
        time.sleep(0.2)
    raise SystemExit("the player never reached the sidecar")


def editing(results, size, center):
    """Every tool of the editor, each checked against what the database now holds."""
    x, z = center
    a, b = results["seeded"]
    press(OPEN_EDITOR, size)
    phase4.shot("02-editor-open")

    # The node tool: a click on empty ground places a node there.
    press(NODE, size)
    spot = (x + 20, z + 80)
    click_world(size, center, spot)
    placed, state = wait_network(lambda net: len(net["nodes"]) == 3)
    results["node_tool_places"] = placed
    new = next((n for n in state["nodes"] if n["id"] not in (a, b)), None)
    results["node_within_snap"] = new is not None and abs(new["x"] - spot[0]) <= 3 and abs(new["z"] - spot[1]) <= 3
    # Clicking the same place again finds the node: nothing new, and the cursor is left on it, so it shows lit.
    click_world(size, center, spot)
    time.sleep(0.5)
    phase4.shot("03-node-under-cursor")
    results["no_second_node_on_a_node"] = len(network()["nodes"]) == 3

    # The line tool: node to node, snapping to a click a few blocks off.
    press(LINE, size)
    node_a = next(n for n in state["nodes"] if n["id"] == a)
    click_world(size, center, (node_a["x"] + 4, node_a["z"] - 4))
    click_world(size, center, spot)
    joined, state = wait_network(lambda net: len(net["lines"]) == 2)
    results["line_tool_joins_nodes"] = joined and any({l["a"], l["b"]} == {a, new["id"]} for l in state["lines"])

    # Drawing goes on from the node just reached: empty ground makes the node and then the line.
    press(TRAIN, size)
    press(ONE_WAY, size)
    far = (x - 60, z + 90)
    click_world(size, center, far)
    chained, state = wait_network(lambda net: len(net["nodes"]) == 4 and len(net["lines"]) == 3)
    results["chain_places_node_then_line"] = chained
    last = state["lines"][-1] if state["lines"] else {}
    results["chain_line_kind"] = (last.get("mode"), last.get("one_way"))
    time.sleep(0.5)
    phase4.shot("04-rubber-band")

    # Escape drops the half-drawn line and stays in the editor.
    s5.ctl("key", "esc", "tap")
    time.sleep(0.5)
    results["escape_keeps_the_map"] = phase4.screen_id() == "mod"

    # The closing tool, on the seeded road's middle.
    press(CLOSE, size)
    first_line = state["lines"][0]
    ends = [n for n in state["nodes"] if n["id"] in (first_line["a"], first_line["b"])]
    click_world(size, center, ((ends[0]["x"] + ends[1]["x"]) / 2, (ends[0]["z"] + ends[1]["z"]) / 2))
    closed, state = wait_network(lambda net: net["lines"][0]["closed"])
    results["close_tool_closes"] = closed
    time.sleep(2.5)  # The next poll brings the closed state back to the client.
    phase4.shot("05-closed")

    # A drag pans and draws nothing.
    before = len(network()["nodes"])
    press(NODE, size)
    drag_from = screen_of(size, center, (x + 90, z - 60))
    s5.ctl("drag-at", str(drag_from[0]), str(drag_from[1]), str(drag_from[0] + 40), str(drag_from[1] + 10))
    time.sleep(1)
    results["drag_draws_nothing"] = len(network()["nodes"]) == before
    phase4.shot("06-after-drag")

    # Leaving the editor with Escape keeps the map open, and the next one closes it.
    s5.ctl("key", "esc", "tap")
    time.sleep(0.4)
    results["escape_leaves_the_editor"] = phase4.screen_id() == "mod"
    s5.ctl("key", "esc", "tap")
    results["escape_then_closes_the_map"] = phase4.wait_screen("playing")


def decode_tile(path):
    """The pixels of a stored tile, or None. Control byte below 128: that many plus one literals, else a repeated byte."""
    try:
        data = path.read_bytes()[4:]
    except OSError:
        return None
    out, at = bytearray(), 0
    while at < len(data):
        control = data[at]
        at += 1
        if control < 128:
            out += data[at:at + control + 1]
            at += control + 1
        else:
            out += bytes([data[at]]) * (control - 125)
            at += 1
    return out if len(out) == 65536 else None


def material_at(root, level, x, z):
    """The material the tile store at `root` holds for a block, or None without a tile."""
    span = 256 << level
    tile = decode_tile(root / str(level) / f"{x // span}.{z // span}.vxt")
    if tile is None:
        return None
    return tile[((z % span) >> level) * 256 + ((x % span) >> level)] & 15


ICE = 7


def first_true(check, seconds):
    """Seconds until `check` holds, or None."""
    started = time.monotonic()
    while time.monotonic() - started < seconds:
        if check():
            return round(time.monotonic() - started, 2)
        time.sleep(0.1)
    return None


def live_map(results, server):
    """A block changed and not saved reaches the sidecar's tiles and then the client. No world folder is scanned here."""
    tiles = s5.RUN / "tiles" / "public" / "minecraft~overworld"
    cached = s5.RUN / "client-game" / "config" / "tiles" / "minecraft~overworld"
    x, z = (round(value) for value in player_state())
    x, z = x + 24, z + 24
    # The client shows a coarser level while zoomed out, so any level of its copy counts.
    in_client = lambda wanted: any(wanted(material_at(cached, level, x, z)) for level in (0, 1, 2))  # noqa: E731
    results["live_ice_before"] = in_client(lambda found: found == ICE)
    server.stdin.write(f"fill {x - 12} 250 {z - 12} {x + 12} 250 {z + 12} minecraft:blue_ice\n")
    server.stdin.flush()
    results["live_change_in_store_s"] = first_true(lambda: material_at(tiles, 0, x, z) == ICE, 30)
    results["live_change_in_client_s"] = first_true(lambda: in_client(lambda found: found == ICE), 20)
    phase4.shot("08-live-change")
    server.stdin.write(f"fill {x - 12} 250 {z - 12} {x + 12} 250 {z + 12} minecraft:air\n")
    server.stdin.flush()
    results["live_removal_in_store_s"] = first_true(lambda: material_at(tiles, 0, x, z) != ICE, 30)
    results["live_removal_in_client_s"] = first_true(lambda: in_client(lambda found: found not in (ICE, None)), 20)
    time.sleep(0.5)
    phase4.shot("09-live-removal")


def client_log(needle):
    return admission.lines(admission.CLIENT_LOG, needle)


def follow_route(results, uuid):
    """Asks for a route on the map, and has the sidecar told to hurry by the load button."""
    s5.ctl("key", "m", "tap")
    results["map_reopened"] = phase4.wait_screen("mod")
    time.sleep(1)
    size = gui_size()
    center = player_state()
    press(LOAD_MAP, size)
    time.sleep(1)
    before = len(client_log("[vexmap] route:"))
    press(ROUTE, size)
    click_world(size, center, (center[0] + 60, center[1] + 40))
    results["route_built_s"] = first_true(lambda: len(client_log("[vexmap] route:")) > before, 8)
    results["route_line"] = (client_log("[vexmap] route:") or [""])[-1][-30:]
    time.sleep(1)
    phase4.shot("10-route")
    s5.ctl("key", "esc", "tap")
    phase4.wait_screen("playing")
    s5.ctl("look", "0", "20")
    time.sleep(1.5)
    phase4.shot("11-arrow-in-the-world")
    s5.ctl("key", "m", "tap")
    phase4.wait_screen("mod")
    time.sleep(1)
    press(ROUTE, size)  # Drops it again.
    time.sleep(0.5)
    s5.ctl("key", "esc", "tap")
    results["map_closed_after_route"] = phase4.wait_screen("playing")


def run():
    secret = secrets.token_hex(16)
    processes, logs, results = [], {}, {}
    write_settings()
    os.environ["VEXMAP_DIAG"] = "1"
    os.environ["VEXMAP_TILE_DIR"] = str(s5.RUN / "tiles")
    shutil.rmtree(s5.RUN / "tiles", ignore_errors=True)
    sidecar = admission.start_sidecar()
    admission.api("PUT", "/v1/features/routing", "bench-routing", {"enabled": True})
    pack_dir, sha1 = phase4.make_pack()
    pack_server, _ = s5.serve(pack_dir)
    pack_env = {"VEXMAP_PACK_URL": f"http://127.0.0.1:{s5.PACK_PORT}/vexmap-pack.zip", "VEXMAP_PACK_SHA1": sha1}
    touched = None
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
        admission.CLIENT_LOG.unlink(missing_ok=True)
        admission.launch(admission.MODDED)
        results["join_s"] = admission.join(logs)
        results["pack_loaded_s"], _ = phase4.accept_prompt_until(logs["a"], "Loaded", before, 60)
        s5.ctl("wait-ready", "60", timeout=90)
        time.sleep(3)
        admit = admission.lines(logs["a"], "[vexmap] admit")
        results["routing_granted"] = bool(admit) and re.search(r"features (0x[0-9a-f]+)", admit[-1]) is not None \
            and int(re.search(r"features (0x[0-9a-f]+)", admit[-1]).group(1), 16) & 4 != 0

        # Lines drawn elsewhere reach the client that is already playing, without an admin among them.
        uuid = player_uuid()
        x, z = (round(value) for value in player_state())
        a = admission.api("POST", "/v1/network/nodes", "seed-a", {"x": x - 70, "z": z - 30})["seq"]
        b = admission.api("POST", "/v1/network/nodes", "seed-b", {"x": x + 70, "z": z - 30, "transfer": True})["seq"]
        admission.api("POST", "/v1/network/lines", "seed-l", {"a": a, "b": b, "mode": "walk"})
        results["seeded"] = (a, b)

        s5.ctl("command", "map")
        results["map_opens"] = phase4.wait_screen("mod")
        results["seed_reaches_client_s"] = wait_client((2, 1, False), 40)
        time.sleep(1)
        phase4.shot("01-no-editor-for-a-player")

        # The role is read on every poll: no reconnect is needed.
        admission.api("PUT", f"/v1/players/{uuid}/role", "bench-admin", {"role": "admin", "categories": []})
        results["admin_seen_by_client_s"] = wait_client((2, 1, True), 10)
        time.sleep(0.5)

        center = player_state()
        size = gui_size()
        results["gui"] = size
        editing(results, size, center)

        live_map(results, processes[0])
        follow_route(results, uuid)

        s5.ctl("key", "m", "tap")  # Open again: the copy and the view survive.
        phase4.wait_screen("mod")
        time.sleep(1)
        phase4.shot("07-reopened")
        s5.ctl("key", "esc", "tap")
        phase4.wait_screen("playing")

        final = network()
        results["final_counts"] = (len(final["nodes"]), len(final["lines"]))
        journal = admission.api("GET", "/v1/journal?after=0&limit=200")
        entries = journal["entries"] if isinstance(journal, dict) else journal
        made = [entry for entry in entries if entry["kind"].startswith("route.") and uuid in entry["actor"]]
        results["edits_journaled_as_the_player"] = len(made)
        results["client_last_network"] = client_network_lines()[-1:]
        results["kicks"] = sum(len(admission.lines(path, "[vexmap] kick")) for path in logs.values())
        results["tick_reports"] = [line[-60:] for line in admission.lines(logs["a"], "tick us:")][-3:]
        text = admission.CLIENT_LOG.read_text(encoding="utf-8", errors="replace")
        touched = text.count("input from a person while an agent was driving")
        s5.ctl("quit")
        time.sleep(4)
    finally:
        phase4.finish(sidecar, processes, pack_server)
    results["person_touched"] = touched
    checks = ("routing_granted", "map_opens", "seed_reaches_client_s", "admin_seen_by_client_s", "node_tool_places",
              "node_within_snap", "no_second_node_on_a_node", "line_tool_joins_nodes", "chain_places_node_then_line",
              "escape_keeps_the_map", "close_tool_closes", "drag_draws_nothing", "escape_leaves_the_editor",
              "escape_then_closes_the_map", "map_reopened", "map_closed_after_route", "live_change_in_store_s", "live_change_in_client_s")
    timings = ("seed_reaches_client_s", "admin_seen_by_client_s", "route_built_s", "live_change_in_store_s", "live_change_in_client_s", "live_removal_in_store_s", "live_removal_in_client_s")
    results["failed"] = [key for key in checks if not results.get(key) and key not in timings]
    results["failed"] += [key for key in timings if results.get(key) is None]
    if results.get("live_ice_before"):
        results["failed"].append("live_ice_before")
    results["passed"] = not results["failed"] and results.get("kicks") == 0
    (s5.RUN / "results.json").write_text(json.dumps(results, indent=1, ensure_ascii=False), encoding="utf-8")
    print(json.dumps(results, indent=1, ensure_ascii=False))
    return 0 if results["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(run())
