"""The two clients: the hidden 1.2 original through aaemu-clientdriver, and Bevy aa-client."""

from __future__ import annotations

import base64
import json
import os
import re
import subprocess
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")


class ClientError(RuntimeError):
    pass


class Rpc:
    """Minimal newline-delimited JSON-RPC client for `aaemu-clientdriver mcp`."""

    def __init__(self, command: list[str], cwd: Path | None = None):
        self.process = subprocess.Popen(
            command,
            cwd=str(cwd) if cwd else None,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            encoding="utf-8",
        )
        self.sequence = 0
        self.call("initialize", {"protocolVersion": "2025-11-25", "capabilities": {},
                                 "clientInfo": {"name": "aaemu-oracle-suite", "version": "1"}})
        assert self.process.stdin and self.process.stdout
        self.process.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
        self.process.stdin.flush()

    def call(self, method: str, params: Any = None) -> Any:
        assert self.process.stdin and self.process.stdout
        self.sequence += 1
        request: dict[str, Any] = {"jsonrpc": "2.0", "id": self.sequence, "method": method}
        if params is not None:
            request["params"] = params
        self.process.stdin.write(json.dumps(request) + "\n")
        self.process.stdin.flush()
        line = self.process.stdout.readline()
        if not line:
            raise ClientError("MCP server closed stdout")
        response = json.loads(line)
        if response.get("id") != self.sequence:
            raise ClientError("MCP response id mismatch")
        if "error" in response:
            raise ClientError(str(response["error"]))
        return response["result"]

    def tool(self, name: str, **arguments: Any) -> dict:
        result = self.call("tools/call", {"name": name, "arguments": arguments})
        if result.get("isError"):
            raise ClientError("\n".join(item.get("text", "") for item in result.get("content", [])))
        return result

    def text(self, name: str, **arguments: Any) -> str:
        return "\n".join(item["text"] for item in self.tool(name, **arguments)["content"]
                         if item["type"] == "text")

    def close(self) -> None:
        try:
            if self.process.stdin:
                self.process.stdin.close()
        except OSError:
            pass
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.kill()


@dataclass
class StepResult:
    original: Any = None
    bevy: Any = None
    original_note: str | None = None
    bevy_note: str | None = None
    skipped: bool = False


@dataclass
class ClientRun:
    trace_start: int = 0
    trace_end: int = 0
    lua: dict[str, Any] = field(default_factory=dict)
    errors: list[str] = field(default_factory=list)
    log: str = ""
    returncode: int | None = None


