"""Import AAEmu Classic probe recordings as deterministic evidence records.

The Classic probe addon writes data-only Lua tables (``probe_<session>_<chunk>.lua``
and ``boot_N.lua``). This tool parses that subset without executing Lua and emits
JSON evidence for quests, rewards, item sources, NPC positions and kills. Every
record keeps its probe provenance; fields the recording does not contain are
omitted rather than guessed.

    python import_observations.py --input <observations dir> --output evidence.json
    python import_observations.py --input <dir> --input <dir> --output evidence.json \
        --db <1.2 compact.sqlite3>

With ``--db`` each quest-related evidence record also gets the expected 1.2
quest/reward/supply data and a match/mismatch flag. A mismatch is a suspect for
follow-up, not a verdict that either side is wrong.

Inputs are opened read-only. The JSON output contains no timestamps or absolute
paths, so the same recordings always produce the same bytes.

Item acquisition source codes observed in the recordings: 10 = loot,
34 = quest supply, 35 = quest reward/turn-in, everything else = other.
"""

from __future__ import annotations

import argparse
import bisect
from collections import Counter, defaultdict
import hashlib
import json
import math
from pathlib import Path
import re
import sqlite3
import sys
from typing import Any

SOURCE = "observed in ArcheAge Classic 3.5"
SCHEMA = "aaemu_quest_guide.probe_evidence"
SCHEMA_VERSION = 1
TOOL_VERSION = "0.1.0"
QUEST_ID_TTL_MS = 60000
LOOT_WINDOW_MS = 3000
REWARD_WINDOW_MS = 5000
MATCH_EPSILON = 1.0
ITEM_EVENT_SOURCES = {10: "loot", 34: "quest_supply", 35: "quest_reward"}
SUPPLY_KIND = 3
REWARD_KIND = 8
SUPPLY_DETAIL_TABLES = {
    "QuestActSupplyItem": "quest_act_supply_items",
    "QuestActSupplySelectiveItem": "quest_act_supply_selective_items",
}

PROBE_FILE_RE = re.compile(r"^probe_(?P<session>.+)_(?P<chunk>\d+)\.lua$")
NAME_RE = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
NUMBER_RE = re.compile(
    r"-?(?:0[xX][0-9a-fA-F]+|(?:\d+(?:\.\d*)?|\.\d+)(?:[eE][+-]?\d+)?)"
)
WHITESPACE = " \t\r\n\v\f"


class EvidenceError(RuntimeError):
    """A recording, database or output problem with a human-readable message."""


class LuaParseError(EvidenceError):
    """A token outside the data-only Lua table subset, or malformed Lua data."""


def _line_column(text: str, offset: int) -> tuple[int, int]:
    line = text.count("\n", 0, offset) + 1
    last = text.rfind("\n", 0, offset)
    return line, offset - last


class _LuaParser:
    def __init__(self, text: str, name: str):
        self.text = text
        self.name = name
        self.pos = 0
        self.length = len(text)

    def error(self, message: str, offset: int | None = None):
        at = self.pos if offset is None else offset
        line, column = _line_column(self.text, at)
        raise LuaParseError(
            f"{self.name}: offset {at} (line {line}, column {column}): {message}"
        )

    def _keyword(self, word: str, at: int | None = None) -> bool:
        at = self.pos if at is None else at
        if not self.text.startswith(word, at):
            return False
        end = at + len(word)
        return end >= self.length or not (
            self.text[end].isalnum() or self.text[end] == "_"
        )

    def skip(self):
        while self.pos < self.length:
            char = self.text[self.pos]
            if char in WHITESPACE:
                self.pos += 1
                continue
            if self.text.startswith("--", self.pos):
                if self.text.startswith("--[[", self.pos):
                    end = self.text.find("]]", self.pos + 4)
                    if end < 0:
                        self.error("unterminated long comment")
                    self.pos = end + 2
                else:
                    end = self.text.find("\n", self.pos)
                    self.pos = self.length if end < 0 else end + 1
                continue
            break

    def parse_document(self) -> dict:
        self.skip()
        if self._keyword("return"):
            self.pos += len("return")
            self.skip()
        value = self.parse_value()
        self.skip()
        if self.pos != self.length:
            self.error("unexpected trailing data")
        if not isinstance(value, dict):
            self.error("top-level value must be a table", 0)
        return value

    def parse_value(self) -> Any:
        self.skip()
        if self.pos >= self.length:
            self.error("unexpected end of input")
        char = self.text[self.pos]
        if char == "{":
            return self.parse_table()
        if char in "\"'":
            return self.parse_string()
        if char == "-" or char.isdigit() or char == ".":
            return self.parse_number()
        match = NAME_RE.match(self.text, self.pos)
        if match:
            word = match.group(0)
            if word == "true":
                self.pos = match.end()
                return True
            if word == "false":
                self.pos = match.end()
                return False
            if word == "nil":
                self.pos = match.end()
                return None
            self.error(f"unsupported identifier {word!r}")
        self.error(f"unexpected character {char!r}")

    def parse_number(self) -> int | float:
        start = self.pos
        match = NUMBER_RE.match(self.text, self.pos)
        if not match:
            self.error("expected a number")
        token = match.group(0)
        self.pos = match.end()
        if self.pos < self.length and (
            self.text[self.pos].isalnum() or self.text[self.pos] in "_."
        ):
            self.error("unexpected character after number", start)
        lowered = token.lower()
        if "x" in lowered or ("." not in token and "e" not in lowered):
            value: int | float = int(token, 16 if "x" in lowered else 10)
        else:
            value = float(token)
        if isinstance(value, float) and not math.isfinite(value):
            self.error("number is not finite", start)
        return value

    def parse_string(self) -> str:
        quote = self.text[self.pos]
        start = self.pos
        self.pos += 1
        out: list[str] = []
        escapes = {
            "a": "\a",
            "b": "\b",
            "f": "\f",
            "n": "\n",
            "r": "\r",
            "t": "\t",
            "v": "\v",
            "\\": "\\",
            '"': '"',
            "'": "'",
        }
        while self.pos < self.length:
            char = self.text[self.pos]
            if char == quote:
                self.pos += 1
                return "".join(out)
            if char == "\\":
                escape_at = self.pos
                self.pos += 1
                if self.pos >= self.length:
                    self.error("unterminated escape", escape_at)
                escape = self.text[self.pos]
                if escape in escapes:
                    out.append(escapes[escape])
                    self.pos += 1
                elif escape in "\r\n":
                    self.pos += 1
                    if (
                        escape == "\r"
                        and self.pos < self.length
                        and self.text[self.pos] == "\n"
                    ):
                        self.pos += 1
                    out.append("\n")
                elif escape.isdigit():
                    digits = ""
                    while (
                        len(digits) < 3
                        and self.pos < self.length
                        and self.text[self.pos].isdigit()
                    ):
                        digits += self.text[self.pos]
                        self.pos += 1
                    value = int(digits)
                    if value > 255:
                        self.error("decimal escape out of range", escape_at)
                    out.append(chr(value))
                elif escape == "x":
                    digits = self.text[self.pos + 1 : self.pos + 3]
                    if len(digits) != 2 or not re.fullmatch(
                        r"[0-9a-fA-F]{2}", digits
                    ):
                        self.error("\\x needs two hex digits", escape_at)
                    out.append(chr(int(digits, 16)))
                    self.pos += 3
                elif escape == "z":
                    self.pos += 1
                    while (
                        self.pos < self.length
                        and self.text[self.pos] in WHITESPACE
                    ):
                        self.pos += 1
                else:
                    self.error(f"unsupported escape \\{escape}", escape_at)
                continue
            if char in "\r\n":
                self.error("unescaped newline in string")
            out.append(char)
            self.pos += 1
        self.error("unterminated string", start)

    def parse_table(self) -> dict:
        self.pos += 1
        result: dict[Any, Any] = {}
        next_index = 1
        while True:
            self.skip()
            if self.pos >= self.length:
                self.error("unterminated table")
            if self.text[self.pos] == "}":
                self.pos += 1
                return result
            entry_at = self.pos
            key: Any = None
            if self.text[self.pos] == "[":
                key_at = self.pos
                self.pos += 1
                key_value = self.parse_value()
                self.skip()
                if self.pos >= self.length or self.text[self.pos] != "]":
                    self.error("expected ']' after table key", key_at)
                self.pos += 1
                self.skip()
                if self.pos >= self.length or self.text[self.pos] != "=":
                    self.error("expected '=' after table key")
                self.pos += 1
                if isinstance(key_value, bool) or not isinstance(
                    key_value, (str, int, float)
                ):
                    self.error(
                        "table key must be a string or number", key_at
                    )
                key = _normalize_key(key_value)
            else:
                match = NAME_RE.match(self.text, self.pos)
                if match:
                    word = match.group(0)
                    after = match.end()
                    while (
                        after < self.length and self.text[after] in WHITESPACE
                    ):
                        after += 1
                    if after < self.length and self.text[after] == "=":
                        if word in ("true", "false", "nil"):
                            self.error(
                                f"unexpected keyword {word!r} as a table key",
                                entry_at,
                            )
                        key = word
                        self.pos = after + 1
            if key is None:
                key = next_index
                next_index += 1
            value = self.parse_value()
            if key in result:
                self.error(f"duplicate table key {key!r}", entry_at)
            if value is not None:
                result[key] = value
            self.skip()
            if self.pos >= self.length:
                self.error("unterminated table")
            char = self.text[self.pos]
            if char in ",;":
                self.pos += 1
                continue
            if char == "}":
                self.pos += 1
                return result
            self.error("expected ',' or '}'")


