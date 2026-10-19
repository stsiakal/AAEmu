"""Assemble the installable addon folder and deterministic ZIP.

The catalog builder writes ``aaemu_quest_guide/data``. This module adds the
reviewed Lua runtime, README and an observations placeholder, then builds the
ZIP. It never writes to the source tree or to builder inputs.
"""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import shutil
import tempfile
import zipfile

VERSION = "0.1.0"
ADDON_ID = "aaemu_quest_guide"
ZIP_NAME = ADDON_ID + "-" + VERSION + ".zip"
MARKER_NAME = ".aaemu_quest_guide_generated"
FIXED_TIME = (1980, 1, 1, 0, 0, 0)
RUNTIME_FILES = (
    "main.lua", "core.lua", "ui.lua", "nav.lua", "recorder.lua",
    "pages.lua", "catalog.lua", "util.lua", "README.md",
)


class PackageError(RuntimeError):
    pass


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def is_relative_to(path: Path, parent: Path) -> bool:
    try:
        path.relative_to(parent)
        return True
    except ValueError:
        return False


def atomic_write(path: Path, data: bytes):
    path.parent.mkdir(parents=True, exist_ok=True)
    handle = tempfile.NamedTemporaryFile(
        "wb", dir=path.parent, prefix="." + path.name + ".", delete=False)
    try:
        with handle:
            handle.write(data)
        os.replace(handle.name, path)
    finally:
        if os.path.exists(handle.name):
            os.unlink(handle.name)


def check_folder_safe(output: Path, folder: Path, data_dir: Path):
    output_resolved = output.resolve()
    if not is_relative_to(folder.absolute(), output_resolved):
        raise PackageError(f"package folder escapes output: {folder}")
    resolved = folder.resolve(strict=False)
    if resolved == output_resolved or not is_relative_to(resolved, output_resolved):
        raise PackageError(f"package folder resolves outside output: {folder} -> {resolved}")
    if folder.exists() and (folder.is_symlink()
                            or getattr(folder, "is_junction", lambda: False)()):
        raise PackageError(f"refusing to replace linked package folder: {folder}")
    if data_dir.exists() and not (data_dir / MARKER_NAME).is_file():
        raise PackageError(
            f"refusing to package foreign folder: {data_dir} has no generated marker")
    if folder.exists() and folder.is_dir():
        entries = list(folder.iterdir())
        if entries and not (data_dir / MARKER_NAME).is_file():
            raise PackageError(
                f"refusing to replace foreign package folder: {folder}")


def read_manifest(output: Path) -> dict:
    path = output / "manifest.json"
    try:
        manifest = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise PackageError(f"cannot read generated manifest {path}: {error}") from error
    if not isinstance(manifest, dict) or manifest.get("schema") != "aaemu_quest_guide.catalog":
        raise PackageError(f"manifest is not a generated quest guide catalog: {path}")
    return manifest


def build_zip(folder: Path, zip_path: Path, exclude_names: set[str]):
    files = []
    for path in folder.rglob("*"):
        if not path.is_file():
            continue
        relative = path.relative_to(folder).as_posix()
        if relative in exclude_names:
            continue
        if path.name == MARKER_NAME:
            continue
        if ".." in Path(relative).parts:
            raise PackageError(f"unsafe ZIP path: {relative}")
        files.append((relative, path))
    files.sort(key=lambda item: item[0])
    temporary = zip_path.with_name("." + zip_path.name + ".tmp")
    if temporary.exists():
        temporary.unlink()
    try:
        with zipfile.ZipFile(temporary, "w") as archive:
            for relative, path in files:
                info = zipfile.ZipInfo(ADDON_ID + "/" + relative, FIXED_TIME)
                info.compress_type = zipfile.ZIP_DEFLATED
                info.create_system = 0
                info.external_attr = 0o100644 << 16
                archive.writestr(info, path.read_bytes(), compresslevel=9)
        os.replace(temporary, zip_path)
    finally:
        if temporary.exists():
            temporary.unlink()
    return files


def package_addon(output: Path, source_dir: Path, version: str = VERSION):
    output = output.resolve()
    source_dir = source_dir.resolve()
    runtime_src = source_dir / "addon" / ADDON_ID
    data_dir = output / ADDON_ID / "data"
    folder = output / ADDON_ID
    if not runtime_src.is_dir():
        raise PackageError(f"addon runtime source not found: {runtime_src}")
    for name in RUNTIME_FILES:
        if not (runtime_src / name).is_file():
            raise PackageError(f"missing runtime source file: {runtime_src / name}")
    placeholder = runtime_src / "observations" / "README.txt"
    if not placeholder.is_file():
        raise PackageError(f"missing observations placeholder: {placeholder}")
    if not data_dir.is_dir():
        raise PackageError(f"generated catalog data not found: {data_dir}")
    check_folder_safe(output, folder, data_dir)

    temp_root = Path(tempfile.mkdtemp(prefix=".aaemu_package.", dir=output))
    stage = temp_root / ADDON_ID
    try:
        shutil.copytree(data_dir, stage / "data")
        for name in RUNTIME_FILES:
            shutil.copyfile(runtime_src / name, stage / name)
        (stage / "observations").mkdir(parents=True)
        shutil.copyfile(placeholder, stage / "observations" / "README.txt")

        staging_folder = stage
        if folder.exists():
            shutil.rmtree(folder)
        folder.parent.mkdir(parents=True, exist_ok=True)
        os.replace(staging_folder, folder)

        zip_name = ADDON_ID + "-" + version + ".zip"
        zip_path = output / zip_name
        files = build_zip(folder, zip_path, set())
        package_files = [{
            "path": ADDON_ID + "/" + relative,
            "sha256": sha256_file(path),
            "bytes": path.stat().st_size,
        } for relative, path in files if not relative.startswith("data/")]

        manifest = read_manifest(output)
        manifest["package"] = {
            "folder": ADDON_ID,
            "zip": zip_name,
            "version": version,
            "readme": ADDON_ID + "/README.md",
            "observations_placeholder": ADDON_ID + "/observations/README.txt",
            "files": package_files,
            "zip_sha256": sha256_file(zip_path),
            "zip_bytes": zip_path.stat().st_size,
        }
        atomic_write(output / "manifest.json",
                     (json.dumps(manifest, ensure_ascii=False, indent=2,
                                 sort_keys=True) + "\n").encode("utf-8"))

        summary_path = output / "summary.json"
        try:
            summary = json.loads(summary_path.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError):
            summary = {}
        summary["package"] = manifest["package"]
        atomic_write(summary_path,
                     (json.dumps(summary, ensure_ascii=False, indent=2,
                                 sort_keys=True) + "\n").encode("utf-8"))
        return manifest, summary, zip_path
    finally:
        shutil.rmtree(temp_root, ignore_errors=True)
