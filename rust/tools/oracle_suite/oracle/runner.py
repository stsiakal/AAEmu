"""Run one or more scenarios against both clients and write the comparison report."""

from __future__ import annotations

import json
import math
import os
import re
import secrets
import shutil
import string
import subprocess
import time
from dataclasses import dataclass, field
from datetime import datetime, time as dtime
from pathlib import Path
from typing import Any

from .checks import ClientEvidence, evaluate_all
from .clients import (BevyClient, ClientError, OriginalClient, bevy_script,
                      parse_bevy_lua_values)
from .report import index_report, make_sheet, scenario_report
from .stack import Lane, Stack, require_file
from .trace import Segment

STEP_ACTIONS = {"wait", "gm", "move", "key", "key_down", "key_up", "use_item", "mount",
                "bind_skill", "lua", "click", "drag", "build", "house_place"}
SCRIPT_RE = re.compile(r"SCRIPT \[\s*([\d.]+)s\] (?:line (\d+): |> )(.+)$")
TRACE_CLOCK_RE = re.compile(r"^(\d\d):(\d\d):(\d\d) ")


def load_scenario(path: Path) -> dict:
    scenario = json.loads(path.read_text(encoding="utf-8"))
    for key in ("name", "steps"):
        if key not in scenario:
            raise ValueError(f"{path}: missing {key!r}")
    names: set[str] = set()
    for step in scenario["steps"]:
        for key in ("name", "action"):
            if key not in step:
                raise ValueError(f"{path}: step without {key!r}: {step}")
        if step["action"] not in STEP_ACTIONS:
            raise ValueError(f"{path}: step {step['name']}: unknown action {step['action']!r}")
        if step["name"] in names:
            raise ValueError(f"{path}: duplicate step name {step['name']!r}")
        names.add(step["name"])
    return scenario


def scenario_paths(root: Path) -> list[Path]:
    return sorted((root / "scenarios").glob("*.json"))


def _today_epoch(hour: int, minute: int, second: int) -> float:
    now = datetime.now()
    return datetime.combine(now.date(), dtime(hour, minute, second)).timestamp()


def _first_packet_epoch(text: str, packet_name: str) -> float | None:
    for line in text.splitlines():
        match = TRACE_CLOCK_RE.match(line)
        if match and f" {packet_name} " in line:
            return _today_epoch(int(match.group(1)), int(match.group(2)), int(match.group(3)))
    return None


def _norm_script_command(command: str) -> str:
    command = command.strip()
    if command.startswith("shot "):
        return "screenshot " + command[5:]
    return command


def _bevy_step_intervals(log_text: str, script_lines: list[str],
                         line_steps: dict[int, str], steps: list[dict],
                         trace_text: str):
    """Cut the Bevy trace by the script's relative command log.

    Command lines count seconds from the script start.  Most are printed as
    ``SCRIPT [ 12.3s] > cmd``; only screenshots carry the source line.  Replay
    the commands in order against the generated script to recover each step.
    The first setup gm produces a CSSendChatMessagePacket; use it as the wall-clock
    anchor, so every step start is anchor + its relative time.
    """
    order = [step["name"] for step in steps]
    relative: dict[str, float] = {}
    script_index = 0
    for line in log_text.splitlines():
        match = SCRIPT_RE.search(line)
        if not match:
            continue
        seconds = float(match.group(1))
        source_line = int(match.group(2)) if match.group(2) else None
        command = match.group(3)
        if source_line is not None:
            index = source_line - 1
            script_index = index + 1
        else:
            while (script_index < len(script_lines)
                   and _norm_script_command(script_lines[script_index])
                   != _norm_script_command(command)):
                script_index += 1
            index = script_index
            script_index += 1
        step = line_steps.get(index + 1)
        if step is not None and step not in relative:
            relative[step] = seconds
    if not relative:
        return {}
    anchor_step = next((step["name"] for step in steps if step["action"] == "gm"), None)
    anchor_relative = relative.get(anchor_step) if anchor_step else None
    anchor_epoch = _first_packet_epoch(trace_text, "CSSendChatMessagePacket")
    if anchor_relative is None or anchor_epoch is None:
        return {}
    offset = anchor_epoch - anchor_relative
    intervals = {}
    present = [name for name in order if name in relative]
    for index, name in enumerate(present):
        start = offset + relative[name]
        end = (offset + relative[present[index + 1]]) if index + 1 < len(present) else start + 30_000
        intervals[name] = (math.floor(start), math.ceil(end))
    return intervals