def _normalize_key(value: Any) -> Any:
    if isinstance(value, float) and value.is_integer():
        return int(value)
    return value


def parse_lua_table(text: str, name: str = "<input>") -> dict:
    """Parse the data-only Lua table subset written by the probe."""
    return _LuaParser(text, name).parse_document()


def as_array(value: Any, context: str) -> list:
    """Return the contiguous 1-based array part of a parsed Lua table."""
    if value is None:
        return []
    if not isinstance(value, dict):
        raise EvidenceError(f"{context}: expected a table, got {type(value).__name__}")
    keys = sorted(
        key
        for key in value
        if isinstance(key, int) and not isinstance(key, bool) and key >= 1
    )
    if not keys:
        return []
    if keys != list(range(1, keys[-1] + 1)):
        raise EvidenceError(f"{context}: array keys are not contiguous from 1")
    return [value[key] for key in keys]


def _as_int(value: Any) -> int | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, float) and value.is_integer():
        return int(value)
    if isinstance(value, str) and re.fullmatch(r"-?\d+", value):
        return int(value)
    return None


def _as_number(value: Any) -> int | float | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, (int, float)) and math.isfinite(value):
        return value
    return None


def _as_text(value: Any) -> str | None:
    return value if isinstance(value, str) and value != "" else None


def _as_pos(value: Any) -> list[float] | None:
    if not isinstance(value, dict):
        return None
    values = []
    for key in (1, 2, 3):
        number = _as_number(value.get(key))
        if number is None:
            return None
        values.append(float(number))
    return values


def _text_key(value: Any) -> str | None:
    if value is None:
        return None
    if isinstance(value, bool):
        return None
    return str(value)


def _distance(left: list[float], right: list[float]) -> float:
    return math.sqrt(sum((a - b) ** 2 for a, b in zip(left, right)))


class SourceRecord:
    def __init__(
        self,
        session: str,
        file: str,
        chunk: int,
        seq: int,
        index: int,
        data: dict,
        probe_version: str,
    ):
        self.session = session
        self.file = file
        self.chunk = chunk
        self.seq = seq
        self.index = index
        self.data = data
        self.probe_version = probe_version
        self.lt: Any = None
        self.lt_source: str | None = None

    @property
    def kind(self) -> str:
        return self.data.get("k", "")

    def ms(self) -> int | float | None:
        return _as_number(self.data.get("ms"))


def discover_files(input_paths: list[Path]):
    files: list[Path] = []
    summaries: dict[str, dict] = {}
    for raw in input_paths:
        path = Path(raw)
        if not path.exists():
            raise EvidenceError(f"input not found: {path}")
        resolved = path.resolve()
        key = resolved.as_posix()
        if path.is_dir():
            candidates = sorted(
                (
                    item
                    for item in path.iterdir()
                    if item.is_file()
                    and item.suffix == ".lua"
                    and (
                        item.name.startswith("probe_")
                        or item.name.startswith("boot_")
                    )
                ),
                key=lambda item: item.name.lower(),
            )
            if not candidates:
                raise EvidenceError(
                    f"no probe_*.lua or boot_*.lua files in {path}"
                )
            files.extend(candidates)
            summaries[key] = {
                "name": path.name,
                "probe_files": sum(
                    1 for item in candidates if item.name.startswith("probe_")
                ),
                "boot_files": sum(
                    1 for item in candidates if item.name.startswith("boot_")
                ),
            }
        elif path.is_file():
            files.append(path)
            summaries[key] = {
                "name": path.name,
                "probe_files": 1 if path.name.startswith("probe_") else 0,
                "boot_files": 1 if path.name.startswith("boot_") else 0,
            }
        else:
            raise EvidenceError(f"input is neither a file nor a directory: {path}")

    unique: list[Path] = []
    seen: set[str] = set()
    for path in files:
        key = path.resolve().as_posix()
        if key in seen:
            continue
        seen.add(key)
        unique.append(path)
    unique.sort(key=lambda item: (item.name, item.as_posix()))
    return unique, [summaries[key] for key in sorted(summaries)]


