"""Player-mode quest ordering for a race and a level range.

The quest bot's normal plan (``plan.py``) is a static per-quest step list.  A
player, however, does not do every quest: the race story chain comes first, the
prologue is played once at character creation, and side quests are picked up
only when the character is close to their level and their starter NPC is in the
current or a neighbouring zone.  This tool builds that order from
``compact.sqlite3`` plus the world spawn files and writes:

* ``--output``: a JSON array of plan quests in play order.  The array is the
  same shape ``plan.py`` writes (``quest_id``, ``name``, ``category_id``,
  ``level``, ``min_level``, ``prereq_quests``, ``requires``, ``steps``,
  ``status``, ``blockers``) so ``aaemu-testclient quest --plan <file>`` can load
  it directly.  Each entry also carries informational fields the runner
  ignores: ``order``, ``source`` (``prologue`` / ``story`` / ``side``),
  ``zone_id``, ``chapter_idx``, ``quest_idx``, ``xp``, ``level_before``,
  ``level_after``, ``start_kind``, ``start_npcs``, ``start_locations`` and
  ``distance_m``.
* ``--report`` (optional): the ordering summary: assumptions, scheduled counts,
  every quest that can never become available in the requested window and why,
  the side quests without a player-usable starter, and the tutorial contexts the
  server loader deliberately skips.
* ``--listing`` (optional, otherwise stdout): a readable numbered listing.

Race story categories (``quest_contexts.category_id``), from ``player-mode``
design note §3 and the race quest names (``[종족 퀘스트]``):

======================  ======  ==========  =================================
race                    race_id  category   note
======================  ======  ==========  =================================
Nuian                   1        3           default
Elf                     4        8
Firran / Ferre          6        61
Harani / Harihara       5        62
Dwarf                   3        93          not playable in 1.2, data only
======================  ======  ==========  =================================

``tutorial`` is category 45.  The server loader skips it
(``systems/quests/data.rs::QUEST_CATEGORY_TUTORIAL``), so this tool never puts
it in the play order; the report lists the skipped contexts.  The prologue is
category 108 and is ordered first for a new character (it is a separate
instance).  Category 28 is the tutorial reward/support records and is skipped
like the tutorial.

Ordering rules
--------------

* The race story chain is ordered by ``(chapter_idx, quest_idx, id)`` and limited
  to ``LEVEL <= --max-level``.  The prologue is put in front (topological order
  of its prerequisite edges; race missions of other races are removed).
* A quest is acceptable when at least one ``Start`` component's requirements
  pass (the server's ``starter_accepts``).  ``unit_reqs`` are evaluated per
  component: ``or_unit_reqs`` turns a component's requirements into an OR group;
  without it all of them must pass.  Level (kind 1, min/max), race (kind 3),
  gender (kind 4, only when ``--gender`` is given), max-level (kind 0x45),
  completed quest (kind 0x1F), except-completed (0x24) and pre-completed (0x25)
  are evaluated; other kinds (items, abilities, faction, zone, ...) are treated
  as satisfiable by the player and recorded as unverified in the report.
  ``QuestActConAcceptLevelUp`` adds its level to the component's level demand.
* Side quests are every context outside the race story, prologue and tutorial
  categories.  A side quest is eligible when:

  - it has a runnable ``Start`` component whose starter is an NPC
    (``QuestActConAcceptNpc`` and its Kill / Emotion variants);
  - its ``LEVEL`` is at most ``current level + --side-level-margin`` (2 by
    default) and at most ``--max-level + margin``;
  - its ``quest_contexts.zone_id`` is the current zone or a neighbouring one.

  Eligible side quests are taken in ``(LEVEL, distance, quest_id)`` order, where
  distance is from the start NPC of the last scheduled quest to the nearest
  spawn of the side quest's start NPC.  A side quest that would satisfy another
  currently eligible side quest's ``ExceptCompleteQuestContext`` requirement is
  deferred until that quest has been taken (quest 330 is lost after 6198, so
  6198 waits while 330 is on the table).  Side quests are harvested after every
  story quest (and repeatedly while a story quest is waiting for a level),
  which is how a player picks up the local side content.

Zones and distances
-------------------

``quest_contexts.zone_id`` is a ``zones.id``.  Adjacency is approximated from
``zone_groups`` because the compact database has no zone polygons: two zones are
neighbours when they share a zone group or their group rectangles are within
``--neighbor-distance`` metres (default 4000).  Spawn positions come from the
same world files ``plan.py`` uses (``Data/Worlds``); a quest with no static
spawn still gets a location list from the plan.

XP and the level simulation
---------------------------

The character level is simulated from the quest rewards only:
``quest_act_supply_exps.exp`` summed per quest, with level thresholds from
``levels.total_exp`` (cumulative).  ``--level`` is the starting level and its
``levels.total_exp`` is the starting XP.  Kill XP and other sources are
**ignored by default**; ``--kill-xp N`` adds a fixed estimate per completed quest
for sensitivity runs.  The assumption is printed in the listing and report.
Because most 1.2 side quests grant no ``QuestActSupplyExp`` at all, the default
simulation usually stays at the starting level; that is a data fact, not an
error, and the report says so.

The tool never imports anything writable: the database and world files are
opened read-only.

Usage::

    python player_order.py --db compact.sqlite3 --worlds Data/Worlds \\
        --race nuian --level 1 --max-level 15 \\
        --output player-plan.json --report player-plan-report.json \\
        --listing player-plan.txt

"""

import argparse
from collections import defaultdict
import heapq
import json
import math
from pathlib import Path
import sqlite3
import sys

import plan as quest_plan


# The loader's tutorial categories.  Category 45 is skipped by
# `systems/quests/data.rs::QUEST_CATEGORY_TUTORIAL`; category 28 holds the
# tutorial reward/support definitions (Quest-Audit: "auxiliary" contexts).
TUTORIAL_CATEGORY = 45
TUTORIAL_SUPPORT_CATEGORY = 28
PROLOGUE_CATEGORY = 108

KIND_START = 2

# `systems/quests/unit_reqs.rs::req_kind` values this tool evaluates.
REQ_LEVEL = 0x01
REQ_RACE = 0x03
REQ_GENDER = 0x04
REQ_COMPLETE_QUEST = 0x1F
REQ_EXCEPT_COMPLETE_QUEST = 0x24
REQ_PRE_COMPLETE_QUEST = 0x25
REQ_MAX_LEVEL = 0x45

