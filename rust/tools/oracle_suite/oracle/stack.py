"""Lane stack discovery, build/start helpers and binary lookup."""

from __future__ import annotations

import os
import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path


def find_bash() -> str:
    candidates = [
        os.environ.get("AAEMU_BASH"),
        shutil.which("bash"),
        r"C:\Program Files\Git\bin\bash.exe",
        r"C:\Program Files\Git\usr\bin\bash.exe",
    ]
    for candidate in candidates:
        if candidate and Path(candidate).is_file():
            return str(candidate)
    raise RuntimeError("bash not found; set AAEMU_BASH to Git's bash.exe")


def repo_root() -> Path:
    return Path(__file__).resolve().parents[4]


def parse_env_file(path: Path) -> dict[str, str]:
    values: dict[str, str] = {}
    for raw in path.read_text(encoding="utf-8", errors="replace").splitlines():
        line = raw.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        values[key.strip()] = value.strip()
    return values


@dataclass
class Lane:
    name: str
    worktree: Path
    target: Path
    live: Path
    port_prefix: str
    login_port: int
    game_port: int
    db_login: str
    db_game: str

    @property
    def game_log(self) -> Path:
        return self.live / "logs" / "rs-game.log"

    @property
    def login_log(self) -> Path:
        return self.live / "logs" / "rs-login.log"

    @property
    def bin_dir(self) -> Path:
        return self.target / "live"

    def exe(self, name: str) -> Path:
        return self.bin_dir / name

    def driver_dll(self) -> Path | None:
        for candidate in (
            self.target / "aadriver" / "aadriver.dll",
            self.target / "live" / "aadriver" / "aadriver.dll",
            repo_root() / "rust" / "crates" / "aaemu-clientdriver" / "native" / "out" / "aadriver.dll",
        ):
            if candidate.is_file():
                return candidate
        return None

    @classmethod
    def load(cls, name: str) -> "Lane":
        root = Path(os.environ.get("AAEMU_LANES", r"D:/aaemu-lanes"))
        path = root / f"{name}.env"
        if not path.is_file():
            raise RuntimeError(f"no lane '{name}' ({path}); create it with Scripts/lane.sh new")
        env = parse_env_file(path)
        prefix = env.get("PORT_PREFIX", "24")
        return cls(
            name=name,
            worktree=Path(env["WT"].replace("/", os.sep)),
            target=Path(env["TARGET"].replace("/", os.sep)),
            live=Path(env["LIVE"].replace("/", os.sep)),
            port_prefix=prefix,
            login_port=int(env.get("LOGIN_PORT", prefix + "237")),
            game_port=int(env.get("GAME_PORT", prefix + "239")),
            db_login=env.get("DB_LOGIN", "rs_login"),
            db_game=env.get("DB_GAME", "rs_game"),
        )


