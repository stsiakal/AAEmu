"""Static quest plan for a bot: ordered world targets from compact.sqlite3 + spawn data.

Read-only in, JSON out. For every loaded quest context (the loader skips the
category-45 tutorials, see `data.rs::QUEST_CATEGORY_TUTORIAL`) the acts are ordered
like the loader (`load_components`: quest_context_id, component_kind_id, id; acts by
act id) and each act carries its target ids, up to five spawn locations for
npc/doodad targets and a status. Quests whose non-optional npc/doodad targets have
no spawn anywhere in the world files are flagged; npc targets that are only summoned
at runtime by `SpawnEffect` skills (`spawn_effects.owner_type_id == 1`) get
"dynamic_spawn" with their sources instead, and steps whose every location is
gated on a game schedule / spawn-time window get "scheduled_spawn" (locations
carry a `schedule` annotation); act types the plan cannot map are
flagged "unsupported", never guessed. Quests without a Start component (auxiliary
records) get status "auxiliary". Item targets carry `item_sources` (npc loot,
doodad funcs, earlier quest supplies, crafts, stores, bundle items opened by
use skills: `item_open` with the container's own sources in `container_sources`,
depth-capped and cycle-guarded) and a target without any source gets
"unknown_item_source": it is reported but does not block the quest, because items
also come from paths the plan does not model (mail, other quests); the live
runner decides. Each quest also carries the accept prerequisites
(`unit_reqs` of its Start components, owner type "QuestComponent", the only quest
owner type the accept path evaluates) plus `min_level` and `prereq_quests`. With
`--spheres` the sphere steps (ObjSphere / ConAcceptSphere / CheckSphere) get their
`quest_sign_sphere.g` centres (world.xml zone origins are required in the export;
see `load_spheres`). Run:

    python plan.py --db compact.sqlite3 --worlds Data/Worlds --output plan.json
"""

import argparse
from collections import Counter, defaultdict
import json
from pathlib import Path
import sqlite3
import sys


# `QUEST_CATEGORY_TUTORIAL` (systems/quests/data.rs); these contexts are not loaded.
TUTORIAL = 45
# `QuestComponentKind` member names (`systems/quests/model.rs::kind`).
COMPONENT_KINDS = {0: "Invalid", 1: "None", 2: "Start", 3: "Supply", 4: "Progress",
                   5: "Fail", 6: "Ready", 7: "Drop", 8: "Reward"}
# Fail / Drop are the terminal failure branches of `QuestEngine::go_to_next_step`; a
# bot completing the quest never needs their spawns, so missing targets there do not
# block the plan.
OPTIONAL_KINDS = {5, 7}
KIND_LEVEL = 1
KIND_START = 2
KIND_COMPLETE_QUEST_CONTEXT = 0x1F
# Item-target act types that get `item_sources` (the runner needs to know where the
# items come from). ObjCraft is excluded: its target is the craft recipe itself.
ITEM_SOURCE_ACTS = {"QuestActObjItemGather", "QuestActObjItemUse", "QuestActConAcceptItem",
                    "QuestActEtcItemObtain", "QuestActConAcceptItemGain",
                    "QuestActObjItemGroupGather", "QuestActObjItemGroupUse"}
# Act class -> supply detail table (the earlier-act `quest_supply` source).
ITEM_SUPPLY_ACTS = {"QuestActSupplyItem": "quest_act_supply_items",
                    "QuestActSupplySelectiveItem": "quest_act_supply_selective_items"}
# How many nested bundle openings (`item_open` -> `container_sources` -> ...) the plan
# resolves before it stops expanding; a container already on the chain is never reopened.
ITEM_OPEN_DEPTH = 3
# The one `SpecialType` the item-use path grants item `value1` for: 27 `GainItem`
# (`economy/bridges.rs` `special::GAIN_ITEM` -> `item_effects.rs::gain_item`). The
# `GAIN_ITEM_SPECIALS` 32/45 are only read as item grants by the doodad-func skill-hit
# path (`skills/bridges.rs`); on skill use 45 imprints a crest stamp instead.
GAIN_ITEM_SPECIALS = {27}
# The acts whose `quest_sign_sphere.g` component spheres are exported by the gated
# `real_compact_db_quest_accounting` test (systems/quests/tests.rs).
SPHERE_ACTS = {"QuestActObjSphere", "QuestActConAcceptSphere", "QuestActCheckSphere"}
# `unit_reqs.kind_id` -> symbolic name. The names are the failure keys of
# `systems/skills/unit_reqs.rs::validate` (`skill_urk_*` without the prefix); kinds whose
# skill arm returns the generic "skill_failure" take the constant name of
# `systems/quests/unit_reqs.rs::req_kind` lowercased instead.
REQ_KINDS = {
    0x01: "level", 0x02: "ability", 0x03: "race", 0x04: "gender",
    0x08: "equip_slot", 0x09: "equip_item", 0x0A: "own_item", 0x0B: "trained_skill",
    0x0C: "combat", 0x0F: "buff", 0x10: "target_buff", 0x11: "target_combat",
    0x14: "doodad_range", 0x15: "equip_shield", 0x16: "nobuff", 0x17: "target_buff_tag",
    0x1A: "target_health_less_than", 0x1B: "target_npc", 0x1C: "target_doodad",
    0x1D: "equip_ranged", 0x1E: "no_buff_tag", 0x1F: "complete_quest_context",
    0x20: "progress_quest_context", 0x21: "ready_quest_context", 0x22: "target_npc_group",
    0x23: "area_sphere", 0x24: "except_complete_quest_context", 0x25: "pre_complete_quest_context",
    0x26: "target_owner_type", 0x27: "not_under_water", 0x28: "faction_match", 0x29: "tod",
    0x2A: "mother_faction", 0x2B: "act_ability_point", 0x2C: "crime_point", 0x2D: "honor_point",
    0x2E: "crime_record", 0x2F: "jury_point", 0x30: "source_owner_type", 0x31: "appellation",
    0x32: "living_point", 0x33: "in_zone", 0x34: "out_zone", 0x36: "verdict_only",
    0x37: "faction_match_only", 0x38: "mother_faction_only", 0x3A: "faction_match_only_not",
    0x3B: "mother_faction_only_not", 0x43: "labor_power_margin",
    0x44: "not_on_moving_physical_vehicle", 0x45: "max_level", 0x46: "expedition_owner",
    0x47: "expedition_member", 0x48: "except_progress_quest_context",
    0x49: "except_ready_quest_context",
}
# act class -> (target kind, detail table, target id column, count column or None).
# Tables and columns verified against compact.sqlite3 and `data.rs::load_detail_acts`;
# the cited handler is the `acts.rs` arm that consumes the detail row.
ACT_TARGETS = {
    "QuestActConAcceptNpc": ("npc", "quest_act_con_accept_npcs", "npc_id", None),
    "QuestActConReportNpc": ("npc", "quest_act_con_report_npcs", "npc_id", None),
    "QuestActObjTalk": ("npc", "quest_act_obj_talks", "npc_id", None),
    "QuestActObjMonsterHunt": ("npc", "quest_act_obj_monster_hunts", "npc_id", "count"),
    "QuestActConAcceptNpcKill": ("npc", "quest_act_con_accept_npc_kills", "npc_id", None),
    "QuestActObjInteraction": ("doodad", "quest_act_obj_interactions", "doodad_id", "count"),
    "QuestActConAcceptDoodad": ("doodad", "quest_act_con_accept_doodads", "doodad_id", None),
    "QuestActConReportDoodad": ("doodad", "quest_act_con_report_doodads", "doodad_id", None),
    "QuestActObjItemGather": ("item", "quest_act_obj_item_gathers", "item_id", "count"),
    "QuestActObjItemUse": ("item", "quest_act_obj_item_uses", "item_id", "count"),
    "QuestActConAcceptItem": ("item", "quest_act_con_accept_items", "item_id", None),
    "QuestActObjSphere": ("sphere", "quest_act_obj_spheres", "sphere_id", None),
    "QuestActConAcceptSphere": ("sphere", "quest_act_con_accept_spheres", "sphere_id", None),
    # acts.rs act_event `(EtcItemObtain, QuestEvent::ItemGather)` (the C# disables the check).
    "QuestActEtcItemObtain": ("item", "quest_act_etc_item_obtains", "item_id", "count"),
    # acts.rs RunAct `ConAcceptItemGain { item_id }` -> `check_items(count)`.
    "QuestActConAcceptItemGain": ("item", "quest_act_con_accept_item_gains", "item_id", "count"),
    # acts.rs RunAct `ConAcceptComponent { .. } => true`; the detail names the quest context.
    "QuestActConAcceptComponent": ("quest", "quest_act_con_accept_components",
                                   "quest_context_id", None),
    # acts.rs act_event `(ObjCompleteQuest { quest_id }, QuestEvent::QuestComplete)`.
    "QuestActObjCompleteQuest": ("quest", "quest_act_obj_complete_quests", "quest_id", None),
}


