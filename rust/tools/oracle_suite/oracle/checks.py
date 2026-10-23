"""Scenario checks: compare the two clients' observations and print evidence."""

from __future__ import annotations

import math
from dataclasses import dataclass, field

from .trace import (Segment, instantaneous_speeds, median, path_length, percentile,
                    primary_object)


@dataclass
class CheckResult:
    name: str
    status: str
    original: str = ""
    bevy: str = ""
    detail: str = ""

    def to_dict(self) -> dict:
        return {"name": self.name, "status": self.status, "original": self.original,
                "bevy": self.bevy, "detail": self.detail}


@dataclass
class ClientEvidence:
    segments: dict[str, Segment] = field(default_factory=dict)
    lua: dict[str, str] = field(default_factory=dict)
    shots: dict[str, str] = field(default_factory=dict)


def _same(expected: str, actual: str) -> bool:
    return expected.strip() == actual.strip()


def _numbers_equal(a: float, b: float, rel: float, absolute: float) -> bool:
    return abs(a - b) <= max(absolute, rel * max(abs(a), abs(b)))


def _packet_fields(packet, field: str):
    if field in packet.fields:
        return packet.fields[field]
    return None


def evaluate_packet(check: dict, original: ClientEvidence, bevy: ClientEvidence) -> CheckResult:
    name = check["name"]
    packet_name = check["packet"]
    steps = check.get("steps") or ([check["step"]] if "step" in check else [])
    fields = check.get("fields", [])
    keys = list(fields)
    expected = check.get("expect")
    observed: dict[str, list[dict]] = {}
    missing: list[str] = []
    for side, evidence in (("original", original), ("bevy", bevy)):
        rows: list[dict] = []
        for step in steps:
            segment = evidence.segments.get(step)
            if segment is None:
                missing.append(f"{side}:{step}")
                continue
            for packet in segment.named(packet_name):
                row = {field: _packet_fields(packet, field) for field in keys}
                rows.append(row)
        observed[side] = rows
    original_text = _packet_text(observed["original"], keys)
    bevy_text = _packet_text(observed["bevy"], keys)
    if not observed["original"] or not observed["bevy"]:
        return CheckResult(name, "NOT RUN", original_text, bevy_text,
                           "missing trace for " + ", ".join(missing) if missing else
                           "no packet on one side")
    original_ok = _matches(observed["original"], expected)
    bevy_ok = _matches(observed["bevy"], expected)
    if expected is not None:
        status = "SAME" if original_ok and bevy_ok else "DIFFERENT"
        detail = "" if status == "SAME" else "values do not match the scenario expectation"
    else:
        status = "SAME" if _rows_equal(observed["original"], observed["bevy"], keys) else "DIFFERENT"
        detail = ""
    return CheckResult(name, status, original_text, bevy_text, detail)


def _packet_text(rows: list[dict], fields: dict) -> str:
    if not rows:
        return "-"
    if not fields:
        return f"{len(rows)} packet(s)"
    keys = list(fields)
    return "; ".join(", ".join(f"{key}={row.get(key)}" for key in keys) for row in rows)


def _matches(rows: list[dict], expected) -> bool:
    if expected is None:
        return True
    count = expected.get("count")
    if count is not None and len(rows) != count:
        return False
    for key, wanted in expected.items():
        if key == "count":
            continue
        actual = [row.get(key) for row in rows]
        if actual != wanted:
            return False
    return True


def _rows_equal(a: list[dict], b: list[dict], fields: dict) -> bool:
    keys = list(fields)
    return [[row.get(key) for key in keys] for row in a] == [[row.get(key) for key in keys] for row in b]


def evaluate_lua(check: dict, original: ClientEvidence, bevy: ClientEvidence) -> CheckResult:
    steps = check["steps"]
    left = [original.lua.get(step) for step in steps]
    right = [bevy.lua.get(step) for step in steps]
    left_text = " -> ".join(value if value is not None else "-" for value in left)
    right_text = " -> ".join(value if value is not None else "-" for value in right)
    if any(value is None for value in left) or any(value is None for value in right):
        return CheckResult(check["name"], "NOT RUN", left_text, right_text, "missing Lua value")
    expected = check.get("expect")
    if expected is not None:
        expected = [str(value) for value in expected]
        status = "SAME" if left == expected and right == expected else "DIFFERENT"
        detail = "" if status == "SAME" else f"expected {expected}"
    else:
        status = "SAME" if left == right else "DIFFERENT"
        detail = ""
    return CheckResult(check["name"], status, left_text, right_text, detail)


MOUNT_METRICS = {
    "run_speed": {"rel": 0.10, "abs": 0.15},
    "accel_time": {"rel": 0.35, "abs": 0.12},
    "decel_time": {"rel": 0.35, "abs": 0.12},
    "turn_rate": {"rel": 0.20, "abs": 1.0},
    "back_speed": {"rel": 0.15, "abs": 0.15},
    "jump_height": {"rel": 0.20, "abs": 0.12},
    "velocity_ratio": {"rel": 0.15, "abs": 0.08},
}