class Stack:
    def __init__(self, lane: Lane, *, log_sink=None):
        self.lane = lane
        self.bash = find_bash()
        self.log_sink = log_sink or (lambda message: print(message, flush=True))

    def _run(self, args: list[str], *, env: dict[str, str] | None = None, timeout: int = 1800) -> subprocess.CompletedProcess:
        import tempfile
        command = [self.bash, str(self.lane.worktree / "Scripts" / "live-stack.sh"), *args]
        merged = os.environ.copy()
        if env:
            merged.update(env)
        with tempfile.NamedTemporaryFile(delete=False, suffix=".log") as output:
            log_path = Path(output.name)
        try:
            with log_path.open("wb") as sink:
                result = subprocess.run(command, cwd=self.lane.worktree, env=merged,
                                        stdout=sink, stderr=subprocess.STDOUT, timeout=timeout)
            text = log_path.read_text(encoding="utf-8", errors="replace")
        finally:
            try:
                log_path.unlink()
            except OSError:
                pass
        return subprocess.CompletedProcess(command, result.returncode, text, "")

    def status(self) -> dict[str, bool]:
        result = self._run(["status"], timeout=60)
        output = (result.stdout or "") + (result.stderr or "")
        def up(name: str) -> bool:
            return any(line.strip().lower().startswith("up") and name in line
                       for line in output.splitlines())
        return {"login": up("aaemu-login.exe"), "game": up("aaemu-game.exe"),
                "mysql": up("mysql"), "text": output}

    def running(self) -> bool:
        state = self.status()
        return bool(state["login"] and state["game"])

    def build(self) -> None:
        self.log_sink(f"building lane {self.lane.name} (live profile)")
        result = self._run(["build"], timeout=3600)
        if result.returncode != 0:
            raise RuntimeError(f"live-stack build failed:\n{result.stdout[-4000:]}\n{result.stderr[-4000:]}")

        self.ensure_native_driver()

    def ensure_native_driver(self) -> None:
        if self.lane.driver_dll() is not None:
            return
        out = self.lane.target / "aadriver"
        out.mkdir(parents=True, exist_ok=True)
        script = repo_root() / "rust" / "crates" / "aaemu-clientdriver" / "native" / "build.bat"
        if not script.is_file():
            raise RuntimeError(f"native driver build script missing: {script}")
        self.log_sink(f"building native driver into {out}")
        env = os.environ.copy()
        env["AADRIVER_OUT"] = str(out)
        result = subprocess.run(["cmd", "/c", str(script)], env=env, text=True,
                                encoding="utf-8", errors="replace", capture_output=True, timeout=600)
        if result.returncode != 0:
            raise RuntimeError(f"native driver build failed:\n{result.stdout[-4000:]}\n{result.stderr[-4000:]}")

    def ensure_synced(self) -> None:
        result = self._run(["sync"], timeout=600)
        if result.returncode != 0:
            raise RuntimeError(f"live-stack sync failed:\n{result.stdout[-4000:]}\n{result.stderr[-4000:]}")

    def start(self, *, restart: bool = False) -> None:
        if restart and self.running():
            self.stop()
        if not self.running():
            self.ensure_synced()
            env = {"RUST_LOG": "info,aaemu_game::net::fields=trace", "NO_COLOR": "1"}
            self.log_sink("starting lane stack")
            result = self._run(["start"], env=env, timeout=600)
            if result.returncode != 0:
                raise RuntimeError(f"live-stack start failed:\n{result.stdout[-4000:]}\n{result.stderr[-4000:]}")
        if not self.running():
            raise RuntimeError("lane stack did not come up; run Scripts/live-stack.sh status")

    def stop(self) -> None:
        result = self._run(["stop"], timeout=120)
        if result.returncode != 0:
            self.log_sink(f"live-stack stop returned {result.returncode}: {result.stderr.strip()[:400]}")

    def trace_enabled(self) -> bool:
        log = self.lane.game_log
        if not log.is_file():
            return False
        try:
            with log.open("rb") as handle:
                handle.seek(0, os.SEEK_END)
                size = handle.tell()
                handle.seek(max(0, size - 2_000_000))
                tail = handle.read().decode("utf-8", "replace")
        except OSError:
            return False
        return "aaemu_game::net::fields" in tail or " fields - " in tail


def mysql_exe() -> Path:
    candidates = []
    if os.environ.get("AAEMU_MYSQL_BIN"):
        candidates.append(Path(os.environ["AAEMU_MYSQL_BIN"].replace("/", os.sep)) / "mysql.exe")
    for version in ("8.4", "8.0"):
        candidates.append(Path(rf"C:/Program Files/MySQL/MySQL Server {version}/bin/mysql.exe"))
    for candidate in candidates:
        if candidate.is_file():
            return candidate
    raise RuntimeError("mysql.exe not found; set AAEMU_MYSQL_BIN")


def require_file(path: Path, label: str) -> Path:
    if not path.is_file():
        raise RuntimeError(f"{label} not found: {path}")
    return path