def rows(conn, table):
    # Identifiers come from the audited source, never CLI input.
    return [dict(row) for row in conn.execute('SELECT * FROM "' + table.replace('"', '""') + '"')]


def query(conn, sql):
    return [dict(row) for row in conn.execute(sql)]


def truth(value):
    return value in ("t", "true", 1, True)


def prop(obj, name):
    """Newtonsoft member match: exact first, then case-insensitive (`json.rs::prop`)."""
    if name in obj:
        return obj[name]
    for key, value in obj.items():
        if key.lower() == name.lower():
            return value
    return None


def clean_json(text):
    """The lenient Newtonsoft dialect of the World files (`config::relax_json`):
    `//` and `/* */` comments plus trailing commas outside strings."""

    def skip_trivia(j):
        while j < len(text):
            while j < len(text) and text[j].isspace():
                j += 1
            if text.startswith("//", j):
                while j < len(text) and text[j] != "\n":
                    j += 1
            elif text.startswith("/*", j):
                j += 2
                while j + 1 < len(text) and text[j:j + 2] != "*/":
                    j += 1
                j = min(j + 2, len(text))
            else:
                return j
        return j

    out = []
    i = 0
    in_string = False
    while i < len(text):
        c = text[i]
        if in_string:
            out.append(c)
            if c == "\\" and i + 1 < len(text):
                out.append(text[i + 1])
                i += 2
                continue
            if c == '"':
                in_string = False
            i += 1
        elif c == '"':
            in_string = True
            out.append(c)
            i += 1
        elif c == "/" and text.startswith(("/", "*"), i + 1):
            i = skip_trivia(i)
        elif c == ",":
            # Drop a trailing comma (the next significant character closes the container).
            j = skip_trivia(i + 1)
            if j < len(text) and text[j] in "}]":
                i += 1
            else:
                out.append(c)
                i += 1
        else:
            out.append(c)
            i += 1
    return "".join(out)


