#!/usr/bin/env python3
"""Spike S5: one Velocity proxy, two Horizon servers, one real client. Does mod_net pass, and what does a switch cost?

Local only. Everything lives under .deps/run/s5. A throwaway random forwarding secret and a home-made pack, nothing else.

    python tools/dev/s5_measure.py plain     # no resource pack
    python tools/dev/s5_measure.py pack      # a required server resource pack on both servers
"""
import hashlib
import http.server
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import socket
import subprocess
import sys
import threading
import time
import uuid
import zipfile

ROOT = Path(__file__).resolve().parents[2]
DEPS = Path(os.environ.get("VEXMAP_DEPS", ROOT / ".deps"))
BIN = DEPS / "target" / "fast"
VERSION = Path(os.environ["APPDATA"]) / "horizon-launcher" / "versions" / "nightly-2026-11-07-6560641"
RESOURCES = VERSION / ".local" / "horizon-client" / "26.3"
CTL = VERSION / "horizon-ctl.exe"
JAVA = Path(os.environ["APPDATA"]) / "horizon-launcher/minecraft/runtime/java-runtime-epsilon/windows-x64/java-runtime-epsilon/bin/java.exe"
JAR = DEPS / "tools" / "velocity" / "velocity.jar"
RUN = DEPS / "run" / "s5"
PROXY_PORT = 25599
PACK_PORT = 25610
BACKENDS = {"a": 25601, "b": 25602}
SWITCHES = 6


def log(message):
    print(f"[s5 {time.strftime('%H:%M:%S')}] {message}", flush=True)


def wait_port(port, process, what, seconds=240):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise SystemExit(f"{what} stopped with {process.returncode}")
        try:
            socket.create_connection(("127.0.0.1", port), timeout=1).close()
            return
        except OSError:
            time.sleep(0.5)
    process.kill()
    raise SystemExit(f"{what} did not open port {port}")


def make_pack(directory):
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / "vexmap-test.zip"
    with zipfile.ZipFile(path, "w") as archive:
        archive.writestr("pack.mcmeta", json.dumps({"pack": {"description": "vexmap S5 test", "min_format": 97,
                                                             "max_format": 97, "pack_format": 97}}))
    return path, hashlib.sha1(path.read_bytes()).hexdigest()


def serve(directory):
    class Quiet(http.server.SimpleHTTPRequestHandler):
        def __init__(self, *args, **kwargs):
            super().__init__(*args, directory=str(directory), **kwargs)

        def log_message(self, *args):
            hits.append(args[1] if len(args) > 1 else "")

    hits = []
    server = http.server.ThreadingHTTPServer(("127.0.0.1", PACK_PORT), Quiet)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, hits


TRANSFERS = "expect_seconds=20,max_expect_seconds=60,max_expected=8,max_freeze_seconds=30,max_hold_seconds=60"


def start_backend(name, port, secret, pack, transfers=False, extra_env=None):
    directory = RUN / name
    directory.mkdir(parents=True, exist_ok=True)
    properties = {"server-ip": "127.0.0.1", "server-port": str(port), "online-mode": "false", "level-seed": "12345",
                  "view-distance": "10", "simulation-distance": "10", "gamemode": "creative", "force-gamemode": "true",
                  "white-list": "false", "horizon-clients-only": os.environ.get("VEXMAP_S5_CLIENTS_ONLY", "false"), "spawn-protection": "0",
                  "resource-pack": "", "resource-pack-sha1": "", "require-resource-pack": "false"}
    if pack:
        properties.update({"resource-pack": f"http\\://127.0.0.1\\:{PACK_PORT}/vexmap-test.zip",
                           "resource-pack-sha1": pack, "require-resource-pack": "true"})
    (directory / "server.properties").write_text("".join(f"{k}={v}\n" for k, v in properties.items()))
    # The owner accepted Mojang's EULA in chat on 2026-11-07, for local test servers only.
    (directory / "eula.txt").write_text("eula=true\n")
    (directory / "forwarding.secret").write_text(secret)
    env = dict(os.environ, VEXMAP_S3_SIZE="800", VEXMAP_S3_MSGS="1", HORIZON_NO_AUDIO="1",
               HORIZON_HOME=str(directory / "home"), **(extra_env or {}))
    logfile = directory / "server.log"
    extra = []
    if transfers:
        (directory / ".horizon").mkdir(exist_ok=True)
        extra = ["--control-socket", str(directory / ".horizon" / "engine.json"), "--transfers", TRANSFERS]
    process = subprocess.Popen(
        [str(BIN / "horizon-client.exe"), "--resources", str(RESOURCES), *extra, "--dedicated", "--nogui"],
        cwd=directory, env=env, stdin=subprocess.PIPE, stdout=logfile.open("w", encoding="utf-8"),
        stderr=subprocess.STDOUT, text=True)
    wait_port(port, process, f"server {name}")
    return process, logfile