def load_observations(input_paths: list[Path]):
    files, input_summaries = discover_files(input_paths)
    records: list[SourceRecord] = []
    probe_files: list[str] = []
    boot_files: list[str] = []
    by_chunk: dict[tuple[str, int], tuple[str, str]] = {}
    versions: set[str] = set()

    for path in files:
        raw = path.read_bytes()
        try:
            text = raw.decode("utf-8-sig")
        except UnicodeDecodeError as error:
            raise EvidenceError(f"{path.name}: not UTF-8: {error}") from error
        table = parse_lua_table(text, path.name)
        if "records" not in table:
            boot_files.append(path.name)
            continue

        match = PROBE_FILE_RE.match(path.name)
        file_session = match.group("session") if match else None
        file_chunk = int(match.group("chunk")) if match else 0
        top_session = table.get("session", file_session)
        if top_session is None:
            raise EvidenceError(f"{path.name}: no session in file or name")
        top_session = str(top_session)
        if file_session is not None and top_session != file_session:
            raise EvidenceError(
                f"{path.name}: session {top_session!r} does not match filename"
            )
        chunk = _as_int(table.get("chunk"))
        if chunk is None:
            chunk = file_chunk

        records_table = table.get("records")
        array = as_array(records_table, f"{path.name}: records")
        version = _as_text(table.get("probe_version"))
        if version is None:
            for item in array:
                if isinstance(item, dict):
                    version = _as_text(item.get("probe_version"))
                    if version is not None:
                        break
        version = version or "unknown"
        versions.add(version)

        digest = hashlib.sha256(raw).hexdigest()
        chunk_key = (top_session, chunk)
        if chunk_key in by_chunk:
            old_digest, old_name = by_chunk[chunk_key]
            if old_digest != digest:
                raise EvidenceError(
                    f"duplicate session {top_session} chunk {chunk} with "
                    f"different contents: {old_name} and {path.name}"
                )
            continue
        by_chunk[chunk_key] = (digest, path.name)
        probe_files.append(path.name)

        for index, item in enumerate(array, start=1):
            if not isinstance(item, dict):
                raise EvidenceError(
                    f"{path.name}: record {index} is not a table"
                )
            kind = item.get("k")
            if not isinstance(kind, str) or kind == "":
                raise EvidenceError(f"{path.name}: record {index} has no k")
            seq = _as_int(item.get("seq"))
            if seq is None:
                raise EvidenceError(
                    f"{path.name}: record {index} has no integer seq"
                )
            records.append(
                SourceRecord(
                    top_session, path.name, chunk, seq, index, item, version
                )
            )

    if not probe_files:
        raise EvidenceError("no probe_*.lua recording files found")
    records.sort(
        key=lambda record: (
            record.session,
            record.seq,
            record.chunk,
            record.index,
        )
    )
    last_lt: dict[str, Any] = {}
    for record in records:
        own = record.data.get("lt")
        if own is not None:
            record.lt = own
            record.lt_source = "record"
            last_lt[record.session] = own
        elif record.session in last_lt:
            record.lt = last_lt[record.session]
            record.lt_source = "inherited"
    return records, {
        "probe_files": sorted(probe_files),
        "boot_files": sorted(boot_files),
        "sessions": sorted({record.session for record in records}),
        "probe_versions": sorted(versions),
        "inputs": input_summaries,
    }


def _stamp(record: SourceRecord, include_ms: bool = True) -> dict:
    stamp = {
        "source": SOURCE,
        "probe_version": record.probe_version,
        "session": record.session,
        "file": record.file,
        "seq": record.seq,
    }
    if record.lt is not None:
        stamp["lt"] = record.lt
        if record.lt_source == "inherited":
            stamp["lt_inherited"] = True
    if include_ms:
        ms = record.ms()
        if ms is not None:
            stamp["ms"] = ms
    return stamp


def _provenance(record: SourceRecord, include_ms: bool = True) -> dict:
    provenance = {"file": record.file, "seq": record.seq}
    if record.lt is not None:
        provenance["lt"] = record.lt
        if record.lt_source == "inherited":
            provenance["lt_inherited"] = True
    if include_ms:
        ms = record.ms()
        if ms is not None:
            provenance["ms"] = ms
    return provenance


def _event_name(record: SourceRecord) -> str | None:
    return _as_text(record.data.get("e"))


def _event_args(record: SourceRecord) -> list:
    return as_array(record.data.get("args"), f"{record.file}: args")


def _item_id_from_link(link: Any) -> int | None:
    if not isinstance(link, str):
        return None
    match = re.match(r"^\|i(\d+)", link)
    return int(match.group(1)) if match else None


def _clean_state(value: Any) -> dict:
    if not isinstance(value, dict):
        return {}
    state = {}
    for key in ("completed", "active_title", "main_title", "body"):
        if key in value and value[key] is not None:
            state[key] = value[key]
    return state


class _Index:
    def __init__(self, records: list[SourceRecord]):
        self.records = records
        self.kinds = Counter(record.kind for record in records)
        self.item_catalogue: dict[int, dict] = {}
        self.zone_points: dict[str, list[tuple[float, int, int | float]]] = (
            defaultdict(list)
        )
        self.zone_keys: dict[str, list[float]] = defaultdict(list)
        self.quest_timeline: dict[str, list[tuple[float, int, int, str]]] = (
            defaultdict(list)
        )
        self.quest_keys: dict[str, list[tuple[float, int]]] = defaultdict(list)
        self.reward_timeline: dict[str, list[tuple[float, int, int | None, SourceRecord]]] = (
            defaultdict(list)
        )
        self.kill_timeline: dict[str, list[tuple[float, int, SourceRecord]]] = (
            defaultdict(list)
        )
        self.loot_records: dict[str, list[SourceRecord]] = defaultdict(list)
        self._scan()

    def _scan(self):
        for record in self.records:
            data = record.data
            ms = record.ms()
            if _as_number(data.get("zone")) is not None and ms is not None:
                self.zone_points[record.session].append(
                    (float(ms), record.seq, _as_number(data.get("zone")))
                )
                self.zone_keys[record.session].append(float(ms))
            if record.kind == "item_catalogue":
                item_id = _as_int(data.get("item_type_id"))
                if item_id is not None:
                    entry = self.item_catalogue.setdefault(item_id, {})
                    for key in ("name", "grade", "category"):
                        value = data.get(key)
                        if key not in entry and value is not None:
                            entry[key] = value
            if record.kind == "quest_state":
                qid = _as_int(data.get("qid"))
                if qid is not None and ms is not None:
                    self.quest_timeline[record.session].append(
                        (float(ms), record.seq, qid, "quest_state")
                    )
            elif record.kind == "event" and _event_name(record) == "CHAT_MSG_QUEST":
                args = _event_args(record)
                if len(args) > 8:
                    qid = _as_int(args[8])
                    if qid is not None and qid > 0 and ms is not None:
                        self.quest_timeline[record.session].append(
                            (float(ms), record.seq, qid, "chat")
                        )
            elif record.kind == "quest_reward":
                qid = _as_int(data.get("quest_id"))
                if ms is not None:
                    self.reward_timeline[record.session].append(
                        (float(ms), record.seq, qid, record)
                    )
            elif record.kind == "kill":
                if ms is not None:
                    self.kill_timeline[record.session].append(
                        (float(ms), record.seq, record)
                    )
            elif record.kind == "loot":
                self.loot_records[record.session].append(record)

        for session, points in self.zone_points.items():
            points.sort(key=lambda item: (item[0], item[1], item[2]))
            self.zone_keys[session] = [item[0] for item in points]
        for session, points in self.quest_timeline.items():
            points.sort(key=lambda item: (item[0], item[1], item[2]))
            self.quest_keys[session] = [
                (item[0], item[1]) for item in points
            ]
        for session, points in self.reward_timeline.items():
            points.sort(key=lambda item: (item[0], item[1]))
        for session, points in self.kill_timeline.items():
            points.sort(key=lambda item: (item[0], item[1]))

    def zone_at(self, session: str, ms: int | float | None):
        if ms is None or not self.zone_points.get(session):
            return None, None
        keys = self.zone_keys[session]
        points = self.zone_points[session]
        index = bisect.bisect_right(keys, float(ms))
        candidates = []
        if index > 0:
            candidates.append(points[index - 1])
        if index < len(points):
            candidates.append(points[index])
        chosen = min(candidates, key=lambda item: (abs(item[0] - ms), item[1]))
        return chosen[2], "session_context"

    def last_quest_at(self, session: str, ms: int | float | None, seq: int):
        if ms is None:
            return None, None, None
        keys = self.quest_keys.get(session, [])
        if not keys:
            return None, None, None
        index = bisect.bisect_right(keys, (float(ms), seq))
        if index <= 0:
            return None, None, None
        point_ms, point_seq, qid, kind = self.quest_timeline[session][index - 1]
        delta = float(ms) - point_ms
        if delta < 0 or delta > QUEST_ID_TTL_MS:
            return None, None, None
        return qid, kind, delta

    def reward_at(self, session: str, ms: int | float | None):
        if ms is None:
            return None
        points = [
            (abs(point_ms - float(ms)), point_seq, qid, record)
            for point_ms, point_seq, qid, record in self.reward_timeline.get(
                session, []
            )
            if abs(point_ms - float(ms)) <= REWARD_WINDOW_MS
        ]
        if not points:
            return None
        _delta, _seq, qid, record = min(
            points, key=lambda item: (item[0], item[1])
        )
        return qid, record

    def last_kill_at(self, session: str, ms: int | float | None):
        if ms is None:
            return None
        points = self.kill_timeline.get(session, [])
        best = None
        for point_ms, _seq, record in points:
            if point_ms <= float(ms) and float(ms) - point_ms <= LOOT_WINDOW_MS:
                best = record
        return best