def load_spawns(worlds, ids_by_kind, npc_spawner_npcs=(), schedule_spawners=None,
                schedule_doodads=None, spawner_windows=None, known_schedules=()):
    """Spawn locations by template id: ``{kind: {id: [(world, x, y, z), ...]}, "gates": ...}``.

    Mirrors `SpawnManager::load_npc_spawns` / `load_doodad_spawns`: `UnitId` is the
    *template* id (NPC: `npcs.id`, doodad: `doodad_almighties.id`) checked with
    `exists.npc` / `exists.doodad`, not an `npc_spawners` row. The compact spawner
    tables are indirect only: an ordinary NPC entry (no `NpcSpawnerIds`) clones the
    game-data spawner templates whose `npc_spawner_npcs.member_id` is the UnitId, so
    those members can spawn at the same spot and are expanded here; event entries
    (`NpcSpawnerIds` set, `[0]` counts as empty) spawn the UnitId alone. Doodad
    entries have no such indirection. Files are read in the load order
    (`spawn_files` name sort, then `files.reverse()`), with the same per-UnitId
    near-duplicate filter (2.0 for NPCs, 0.01 for doodads).

    `gates` maps a placement ``(kind, target, world, x, y, z)`` to every spawner
    template that can place the target there: ``{template_id: gate or None}``
    (``0`` = a clone without a compact template). A gate is a dict when that path
    is gated: `spawner.rs::is_spawning_schedule_enabled` checks the clone's game
    schedules (`period_status_npc`; the schedule map alone gates, even with
    dangling schedule ids) and the compact template's daily window
    (`is_within_spawn_time`); `doodad/spawner.rs::do_spawn` gates on
    `game_schedule_doodads` when one of its schedule ids exists. `activation_state`
    is not a spawn gate (only `NpcSpawnerSpawnEffect`, bridges.rs). A placement is
    schedule-gated when every path to it is gated.
    """
    schedule_spawners = schedule_spawners or {}
    doodad_schedules = schedule_doodads or {}
    windows = spawner_windows or {}
    known = set(known_schedules)

    def npc_gate(template_id, window):
        info = None
        if template_id in schedule_spawners:
            info = {"spawner_id": template_id,
                    "schedule_ids": list(schedule_spawners[template_id])}
        if window and template_id in windows:
            start, end = windows[template_id]
            if info is None:
                info = {"spawner_id": template_id, "schedule_ids": []}
            info["start_time"] = start
            info["end_time"] = end
        return info

    def doodad_gate(template_id):
        ids = doodad_schedules.get(template_id, [])
        if ids and any(schedule_id in known for schedule_id in ids):
            return {"spawner_id": template_id, "schedule_ids": list(ids)}
        return None

    spawner_members = defaultdict(list)
    member_spawners = defaultdict(list)
    for spawner_id, member_id in npc_spawner_npcs:
        spawner_members[spawner_id].append(member_id)
        member_spawners[member_id].append(spawner_id)
    spawns = {"npc": defaultdict(list), "doodad": defaultdict(list)}
    gates = defaultdict(dict)
    for world_dir in sorted(path for path in worlds.iterdir() if path.is_dir()):
        for kind, prefix, tolerance in (("npc", "npc_spawns", 2.0),
                                        ("doodad", "doodad_spawns", 0.01)):
            files = sorted((f for f in world_dir.iterdir()
                            if f.is_file() and f.name.lower().startswith(prefix)
                            and f.name.lower().endswith(".json")),
                           key=lambda f: f.name.lower())
            stored = defaultdict(list)
            for path in reversed(files):
                text = path.read_text(encoding="utf-8-sig")
                if not text.strip():
                    continue
                for entry in json.loads(clean_json(text)):
                    if not isinstance(entry, dict):
                        continue
                    unit_id = prop(entry, "UnitId")
                    position = prop(entry, "Position")
                    if not isinstance(unit_id, int) or not isinstance(position, dict):
                        continue
                    x, y, z = (prop(position, axis) for axis in "XYZ")
                    if any(not isinstance(v, (int, float)) for v in (x, y, z)):
                        continue
                    if unit_id not in ids_by_kind[kind]:
                        continue
                    if any(abs(sx - x) < tolerance and abs(sy - y) < tolerance
                           and abs(sz - z) < tolerance for sx, sy, sz in stored[unit_id]):
                        continue
                    stored[unit_id].append((x, y, z))
                    placement = (world_dir.name, float(x), float(y), float(z))
                    if kind == "doodad":
                        placed = [(unit_id, 0, doodad_gate(unit_id))]
                    else:
                        event_ids = prop(entry, "NpcSpawnerIds")
                        event = isinstance(event_ids, list) and event_ids and event_ids != [0]
                        if event:
                            side = prop(entry, "SpawnerId")
                            side = side if isinstance(side, int) else 0
                            placed = [(unit_id, side, npc_gate(side, window=False))]
                        else:
                            templates = list(dict.fromkeys(member_spawners.get(unit_id, [])))
                            if templates:
                                placed = [(member, template_id, npc_gate(template_id, True))
                                          for template_id in templates
                                          for member in spawner_members[template_id]]
                            else:
                                # Generated fake spawner (template 1 clone): no schedule.
                                placed = [(unit_id, 0, None)]
                    for member, spawner_id, gate in placed:
                        if member not in ids_by_kind[kind]:
                            continue
                        if placement not in spawns[kind][member]:
                            spawns[kind][member].append(placement)
                        gates[(kind, member) + placement][spawner_id] = gate
    result = {kind: dict(by_id) for kind, by_id in spawns.items()}
    result["gates"] = dict(gates)
    return result


def merge_locations(kind, ids, spawns):
    """``{placement: merged gate map}`` over the target ids (insertion order)."""
    merged = {}
    gate_data = spawns.get("gates", {})
    for target in ids:
        for placement in spawns[kind].get(target, []):
            entry = merged.setdefault(placement, {})
            gates = gate_data.get((kind, target) + placement)
            if gates:
                entry.update(gates)
            else:
                entry.setdefault(0, None)
    return merged


def gated_info(gates):
    """The `schedule` annotation of a placement gated on every path, else None."""
    if gates and all(info is not None for info in gates.values()):
        return min((info for info in gates.values()), key=lambda g: g["spawner_id"])
    return None


def location_dict(placement, gates):
    location = {"world": placement[0], "x": placement[1], "y": placement[2], "z": placement[3]}
    info = gated_info(gates)
    if info:
        location["schedule"] = info
    return location


def location_dicts(kind, ids, spawns):
    """Up to five nearest locations of the ids, with `schedule` where gated."""
    merged = merge_locations(kind, ids, spawns)
    return [location_dict(placement, merged[placement])
            for placement in closest(list(merged))]


def load_spheres(path):
    """Quest sign spheres from the `real_compact_db_quest_accounting` export.

    The world conversion is the server's: `spheres.rs::SphereQuestManager::load`
    sets ``xyz = (origin_x * 1024 + local[0], origin_y * 1024 + local[1], local[2])``
    where the origin is `zone_origin_cell` (`mod.rs::System::start` ->
    `subzone.rs::zone_origin_cell` -> `WorldTemplates.xml_world_zones`, the
    `<Zone originX= originY=>` attributes of the client pak's world.xml,
    `data/world_templates.rs::read_zone`; `data.rs::load_sign_spheres` reads the
    zone-local `pos`). The compact DB has no origin cells, so the export must
    carry them: a top-level ``"zone_origins": {"<zone_id>": [origin_x, origin_y]}``
    map, or ``"origin": [x, y]`` / ``"originX"/"originY"`` on each row. Rows
    without an origin are indexed but produce no coordinates.
    """
    export = json.loads(Path(path).read_text(encoding="utf-8"))
    rows = export.get("sign_spheres", []) if isinstance(export, dict) else export
    origins = export.get("zone_origins", {}) if isinstance(export, dict) else {}

    def origin_of(row):
        origin = row.get("origin")
        if origin is None and "originX" in row:
            origin = [row.get("originX"), row.get("originY")]
        if origin is None:
            origin = origins.get(str(row.get("zone_id")))
            if origin is None:
                origin = origins.get(row.get("zone_id"))
        if isinstance(origin, dict):
            return origin.get("origin_x", origin.get("x")), origin.get("origin_y", origin.get("y"))
        if isinstance(origin, (list, tuple)) and len(origin) == 2:
            return origin[0], origin[1]
        return None

    index = {"by_component": defaultdict(list), "by_quest": defaultdict(list),
             "with_origin": 0, "without_origin": 0}
    for row in rows:
        entry = {"quest_id": row.get("quest_id"), "component_id": row.get("component_id"),
                 "zone_id": row.get("zone_id"), "radius": row.get("radius")}
        origin = origin_of(row)
        local = row.get("local") or [0.0, 0.0, 0.0]
        if (origin and len(local) == 3
                and all(isinstance(v, (int, float)) for v in origin + tuple(local[:3]))):
            entry.update(world=row.get("world", "main_world"),
                         x=origin[0] * 1024.0 + local[0],
                         y=origin[1] * 1024.0 + local[1], z=local[2])
            index["with_origin"] += 1
        else:
            index["without_origin"] += 1
        index["by_component"][(entry["quest_id"], entry["component_id"])].append(entry)
        index["by_quest"][entry["quest_id"]].append(entry)
    return index


