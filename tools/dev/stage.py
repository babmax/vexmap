#!/usr/bin/env python3
"""Apply a native mod bundle to a pinned Horizon commit and run cargo on the result. Works on Windows and Linux.

Does what the template's tools/horizon.py and horizon-mods' native.py do, without their Linux-only parts (fcntl, rsync).
All AST changes go through horizon-mixins. Outputs stay under .deps/, which git ignores.

    python tools/dev/stage.py check spikes/s0_pipeline
"""
import argparse
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[2]
DEPS = ROOT / ".deps"
MIXINS = DEPS / "horizon-mixins"
USER_AGENT = "vexmap-dev"


def log(message):
    print(f"[stage {time.strftime('%H:%M:%S')}] {message}", flush=True)


def fetch_source(revision):
    """Horizon's source at `revision`, from GitHub's archive of the commit, unpacked once."""
    out = DEPS / "stage" / "horizon" / revision
    if (out / ".horizon-revision").is_file():
        return out
    log(f"downloading Horizon {revision[:7]}")
    request = urllib.request.Request(
        f"https://codeload.github.com/horizon-rust/horizon/tar.gz/{revision}", headers={"User-Agent": USER_AGENT})
    with urllib.request.urlopen(request, timeout=280) as response:
        data = response.read()
    partial = DEPS / "stage" / "horizon" / f".{revision}.partial"
    shutil.rmtree(partial, ignore_errors=True)
    partial.mkdir(parents=True)
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as archive:
        # horizon-mixins refuses links and none is needed to build; Windows may not allow creating them either.
        members = [m for m in archive.getmembers() if not (m.issym() or m.islnk())]
        archive.extractall(partial, members=members, filter="data")
    top = next(partial.iterdir())
    (top / ".horizon-revision").write_text(revision + "\n")
    shutil.rmtree(out, ignore_errors=True)
    top.rename(out)
    shutil.rmtree(partial)
    return out


def build_mixins():
    exe = MIXINS / "target" / "release" / ("horizon-mixins" + (".exe" if os.name == "nt" else ""))
    log("building horizon-mixins")
    subprocess.run(["cargo", "build", "--release", "--locked", "-p", "horizon-mixins-cli"], cwd=MIXINS, check=True)
    return exe


def apply_bundle(tool, source, mod_dir):
    """The bundle applied to `source`. Unchanged files keep their mtimes, so cargo rebuilds only what changed."""
    out = DEPS / "stage" / "modded" / mod_dir.name
    fresh = DEPS / "stage" / "fresh" / mod_dir.name
    shutil.rmtree(fresh, ignore_errors=True)
    fresh.parent.mkdir(parents=True, exist_ok=True)
    # horizon-mixins replaces an existing stage on Linux only (E_ATOMIC_STAGE), so it always gets a fresh folder here.
    result = subprocess.run(
        [str(tool), "apply", "--workspace", str(source), "--mods", str(mod_dir / "mixins"), "--out", str(fresh)],
        capture_output=True, text=True, encoding="utf-8", errors="replace")
    if result.returncode:
        sys.stderr.write(result.stderr)
        raise SystemExit(f"horizon-mixins apply failed with exit code {result.returncode}")
    changed = sync_changed(fresh, out)
    shutil.rmtree(fresh, ignore_errors=True)
    log(f"applied, {changed} files changed in the stage")
    return out


def sync_changed(fresh, out):
    """Makes `out` match `fresh`, touching only the files that differ."""
    changed = 0
    wanted = set()
    for path in fresh.rglob("*"):
        if not path.is_file():
            continue
        relative = path.relative_to(fresh)
        wanted.add(relative)
        target = out / relative
        if not target.is_file() or target.stat().st_size != path.stat().st_size \
                or target.read_bytes() != path.read_bytes():
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(path, target)
            changed += 1
    for path in out.rglob("*"):
        if path.is_file() and path.relative_to(out) not in wanted:
            path.unlink()
            changed += 1
    return changed


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["stage", "check", "build"])
    parser.add_argument("mod_dir", type=Path, help="folder with compatibility.json and mixins/<id>/")
    parser.add_argument("--audio", action="store_true", help="keep Horizon's default features (audio, controllers)")
    parser.add_argument("--profile", default="fast", help="cargo profile for build (default: fast)")
    args = parser.parse_args()
    mod_dir = args.mod_dir.resolve()
    revision = json.loads((mod_dir / "compatibility.json").read_text(encoding="utf-8"))["horizon_revision"]

    started = time.monotonic()
    source = fetch_source(revision)
    log(f"source ready after {time.monotonic() - started:.0f} s")
    tool = build_mixins()
    log(f"tool ready after {time.monotonic() - started:.0f} s")
    stage = apply_bundle(tool, source, mod_dir)
    log(f"stage ready after {time.monotonic() - started:.0f} s: {stage}")
    if args.action == "stage":
        return 0

    command = ["cargo", args.action, "--locked", "-p", "horizon-client", "--bin", "horizon-client",
               "--target-dir", str(DEPS / "target")]
    if not args.audio:
        command.append("--no-default-features")
    if args.action == "build":
        command += ["--profile", args.profile]
    log("running: " + " ".join(command))
    cargo_started = time.monotonic()
    code = subprocess.run(command, cwd=stage).returncode
    log(f"cargo {args.action} exit {code} after {time.monotonic() - cargo_started:.0f} s")
    return code


if __name__ == "__main__":
    raise SystemExit(main())
