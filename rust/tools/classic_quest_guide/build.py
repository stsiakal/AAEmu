"""Build the lazy Lua data catalog for the ArcheAge Classic quest guide addon.

The catalog is expected data from the 1.2 AAEmu static database and world files.
It is *not* a record of ArcheAge Classic values: Classic title/body text and
observed state are supplied at runtime by the addon. Coordinates from
``Data/Worlds`` are 1.2 candidates and must be treated as unverified in Classic.

Inputs are opened read-only. All output goes below an explicit ``--output``
directory and is deterministic unless ``--timestamp`` is requested.

    python build.py --db compact.sqlite3 --worlds Data/Worlds \
        --spheres quests-spheres.json --output /path/to/package

Layout below ``--output``::

    manifest.json
    summary.json
    aaemu_quest_guide/data/index.lua
    aaemu_quest_guide/data/meta.lua
    aaemu_quest_guide/data/route.lua
    aaemu_quest_guide/data/chunk_0001.lua ...

``index.lua`` is the browsable metadata index. ``chunk_NNNN.lua`` files hold the
full quest records and are meant to be loaded lazily by the addon. The builder
and tests are Python 3.12 stdlib only; Lupa is used only by the tests.
"""

from __future__ import annotations

import argparse
import copy
from collections import Counter, defaultdict
from datetime import datetime, timezone
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import re
import shutil
import sqlite3
import subprocess
import sys
import tempfile


SCHEMA = "aaemu_quest_guide.catalog"
SCHEMA_VERSION = 1
DATA_VERSION = "1.2"
GENERATOR_NAME = "aaemu_quest_guide build.py"
GENERATOR_VERSION = "0.1.0"
MARKER_NAME = ".aaemu_quest_guide_generated"
MARKER_TEXT = "aaemu_quest_guide generated data v1; safe to replace\n"
DEFAULT_CHUNK_SIZE = 200
TUTORIAL_CATEGORY = 45
FAIL_KIND = 5
DROP_KIND = 7
OPTIONAL_KINDS = {FAIL_KIND, DROP_KIND}
SUPPLY_KINDS = {3, 8}
KIND_NAMES = {
    0: "Invalid", 1: "None", 2: "Start", 3: "Supply", 4: "Progress",
    5: "Fail", 6: "Ready", 7: "Drop", 8: "Reward",
}
# Act class -> (detail table, aggregate field). The detail table is still read
# for every act; this map additionally builds the expected reward aggregate.
SUPPLY_DETAILS = {
    "QuestActSupplyExp": ("quest_act_supply_exps", "exp"),
    "QuestActSupplyCopper": ("quest_act_supply_coppers", "copper"),
    "QuestActSupplyItem": ("quest_act_supply_items", "items"),
    "QuestActSupplySelectiveItem": ("quest_act_supply_selective_items", "selective_items"),
    "QuestActSupplyAppellation": ("quest_act_supply_appellations", "appellations"),
    "QuestActSupplyHonorPoint": ("quest_act_supply_honor_points", "honor_points"),
    "QuestActSupplyLivingPoint": ("quest_act_supply_living_points", "living_points"),
    "QuestActSupplyJuryPoint": ("quest_act_supply_jury_points", "jury_points"),
    "QuestActSupplyCrimePoint": ("quest_act_supply_crime_points", "crime_points"),
    "QuestActSupplyAAPoint": ("quest_act_supply_aa_points", "aa_points"),
    "QuestActSupplyLp": ("quest_act_supply_lps", "lp"),
    "QuestActSupplyLP": ("quest_act_supply_lps", "lp"),
    "QuestActSupplySkill": ("quest_act_supply_skills", "skills"),
    "QuestActSupplyInteraction": ("quest_act_supply_interactions", "interactions"),
    "QuestActSupplyRemoveItem": ("quest_act_supply_remove_items", "remove_items"),
}
REWARD_WARNING = (
    "act_exp/act_copper are immediate 1.2 supply acts; level_supply is the "
    "quest_supplies row applied on the Reward step. These are expected 1.2 "
    "semantics, not observed Classic values."
)
ROUTE_WARNING = (
    "Quest 330 must be taken before quest 6198. Completing 6198 can block 330; "
    "the 330-before-6198 exclusion is preserved from the 1.2 analysis and must "
    "be verified in Classic."
)
ROUTE_DISCLAIMER = (
    "Candidate starter route only. It is not validated, not certified and not "
    "claimed optimal. The full catalog is not a leveling route."
)
COORDINATE_NOTICE = (
    "World positions come from the 1.2 Data/Worlds spawn files and are "
    "unverified candidates for the Classic world. They are not Classic "
    "coordinates and must not be used without explicit opt-in."
)
TEXT_NOTICE = (
    "compact.sqlite3 names and context texts are Korean internal 1.2 data. "
    "Classic localized title/body must be read at runtime with api.Quest; do "
    "not present these strings as Classic localized text."
)
ID_SPACE_NOTICE = (
    "Quest target ids are static 1.2 template ids (NPC/doodad/item). They are "
    "not runtime UnitInfo/GetUnitId opaque ids."
)


class BuildError(RuntimeError):
    """A user-facing build failure."""


# ---------------------------------------------------------------------------
# Small helpers


def truth(value):
    return value in ("t", "true", 1, True)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def sha256_tree(root: Path) -> tuple[str, int, int]:
    """Hash a directory tree by relative path and content, in sorted order."""
    digest = hashlib.sha256()
    files = 0
    size = 0
    for path in sorted((p for p in root.rglob("*") if p.is_file()),
                       key=lambda p: p.relative_to(root).as_posix()):
        relative = path.relative_to(root).as_posix().encode("utf-8")
        digest.update(relative)
        digest.update(b"\0")
        with path.open("rb") as handle:
            for block in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(block)
                size += len(block)
        digest.update(b"\0")
        files += 1
    return digest.hexdigest(), files, size