def closest(locations, limit=5):
    """The first location plus the up to `limit` - 1 nearest of the rest."""
    if not locations:
        return []
    unique = list(dict.fromkeys(locations))
    anchor = unique[0]
    rest = sorted(unique[1:], key=lambda l: (l[1] - anchor[1]) ** 2
                  + (l[2] - anchor[2]) ** 2 + (l[3] - anchor[3]) ** 2)
    return [anchor] + rest[:limit - 1]


def detail_of(conn, cache, table, detail_id):
    if table not in cache:
        cache[table] = {row["id"]: row for row in rows(conn, table)}
    return cache[table].get(detail_id)


def count_of(detail, column):
    if column and detail and isinstance(detail.get(column), int) and detail[column] > 0:
        return detail[column]
    return 1


def int_of(row, column, default=0):
    value = row.get(column) if row else None
    return value if isinstance(value, int) else default


# The four "reach a level" objectives (`acts.rs` RunAct) and their detail tables.
LEVEL_ACTS = {
    "QuestActObjAbilityLevel": "quest_act_obj_ability_levels",
    "QuestActObjMateLevel": "quest_act_obj_mate_levels",
    "QuestActObjLevel": "quest_act_obj_levels",
    "QuestActConAcceptLevelUp": "quest_act_con_accept_level_ups",
}


def load_lookups(conn):
    """Member relations the act targets expand through (all small tables)."""
    lookup = {"group_npcs": defaultdict(list), "group_items": defaultdict(list),
              "craft_items": defaultdict(list)}
    for row in rows(conn, "quest_monster_npcs"):
        lookup["group_npcs"][row["quest_monster_group_id"]].append(row["npc_id"])
    for row in rows(conn, "quest_item_group_items"):
        lookup["group_items"][row["quest_item_group_id"]].append(row["item_id"])
    for row in rows(conn, "craft_products"):
        lookup["craft_items"][row["craft_id"]].append(row["item_id"])
    return lookup


def load_item_source_data(conn):
    """Reverse lookup tables for the item sources (item id -> candidates).

    Mirrors the loaders / handlers the server uses:
    * `loots` / `loot_pack_dropping_npcs` (`economy/loot_data.rs`, `economy/item_effects.rs`
      and `looting.rs::generate_loot`) for NPC drops;
    * `doodad_func_loot_items` / `doodad_func_loot_packs` / `doodad_func_purchases` through
      `doodad_funcs` -> `doodad_func_groups` (`npcs/doodad/funcs/items.rs`: `loot_item`,
      `loot_pack`, `purchase`);
    * `merchant_goods` -> `merchant_packs` / `merchants` for store vendors;
    * bundle items opened by use skills (`item_open`): a `GainLootPackItemEffect`
      (`economy/item_effects.rs::gain_loot_pack_item`, applied by
      `economy/bridges.rs::apply_effect`) is an `effects` row (actual id =
      `gain_loot_pack_item_effects.id`) of a skill (the `skill_effects` chain of
      `skills/data/mod.rs`); the skill is the `items.use_skill_id` of the container item.
      `GainItem` `SpecialEffect`s (`item_effects.rs::gain_item`) and `skill_products`
      (`skills/skill.rs` acquires the products on skill use) grant items directly.
    """
    data = {"loot": defaultdict(list), "pack_npcs": defaultdict(list),
            "item_doodads": defaultdict(list), "item_crafts": defaultdict(list),
            "item_stores": defaultdict(list), "pack_vendors": defaultdict(list),
            "pack_skills": defaultdict(list), "skill_gain_items": defaultdict(list),
            "skill_product_items": defaultdict(list), "item_use_skills": defaultdict(list),
            "item_quests": {}}
    item_ids = set()
    for row in query(conn, "SELECT id, loot_quest_id, use_skill_id FROM items"):
        item_ids.add(row["id"])
        if row["loot_quest_id"]:
            data["item_quests"][row["id"]] = row["loot_quest_id"]
        if row["use_skill_id"]:
            data["item_use_skills"][row["use_skill_id"]].append(row["id"])
    for row in rows(conn, "loots"):
        data["loot"][row["item_id"]].append(row)
    for row in rows(conn, "loot_pack_dropping_npcs"):
        data["pack_npcs"][row["loot_pack_id"]].append(row["npc_id"])
    groups = {row["id"]: row["doodad_almighty_id"]
              for row in rows(conn, "doodad_func_groups")}
    func_doodads = defaultdict(list)
    for row in rows(conn, "doodad_funcs"):
        if row["actual_func_type"] in ("DoodadFuncLootItem", "DoodadFuncLootPack",
                                       "DoodadFuncPurchase"):
            doodad = groups.get(row["doodad_func_group_id"])
            if doodad:
                func_doodads[(row["actual_func_type"], row["actual_func_id"])].append(doodad)
    for row in rows(conn, "doodad_func_loot_items"):
        for doodad in func_doodads.get(("DoodadFuncLootItem", row["id"]), []):
            data["item_doodads"][row["item_id"]].append(("DoodadFuncLootItem", doodad))
    for row in rows(conn, "doodad_func_purchases"):
        for doodad in func_doodads.get(("DoodadFuncPurchase", row["id"]), []):
            data["item_doodads"][row["item_id"]].append(("DoodadFuncPurchase", doodad))
    pack_funcs = defaultdict(list)
    for row in rows(conn, "doodad_func_loot_packs"):
        pack_funcs[row["loot_pack_id"]].append(row["id"])
    for item_id, loots in data["loot"].items():
        for loot in loots:
            for func_id in pack_funcs.get(loot["loot_pack_id"], []):
                for doodad in func_doodads.get(("DoodadFuncLootPack", func_id), []):
                    data["item_doodads"][item_id].append(("DoodadFuncLootPack", doodad))
    for row in rows(conn, "craft_products"):
        data["item_crafts"][row["item_id"]].append(row["craft_id"])
    for row in rows(conn, "merchant_goods"):
        data["item_stores"][row["item_id"]].append(row["merchant_pack_id"])
    for row in rows(conn, "merchant_packs"):
        data["pack_vendors"][row["id"]].append(row["owner_npc_id"])
    for row in rows(conn, "merchants"):
        data["pack_vendors"][row["merchant_pack_id"]].append(row["npc_id"])
    # `skill_effect` -> `effects` -> `gain_loot_pack_item_effects` (the loot pack the
    # effect grants); the skill must exist like the loader requires (`skills` join).
    for row in query(conn,
                     "SELECT se.skill_id, gl.loot_pack_id FROM effects ef"
                     " JOIN skill_effects se ON se.effect_id = ef.id"
                     " JOIN gain_loot_pack_item_effects gl ON gl.id = ef.actual_id"
                     " JOIN skills s ON s.id = se.skill_id"
                     " WHERE ef.actual_type = 'GainLootPackItemEffect'"):
        data["pack_skills"][row["loot_pack_id"]].append(row["skill_id"])
    specials = ",".join(str(t) for t in sorted(GAIN_ITEM_SPECIALS))
    for row in query(conn,
                     "SELECT se.skill_id, sp.value1 AS item_id FROM effects ef"
                     " JOIN skill_effects se ON se.effect_id = ef.id"
                     " JOIN special_effects sp ON sp.id = ef.actual_id"
                     " JOIN skills s ON s.id = se.skill_id"
                     " WHERE ef.actual_type = 'SpecialEffect'"
                     f" AND sp.special_effect_type_id IN ({specials})"):
        if row["item_id"] in item_ids:
            data["skill_gain_items"][row["skill_id"]].append(row["item_id"])
    for row in query(conn, "SELECT sp.skill_id, sp.item_id FROM skill_products sp"
                           " JOIN skills s ON s.id = sp.skill_id"):
        if row["item_id"] in item_ids:
            data["skill_product_items"][row["skill_id"]].append(row["item_id"])
    return data