def start_proxy(secret):
    directory = RUN / "proxy"
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "forwarding.secret").write_text(secret)
    (directory / "velocity.toml").write_text(f'''config-version = "2.7"
bind = "127.0.0.1:{PROXY_PORT}"
motd = "vexmap S5"
show-max-players = 500
online-mode = false
force-key-authentication = false
prevent-client-proxy-connections = false
player-info-forwarding-mode = "modern"
forwarding-secret-file = "forwarding.secret"
announce-forge = false
kick-existing-players = false
ping-passthrough = "DISABLED"
enable-player-address-logging = false

[servers]
a = "127.0.0.1:{BACKENDS['a']}"
b = "127.0.0.1:{BACKENDS['b']}"
try = ["a"]

[forced-hosts]

[advanced]
compression-threshold = 256
compression-level = -1
login-ratelimit = 0
connection-timeout = 5000
read-timeout = 30000
haproxy-protocol = false
tcp-fast-open = false
bungee-plugin-message-channel = true
show-ping-requests = false
failover-on-unexpected-server-disconnect = true
announce-proxy-commands = true
log-command-executions = true
log-player-connections = true

[query]
enabled = false
port = 25577
map = "Velocity"
show-plugins = false
''')
    # No metrics leave this machine from a test proxy (Velocity sends them to bStats by default).
    stats = directory / "plugins" / "bStats"
    stats.mkdir(parents=True, exist_ok=True)
    (stats / "config.txt").write_text("enabled=false\n")
    logfile = directory / "proxy.log"
    process = subprocess.Popen([str(JAVA), "-Xmx512M", "-jar", str(JAR)], cwd=directory, stdin=subprocess.PIPE,
                               stdout=logfile.open("w", encoding="utf-8"), stderr=subprocess.STDOUT, text=True)
    wait_port(PROXY_PORT, process, "proxy", 120)
    return process, logfile


def ctl(*args, timeout=90):
    env = dict(os.environ, HORIZON_HOME=str(RUN / "client-home"))
    return subprocess.run([str(CTL), *args], env=env, capture_output=True, text=True, encoding="utf-8",
                          errors="replace", timeout=timeout)


def count(logfile, pattern):
    return sum(1 for line in logfile.read_text(encoding="utf-8", errors="replace").splitlines() if pattern in line)


def field(text, key):
    match = re.search(rf'"{key}":"?([^",}}]*)', text)
    return match.group(1) if match else None


def answer_pack_prompt():
    """Accepts the client's resource pack prompt if one is up. Returns True if it clicked."""
    for label in ("Yes", "Proceed", "Да", "Продолжить"):
        result = ctl("click-button", label, timeout=20)
        if result.returncode == 0 and '"ok":true' in result.stdout:
            return True
    return False


def switch(target, logs, use_pack):
    before = count(logs[target], "logged in")
    started = time.monotonic()
    ctl("command", f"server {target}")
    logged = None
    deadline = started + 90
    prompts = 0
    while time.monotonic() < deadline:
        if use_pack and answer_pack_prompt():
            prompts += 1
        if count(logs[target], "logged in") > before:
            logged = time.monotonic() - started
            break
        time.sleep(0.2)
    if logged is None:
        return {"target": target, "ok": False}
    ctl("wait-ready", "60", timeout=90)
    return {"target": target, "ok": True, "login_s": round(logged, 2), "ready_s": round(time.monotonic() - started, 2),
            "prompts_clicked": prompts}