# `--race` -> (display name, characters.race id, story category).
RACES = {
    "nuian": ("Nuian", 1, 3),
    "elf": ("Elf", 4, 8),
    "firran": ("Firran (Ferre)", 6, 61),
    "ferre": ("Firran (Ferre)", 6, 61),
    "harani": ("Harani (Harihara)", 5, 62),
    "harihara": ("Harani (Harihara)", 5, 62),
    "hariharan": ("Harani (Harihara)", 5, 62),
    "dwarf": ("Dwarf", 3, 93),
}

# Start acts that make the player walk to an NPC (the only side-quest starters
# this ordering understands).
NPC_START_ACTS = {
    "QuestActConAcceptNpc",
    "QuestActConAcceptNpcKill",
    "QuestActConAcceptNpcEmotion",
}

# Start acts used for the `start_kind` annotation.  `QuestActConAcceptNpcKill`
# and `...Emotion` are accepted at the NPC (LIVE-TESTING.md).
START_ACT_KINDS = {
    **{name: "npc" for name in NPC_START_ACTS},
    "QuestActConAcceptDoodad": "doodad",
    "QuestActConAcceptItem": "item",
    "QuestActConAcceptItemGain": "item",
    "QuestActConAcceptItemEquip": "item",
    "QuestActConAcceptSphere": "sphere",
    "QuestActConAcceptSkill": "skill",
    "QuestActConAcceptBuff": "buff",
    "QuestActConAcceptLevelUp": "level_up",
    "QuestActConAutoComplete": "auto",
    "QuestActConAcceptComponent": "component",
}

# Detail tables for the starter acts (`data.rs::load_detail_acts`).
START_DETAIL_TABLES = {
    "npc": "quest_act_con_accept_npcs",
    "doodad": "quest_act_con_accept_doodads",
    "item": "quest_act_con_accept_items",
    "sphere": "quest_act_con_accept_spheres",
    "skill": "quest_act_con_accept_skills",
    "buff": "quest_act_con_accept_buffs",
    "level_up": "quest_act_con_accept_level_ups",
    "component": "quest_act_con_accept_components",
}

# Detail value column per starter kind.
START_DETAIL_COLUMNS = {
    "npc": "npc_id",
    "doodad": "doodad_id",
    "item": "item_id",
    "sphere": "sphere_id",
    "skill": "skill_id",
    "buff": "buff_id",
    "level_up": "LEVEL",
    "component": "quest_context_id",
}

# ---------------------------------------------------------------------------
# Small helpers


def rows_if(conn, table):
    """Rows of an optional table (fixtures often omit detail tables)."""
    try:
        return quest_plan.rows(conn, table)
    except sqlite3.OperationalError:
        return []


def int_or(value, default=0):
    return value if isinstance(value, int) else default


def text_of(row, column, default=""):
    value = row.get(column)
    return value if isinstance(value, str) else default


def level_of(ctx):
    """The context's intended level; null / 0 means "no level"."""
    value = ctx.get("LEVEL")
    return value if isinstance(value, int) and value > 0 else None


def race_applicable(ctx, components, race_id):
    """True when at least one Start component has no race gate or a matching one.

    Only the race requirements are inspected here; prerequisite edges are
    handled separately so the prologue can be filtered before it is ordered.
    """
    comps = components.get(ctx["id"], [])
    if not comps:
        return True
    for comp in comps:
        race_reqs = [r for r in comp["_reqs"] if r["kind_id"] == REQ_RACE]
        if not race_reqs:
            return True
        if comp["_or"]:
            if any(r["value1"] == race_id for r in race_reqs):
                return True
        elif all(r["value1"] == race_id for r in race_reqs):
            return True
    return False


# ---------------------------------------------------------------------------
# Data loading


def load_start_data(conn):
    """Per-quest Start components with requirements, starters and details.

    ``components[qid]`` is the list of Start components (component id order).
    Each component has ``_reqs`` (``unit_reqs`` rows, owner type
    QuestComponent), ``_or`` (``or_unit_reqs``), ``_starts`` (classified
    starter acts), ``_level_up`` (the level of a ``ConAcceptLevelUp`` starter,
    or 0), ``_npc_ids`` and ``_min_level``.
    """
    reqs = defaultdict(list)
    for row in quest_plan.rows(conn, "unit_reqs"):
        if row.get("owner_type") == "QuestComponent":
            reqs[row.get("owner_id")].append(row)

    components = defaultdict(list)
    for row in quest_plan.rows(conn, "quest_components"):
        if row.get("component_kind_id") == KIND_START:
            components[row.get("quest_context_id")].append(row)

    acts = defaultdict(list)
    for row in quest_plan.rows(conn, "quest_acts"):
        acts[row.get("quest_component_id")].append(row)

    details = {}
    for kind, table in START_DETAIL_TABLES.items():
        details[kind] = {r["id"]: r for r in rows_if(conn, table)}

    for qid in list(components):
        for comp in components[qid]:
            comp["_reqs"] = list(reqs.get(comp["id"], []))
            comp["_or"] = quest_plan.truth(comp.get("or_unit_reqs"))
            starts = []
            for act in acts.get(comp["id"], []):
                act_type = act.get("act_detail_type")
                kind = START_ACT_KINDS.get(act_type)
                if kind is None:
                    continue
                detail = details.get(kind, {}).get(act.get("act_detail_id"))
                value = None
                column = START_DETAIL_COLUMNS.get(kind)
                if detail is not None and column:
                    value = detail.get(column)
                starts.append({"act_type": act_type, "kind": kind, "value": value})
            comp["_starts"] = starts
            comp["_level_up"] = max(
                (int_or(s["value"]) for s in starts if s["kind"] == "level_up"),
                default=0,
            )
            comp["_npc_ids"] = [
                s["value"] for s in starts if s["kind"] == "npc" and s["value"]
            ]
            comp["_min_level"] = max(
                (r["value1"] for r in comp["_reqs"] if r["kind_id"] == REQ_LEVEL),
                default=1,
            )
            comp["_min_level"] = max(comp["_min_level"], comp["_level_up"])
    return components


def load_levels(conn):
    rows = []
    for row in rows_if(conn, "levels"):
        if isinstance(row.get("id"), int) and isinstance(row.get("total_exp"), int):
            rows.append(row)
    return sorted(rows, key=lambda r: r["id"])