def _segment_by_clock(text: str, start: float, end: float) -> Segment:
    lines = []
    for line in text.splitlines():
        match = TRACE_CLOCK_RE.match(line)
        if not match:
            continue
        stamp = _today_epoch(int(match.group(1)), int(match.group(2)), int(match.group(3)))
        if start <= stamp < end:
            lines.append(line)
    return Segment("\n".join(lines))


def _log_size(path: Path) -> int:
    return path.stat().st_size if path.is_file() else 0


def _read_range(path: Path, start: int, end: int) -> str:
    if not path.is_file():
        return ""
    with path.open("rb") as handle:
        handle.seek(start)
        return handle.read(max(0, end - start)).decode("utf-8", "replace")


def _run_name() -> str:
    return time.strftime("%Y%m%d-%H%M%S") + "-" + secrets.token_hex(2)


def _character_name(prefix: str) -> str:
    alphabet = string.ascii_lowercase
    return (prefix + "".join(secrets.choice(alphabet) for _ in range(6))).lower()


@dataclass
class SuiteOptions:
    lane: str
    scenarios: list[str]
    out: Path | None = None
    bevy_client: Path = Path("D:/aaemu-target/bevy/debug/aa-client.exe")
    original_client: Path = Path("D:/AA1.2/bin32/archeage.exe")
    build: str = "auto"          # auto | always | never
    keep_stack: bool = False
    restart_stack: bool = True
    bevy_size: str = "1280x720"


