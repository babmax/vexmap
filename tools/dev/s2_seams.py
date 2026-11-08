#!/usr/bin/env python3
"""Checks that panorama faces join: the touching edges of neighbouring cube faces should nearly match.

Reads the PNGs the S2 spike wrote, with no image library. Compares each seam with the same faces' non-touching edges as a
control. Small seam error against a large control error means the camera really stood still and turned by 90 degrees.

    python -I tools/dev/s2_seams.py .deps/run/s2/faces
"""
from pathlib import Path
import struct
import sys
import zlib


def read_png(path):
    data = path.read_bytes()
    at, idat, width, height = 8, b"", 0, 0
    while at < len(data):
        length, kind = struct.unpack(">I4s", data[at:at + 8])
        chunk = data[at + 8:at + 8 + length]
        if kind == b"IHDR":
            width, height = struct.unpack(">II", chunk[:8])
        elif kind == b"IDAT":
            idat += chunk
        at += 12 + length
    raw = zlib.decompress(idat)
    stride = width * 4
    rows, previous, at = [], bytearray(stride), 0
    for _ in range(height):
        kind = raw[at]
        row = bytearray(raw[at + 1:at + 1 + stride])
        at += 1 + stride
        for i in range(stride):
            left = row[i - 4] if i >= 4 else 0
            up = previous[i]
            corner = previous[i - 4] if i >= 4 else 0
            if kind == 1:
                row[i] = (row[i] + left) & 255
            elif kind == 2:
                row[i] = (row[i] + up) & 255
            elif kind == 3:
                row[i] = (row[i] + (left + up) // 2) & 255
            elif kind == 4:
                p = left + up - corner
                pa, pb, pc = abs(p - left), abs(p - up), abs(p - corner)
                predictor = left if pa <= pb and pa <= pc else up if pb <= pc else corner
                row[i] = (row[i] + predictor) & 255
        rows.append(row)
        previous = row
    return width, height, rows


def column(image, x):
    return [image[2][y][x * 4:x * 4 + 3] for y in range(image[1])]


def difference(a, b):
    return sum(abs(p - q) for pa, pb in zip(a, b) for p, q in zip(pa, pb)) / (3 * len(a))


def main():
    folder = Path(sys.argv[1])
    numbers = sorted({p.name[:3] for p in folder.glob("*-south.png")})
    # Turning right from south: west, north, east, then back to south.
    ring = ["south", "west", "north", "east"]
    for number in (numbers[0], numbers[len(numbers) // 2], numbers[-1]):
        faces = {name: read_png(folder / f"{number}-{name}.png") for name in ring}
        seams, controls = [], []
        for index, name in enumerate(ring):
            near = faces[name]
            far = faces[ring[(index + 1) % 4]]
            seams.append(difference(column(near, near[0] - 1), column(far, 0)))
            controls.append(difference(column(near, near[0] // 2), column(far, 0)))
        print(f"{number}: seam error " + " ".join(f"{s:5.1f}" for s in seams)
              + f" | control " + " ".join(f"{c:5.1f}" for c in controls) + "  (0 to 255)")


if __name__ == "__main__":
    main()