def load_zone_data(conn):
    zones = {}
    for row in rows_if(conn, "zones"):
        if isinstance(row.get("id"), int):
            zones[row["id"]] = row.get("group_id")
    groups = {}
    for row in rows_if(conn, "zone_groups"):
        if not isinstance(row.get("id"), int):
            continue
        groups[row["id"]] = (
            float(row.get("x") or 0.0),
            float(row.get("y") or 0.0),
            float(row.get("w") or 0.0),
            float(row.get("h") or 0.0),
        )
    return zones, groups


def load_spawns(conn, worlds):
    """World spawns exactly like ``plan.py::main``; read-only."""
    ids = {
        "npc": {row["id"] for row in rows_if(conn, "npcs")},
        "doodad": {row["id"] for row in rows_if(conn, "doodad_almighties")},
    }
    spawner_npcs = [(row["npc_spawner_id"], row["member_id"])
                    for row in rows_if(conn, "npc_spawner_npcs")]
    schedule_spawners = defaultdict(list)
    for row in rows_if(conn, "game_schedule_spawners"):
        schedule_spawners[row["spawner_id"]].append(row["game_schedule_id"])
    schedule_doodads = defaultdict(list)
    for row in rows_if(conn, "game_schedule_doodads"):
        schedule_doodads[row["doodad_id"]].append(row["game_schedule_id"])
    windows = {row["id"]: (row["startTime"], row["endTime"])
               for row in rows_if(conn, "npc_spawners")
               if (row.get("startTime") or 0) > 0 or (row.get("endTime") or 0) > 0}
    known_schedules = {row["id"] for row in rows_if(conn, "game_schedules")}
    return quest_plan.load_spawns(worlds, ids, spawner_npcs, schedule_spawners,
                                  schedule_doodads, windows, known_schedules)


def load_quest_xp(conn):
    """``quest_id -> exp`` from only ``QuestActSupplyExp`` detail rows."""
    xp = defaultdict(int)
    exp_rows = {r["id"]: r.get("exp") for r in rows_if(conn, "quest_act_supply_exps")}
    components = {row["id"]: row for row in quest_plan.rows(conn, "quest_components")}
    for act in quest_plan.rows(conn, "quest_acts"):
        if act.get("act_detail_type") != "QuestActSupplyExp":
            continue
        component = components.get(act.get("quest_component_id"))
        if not component:
            continue
        value = exp_rows.get(act.get("act_detail_id"))
        if isinstance(value, int):
            xp[component["quest_context_id"]] += value
    return xp


# ---------------------------------------------------------------------------
# Requirements and quest state


def requirement_passes(req, level, completed, race_id, gender):
    """(passes, known) for one ``unit_reqs`` row.

    Unknown kinds are reported as ``known=False`` and treated as passing: the
    ordering cannot prove an item / ability / faction gate impossible.
    """
    kind = req.get("kind_id")
    value1 = int_or(req.get("value1"))
    value2 = int_or(req.get("value2"))
    if kind == REQ_LEVEL:
        return level >= value1 and (value2 == 0 or level <= value2), True
    if kind == REQ_RACE:
        return value1 == race_id, True
    if kind == REQ_GENDER:
        return gender is None or value1 == gender, True
    if kind == REQ_MAX_LEVEL:
        return level <= value1, True
    if kind == REQ_COMPLETE_QUEST:
        return value1 in completed, True
    if kind == REQ_EXCEPT_COMPLETE_QUEST:
        return value1 not in completed, True
    if kind == REQ_PRE_COMPLETE_QUEST:
        return False, True
    return True, False


def evaluate_component(comp, level, completed, race_id, gender):
    """Evaluate one Start component; the server's ``CanComponentRun``."""
    failed = []
    unverified = []
    if comp["_or"]:
        if comp["_reqs"]:
            results = [requirement_passes(r, level, completed, race_id, gender)
                       for r in comp["_reqs"]]
            if not any(passed for passed, _ in results):
                failed = [r for r, (passed, _) in zip(comp["_reqs"], results)
                          if not passed]
        # An OR group with no requirements runs unconditionally.
    else:
        for req in comp["_reqs"]:
            passed, known = requirement_passes(req, level, completed, race_id, gender)
            if not passed:
                failed.append(req)
            if not known:
                unverified.append(req)
    level_ok = level >= comp["_level_up"] if comp["_level_up"] else True
    return {
        "runnable": not failed and level_ok,
        "failed": failed,
        "unverified": unverified,
        "level_gate": comp["_level_up"] or max(
            (r["value1"] for r in comp["_reqs"] if r["kind_id"] == REQ_LEVEL),
            default=1,
        ),
    }


def quest_state(ctx, components, level, completed, race_id, gender, spawns):
    """Runnable state of a quest plus the info the orderer needs.

    A quest is acceptable when any Start component is runnable (the server's
    ``starter_accepts``).  ``start_kind`` / ``start_npcs`` come from the
    runnable components; when none is runnable they describe every starter so
    the report can still name the missing NPCs.
    """
    comps = components.get(ctx["id"], [])
    if not comps:
        return {
            "runnable": False,
            "reason": "no_start_component",
            "start_kind": "none",
            "start_npcs": [],
            "prereqs": [],
            "requires": [],
            "level_gate": None,
            "failed": [],
            "unverified": [],
            "npc_ids_all": [],
        }
    evaluations = [(comp, evaluate_component(comp, level, completed, race_id, gender))
                   for comp in comps]
    runnable = [(comp, ev) for comp, ev in evaluations if ev["runnable"]]
    chosen = runnable or evaluations

    starts = [s for comp, _ in chosen for s in comp["_starts"]]
    kinds = [s["kind"] for s in starts]
    # Prefer the starter the player can actually click; auto/component starts
    # are next (the server adds them itself).  A Start component without an
    # act is the engine's `Starter::Auto` fallback.
    priority = ["npc", "level_up", "auto", "component", "item", "doodad",
                "sphere", "skill", "buff"]
    start_kind = next((k for k in priority if k in kinds), "auto")
    start_npcs = sorted({s["value"] for s in starts
                         if s["kind"] == "npc" and s["value"]})
    npc_ids_all = sorted({s["value"] for comp, _ in evaluations
                          for s in comp["_starts"]
                          if s["kind"] == "npc" and s["value"]})

    # `prereq_quests` must be the satisfied dependency set: the Rust runner
    # treats it as a hard list.  In an OR group only the satisfied alternatives
    # count (the prologue's 6043 requires *one* race mission, not all four).
    prereqs = set()
    requires = []
    for comp, _ in chosen:
        complete_reqs = [r for r in comp["_reqs"]
                         if r["kind_id"] == REQ_COMPLETE_QUEST]
        if comp["_or"]:
            prereqs.update(r["value1"] for r in complete_reqs
                           if r["value1"] in completed)
        else:
            prereqs.update(r["value1"] for r in complete_reqs)
        for req in comp["_reqs"]:
            entry = {"component_id": comp["id"], "kind_id": req["kind_id"],
                     "value1": req["value1"], "value2": req["value2"]}
            if comp["_or"]:
                entry["or_group"] = True
            requires.append(entry)
    prereqs = sorted(prereqs)

    if runnable:
        reason = None
        level_gate = min(ev["level_gate"] for _, ev in runnable)
    else:
        level_gate = min((ev["level_gate"] for _, ev in evaluations), default=None)
        all_failed = [r for _, ev in evaluations for r in ev["failed"]]
        if any(r["kind_id"] == REQ_RACE for r in all_failed):
            reason = "race_mismatch"
        elif all_failed and all(r["kind_id"] == REQ_LEVEL for r in all_failed):
            reason = "level_gate"
        elif all_failed and any(r["kind_id"] == REQ_COMPLETE_QUEST for r in all_failed):
            reason = "prereq_missing"
        elif all_failed and any(r["kind_id"] == REQ_EXCEPT_COMPLETE_QUEST
                                for r in all_failed):
            reason = "excluded_by_completed"
        elif any(ev["level_gate"] > level for _, ev in evaluations):
            reason = "level_gate"
        else:
            reason = "requirements_unmet"

    return {
        "runnable": bool(runnable),
        "reason": reason,
        "start_kind": start_kind,
        "start_npcs": start_npcs,
        "npc_ids_all": npc_ids_all,
        "prereqs": prereqs,
        "requires": requires,
        "level_gate": level_gate,
        "failed": [r for _, ev in evaluations for r in ev["failed"]],
        "unverified": [r for _, ev in evaluations for r in ev["unverified"]],
        "start_locations": [p for npc in start_npcs
                            for p in spawns.get("npc", {}).get(npc, [])],
    }