def bounded(ids, spawn_index, limit=5):
    """Up to `limit` unique ids, spawn-located ones (by id) first."""
    unique = sorted(set(ids))
    order = sorted(unique, key=lambda i: (i not in spawn_index, i))
    return order[:limit]


def item_sources(item_id, supplied, spawns, data, depth=0, chain=()):
    """Bounded candidate sources of one item id, location-backed first.

    `chance` is the raw `loots.drop_rate` (10,000,000 = 100%,
    `economy/loot.rs` divides by 10,000,000); `loot_quest_id` mirrors the
    `HasQuest` gate of `looting.rs::try_take_loot_locked` for quest-only drops;
    `locations` carry the `schedule` annotation of `load_spawns`.

    `item_open` sources are bundle items opened by their `use_skill_id`: the item is in a
    loot pack the skill's `GainLootPackItemEffect` grants, or the skill grants it directly
    (`GainItem` special / `skill_products`). `container_sources` resolves the container
    item's own sources down to `ITEM_OPEN_DEPTH`; `chain` holds the items already being
    opened so a cycle is dropped instead of recursed into.
    """
    sources = []
    seen_packs = set()
    for loot in data["loot"].get(item_id, []):
        pack = loot["loot_pack_id"]
        if pack in seen_packs:
            continue
        seen_packs.add(pack)
        npcs = bounded(data["pack_npcs"].get(pack, []), spawns["npc"])
        entry = {"type": "npc_loot", "npc_ids": npcs, "loot_pack_id": pack,
                 "chance": loot["drop_rate"],
                 "locations": location_dicts("npc", npcs, spawns)}
        if item_id in data["item_quests"]:
            entry["loot_quest_id"] = data["item_quests"][item_id]
        sources.append(entry)
    doodads = defaultdict(list)
    for func_type, doodad_id in data["item_doodads"].get(item_id, []):
        doodads[func_type].append(doodad_id)
    for func_type in ("DoodadFuncLootItem", "DoodadFuncLootPack", "DoodadFuncPurchase"):
        if doodads.get(func_type):
            ids = bounded(doodads[func_type], spawns["doodad"])
            sources.append({"type": "doodad", "doodad_ids": ids, "func": func_type,
                            "locations": location_dicts("doodad", ids, spawns)})
    for act_id in sorted(supplied.get(item_id, [])):
        sources.append({"type": "quest_supply", "act_id": act_id})
    crafts = bounded(data["item_crafts"].get(item_id, []), {})
    if crafts:
        sources.append({"type": "craft", "craft_ids": crafts})
    packs = bounded(data["item_stores"].get(item_id, []), {})
    if packs:
        vendors = bounded([npc for p in packs for npc in data["pack_vendors"].get(p, [])],
                          spawns["npc"])
        sources.append({"type": "store", "npc_ids": vendors, "pack_ids": packs,
                        "locations": location_dicts("npc", vendors, spawns)})
    if depth < ITEM_OPEN_DEPTH:
        opened = []
        seen_opens = set()
        for loot in data["loot"].get(item_id, []):
            pack = loot["loot_pack_id"]
            for skill_id in sorted(set(data["pack_skills"].get(pack, []))):
                for container in sorted(set(data["item_use_skills"].get(skill_id, []))):
                    key = ("loot_pack", container, skill_id, pack)
                    if key not in seen_opens:
                        seen_opens.add(key)
                        opened.append({"type": "item_open", "container_item_id": container,
                                       "skill_id": skill_id, "loot_pack_id": pack,
                                       "chance": loot["drop_rate"]})
        for grant, skill_items in (("gain_item", data["skill_gain_items"]),
                                   ("skill_product", data["skill_product_items"])):
            for skill_id, granted in skill_items.items():
                if item_id not in granted:
                    continue
                for container in sorted(set(data["item_use_skills"].get(skill_id, []))):
                    key = (grant, container, skill_id, 0)
                    if key not in seen_opens:
                        seen_opens.add(key)
                        opened.append({"type": "item_open", "container_item_id": container,
                                       "skill_id": skill_id, "grant": grant})
        for source in sorted(opened, key=lambda s: (s["container_item_id"], s["skill_id"],
                                                    s.get("loot_pack_id", 0))):
            if source["container_item_id"] in chain or source["container_item_id"] == item_id:
                continue
            source["container_sources"] = item_sources(
                source["container_item_id"], supplied, spawns, data,
                depth + 1, chain + (item_id,))
            sources.append(source)
    order = {"npc_loot": 0, "doodad": 1, "quest_supply": 2, "craft": 3, "store": 4,
             "item_open": 5}
    by_type = Counter()
    kept = []
    for source in sorted(sources, key=lambda s: (order[s["type"]], not s.get("locations"),
                                                 s.get("loot_pack_id", 0))):
        if by_type[source["type"]] < 5:
            by_type[source["type"]] += 1
            kept.append(source)
    return kept