def _item_sources_for_reward(record: SourceRecord) -> list[tuple[int, int | None]]:
    items = []
    for item in as_array(record.data.get("items"), f"{record.file}: items"):
        if not isinstance(item, dict):
            continue
        item_id = _as_int(item.get("item_type_id"))
        if item_id is None:
            item_id = _as_int(item.get("item_id"))
        if item_id is None:
            item_id = _as_int(item.get("id"))
        if item_id is None:
            continue
        items.append((item_id, _as_int(item.get("count"))))
    return items


def _unit_from_fingerprint(fingerprint: Any, fallback_id: Any = None):
    name = None
    unit_id = _text_key(fallback_id)
    if isinstance(fingerprint, dict):
        name = _as_text(fingerprint.get("name"))
        if unit_id is None:
            unit_id = _text_key(fingerprint.get("id"))
    return name, unit_id


class EvidenceBuilder:
    def __init__(self, records: list[SourceRecord]):
        self.records = records
        self.index = _Index(records)
        self.quests: dict[int, dict] = {}
        self.rewards: list[dict] = []
        self.item_acquisitions: list[dict] = []
        self.npcs: dict[str, dict] = {}
        self.kills: dict[str, dict] = {}
        self._reward_seen: set[tuple[str, int, int, int | None, int | None]] = set()
        self._loot_seen: set[tuple[str, int, int | None, int | None]] = set()
        self._build()

    def _ensure_quest(self, qid: int, record: SourceRecord) -> dict:
        entry = self.quests.get(qid)
        if entry is None:
            entry = {
                "quest_id": qid,
                "_first": record,
                "observations": [],
                "progress": [],
                "completion_evidence": [],
                "_completion_keys": set(),
                "_accepted": None,
                "_titles": [],
            }
            self.quests[qid] = entry
        return entry

    def _add_completion(
        self,
        entry: dict,
        kind: str,
        record: SourceRecord,
        extra: dict | None = None,
    ):
        key = (kind, record.session, record.seq)
        if key in entry["_completion_keys"]:
            return
        entry["_completion_keys"].add(key)
        evidence = {"kind": kind}
        evidence.update(_provenance(record))
        if extra:
            evidence.update(extra)
        entry["completion_evidence"].append(evidence)

    def _build(self):
        self._build_quests()
        self._build_rewards()
        self._build_item_sources()
        self._build_npcs()
        self._build_kills()

    def _build_quests(self):
        for record in self.records:
            data = record.data
            kind = record.kind
            if kind == "quest_state":
                qid = _as_int(data.get("qid"))
                if qid is None:
                    continue
                entry = self._ensure_quest(qid, record)
                state = _clean_state(data.get("state"))
                transition = data.get("transition")
                observation = _provenance(record)
                observation["transition"] = transition
                event = data.get("event")
                if event is not None:
                    observation["event"] = event
                if state:
                    observation["state"] = state
                entry["observations"].append(observation)
                title = _as_text(state.get("main_title"))
                if title is not None and title not in entry["_titles"]:
                    entry["_titles"].append(title)
                if transition == "accepted" and entry["_accepted"] is None:
                    accepted = {"evidence": "quest_state"}
                    accepted.update(_provenance(record))
                    if state:
                        accepted["state"] = state
                    entry["_accepted"] = accepted
                active_title = state.get("active_title")
                if isinstance(active_title, str) and active_title:
                    progress = {
                        "active_title": active_title,
                    }
                    if transition is not None:
                        progress["transition"] = transition
                    if data.get("event") is not None:
                        progress["event"] = data["event"]
                    progress.update(_provenance(record))
                    if progress not in entry["progress"]:
                        entry["progress"].append(progress)
                if state.get("completed") is True:
                    self._add_completion(entry, "quest_state", record)
            elif kind == "event" and _event_name(record) == "CHAT_MSG_QUEST":
                args = _event_args(record)
                if len(args) <= 8:
                    continue
                qid = _as_int(args[8])
                if qid is None or qid <= 0:
                    continue
                self._ensure_quest(qid, record)
            elif kind == "quest_reward":
                qid = _as_int(data.get("quest_id"))
                if qid is not None:
                    entry = self._ensure_quest(qid, record)
                    self._add_completion(entry, "quest_reward", record)
            elif (
                kind == "event"
                and _event_name(record) == "REMOVED_ITEM"
            ):
                args = _event_args(record)
                if len(args) <= 3 or _as_int(args[3]) != 35:
                    continue
                qid, source_kind, delta = self.index.last_quest_at(
                    record.session, record.ms(), record.seq
                )
                if qid is None:
                    continue
                entry = self._ensure_quest(qid, record)
                self._add_completion(
                    entry,
                    "turn_in",
                    record,
                    {
                        "quest_id_source": source_kind,
                        "last_quest_delta_ms": delta,
                    },
                )

    def _build_rewards(self):
        for record in self.records:
            if record.kind != "quest_reward":
                continue
            data = record.data
            item = {
                "kind": "reward",
            }
            item.update(_stamp(record))
            qid = _as_int(data.get("quest_id"))
            if qid is not None:
                item["quest_id"] = qid
            items = []
            for item_id, count in _item_sources_for_reward(record):
                entry = {"id": item_id}
                if count is not None:
                    entry["count"] = count
                items.append(entry)
            if items:
                item["items"] = items
            ms = record.ms()
            for item_id, count in _item_sources_for_reward(record):
                self._reward_seen.add(
                    (
                        record.session,
                        item_id,
                        count,
                        qid,
                        None if ms is None else int(ms),
                    )
                )
            money = _as_number(data.get("money_delta"))
            if money is not None:
                item["money"] = money
            xp = _as_number(data.get("exp_delta"))
            if xp is not None:
                item["xp"] = xp
            zone = _as_number(data.get("zone"))
            if zone is not None:
                item["zone"] = zone
            pos = _as_pos(data.get("pos"))
            if pos is not None:
                item["pos"] = pos
            self.rewards.append(item)

    def _add_acquisition(
        self,
        record: SourceRecord,
        item_id: int,
        count: int | None,
        via: str,
        quest_id: int | None = None,
        quest_id_source: str | None = None,
        unit: dict | None = None,
        probe_source: int | None = None,
        zone: int | float | None = None,
        pos: list[float] | None = None,
    ):
        acquisition = {
            "item_id": item_id,
            "count": count,
            "via": via,
            "record": record,
            "quest_id": quest_id,
            "quest_id_source": quest_id_source,
            "unit": unit,
            "probe_source": probe_source,
            "zone": zone,
            "pos": pos,
        }
        self.item_acquisitions.append(acquisition)

    def _build_item_sources(self):
        for record in self.records:
            kind = record.kind
            data = record.data
            if kind == "quest_reward":
                qid = _as_int(data.get("quest_id"))
                zone = _as_number(data.get("zone"))
                pos = _as_pos(data.get("pos"))
                for item_id, count in _item_sources_for_reward(record):
                    self._add_acquisition(
                        record,
                        item_id,
                        count,
                        "quest_reward",
                        quest_id=qid,
                        quest_id_source="quest_reward",
                        probe_source=35,
                        zone=zone,
                        pos=pos,
                    )
            elif kind == "loot":
                item_id = _as_int(data.get("item_type_id"))
                if item_id is None:
                    continue
                count = _as_int(data.get("count"))
                name, unit_id = _unit_from_fingerprint(
                    data.get("npc"), data.get("npc_id")
                )
                unit = {}
                if name is not None:
                    unit["name"] = name
                if unit_id is not None:
                    unit["unit_id"] = unit_id
                if not unit:
                    unit = None
                self._loot_seen.add(
                    (
                        record.session,
                        item_id,
                        count,
                        _kill_key(record),
                    )
                )
                self._add_acquisition(
                    record,
                    item_id,
                    count,
                    "loot",
                    unit=unit,
                    probe_source=10,
                    zone=_as_number(data.get("zone")),
                    pos=_as_pos(data.get("pos")),
                )
            elif kind == "event" and _event_name(record) == "ADDED_ITEM":
                args = _event_args(record)
                if len(args) < 3:
                    continue
                item_id = _item_id_from_link(args[0])
                if item_id is None:
                    continue
                count = _as_int(args[1])
                probe_source = _as_int(args[2])
                ms = record.ms()
                zone = _as_number(data.get("zone"))
                pos = _as_pos(data.get("pos"))
                if probe_source == 35:
                    if self._reward_matches(record.session, item_id, count, ms):
                        continue
                    qid, _reward = self.index.reward_at(record.session, ms)
                    source_kind = "quest_reward" if qid is not None else None
                    if qid is None:
                        qid, source_kind, _delta = self.index.last_quest_at(
                            record.session, ms, record.seq
                        )
                        if qid is not None:
                            source_kind = "last_quest_event"
                    self._add_acquisition(
                        record,
                        item_id,
                        count,
                        "quest_reward",
                        quest_id=qid,
                        quest_id_source=source_kind,
                        probe_source=35,
                        zone=zone,
                        pos=pos,
                    )
                elif probe_source == 34:
                    qid, source_kind, _delta = self.index.last_quest_at(
                        record.session, ms, record.seq
                    )
                    self._add_acquisition(
                        record,
                        item_id,
                        count,
                        "quest_supply",
                        quest_id=qid,
                        quest_id_source=(
                            "last_quest_event" if qid is not None else None
                        ),
                        probe_source=34,
                        zone=zone,
                        pos=pos,
                    )
                elif probe_source == 10:
                    kill = self.index.last_kill_at(record.session, ms)
                    if kill is not None and self._loot_matches(
                        record.session, item_id, count, kill
                    ):
                        continue
                    unit = None
                    if kill is not None:
                        name, unit_id = _unit_from_fingerprint(
                            kill.data.get("fingerprint"),
                            kill.data.get("target_id"),
                        )
                        unit = {}
                        if name is not None:
                            unit["name"] = name
                        if unit_id is not None:
                            unit["unit_id"] = unit_id
                        if not unit:
                            unit = None
                    self._add_acquisition(
                        record,
                        item_id,
                        count,
                        "loot",
                        unit=unit,
                        probe_source=10,
                        zone=zone,
                        pos=pos,
                    )
                else:
                    self._add_acquisition(
                        record,
                        item_id,
                        count,
                        "other",
                        probe_source=probe_source,
                        zone=zone,
                        pos=pos,
                    )

    def _reward_matches(
        self,
        session: str,
        item_id: int,
        count: int | None,
        ms: int | float | None,
    ) -> bool:
        if ms is None:
            return False
        for seen_session, seen_item, seen_count, _qid, seen_ms in self._reward_seen:
            if (
                seen_session == session
                and seen_item == item_id
                and seen_count == count
                and seen_ms is not None
                and abs(seen_ms - float(ms)) <= REWARD_WINDOW_MS
            ):
                return True
        return False

    def _loot_matches(
        self, session: str, item_id: int, count: int | None, kill: SourceRecord
    ) -> bool:
        for seen_session, seen_item, seen_count, unit in self._loot_seen:
            if (
                seen_session == session
                and seen_item == item_id
                and seen_count == count
                and unit == _kill_key(kill)
            ):
                return True
        return False

    def _build_npcs(self):
        for record in self.records:
            kind = record.kind
            data = record.data
            if kind == "npc_catalogue":
                fingerprint = data.get("fingerprint")
                name, unit_id = _unit_from_fingerprint(fingerprint)
                ids = set()
                for value in as_array(data.get("ids"), f"{record.file}: ids"):
                    text = _text_key(value)
                    if text is not None:
                        ids.add(text)
                if unit_id is not None:
                    ids.add(unit_id)
                for position in as_array(
                    data.get("positions"), f"{record.file}: positions"
                ):
                    if not isinstance(position, dict):
                        continue
                    pos = _as_pos(position.get("pos"))
                    if pos is None:
                        continue
                    ms = _as_number(position.get("ms"))
                    if ms is None:
                        ms = record.ms()
                    zone, zone_source = self.index.zone_at(record.session, ms)
                    self._add_npc_position(
                        name,
                        ids,
                        pos,
                        record,
                        _as_text(position.get("origin")) or "npc_catalogue",
                        ms,
                        zone,
                        zone_source,
                    )
            elif kind in ("target", "pet"):
                fingerprint = data.get("fingerprint")
                if not isinstance(fingerprint, dict):
                    continue
                name, unit_id = _unit_from_fingerprint(
                    fingerprint, data.get("id")
                )
                pos = _as_pos(fingerprint.get("position"))
                if pos is None:
                    continue
                ids = {unit_id} if unit_id is not None else set()
                self._add_npc_position(
                    name,
                    ids,
                    pos,
                    record,
                    kind,
                    record.ms(),
                    _as_number(data.get("zone")),
                    "record" if _as_number(data.get("zone")) is not None else None,
                )
            elif kind == "kill":
                fingerprint = data.get("fingerprint")
                name, unit_id = _unit_from_fingerprint(
                    fingerprint, data.get("target_id")
                )
                pos = _as_pos(data.get("position"))
                if pos is None:
                    continue
                ids = {unit_id} if unit_id is not None else set()
                zone, zone_source = self.index.zone_at(
                    record.session, record.ms()
                )
                self._add_npc_position(
                    name,
                    ids,
                    pos,
                    record,
                    "kill",
                    record.ms(),
                    zone,
                    zone_source,
                )

    def _add_npc_position(
        self,
        name: str | None,
        unit_ids: set[str],
        pos: list[float],
        record: SourceRecord,
        origin: str,
        ms: int | float | None,
        zone: int | float | None,
        zone_source: str | None,
    ):
        key = name or (min(unit_ids) if unit_ids else None)
        if key is None:
            return
        entry = self.npcs.get(key)
        if entry is None:
            entry = {
                "_first": record,
                "name": name,
                "unit_ids": set(),
                "positions": [],
            }
            self.npcs[key] = entry
        entry["unit_ids"].update(unit_ids)
        for existing in entry["positions"]:
            if _distance(existing["_pos"], pos) <= MATCH_EPSILON:
                if existing.get("zone") is None and zone is not None:
                    existing["zone"] = zone
                    if zone_source is not None:
                        existing["zone_source"] = zone_source
                if origin not in existing["origins"]:
                    existing["origins"].append(origin)
                return
        point = {
            "x": pos[0],
            "y": pos[1],
            "z": pos[2],
            "origins": [origin],
            "file": record.file,
            "seq": record.seq,
            "_pos": pos,
        }
        if ms is not None:
            point["ms"] = ms
        if zone is not None:
            point["zone"] = zone
            if zone_source is not None:
                point["zone_source"] = zone_source
        entry["positions"].append(point)

    def _build_kills(self):
        for record in self.records:
            if record.kind != "kill":
                continue
            data = record.data
            fingerprint = data.get("fingerprint")
            name, unit_id = _unit_from_fingerprint(
                fingerprint, data.get("target_id")
            )
            key = name or unit_id
            if key is None:
                continue
            entry = self.kills.get(key)
            if entry is None:
                entry = {
                    "_first": record,
                    "name": name,
                    "unit_ids": set(),
                    "count": 0,
                    "positions": [],
                    "kills": [],
                }
                self.kills[key] = entry
            entry["count"] += 1
            if unit_id is not None:
                entry["unit_ids"].add(unit_id)
            zone, zone_source = self.index.zone_at(
                record.session, record.ms()
            )
            pos = _as_pos(data.get("position"))
            if pos is not None:
                for existing in entry["positions"]:
                    if _distance(existing["_pos"], pos) <= MATCH_EPSILON:
                        if existing.get("zone") is None and zone is not None:
                            existing["zone"] = zone
                            if zone_source is not None:
                                existing["zone_source"] = zone_source
                        break
                else:
                    point = {
                        "x": pos[0],
                        "y": pos[1],
                        "z": pos[2],
                        "file": record.file,
                        "seq": record.seq,
                        "_pos": pos,
                    }
                    if record.ms() is not None:
                        point["ms"] = record.ms()
                    if zone is not None:
                        point["zone"] = zone
                        if zone_source is not None:
                            point["zone_source"] = zone_source
                    entry["positions"].append(point)
            kill = _provenance(record)
            exp = _as_number(data.get("exp"))
            if exp is not None:
                kill["exp"] = exp
            if unit_id is not None:
                kill["unit_id"] = unit_id
            if zone is not None:
                kill["zone"] = zone
            entry["kills"].append(kill)

    def evidence(self) -> dict:
        records = []
        records.extend(self._quest_records())
        records.extend(self.rewards)
        records.extend(self._item_records())
        records.extend(self._npc_records())
        records.extend(self._kill_records())
        return records

    def _quest_records(self) -> list[dict]:
        out = []
        for qid in sorted(self.quests):
            entry = self.quests[qid]
            record = {
                "kind": "quest",
                "quest_id": qid,
            }
            record.update(_stamp(entry["_first"]))
            title = entry["_titles"][0] if entry["_titles"] else None
            if title is not None:
                record["title"] = title
            if entry["_accepted"] is not None:
                record["accepted"] = entry["_accepted"]
            if entry["progress"]:
                record["progress"] = entry["progress"]
            evidence = sorted(
                entry["completion_evidence"],
                key=lambda item: (item.get("seq", 0), item.get("kind", "")),
            )
            record["completed"] = bool(evidence)
            record["completion_inferred"] = any(
                item["kind"] in ("quest_reward", "turn_in")
                for item in evidence
            )
            record["completion_evidence"] = evidence
            if entry["observations"]:
                record["observations"] = entry["observations"]
            out.append(record)
        return out

    def _item_records(self) -> list[dict]:
        grouped: dict[int, dict] = {}
        for acquisition in self.item_acquisitions:
            item_id = acquisition["item_id"]
            entry = grouped.get(item_id)
            if entry is None:
                entry = {"_first": acquisition["record"], "sources": {}}
                grouped[item_id] = entry
            unit = acquisition["unit"]
            unit_key = None
            if unit is not None:
                unit_key = (
                    unit.get("name"),
                    unit.get("unit_id"),
                )
            source_key = (
                acquisition["via"],
                acquisition["quest_id"],
                unit_key,
                acquisition["probe_source"],
            )
            source = entry["sources"].get(source_key)
            if source is None:
                source = {
                    "via": acquisition["via"],
                    "observations": [],
                }
                entry["sources"][source_key] = source
            if acquisition["count"] is not None:
                source["count"] = source.get("count", 0) + acquisition["count"]
            else:
                source["count_unknown"] = True
            if acquisition["quest_id"] is not None:
                source["quest_id"] = acquisition["quest_id"]
            if acquisition["quest_id_source"] is not None:
                source["quest_id_source"] = acquisition["quest_id_source"]
            if unit is not None:
                source["unit"] = dict(unit)
            if acquisition["probe_source"] is not None:
                source["probe_source"] = acquisition["probe_source"]
            observation = _provenance(acquisition["record"])
            if acquisition["count"] is not None:
                observation["count"] = acquisition["count"]
            if acquisition["zone"] is not None:
                observation["zone"] = acquisition["zone"]
            if acquisition["pos"] is not None:
                observation["pos"] = acquisition["pos"]
            source["observations"].append(observation)
            if (
                acquisition["record"].session,
                acquisition["record"].seq,
            ) < (entry["_first"].session, entry["_first"].seq):
                entry["_first"] = acquisition["record"]

        out = []
        for item_id in sorted(grouped):
            entry = grouped[item_id]
            sources = []
            for key in sorted(
                entry["sources"],
                key=lambda item: (
                    item[0],
                    -1 if item[1] is None else item[1],
                    "" if item[2] is None else str(item[2]),
                    -1 if item[3] is None else item[3],
                ),
            ):
                source = entry["sources"][key]
                source["observations"].sort(
                    key=lambda item: (item.get("seq", 0), item.get("file", ""))
                )
                sources.append(source)
            record = {
                "kind": "item_source",
                "item_id": item_id,
            }
            record.update(_stamp(entry["_first"]))
            catalogue = self.index.item_catalogue.get(item_id)
            if catalogue and catalogue.get("name") is not None:
                record["name"] = catalogue["name"]
            record["how"] = sorted({source["via"] for source in sources})
            record["sources"] = sources
            out.append(record)
        return out

    def _npc_records(self) -> list[dict]:
        out = []
        for key in sorted(self.npcs):
            entry = self.npcs[key]
            record = {
                "kind": "npc_position",
            }
            record.update(_stamp(entry["_first"]))
            unit = {}
            if entry["name"] is not None:
                unit["name"] = entry["name"]
            if entry["unit_ids"]:
                unit["unit_ids"] = sorted(entry["unit_ids"])
            record["unit"] = unit
            positions = []
            for point in entry["positions"]:
                item = {
                    "x": point["x"],
                    "y": point["y"],
                    "z": point["z"],
                    "origins": point["origins"],
                    "file": point["file"],
                    "seq": point["seq"],
                }
                if "ms" in point:
                    item["ms"] = point["ms"]
                if "zone" in point:
                    item["zone"] = point["zone"]
                    if "zone_source" in point:
                        item["zone_source"] = point["zone_source"]
                positions.append(item)
            record["positions"] = positions
            out.append(record)
        return out

    def _kill_records(self) -> list[dict]:
        out = []
        for key in sorted(self.kills):
            entry = self.kills[key]
            record = {
                "kind": "kill",
            }
            record.update(_stamp(entry["_first"]))
            unit = {}
            if entry["name"] is not None:
                unit["name"] = entry["name"]
            if entry["unit_ids"]:
                unit["unit_ids"] = sorted(entry["unit_ids"])
            record["unit"] = unit
            record["count"] = entry["count"]
            positions = []
            for point in entry["positions"]:
                item = {
                    "x": point["x"],
                    "y": point["y"],
                    "z": point["z"],
                    "file": point["file"],
                    "seq": point["seq"],
                }
                if "ms" in point:
                    item["ms"] = point["ms"]
                if "zone" in point:
                    item["zone"] = point["zone"]
                    if "zone_source" in point:
                        item["zone_source"] = point["zone_source"]
                positions.append(item)
            record["positions"] = positions
            record["kills"] = entry["kills"]
            out.append(record)
        return out