# ---------------------------------------------------------------------------
# Zones, positions and levels


def box_distance(a, b):
    ax, ay, aw, ah = a
    bx, by, bw, bh = b
    dx = max(0.0, max(ax - (bx + bw), bx - (ax + aw)))
    dy = max(0.0, max(ay - (by + bh), by - (ay + ah)))
    return math.hypot(dx, dy)


def neighbours_of(zone_id, zones, groups, distance, cache):
    if zone_id in cache:
        return cache[zone_id]
    group = zones.get(zone_id)
    if group is None or group not in groups:
        cache[zone_id] = {zone_id}
        return cache[zone_id]
    box = groups[group]
    result = {zone_id}
    for other, other_group in zones.items():
        if other_group is None:
            continue
        if other_group == group:
            result.add(other)
        elif other_group in groups and box_distance(box, groups[other_group]) <= distance:
            result.add(other)
    cache[zone_id] = result
    return result


def position_distance(a, b):
    if a is None or b is None:
        return float("inf")
    if a[0] != b[0]:
        return float("inf")
    return math.sqrt((a[1] - b[1]) ** 2 + (a[2] - b[2]) ** 2 + (a[3] - b[3]) ** 2)


def nearest_position(locations, anchor):
    """The placement closest to ``anchor`` (first one without an anchor)."""
    if not locations:
        return None
    if anchor is None:
        return locations[0]
    return min(locations, key=lambda p: position_distance(p, anchor))


def level_for_xp(xp, levels):
    level = 1
    for row in levels:
        if row["total_exp"] <= xp:
            level = max(level, row["id"])
        else:
            break
    return level


def xp_for_level(level, levels):
    for row in levels:
        if row["id"] == level:
            return row["total_exp"]
    return 0


# ---------------------------------------------------------------------------
# Ordering