class OriginalClient:
    def __init__(self, *, launcher: Path, client: Path, driver_exe: Path, driver_dll: Path,
                 lane, password_file: Path, account: str, run_dir: Path, log_sink):
        self.launcher = launcher
        self.client = client
        self.driver_exe = driver_exe
        self.driver_dll = driver_dll
        self.lane = lane
        self.password_file = password_file
        self.account = account
        self.run_dir = run_dir
        self.log_sink = log_sink
        self.pid: int | None = None
        self.rpc: Rpc | None = None
        self.character = ""
        self.screen_size = (2560, 1440)

    def launch(self) -> None:
        if self.pid is not None:
            return
        self.run_dir.mkdir(parents=True, exist_ok=True)
        command = [
            str(self.launcher),
            "--client", str(self.client),
            "--login", f"127.0.0.1:{self.lane.login_port}",
            "--user", self.account,
            "--password-file", str(self.password_file),
            "--hidden",
            "--nosplash",
            "--driver-dll", str(self.driver_dll),
            "--multi-client",
            "--arg", "+r_fullscreen", "--arg", "0",
        ]
        self.log_sink(f"launching hidden original client for {self.account}")
        result = subprocess.run(command, cwd=str(self.lane.worktree), text=True,
                                encoding="utf-8", errors="replace", capture_output=True, timeout=180)
        output = (result.stdout or "") + (result.stderr or "")
        (self.run_dir / "original-launcher.log").write_text(output, encoding="utf-8")
        match = re.search(r"client started \(pid (\d+)\)", output)
        if not match:
            raise ClientError(f"original launcher did not report a client pid:\n{output[-2000:]}")
        self.pid = int(match.group(1))
        self.rpc = Rpc([str(self.driver_exe), "mcp", "--dll", str(self.driver_dll),
                        "--artifact-dir", str(self.run_dir / "mcp")], cwd=self.lane.worktree)
        self._wait_driver()

    def _wait_driver(self, timeout: float = 90) -> None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                clients = json.loads(self.rpc.text("client_list"))
            except (ClientError, json.JSONDecodeError):
                clients = []
            for client in clients:
                if client.get("pid") == self.pid and client.get("driver_loaded"):
                    return
            time.sleep(0.5)
        raise ClientError(f"driver did not load into pid {self.pid}")

    def lua(self, code: str, target: str = "ui") -> str:
        return self.rpc.text("client_lua", pid=self.pid, target=target, code=code)

    def tool(self, name: str, **arguments: Any) -> str:
        arguments.setdefault("pid", self.pid)
        result = self.rpc.tool(name, **arguments)
        structured = result.get("structuredContent")
        if structured is not None:
            return structured if isinstance(structured, str) else json.dumps(structured)
        return "\n".join(item.get("text", "") for item in result.get("content", [])
                         if item["type"] == "text")

    def wait_stage(self, timeout: float = 120) -> None:
        deadline = time.monotonic() + timeout
        last = ""
        while time.monotonic() < deadline:
            try:
                last = self.lua("return X2LoginCharacter and X2LoginCharacter:GetCurrentStage()")
                if last.strip().startswith("3"):
                    return
            except ClientError as error:
                last = str(error)
            time.sleep(0.5)
        raise ClientError(f"login stage 3 not reached: {last}")

    def connect_server(self, timeout: float = 60) -> None:
        """Stage 3 is the server list; press Connect and wait for character select."""
        self.wait_stage()
        self.lua("backgroundWindow.mainWindow:SelectByDataKey(1) "
                 "backgroundWindow.enterWorldBtn:OnClick()")
        deadline = time.monotonic() + timeout
        last = ""
        while time.monotonic() < deadline:
            try:
                last = self.lua("return X2LoginCharacter:GetCurrentStage(), "
                                "X2LoginCharacter:GetNumLoginCharacters()")
                if last and not last.startswith("3"):
                    return
            except ClientError as error:
                last = str(error)
            time.sleep(0.5)
        raise ClientError(f"character select not reached: {last}")

    def ensure_character(self, name: str, timeout: float = 90) -> None:
        self.connect_server()
        count = 0
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            try:
                count = int(float(self.lua("return X2LoginCharacter:GetNumLoginCharacters()") or "0"))
                break
            except (ClientError, ValueError):
                time.sleep(0.5)
        if count > 0:
            self.character = name
            return
        self.log_sink(f"creating original character {name}")
        self.lua(f"X2LoginCharacter:NewCharacterCBT2('{name}', 'nuian', 'male', 1, 0, 0, 1)")
        time.sleep(3)
        self.lua("characterCreate:OnNextStep()")
        time.sleep(1.5)
        self.lua("characterCreate:OnNextStep()")
        time.sleep(1.5)
        self.lua("characterCreate.step3.content.nameWnd.characterName:SetText("
                 f"'{name}')")
        time.sleep(1.5)
        self.lua("SelectAbility(1)")
        time.sleep(1)
        self.lua("characterCreate:OnCompleteCurrentStep()")
        for _ in range(30):
            try:
                if self.lua("return X2LoginCharacter:GetCurrentStage()") == "7":
                    break
            except ClientError:
                pass
            time.sleep(0.5)
        self.lua("CreateCharacter(false)")
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                count = int(float(self.lua("return X2LoginCharacter:GetNumLoginCharacters()") or "0"))
                if count > 0:
                    self.character = name
                    return
            except (ClientError, ValueError):
                pass
            time.sleep(2)
        raise ClientError("original character was not created")

    def enter_world(self, timeout: float = 180) -> None:
        deadline = time.monotonic() + timeout
        started = False
        last = ""
        while time.monotonic() < deadline:
            try:
                state = self.lua("return X2LoginCharacter and X2LoginCharacter:GetCurrentStage(), "
                                 "X2Unit and X2Unit:UnitName('player')")
                stage, _, name = state.partition("\t")
                last = state
                if stage.strip() == "0" and name.strip() not in ("", "nil", "false"):
                    self.character = name.strip()
                    self.remember_screen_size()
                    return
                if not started:
                    self.lua("characterSelectWindow.bottomPanel.startButton:"
                             "OnClick('LeftButton')")
                    started = True
            except ClientError as error:
                last = str(error)
            time.sleep(0.5)
        raise ClientError(f"original did not enter the world: {last}")

    def remember_screen_size(self) -> None:
        try:
            png = self.screenshot_bytes()
            image = __import__("PIL.Image", fromlist=["Image"])
            import io
            self.screen_size = image.open(io.BytesIO(png)).size
        except Exception:
            pass

    def screenshot_bytes(self) -> bytes:
        content = self.rpc.tool("client_screenshot", pid=self.pid)["content"]
        image = next(item for item in content if item["type"] == "image")
        return base64.b64decode(image["data"])

    def screenshot(self, path: Path) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(self.screenshot_bytes())

    def click_fraction(self, fx: float, fy: float, button: str = "left") -> str:
        x = max(0, min(4095, round(fx * self.screen_size[0])))
        y = max(0, min(4095, round(fy * self.screen_size[1])))
        action = "click" if button == "left" else "right_click"
        return self.tool("client_pointer", action=action, x=x, y=y)

    def drag_fraction(self, start: list[float], end: list[float]) -> str:
        x1 = round(start[0] * self.screen_size[0])
        y1 = round(start[1] * self.screen_size[1])
        x2 = round(end[0] * self.screen_size[0])
        y2 = round(end[1] * self.screen_size[1])
        return self.tool("client_drag", x1=x1, y1=y1, x2=x2, y2=y2)

    def stop(self) -> None:
        if self.rpc is not None:
            try:
                self.rpc.tool("client_window", pid=self.pid, visible=False)
            except ClientError:
                pass
            try:
                self.rpc.close()
            except Exception:
                pass
            self.rpc = None
        if self.pid is not None:
            subprocess.run(["taskkill", "/PID", str(self.pid), "/F"], capture_output=True, timeout=30)
            time.sleep(0.5)
            try:
                import os
                import signal
                os.kill(self.pid, signal.SIGTERM)
            except OSError:
                pass
            self.pid = None