def _kill_key(record: SourceRecord) -> str | None:
    fingerprint = record.data.get("fingerprint")
    if isinstance(fingerprint, dict):
        key = _as_text(fingerprint.get("key"))
        if key is not None:
            return key
    return _text_key(record.data.get("npc_key"))


def _read_only_connection(path: Path) -> sqlite3.Connection:
    try:
        return sqlite3.connect(path.resolve().as_uri() + "?mode=ro", uri=True)
    except sqlite3.Error as error:
        raise EvidenceError(f"cannot open database {path}: {error}") from error


def load_expectations(db_path: Path, quest_ids: set[int]) -> dict[int, dict]:
    if not db_path.is_file():
        raise EvidenceError(f"database not found: {db_path}")
    conn = _read_only_connection(db_path)
    conn.row_factory = sqlite3.Row
    try:
        tables = {
            row[0]
            for row in conn.execute(
                "SELECT name FROM sqlite_master WHERE type = 'table'"
            )
        }
        required = {"quest_contexts", "quest_components", "quest_acts"}
        missing = sorted(required - tables)
        if missing:
            raise EvidenceError(
                f"{db_path.name}: not a 1.2 compact database (missing "
                + ", ".join(missing)
                + ")"
            )
        if not quest_ids:
            return {}
        placeholders = ",".join("?" for _ in quest_ids)
        ids = sorted(quest_ids)
        contexts = {
            row["id"]: row
            for row in conn.execute(
                f"SELECT id, name, LEVEL FROM quest_contexts "
                f"WHERE id IN ({placeholders})",
                ids,
            )
        }
        components: dict[int, list[sqlite3.Row]] = defaultdict(list)
        for row in conn.execute(
            f"SELECT id, quest_context_id, component_kind_id "
            f"FROM quest_components WHERE quest_context_id IN ({placeholders})",
            ids,
        ):
            components[row["quest_context_id"]].append(row)
        components_by_id = {
            row["id"]: row
            for rows in components.values()
            for row in rows
        }
        component_ids = sorted(components_by_id)
        acts: dict[int, list[sqlite3.Row]] = defaultdict(list)
        if component_ids:
            act_placeholders = ",".join("?" for _ in component_ids)
            for row in conn.execute(
                f"SELECT id, quest_component_id, act_detail_id, act_detail_type "
                f"FROM quest_acts WHERE quest_component_id IN ({act_placeholders})",
                component_ids,
            ):
                acts[row["quest_component_id"]].append(row)
        detail_ids: dict[str, set[int]] = defaultdict(set)
        for rows in acts.values():
            for row in rows:
                table = SUPPLY_DETAIL_TABLES.get(row["act_detail_type"])
                if table is not None and table in tables:
                    detail_ids[table].add(row["act_detail_id"])
        details: dict[str, dict[int, sqlite3.Row]] = {}
        for table, ids_for_table in detail_ids.items():
            placeholders_for_table = ",".join("?" for _ in ids_for_table)
            details[table] = {
                row["id"]: row
                for row in conn.execute(
                    f"SELECT id, item_id, count FROM {table} "
                    f"WHERE id IN ({placeholders_for_table})",
                    sorted(ids_for_table),
                )
            }
        expectations = {}
        for qid in ids:
            context = contexts.get(qid)
            expectation = {
                "quest_exists": context is not None,
                "supply_items": [],
                "reward_items": [],
            }
            if context is not None:
                expectation["title"] = context["name"]
                expectation["level"] = context["LEVEL"]
                for component in components.get(qid, []):
                    kind = component["component_kind_id"]
                    if kind not in (SUPPLY_KIND, REWARD_KIND):
                        continue
                    target = (
                        expectation["supply_items"]
                        if kind == SUPPLY_KIND
                        else expectation["reward_items"]
                    )
                    for act in acts.get(component["id"], []):
                        table = SUPPLY_DETAIL_TABLES.get(
                            act["act_detail_type"]
                        )
                        if table is None:
                            continue
                        row = details.get(table, {}).get(act["act_detail_id"])
                        if row is None:
                            continue
                        target.append(
                            {"id": row["item_id"], "count": row["count"]}
                        )
                expectation["supply_items"].sort(
                    key=lambda item: (item["id"], item["count"])
                )
                expectation["reward_items"].sort(
                    key=lambda item: (item["id"], item["count"])
                )
            expectations[qid] = expectation
        return expectations
    except sqlite3.Error as error:
        raise EvidenceError(f"database error: {error}") from error
    finally:
        conn.close()


