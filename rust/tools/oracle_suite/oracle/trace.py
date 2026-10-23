"""Parse the opt-in packet field trace (`aaemu_game::net::fields=trace`).

The server logs every client packet as a hex body line and, for the packets in
`net::registry::FIELD_TRACE_PACKETS`, a decoded `fields:` line.  The trace has no
connection id, so callers should pass a log segment recorded around one client
step.
"""

from __future__ import annotations

import math
import re
from dataclasses import dataclass, field
from pathlib import Path

ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")
HEX_RE = re.compile(
    r"C->S (?:level=(\d+) opcode=(0x[0-9A-Fa-f]+) )?(\w+Packet) len=(\d+) body=([0-9a-fA-F ]*)"
)
FIELDS_RE = re.compile(r"C->S (\w+Packet) fields: (.*)")
NUMBER_RE = re.compile(r"[-+]?(?:\d+\.\d*|\.\d+|\d+)(?:[eE][-+]?\d+)?")
KV_RE = re.compile(r"([A-Za-z_][A-Za-z0-9_]*): ([^,}]+)")


@dataclass
class Packet:
    direction: str
    name: str
    level: int | None
    opcode: int | None
    body: bytes
    fields: dict[str, object] = field(default_factory=dict)
    line: str = ""

    def number(self, key: str) -> float | None:
        value = self.fields.get(key)
        if isinstance(value, (int, float)):
            return float(value)
        if isinstance(value, str):
            match = NUMBER_RE.search(value)
            if match:
                return float(match.group(0))
        return None

    def integer(self, key: str) -> int | None:
        value = self.number(key)
        return None if value is None else int(value)


@dataclass
class MoveSample:
    obj_id: int
    time_ms: int
    x: float
    y: float
    z: float
    yaw: float
    vx: float
    vy: float
    vz: float

    @property
    def speed_from_packet(self) -> float:
        return math.hypot(self.vx, self.vy)


def _scalar(text: str) -> object:
    text = text.strip()
    if text in ("true", "false"):
        return text == "true"
    if text.startswith("Some(") and text.endswith(")"):
        text = text[5:-1]
    try:
        return int(text)
    except ValueError:
        pass
    try:
        return float(text)
    except ValueError:
        return text


def parse_fields(text: str) -> dict[str, object]:
    values: dict[str, object] = {}
    for key, raw in KV_RE.findall(text):
        values[key] = _scalar(raw)
    return values


BODY_PARSERS = {
    # name: [(field, offset, width, signed)]
    "CSUpdateActionSlotPacket": [("slot", 0, 1, False), ("kind", 1, 1, False),
                                  ("skill", 2, 4, False)],
    "CSConstructHouseTaxPacket": [("design_id", 0, 4, False), ("x_raw", 4, 8, True),
                                   ("y_raw", 12, 8, True), ("z", 20, 4, False)],
}


def packet_from_line(line: str, pending_fields: dict[str, dict[str, object]]) -> Packet | None:
    line = ANSI_RE.sub("", line)
    match = FIELDS_RE.search(line)
    if match:
        name = match.group(1)
        pending_fields[name] = parse_fields(match.group(2))
        return None
    match = HEX_RE.search(line)
    if not match:
        return None
    name = match.group(3)
    body = bytes.fromhex(match.group(5).replace(" ", ""))
    packet = Packet(
        direction="C->S",
        name=name,
        level=int(match.group(1)) if match.group(1) else None,
        opcode=int(match.group(2), 16) if match.group(2) else None,
        body=body,
        line=line,
    )
    for field_name, offset, width, signed in BODY_PARSERS.get(name, []):
        if len(body) < offset + width:
            continue
        if width == 4 and field_name not in ("design_id",):
            import struct
            value = struct.unpack_from("<I" if not signed else "<i", body, offset)[0]
            if name == "CSConstructHouseTaxPacket" and field_name == "z":
                value = struct.unpack_from("<f", body, offset)[0]
        else:
            value = int.from_bytes(body[offset:offset + width], "little", signed=signed)
        packet.fields[field_name] = value
    if name == "CSConstructHouseTaxPacket":
        packet.fields.setdefault("x", packet.fields.get("x_raw", 0) / 4096.0)
        packet.fields.setdefault("y", packet.fields.get("y_raw", 0) / 4096.0)
    if name in pending_fields:
        packet.fields.update(pending_fields.pop(name))
    return packet


