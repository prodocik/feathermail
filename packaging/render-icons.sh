#!/usr/bin/env bash
# T-071/T-072: rescale the canonical icon.png into the hicolor sizes.
#
# `icon.png` is 1254x1254 — a size no icon theme accepts, and linuxdeploy
# rejects it outright. This is a rescale of the canonical mark, not a
# replacement for it (D1 / DESIGN.md): same artwork, exact theme sizes.
# The outputs are committed, so packaging never depends on Pillow.
set -euo pipefail
cd "$(dirname "$0")/.."

command -v python3 >/dev/null || { echo "python3 with Pillow is required" >&2; exit 2; }

python3 - <<'PY'
from PIL import Image

src = Image.open("icon.png").convert("RGBA")
if src.size != (1254, 1254):
    raise SystemExit(f"icon.png is {src.size}, expected the canonical 1254x1254")

import os

for size in (512, 256, 128, 64, 48, 32, 24, 16):
    out = f"packaging/icons/hicolor/{size}x{size}/apps/app.feathermail.FeatherMail.png"
    os.makedirs(os.path.dirname(out), exist_ok=True)
    src.resize((size, size), Image.LANCZOS).save(out, optimize=True)
    print(out)
PY