def _expected_count(items: list[dict], item_id: int) -> int | None:
    counts = [item["count"] for item in items if item["id"] == item_id]
    return sum(counts) if counts else None


def _compare_items(
    observed: list[tuple[str, int, int | None]],
    expectation: dict,
) -> list[str]:
    notes = []
    for phase, item_id, count in observed:
        key = "supply_items" if phase == "quest_supply" else "reward_items"
        items = expectation.get(key, [])
        expected = _expected_count(items, item_id)
        if expected is None:
            notes.append(
                f"observed {phase} item {item_id} is not a 1.2 {key.replace('_items', '')} item"
            )
        elif count is not None and expected != count:
            notes.append(
                f"observed {phase} item {item_id} count {count} != 1.2 count {expected}"
            )
    return notes


def annotate_evidence(evidence: dict, expectations: dict[int, dict]):
    observed_by_quest: dict[int, list[tuple[str, int, int | None]]] = defaultdict(list)
    for record in evidence["records"]:
        if record.get("kind") != "item_source":
            continue
        for source in record.get("sources", []):
            qid = source.get("quest_id")
            if qid is None or source.get("via") not in (
                "quest_supply",
                "quest_reward",
            ):
                continue
            observed_by_quest[qid].append(
                (source["via"], record["item_id"], source.get("count"))
            )

    matches = Counter()
    for record in evidence["records"]:
        kind = record.get("kind")
        if kind == "quest":
            qid = record["quest_id"]
            expectation = expectations.get(qid)
            if expectation is None or not expectation.get("quest_exists"):
                record["expectation_1_2"] = {"quest_exists": False}
                record["match"] = "mismatch"
                record["match_notes"] = [
                    "quest not found in 1.2 compact.sqlite3"
                ]
                matches["mismatch"] += 1
                continue
            record["expectation_1_2"] = expectation
            observed = observed_by_quest.get(qid, [])
            if not observed:
                record["match"] = "unknown"
                record["match_notes"] = [
                    "quest exists in 1.2 but no supply/reward item was observed"
                ]
                matches["unknown"] += 1
                continue
            notes = _compare_items(observed, expectation)
            record["match"] = "mismatch" if notes else "match"
            if notes:
                record["match_notes"] = notes
            matches[record["match"]] += 1
        elif kind == "reward":
            qid = record.get("quest_id")
            if qid is None:
                record["match"] = "unknown"
                record["match_notes"] = [
                    "reward has no quest id to compare against 1.2"
                ]
                matches["unknown"] += 1
                continue
            expectation = expectations.get(qid)
            if expectation is None or not expectation.get("quest_exists"):
                record["expectation_1_2"] = {"quest_exists": False}
                record["match"] = "mismatch"
                record["match_notes"] = [
                    "quest not found in 1.2 compact.sqlite3"
                ]
                matches["mismatch"] += 1
                continue
            record["expectation_1_2"] = expectation
            observed = [
                ("quest_reward", item["id"], item.get("count"))
                for item in record.get("items", [])
            ]
            notes = _compare_items(observed, expectation)
            record["match"] = "mismatch" if notes else "match"
            if notes:
                record["match_notes"] = notes
            matches[record["match"]] += 1
        elif kind == "item_source":
            applied = False
            notes = []
            quest_expectations = {}
            for source in record.get("sources", []):
                qid = source.get("quest_id")
                phase = source.get("via")
                if qid is None or phase not in ("quest_supply", "quest_reward"):
                    continue
                expectation = expectations.get(qid)
                if expectation is None or not expectation.get("quest_exists"):
                    source["match"] = "mismatch"
                    source["match_notes"] = [
                        "quest not found in 1.2 compact.sqlite3"
                    ]
                    notes.extend(source["match_notes"])
                    applied = True
                    continue
                quest_expectations[qid] = expectation
                source_notes = _compare_items(
                    [(phase, record["item_id"], source.get("count"))],
                    expectation,
                )
                source["match"] = "mismatch" if source_notes else "match"
                if source_notes:
                    source["match_notes"] = source_notes
                    notes.extend(source_notes)
                applied = True
            if quest_expectations:
                record["expectation_1_2"] = {
                    "quests": [
                        dict(expectation, quest_id=qid)
                        for qid, expectation in sorted(quest_expectations.items())
                    ]
                }
            if applied:
                record["match"] = "mismatch" if notes else "match"
                if notes:
                    record["match_notes"] = notes
                matches[record["match"]] += 1
            else:
                record["match"] = "unknown"
                record["match_notes"] = [
                    "item source is not linked to a 1.2 quest supply or reward"
                ]
                matches["unknown"] += 1
    return matches


