"""Minimal read-only/write `game_pak` (AAPak type A) helper.

The later clients store a 0x200-byte AES-CBC header at the end, a table of 0x150-byte
AES-CBC entries before it, and raw file data at the front. This module reads a pak
(`Pak`) and writes an overlay pak (`write_pak`) with the same layout; it is used by the
Sunbite builder to read the 3.0 client and to package its terrain cells for the Bevy
client. It never modifies a source pak.
"""

from __future__ import annotations

import os
import struct
from pathlib import Path

from Crypto.Cipher import AES

KEY = bytes(
    [0x32, 0x1F, 0x2A, 0xEE, 0xAA, 0x58, 0x4A, 0xB4, 0x9A, 0x6C, 0x9E, 0x09, 0xD5, 0x9E, 0x9C, 0x6F]
)
HEADER = 0x200
ENTRY = 0x150
NAME_LEN = 0x108


def _cipher() -> AES:
    return AES.new(KEY, AES.MODE_CBC, iv=bytes(16))


def _decrypt(b: bytes) -> bytes:
    return _cipher().decrypt(b)


def _encrypt(b: bytes) -> bytes:
    return _cipher().encrypt(b)


class Pak:
    """Open a WIBO/IDEJ/ZERO `game_pak`; files are read with positioned I/O."""

    def __init__(self, path: str | Path) -> None:
        self.path = Path(path)
        self.f = self.path.open("rb")
        self.f.seek(0, 2)
        self.length = self.f.tell()
        self.f.seek(self.length - HEADER)
        h = _decrypt(self.f.read(HEADER))
        if h[0:4] == b"WIBO":
            self.kind, files, extra = "A", *struct.unpack_from("<II", h, 8)
        elif h[8:12] == b"IDEJ":
            self.kind = "B"
            files, extra = struct.unpack_from("<I", h, 12)[0], struct.unpack_from("<I", h, 0)[0]
        elif h[0:4] == b"ZERO":
            self.kind, files, extra = "F", *struct.unpack_from("<II", h, 8)
        else:
            raise ValueError("bad game_pak header: %r" % h[:16])
        total = (files + extra) * ENTRY
        table_at = self.length - HEADER - total
        table_at -= table_at % HEADER
        self.f.seek(table_at)
        fat = self.f.read(total)
        self.entries: list[tuple[str, int, int]] = []
        for i in range(files + extra):
            d = _decrypt(fat[i * ENTRY : (i + 1) * ENTRY])
            if self.kind == "A":
                name = d[0:NAME_LEN].split(b"\0")[0].decode("latin1")
                off, size = struct.unpack_from("<qq", d, NAME_LEN)
            elif self.kind == "B":
                name = d[0x20 : 0x20 + NAME_LEN].split(b"\0")[0].decode("latin1")
                size = struct.unpack_from("<q", d, 0x130)[0]
                off = struct.unpack_from("<q", d, 0x18)[0]
            else:
                name = d[8 : 8 + NAME_LEN].split(b"\0")[0].decode("latin1")
                off, size = struct.unpack_from("<qq", d, 0x110)
            is_file = (i < files) if self.kind in "AF" else (i >= extra)
            if is_file and name and name != "__unused__":
                self.entries.append((name, off, size))
        self.by_name = {name.lower(): (off, size) for name, off, size in self.entries}

    def read(self, name: str) -> bytes:
        off, size = self.by_name[name.lower().replace("\\", "/")]
        self.f.seek(off)
        return self.f.read(size)

    def names(self) -> list[str]:
        return [n for n, _, _ in self.entries]

    def close(self) -> None:
        self.f.close()

    def __enter__(self) -> "Pak":
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()


def write_pak(path: str | Path, files: list[tuple[str, bytes]]) -> int:
    """Write a type-A `game_pak` with `files` (`(normalised name, bytes)`). Returns bytes."""
    data = bytearray()
    table_entries = []
    for name, content in files:
        off = len(data)
        data += content
        table_entries.append((name, off, len(content)))
    data += b"\0" * ((HEADER - len(data) % HEADER) % HEADER)
    table = bytearray()
    for name, off, size in table_entries:
        entry = bytearray(ENTRY)
        encoded = name.encode("latin1")
        if len(encoded) >= NAME_LEN:
            raise ValueError("pak entry name too long: %s" % name)
        entry[0 : len(encoded)] = encoded
        struct.pack_into("<qq", entry, NAME_LEN, off, size)
        table += _encrypt(bytes(entry))
    out = data + table
    out += b"\0" * ((HEADER - len(out) % HEADER) % HEADER)
    header = bytearray(HEADER)
    header[0:4] = b"WIBO"
    struct.pack_into("<II", header, 8, len(table_entries), 0)
    out += _encrypt(bytes(header))
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_bytes(out)
    os.replace(tmp, path)
    return len(out)


def read_pak_member(path: str | Path, name: str) -> bytes:
    with Pak(path) as pak:
        return pak.read(name)