def dynamic_npc_sources(conn):
    """NPC template id -> spawn sources applied at runtime instead of the world files.

    `spawn_effects.owner_type_id == 1` is the NPC branch of `bridges.rs::spawn_effect`
    (`sub_type` = NPC template id, spawned at the caster/target position, so no world
    location exists). The skills come from the `skill_effects` -> `effects` chain of
    `skills/data/mod.rs` (`effects.actual_type == 'SpawnEffect'`, actual id =
    `spawn_effects.id`); skills must exist like the loader requires.
    """
    sub_types = {row["id"]: row["sub_type"] for row in rows(conn, "spawn_effects")
                 if row["owner_type_id"] == 1}
    skills = defaultdict(list)
    for row in conn.execute(
            "SELECT se.skill_id, ef.actual_id FROM skill_effects se"
            " JOIN effects ef ON ef.id = se.effect_id"
            " JOIN skills s ON s.id = se.skill_id"
            " WHERE ef.actual_type = 'SpawnEffect'"):
        if row["actual_id"] in sub_types:
            skills[row["actual_id"]].append(row["skill_id"])
    sources = defaultdict(list)
    for effect_id, sub_type in sorted(sub_types.items()):
        sources[sub_type].append({"effect_id": effect_id,
                                  "skill_ids": sorted(set(skills[effect_id]))})
    return sources


def step_target(conn, details, lookup, act_class, detail_id):
    """The plan `target` of one act from its detail row; None when unsupported."""

    def target(kind, ids, count, **extra):
        return dict({"kind": kind, "ids": ids, "count": count}, **extra)

    if act_class == "QuestActObjMonsterGroupHunt":
        detail = detail_of(conn, details, "quest_act_obj_monster_group_hunts", detail_id)
        ids = list(lookup["group_npcs"].get(int_of(detail, "quest_monster_group_id"), []))
        return target("npc", ids, count_of(detail, "count"))
    if act_class == "QuestActObjTalkNpcGroup":
        # acts.rs act_event `(ObjTalkNpcGroup { npc_group_id }, QuestEvent::TalkNpcGroupMade)`;
        # members are the `quest_monster_npcs` rows (`data.rs` `_groupNpcs`).
        detail = detail_of(conn, details, "quest_act_obj_talk_npc_groups", detail_id)
        ids = list(lookup["group_npcs"].get(int_of(detail, "npc_group_id"), []))
        return target("npc", ids, 1)
    if act_class == "QuestActObjExpressFire":
        # acts.rs act_event `(ObjExpressFire { express_key_id, npc_group_id }, QuestEvent::
        # ExpressFire { emotion_id, npc_id })` with `group_npcs.contains(npc_group_id, npc_id)`.
        detail = detail_of(conn, details, "quest_act_obj_express_fires", detail_id)
        ids = list(lookup["group_npcs"].get(int_of(detail, "npc_group_id"), []))
        return target("npc", ids, count_of(detail, "count"),
                      emotion_id=int_of(detail, "express_key_id"))
    if act_class in ("QuestActObjItemGroupGather", "QuestActObjItemGroupUse"):
        # acts.rs act_event `(ObjItemGroupGather/Use { item_group_id }, ...)`; members are the
        # `quest_item_group_items` rows (`data.rs` `_groupItems`).
        table = ("quest_act_obj_item_group_gathers" if act_class == "QuestActObjItemGroupGather"
                 else "quest_act_obj_item_group_uses")
        detail = detail_of(conn, details, table, detail_id)
        ids = list(lookup["group_items"].get(int_of(detail, "item_group_id"), []))
        return target("item", ids, count_of(detail, "count"))
    if act_class == "QuestActObjCraft":
        # acts.rs act_event `(ObjCraft { craft_id }, QuestEvent::Craft)`; the crafted items are
        # the `craft_products` rows of the recipe.
        detail = detail_of(conn, details, "quest_act_obj_crafts", detail_id)
        craft_id = int_of(detail, "craft_id")
        return target("item", list(lookup["craft_items"].get(craft_id, [])),
                      count_of(detail, "count"), craft_id=craft_id)
    if act_class == "QuestActObjZoneKill":
        # acts.rs act_event `(ObjZoneKill { .. }, QuestEvent::ZoneKill)`: npc victims filtered by
        # lv_min_npc/lv_max_npc (player victims by lv_min/lv_max); `data.rs` count is
        # max(count_npc, count_pk).
        detail = detail_of(conn, details, "quest_act_obj_zone_kills", detail_id)
        if int_of(detail, "count_npc") > 0:
            level_min, level_max = int_of(detail, "lv_min_npc"), int_of(detail, "lv_max_npc")
        else:
            level_min, level_max = int_of(detail, "lv_min"), int_of(detail, "lv_max")
        count = max(int_of(detail, "count_npc"), int_of(detail, "count_pk")) or 1
        return target("zone", [], count, zone_id=int_of(detail, "zone_id"),
                      level_min=level_min, level_max=level_max)
    if act_class in LEVEL_ACTS:
        # acts.rs RunAct `ObjAbilityLevel` / `ObjMateLevel` (mate_level_objective) / `ObjLevel` /
        # `ConAcceptLevelUp` compare the level (and the mate item for ObjMateLevel).
        detail = detail_of(conn, details, LEVEL_ACTS[act_class], detail_id)
        extra = {"level": int_of(detail, "LEVEL")}
        if act_class == "QuestActObjAbilityLevel":
            extra["ability_id"] = int_of(detail, "ability_id")
        elif act_class == "QuestActObjMateLevel":
            extra["item_id"] = int_of(detail, "item_id")
        return target("level", [], 1, **extra)
    if act_class == "QuestActObjCinema":
        # acts.rs act_event `(ObjCinema { cinema_id }, QuestEvent::CinemaStarted/Ended)`.
        detail = detail_of(conn, details, "quest_act_obj_cinemas", detail_id)
        return target("none", [], 1, cinema_id=int_of(detail, "cinema_id"))
    if act_class == "QuestActObjAggro":
        # acts.rs act_event `(ObjAggro { rank1..rank3 }, QuestEvent::Kill)` keys on the quest
        # acceptor NPC and the victim's aggro_percent; `data.rs` count 3 (MaxRankObjective).
        detail = detail_of(conn, details, "quest_act_obj_aggros", detail_id)
        return target("none", [], 3, rank1=int_of(detail, "rank1"), rank2=int_of(detail, "rank2"),
                      rank3=int_of(detail, "rank3"))
    if act_class == "QuestActConReportJournal":
        # acts.rs act_event `(ConReportJournal, QuestEvent::ReportJournal)`; report from the
        # journal, no world target.
        return target("none", [], 1)
    mapped = ACT_TARGETS.get(act_class)
    if mapped:
        kind, table, id_column, count_column = mapped
        detail = detail_of(conn, details, table, detail_id)
        value = detail.get(id_column) if detail else None
        return target(kind, ([value] if value else []), count_of(detail, count_column))
    if act_class == "QuestActConAutoComplete" or act_class.startswith(
            ("QuestActSupply", "QuestActCheck")):
        return target("none", [], 1)
    return None