class Orderer:
    """Stateful player-order builder (also used by the unit tests)."""

    def __init__(self, conn, spawns, race_name, start_level=1, max_level=15,
                 side_margin=2, neighbor_distance=4000.0, kill_xp=0,
                 gender=None, skip_prologue=False, assume_previous=True,
                 plan_quests=None):
        self.conn = conn
        self.spawns = spawns
        self.race_name = race_name
        self.race_label, self.race_id, self.story_category = RACES[race_name]
        self.start_level = start_level
        self.max_level = max_level
        self.side_margin = side_margin
        self.neighbor_distance = neighbor_distance
        self.kill_xp = kill_xp
        self.gender = gender
        self.skip_prologue = skip_prologue
        self.assume_previous = assume_previous
        self.plan_quests = plan_quests or {}

        self.contexts = {row["id"]: row for row in quest_plan.rows(conn, "quest_contexts")}
        self.components = load_start_data(conn)
        self.levels = load_levels(conn)
        self.xp_by_quest = load_quest_xp(conn)
        self.zones, self.groups = load_zone_data(conn)
        self.zone_cache = {}

        self.completed = set()
        self.scheduled = []
        self.scheduled_ids = set()
        self.side_scheduled = set()
        self.level = start_level
        self.xp_total = xp_for_level(start_level, self.levels)
        self.current_zone = None
        self.current_anchor = None
        self.prologue = []
        self.story = []
        self.side_candidates = []
        self.not_applicable = []
        self.prologue_blocked = []
        self.story_blocked = []
        self.story_out_of_range = []
        self.side_blocked = []
        self.visited_zones = set()
        self.assumed = set()
        self._prepare_chains()

    # -- chain preparation -------------------------------------------------

    def _prepare_chains(self):
        # Prologue: race-applicable contexts, ordered by their prerequisite DAG.
        prologue_ctxs = [
            ctx for ctx in sorted(self.contexts.values(), key=lambda c: c["id"])
            if ctx.get("category_id") == PROLOGUE_CATEGORY
        ]
        applicable = []
        for ctx in prologue_ctxs:
            if race_applicable(ctx, self.components, self.race_id):
                applicable.append(ctx)
            else:
                self.not_applicable.append(ctx["id"])
        by_id = {ctx["id"]: ctx for ctx in applicable}
        deps = {}
        for ctx in applicable:
            prereqs = set()
            for comp in self.components.get(ctx["id"], []):
                prereqs.update(r["value1"] for r in comp["_reqs"]
                               if r["kind_id"] == REQ_COMPLETE_QUEST)
            deps[ctx["id"]] = {p for p in prereqs if p in by_id and p != ctx["id"]}
        self.prologue = self._topological(applicable, deps)

        # Story: race category, chapter/quest order, within the content level cap.
        story = [
            ctx for ctx in self.contexts.values()
            if ctx.get("category_id") == self.story_category
        ]
        story.sort(key=lambda c: (int_or(c.get("chapter_idx")),
                                  int_or(c.get("quest_idx")), c["id"]))
        for ctx in story:
            level = level_of(ctx)
            if level is not None and level > self.max_level:
                self.story_out_of_range.append(ctx["id"])
                continue
            if (self.assume_previous and self.start_level > 1
                    and level is not None and level < self.start_level):
                self.assumed.add(ctx["id"])
                self.completed.add(ctx["id"])
                continue
            self.story.append(ctx)

        # Side quests: everything outside the campaign and tutorial categories,
        # within the level window.  Zone and starter checks happen at pick time.
        self.side_candidates = [
            ctx for ctx in self.contexts.values()
            if ctx.get("category_id") not in (
                TUTORIAL_CATEGORY, TUTORIAL_SUPPORT_CATEGORY,
                PROLOGUE_CATEGORY, self.story_category,
            )
            and level_of(ctx) is not None
            and level_of(ctx) <= self.max_level + self.side_margin
        ]

        # If the run starts above level 1 (or the prologue is skipped), the
        # prologue is in the past: mark it completed instead of replaying it.
        if self.skip_prologue or self.start_level > 1:
            self.assumed.update(ctx["id"] for ctx in self.prologue)
            self.completed.update(ctx["id"] for ctx in self.prologue)

    @staticmethod
    def _topological(contexts, deps):
        """Stable Kahn order (ties by quest id); cycles go last, by id."""
        remaining = {ctx["id"]: set(deps.get(ctx["id"], set())) for ctx in contexts}
        by_id = {ctx["id"]: ctx for ctx in contexts}
        ready = [ctx_id for ctx_id, need in remaining.items() if not need]
        heapq.heapify(ready)
        ordered = []
        while ready:
            ctx_id = heapq.heappop(ready)
            ordered.append(by_id[ctx_id])
            del remaining[ctx_id]
            for other, need in remaining.items():
                if ctx_id in need:
                    need.remove(ctx_id)
                    if not need:
                        heapq.heappush(ready, other)
        ordered_ids = {ctx["id"] for ctx in ordered}
        ordered.extend(sorted((ctx for ctx in contexts if ctx["id"] not in ordered_ids),
                              key=lambda c: c["id"]))
        return ordered

    # -- scheduling --------------------------------------------------------

    def _distance_to_anchor(self, locations):
        if not locations or self.current_anchor is None:
            return 0.0 if locations else float("inf")
        return min(position_distance(p, self.current_anchor) for p in locations)

    def _npc_locations(self, npc_ids):
        return [placement for npc in npc_ids
                for placement in self.spawns.get("npc", {}).get(npc, [])]

    def _zone_neighbours(self):
        if self.current_zone is None:
            return None
        return neighbours_of(self.current_zone, self.zones, self.groups,
                             self.neighbor_distance, self.zone_cache)

    def _zone_ok(self, ctx, locations):
        zone = ctx.get("zone_id")
        neighbours = self._zone_neighbours()
        if neighbours is not None and isinstance(zone, int) and zone in self.zones:
            return zone in neighbours
        # No zone id / unknown zone: fall back to the starter's distance.
        if not locations or self.current_anchor is None:
            return True
        return self._distance_to_anchor(locations) <= max(self.neighbor_distance * 2, 1.0)

    def _exclusive_side_edges(self):
        """``excluded quest -> quests whose ExceptComplete requirement blocks it``."""
        edges = defaultdict(set)
        wanted = {ctx["id"] for ctx in self.side_candidates}
        for ctx in self.side_candidates:
            for comp in self.components.get(ctx["id"], []):
                for req in comp["_reqs"]:
                    if (req["kind_id"] == REQ_EXCEPT_COMPLETE_QUEST
                            and req["value1"] in wanted):
                        edges[req["value1"]].add(ctx["id"])
        return edges

    def _eligible_side(self):
        candidates = []
        for ctx in self.side_candidates:
            qid = ctx["id"]
            if qid in self.scheduled_ids:
                continue
            level = level_of(ctx)
            if level > self.level + self.side_margin:
                continue
            state = quest_state(ctx, self.components, self.level, self.completed,
                                self.race_id, self.gender, self.spawns)
            if not state["runnable"]:
                continue
            if not state["start_npcs"]:
                # Only player-clickable side starters make the order.
                continue
            locations = self._npc_locations(state["start_npcs"])
            if not self._zone_ok(ctx, locations):
                continue
            candidates.append({
                "ctx": ctx,
                "state": state,
                "distance": self._distance_to_anchor(locations),
            })
        # Do not take a quest that would satisfy another eligible quest's
        # ExceptComplete requirement first: 330 ("나를 찾는 사람") is lost
        # after 6198, so 6198 is deferred while 330 is still on the table.
        candidate_ids = {c["ctx"]["id"] for c in candidates}
        edges = self._exclusive_side_edges()
        def blocks_other(candidate):
            return bool(edges.get(candidate["ctx"]["id"], set()) & candidate_ids)
        candidates.sort(key=lambda c: (level_of(c["ctx"]), blocks_other(c),
                                       c["distance"], c["ctx"]["id"]))
        return candidates

    def _next_zone(self, ctx, locations):
        zone = ctx.get("zone_id")
        if isinstance(zone, int) and zone in self.zones:
            self.current_zone = zone
            self.visited_zones.add(zone)
            self.visited_zones.update(neighbours_of(
                zone, self.zones, self.groups, self.neighbor_distance, self.zone_cache))
        anchor = nearest_position(locations, self.current_anchor)
        if anchor is not None:
            self.current_anchor = anchor

    def _schedule(self, ctx, source, state, distance=None):
        qid = ctx["id"]
        xp = self.xp_by_quest.get(qid, 0)
        level_before = self.level
        if distance is None:
            distance = (float("inf") if source == "prologue"
                        else self._distance_to_anchor(state["start_locations"]))
        self.xp_total += xp + self.kill_xp
        self.level = max(level_before, level_for_xp(self.xp_total, self.levels))
        entry = self._entry(ctx, source, state, distance, xp, level_before)
        self.scheduled.append(entry)
        self.scheduled_ids.add(qid)
        self.completed.add(qid)
        if source == "side":
            self.side_scheduled.add(qid)
        self._next_zone(ctx, state["start_locations"])
        return entry

    def _entry(self, ctx, source, state, distance, xp, level_before):
        base = dict(self.plan_quests.get(ctx["id"], {}))
        base.setdefault("quest_id", ctx["id"])
        base.setdefault("name", text_of(ctx, "name"))
        base.setdefault("category_id", ctx.get("category_id"))
        base.setdefault("level", level_of(ctx))
        base.setdefault("steps", [])
        base.setdefault("status", "ok")
        base.setdefault("blockers", [])
        # The player gate is the Start component's actual demand; the plan's
        # `min_level` also folds in the display LEVEL, which is not a server
        # requirement.  The ordered JSON keeps the real gate.
        base["min_level"] = state.get("level_gate")
        base["prereq_quests"] = state["prereqs"]
        base["requires"] = [
            dict(req, kind=quest_plan.REQ_KINDS.get(req["kind_id"],
                                                    f"unknown_{req['kind_id']}"))
            for req in state["requires"]
        ]
        base.update({
            "order": len(self.scheduled) + 1,
            "source": source,
            "zone_id": ctx.get("zone_id"),
            "chapter_idx": ctx.get("chapter_idx"),
            "quest_idx": ctx.get("quest_idx"),
            "xp": xp,
            "level_before": level_before,
            "level_after": self.level,
            "start_kind": state["start_kind"],
            "start_npcs": state["start_npcs"],
            "start_locations": [
                {"world": p[0], "x": p[1], "y": p[2], "z": p[3]}
                for p in state["start_locations"][:5]
            ],
            "distance_m": None if math.isinf(distance) else round(distance, 1),
        })
        if state["unverified"]:
            base["unverified_requirements"] = [
                {"kind_id": r["kind_id"], "value1": r["value1"],
                 "value2": r["value2"]} for r in state["unverified"]
            ]
        if base.get("status") not in ("ok", "auxiliary"):
            base["plan_note"] = f"plan status is {base.get('status')}"
        return base

    def _harvest_side(self):
        """Take every currently eligible side quest; repeat as XP opens more."""
        while True:
            candidates = self._eligible_side()
            if not candidates:
                return
            chosen = candidates[0]
            self._schedule(chosen["ctx"], "side", chosen["state"],
                           chosen["distance"])

    # -- main loop ---------------------------------------------------------

    def run(self):
        # Prologue first (unless it is in the past).
        if not (self.skip_prologue or self.start_level > 1):
            for ctx in self.prologue:
                state = quest_state(ctx, self.components, self.level, self.completed,
                                    self.race_id, self.gender, self.spawns)
                if state["runnable"]:
                    self._schedule(ctx, "prologue", state)
                else:
                    self.prologue_blocked.append({
                        "quest_id": ctx["id"],
                        "name": text_of(ctx, "name"),
                        "level": level_of(ctx),
                        "zone_id": ctx.get("zone_id"),
                        "reason": state["reason"] or "requirements_unmet",
                        "level_gate": state["level_gate"],
                    })

        # Story chain with side quests harvested after each story quest.
        index = 0
        while index < len(self.story):
            ctx = self.story[index]
            state = quest_state(ctx, self.components, self.level, self.completed,
                                self.race_id, self.gender, self.spawns)
            if state["runnable"]:
                self._schedule(ctx, "story", state)
                index += 1
                self._harvest_side()
                continue
            # The next story quest is blocked (usually by a level gate): let the
            # side quests provide the levels, then look again.
            before = len(self.scheduled)
            self._harvest_side()
            if len(self.scheduled) == before:
                self.story_blocked.append({
                    "quest_id": ctx["id"],
                    "name": text_of(ctx, "name"),
                    "level": level_of(ctx),
                    "zone_id": ctx.get("zone_id"),
                    "reason": state["reason"] or "requirements_unmet",
                    "level_gate": state["level_gate"],
                })
                for rest in self.story[index + 1:]:
                    self.story_blocked.append({
                        "quest_id": rest["id"],
                        "name": text_of(rest, "name"),
                        "level": level_of(rest),
                        "zone_id": rest.get("zone_id"),
                        "reason": "behind_blocker",
                        "blocked_by": ctx["id"],
                    })
                break
            # Retry the same story quest after the side XP.
        else:
            # Story chain completed: finish every side quest that is still open.
            self._harvest_side()

        return self._result()

    # -- result ------------------------------------------------------------

    def _classify_side_block(self, state, final_level):
        """(reason, never) for a side quest that did not make the order.

        ``never`` is true only when no amount of kill XP in this window can
        open the quest: wrong race, an absent prerequisite, a level above the
        data's level table, an exclusive alternative already taken, or no
        player-clickable starter.  A level that more XP can still reach and a
        prerequisite that is merely not done yet are ``pending``.
        """
        if state["runnable"] and not state["start_npcs"]:
            return "no_player_start_npc", True
        if state["runnable"]:
            return "not_reached", False
        reason = state["reason"] or "requirements_unmet"
        if reason == "race_mismatch":
            return "race_mismatch", True
        if reason == "excluded_by_completed":
            return "excluded_by_completed", True
        if reason == "prereq_missing":
            missing = [r["value1"] for r in state["failed"]
                       if r["kind_id"] == REQ_COMPLETE_QUEST]
            if any(qid not in self.contexts for qid in missing):
                return "prereq_absent", True
            return "prereq_pending", False
        if reason == "level_gate":
            max_table = max((row["id"] for row in self.levels), default=0)
            gate = state["level_gate"] or 1
            if max_table and gate > max_table:
                return "level_unreachable", True
            if gate > final_level:
                return "needs_level", False
            return "level_gate", False
        if reason == "requirements_unmet":
            # Unknown requirement kinds are treated as satisfiable, so this is
            # usually a pre-complete / active-quest condition the run can open.
            return "requirements_pending", False
        return reason, False

    def _side_report(self):
        """Why the side candidates did not make the order.

        Quests in zones the run never comes near (other continents) are not
        "never available" in this window; they are counted as out of scope.
        """
        never = []
        pending = []
        no_starter = []
        out_of_range = []
        out_of_zone = []
        above_level = []
        final_level = self.level
        for ctx in sorted(self.side_candidates, key=lambda c: (level_of(c), c["id"])):
            qid = ctx["id"]
            if qid in self.side_scheduled:
                continue
            level = level_of(ctx)
            if level > self.max_level + self.side_margin:
                out_of_range.append(qid)
                continue
            if level > final_level + self.side_margin:
                # More XP (kill XP, or a longer run) can still open it; it is
                # not "never available" under the data.
                above_level.append(qid)
                continue
            zone = ctx.get("zone_id")
            zone_known = isinstance(zone, int) and zone in self.zones
            if zone_known and self.visited_zones and zone not in self.visited_zones:
                out_of_zone.append(qid)
                continue
            state = quest_state(ctx, self.components, final_level, self.completed,
                                self.race_id, self.gender, self.spawns)
            if not state["start_npcs"] and not state["npc_ids_all"]:
                no_starter.append(qid)
                continue
            reason, is_never = self._classify_side_block(state, final_level)
            entry = {
                "quest_id": qid,
                "name": text_of(ctx, "name"),
                "reason": reason,
                "level": level,
                "zone_id": zone,
                "start_npcs": state["npc_ids_all"],
                "level_gate": state["level_gate"],
            }
            (never if is_never else pending).append(entry)
        return never, pending, no_starter, out_of_range, out_of_zone, above_level

    def _result(self):
        side_never, side_pending, no_starter, out_of_range, out_of_zone, above_level = (
            self._side_report()
        )
        plan_not_ok = [
            {"quest_id": q["quest_id"], "source": q["source"],
             "status": q.get("status"), "blockers": q.get("blockers", [])}
            for q in self.scheduled
            if q.get("status") not in ("ok", "auxiliary")
        ]
        summary = {
            "race": self.race_name,
            "race_label": self.race_label,
            "race_id": self.race_id,
            "story_category": self.story_category,
            "start_level": self.start_level,
            "max_level": self.max_level,
            "end_level": self.level,
            "end_xp": self.xp_total,
            "kill_xp_per_quest": self.kill_xp,
            "scheduled": {
                "total": len(self.scheduled),
                "prologue": sum(1 for q in self.scheduled if q["source"] == "prologue"),
                "story": sum(1 for q in self.scheduled if q["source"] == "story"),
                "side": sum(1 for q in self.scheduled if q["source"] == "side"),
            },
            "never_available": {
                "prologue": len(self.prologue_blocked),
                "story": len(self.story_blocked),
                "side": len(side_never),
                "side_without_player_starter": len(no_starter),
                "side_out_of_level_range": len(out_of_range),
                "side_out_of_zone": len(out_of_zone),
                "side_above_final_level": len(above_level),
                "story_out_of_level_range": len(self.story_out_of_range),
            },
            "not_scheduled": {
                "side": len(side_pending),
            },
            "scheduled_not_ok": len(plan_not_ok),
            "assumed_completed": sorted(self.assumed),
        }
        report = {
            "race": self.race_name,
            "race_label": self.race_label,
            "race_id": self.race_id,
            "story_category": self.story_category,
            "start_level": self.start_level,
            "max_level": self.max_level,
            "assumptions": [
                "Levels are simulated from quest XP only "
                "(QuestActSupplyExp.exp summed per quest, thresholds from "
                "levels.total_exp).",
                f"Kill XP is ignored; --kill-xp adds {self.kill_xp} XP per "
                "completed quest for sensitivity runs.",
                "Zone membership comes from quest_contexts.zone_id; adjacency "
                "is approximated from zone_groups rectangles "
                f"(same group or <= {self.neighbor_distance:g} m apart).",
                "Side quests are capped at --max-level + --side-level-margin.",
                "Requirements other than level/race/gender/max-level/quest "
                "completion are treated as satisfiable and listed as "
                "unverified.",
                "Tutorial category 45 is not loaded by the server "
                "(QUEST_CATEGORY_TUTORIAL) and is never scheduled.",
            ],
            "summary": summary,
            "scheduled": self.scheduled,
            "never_available": {
                "prologue": self.prologue_blocked,
                "story": self.story_blocked,
                "side": side_never,
            },
            "not_scheduled": {
                "side": side_pending,
            },
            "scheduled_not_ok": plan_not_ok,
            "excluded": {
                "prologue_other_race": sorted(self.not_applicable),
                "side_without_player_starter": sorted(no_starter),
                "side_out_of_level_range": sorted(out_of_range),
                "side_out_of_zone": sorted(out_of_zone),
                "side_above_final_level": sorted(above_level),
                "story_above_level_range": sorted(self.story_out_of_range),
                "tutorial_category": sorted(
                    ctx["id"] for ctx in self.contexts.values()
                    if ctx.get("category_id") == TUTORIAL_CATEGORY),
                "tutorial_support_category": sorted(
                    ctx["id"] for ctx in self.contexts.values()
                    if ctx.get("category_id") == TUTORIAL_SUPPORT_CATEGORY),
            },
            "assumed_completed": sorted(self.assumed),
        }
        return {"quests": self.scheduled, "report": report}