def build_summary(
    evidence: dict,
    records: list[SourceRecord],
    file_info: dict,
    db_info: dict | None = None,
) -> dict:
    kinds = Counter(record.kind for record in records)
    evidence_records = evidence["records"]
    evidence_kinds = Counter(record["kind"] for record in evidence_records)
    quests = []
    completions = []
    items = []
    kills = []
    for record in evidence_records:
        if record["kind"] == "quest":
            quest = {
                "quest_id": record["quest_id"],
                "title": record.get("title"),
                "completed": record["completed"],
                "completion_evidence": [
                    item["kind"] for item in record["completion_evidence"]
                ],
            }
            quests.append(quest)
            if record["completed"]:
                completions.append(quest)
        elif record["kind"] == "item_source":
            items.append(
                {
                    "item_id": record["item_id"],
                    "name": record.get("name"),
                    "how": record["how"],
                    "quest_ids": sorted(
                        {
                            source["quest_id"]
                            for source in record["sources"]
                            if source.get("quest_id") is not None
                        }
                    ),
                }
            )
        elif record["kind"] == "kill":
            kills.append(
                {
                    "name": record["unit"].get("name"),
                    "count": record["count"],
                }
            )
    summary = {
        "source": SOURCE,
        "schema": SCHEMA,
        "records_read": len(records),
        "record_kinds": dict(sorted(kinds.items())),
        "evidence_counts": dict(sorted(evidence_kinds.items())),
        "files": {
            "probe": len(file_info["probe_files"]),
            "boot": len(file_info["boot_files"]),
            "sessions": file_info["sessions"],
            "probe_versions": file_info["probe_versions"],
        },
        "quests": sorted(quests, key=lambda item: item["quest_id"]),
        "completions": sorted(
            completions, key=lambda item: item["quest_id"]
        ),
        "items": sorted(items, key=lambda item: item["item_id"]),
        "kills": sorted(kills, key=lambda item: (item["name"] or "", item["count"])),
    }
    if db_info is not None:
        summary["db"] = db_info
    return summary