def plan(conn, spawns, quest_filter=None, spheres=None):
    """Quest plans (sorted by quest id) plus the summary counters.

    `spheres` is the `load_spheres` index (or None without `--spheres`); sphere
    steps then carry `spheres` and their first location is the sphere centre.
    """
    contexts = rows(conn, "quest_contexts")
    components = rows(conn, "quest_components")
    acts = rows(conn, "quest_acts")
    lookups = load_lookups(conn)
    dynamic = dynamic_npc_sources(conn)
    source_data = load_item_source_data(conn)
    # `CanComponentRun` reads owner type "QuestComponent" (the only quest owner type in the
    # table, engine.rs `starter_accepts` -> `can_component_run`).
    requirements = defaultdict(list)
    for row in rows(conn, "unit_reqs"):
        if row["owner_type"] == "QuestComponent":
            requirements[row["owner_id"]].append(row)
    by_quest = defaultdict(list)
    for component in components:
        by_quest[component["quest_context_id"]].append(component)
    by_component = defaultdict(list)
    for act in acts:
        by_component[act["quest_component_id"]].append(act)
    details = {}
    quests = []
    missing_by_quest = defaultdict(set)
    sphere_quest_only = Counter()
    for quest in sorted(contexts, key=lambda q: q["id"]):
        if quest["category_id"] == TUTORIAL:
            continue
        ordered = sorted(by_quest[quest["id"]], key=lambda c: (c["component_kind_id"], c["id"]))
        requires = []
        for component in ordered:
            if component["component_kind_id"] != KIND_START:
                continue
            # `check_all` (unit_reqs.rs): `or_unit_reqs` turns the component's requirements
            # into an OR group, otherwise they must all pass.
            or_group = truth(component["or_unit_reqs"])
            for req in requirements[component["id"]]:
                entry = {"kind_id": req["kind_id"],
                         "kind": REQ_KINDS.get(req["kind_id"], f"unknown_{req['kind_id']}"),
                         "value1": req["value1"], "value2": req["value2"]}
                if or_group:
                    entry["or_group"] = True
                requires.append(entry)
        level = quest["LEVEL"] if isinstance(quest["LEVEL"], int) else 0
        min_level = max((r["value1"] for r in requires if r["kind_id"] == KIND_LEVEL),
                        default=level)
        steps = []
        blockers = []
        blocked = partial = False
        supplied = defaultdict(list)
        for component in ordered:
            kind_id = component["component_kind_id"]
            for act in sorted(by_component[component["id"]], key=lambda a: a["id"]):
                act_class = act["act_detail_type"]
                if act_class in ITEM_SUPPLY_ACTS:
                    # Earlier same-quest supplies (START/SUPPLY components run before the
                    # objective steps): the `quest_supply` item source.
                    detail = detail_of(conn, details, ITEM_SUPPLY_ACTS[act_class],
                                       act["act_detail_id"])
                    if detail and detail.get("item_id"):
                        supplied[detail["item_id"]].append(act["id"])
                target = step_target(conn, details, lookups, act_class, act["act_detail_id"])
                kind = target["kind"] if target else "none"
                ids = target["ids"] if target else []
                merged = {}
                gated = False
                if kind in ("npc", "doodad"):
                    merged = merge_locations(kind, ids, spawns)
                    gated = bool(merged) and all(gated_info(g) for g in merged.values())
                found = list(merged)
                sources = []
                item_targets = None
                if target is None:
                    status = "unsupported"
                elif kind == "npc" and not found:
                    # Summoned at runtime by a SpawnEffect skill: no static location needed.
                    sources = [source for t in ids for source in dynamic.get(t, [])]
                    status = "dynamic_spawn" if sources else "no_spawn"
                elif kind == "doodad" and not found:
                    status = "no_spawn"
                elif gated:
                    # Every placement is inside a game-schedule window (or spawn-time window).
                    status = "scheduled_spawn"
                elif kind == "item" and act_class in ITEM_SOURCE_ACTS:
                    item_targets = [{"item_id": i, "sources": item_sources(
                        i, supplied, spawns, source_data)} for i in ids]
                    status = "ok" if any(e["sources"] for e in item_targets) else "unknown_item_source"
                else:
                    status = "ok"
                step = {"component_id": component["id"],
                        "component_kind": COMPONENT_KINDS.get(kind_id, str(kind_id)),
                        "act_id": act["id"], "act_type": act_class,
                        "target": target or {"kind": "none", "ids": [], "count": 1},
                        "locations": [location_dict(placement, merged[placement])
                                      for placement in closest(found)],
                        "status": status}
                if status == "dynamic_spawn":
                    step["sources"] = sources
                if item_targets is not None:
                    step["item_sources"] = item_targets
                if spheres is not None and act_class in SPHERE_ACTS:
                    # `quest_sign_sphere.g` areas are keyed by (quest, component); fall back
                    # to any sphere of the quest when the component has none.
                    matched = spheres["by_component"].get((quest["id"], component["id"]))
                    if matched:
                        sphere_data = matched
                    else:
                        sphere_data = spheres["by_quest"].get(quest["id"], [])
                        if sphere_data:
                            sphere_quest_only[quest["id"]] += 1
                    converted = [s for s in sphere_data if "x" in s]
                    step["spheres"] = [{"world": s["world"], "x": s["x"], "y": s["y"],
                                        "z": s["z"], "radius": s["radius"],
                                        "zone_id": s["zone_id"],
                                        "component_id": s["component_id"]}
                                       for s in converted]
                    if converted:
                        centres = [(s["world"], s["x"], s["y"], s["z"]) for s in converted]
                        step["locations"] = [
                            {"world": w, "x": x, "y": y, "z": z}
                            for w, x, y, z in closest(centres)] + step["locations"]
                steps.append(step)
                optional = kind_id in OPTIONAL_KINDS
                if status == "unsupported":
                    partial = True
                    blockers.append(f"unsupported act {act_class} "
                                    f"(act {act['id']}, component {component['id']})")
                elif status == "no_spawn" and not optional:
                    blocked = True
                    missing_by_quest[quest["id"]].add(kind)
                    targets = " ".join(map(str, ids)) if ids else "unresolved"
                    blockers.append(f"no spawn for {kind} {targets} "
                                    f"({act_class}, component {component['id']})")
        if not any(c["component_kind_id"] == KIND_START for c in ordered):
            # Auxiliary records (tutorial helpers, reserved tasks): no Start component, so
            # they cannot be accepted in play (Quest-Audit wiki: 119 loaded records).
            quest_status = "auxiliary"
            blockers.insert(0, "auxiliary context: no Start component")
        else:
            quest_status = "blocked" if blocked else ("partial" if partial else "ok")
        quests.append({"quest_id": quest["id"], "name": quest["name"],
                       "category_id": quest["category_id"], "level": quest["LEVEL"],
                       "min_level": min_level, "requires": requires,
                       "prereq_quests": sorted({r["value1"] for r in requires
                                                if r["kind_id"] == KIND_COMPLETE_QUEST_CONTEXT
                                                and r["value1"]}),
                       "steps": steps, "status": quest_status, "blockers": blockers})
    by_id = {q["quest_id"]: q for q in quests}
    for quest in quests:
        notes = []
        for prereq in quest["prereq_quests"]:
            prerequisite = by_id.get(prereq)
            if prerequisite is None:
                notes.append(f"prerequisite quest {prereq} is not in the plan")
            elif prerequisite["status"] != "ok":
                notes.append(f"prerequisite quest {prereq} is {prerequisite['status']}")
        if notes:
            quest["prereq_notes"] = notes
    if quest_filter:
        quests = [q for q in quests if q["quest_id"] in quest_filter]
    summary = {"quests": len(quests),
               "status": dict(sorted(Counter(q["status"] for q in quests).items())),
               "unsupported_act_types": dict(Counter(
                   s["act_type"] for q in quests for s in q["steps"]
                   if s["status"] == "unsupported").most_common(20)),
               "blocked_missing_npc_spawns": sum(
                   "npc" in missing_by_quest[q["quest_id"]] for q in quests),
               "blocked_missing_doodad_spawns": sum(
                   "doodad" in missing_by_quest[q["quest_id"]] for q in quests),
               "dynamic_spawn_steps": sum(
                   s["status"] == "dynamic_spawn" for q in quests for s in q["steps"]),
               "sphere_steps": sum(
                   s["act_type"] in SPHERE_ACTS for q in quests for s in q["steps"]),
               "sphere_steps_with_coordinates": sum(
                   bool(s.get("spheres")) for q in quests for s in q["steps"]),
               "sphere_steps_quest_only_match": sum(
                   sphere_quest_only[q["quest_id"]] for q in quests),
               "sign_spheres_with_origin": spheres["with_origin"] if spheres else 0,
               "sign_spheres_without_origin": spheres["without_origin"] if spheres else 0,
               "scheduled_spawn_steps": sum(
                   s["status"] == "scheduled_spawn" for q in quests for s in q["steps"]),
               "unknown_item_source_steps": sum(
                   s["status"] == "unknown_item_source" for q in quests for s in q["steps"]),
               "item_steps_with_npc_loot": sum(
                   any(so["type"] == "npc_loot" for e in s.get("item_sources", [])
                       for so in e["sources"]) for q in quests for s in q["steps"]),
               "item_steps_with_doodad_sources": sum(
                   any(so["type"] == "doodad" for e in s.get("item_sources", [])
                       for so in e["sources"]) for q in quests for s in q["steps"]),
               "item_steps_with_item_open": sum(
                   any(so["type"] == "item_open" for e in s.get("item_sources", [])
                       for so in e["sources"]) for q in quests for s in q["steps"]),
               "requirement_kinds": dict(sorted(Counter(
                   r["kind"] for q in quests for r in q["requires"]).items())),
               "quests_with_requirements": sum(bool(q["requires"]) for q in quests)}
    return {"quests": quests, "summary": summary}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--db", type=Path, required=True)
    parser.add_argument("--worlds", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--quest", type=int, action="append", dest="quests",
                        help="Only plan this quest context id (repeatable)")
    parser.add_argument("--spheres", type=Path,
                        help="quest-sign-sphere export of real_compact_db_quest_accounting")
    args = parser.parse_args()
    db = args.db.resolve(strict=True)
    worlds = args.worlds.resolve(strict=True)
    if args.output.resolve() == db:
        parser.error("Output must not overwrite the source database")
    spheres = None
    if args.spheres:
        spheres = load_spheres(args.spheres.resolve(strict=True))
        if spheres["without_origin"]:
            print(
                f"warning: {spheres['without_origin']} of "
                f"{spheres['without_origin'] + spheres['with_origin']} sign spheres have no "
                'zone origin cells; add a top-level "zone_origins": {"<zone_id>": '
                "[origin_x, origin_y]} map (or per-row \"origin\": [x, y]) to the export "
                "for world coordinates. The values are the <Zone originX= originY=> "
                "attributes of the client pak world.xml "
                "(WorldTemplates.xml_world_zones, subzone.rs::zone_origin_cell).",
                file=sys.stderr)
    with sqlite3.connect(f"file:{db.as_posix()}?mode=ro", uri=True) as conn:
        conn.row_factory = sqlite3.Row
        ids = {"npc": {row["id"] for row in rows(conn, "npcs")},
               "doodad": {row["id"] for row in rows(conn, "doodad_almighties")}}
        spawner_npcs = [(row["npc_spawner_id"], row["member_id"])
                        for row in rows(conn, "npc_spawner_npcs")]
        schedule_spawners = defaultdict(list)
        for row in rows(conn, "game_schedule_spawners"):
            schedule_spawners[row["spawner_id"]].append(row["game_schedule_id"])
        schedule_doodads = defaultdict(list)
        for row in rows(conn, "game_schedule_doodads"):
            schedule_doodads[row["doodad_id"]].append(row["game_schedule_id"])
        windows = {row["id"]: (row["startTime"], row["endTime"])
                   for row in rows(conn, "npc_spawners")
                   if row["startTime"] > 0 or row["endTime"] > 0}
        known_schedules = {row["id"] for row in rows(conn, "game_schedules")}
        spawns = load_spawns(worlds, ids, spawner_npcs, schedule_spawners,
                             schedule_doodads, windows, known_schedules)
        result = plan(conn, spawns, set(args.quests) if args.quests else None, spheres)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result["quests"], ensure_ascii=False, indent=2) + "\n",
                           encoding="utf-8")
    print(json.dumps(result["summary"], indent=2))


if __name__ == "__main__":
    main()
