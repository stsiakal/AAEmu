"""Assemble the installable addon folder and deterministic ZIP.

The catalog builder writes ``aaemu_quest_guide/data``. Packaging updates only
known runtime files and the README; it never deletes or replaces the addon
folder, so observations and user files survive rebuilds. The distributable ZIP
is built from an explicit allowlist and never includes preserved user data.
"""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import tempfile
import zipfile

VERSION = "0.2.0"
ADDON_ID = "aaemu_quest_guide"
ZIP_NAME = ADDON_ID + "-" + VERSION + ".zip"
MARKER_NAME = ".aaemu_quest_guide_generated"
FIXED_TIME = (1980, 1, 1, 0, 0, 0)
VERSION_RE = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+$")
RUNTIME_FILES = (
    "main.lua", "core.lua", "ui.lua", "nav.lua", "recorder.lua",
    "pages.lua", "catalog.lua", "checklist.lua", "progress.lua",
    "util.lua", "README.md",
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


def overlaps(a: Path, b: Path) -> bool:
    return a == b or is_relative_to(a, b) or is_relative_to(b, a)


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


def is_link(path: Path) -> bool:
    return path.is_symlink() or getattr(path, "is_junction", lambda: False)()


def validate_version(version: str):
    if not isinstance(version, str) or not VERSION_RE.match(version):
        raise PackageError(f"invalid addon version: {version!r}")
    return version


def safe_existing_file(folder: Path, path: Path, label: str) -> Path:
    if not path.is_file():
        raise PackageError(f"missing {label}: {path}")
    if is_link(path):
        raise PackageError(f"refusing linked {label}: {path}")
    resolved = path.resolve()
    if not is_relative_to(resolved, folder.resolve()):
        raise PackageError(f"{label} escapes package folder: {path} -> {resolved}")
    return path


def check_folder_safe(output: Path, folder: Path, data_dir: Path):
    output_resolved = output.resolve()
    if not is_relative_to(folder.absolute(), output_resolved):
        raise PackageError(f"package folder escapes output: {folder}")
    resolved = folder.resolve(strict=False)
    if resolved == output_resolved or not is_relative_to(resolved, output_resolved):
        raise PackageError(f"package folder resolves outside output: {folder} -> {resolved}")
    if folder.exists() and is_link(folder):
        raise PackageError(f"refusing linked package folder: {folder}")
    if not data_dir.is_dir():
        raise PackageError(f"generated catalog data not found: {data_dir}")
    if is_link(data_dir):
        raise PackageError(f"refusing linked catalog data directory: {data_dir}")
    if not (data_dir / MARKER_NAME).is_file():
        raise PackageError(
            f"refusing to package foreign folder: {data_dir} has no generated marker")
    if not is_relative_to(data_dir.resolve(), folder.resolve()):
        raise PackageError(f"catalog data escapes package folder: {data_dir}")


def read_manifest(output: Path) -> dict:
    path = output / "manifest.json"
    try:
        manifest = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise PackageError(f"cannot read generated manifest {path}: {error}") from error
    if not isinstance(manifest, dict) or manifest.get("schema") != "aaemu_quest_guide.catalog":
        raise PackageError(f"manifest is not a generated quest guide catalog: {path}")
    return manifest


def copy_known_runtime(folder: Path, runtime_src: Path, placeholder: Path):
    for name in RUNTIME_FILES:
        source = runtime_src / name
        if not source.is_file() or is_link(source):
            raise PackageError(f"missing or linked runtime source file: {source}")
        target = folder / name
        if target.exists() and is_link(target):
            raise PackageError(f"refusing to overwrite linked runtime file: {target}")
        temporary = target.with_name("." + target.name + ".tmp")
        shutil.copyfile(source, temporary)
        os.replace(temporary, target)
    observations = folder / "observations"
    if observations.exists() and not observations.is_dir():
        raise PackageError(f"observations path is not a directory: {observations}")
    if observations.exists() and is_link(observations):
        raise PackageError(f"refusing linked observations directory: {observations}")
    observations.mkdir(exist_ok=True)
    placeholder_target = observations / "README.txt"
    if not placeholder_target.exists():
        if is_link(placeholder_target):
            raise PackageError(f"refusing linked observations placeholder: {placeholder_target}")
        shutil.copyfile(placeholder, placeholder_target)


def package_files(folder: Path, manifest: dict, runtime_src: Path,
                  placeholder: Path):
    """Explicit allowlist: generated data + known runtime + source placeholder."""
    entries = []
    data_prefix = ADDON_ID + "/data/"
    data_entries = [
        entry for entry in (manifest.get("files") or [])
        if isinstance(entry, dict)
        and isinstance(entry.get("path"), str)
        and entry["path"].startswith(data_prefix)
        and entry["path"].endswith(".lua")
    ]
    if not data_entries:
        raise PackageError("manifest has no generated data files to package")
    for entry in sorted(data_entries, key=lambda item: item["path"]):
        relative = entry["path"][len(ADDON_ID) + 1:]
        path = safe_existing_file(folder, folder / relative, "catalog file")
        entries.append((ADDON_ID + "/" + relative, path))
    for name in RUNTIME_FILES:
        path = safe_existing_file(folder, folder / name, "runtime file")
        entries.append((ADDON_ID + "/" + name, path))
    if not placeholder.is_file() or is_link(placeholder):
        raise PackageError(f"missing or linked observations placeholder: {placeholder}")
    entries.append((ADDON_ID + "/observations/README.txt", placeholder))
    return entries


def build_zip(zip_path: Path, entries):
    temporary = zip_path.with_name("." + zip_path.name + ".tmp")
    if temporary.exists():
        temporary.unlink()
    try:
        with zipfile.ZipFile(temporary, "w") as archive:
            for arcname, path in sorted(entries, key=lambda item: item[0]):
                info = zipfile.ZipInfo(arcname, FIXED_TIME)
                info.compress_type = zipfile.ZIP_DEFLATED
                info.create_system = 0
                info.external_attr = 0o100644 << 16
                archive.writestr(info, path.read_bytes(), compresslevel=9)
        os.replace(temporary, zip_path)
    finally:
        if temporary.exists():
            temporary.unlink()


def package_addon(output: Path, source_dir: Path, version: str = VERSION):
    output = output.resolve()
    source_dir = source_dir.resolve()
    validate_version(version)
    if overlaps(output, source_dir):
        raise PackageError(
            f"output {output} overlaps runtime source {source_dir}")
    runtime_src = source_dir / "addon" / ADDON_ID
    data_dir = output / ADDON_ID / "data"
    folder = output / ADDON_ID
    if not runtime_src.is_dir():
        raise PackageError(f"addon runtime source not found: {runtime_src}")
    placeholder = runtime_src / "observations" / "README.txt"
    if not placeholder.is_file() or is_link(placeholder):
        raise PackageError(f"missing observations placeholder: {placeholder}")
    check_folder_safe(output, folder, data_dir)
    folder.mkdir(parents=True, exist_ok=True)
    copy_known_runtime(folder, runtime_src, placeholder)
    manifest = read_manifest(output)
    entries = package_files(folder, manifest, runtime_src, placeholder)
    zip_name = ADDON_ID + "-" + version + ".zip"
    zip_path = output / zip_name
    if is_link(zip_path):
        raise PackageError(f"refusing linked ZIP target: {zip_path}")
    build_zip(zip_path, entries)
    package_list = [{
        "path": arcname,
        "sha256": sha256_file(path),
        "bytes": path.stat().st_size,
    } for arcname, path in entries if not arcname.startswith(ADDON_ID + "/data/")]

    manifest["package"] = {
        "folder": ADDON_ID,
        "zip": zip_name,
        "version": version,
        "readme": ADDON_ID + "/README.md",
        "observations_placeholder": ADDON_ID + "/observations/README.txt",
        "files": package_list,
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
