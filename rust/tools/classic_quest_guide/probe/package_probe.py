"""Build the deterministic aaemu_probe.zip from an explicit file allowlist.

    python package_probe.py --output <dir>

The ZIP holds a single top-level ``aaemu_probe/`` folder, ready to unpack into
``Documents/AAClassic/Addon/``. It never contains observation files.
"""

from __future__ import annotations

import argparse
import hashlib
import os
from pathlib import Path
import zipfile

HERE = Path(__file__).resolve().parent
ADDON_ID = "aaemu_probe"
ZIP_NAME = ADDON_ID + ".zip"
FIXED_TIME = (1980, 1, 1, 0, 0, 0)
RUNTIME_FILES = (
    "main.lua", "core.lua", "ui.lua", "util.lua", "getters.lua", "events.lua",
    "README.md",
)


class PackageError(RuntimeError):
    pass


def entries(source: Path = HERE / ADDON_ID):
    result = []
    for name in RUNTIME_FILES:
        path = source / name
        if not path.is_file() or path.is_symlink():
            raise PackageError(f"missing or linked runtime file: {path}")
        data = path.read_bytes().replace(b"\r\n", b"\n")
        result.append((f"{ADDON_ID}/{name}", data))
    return result


def build_zip(zip_path: Path, source: Path = HERE / ADDON_ID) -> str:
    zip_path.parent.mkdir(parents=True, exist_ok=True)
    temporary = zip_path.with_name("." + zip_path.name + ".tmp")
    try:
        with zipfile.ZipFile(temporary, "w") as archive:
            archive.writestr(zipfile.ZipInfo(f"{ADDON_ID}/observations/", FIXED_TIME), b"")
            for arcname, data in entries(source):
                info = zipfile.ZipInfo(arcname, FIXED_TIME)
                info.compress_type = zipfile.ZIP_DEFLATED
                info.external_attr = 0o644 << 16
                archive.writestr(info, data)
        os.replace(temporary, zip_path)
    finally:
        if temporary.exists():
            temporary.unlink()
    return hashlib.sha256(zip_path.read_bytes()).hexdigest()


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True,
                        help="directory that receives aaemu_probe.zip")
    args = parser.parse_args(argv)
    zip_path = args.output / ZIP_NAME
    digest = build_zip(zip_path)
    print(f"{zip_path} sha256={digest}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