class BevyClient:
    def __init__(self, *, exe: Path, lane, password_file: Path, account: str,
                 character: str, run_dir: Path, log_sink, size: str = "1280x720"):
        self.exe = exe
        self.lane = lane
        self.password_file = password_file
        self.account = account
        self.character = character
        self.run_dir = run_dir
        self.log_sink = log_sink
        self.size = size
        self.screen_size = tuple(int(part) for part in size.split("x"))

    def _base(self) -> list[str]:
        return [
            str(self.exe),
            "--mute",
            "--size", self.size,
            "--account", self.account,
            "--login", f"127.0.0.1:{self.lane.login_port}",
            "--password-file", str(self.password_file),
            "--allow-lua-error", "tab_language",
        ]

    def create_character(self, name: str) -> None:
        self.run_dir.mkdir(parents=True, exist_ok=True)
        log_path = self.run_dir / "bevy-create.log"
        command = self._base() + [
            "--goto", "create",
            "--clua", f"CC.create('{name}', 1)", "9",
            "--exit-after-create",
        ]
        self.log_sink(f"creating Bevy character {name}")
        with log_path.open("w", encoding="utf-8") as log:
            result = subprocess.run(command, cwd=str(self.exe.parent), stdout=log,
                                    stderr=subprocess.STDOUT, text=True, timeout=300)
        text = ANSI_RE.sub("", log_path.read_text(encoding="utf-8", errors="replace"))
        if result.returncode != 0 and "the server created" not in text:
            if "duplicate" not in text.lower() and "reason 4" not in text:
                raise ClientError(f"Bevy character creation failed (exit {result.returncode})")

    def run_script(self, script_path: Path, trace_path: Path, shot_boot: Path) -> ClientRun:
        self.run_dir.mkdir(parents=True, exist_ok=True)
        log_path = self.run_dir / "bevy.log"
        command = self._base() + [
            "--character", self.character,
            "--script", str(script_path),
            "--offscreen-shot", str(shot_boot),
            "--shot-after", "3",
        ]
        self.log_sink(f"running Bevy script {script_path.name}")
        with log_path.open("w", encoding="utf-8") as log:
            try:
                result = subprocess.run(command, cwd=str(self.exe.parent), stdout=log,
                                        stderr=subprocess.STDOUT, text=True, timeout=900)
                code = result.returncode
            except subprocess.TimeoutExpired:
                log.write("\nORACLE: bevy client timed out\n")
                code = 124
        run = ClientRun(returncode=code)
        run.log = ANSI_RE.sub("", log_path.read_text(encoding="utf-8", errors="replace"))
        trace_path.write_text(run.log, encoding="utf-8")
        return run


