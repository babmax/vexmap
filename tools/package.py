#!/usr/bin/env python3
"""Package mods/vexmap as a source-only `.hnmod`, the file Horizon's launcher takes.

Adapted from horizon-native-mod-template's tools/package.py: the bundle has a `hooks/` folder, so files are taken
recursively. Fixed dates and modes, so the same sources give the same bytes. Output: target/<id>-<version>.hnmod
"""
import hashlib
import json
from pathlib import Path
import tomllib
import zipfile

MOD = Path(__file__).resolve().parents[1] / 'mods' / 'vexmap'
OUT = Path(__file__).resolve().parents[1] / 'target'
GAME = '26.3'


def package():
    compat = json.loads((MOD / 'compatibility.json').read_text())
    bundle = MOD / 'mixins' / 'vexmap'
    mod = tomllib.loads((bundle / 'horizon-mixin.toml').read_text())
    meta = mod['mod']
    if mod.get('compatibility', {}).get('horizon_revision') != compat['horizon_revision']:
        raise SystemExit('horizon-mixin.toml and compatibility.json pin different Horizon revisions')
    manifest = dict(schema=1, id=meta['id'], version=meta['version'], game=GAME,
                    horizon=compat['horizon_revision'], mixins=compat['horizon_mixins_revision'])
    files = {'horizon-native.json': (json.dumps(manifest, indent=2, sort_keys=True) + '\n').encode()}
    for path in sorted(bundle.rglob('*')):
        if path.is_file() and path.suffix in ('.rs', '.toml'):
            files[f"mixins/{meta['id']}/{path.relative_to(bundle).as_posix()}"] = path.read_bytes()
    OUT.mkdir(exist_ok=True)
    out = OUT / f"{meta['id']}-{meta['version']}.hnmod"
    with zipfile.ZipFile(out, 'w', compression=zipfile.ZIP_DEFLATED) as archive:
        for name, content in sorted(files.items()):
            info = zipfile.ZipInfo(name, (2026, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o100644 << 16
            archive.writestr(info, content.replace(b'\r\n', b'\n'))
    print(json.dumps(dict(file=str(out), files=len(files), size=out.stat().st_size,
                          sha256=hashlib.sha256(out.read_bytes()).hexdigest()), indent=2))


if __name__ == '__main__':
    package()