class Suite:
    def __init__(self, options: SuiteOptions):
        self.options = options
        self.lane = Lane.load(options.lane)
        self.out = (options.out or Path("D:/aaemu-oracle-runs") /
                    f"{options.lane}-{_run_name()}").resolve()
        self.stack = Stack(self.lane, log_sink=self.log)
        self.password_file = self.out / ".oracle-password.txt"
        self.password = secrets.token_urlsafe(18)
        self.run_id = _run_name()
        self.account_original = ("orco" + self.run_id.replace("-", "")[-16:])[:32]
        self.account_bevy = ("orcb" + self.run_id.replace("-", "")[-16:])[:32]
        self.character_original = _character_name("OrcO")
        self.character_bevy = _character_name("OrcB")
        self.notes: list[str] = []
        self.original: OriginalClient | None = None
        self.bevy: BevyClient | None = None

    def log(self, message: str) -> None:
        print(f"[oracle] {message}", flush=True)

    # -- setup ---------------------------------------------------------------------------------
    def prepare(self) -> None:
        self.out.mkdir(parents=True, exist_ok=True)
        self.password_file.write_text(self.password, encoding="ascii")
        self.log(f"lane {self.lane.name}, output {self.out}")
        launcher = self.lane.exe("aaemu-launcher.exe")
        driver = self.lane.exe("aaemu-clientdriver.exe")
        missing = [str(path) for path in (launcher, driver) if not path.is_file()]
        if missing and self.options.build == "never":
            raise RuntimeError("missing binaries: " + ", ".join(missing))
        if missing or self.options.build == "always":
            self.stack.build()
        require_file(launcher, "aaemu-launcher")
        require_file(driver, "aaemu-clientdriver")
        require_file(self.options.original_client, "1.2 client")
        require_file(self.options.bevy_client, "Bevy aa-client")
        self.stack.ensure_native_driver()
        driver_dll = self.lane.driver_dll()
        if driver_dll is None:
            raise RuntimeError("aadriver.dll not found")
        self.stack.start(restart=self.options.restart_stack)
        if self.stack.trace_enabled():
            self.notes.append("reused a running stack whose log already has field-trace lines")
        elif not self.options.restart_stack:
            self.log("field trace is not visible in the reused stack; restarting it")
            self.stack.start(restart=True)
            self.notes.append("restarted the reused stack to enable aaemu_game::net::fields=trace")
        self.original = OriginalClient(
            launcher=launcher,
            client=self.options.original_client,
            driver_exe=driver,
            driver_dll=driver_dll,
            lane=self.lane,
            password_file=self.password_file,
            account=self.account_original,
            run_dir=self.out / "original",
            log_sink=self.log,
        )
        self.bevy = BevyClient(
            exe=self.options.bevy_client,
            lane=self.lane,
            password_file=self.password_file,
            account=self.account_bevy,
            character=self.character_bevy,
            run_dir=self.out / "bevy",
            log_sink=self.log,
            size=self.options.bevy_size,
        )
        self.log("creating the Bevy character")
        self.bevy.create_character(self.character_bevy)
        self.log("creating the original character")
        self.original.launch()
        self.original.ensure_character(self.character_original)
        self.grant_gm(self.character_original)
        self.grant_gm(self.character_bevy)
        self.original.stop()

    def grant_gm(self, character: str) -> None:
        """Give the throwaway lane character access level 100 (lane database only)."""
        from .stack import mysql_exe
        command = [str(mysql_exe()), "--host=127.0.0.1", "--port=3317", "--user=root",
                   self.lane.db_game, "-e",
                   f"UPDATE characters SET access_level=100 WHERE LOWER(name)=LOWER('{character}')"]
        result = subprocess.run(command, text=True, encoding="utf-8", errors="replace",
                                capture_output=True, timeout=30)
        if result.returncode != 0:
            raise RuntimeError(f"granting GM to {character} failed: {result.stderr[-500:]}")

    # -- one scenario --------------------------------------------------------------------------
    def run_scenario(self, scenario: dict) -> tuple[Path, list]:
        directory = self.out / scenario["name"]
        if directory.exists():
            shutil.rmtree(directory)
        directory.mkdir(parents=True, exist_ok=True)
        shots = set(scenario.get("screenshots", []))
        for step in scenario.get("setup", []) + scenario["steps"]:
            if step["action"] == "move":
                self.position_character(self.character_original, step)
                self.position_character(self.character_bevy, step)
                break
        self.log(f"scenario {scenario['name']}: original")
        assert self.original is not None
        self.original.launch()
        self.original.ensure_character(self.character_original)
        self.original.enter_world()
        try:
            original_evidence = self._run_original(scenario, directory, shots)
        finally:
            self.original.stop()
        self.log(f"scenario {scenario['name']}: Bevy")
        bevy_evidence = self._run_bevy(scenario, directory, shots)
        results = evaluate_all(scenario.get("checks", []), original_evidence, bevy_evidence)

        sheet_paths = []
        for name in sorted(shots):
            left = directory / "original" / "shots" / f"{name}.png"
            right = directory / "bevy" / "shots" / f"{name}.png"
            target = directory / "sheets" / f"{name}.png"
            if make_sheet(left, right, target, name):
                sheet_paths.append(target)
        report_path = directory / "report.md"
        scenario_report(report_path,
                        scenario,
                        f"{self.account_original} / {self.character_original}",
                        f"{self.account_bevy} / {self.character_bevy}",
                        results, sheet_paths,
                        [
                            "original: hidden 1.2 client through aaemu-clientdriver "
                            "(no desktop input)",
                            "Bevy: aa-client --offscreen-shot --mute (no window)",
                            "packet fields from RUST_LOG=info,aaemu_game::net::fields=trace",
                            "characters are positioned in the lane DB before login; far "
                            "in-game teleports crash the offscreen Bevy renderer",
                        ] + list(scenario.get("notes", [])))
        return report_path, results

    def _run_original(self, scenario: dict, directory: Path, shots: set[str]) -> ClientEvidence:
        assert self.original is not None
        evidence = ClientEvidence()
        run_start = _log_size(self.lane.game_log)
        for step in scenario.get("setup", []) + scenario["steps"]:
            start = _log_size(self.lane.game_log)
            try:
                value = self._original_step(step)
            except ClientError as error:
                evidence.segments[step["name"]] = Segment("")
                raise ClientError(f"original step {step['name']}: {error}") from error
            time.sleep(float(step.get("wait", 0.5)))
            segment_end = _log_size(self.lane.game_log)
            segment = Segment.read(self.lane.game_log, start, segment_end)
            evidence.segments[step["name"]] = segment
            if step["action"] == "lua" and value is not None:
                evidence.lua[step["name"]] = str(value).strip()
            if step["name"] in shots:
                path = directory / "original" / "shots" / f"{step['name']}.png"
                self.original.screenshot(path)
                evidence.shots[step["name"]] = str(path)
        run_end = _log_size(self.lane.game_log)
        (directory / "original").mkdir(parents=True, exist_ok=True)
        (directory / "original" / "trace.log").write_text(
            _read_range(self.lane.game_log, run_start, run_end), encoding="utf-8")
        return evidence

    def _original_step(self, step: dict) -> Any:
        assert self.original is not None
        client = self.original
        action = step["action"]
        if action == "wait":
            time.sleep(float(step.get("seconds", step.get("wait", 1.0))))
            return None
        if action == "gm":
            return client.tool("client_chat", message="/" + step["command"])
        if action == "move":
            return client.tool("client_chat", message=f"/move {step['x']} {step['y']} {step['z']}")
        if action == "key":
            return client.tool("client_input", key=step["key"],
                               duration_ms=int(step.get("duration_ms", 100)),
                               **({"modifiers": step["modifiers"]} if step.get("modifiers") else {}))
        if action == "key_down":
            return client.tool("client_input", key=step["key"],
                               duration_ms=int(step.get("hold_ms", 7000)))
        if action == "key_up":
            return client.tool("client_input", key="release")
        if action == "use_item":
            return client.tool("client_use_item", item_template_id=int(step["item"]))
        if action == "house_place":
            return client.tool("client_use_item", item_template_id=int(step["item"]))
        if action == "mount":
            return client.lua("X2Mate:MountMate()")
        if action == "bind_skill":
            slot = (int(step.get("page", 1)) - 1) * 48 + int(step["cell"])
            return client.lua(f"X2Action:SetActionSpell({slot},{int(step['skill'])})")
        if action == "lua":
            return client.lua(step["code"])
        if action == "click":
            point = step.get("point", [0.5, 0.5])
            return client.click_fraction(point[0], point[1], step.get("button", "left"))
        if action == "drag":
            return client.drag_fraction(step["from"], step["to"])
        if action == "build":
            point = step.get("point", [0.5, 0.5])
            return client.click_fraction(point[0], point[1], "left")
        raise ClientError(f"unknown action {action}")

    def position_character(self, character: str, step: dict) -> None:
        """Move a character row before login.

        A far in-game teleport after world entry trips a renderer validation
        error in the offscreen Bevy client, and the server spawns a summoned
        mate at the character row position; spawning at the scenario's target
        avoids both.  The scenario keeps its abstract move step (the original
        also runs its GM /move).
        """
        from .stack import mysql_exe
        command = [str(mysql_exe()), "--host=127.0.0.1", "--port=3317", "--user=root",
                   self.lane.db_game, "-e",
                   f"UPDATE characters SET x={float(step['x'])}, y={float(step['y'])}, "
                   f"z={float(step['z'])} WHERE LOWER(name)=LOWER('{character}')"]
        result = subprocess.run(command, text=True, encoding="utf-8", errors="replace",
                                capture_output=True, timeout=30)
        if result.returncode != 0:
            raise RuntimeError(f"positioning {character} failed: {result.stderr[-500:]}")

    def _run_bevy(self, scenario: dict, directory: Path, shots: set[str]) -> ClientEvidence:
        assert self.bevy is not None
        evidence = ClientEvidence()
        steps = []
        for index, step in enumerate(scenario.get("setup", []) + scenario["steps"]):
            copy = dict(step)
            copy["_marker"] = f"__ORACLE_{index}"
            steps.append(copy)
        bevy_steps = scenario.get("setup", []) + scenario["steps"]
        (directory / "bevy" / "shots").mkdir(parents=True, exist_ok=True)
        lines, line_steps = bevy_script(steps, shots, directory / "bevy" / "shots",
                                        self.bevy.screen_size, move_as_wait=True)
        script_path = directory / "bevy" / "script.txt"
        script_path.parent.mkdir(parents=True, exist_ok=True)
        script_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
        start = _log_size(self.lane.game_log)
        run = self.bevy.run_script(script_path, directory / "bevy" / "client.log",
                                   directory / "bevy" / "boot.png")
        end = _log_size(self.lane.game_log)
        trace_text = _read_range(self.lane.game_log, start, end)
        (directory / "bevy" / "trace.log").write_text(trace_text, encoding="utf-8")

        intervals = _bevy_step_intervals(run.log, lines, line_steps, bevy_steps, trace_text)
        if intervals:
            for step in bevy_steps:
                if step["name"] in intervals:
                    evidence.segments[step["name"]] = _segment_by_clock(trace_text, *intervals[step["name"]])
        else:
            self.notes.append(f"{scenario['name']}: Bevy script log timestamps missing; "
                              "each step sees the whole Bevy trace")
            for step in bevy_steps:
                evidence.segments[step["name"]] = Segment(trace_text)

        index_to_step = {index: step["name"] for index, step in enumerate(steps)
                         if step["action"] == "lua"}
        for value_index, value in parse_bevy_lua_values(run.log).items():
            step = index_to_step.get(value_index)
            if step is not None:
                evidence.lua[step] = value
        for name in shots:
            path = directory / "bevy" / "shots" / f"{name}.png"
            if path.is_file():
                evidence.shots[name] = str(path)
        if run.returncode != 0:
            tail = "\n".join(run.log.splitlines()[-15:])
            self.notes.append(f"{scenario['name']}: Bevy exited {run.returncode}:\n{tail}")
        return evidence

    # -- entry ---------------------------------------------------------------------------------
    def run(self) -> int:
        paths = scenario_paths(Path(__file__).resolve().parent.parent)
        selected = [path for path in paths
                    if not self.options.scenarios or path.stem in self.options.scenarios]
        if not selected:
            raise RuntimeError("no scenarios selected")
        summaries = []
        try:
            self.prepare()
            for path in selected:
                scenario = load_scenario(path)
                report, results = self.run_scenario(scenario)
                summaries.append((scenario["name"], report, results))
                counts = {status: sum(1 for result in results if result.status == status)
                          for status in ("SAME", "DIFFERENT", "NOT RUN")}
                self.log(f"{scenario['name']}: SAME {counts['SAME']}, "
                         f"DIFFERENT {counts['DIFFERENT']}, NOT RUN {counts['NOT RUN']}")
        finally:
            if self.original is not None:
                self.original.stop()
            if not self.options.keep_stack:
                self.stack.stop()
            try:
                self.password_file.unlink()
            except OSError:
                pass
        if summaries:
            index_report(self.out / "REPORT.md", summaries, self.notes)
            (self.out / "run.json").write_text(json.dumps({
                "lane": self.lane.name,
                "accounts": {"original": self.account_original, "bevy": self.account_bevy},
                "characters": {"original": self.character_original, "bevy": self.character_bevy},
                "scenarios": [{"name": name, "report": str(report),
                               "checks": [result.to_dict() for result in results]}
                              for name, report, results in summaries],
                "notes": self.notes,
            }, indent=2), encoding="utf-8")
            self.log(f"report: {self.out / 'REPORT.md'}")
        return 0