def bevy_script(steps, shot_after: set[str], shots_dir: Path,
                screen_size, move_as_wait: bool = False) -> tuple[list[str], dict[int, str]]:
    """Translate abstract steps to aa-client's script language.

    Returns the lines and a map from 1-based source line number to step name, used
    to cut the server trace by the script's own command log.
    """
    lines = ["wait 1"]
    line_steps: dict[int, str] = {}

    def add(line: str, step_name: str | None = None) -> None:
        lines.append(line)
        if step_name is not None:
            line_steps[len(lines)] = step_name

    for step in steps:
        name = step["name"]
        action = step["action"]
        wait = float(step.get("wait", 0.5))
        if action == "wait":
            add(f"wait {float(step.get('seconds', wait)):g}", name)
            wait = 0.0
        elif action == "gm":
            add(f"gm {step['command']}", name)
        elif action == "move":
            if move_as_wait:
                add("wait 0.5", name)
            else:
                add(f"tp {step['x']} {step['y']} {step['z']}", name)
        elif action == "key":
            key = step["key"]
            modifiers = step.get("modifiers", [])
            for modifier in modifiers:
                add(f"keydown {modifier}", name)
            add(f"key {key}", name)
            for modifier in reversed(modifiers):
                add(f"keyup {modifier}", name)
        elif action == "key_down":
            add(f"keydown {step['key']}", name)
        elif action == "key_up":
            add(f"keyup {step['key']}", name)
        elif action == "use_item":
            add(f"use {step['item']}", name)
        elif action == "house_place":
            add(f"house_place {step['item']} {step.get('dx', 0):g} {step.get('dy', 8):g}", name)
        elif action == "mount":
            add("lua X2Mate:MountMate()", name)
        elif action == "bind_skill":
            add(f"lua HUD.drag_action_set({int(step['cell'])},'skill',{int(step['skill'])})", name)
        elif action == "lua":
            marker = step.get("_marker", "ORACLE")
            code = step["code"].strip()
            if code.startswith("return "):
                body = code[len("return "):]
                add(f'lua print("{marker}\\t"..tostring((function() return {body} end)()))', name)
            else:
                add(f"lua {code}", name)
        elif action == "click":
            point = step.get("point")
            if point:
                add(f"cursor {point[0]:g} {point[1]:g}", name)
            add(f"click {step.get('button', 'left')}", name)
        elif action == "drag":
            raise ClientError("Bevy aa-client has no scripted UI drag; the drag step is not drivable")
        elif action == "build":
            add("house_build", name)
        else:
            raise ClientError(f"unknown step action {action!r}")
        if wait:
            add(f"wait {wait:g}", name)
        if name in shot_after:
            add(f"shot {(shots_dir / (name + '.png')).as_posix()}", name)
    add("quit", steps[-1]["name"] if steps else None)
    return lines, line_steps


def parse_bevy_lua_values(text: str) -> dict[int, str]:
    values: dict[int, str] = {}
    for line in text.splitlines():
        match = re.search(r"__ORACLE_(\d+)\t(.*)$", ANSI_RE.sub("", line))
        if match:
            values[int(match.group(1))] = match.group(2).strip()
    return values