def main():
    mode = sys.argv[1]
    use_pack = mode == "pack"
    secret = secrets.token_hex(16)
    pack_server = pack_hash = hits = None
    if use_pack:
        _, pack_hash = make_pack(RUN / "pack")
        pack_server, hits = serve(RUN / "pack")
    processes = []
    logs = {}
    results = {"mode": mode}
    client_log = Path(os.environ["TEMP"]) / "horizon-control.log"
    try:
        for name, port in BACKENDS.items():
            process, logfile = start_backend(name, port, secret, pack_hash)
            processes.append(process)
            logs[name] = logfile
            log(f"server {name} up")
        proxy, proxy_log = start_proxy(secret)
        processes.append(proxy)
        log("proxy up")

        launched = ctl("launch", "--game", str(BIN / "horizon-client.exe"), "--resources", str(RESOURCES),
                       "--game-dir", str(RUN / "client-game"), "--width", "1280", "--height", "720", "--no-pause",
                       "--quickPlayMultiplayer", f"127.0.0.1:{PROXY_PORT}", timeout=240)
        log(f"client launch exit {launched.returncode}")
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline and count(logs["a"], "logged in") == 0:
            if use_pack:
                answer_pack_prompt()
            time.sleep(1)
        ctl("wait-ready", "60", timeout=90)
        results["joined_through_proxy"] = count(logs["a"], "logged in") > 0
        if not results["joined_through_proxy"]:
            raise SystemExit("the client did not get through the proxy, see the logs")
        log("waiting for mod messages through the proxy")
        time.sleep(35)
        server_lines = [l for l in logs["a"].read_text(encoding="utf-8", errors="replace").splitlines()
                        if "[vexmap_s3] server" in l and "players=1 " in l]
        results["through_proxy_server"] = server_lines[-1:]
        client_lines = [l for l in client_log.read_text(encoding="utf-8", errors="replace").splitlines()
                        if "[vexmap_s3] client" in l]
        results["through_proxy_client"] = client_lines[-1:]

        moves = []
        for index in range(SWITCHES):
            target = "b" if index % 2 == 0 else "a"
            move = switch(target, logs, use_pack)
            moves.append(move)
            log(json.dumps(move))
            time.sleep(25)  # Long enough for a 200-tick report on the new server.
            lines = [l for l in logs[target].read_text(encoding="utf-8", errors="replace").splitlines()
                     if "[vexmap_s3] server" in l and "players=1 " in l]
            move["messages_after"] = lines[-1:] and re.findall(r"rtt_samples=(\d+)", lines[-1])
        results["switches"] = moves
        text = client_log.read_text(encoding="utf-8", errors="replace").splitlines()
        results["client_mod_lines"] = sum(1 for l in text if "[vexmap_s3] client" in l)
        results["client_notable"] = [l[:170] for l in text if re.search(r"resource pack|Reload|reload|isconnect", l)][-10:]
        results["pack_requests"] = len(hits) if hits is not None else None
        results["proxy_notable"] = [l[:170] for l in proxy_log.read_text(encoding="utf-8", errors="replace").splitlines()
                                    if re.search(r"WARN|ERROR|Unable|disconnected", l)][-8:]
        ctl("screenshot", str(RUN / f"after-{mode}.png"))
        ctl("quit")
        time.sleep(4)
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
        if pack_server:
            pack_server.shutdown()
    (RUN / f"results-{mode}.json").write_text(json.dumps(results, indent=1, ensure_ascii=False))
    print(json.dumps(results, indent=1, ensure_ascii=False))


class Control:
    """An engine's control endpoint: one JSON line asked, one answered."""

    def __init__(self, name):
        info = json.loads((RUN / name / ".horizon" / "engine.json").read_text())
        self.sock = socket.create_connection(("127.0.0.1", info["port"]), timeout=30)
        self.reader = self.sock.makefile("r", encoding="utf-8")
        self.sock.sendall(f"auth {info['token']}\n".encode())
        if '"ok"' not in self.reader.readline():
            raise SystemExit(f"{name}: the control socket refused the token")

    def ask(self, line):
        self.sock.sendall((line + "\n").encode())
        return json.loads(self.reader.readline())


def offline_uuid(name):
    digest = bytearray(hashlib.md5(("OfflinePlayer:" + name).encode()).digest())
    digest[6] = (digest[6] & 0x0F) | 0x30
    digest[8] = (digest[8] & 0x3F) | 0x80
    return str(uuid.UUID(bytes=bytes(digest)))


def console(process, line):
    process.stdin.write(line + "\n")
    process.stdin.flush()


def diamonds():
    """Diamonds the client's own game says the player has: what the player would see."""
    # The reply lists each stack by slot and again in `counts`: read `counts` alone.
    return json.loads(ctl("inventory", timeout=30).stdout).get("counts", {}).get("minecraft:diamond", 0)


def client_position():
    return json.loads(ctl("state", timeout=30).stdout)["player"]["position"]


def hand_over(source_ctl, target, target_ctl, proxy, name, player):
    started = time.monotonic()
    steps = {}

    def mark(step):
        steps[step] = round(time.monotonic() - started, 3)

    reply = target_ctl.ask(f"player-expect {player}")
    if not reply.get("ok"):
        return {"ok": False, "failed": "expect", "reply": reply}
    mark("expect")
    exported = source_ctl.ask(f"player-export {player} freeze")
    if not exported.get("ok"):
        target_ctl.ask(f"player-drop {player}")
        return {"ok": False, "failed": "export", "reply": str(exported)[:300]}
    record = exported["record"]
    handover = record["handover"]
    mark("frozen")
    console(proxy, f"send {name} {target}")
    deadline = time.monotonic() + 30
    held = False
    while time.monotonic() < deadline:
        if target_ctl.ask("transfer-status").get("held", 0) >= 1:
            held = True
            break
        time.sleep(0.01)
    mark("held")
    if not held:
        source_ctl.ask(f"player-release {player} {handover} abort")
        return {"ok": False, "failed": "held", "steps": steps}
    imported = target_ctl.ask(f"player-import {player} " + json.dumps({"record": record, "announce": False}))
    mark("imported")
    if not imported.get("ok"):
        return {"ok": False, "failed": "import", "reply": str(imported)[:400], "steps": steps}
    released = source_ctl.ask(f"player-release {player} {handover} moved")
    mark("released")
    ctl("wait-ready", "60", timeout=90)
    mark("ready")
    return {"ok": True, "steps": steps, "recentered": imported.get("recentered"), "placed": imported.get("placed"),
            "release": released.get("released") or str(released)[:200]}