def is_relative_to(path: Path, parent: Path) -> bool:
    try:
        path.relative_to(parent)
        return True
    except ValueError:
        return False


def overlaps(a: Path, b: Path) -> bool:
    return a == b or is_relative_to(a, b) or is_relative_to(b, a)


def require_file(path: Path, label: str) -> Path:
    resolved = path.expanduser().resolve()
    if not resolved.is_file():
        raise BuildError(f"{label} is not a file: {path}")
    return resolved


def require_dir(path: Path, label: str) -> Path:
    resolved = path.expanduser().resolve()
    if not resolved.is_dir():
        raise BuildError(f"{label} is not a directory: {path}")
    return resolved


def find_repo_root(start: Path) -> Path | None:
    for candidate in (start, *start.parents):
        if (candidate / ".git").exists():
            return candidate
    return None


def git_provenance(source_dir: Path, commit_override: str | None):
    root = find_repo_root(source_dir)
    commit = None
    source = "unknown"
    dirty = None
    if commit_override:
        commit = commit_override
        source = "cli"
    elif root is not None:
        try:
            commit = subprocess.run(
                ["git", "-C", str(root), "rev-parse", "HEAD"],
                check=True, capture_output=True, text=True, timeout=15,
            ).stdout.strip() or None
            if commit:
                source = "git"
        except (OSError, subprocess.SubprocessError):
            commit = None
    if root is not None:
        try:
            relative = source_dir.resolve().relative_to(root.resolve()).as_posix()
            status = subprocess.run(
                ["git", "-C", str(root), "status", "--porcelain", "--", relative],
                check=True, capture_output=True, text=True, timeout=15,
            ).stdout
            dirty = bool(status.strip())
        except (OSError, subprocess.SubprocessError, ValueError):
            dirty = None
    return {"commit": commit, "source": source, "dirty": dirty}