# ---------------------------------------------------------------------------
# Listing and CLI


def format_listing(result, race_label, start_level, max_level):
    quests = result["quests"]
    report = result["report"]
    summary = report["summary"]
    lines = [
        f"Player quest order: {race_label} (category {report['story_category']}), "
        f"level {start_level} -> {max_level}",
        "",
        "Assumptions:",
    ]
    lines.extend("  - " + a for a in report["assumptions"])
    lines.append("")

    def append_block(title, items):
        if not items:
            return
        lines.append(title)
        for quest in items:
            distance = quest.get("distance_m")
            distance_text = "-" if distance is None else f"{distance:g}m"
            lines.append(
                "  {order:>3}. [{source:<8}] {qid:<5} L{level!s:<2} zone {zone!s:<4} "
                "{start_kind:<9} {distance:>7} xp {xp:<6} level {before}->{after}  "
                "{name}".format(
                    order=quest["order"],
                    source=quest["source"],
                    qid=quest["quest_id"],
                    level=quest.get("level") if quest.get("level") is not None else "-",
                    zone=quest.get("zone_id") if quest.get("zone_id") is not None else "-",
                    start_kind=quest.get("start_kind", "?"),
                    distance=distance_text,
                    xp=quest.get("xp", 0),
                    before=quest.get("level_before"),
                    after=quest.get("level_after"),
                    name=quest.get("name", ""),
                )
            )
        lines.append("")

    append_block("Prologue (new character):",
                 [q for q in quests if q["source"] == "prologue"])
    append_block("Story and side quests:",
                 [q for q in quests if q["source"] != "prologue"])

    lines.append("Summary")
    lines.append(
        "  scheduled {total} (prologue {prologue}, story {story}, side {side}); "
        "simulated level {start} -> {end} ({xp} XP)".format(
            total=summary["scheduled"]["total"],
            prologue=summary["scheduled"]["prologue"],
            story=summary["scheduled"]["story"],
            side=summary["scheduled"]["side"],
            start=summary["start_level"],
            end=summary["end_level"],
            xp=summary["end_xp"],
        )
    )
    never = summary["never_available"]
    lines.append(
        "  never available: prologue {prologue}, story {story}, side {side}; "
        "side without a player starter {no_starter}; side above the range {out}; "
        "side outside the visited zones {zone}; side opened by more XP {above}; "
        "side waiting on levels/prereqs {pending}; story above the range {story_out}".format(
            prologue=never["prologue"], story=never["story"], side=never["side"],
            no_starter=never["side_without_player_starter"],
            out=never["side_out_of_level_range"],
            zone=never["side_out_of_zone"],
            above=never["side_above_final_level"],
            pending=report["summary"]["not_scheduled"]["side"],
            story_out=never["story_out_of_level_range"],
        )
    )
    if report["assumed_completed"]:
        lines.append(f"  assumed completed before the run: "
                     f"{len(report['assumed_completed'])} quests")
    if report["scheduled_not_ok"]:
        lines.append(
            "  scheduled but the static plan is not ok: "
            f"{len(report['scheduled_not_ok'])} quests (see the report JSON)"
        )
    lines.append("")

    def append_never(title, items, limit=40):
        if not items:
            return
        lines.append(title)
        for source, entry in items[:limit]:
            detail = entry.get("reason", "?")
            if entry.get("level_gate"):
                detail += f" (level gate {entry['level_gate']})"
            if entry.get("start_npcs"):
                detail += f" (start NPCs {entry['start_npcs']})"
            lines.append(
                "  - {qid:<5} {source:<8} zone {zone!s:<4} level {level!s:<2} "
                "{reason} {name}".format(
                    qid=entry.get("quest_id"),
                    source=source,
                    zone=entry.get("zone_id", "-"),
                    level=entry.get("level", "-"),
                    reason=detail,
                    name=entry.get("name", ""),
                )
            )
        if len(items) > limit:
            lines.append(f"  ... {len(items) - limit} more (see the report JSON)")
        lines.append("")

    append_never("Never available in this window:", (
        [("prologue", entry) for entry in report["never_available"]["prologue"]] +
        [("story", entry) for entry in report["never_available"]["story"]] +
        [("side", entry) for entry in report["never_available"]["side"]]
    ))
    excluded = report["excluded"]
    if excluded["side_without_player_starter"]:
        lines.append(
            "Side quests without a player starter (not ordered): "
            f"{len(excluded['side_without_player_starter'])} "
            f"({', '.join(map(str, excluded['side_without_player_starter'][:20]))}"
            + (" ..." if len(excluded["side_without_player_starter"]) > 20 else "") + ")"
        )
    if excluded["tutorial_category"]:
        lines.append(
            "Tutorial category 45 (not loaded by the server): "
            f"{len(excluded['tutorial_category'])} contexts"
        )
    if excluded["prologue_other_race"]:
        lines.append(
            "Prologue quests of other races: "
            f"{', '.join(map(str, excluded['prologue_other_race']))}"
        )
    return "\n".join(lines).rstrip() + "\n"


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--db", type=Path, required=True)
    parser.add_argument("--worlds", type=Path, required=True)
    parser.add_argument("--race", default="nuian", choices=sorted(RACES))
    parser.add_argument("--level", type=int, default=1,
                        help="starting character level (default 1)")
    parser.add_argument("--max-level", type=int, default=15,
                        help="highest story/side content level (default 15)")
    parser.add_argument("--side-level-margin", type=int, default=2,
                        help="side quests may be up to current level + this (default 2)")
    parser.add_argument("--neighbor-distance", type=float, default=4000.0,
                        help="zone-group rectangle distance treated as neighbouring "
                             "(metres, default 4000)")
    parser.add_argument("--kill-xp", type=int, default=0,
                        help="estimated kill XP per completed quest (default 0: ignored)")
    parser.add_argument("--gender", choices=["m", "f"], default=None,
                        help="evaluate gender requirements (default: either)")
    parser.add_argument("--skip-prologue", action="store_true",
                        help="assume the prologue is already done")
    parser.add_argument("--no-assume-previous", action="store_true",
                        help="do not assume story quests below --level are done")
    parser.add_argument("--output", type=Path, required=True,
                        help="JSON array of plan quests in play order")
    parser.add_argument("--report", type=Path,
                        help="optional JSON report (assumptions, never available)")
    parser.add_argument("--listing", type=Path,
                        help="readable listing file (default: stdout)")
    parser.add_argument("--no-plan-steps", action="store_true",
                        help="do not merge plan.py steps (smaller, not runnable)")
    return parser.parse_args(argv)