def transfer():
    secret = secrets.token_hex(16)
    processes = {}
    logs = {}
    results = {"mode": "transfer", "moves": []}
    try:
        for name, port in BACKENDS.items():
            processes[name], logs[name] = start_backend(name, port, secret, None, transfers=True)
            log(f"server {name} up")
        controls = {name: Control(name) for name in BACKENDS}
        processes["proxy"], proxy_log = start_proxy(secret)
        log("proxy up")
        launched = ctl("launch", "--game", str(BIN / "horizon-client.exe"), "--resources", str(RESOURCES),
                       "--game-dir", str(RUN / "client-game"), "--width", "1280", "--height", "720", "--no-pause",
                       "--quickPlayMultiplayer", f"127.0.0.1:{PROXY_PORT}", timeout=240)
        log(f"client launch exit {launched.returncode}")
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline and count(logs["a"], "logged in") == 0:
            time.sleep(0.5)
        ctl("wait-ready", "60", timeout=90)
        found = re.findall(r"(Player\d+)", proxy_log.read_text(encoding="utf-8", errors="replace"))
        if not found:
            raise SystemExit("no player in the proxy log")
        name = found[-1]
        player = offline_uuid(name)
        log(f"player {name} {player}")
        console(processes["a"], f"clear {name}")
        time.sleep(1)

        expected = 0
        where = "a"
        for index in range(8):
            target = "b" if where == "a" else "a"
            # Items and place change on the source each time, so a stale copy cannot pass by chance.
            expected += 3
            console(processes[where], f"give {name} minecraft:diamond 3")
            spot = [100 + index * 40, 120 + index, -50 - index * 30]
            console(processes[where], f"tp {name} {spot[0]} {spot[1]} {spot[2]}")
            time.sleep(4)
            before_items, before_place = diamonds(), client_position()
            move = hand_over(controls[where], target, controls[target], processes["proxy"], name, player)
            time.sleep(1.5)
            if move["ok"]:
                move["items"] = {"expected": expected, "before": before_items, "after": diamonds()}
                after_place = client_position()
                move["place_error_blocks"] = round(max(abs(x - y) for x, y in zip(before_place, after_place)), 2)
                move["to"] = target
                where = target
            results["moves"].append(move)
            log(json.dumps(move))
            if not move["ok"]:
                break
            time.sleep(4)

        # A hand-over called off before the client moved: the player must simply play on.
        target = "b" if where == "a" else "a"
        controls[target].ask(f"player-expect {player}")
        exported = controls[where].ask(f"player-export {player} freeze")
        if exported.get("ok"):
            handover = exported["record"]["handover"]
            controls[target].ask(f"player-drop {player}")
            aborted = controls[where].ask(f"player-release {player} {handover} abort")
            results["abort"] = {"reply": aborted, "status": controls[where].ask("transfer-status"),
                                "items": diamonds(), "expected_items": expected}
            log(json.dumps(results["abort"]))
            results["late_moved"] = controls[where].ask(f"player-release {player} {handover} moved")
            log(json.dumps(results["late_moved"]))
        ctl("screenshot", str(RUN / "after-transfer.png"))
        ctl("quit")
        time.sleep(4)
        for key, logfile in logs.items():
            lines = logfile.read_text(encoding="utf-8", errors="replace").splitlines()
            results[f"log_{key}"] = [line[:170] for line in lines
                                     if re.search(r"is held|placed:|moved to another|left the game|WARN.*(hand|held|freeze)", line)][-8:]
    finally:
        for key, process in processes.items():
            try:
                console(process, "shutdown" if key == "proxy" else "stop")
            except OSError:
                pass
        for process in processes.values():
            try:
                process.wait(timeout=90)
            except subprocess.TimeoutExpired:
                process.kill()
    (RUN / "results-transfer.json").write_text(json.dumps(results, indent=1, ensure_ascii=False))
    print(json.dumps({k: v for k, v in results.items() if k != "moves"}, indent=1, ensure_ascii=False))


if __name__ == "__main__":
    if sys.argv[1] == "transfer":
        transfer()
    else:
        main()