def load_plan_module():
    path = Path(__file__).resolve().parent.parent / "quest_audit" / "plan.py"
    if not path.is_file():
        raise BuildError(f"plan module not found: {path}")
    spec = importlib.util.spec_from_file_location("aaemu_quest_plan", path)
    if spec is None or spec.loader is None:
        raise BuildError(f"cannot import plan module: {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def validate_output_path(output: Path, inputs: dict[str, Path], source_dir: Path):
    """Reject output paths that overlap an input or the tracked tool source.

    Overlap includes equal paths, output containing an input, and output living
    inside an input directory. The builder never writes near data it cannot
    prove is its own.
    """
    resolved = output.expanduser().resolve()
    source = source_dir.resolve()
    for label, path in inputs.items():
        if overlaps(resolved, path):
            raise BuildError(
                f"output {resolved} overlaps {label} {path}; choose a path "
                "outside the inputs and the tool source"
            )
    if overlaps(resolved, source):
        raise BuildError(
            f"output {resolved} overlaps the tracked tool source {source}"
        )
    return resolved


def open_readonly(db: Path) -> sqlite3.Connection:
    uri = db.resolve().as_uri() + "?mode=ro"
    try:
        conn = sqlite3.connect(uri, uri=True)
    except sqlite3.Error as error:
        raise BuildError(f"cannot open database read-only: {error}") from error
    conn.row_factory = sqlite3.Row
    return conn


def table_names(conn: sqlite3.Connection) -> set[str]:
    return {
        row[0] for row in conn.execute(
            "SELECT name FROM sqlite_master WHERE type = 'table'"
        )
    }


def rows(conn: sqlite3.Connection, table: str) -> list[dict]:
    if not re.fullmatch(r"[A-Za-z0-9_]+", table):
        raise BuildError(f"unsafe table name: {table!r}")
    return [dict(row) for row in conn.execute(f'SELECT * FROM "{table}"')]


def require_tables(conn: sqlite3.Connection, required: set[str]):
    present = table_names(conn)
    missing = sorted(required - present)
    if missing:
        raise BuildError(
            "database is missing required table(s): " + ", ".join(missing)
        )


# ---------------------------------------------------------------------------
# Lua serialization (Lua 5.1)


LUA_KEYWORDS = {
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function",
    "if", "in", "local", "nil", "not", "or", "repeat", "return", "then",
    "true", "until", "while",
}
LUA_IDENTIFIER = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")


def lua_string(value: str) -> str:
    """Escape a Python string as a Lua 5.1 double-quoted string.

    UTF-8 text is emitted directly. Control characters (including NUL and DEL)
    use three-digit decimal escapes so a following digit cannot extend them.
    Quotes and backslashes are always escaped.
    """
    if not isinstance(value, str):
        raise BuildError(f"not a string: {value!r}")
    out = ['"']
    for char in value:
        code = ord(char)
        if char == '"':
            out.append('\\"')
        elif char == "\\":
            out.append("\\\\")
        elif char == "\n":
            out.append("\\n")
        elif char == "\r":
            out.append("\\r")
        elif char == "\t":
            out.append("\\t")
        elif code < 32 or code == 127:
            out.append("\\%03d" % code)
        else:
            try:
                char.encode("utf-8")
            except UnicodeEncodeError:
                out.append("?")
            else:
                out.append(char)
    out.append('"')
    return "".join(out)


def _lua_key(key) -> str:
    if isinstance(key, bool):
        raise BuildError("boolean table keys are not supported")
    if isinstance(key, int):
        return "[" + str(key) + "]"
    if isinstance(key, str):
        if LUA_IDENTIFIER.match(key) and key not in LUA_KEYWORDS:
            return key
        return "[" + lua_string(key) + "]"
    raise BuildError(f"unsupported table key: {key!r}")


def lua_literal(value, seen=None) -> str:
    if value is None:
        return "nil"
    if value is True:
        return "true"
    if value is False:
        return "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, float):
        if not math.isfinite(value):
            raise BuildError(f"non-finite number cannot be written to Lua: {value!r}")
        return repr(value)
    if isinstance(value, str):
        return lua_string(value)
    if isinstance(value, (list, tuple)):
        if seen is None:
            seen = set()
        marker = id(value)
        if marker in seen:
            raise BuildError("cycle in generated Lua data")
        seen.add(marker)
        try:
            return "{" + ",".join(lua_literal(item, seen) for item in value) + "}"
        finally:
            seen.remove(marker)
    if isinstance(value, dict):
        if seen is None:
            seen = set()
        marker = id(value)
        if marker in seen:
            raise BuildError("cycle in generated Lua data")
        seen.add(marker)
        try:
            parts = []
            for key in sorted(value, key=lambda k: (1, str(k)) if isinstance(k, str)
                              else (0, k)):
                parts.append(_lua_key(key) + "=" + lua_literal(value[key], seen))
            return "{" + ",".join(parts) + "}"
        finally:
            seen.remove(marker)
    raise BuildError(f"unsupported value for Lua serialization: {type(value).__name__}")


def lua_header(description: str) -> str:
    return (
        "-- Generated by " + GENERATOR_NAME + " " + GENERATOR_VERSION + ".\n"
        "-- " + description + "\n"
        "-- Expected data version: " + DATA_VERSION + ". Do not edit.\n"
    )


# ---------------------------------------------------------------------------
# Act detail tables


def snake_case(name: str) -> str:
    name = re.sub(r"(?<=[a-z0-9])(?=[A-Z])", "_", name)
    name = re.sub(r"(?<=[A-Z])(?=[A-Z][a-z])", "_", name)
    return name.lower()


def detail_table_for(act_type: str, tables: set[str]) -> str | None:
    if not act_type.startswith("QuestAct"):
        return None
    base = "quest_act_" + snake_case(act_type[len("QuestAct"):])
    for candidate in (base + "s", base + "es"):
        if candidate in tables:
            return candidate
    if base.endswith("y"):
        candidate = base[:-1] + "ies"
        if candidate in tables:
            return candidate
    return None


# ---------------------------------------------------------------------------
# Catalog construction


def load_plan_result(conn: sqlite3.Connection, worlds: Path, spheres_path: Path | None,
                     plan_module):
    require_tables(conn, {
        "quest_contexts", "quest_components", "quest_acts", "unit_reqs",
        "npcs", "doodad_almighties", "npc_spawner_npcs", "game_schedule_spawners",
        "game_schedule_doodads", "npc_spawners", "game_schedules",
        "quest_monster_npcs", "quest_item_group_items", "craft_products",
        "items", "loots", "loot_pack_dropping_npcs", "gain_loot_pack_item_effects",
        "special_effects", "skill_products", "doodad_func_groups", "doodad_funcs",
        "doodad_func_loot_items", "doodad_func_loot_packs", "doodad_func_purchases",
        "merchant_goods", "merchant_packs", "merchants", "skills", "skill_effects",
        "effects", "spawn_effects",
    })
    ids = {
        "npc": {row["id"] for row in rows(conn, "npcs")},
        "doodad": {row["id"] for row in rows(conn, "doodad_almighties")},
    }
    spawner_npcs = [
        (row["npc_spawner_id"], row["member_id"])
        for row in rows(conn, "npc_spawner_npcs")
    ]
    schedule_spawners = defaultdict(list)
    for row in rows(conn, "game_schedule_spawners"):
        schedule_spawners[row["spawner_id"]].append(row["game_schedule_id"])
    schedule_doodads = defaultdict(list)
    for row in rows(conn, "game_schedule_doodads"):
        schedule_doodads[row["doodad_id"]].append(row["game_schedule_id"])
    windows = {
        row["id"]: (row["startTime"], row["endTime"])
        for row in rows(conn, "npc_spawners")
        if row.get("startTime", 0) > 0 or row.get("endTime", 0) > 0
    }
    known_schedules = {row["id"] for row in rows(conn, "game_schedules")}
    spawns = plan_module.load_spawns(
        worlds, ids, spawner_npcs, schedule_spawners, schedule_doodads,
        windows, known_schedules,
    )
    spheres = None
    sphere_stats = None
    if spheres_path is not None:
        spheres = plan_module.load_spheres(spheres_path)
        sphere_stats = {
            "with_origin": spheres["with_origin"],
            "without_origin": spheres["without_origin"],
        }
    result = plan_module.plan(conn, spawns, None, spheres)
    return result, sphere_stats


def build_quest_record(
    conn: sqlite3.Connection,
    plan_quest: dict,
    context: dict,
    category_names: dict,
    components: list[dict],
    requirements: dict[int, list[dict]],
    acts_by_id: dict[int, dict],
    detail_cache: dict[str, dict[int, dict]],
    tables: set[str],
    context_texts: dict[int, list[dict]],
    level_supplies: dict[int, dict],
    plan_module,
) -> dict:
    quest_id = plan_quest["quest_id"]
    component_by_id = {component["id"]: component for component in components}

    groups = []
    start_components = []
    for component in sorted(
        components, key=lambda c: (c.get("component_kind_id") or 0, c["id"])
    ):
        component_requirements = requirements.get(component["id"], [])
        kind_id = component.get("component_kind_id")
        if kind_id == 2:
            start_components.append(component["id"])
        if not component_requirements and kind_id != 2:
            continue
        groups.append({
            "component_id": component["id"],
            "component_kind": KIND_NAMES.get(kind_id, str(kind_id)),
            "component_kind_id": kind_id,
            "scope": "accept" if kind_id == 2 else "component",
            "optional_branch": kind_id in OPTIONAL_KINDS,
            "mode": "OR" if truth(component.get("or_unit_reqs")) else "AND",
            "unrestricted": not component_requirements,
            "requirements": [
                {
                    "id": requirement["id"],
                    "kind_id": requirement["kind_id"],
                    "kind": plan_module.REQ_KINDS.get(
                        requirement["kind_id"],
                        "unknown_%s" % requirement["kind_id"],
                    ),
                    "value1": requirement["value1"],
                    "value2": requirement["value2"],
                }
                for requirement in sorted(component_requirements,
                                          key=lambda r: r["id"])
            ],
        })
    acceptance = {
        "mode": "any-start-component",
        "start_components": start_components,
        "start_components_are_alternatives": True,
        "note": (
            "Each Start component is an alternative accept path. Requirements "
            "inside one component follow that component's AND/OR mode; an "
            "empty Start group is an unrestricted alternative. Do not treat "
            "the flattened prereq_quests list as a mandatory all-of list."
        ),
    }

    plan_steps_by_component: dict[int, list[dict]] = defaultdict(list)
    for step in plan_quest.get("steps", []):
        plan_steps_by_component[step["component_id"]].append(step)

    normal_steps = []
    branches = {"fail": [], "drop": []}
    supply_acts = []
    expected = {
        "act_exp": 0,
        "act_copper": 0,
        "items": [],
        "selective_items": [],
        "appellations": [],
        "honor_points": 0,
        "living_points": 0,
        "jury_points": 0,
        "crime_points": 0,
        "aa_points": 0,
        "lp": 0,
        "skills": [],
        "interactions": [],
        "remove_items": [],
    }
    unmapped_act_details = []
    for component_id in sorted(
        plan_steps_by_component,
        key=lambda cid: (
            (component_by_id.get(cid) or {}).get("component_kind_id") or 0,
            cid,
        ),
    ):
        component = component_by_id.get(component_id) or {}
        kind_id = component.get("component_kind_id")
        component_steps = []
        for plan_step in plan_steps_by_component[component_id]:
            step = copy.deepcopy(plan_step)
            act_type = step.get("act_type", "")
            detail_table = detail_table_for(act_type, tables)
            act_row = acts_by_id.get(step.get("act_id"))
            detail_id = act_row.get("act_detail_id") if act_row else None
            detail = None
            if detail_table is not None and detail_id is not None:
                detail = detail_cache.get(detail_table, {}).get(detail_id)
                detail = copy.deepcopy(detail) if detail is not None else None
            else:
                unmapped_act_details.append(act_type)
            step["component_kind_id"] = kind_id
            step["optional_branch"] = kind_id in OPTIONAL_KINDS
            step["act_detail_id"] = detail_id
            step["act_detail_table"] = detail_table
            step["act_detail"] = detail
            target = step.get("target")
            if isinstance(target, dict):
                target.setdefault("id_space", "static_template")
            for key, source in (("locations", "1.2-worlds-candidate"),
                                ("spheres", "1.2-sign-spheres")):
                if step.get(key):
                    step[key + "_source"] = source
                    step["unverified_coordinates"] = True
            if (act_type in SUPPLY_DETAILS and detail is not None
                    and kind_id not in OPTIONAL_KINDS):
                field = SUPPLY_DETAILS[act_type][1]
                step["supply_field"] = field
                supply_acts.append({
                    "component_id": component_id,
                    "component_kind": KIND_NAMES.get(kind_id, str(kind_id)),
                    "component_kind_id": kind_id,
                    "act_id": step.get("act_id"),
                    "act_type": act_type,
                    "detail_table": detail_table,
                    "detail": copy.deepcopy(detail),
                })
                if field == "exp":
                    expected["act_exp"] += int(detail.get("exp") or 0)
                elif field == "copper":
                    expected["act_copper"] += int(detail.get("amount") or 0)
                elif field in ("items", "selective_items"):
                    expected[field].append({
                        "item_id": detail.get("item_id"),
                        "count": detail.get("count"),
                        "grade_id": detail.get("grade_id"),
                    })
                elif field == "appellations":
                    expected["appellations"].append(detail.get("appellation_id"))
                elif field in ("honor_points", "living_points", "jury_points",
                               "crime_points", "aa_points"):
                    expected[field] += int(detail.get("point") or 0)
                elif field == "lp":
                    expected["lp"] += int(detail.get("lp") or 0)
                elif field == "skills":
                    expected["skills"].append(detail.get("skill_id"))
                elif field == "interactions":
                    expected["interactions"].append(detail.get("wi_id"))
                elif field == "remove_items":
                    expected["remove_items"].append({
                        "item_id": detail.get("item_id"),
                        "count": detail.get("count"),
                    })
            component_steps.append(step)

        if kind_id == FAIL_KIND:
            branches["fail"].append({
                "component_id": component_id,
                "component_kind": "Fail",
                "component_kind_id": kind_id,
                "optional_branch": True,
                "steps": component_steps,
            })
        elif kind_id == DROP_KIND:
            branches["drop"].append({
                "component_id": component_id,
                "component_kind": "Drop",
                "component_kind_id": kind_id,
                "optional_branch": True,
                "steps": component_steps,
            })
        else:
            normal_steps.extend(component_steps)

    level = context.get("LEVEL")
    level_supply = copy.deepcopy(level_supplies.get(level)) if level is not None else None

    record = {
        "id": quest_id,
        "name": plan_quest.get("name"),
        "category_id": context.get("category_id"),
        "category_name": category_names.get(context.get("category_id")),
        "level": level,
        "min_level": plan_quest.get("min_level"),
        "status": plan_quest.get("status"),
        "blockers": plan_quest.get("blockers", []),
        "repeatable": truth(context.get("REPEATABLE")),
        "selective": truth(context.get("selective")),
        "successive": truth(context.get("successive")),
        "restart_on_fail": truth(context.get("restart_on_fail")),
        "let_it_done": truth(context.get("let_it_done")),
        "chapter_idx": context.get("chapter_idx"),
        "quest_idx": context.get("quest_idx"),
        "milestone_id": context.get("milestone_id"),
        "zone_id": context.get("zone_id"),
        "degree": context.get("degree"),
        "grade_id": context.get("grade_id"),
        "translate": truth(context.get("translate")),
        "prereq_quests": plan_quest.get("prereq_quests", []),
        "acceptance": acceptance,
        "requirement_groups": groups,
        "steps": normal_steps,
        "branches": branches,
        "rewards": {
            "supply_acts": supply_acts,
            "expected": expected,
            "level_supply": level_supply,
            "semantics": REWARD_WARNING,
        },
        "context_texts": [
            {"kind_id": text.get("quest_context_text_kind_id"),
             "text": text.get("text")}
            for text in sorted(context_texts.get(quest_id, []),
                               key=lambda t: (t.get("quest_context_text_kind_id") or 0,
                                              t.get("id") or 0))
        ],
        "text_source_notice": TEXT_NOTICE,
        "target_id_space": ID_SPACE_NOTICE,
    }
    if plan_quest.get("prereq_notes"):
        record["prereq_notes"] = plan_quest["prereq_notes"]
    if record["prereq_quests"]:
        record["prereq_quests_note"] = (
            "Aggregated from alternative Start components; not a mandatory "
            "all-of condition."
        )
    if unmapped_act_details:
        record["unmapped_act_details"] = sorted(set(unmapped_act_details))
    return record


def build_catalog(conn: sqlite3.Connection, plan_result: dict,
                  plan_module, chunk_size: int = DEFAULT_CHUNK_SIZE):
    tables = table_names(conn)
    require_tables(conn, {
        "quest_contexts", "quest_components", "quest_acts", "unit_reqs",
        "quest_categories", "quest_context_texts", "quest_supplies",
    })
    contexts = {row["id"]: row for row in rows(conn, "quest_contexts")}
    categories = {
        row["id"]: row.get("name") for row in rows(conn, "quest_categories")
    }
    components_by_quest: dict[int, list[dict]] = defaultdict(list)
    for component in rows(conn, "quest_components"):
        components_by_quest[component["quest_context_id"]].append(component)
    requirements: dict[int, list[dict]] = defaultdict(list)
    for requirement in rows(conn, "unit_reqs"):
        if requirement.get("owner_type") == "QuestComponent":
            requirements[requirement["owner_id"]].append(requirement)
    acts: dict[int, list[dict]] = defaultdict(list)
    acts_by_id: dict[int, dict] = {}
    for act in rows(conn, "quest_acts"):
        acts[act["quest_component_id"]].append(act)
        acts_by_id[act["id"]] = act
    context_texts: dict[int, list[dict]] = defaultdict(list)
    for text in rows(conn, "quest_context_texts"):
        context_texts[text["quest_context_id"]].append(text)
    level_supplies = {row["LEVEL"]: row for row in rows(conn, "quest_supplies")}

    act_types = {act["act_detail_type"] for act in rows(conn, "quest_acts")}
    detail_tables = set()
    for act_type in act_types:
        table = detail_table_for(act_type, tables)
        if table is not None:
            detail_tables.add(table)
    detail_cache: dict[str, dict[int, dict]] = {}
    for table in sorted(detail_tables):
        detail_cache[table] = {row["id"]: row for row in rows(conn, table)}

    quests = []
    for plan_quest in sorted(plan_result["quests"], key=lambda q: q["quest_id"]):
        quest_id = plan_quest["quest_id"]
        context = contexts.get(quest_id, {})
        quests.append(build_quest_record(
            conn, plan_quest, context, categories,
            components_by_quest.get(quest_id, []), requirements, acts_by_id,
            detail_cache, tables, context_texts, level_supplies, plan_module,
        ))

    chunks = []
    chunk_files: dict[str, str] = {}
    for number, start in enumerate(range(0, len(quests), chunk_size), start=1):
        group = quests[start:start + chunk_size]
        file_key = "chunk_%04d" % number
        lines = [lua_header(
            "Quest chunk %d; load lazily. Contains %d quest records." % (number, len(group))
        ), "return {\n",
            "  chunk = %d,\n" % number,
            "  first_id = %s,\n" % (group[0]["id"] if group else "nil"),
            "  last_id = %s,\n" % (group[-1]["id"] if group else "nil"),
            "  quests = {\n"]
        for quest in group:
            lines.append("    [%d] = %s,\n" % (quest["id"], lua_literal(quest)))
        lines.append("  },\n}\n")
        chunk_files[file_key] = "".join(lines)
        chunks.append({
            "chunk": number,
            "file": "data/" + file_key,
            "first_id": group[0]["id"] if group else None,
            "last_id": group[-1]["id"] if group else None,
            "count": len(group),
        })

    chunk_by_id = {}
    sorted_ids = [quest["id"] for quest in quests]
    for index, quest_id in enumerate(sorted_ids):
        chunk_by_id[quest_id] = index // chunk_size + 1

    index_entries = []
    for quest in quests:
        index_entries.append({
            "id": quest["id"],
            "name": quest["name"],
            "category_id": quest["category_id"],
            "level": quest["level"],
            "min_level": quest["min_level"],
            "status": quest["status"],
            "zone_id": quest["zone_id"],
            "chunk": chunk_by_id[quest["id"]],
            "has_branches": bool(quest["branches"]["fail"] or quest["branches"]["drop"]),
        })
    index = {
        "data_version": DATA_VERSION,
        "generator_version": GENERATOR_VERSION,
        "chunk_size": chunk_size,
        "chunk_count": len(chunks),
        "quest_count": len(quests),
        "categories": categories,
        "chunks": chunks,
        "quests": index_entries,
        "coordinate_notice": COORDINATE_NOTICE,
        "text_notice": TEXT_NOTICE,
        "id_space_notice": ID_SPACE_NOTICE,
    }
    index_text = lua_header(
        "Metadata index for browsing; chunks are loaded on demand."
    )
    index_text += "return {\n"
    for key in ("data_version", "generator_version", "chunk_size", "chunk_count",
                "quest_count", "coordinate_notice", "text_notice",
                "id_space_notice"):
        index_text += "  %s = %s,\n" % (key, lua_literal(index[key]))
    index_text += "  categories = %s,\n" % lua_literal(categories)
    index_text += "  chunks = {\n"
    for chunk in chunks:
        index_text += "    %s,\n" % lua_literal(chunk)
    index_text += "  },\n  quests = {\n"
    for entry in index_entries:
        index_text += "    %s,\n" % lua_literal(entry)
    index_text += "  },\n}\n"

    route = {
        "label": "Candidate starter route (unverified)",
        "status": "unverified",
        "validated": False,
        "optimal": False,
        "full_catalog_is_route": False,
        "quest_ids": [330, 6198],
        "warnings": [ROUTE_DISCLAIMER, ROUTE_WARNING],
        "note": "Only the 330-before-6198 ordering is preserved; verify all "
                "quest availability in ArcheAge Classic.",
    }
    route_text = lua_header("Candidate route metadata; not validated or optimal.")
    route_text += "return " + lua_literal(route) + "\n"
    chunk_files["index"] = index_text
    chunk_files["route"] = route_text

    return {
        "quests": quests,
        "chunks": chunks,
        "index": index,
        "route": route,
        "files": chunk_files,
        "chunk_by_id": chunk_by_id,
    }


def build_meta(manifest: dict) -> dict:
    return {
        "schema": manifest["schema"],
        "schema_version": manifest["schema_version"],
        "data_version": manifest["data_version"],
        "expected_data_version": DATA_VERSION,
        "generator": manifest["generator"],
        "inputs": {
            "db": manifest["inputs"]["db"],
            "worlds": manifest["inputs"]["worlds"],
            "spheres": manifest["inputs"].get("spheres"),
        },
        "coordinate_provenance": manifest["coordinate_provenance"],
        "observed_environment": manifest["observed_environment"],
        "catalog": manifest["catalog"],
        "route": manifest["route"],
        "notices": manifest["notices"],
    }


def summarize(catalog: dict, plan_result: dict, manifest: dict) -> dict:
    quests = catalog["quests"]
    status = Counter(quest["status"] for quest in quests)
    step_status = Counter(
        step["status"] for quest in quests for step in quest["steps"]
    )
    branch_steps = sum(
        len(component["steps"])
        for quest in quests
        for branch in ("fail", "drop")
        for component in quest["branches"][branch]
    )
    supply_acts = sum(len(quest["rewards"]["supply_acts"]) for quest in quests)
    level_supplies = sum(
        1 for quest in quests if quest["rewards"]["level_supply"]
    )
    requirement_groups = sum(len(quest["requirement_groups"]) for quest in quests)
    requirement_entries = sum(
        len(group["requirements"])
        for quest in quests for group in quest["requirement_groups"]
    )
    or_groups = sum(
        1 for quest in quests for group in quest["requirement_groups"]
        if group["mode"] == "OR"
    )
    locations = sum(
        len(step.get("locations", [])) for quest in quests for step in quest["steps"]
    )
    steps_with_locations = sum(
        1 for quest in quests for step in quest["steps"] if step.get("locations")
    )
    sphere_steps = sum(
        1 for quest in quests for step in quest["steps"] if step.get("spheres")
    )
    spheres = sum(
        len(step.get("spheres", [])) for quest in quests for step in quest["steps"]
    )
    sources = sum(
        1 for quest in quests for step in quest["steps"] if step.get("sources")
    )
    item_sources = sum(
        1 for quest in quests for step in quest["steps"] if step.get("item_sources")
    )
    act_details = sum(
        1 for quest in quests
        for step in list(quest["steps"])
        + [s for b in ("fail", "drop") for c in quest["branches"][b]
           for s in c["steps"]]
        if step.get("act_detail") is not None
    )
    files = manifest["files"]
    return {
        "schema": SCHEMA,
        "schema_version": SCHEMA_VERSION,
        "data_version": DATA_VERSION,
        "expected_data_version": DATA_VERSION,
        "quests": len(quests),
        "contexts": manifest["catalog"]["contexts"],
        "status": dict(sorted(status.items())),
        "step_status": dict(sorted(step_status.items())),
        "steps": {
            "normal": sum(len(quest["steps"]) for quest in quests),
            "fail_branch": sum(
                len(component["steps"]) for quest in quests
                for component in quest["branches"]["fail"]
            ),
            "drop_branch": sum(
                len(component["steps"]) for quest in quests
                for component in quest["branches"]["drop"]
            ),
            "branch_total": branch_steps,
            "with_act_detail": act_details,
        },
        "requirements": {
            "groups": requirement_groups,
            "entries": requirement_entries,
            "or_groups": or_groups,
            "and_groups": requirement_groups - or_groups,
            "quests_with_groups": sum(
                1 for quest in quests if quest["requirement_groups"]
            ),
        },
        "rewards": {
            "supply_acts": supply_acts,
            "quests_with_level_supply": level_supplies,
            "quests_with_act_exp": sum(
                1 for quest in quests
                if quest["rewards"]["expected"]["act_exp"]
            ),
        },
        "coordinates": {
            "steps_with_locations": steps_with_locations,
            "locations": locations,
            "steps_with_item_sources": item_sources,
            "steps_with_dynamic_sources": sources,
            "steps_with_spheres": sphere_steps,
            "spheres": spheres,
            "unverified_classic": True,
        },
        "chunks": {
            "count": len(catalog["chunks"]),
            "size": manifest["catalog"]["chunk_size"],
            "quests": len(quests),
        },
        "sources": {
            "db_sha256": manifest["inputs"]["db"]["sha256"],
            "worlds_sha256": manifest["inputs"]["worlds"]["sha256"],
            "spheres_sha256": (manifest["inputs"].get("spheres") or {}).get("sha256"),
        },
        "files": {
            "count": len(files),
            "bytes": sum(entry["bytes"] for entry in files),
        },
        "warnings": manifest["notices"],
        "route": catalog["route"],
        "spheres": manifest["inputs"].get("spheres"),
    }


# ---------------------------------------------------------------------------
# Output writing


def atomic_write(path: Path, data: bytes):
    path.parent.mkdir(parents=True, exist_ok=True)
    handle = tempfile.NamedTemporaryFile(
        "wb", dir=path.parent, prefix="." + path.name + ".", delete=False
    )
    try:
        with handle:
            handle.write(data)
        os.replace(handle.name, path)
    finally:
        if os.path.exists(handle.name):
            os.unlink(handle.name)


def check_safe_data_dir(output: Path, data_dir: Path):
    """Refuse a data dir that resolves outside output or through a link.

    A generated marker in a linked directory is not enough: replacing it would
    recursively delete foreign data.
    """
    output_absolute = output.resolve()
    if not is_relative_to(data_dir.absolute(), output_absolute):
        raise BuildError(
            f"refusing to use data directory outside output: {data_dir}"
        )
    resolved = data_dir.resolve(strict=False)
    if resolved == output_absolute or not is_relative_to(resolved, output_absolute):
        raise BuildError(
            f"refusing to use data directory that resolves outside output: "
            f"{data_dir} -> {resolved}"
        )
    if data_dir.exists() and (data_dir.is_symlink()
                              or getattr(data_dir, "is_junction", lambda: False)()):
        raise BuildError(
            f"refusing to replace linked data directory: {data_dir}"
        )


def check_output_target(output: Path, data_dir: Path):
    package_folder = data_dir.parent
    if package_folder != output and package_folder.exists() and package_folder.is_dir():
        siblings = [entry for entry in package_folder.iterdir() if entry.name != data_dir.name]
        if siblings and not (data_dir / MARKER_NAME).is_file():
            raise BuildError(
                f"refusing to write into foreign package folder {package_folder}: "
                "it contains files but no generated data marker")
    usable_data = not data_dir.exists()
    if data_dir.exists():
        if data_dir.is_dir():
            marker = data_dir / MARKER_NAME
            if marker.is_file() and marker.read_text(encoding="utf-8") == MARKER_TEXT:
                usable_data = True
            elif not any(data_dir.iterdir()):
                usable_data = True
        if not usable_data:
            raise BuildError(
                f"refusing to replace {data_dir}: it is not generated by "
                f"{GENERATOR_NAME} (missing {MARKER_NAME})"
            )
    for name in ("manifest.json", "summary.json"):
        path = output / name
        if path.exists():
            try:
                existing = json.loads(path.read_text(encoding="utf-8"))
            except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
                raise BuildError(
                    f"refusing to replace {path}: it is not a generated catalog file"
                ) from error
            if not isinstance(existing, dict) or existing.get("schema") != SCHEMA:
                raise BuildError(
                    f"refusing to replace {path}: it is not a generated catalog file"
                )


def write_output(output: Path, files: dict[str, str], manifest: dict,
                 summary: dict, data_dir: Path):
    output.mkdir(parents=True, exist_ok=True)
    check_safe_data_dir(output, data_dir)
    check_output_target(output, data_dir)
    temp_root = Path(tempfile.mkdtemp(prefix=".aaemu_quest_guide.", dir=output))
    try:
        temp_data = temp_root / "data"
        temp_data.mkdir()
        for key in sorted(files):
            name = "chunk_%04d.lua" % int(key[6:]) if key.startswith("chunk_") else (
                key + ".lua"
            )
            (temp_data / name).write_text(files[key], encoding="utf-8",
                                          newline="\n")
        (temp_data / MARKER_NAME).write_text(MARKER_TEXT, encoding="utf-8",
                                             newline="\n")
        files_manifest = []
        for path in sorted(temp_data.iterdir(), key=lambda p: p.name):
            if path.name == MARKER_NAME:
                continue
            files_manifest.append({
                "path": "aaemu_quest_guide/data/" + path.name,
                "sha256": sha256_file(path),
                "bytes": path.stat().st_size,
            })
        manifest = copy.deepcopy(manifest)
        manifest["files"] = files_manifest
        summary = copy.deepcopy(summary)
        summary["files"] = {
            "count": len(files_manifest),
            "bytes": sum(entry["bytes"] for entry in files_manifest),
        }
        if data_dir.exists():
            shutil.rmtree(data_dir)
        data_dir.parent.mkdir(parents=True, exist_ok=True)
        os.replace(temp_data, data_dir)
        atomic_write(output / "manifest.json",
                     (json.dumps(manifest, ensure_ascii=False, indent=2,
                                 sort_keys=True) + "\n").encode("utf-8"))
        atomic_write(output / "summary.json",
                     (json.dumps(summary, ensure_ascii=False, indent=2,
                                 sort_keys=True) + "\n").encode("utf-8"))
        return manifest, summary
    finally:
        shutil.rmtree(temp_root, ignore_errors=True)


# ---------------------------------------------------------------------------
# Main build


def run_build(db_path: Path, worlds_path: Path, output_path: Path,
              spheres_path: Path | None = None, chunk_size: int = DEFAULT_CHUNK_SIZE,
              code_commit: str | None = None, timestamp: bool = False,
              classic_build: str = "unknown", source_dir: Path | None = None,
              data_only: bool = False):
    if chunk_size < 1:
        raise BuildError("--chunk-size must be at least 1")
    source_dir = (source_dir or Path(__file__).resolve().parent).resolve()
    db = require_file(db_path, "database")
    worlds = require_dir(worlds_path, "worlds directory")
    spheres = require_file(spheres_path, "spheres file") if spheres_path else None
    inputs = {"database": db, "worlds": worlds}
    if spheres is not None:
        inputs["spheres"] = spheres
    output = validate_output_path(output_path, inputs, source_dir)

    plan_module = load_plan_module()
    db_hash = sha256_file(db)
    worlds_hash, worlds_files, worlds_bytes = sha256_tree(worlds)
    spheres_info = None
    if spheres is not None:
        spheres_info = {
            "file": spheres.name,
            "sha256": sha256_file(spheres),
            "bytes": spheres.stat().st_size,
        }
    provenance = git_provenance(source_dir, code_commit)

    conn = open_readonly(db)
    try:
        try:
            plan_result, sphere_stats = load_plan_result(
                conn, worlds, spheres, plan_module
            )
            catalog = build_catalog(conn, plan_result, plan_module, chunk_size)
            contexts_total = conn.execute(
                "SELECT COUNT(*) FROM quest_contexts"
            ).fetchone()[0]
            excluded = conn.execute(
                "SELECT COUNT(*) FROM quest_contexts WHERE category_id = ?",
                (TUTORIAL_CATEGORY,),
            ).fetchone()[0]
        except sqlite3.Error as error:
            raise BuildError(f"database error: {error}") from error
    finally:
        conn.close()

    status_counts = Counter(quest["status"] for quest in catalog["quests"])
    contexts = {
        "total": contexts_total,
        "loaded": plan_result["summary"]["quests"],
        "excluded_tutorial_category": TUTORIAL_CATEGORY,
        "excluded": excluded,
    }
    if contexts["loaded"] + contexts["excluded"] != contexts["total"]:
        raise BuildError(
            "context accounting mismatch: "
            f"{contexts['loaded']} loaded + {contexts['excluded']} excluded "
            f"!= {contexts['total']} total"
        )
    generator = {
        "name": GENERATOR_NAME,
        "version": GENERATOR_VERSION,
        "code_commit": provenance["commit"],
        "code_commit_source": provenance["source"],
        "code_dirty": provenance["dirty"],
    }
    coordinate_provenance = {
        "kind": "1.2 Data/Worlds spawn coordinates",
        "coordinate_source": "1.2-worlds-candidate",
        "candidate": True,
        "classic_coordinates_verified": False,
        "unverified_label_required": True,
        "worlds_sha256": worlds_hash,
        "worlds_files": worlds_files,
        "worlds_bytes": worlds_bytes,
        "spheres": sphere_stats,
    }
    notices = {
        "coordinates": COORDINATE_NOTICE,
        "text": TEXT_NOTICE,
        "id_space": ID_SPACE_NOTICE,
        "rewards": REWARD_WARNING,
        "route": ROUTE_WARNING,
        "expected_vs_observed": (
            "The catalog is expected 1.2 static data. ArcheAge Classic values "
            "are observed at runtime and must not be written back as 1.2 truth."
        ),
    }
    manifest = {
        "schema": SCHEMA,
        "schema_version": SCHEMA_VERSION,
        "data_version": DATA_VERSION,
        "expected_data_version": DATA_VERSION,
        "generator": generator,
        "inputs": {
            "db": {
                "file": db.name,
                "sha256": db_hash,
                "bytes": db.stat().st_size,
            },
            "worlds": {
                "dir": worlds.name,
                "sha256": worlds_hash,
                "files": worlds_files,
                "bytes": worlds_bytes,
            },
            "spheres": spheres_info,
        },
        "coordinate_provenance": coordinate_provenance,
        "observed_environment": {
            "server": "ArcheAge Classic",
            "build": classic_build or "unknown",
            "values_source": "observed at runtime only; not 1.2 truth",
        },
        "catalog": {
            "contexts": contexts,
            "quests": len(catalog["quests"]),
            "status": dict(sorted(status_counts.items())),
            "chunk_size": chunk_size,
            "chunks": len(catalog["chunks"]),
        },
        "route": catalog["route"],
        "notices": notices,
        "files": [],
    }
    if timestamp:
        manifest["generated_at"] = datetime.now(timezone.utc).isoformat()
    meta_text = lua_header(
        "Provenance and catalog metadata for runtime use."
    ) + "return " + lua_literal(build_meta(manifest)) + "\n"
    files = dict(catalog["files"])
    files["meta"] = meta_text
    summary = summarize(catalog, plan_result, manifest)
    if timestamp:
        summary["generated_at"] = manifest["generated_at"]

    data_dir = output / "aaemu_quest_guide" / "data"
    manifest, summary = write_output(output, files, manifest, summary, data_dir)
    if not data_only:
        import package_addon
        try:
            manifest, summary, _zip = package_addon.package_addon(output, source_dir)
        except package_addon.PackageError as error:
            raise BuildError(str(error)) from error
    return manifest, summary


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--db", type=Path, required=True,
                        help="compact.sqlite3 (read-only)")
    parser.add_argument("--worlds", type=Path, required=True,
                        help="Data/Worlds directory (read-only)")
    parser.add_argument("--output", type=Path, required=True,
                        help="output directory; must not overlap inputs or the tool source")
    parser.add_argument("--spheres", type=Path,
                        help="optional quest-sign-sphere export (read-only)")
    parser.add_argument("--chunk-size", type=int, default=DEFAULT_CHUNK_SIZE,
                        help=f"quests per lazy Lua chunk (default {DEFAULT_CHUNK_SIZE})")
    parser.add_argument("--code-commit",
                        help="override the git commit recorded in provenance")
    parser.add_argument("--timestamp", action="store_true",
                        help="include the current UTC time in manifest/summary")
    parser.add_argument("--classic-build", default="unknown",
                        help="observed Classic build label; default unknown")
    parser.add_argument("--data-only", action="store_true",
                        help="write only the generated data catalog (no runtime/ZIP)")
    args = parser.parse_args(argv)
    try:
        manifest, summary = run_build(
            args.db, args.worlds, args.output, args.spheres,
            chunk_size=args.chunk_size, code_commit=args.code_commit,
            timestamp=args.timestamp, classic_build=args.classic_build,
            data_only=args.data_only,
        )
    except BuildError as error:
        print("error: " + str(error), file=sys.stderr)
        return 2
    print(json.dumps({
        "output": str(args.output.resolve()),
        "quests": summary["quests"],
        "status": summary["status"],
        "chunks": summary["chunks"]["count"],
        "files": summary["files"],
        "package": summary.get("package"),
        "code_commit": manifest["generator"]["code_commit"],
        "data_version": summary["data_version"],
    }, ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