class Segment:
    """A range of game-log lines, plus lazily parsed packets."""

    def __init__(self, text: str):
        self.text = text
        self._packets: list[Packet] | None = None
        self._moves: list[MoveSample] | None = None

    @classmethod
    def read(cls, path: Path, start: int, end: int | None = None) -> "Segment":
        if not path.is_file():
            return cls("")
        with path.open("rb") as handle:
            handle.seek(start)
            data = handle.read() if end is None else handle.read(max(0, end - start))
        return cls(data.decode("utf-8", "replace"))

    def packets(self) -> list[Packet]:
        if self._packets is None:
            pending: dict[str, dict[str, object]] = {}
            result: list[Packet] = []
            for line in self.text.splitlines():
                clean = ANSI_RE.sub("", line)
                fields_match = FIELDS_RE.search(clean)
                if fields_match:
                    name = fields_match.group(1)
                    fields = parse_fields(fields_match.group(2))
                    for packet in reversed(result):
                        if packet.name == name:
                            packet.fields.update(fields)
                            packet.line = clean
                            break
                    else:
                        pending[name] = fields
                    continue
                packet = packet_from_line(line, pending)
                if packet is not None:
                    result.append(packet)
            self._packets = result
        return self._packets

    def named(self, name: str) -> list[Packet]:
        return [packet for packet in self.packets() if packet.name == name]

    def moves(self) -> list[MoveSample]:
        if self._moves is None:
            result: list[MoveSample] = []
            for packet in self.named("CSMoveUnitPacket"):
                sample = move_sample(packet.line)
                if sample is not None:
                    result.append(sample)
            self._moves = result
        return self._moves


OBJ_RE = re.compile(r"obj_id=(\d+)")
POS_RE = re.compile(r"position=Some\(\(([-+0-9.eE]+), ([-+0-9.eE]+), ([-+0-9.eE]+)\)\)")
ROT_RE = re.compile(r"rotation=\(([-+0-9.eE]+), ([-+0-9.eE]+), ([-+0-9.eE]+)\)")
VEL_RE = re.compile(r"velocity=\(([-+0-9.eE]+), ([-+0-9.eE]+), ([-+0-9.eE]+)\)")
TIME_RE = re.compile(r"time: (\d+)")


def move_sample(line: str) -> MoveSample | None:
    line = ANSI_RE.sub("", line)
    obj = OBJ_RE.search(line)
    pos = POS_RE.search(line)
    rot = ROT_RE.search(line)
    vel = VEL_RE.search(line)
    time = TIME_RE.search(line)
    if not (obj and pos and rot and vel and time):
        return None
    return MoveSample(
        obj_id=int(obj.group(1)),
        time_ms=int(time.group(1)),
        x=float(pos.group(1)),
        y=float(pos.group(2)),
        z=float(pos.group(3)),
        yaw=float(rot.group(3)),
        vx=float(vel.group(1)),
        vy=float(vel.group(2)),
        vz=float(vel.group(3)),
    )


def primary_object(samples: list[MoveSample]) -> list[MoveSample]:
    """The moving object of a step: the object with the largest path length."""
    by_object: dict[int, list[MoveSample]] = {}
    for sample in samples:
        by_object.setdefault(sample.obj_id, []).append(sample)
    best: list[MoveSample] = []
    best_path = -1.0
    for rows in by_object.values():
        rows.sort(key=lambda row: row.time_ms)
        path = sum(math.hypot(b.x - a.x, b.y - a.y) for a, b in zip(rows, rows[1:]))
        if path > best_path:
            best, best_path = rows, path
    return best


def path_length(rows: list[MoveSample]) -> float:
    return sum(math.hypot(b.x - a.x, b.y - a.y) for a, b in zip(rows, rows[1:]))


def duration_s(rows: list[MoveSample]) -> float:
    if len(rows) < 2:
        return 0.0
    return (rows[-1].time_ms - rows[0].time_ms) / 1000.0


def instantaneous_speeds(rows: list[MoveSample]) -> list[float]:
    speeds: list[float] = []
    for a, b in zip(rows, rows[1:]):
        dt = (b.time_ms - a.time_ms) / 1000.0
        if dt > 0:
            speeds.append(math.hypot(b.x - a.x, b.y - a.y) / dt)
    return speeds


def percentile(values: list[float], fraction: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    index = min(len(ordered) - 1, max(0, round((len(ordered) - 1) * fraction)))
    return ordered[index]


def median(values: list[float]) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    middle = len(ordered) // 2
    if len(ordered) % 2:
        return ordered[middle]
    return (ordered[middle - 1] + ordered[middle]) / 2.0
