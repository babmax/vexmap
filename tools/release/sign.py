#!/usr/bin/env python3
"""Package a VexMap release and sign it with minisign.

    python3 tools/release/sign.py --version v0.1.0 --server .deps/target/release/horizon-client --out dist

Writes to --out:
  vexmap-mod-<version>.tar.gz                  the mod bundle (mods/vexmap) the launcher builds from
  vexmap-server-linux-x86_64-<version>.tar.gz  the region server engine with the mod, if --server is given
  vexmap-pack.zip, vexmap-pack.sha1            the server resource pack, if --pack is given (host it, see docs/phase-4)
  manifest.json                                version, Horizon pins, file sizes and SHA-256
  SHA256SUMS, SHA256SUMS.minisig               sums, and their signature

The key comes from the environment, never from the repository: MINISIGN_SECRET_KEY is the base64 of the secret key file
from `minisign -G`, MINISIGN_PASSWORD its password (empty for a key made with `-W`). Without a key the files are written
unsigned and the script exits 2, so a release job fails loudly. Verify with
`minisign -Vm SHA256SUMS -p vexmap.pub`.
"""
import argparse
import base64
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parents[2]
MOD = ROOT / "mods" / "vexmap"


def reproducible(info):
    info.uid = info.gid = 0
    info.uname = info.gname = ""
    info.mtime = 0
    info.mode = 0o755 if info.isdir() or info.mode & 0o111 else 0o644
    return info


def tar_gz(out, entries):
    """`entries`: (path in archive, file on disk). Sorted, fixed owners and times: the same input gives the same bytes."""
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w", format=tarfile.PAX_FORMAT) as archive:
        for name, path in sorted(entries):
            archive.add(path, arcname=name, recursive=False, filter=reproducible)
    out.write_bytes(gzip.compress(buffer.getvalue(), mtime=0))


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--version", required=True)
    parser.add_argument("--server", type=Path, help="built horizon-client with the mod, for the server archive")
    parser.add_argument("--pack", type=Path, help="directory with vexmap-pack.zip and vexmap-pack.sha1")
    parser.add_argument("--out", type=Path, default=ROOT / "dist")
    args = parser.parse_args()
    version = args.version.removeprefix("v")
    out = args.out
    out.mkdir(parents=True, exist_ok=True)

    files = []
    mod_archive = out / f"vexmap-mod-{version}.tar.gz"
    tar_gz(mod_archive, [(f"vexmap/{p.relative_to(MOD).as_posix()}", p) for p in MOD.rglob("*") if p.is_file()])
    files.append(mod_archive)

    if args.server:
        server_archive = out / f"vexmap-server-linux-x86_64-{version}.tar.gz"
        tar_gz(server_archive, [("horizon-engine", args.server), ("LICENSE", ROOT / "LICENSE")])
        files.append(server_archive)

    if args.pack:
        for name in ("vexmap-pack.zip", "vexmap-pack.sha1"):
            copied = out / name
            copied.write_bytes((args.pack / name).read_bytes())
            files.append(copied)

    compat = json.loads((MOD / "compatibility.json").read_text(encoding="utf-8"))
    manifest = out / "manifest.json"
    manifest.write_text(json.dumps({
        "version": version,
        "horizon_revision": compat["horizon_revision"],
        "horizon_mixins_revision": compat["horizon_mixins_revision"],
        "hooks": compat["hooks"],
        "files": {f.name: {"size": f.stat().st_size, "sha256": sha256(f)} for f in files},
    }, indent=2) + "\n", encoding="utf-8")
    files.append(manifest)

    sums = out / "SHA256SUMS"
    sums.write_text("".join(f"{sha256(f)}  {f.name}\n" for f in files), encoding="utf-8")
    print(sums.read_text(encoding="utf-8"), end="")

    key = os.environ.get("MINISIGN_SECRET_KEY")
    if not key:
        print("MINISIGN_SECRET_KEY is not set: SHA256SUMS is NOT signed", file=sys.stderr)
        return 2
    with tempfile.TemporaryDirectory() as temp:
        key_file = Path(temp) / "vexmap.key"
        key_file.write_bytes(base64.b64decode(key))
        password = os.environ.get("MINISIGN_PASSWORD", "")
        command = ["minisign", "-S", "-s", str(key_file), "-m", str(sums), "-t", f"vexmap {version}"]
        # minisign 0.12 reads the password from stdin when it is not a terminal (checked with a throwaway key).
        result = subprocess.run(command, input=(password + "\n").encode(), capture_output=True)
        if result.returncode:
            sys.stderr.write(result.stderr.decode(errors="replace"))
            return result.returncode
    print(f"signed: {sums}.minisig")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