def _mount_values(evidence: ClientEvidence, steps: dict[str, str]) -> dict[str, float | None]:
    def rows(step: str):
        segment = evidence.segments.get(step)
        return primary_object(segment.moves()) if segment else []

    run = rows(steps["run"]) if steps.get("run") else []
    decel = rows(steps["decel"]) if steps.get("decel") else []
    turn = rows(steps["turn"]) if steps.get("turn") else []
    back = rows(steps["back"]) if steps.get("back") else []
    jump = rows(steps["jump"]) if steps.get("jump") else []

    speeds = instantaneous_speeds(run)
    steady = percentile(speeds, 0.65) if speeds else 0.0
    accel = None
    if run:
        start = run[0].time_ms
        for index, speed in enumerate(speeds):
            if steady > 0 and speed >= 0.9 * steady:
                accel = (run[index + 1].time_ms - start) / 1000.0
                break
    decel_time = None
    if run and decel:
        last_fast = max((row for row in run if math.hypot(row.vx, row.vy) >= 0.5 * steady),
                        key=lambda row: row.time_ms, default=run[-1])
        slow = next((row for row in decel if math.hypot(row.vx, row.vy) < 0.5
                     or math.hypot(row.vx, row.vy) < 0.1 * steady), None)
        if slow is not None:
            decel_time = max(0.0, (slow.time_ms - last_fast.time_ms) / 1000.0)
    turn_rate = None
    if len(turn) >= 2:
        delta = _yaw_delta(turn[0].yaw, turn[-1].yaw)
        seconds = (turn[-1].time_ms - turn[0].time_ms) / 1000.0
        turn_rate = abs(delta) / seconds if seconds else None
    back_speed = path_length(back) / max(0.001, (back[-1].time_ms - back[0].time_ms) / 1000.0) if len(back) >= 2 else None
    jump_height = None
    if len(jump) >= 3:
        base = median([row.z for row in jump[:3]])
        jump_height = max(row.z for row in jump) - base
    ratio = None
    if len(run) >= 3:
        ratios = []
        for a, b in zip(run, run[1:]):
            dt = (b.time_ms - a.time_ms) / 1000.0
            if dt <= 0:
                continue
            real = math.hypot(b.x - a.x, b.y - a.y) / dt
            if real > 0.2:
                ratios.append((a.speed_from_packet / 1024.0) / real)
        ratio = median(ratios) if ratios else None
    return {
        "run_speed": steady,
        "accel_time": accel,
        "decel_time": decel_time,
        "turn_rate": turn_rate,
        "back_speed": back_speed,
        "jump_height": jump_height,
        "velocity_ratio": ratio,
    }


def _yaw_delta(a: float, b: float) -> float:
    delta = (b - a + 180.0) % 360.0 - 180.0
    return delta


def evaluate_mount(check: dict, original: ClientEvidence, bevy: ClientEvidence) -> list[CheckResult]:
    steps = check["steps"]
    left = _mount_values(original, steps)
    right = _mount_values(bevy, steps)
    results: list[CheckResult] = []
    for metric, tolerance in MOUNT_METRICS.items():
        label = metric.replace("_", " ")
        a, b = left.get(metric), right.get(metric)
        left_text = "-" if a is None else f"{a:.3f}"
        right_text = "-" if b is None else f"{b:.3f}"
        if a is None or b is None:
            results.append(CheckResult(f"{check['name']}: {label}", "NOT RUN", left_text,
                                       right_text, "metric missing"))
        elif _numbers_equal(a, b, tolerance["rel"], tolerance["abs"]):
            results.append(CheckResult(f"{check['name']}: {label}", "SAME", left_text,
                                       right_text))
        else:
            results.append(CheckResult(f"{check['name']}: {label}", "DIFFERENT", left_text,
                                       right_text,
                                       f"tolerance {tolerance['rel']:.0%} / {tolerance['abs']:g}"))
    return results


def evaluate_all(checks: list[dict], original: ClientEvidence, bevy: ClientEvidence) -> list[CheckResult]:
    results: list[CheckResult] = []
    for check in checks:
        kind = check.get("type", "packet")
        if kind == "packet":
            results.append(evaluate_packet(check, original, bevy))
        elif kind == "lua":
            results.append(evaluate_lua(check, original, bevy))
        elif kind == "mount_metrics":
            results.extend(evaluate_mount(check, original, bevy))
        elif kind == "shot":
            left = original.shots.get(check["name"])
            right = bevy.shots.get(check["name"])
            if left and right:
                results.append(CheckResult(check["name"], "SAME", left, right, "screenshots captured"))
            else:
                results.append(CheckResult(check["name"], "NOT RUN",
                                           left or "-", right or "-", "screenshot missing"))
        else:
            raise ValueError(f"unknown check type {kind!r}")
    return results