def main(argv=None):
    args = parse_args(argv)
    db = args.db.resolve(strict=True)
    worlds = args.worlds.resolve(strict=True)
    if args.output.resolve() == db:
        raise SystemExit("--output must not overwrite the source database")
    if args.max_level < args.level:
        raise SystemExit("--max-level must be >= --level")
    with sqlite3.connect(f"file:{db.as_posix()}?mode=ro", uri=True) as conn:
        conn.row_factory = sqlite3.Row
        spawns = load_spawns(conn, worlds)
        plan_quests = {}
        if not args.no_plan_steps:
            built = quest_plan.plan(conn, spawns)
            plan_quests = {q["quest_id"]: q for q in built["quests"]}
        orderer = Orderer(
            conn, spawns, args.race, start_level=args.level,
            max_level=args.max_level, side_margin=args.side_level_margin,
            neighbor_distance=args.neighbor_distance, kill_xp=args.kill_xp,
            gender=1 if args.gender == "m" else (2 if args.gender == "f" else None),
            skip_prologue=args.skip_prologue,
            assume_previous=not args.no_assume_previous,
            plan_quests=plan_quests,
        )
        result = orderer.run()

    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(
        json.dumps(result["quests"], ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
    )
    if args.report:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(
            json.dumps(result["report"], ensure_ascii=False, indent=2) + "\n",
            encoding="utf-8",
        )
    listing = format_listing(result, orderer.race_label, args.level, args.max_level)
    if args.listing:
        args.listing.parent.mkdir(parents=True, exist_ok=True)
        args.listing.write_text(listing, encoding="utf-8")
    else:
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")
        sys.stdout.write(listing)
    summary = result["report"]["summary"]
    print(json.dumps({"scheduled": summary["scheduled"],
                      "never_available": summary["never_available"],
                      "end_level": summary["end_level"]}, ensure_ascii=False),
          file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
