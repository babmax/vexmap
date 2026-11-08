#!/usr/bin/env python3
"""Write the WGSL inside a Rust file's `r#"..."#` string to a file, so naga can validate it without building the game.

    python tools/dev/extract_wgsl.py spikes/s1_globe/mixins/vexmap_s1/globe.rs out.wgsl
"""
from pathlib import Path
import sys

text = Path(sys.argv[1]).read_text(encoding="utf-8")
start = text.index('r#"') + 3
Path(sys.argv[2]).write_text(text[start:text.index('"#', start)], encoding="utf-8")