def run_import(
    inputs: list[Path],
    output_path: Path,
    db_path: Path | None = None,
):
    records, file_info = load_observations(inputs)
    evidence_records = EvidenceBuilder(records).evidence()
    evidence = {
        "schema": SCHEMA,
        "schema_version": SCHEMA_VERSION,
        "source": SOURCE,
        "tool": "import_observations.py",
        "tool_version": TOOL_VERSION,
        "probe_versions": file_info["probe_versions"],
        "sessions": file_info["sessions"],
        "inputs": file_info["inputs"],
        "records": evidence_records,
    }
    db_info = None
    if db_path is not None:
        quest_ids = {
            record["quest_id"]
            for record in evidence_records
            if record["kind"] in ("quest", "reward")
            and record.get("quest_id") is not None
        }
        quest_ids.update(
            source["quest_id"]
            for record in evidence_records
            if record["kind"] == "item_source"
            for source in record["sources"]
            if source.get("quest_id") is not None
        )
        expectations = load_expectations(db_path, quest_ids)
        matches = annotate_evidence(evidence, expectations)
        db_info = {
            "name": db_path.name,
            "expected_data_version": "1.2",
            "match_counts": dict(sorted(matches.items())),
        }
    summary = build_summary(evidence, records, file_info, db_info)
    output = Path(output_path)
    if output.exists() and output.is_dir():
        raise EvidenceError(f"output is a directory: {output}")
    output.parent.mkdir(parents=True, exist_ok=True)
    try:
        with output.open("w", encoding="utf-8", newline="\n") as handle:
            json.dump(
                evidence,
                handle,
                ensure_ascii=False,
                indent=2,
                sort_keys=True,
                allow_nan=False,
            )
            handle.write("\n")
    except OSError as error:
        raise EvidenceError(f"cannot write {output}: {error}") from error
    return evidence, summary


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Import ArcheAge Classic probe recordings as deterministic "
            "evidence records."
        )
    )
    parser.add_argument(
        "--input",
        type=Path,
        action="append",
        required=True,
        help="observations directory or probe/boot Lua file (repeatable)",
    )
    parser.add_argument(
        "--output",
        type=Path,
        required=True,
        help="evidence JSON file to write",
    )
    parser.add_argument(
        "--db",
        type=Path,
        help="optional 1.2 compact.sqlite3, opened read-only, for annotations",
    )
    args = parser.parse_args(argv)
    try:
        _evidence, summary = run_import(args.input, args.output, args.db)
    except EvidenceError as error:
        print("error: " + str(error), file=sys.stderr)
        return 2
    print(
        json.dumps(
            summary,
            ensure_ascii=False,
            indent=2,
            sort_keys=True,
            allow_nan=False,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
