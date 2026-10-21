#!/usr/bin/env python3
"""Server-side backport of obsidian armor from the 3.0.4.2 data into a 1.2 copy.

Offline and off by default: the tool reads the 1.2 and 3.0.4.2 compact
databases read-only, computes the dependency closure the 1.2 server loaders
actually read (items, item_armors, item stat buffs + unit_modifiers, crafts,
craft_products, craft_materials, craft_pack_crafts, craft skills + their
skill_effects/effects/detail rows, localized texts), emits a deterministic SQL
patch, and applies it to a COPY of the 1.2 database. The live database is
never modified.

Rules:
  * ids >= CUSTOM_ID_MIN (8,000,000) are emulator custom and are never
    borrowed (custom crafts/items are refused and reported).
  * id collisions with existing 1.2 rows are refused, never overwritten.
  * only the 1.2 table shape is written (column intersection, case
    insensitive); 3.0-only columns with non-default values and 1.2-only
    columns filled with 1.2 defaults are reported.
  * the patch is a documented guess: "borrowed from 3.0.4.2".

Usage:
    python -I backport.py --base <1.2 compact.sqlite3> \
        --ref <3.0.4.2 compact.sqlite3> --items obsidian-armor \
        --out-sql obsidian-armor.sql --out-db compact-obsidian.sqlite3 \
        --report obsidian-armor-report.json

    # Batch selection (the 1.2 pak listing provides the look-availability check):
    python -I -B -X utf8 backport.py --base <1.2 compact.sqlite3> \
        --ref <3.0.4.2 compact.sqlite3> --items gear-1a \
        --pak12-listing <1.2 pak listing> --out-sql gear-1a.sql \
        --out-db compact-gear-1a.sqlite3 --report gear-1a-report.json
"""

import argparse
import hashlib
import json
import os
import re
import shutil
import sqlite3
import subprocess
import sys
import time
from collections import OrderedDict, defaultdict
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from gaps import CUSTOM_ID_MIN, Db, camel_to_snake, names_compatible

MONEY_ITEM_ID = 500
NAMED_SETS = {
    "obsidian-armor": list(range(35581, 35626)),
    # Crazed Wizard / Hermit / Slaver (equip sets 396-398): the level 50 craftable
    # cloth, leather and plate sets with set bonuses at 3/4/7 pieces and a proc at 5.
    "crazed-sets": list(range(38877, 38898)),
}

# Named families that are selected from the two databases instead of an id list.
# `gear-1a` is the zero-asset first batch of the 3.0 gear backport: costumes,
# armor and cloaks below the Eanad power line whose four playable-race looks are
# already in the 1.2 pak. `gear-1b-costumes` is its costume complement: the
# costumes whose four-race art is not in the 1.2 pak and is carried by the 3.0
# pak, shipped with an overlay pak.
GEAR_FAMILIES = ("gear-1a", "gear-1b-costumes")
GEAR_1A_MAX_ITEM_LEVEL = 62
PLAYABLE_RACES = ("nuian", "elf", "hariharan", "ferre")
NON_PLAYABLE_RACES = ("dwarf", "warborn")
# The 3.0.4.2 export carries emulator-custom Dawnsdrop sets above this id; their
# items are refused even where the item id itself is below CUSTOM_ID_MIN.
DAWNSDROP_SET_MIN = 9_000_001
DAWNSDROP_ITEM_IDS = range(34936, 34985)

TABLE_ORDER = (
    "item_categories",
    "items",
    "icons",
    "tags",
    "armor_assets",
    "item_assets",
    "item_armor_assets",
    "item_armors",
    "item_weapons",
    "item_accessories",
    "unit_modifiers",
    "item_grade_enchanting_supports",
    "item_enchanting_gems",
    "dyeable_items",
    "item_body_parts",
    "item_summon_mates",
    "item_summon_slaves",
    "item_backpacks",
    "equip_item_sets",
    "equip_item_set_bonuses",
    "item_procs",
    "item_proc_bindings",
    "buffs",
    "skill_modifiers",
    "tagged_buffs",
    "buff_tick_effects",
    "dynamic_unit_modifiers",
    "equip_item_attr_modifiers",
    "crafts",
    "craft_products",
    "craft_materials",
    "craft_pack_crafts",
    "skills",
    "skill_effects",
    "effects",
    "craft_effects",
    "buff_effects",
    "localized_texts",
)

ITEM_CLASS_TABLES = ("item_armors", "item_weapons", "item_accessories")

ITEM_SCOPED_TABLES = (
    ("unit_modifiers", "owner_id", "owner_type = 'Item'"),
    ("item_grade_enchanting_supports", "item_id", None),
    ("item_enchanting_gems", "item_id", None),
    ("dyeable_items", "item_id", None),
    ("item_body_parts", "item_id", None),
    ("item_summon_mates", "item_id", None),
    ("item_summon_slaves", "item_id", None),
    ("item_backpacks", "item_id", None),
)

# Child tables the server keys by item_id (one row per item; a second row either
# errors as a duplicate dictionary/template entry or is silently ignored). When
# the 1.2 database already has a row for an imported item id, the 3.0 values
# must replace it. `unit_modifiers` is deliberately absent: several rows per
# owner are meaningful there. The class tables are handled by `plan_class_row`.
ITEM_KEYED_TABLES = {
    "item_grade_enchanting_supports",
    "item_enchanting_gems",
    "dyeable_items",
    "item_body_parts",
    "item_summon_mates",
    "item_summon_slaves",
    "item_backpacks",
}

OBSIDIAN_WEAPON_RECIPE_IDS = (
    30565,
    30566,
    30567,
    30568,
    30569,
    30570,
    30572,
    30573,
    30574,
    30575,
    31932,
)


def resolve_item_set(spec):
    if spec in NAMED_SETS:
        return list(NAMED_SETS[spec])
    ids = []
    for part in spec.replace(";", ",").split(","):
        part = part.strip()
        if not part:
            continue
        if part in NAMED_SETS:
            ids.extend(NAMED_SETS[part])
        elif "-" in part:
            low, high = part.split("-", 1)
            ids.extend(range(int(low), int(high) + 1))
        else:
            ids.append(int(part))
    if not ids:
        raise ValueError(f"empty item set: {spec!r}")
    return sorted(set(ids))


def _pak_asset_name(path):
    """Normalize an item_assets / pak listing path to the lowercase `game/...` form."""
    path = (path or "").replace("\\", "/").strip().lower()
    if not path:
        return ""
    return path if path.startswith("game/") else "game/" + path


def _playable_race_of_path(path):
    lowered = (path or "").replace("\\", "/").lower()
    for race in PLAYABLE_RACES:
        if f"/characters/{race}/" in lowered:
            return race
    return None


def _non_playable_race_of_path(path):
    lowered = (path or "").replace("\\", "/").lower()
    for race in NON_PLAYABLE_RACES:
        if f"/characters/{race}/" in lowered:
            return race
    return None


def _armor_look_asset_ids(db, item_id):
    """`item_armors.asset_id` / `asset2_id` of an item, in row order."""
    ids = []
    if not db.has("item_armors"):
        return ids
    for row in db.rows(
        "select asset_id, asset2_id from item_armors where item_id = ? order by id",
        (item_id,),
    ):
        for column in ("asset_id", "asset2_id"):
            value = row.get(column) or 0
            if value and value not in ids:
                ids.append(value)
    return ids


def armor_look_paths(db, item_id):
    """Every `item_assets.path` reachable from the item's `item_armors` look chain."""
    paths = []
    if not (db.has("item_armor_assets") and db.has("item_assets")):
        return paths
    for armor_asset_id in _armor_look_asset_ids(db, item_id):
        for row in db.rows(
            "select ia.path from item_armor_assets iaa"
            " join item_assets ia on ia.id = iaa.asset_id"
            " where iaa.armor_asset_id = ? order by iaa.id",
            (armor_asset_id,),
        ):
            path = row.get("path")
            if path and path not in paths:
                paths.append(path)
    return paths


def _item_class_ids(ref):
    """Item id -> referenced equip set ids from the three class tables."""
    sets = defaultdict(set)
    for table in ITEM_CLASS_TABLES:
        if not ref.has(table):
            continue
        for row in ref.rows(
            f'select item_id, eiset_id from "{table}" where eiset_id > 0'
        ):
            sets[row["item_id"]].add(row["eiset_id"])
    return sets


def _set_ids_with_high_attributes(ref):
    if not (ref.has("equip_item_set_bonuses") and ref.has("unit_modifiers")):
        return set()
    return {
        row["equip_item_set_id"]
        for row in ref.rows(
            "select distinct b.equip_item_set_id from equip_item_set_bonuses b"
            " join unit_modifiers um on um.owner_id = b.buff_id"
            " and um.owner_type = 'Buff'"
            " where um.unit_attribute_id > 186"
        )
    }


def _set_ids_with_procs(ref):
    if not ref.has("equip_item_set_bonuses"):
        return set()
    return {
        row["equip_item_set_id"]
        for row in ref.rows(
            "select distinct equip_item_set_id from equip_item_set_bonuses"
            " where proc_id > 0"
        )
    }


def select_gear_1a(base, ref, pak_paths):
    """Select the report's 3.0 gear batch 1a, minus the owner's custom families.

    Selection rule (documented in the generated patch header):

    * item id absent from the 1.2 database and below `CUSTOM_ID_MIN` (8,000,000);
    * not an emulator-custom Dawnsdrop item: no `eiset_id` >= 9,000,001 and no
      item id in 34,936..34,984;
    * `items.level` <= 62 (the Eanad power line);
    * costume (category 10/173 or armor slot 31, category not 94/96/100), armor
      (category 83/84/85) or cloak (category 121);
    * no `item_proc_bindings`, no equip set with a proc or a bonus buff unit
      attribute above 186, no `use_skill_id` missing from 1.2;
    * every one of the four playable races (Nuian, Elf, Hariharan, Ferre) has at
      least one look path, and every playable-race path is already in the 1.2
      pak. Dwarf/Warborn-only paths are not required to exist (they are not
      playable in 1.2) and are reported.

    `pak_paths` is the parsed 1.2 pak listing (paths only, any case, with or
    without the `game/` prefix).
    """
    pak = {_pak_asset_name(path) for path in pak_paths or () if path}
    base_items = base.ids("items")
    base_skills = base.ids("skills")
    sets = _item_class_ids(ref)
    high_attribute_sets = _set_ids_with_high_attributes(ref)
    proc_sets = _set_ids_with_procs(ref)
    proc_items = (
        {row["item_id"] for row in ref.rows("select distinct item_id from item_proc_bindings")}
        if ref.has("item_proc_bindings")
        else set()
    )
    item_columns = set(ref.columns("items"))

    skipped = defaultdict(lambda: {"count": 0, "examples": []})

    def skip(reason, item_id):
        entry = skipped[reason]
        entry["count"] += 1
        if len(entry["examples"]) < 20:
            entry["examples"].append(item_id)

    def classify(item_id, category_id):
        if not ref.has("item_armors"):
            return None
        has_armor = ref.row(
            "select 1 from item_armors where item_id = ? limit 1", (item_id,)
        )
        if (
            has_armor
            and category_id not in (94, 96, 100)
            and (
                category_id in (10, 173)
                or ref.row(
                    "select 1 from item_armors where item_id = ? and slot_type_id = 31"
                    " limit 1",
                    (item_id,),
                )
            )
        ):
            return "costume"
        if category_id in (83, 84, 85):
            return "armor"
        if category_id == 121:
            return "cloak"
        return None

    selected = []
    counts = defaultdict(int)
    all_paths = set()
    armor_asset_ids = set()
    for item in ref.rows("select * from items order by id"):
        item_id = item["id"]
        if item_id in base_items:
            continue
        if item_id >= CUSTOM_ID_MIN:
            skip("custom_id_ge_8m", item_id)
            continue
        set_ids = sets.get(item_id, set())
        if any(set_id >= DAWNSDROP_SET_MIN for set_id in set_ids):
            skip("custom_dawnsdrop_set", item_id)
            continue
        if item_id in DAWNSDROP_ITEM_IDS:
            skip("custom_dawnsdrop_item", item_id)
            continue
        category_id = item.get("category_id") or 0
        kind = classify(item_id, category_id)
        if kind is None:
            skip("outside_batch_family", item_id)
            continue
        level = item.get("level", item.get("LEVEL")) or 0
        if level > GEAR_1A_MAX_ITEM_LEVEL:
            skip("above_eanad", item_id)
            continue
        if item_id in proc_items:
            skip("item_proc", item_id)
            continue
        if set_ids & high_attribute_sets:
            skip("set_attribute_gt_186", item_id)
            continue
        if set_ids & proc_sets:
            skip("set_proc", item_id)
            continue
        use_skill_id = (item.get("use_skill_id") if "use_skill_id" in item_columns else 0) or 0
        if use_skill_id and use_skill_id not in base_skills:
            skip("new_use_skill", item_id)
            continue
        paths = armor_look_paths(ref, item_id)
        races = {_playable_race_of_path(path) for path in paths}
        races.discard(None)
        if len(races) != len(PLAYABLE_RACES):
            skip("no_four_race_art", item_id)
            continue
        playable_paths = [
            path for path in paths if _playable_race_of_path(path) is not None
        ]
        if not playable_paths or any(
            _pak_asset_name(path) not in pak for path in playable_paths
        ):
            skip("playable_art_missing_from_1_2_pak", item_id)
            continue
        selected.append(item_id)
        counts[kind] += 1
        all_paths.update(paths)
        armor_asset_ids.update(_armor_look_asset_ids(ref, item_id))

    missing_paths = sorted(
        path for path in all_paths if _pak_asset_name(path) not in pak
    )
    return {
        "family": "gear-1a",
        "items": selected,
        "counts": {
            "costume": counts["costume"],
            "armor": counts["armor"],
            "cloak": counts["cloak"],
            "total": len(selected),
        },
        "skipped": {reason: dict(entry) for reason, entry in sorted(skipped.items())},
        "looks": {
            "distinct_paths": len(all_paths),
            "armor_asset_ids": sorted(armor_asset_ids),
            "missing_from_1_2_pak": missing_paths,
        },
    }


def select_gear_1b_costumes(base, ref, pak12_paths, pak30_paths, skip_paths=None):
    """Select the costume complement of batch 1a, carried by a 3.0 overlay pak.

    Selection rule (documented in the generated patch header):

    * item id absent from the base (1.2 plus the earlier batch 1a) and below
      `CUSTOM_ID_MIN` (8,000,000);
    * not an emulator-custom Dawnsdrop item: no `eiset_id` >= 9,000,001 and no
      item id in 34,936..34,984;
    * `items.level` <= 62 (the Eanad power line);
    * costume only (category 10/173 or armor slot 31, category not 94/96/100);
    * no `item_proc_bindings`, no equip set with a proc or a bonus buff unit
      attribute above 186, no `use_skill_id` missing from 1.2;
    * a look for each of the four playable races, with at least one playable
      path absent from the 1.2 pak (otherwise batch 1a already has it) and every
      playable path present in the 3.0 pak (the overlay source). Dwarf/Warborn
      paths stay data-only;
    * no look path in `skip_paths`, the direct models whose material -> texture
      closure still has a reference no source pak can satisfy after the
      overlay's texture repair pass.

    `base` is expected to be the 1.2 database with the earlier compact patches
    (batch 1a) applied, so synthetic ids are allocated above their rows.
    """
    pak12 = {_pak_asset_name(path) for path in pak12_paths or () if path}
    pak30 = {_pak_asset_name(path) for path in pak30_paths or () if path}
    skip_paths = {_pak_asset_name(path) for path in skip_paths or () if path}
    base_items = base.ids("items")
    base_skills = base.ids("skills")
    sets = _item_class_ids(ref)
    high_attribute_sets = _set_ids_with_high_attributes(ref)
    proc_sets = _set_ids_with_procs(ref)
    proc_items = (
        {row["item_id"] for row in ref.rows("select distinct item_id from item_proc_bindings")}
        if ref.has("item_proc_bindings")
        else set()
    )
    item_columns = set(ref.columns("items"))

    skipped = defaultdict(lambda: {"count": 0, "examples": []})

    def skip(reason, item_id):
        entry = skipped[reason]
        entry["count"] += 1
        if len(entry["examples"]) < 20:
            entry["examples"].append(item_id)

    selected = []
    all_paths = set()
    playable_paths = set()
    dwarf_warborn_paths = set()
    other_paths = set()
    skipped_missing_textures = []
    for item in ref.rows("select * from items order by id"):
        item_id = item["id"]
        if item_id in base_items:
            # 1.2 content or batch 1a (the base carries the earlier patch).
            continue
        if item_id >= CUSTOM_ID_MIN:
            skip("custom_id_ge_8m", item_id)
            continue
        set_ids = sets.get(item_id, set())
        if any(set_id >= DAWNSDROP_SET_MIN for set_id in set_ids):
            skip("custom_dawnsdrop_set", item_id)
            continue
        if item_id in DAWNSDROP_ITEM_IDS:
            skip("custom_dawnsdrop_item", item_id)
            continue
        category_id = item.get("category_id") or 0
        if not ref.has("item_armors"):
            skip("outside_batch_family", item_id)
            continue
        has_armor = ref.row(
            "select 1 from item_armors where item_id = ? limit 1", (item_id,)
        )
        is_costume = bool(
            has_armor
            and category_id not in (94, 96, 100)
            and (
                category_id in (10, 173)
                or ref.row(
                    "select 1 from item_armors where item_id = ? and slot_type_id = 31"
                    " limit 1",
                    (item_id,),
                )
            )
        )
        if not is_costume:
            skip("outside_batch_family", item_id)
            continue
        level = item.get("level", item.get("LEVEL")) or 0
        if level > GEAR_1A_MAX_ITEM_LEVEL:
            skip("above_eanad", item_id)
            continue
        if item_id in proc_items:
            skip("item_proc", item_id)
            continue
        if set_ids & high_attribute_sets:
            skip("set_attribute_gt_186", item_id)
            continue
        if set_ids & proc_sets:
            skip("set_proc", item_id)
            continue
        use_skill_id = (item.get("use_skill_id") if "use_skill_id" in item_columns else 0) or 0
        if use_skill_id and use_skill_id not in base_skills:
            skip("new_use_skill", item_id)
            continue
        paths = armor_look_paths(ref, item_id)
        missing_texture_paths = sorted(
            {_pak_asset_name(path) for path in paths} & skip_paths
        )
        if missing_texture_paths:
            skip("missing_texture_unresolved", item_id)
            skipped_missing_textures.append(
                {"item_id": item_id, "paths": missing_texture_paths}
            )
            continue
        races = {_playable_race_of_path(path) for path in paths}
        races.discard(None)
        if len(races) != len(PLAYABLE_RACES):
            skip("no_four_race_art", item_id)
            continue
        playable = [
            path for path in paths if _playable_race_of_path(path) is not None
        ]
        if not playable:
            skip("no_four_race_art", item_id)
            continue
        if all(_pak_asset_name(path) in pak12 for path in playable):
            skip("zero_asset_in_1_2", item_id)
            continue
        if any(_pak_asset_name(path) not in pak30 for path in playable):
            skip("playable_art_missing_from_3_0", item_id)
            continue
        selected.append(item_id)
        for path in paths:
            name = _pak_asset_name(path)
            all_paths.add(name)
            if _playable_race_of_path(path) is not None:
                playable_paths.add(name)
            elif _non_playable_race_of_path(path) is not None:
                dwarf_warborn_paths.add(name)
            else:
                other_paths.add(name)

    return {
        "family": "gear-1b-costumes",
        "items": selected,
        "counts": {
            "costume": len(selected),
            "total": len(selected),
        },
        "skipped": {reason: dict(entry) for reason, entry in sorted(skipped.items())},
        "skipped_missing_textures": skipped_missing_textures,
        "looks": {
            "distinct_paths": len(all_paths),
            "playable_paths": len(playable_paths),
            "dwarf_warborn_paths": len(dwarf_warborn_paths),
            "other_paths": len(other_paths),
            "new_playable_paths": sorted(playable_paths - pak12),
            "shared_playable_paths": sorted(playable_paths & pak12),
            "missing_from_1_2_pak": sorted(all_paths - pak12),
            "missing_from_3_0_pak": sorted(all_paths - pak30),
        },
    }


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def is_default_value(value):
    if value is None:
        return True
    if isinstance(value, (int, float)) and value == 0:
        return True
    if isinstance(value, str) and value == "":
        return True
    return False


def sql_literal(value):
    if value is None:
        return "NULL"
    if isinstance(value, bool):
        return "1" if value else "0"
    if isinstance(value, (int, float)):
        return repr(value)
    if isinstance(value, bytes):
        return "X'" + value.hex() + "'"
    return "'" + str(value).replace("'", "''") + "'"


class ColumnInfo:
    def __init__(self, name, type_name, notnull, dflt_value):
        self.name = name
        self.type = (type_name or "").upper()
        self.notnull = notnull
        self.dflt_value = dflt_value


class Borrower:
    def __init__(self, base, ref, skip_craft_pack_crafts=False):
        self.base = base
        self.ref = ref
        self.skip_craft_pack_crafts = skip_craft_pack_crafts
        self.rows = OrderedDict()
        self.seen_ids = {}
        self.synth_next = {}
        self._colinfo = {}
        self._colmap = {}
        self._defaults = {}
        self._sample_defaults = {}
        self.report = {
            "synthesized_ids": defaultdict(lambda: {"count": 0, "first": None, "last": None}),
            "columns_filled_with_1_2_defaults": defaultdict(lambda: defaultdict(int)),
            "columns_dropped_from_3_0": defaultdict(
                lambda: defaultdict(lambda: {"count": 0, "examples": []})
            ),
            "refused": {
                "custom_items": [],
                "custom_crafts": [],
                "collisions": [],
                "name_conflicts": [],
                "missing_references": [],
            },
            "already_present": [],
            "borrowed_rows": defaultdict(int),
            "replaced_rows": defaultdict(int),
            "duplicate_item_keyed_rows": [],
            "boolean_value_conversions": defaultdict(lambda: defaultdict(int)),
            "materials_without_1_2_source": [],
            "icons_missing_in_1_2": [],
            "buff_effects_referenced_buffs_missing_in_1_2": [],
            "craft_pack_crafts_semantics": None,
        }
        self._buff_effect_buff_ids = set()
        self.seen_sets = set()
        self.seen_localized_keys = set()
        self.seen_item_keyed = set()
        self.replacements = defaultdict(list)
        self._max_unit_attribute = None
        self.armor_asset_map = {}
        self.item_asset_map = {}
        self.icon_map = {}
        self._base_item_asset_by_path = None
        self.report["look_borrows"] = {
            "armor_assets": {"new_ids": [], "reused_ids": []},
            "item_assets": {"new_ids": [], "reused_ids": []},
            "item_armor_assets": {"new_ids": []},
            "icon_borrows": {"borrowed_ids": [], "reused_ids": [], "remapped": []},
            "item_armors_asset_id_remap": {},
            "server_reads_look_tables": {
                "armor_assets": False,
                "item_armor_assets": False,
                "item_assets": False,
                "icons": False,
            },
        }

    # -- schema helpers ---------------------------------------------------

    def colinfo(self, table):
        if table not in self._colinfo:
            rows = self.base.conn.execute(f'pragma table_info("{table}")').fetchall()
            self._colinfo[table] = [
                ColumnInfo(r[1], r[2], r[3], r[4]) for r in rows
            ]
        return self._colinfo[table]

    def colmap(self, table):
        if table not in self._colmap:
            base_cols = [c.name for c in self.colinfo(table)]
            ref_lower = {c.lower(): c for c in self.ref.columns(table)}
            mapped = {}
            defaults = []
            for base_col in base_cols:
                ref_col = ref_lower.get(base_col.lower())
                if ref_col is None:
                    defaults.append(base_col)
                else:
                    mapped[base_col] = ref_col
            self._colmap[table] = (base_cols, mapped, defaults)
        return self._colmap[table]

    def base_max_id(self, table):
        """Highest retail (below `CUSTOM_ID_MIN`) id of a table."""
        if not self.base.has(table):
            return 0
        row = self.base.row(
            f'select max(id) as m from "{table}" where id < ?', (CUSTOM_ID_MIN,)
        )
        return (row["m"] if row else 0) or 0

    def base_max_all_id(self, table):
        if not self.base.has(table):
            return 0
        row = self.base.row(f'select max(id) as m from "{table}"')
        return (row["m"] if row else 0) or 0

    def next_synth_id(self, table):
        if table not in self.synth_next:
            base_max = self.base_max_id(table)
            self.synth_next[table] = base_max
            entry = self.report["synthesized_ids"][table]
            entry["base_max"] = base_max
            entry["base_max_all"] = self.base_max_all_id(table)
            entry["above_custom_min"] = entry["base_max_all"] >= CUSTOM_ID_MIN
        while True:
            self.synth_next[table] += 1
            candidate = self.synth_next[table]
            if candidate in self.seen_ids.get(table, set()):
                continue
            if candidate in self.base.ids(table):
                continue
            entry = self.report["synthesized_ids"][table]
            entry["count"] += 1
            if entry["first"] is None:
                entry["first"] = candidate
            entry["last"] = candidate
            return candidate

    def default_value(self, table, column):
        key = (table, column)
        if key in self._defaults:
            return self._defaults[key]
        info = next((c for c in self.colinfo(table) if c.name == column), None)
        value = None
        if info is not None and info.dflt_value is not None:
            raw = str(info.dflt_value)
            if raw.upper() == "NULL":
                value = None
            elif raw.startswith("'") and raw.endswith("'"):
                value = raw[1:-1].replace("''", "'")
            else:
                try:
                    value = int(raw)
                except ValueError:
                    try:
                        value = float(raw)
                    except ValueError:
                        value = raw
        if value is None:
            sample = self._sample_default(table, column)
            if sample is not None:
                value = sample
            elif info is not None and any(
                t in info.type for t in ("INT", "REAL", "FLOA", "DOUB", "NUM")
            ):
                value = 0
            elif info is not None and "BLOB" in info.type:
                value = None
            else:
                value = ""
        self._defaults[key] = value
        return value

    def _sample_default(self, table, column):
        key = (table, column)
        if key in self._sample_defaults:
            return self._sample_defaults[key]
        sample = None
        if self.base.has(table):
            rows = self.base.conn.execute(
                f'select distinct "{column}" from "{table}"'
                f' where "{column}" is not null limit 8'
            ).fetchall()
            values = [r[0] for r in rows]
            if values and all(isinstance(v, str) and v in ("t", "f") for v in values):
                sample = "f"
            elif values and all(isinstance(v, (int, float)) for v in values):
                sample = 0
        self._sample_defaults[key] = sample
        return sample

    def _normalize_value(self, table, column, value):
        if value is None:
            return None
        kind = self._sample_default(table, column)
        if kind == "f" and isinstance(value, int) and value in (0, 1):
            self.report["boolean_value_conversions"][table][column] += 1
            return "t" if value else "f"
        if kind == 0 and isinstance(value, str) and value in ("t", "f"):
            self.report["boolean_value_conversions"][table][column] += 1
            return 1 if value == "t" else 0
        return value

    # -- row planning -----------------------------------------------------

    def _row_values(self, table, ref_row, id_value=None, record=True):
        base_cols, mapped, defaults = self.colmap(table)
        row = {}
        for base_col in base_cols:
            ref_col = mapped.get(base_col)
            if ref_col is None:
                row[base_col] = self.default_value(table, base_col)
                if record:
                    self.report["columns_filled_with_1_2_defaults"][table][
                        base_col
                    ] += 1
            else:
                row[base_col] = self._normalize_value(
                    table, base_col, ref_row.get(ref_col)
                )
        if id_value is not None:
            row["id"] = id_value
        if record:
            ref_only = [
                c
                for c in self.ref.columns(table)
                if c.lower() not in {b.lower() for b in base_cols}
            ]
            for col in ref_only:
                value = ref_row.get(col)
                if not is_default_value(value):
                    entry = self.report["columns_dropped_from_3_0"][table][col]
                    entry["count"] += 1
                    if len(entry["examples"]) < 20:
                        entry["examples"].append(
                            {"id": id_value, "value": value}
                        )
        return row

    def _plan_row(self, table, ref_row, id_value):
        row = self._row_values(table, ref_row, id_value)
        self.rows.setdefault(table, []).append(row)
        self.seen_ids.setdefault(table, set()).add(id_value)
        self.report["borrowed_rows"][table] += 1
        return row

    def plan_row(self, table, ref_row):
        rid = ref_row.get("id")
        if rid is not None and rid in self.seen_ids.get(table, set()):
            return None
        if rid is not None and rid in self.base.ids(table):
            self.report["refused"]["collisions"].append(
                {"table": table, "id": rid}
            )
            return None
        if rid is None:
            rid = self.next_synth_id(table)
        return self._plan_row(table, ref_row, rid)

    def _class_row_matches(self, base_row, values):
        for column, value in values.items():
            if column == "id":
                continue
            base_value = base_row.get(column)
            if base_value == value:
                continue
            if base_value in (None, 0, "") and value in (None, 0, ""):
                continue
            return False
        return True

    def _plan_replacement(self, table, ref_row, id_value, values=None):
        row = dict(values) if values is not None else self._row_values(table, ref_row)
        row["id"] = id_value
        self.replacements[table].append(row)
        self.seen_ids.setdefault(table, set()).add(id_value)
        self.report["replaced_rows"][table] += 1
        return row

    def plan_class_row(self, table, ref_row):
        """Plan an `item_armors` / `item_weapons` / `item_accessories` row.

        The class-table `id` is a surrogate key the server never references: it
        is re-keyed above the 1.2 range when it collides with a row of another
        item. The 1.2 database also carries orphan class rows for items its
        `items` table does not have; when the imported item already has such a
        row, the 3.0 values replace it (`UPDATE`) instead of adding a second
        template (the loader rejects duplicate item templates).
        """
        item_id = ref_row.get("item_id")
        base_row = None
        if item_id is not None and self.base.has(table):
            candidates = self.base.rows(
                f'select * from "{table}" where item_id = ? order by id', (item_id,)
            )
            if candidates:
                base_row = next(
                    (row for row in candidates if row.get("id") == ref_row.get("id")),
                    candidates[0],
                )
        values = self._row_values(table, ref_row, record=False)
        if table == "item_armors":
            asset_id = ref_row.get("asset_id") or 0
            if asset_id:
                values["asset_id"] = self.borrow_armor_look(asset_id)
        if base_row is not None:
            if self._class_row_matches(base_row, values):
                self.report["already_present"].append(
                    {"table": table, "id": base_row["id"]}
                )
                return None
            values = self._row_values(table, ref_row)
            if table == "item_armors":
                asset_id = ref_row.get("asset_id") or 0
                if asset_id:
                    values["asset_id"] = self.borrow_armor_look(asset_id)
            return self._plan_replacement(
                table, ref_row, base_row["id"], values=values
            )
        rid = ref_row.get("id")
        if rid is None or rid in self.base.ids(table):
            rid = self.next_synth_id(table)
        planned = self._plan_row(table, ref_row, rid)
        if table == "item_armors":
            asset_id = ref_row.get("asset_id") or 0
            if asset_id:
                planned["asset_id"] = self.borrow_armor_look(asset_id)
        return planned

    def plan_item_keyed_row(self, table, ref_row):
        """Plan a child row the server keys by `item_id` (one row per item).

        `dyeable_items` errors on a duplicate item id; `item_grade_enchanting_supports`
        keeps the first row; the body-part / summon / backpack rows are templates and
        reject a duplicate item template. A 1.2 orphan row for an imported item is
        therefore updated in place instead of inserting a second row. When the 3.0
        source itself carries several rows for one item (three dyeable costumes do),
        only the first is imported; the rest are reported as dropped duplicates.
        """
        item_id = ref_row.get("item_id")
        key = (table, item_id)
        if key in self.seen_item_keyed:
            self.report["duplicate_item_keyed_rows"].append(
                {
                    "table": table,
                    "item_id": item_id,
                    "dropped_id": ref_row.get("id"),
                }
            )
            return None
        self.seen_item_keyed.add(key)
        base_row = None
        if item_id is not None and self.base.has(table):
            candidates = self.base.rows(
                f'select * from "{table}" where item_id = ? order by id', (item_id,)
            )
            if candidates:
                base_row = next(
                    (row for row in candidates if row.get("id") == ref_row.get("id")),
                    candidates[0],
                )
        if base_row is not None:
            values = self._row_values(table, ref_row, record=False)
            if self._class_row_matches(base_row, values):
                self.report["already_present"].append(
                    {"table": table, "id": base_row["id"]}
                )
                return None
            values = self._row_values(table, ref_row)
            return self._plan_replacement(
                table, ref_row, base_row["id"], values=values
            )
        rid = ref_row.get("id")
        if rid is not None and rid in self.base.ids(table):
            ref_row = dict(ref_row)
            ref_row["id"] = self.next_synth_id(table)
        return self.plan_row(table, ref_row)

    # -- closure borrowing ------------------------------------------------

    def _base_item_assets_by_path(self):
        if self._base_item_asset_by_path is None:
            self._base_item_asset_by_path = {}
            if self.base.has("item_assets"):
                for row in self.base.rows("select id, path from item_assets"):
                    if row["path"]:
                        self._base_item_asset_by_path[row["path"].lower()] = row["id"]
        return self._base_item_asset_by_path

    def _chain_paths(self, db, armor_asset_id):
        if not db.has("item_armor_assets") or not db.has("item_assets"):
            return {}
        return {
            row["path"]: row["asset_id"]
            for row in db.rows(
                "select iaa.asset_id, ia.path from item_armor_assets iaa"
                " join item_assets ia on ia.id = iaa.asset_id"
                " where iaa.armor_asset_id = ?",
                (armor_asset_id,),
            )
        }

    def _asset_id_for_path(self, ref_asset_id):
        if ref_asset_id in self.item_asset_map:
            return self.item_asset_map[ref_asset_id]
        ref_row = self.ref.row("select * from item_assets where id = ?", (ref_asset_id,))
        if ref_row is None or not ref_row.get("path"):
            self.report["refused"]["missing_references"].append(
                {"table": "item_assets", "id": ref_asset_id}
            )
            self.item_asset_map[ref_asset_id] = ref_asset_id
            return ref_asset_id
        path = ref_row["path"].lower()
        base_id = self._base_item_assets_by_path().get(path)
        if base_id is not None:
            self.item_asset_map[ref_asset_id] = base_id
            self.report["look_borrows"]["item_assets"]["reused_ids"].append(
                {"ref_id": ref_asset_id, "id": base_id, "path": ref_row["path"]}
            )
            return base_id
        new_id = self.next_synth_id("item_assets")
        self._plan_row("item_assets", ref_row, new_id)
        self.item_asset_map[ref_asset_id] = new_id
        self.report["look_borrows"]["item_assets"]["new_ids"].append(new_id)
        return new_id

    def borrow_armor_look(self, armor_asset_id):
        if armor_asset_id in self.armor_asset_map:
            return self.armor_asset_map[armor_asset_id]
        if not (
            self.ref.has("armor_assets")
            and self.ref.has("item_armor_assets")
            and self.ref.has("item_assets")
        ):
            self.armor_asset_map[armor_asset_id] = armor_asset_id
            return armor_asset_id
        ref_row = self.ref.row("select * from armor_assets where id = ?", (armor_asset_id,))
        if ref_row is None:
            self.report["refused"]["missing_references"].append(
                {"table": "armor_assets", "id": armor_asset_id}
            )
            self.armor_asset_map[armor_asset_id] = armor_asset_id
            return armor_asset_id
        base_paths = self._chain_paths(self.base, armor_asset_id)
        ref_paths = self._chain_paths(self.ref, armor_asset_id)
        same_meaning = bool(ref_paths) and set(base_paths) == set(ref_paths)
        if same_meaning:
            # The 1.2 look has the same path chain: reuse its id and leave its
            # item_armor_assets rows alone (adding the 3.0 rows again would
            # duplicate every path under a new set of ids).
            self.armor_asset_map[armor_asset_id] = armor_asset_id
            self.report["look_borrows"]["armor_assets"]["reused_ids"].append(
                {"id": armor_asset_id, "reason": "identical item_armor_assets chain"}
            )
            return armor_asset_id
        new_id = self.next_synth_id("armor_assets")
        planned = self._plan_row("armor_assets", ref_row, new_id)
        default_asset = ref_row.get("default_asset_id") or 0
        if default_asset:
            planned["default_asset_id"] = self._asset_id_for_path(default_asset)
        self.report["look_borrows"]["armor_assets"]["new_ids"].append(new_id)
        self.report["look_borrows"]["item_armors_asset_id_remap"][
            str(armor_asset_id)
        ] = new_id
        self.armor_asset_map[armor_asset_id] = new_id
        for iaa_row in self.ref.rows(
            "select * from item_armor_assets where armor_asset_id = ? order by id",
            (armor_asset_id,),
        ):
            asset_id = self._asset_id_for_path(iaa_row["asset_id"])
            row = dict(iaa_row)
            row["armor_asset_id"] = new_id
            row["asset_id"] = asset_id
            row_id = self.next_synth_id("item_armor_assets")
            row["id"] = row_id
            self._plan_row("item_armor_assets", row, row_id)
            self.report["look_borrows"]["item_armor_assets"]["new_ids"].append(row_id)
        return new_id

    def borrow_icon(self, icon_id):
        if icon_id in self.icon_map:
            return self.icon_map[icon_id]
        if not self.ref.has("icons"):
            self.icon_map[icon_id] = icon_id
            return icon_id
        ref_row = self.ref.row("select * from icons where id = ?", (icon_id,))
        if ref_row is None:
            self.report["refused"]["missing_references"].append(
                {"table": "icons", "id": icon_id}
            )
            self.icon_map[icon_id] = icon_id
            return icon_id
        base_row = self.base.row(
            "select id, filename from icons where id = ?", (icon_id,)
        )
        ref_filename = (ref_row.get("filename") or "").lower()
        if base_row is not None and (base_row["filename"] or "").lower() == ref_filename:
            self.icon_map[icon_id] = icon_id
            self.report["look_borrows"]["icon_borrows"]["reused_ids"].append(
                {"id": icon_id, "filename": ref_row.get("filename")}
            )
            return icon_id
        if base_row is None:
            new_id = icon_id
        else:
            new_id = self.next_synth_id("icons")
            self.report["look_borrows"]["icon_borrows"]["remapped"].append(
                {
                    "ref_id": icon_id,
                    "id": new_id,
                    "base_filename": base_row["filename"],
                    "ref_filename": ref_row.get("filename"),
                }
            )
        self._plan_row("icons", ref_row, new_id)
        self.icon_map[icon_id] = new_id
        self.report["look_borrows"]["icon_borrows"]["borrowed_ids"].append(new_id)
        return new_id

    def borrow_item(self, item_id, start=False):
        existing = self.base.row("select id, name from items where id = ?", (item_id,))
        if existing is not None:
            ref_row = self.ref.row("select name from items where id = ?", (item_id,))
            if ref_row is not None and not names_compatible(
                existing["name"], ref_row["name"]
            ):
                self.report["refused"]["name_conflicts"].append(
                    {
                        "table": "items",
                        "id": item_id,
                        "base_name": existing["name"],
                        "ref_name": ref_row["name"],
                    }
                )
            else:
                self.report["already_present"].append({"table": "items", "id": item_id})
            return
        if item_id >= CUSTOM_ID_MIN:
            self.report["refused"]["custom_items"].append({"table": "items", "id": item_id})
            return
        ref_row = self.ref.row("select * from items where id = ?", (item_id,))
        if ref_row is None:
            self.report["refused"]["missing_references"].append(
                {"table": "items", "id": item_id}
            )
            return
        self.borrow_item_category(ref_row.get("category_id") or 0)
        item_row = self._plan_row("items", ref_row, item_id)
        for table in ITEM_CLASS_TABLES:
            if not self.ref.has(table):
                continue
            for row in self.ref.rows(
                f'select * from "{table}" where item_id = ? order by id', (item_id,)
            ):
                planned = self.plan_class_row(table, row)
                if table == "item_armors" and planned is not None:
                    self._check_armor_references(row)
        for table, key, condition in ITEM_SCOPED_TABLES:
            if not self.ref.has(table):
                continue
            where = f'{key} = ?' + (f" and {condition}" if condition else "")
            for row in self.ref.rows(
                f'select * from "{table}" where {where} order by rowid', (item_id,)
            ):
                if table in ITEM_KEYED_TABLES:
                    self.plan_item_keyed_row(table, row)
                else:
                    self.plan_row(table, row)
        for table in ITEM_CLASS_TABLES:
            class_rows = list(self.rows.get(table, []))
            class_rows += list(self.replacements.get(table, []))
            for row in class_rows:
                if row.get("item_id") != item_id:
                    continue
                set_id = row.get("eiset_id") or 0
                if set_id:
                    self.borrow_equip_set(set_id)
        if self.ref.has("item_proc_bindings") and self.base.has("item_proc_bindings"):
            for row in self.ref.rows(
                "select * from item_proc_bindings where item_id = ? order by id", (item_id,)
            ):
                if self.plan_row("item_proc_bindings", row) is not None:
                    self.borrow_item_proc(row["proc_id"])
        buff_id = ref_row.get("buff_id") or 0
        if buff_id > 0:
            self.borrow_buff(buff_id, source="item", item_id=item_id)
        for column in ("icon_id", "over_icon_id", "male_icon_id"):
            icon_id = ref_row.get(column) or 0
            if not icon_id:
                continue
            if icon_id not in self.base.ids("icons"):
                self.report["icons_missing_in_1_2"].append(
                    {"item_id": item_id, "icon_id": icon_id, "column": column}
                )
            item_row[column] = self.borrow_icon(icon_id)

    def borrow_item_category(self, category_id):
        if not category_id or not self.base.has("item_categories"):
            return
        if category_id in self.base.ids("item_categories"):
            return
        if category_id in self.seen_ids.get("item_categories", set()):
            return
        if category_id >= CUSTOM_ID_MIN:
            self.report["refused"]["custom_items"].append(
                {"table": "item_categories", "id": category_id}
            )
            return
        ref_row = self.ref.row(
            "select * from item_categories where id = ?", (category_id,)
        )
        if ref_row is None:
            self.report["refused"]["missing_references"].append(
                {"table": "item_categories", "id": category_id}
            )
            return
        self._plan_row("item_categories", ref_row, category_id)
        group_id = ref_row.get("item_group_id") or 0
        if (
            group_id
            and self.base.has("item_groups")
            and not self.base.row("select 1 from item_groups where id = ?", (group_id,))
        ):
            self.report["refused"]["missing_references"].append(
                {"table": "item_groups", "id": group_id, "needed_by": "item_categories"}
            )

    def borrow_tag(self, tag_id):
        """Plan a missing tag row; `True` when the tag exists (base or planned)."""
        if not tag_id:
            return True
        if tag_id in self.base.ids("tags") or tag_id in self.seen_ids.get("tags", set()):
            return True
        if not (self.base.has("tags") and self.ref.has("tags")):
            return False
        if tag_id >= CUSTOM_ID_MIN:
            self.report["refused"]["custom_items"].append(
                {"table": "tags", "id": tag_id}
            )
            return False
        ref_row = self.ref.row("select * from tags where id = ?", (tag_id,))
        if ref_row is None:
            return False
        self._plan_row("tags", ref_row, tag_id)
        return True

    def _check_armor_references(self, row):
        mod_set_id = row.get("mod_set_id") or 0
        if mod_set_id and mod_set_id not in self.base.ids("equip_item_attr_modifiers"):
            ref_row = self.ref.row(
                "select * from equip_item_attr_modifiers where id = ?", (mod_set_id,)
            )
            if ref_row is not None:
                self.plan_row("equip_item_attr_modifiers", ref_row)
            else:
                self.report["refused"]["missing_references"].append(
                    {"table": "equip_item_attr_modifiers", "id": mod_set_id}
                )

    def borrow_buff(self, buff_id, source, item_id=None):
        if buff_id in self.seen_ids.get("buffs", set()):
            return
        if buff_id in self.base.ids("buffs"):
            ref_row = self.ref.row("select name from buffs where id = ?", (buff_id,))
            base_row = self.base.row("select name from buffs where id = ?", (buff_id,))
            if ref_row is not None and base_row is not None and not names_compatible(
                base_row["name"], ref_row["name"]
            ):
                self.report["refused"]["name_conflicts"].append(
                    {
                        "table": "buffs",
                        "id": buff_id,
                        "base_name": base_row["name"],
                        "ref_name": ref_row["name"],
                    }
                )
            return
        if buff_id >= CUSTOM_ID_MIN:
            self.report["refused"]["custom_items"].append({"table": "buffs", "id": buff_id})
            return
        ref_row = self.ref.row("select * from buffs where id = ?", (buff_id,))
        if ref_row is None:
            self.report["refused"]["missing_references"].append(
                {"table": "buffs", "id": buff_id}
            )
            return
        self._plan_row("buffs", ref_row, buff_id)
        if self.ref.has("unit_modifiers"):
            for row in self.ref.rows(
                "select * from unit_modifiers where owner_type = 'Buff' and owner_id = ?"
                " order by unit_attribute_id, unit_modifier_type_id, value",
                (buff_id,),
            ):
                if self.unknown_unit_attribute(row):
                    self.report.setdefault("unit_modifiers_unknown_in_1_2", []).append(
                        {
                            "buff_id": buff_id,
                            "unit_attribute_id": row.get("unit_attribute_id"),
                            "value": row.get("value"),
                        }
                    )
                    continue
                self.plan_row("unit_modifiers", row)
        if self.ref.has("skill_modifiers") and self.base.has("skill_modifiers"):
            for row in self.ref.rows(
                "select * from skill_modifiers where owner_type = 'Buff' and owner_id = ?"
                " order by rowid",
                (buff_id,),
            ):
                if not self._check_skill_modifier_refs(row):
                    self.report.setdefault("skill_modifiers_skipped", []).append(
                        {
                            "buff_id": buff_id,
                            "skill_id": row.get("skill_id"),
                            "tag_id": row.get("tag_id"),
                            "reason": "skill or tag missing from 1.2",
                        }
                    )
                    continue
                self.plan_row("skill_modifiers", row)
        if self.ref.has("tagged_buffs") and self.base.has("tagged_buffs"):
            for row in self.ref.rows(
                "select * from tagged_buffs where buff_id = ? order by rowid", (buff_id,)
            ):
                if not self._check_tag(row.get("tag_id"), "tagged_buffs", buff_id):
                    self.report.setdefault("tagged_buffs_skipped", []).append(
                        {
                            "buff_id": buff_id,
                            "tag_id": row.get("tag_id"),
                            "reason": "tag missing from 1.2 and not borrowable",
                        }
                    )
                    continue
                self.plan_row("tagged_buffs", row)
        if self.ref.has("buff_tick_effects"):
            for row in self.ref.rows(
                "select * from buff_tick_effects where buff_id = ? order by id", (buff_id,)
            ):
                self.plan_row("buff_tick_effects", row)
        if self.ref.has("dynamic_unit_modifiers"):
            for row in self.ref.rows(
                "select * from dynamic_unit_modifiers where buff_id = ? order by id",
                (buff_id,),
            ):
                self.plan_row("dynamic_unit_modifiers", row)
        if source == "buff_effect":
            self._buff_effect_buff_ids.add(buff_id)

    def max_unit_attribute(self):
        """Highest unit attribute any 1.2 unit_modifiers row uses (the 1.2 client's range)."""
        if self._max_unit_attribute is None:
            row = self.base.row("select max(unit_attribute_id) as m from unit_modifiers")
            # an empty base table gives no range to enforce
            self._max_unit_attribute = (row["m"] if row else 0) or 10**9
        return self._max_unit_attribute

    def unknown_unit_attribute(self, row):
        return (row.get("unit_attribute_id") or 0) > self.max_unit_attribute()

    def buff_has_only_unknown_modifiers(self, buff_id):
        """True when the buff's only effect is unit modifiers 1.2 has no attribute for."""
        if not self.ref.has("unit_modifiers"):
            return False
        mods = self.ref.rows(
            "select unit_attribute_id from unit_modifiers"
            " where owner_type = 'Buff' and owner_id = ?",
            (buff_id,),
        )
        if not mods or not all(self.unknown_unit_attribute(m) for m in mods):
            return False
        for table, column in (
            ("skill_modifiers", "owner_id"),
            ("buff_tick_effects", "buff_id"),
            ("dynamic_unit_modifiers", "buff_id"),
        ):
            if self.ref.has(table) and self.ref.row(
                f'select 1 from "{table}" where "{column}" = ?'
                + (" and owner_type = 'Buff'" if table == "skill_modifiers" else ""),
                (buff_id,),
            ):
                return False
        return True

    def _check_tag(self, tag_id, table, owner_id):
        if not tag_id or not self.base.has("tags"):
            return True
        if self.borrow_tag(tag_id):
            return True
        self.report["refused"]["missing_references"].append(
            {"table": "tags", "id": tag_id, "needed_by": f"{table}:{owner_id}"}
        )
        return False

    def _check_skill_modifier_refs(self, row):
        tag_ok = self._check_tag(
            row.get("tag_id"), "skill_modifiers", row.get("owner_id")
        )
        skill_id = row.get("skill_id") or 0
        if skill_id and skill_id not in self.base.ids("skills"):
            self.report["refused"]["missing_references"].append(
                {
                    "table": "skills",
                    "id": skill_id,
                    "needed_by": f"skill_modifiers:buff {row.get('owner_id')}",
                }
            )
            return False
        return tag_ok

    def borrow_equip_set(self, set_id):
        """equip_item_sets row, its bonuses, bonus buffs and bonus procs."""
        if set_id in self.seen_sets:
            return
        self.seen_sets.add(set_id)
        if not (self.ref.has("equip_item_sets") and self.base.has("equip_item_sets")):
            return
        if set_id in self.base.ids("equip_item_sets"):
            self.report["already_present"].append({"table": "equip_item_sets", "id": set_id})
            return
        ref_row = self.ref.row("select * from equip_item_sets where id = ?", (set_id,))
        if ref_row is None:
            self.report["refused"]["missing_references"].append(
                {"table": "equip_item_sets", "id": set_id}
            )
            return
        self._plan_row("equip_item_sets", ref_row, set_id)
        for bonus in self.ref.rows(
            "select * from equip_item_set_bonuses where equip_item_set_id = ?"
            " order by num_pieces, id",
            (set_id,),
        ):
            buff_id = bonus.get("buff_id") or 0
            if buff_id and self.buff_has_only_unknown_modifiers(buff_id):
                # e.g. the 3.0 PvE damage reduction attribute: 1.2 has no such stat, so the
                # bonus would list an effect nothing implements.
                self.report.setdefault("set_bonuses_omitted", []).append(
                    {
                        "set_id": set_id,
                        "num_pieces": bonus.get("num_pieces"),
                        "buff_id": buff_id,
                        "reason": "only unit attributes 1.2 does not have",
                    }
                )
                continue
            self.plan_row("equip_item_set_bonuses", bonus)
            if buff_id:
                self.borrow_buff(buff_id, source="set_bonus")
            proc_id = bonus.get("proc_id") or 0
            if proc_id:
                self.borrow_item_proc(proc_id)

    def borrow_item_proc(self, proc_id):
        if proc_id in self.seen_ids.get("item_procs", set()):
            return
        if proc_id in self.base.ids("item_procs"):
            self.report["already_present"].append({"table": "item_procs", "id": proc_id})
            return
        ref_row = self.ref.row("select * from item_procs where id = ?", (proc_id,))
        if ref_row is None:
            self.report["refused"]["missing_references"].append(
                {"table": "item_procs", "id": proc_id}
            )
            return
        self._plan_row("item_procs", ref_row, proc_id)
        kinds = self.report.setdefault("proc_chance_kinds", {})
        kind = ref_row.get("chance_kind_id")
        base_kinds = {
            r["chance_kind_id"]
            for r in self.base.rows("select distinct chance_kind_id from item_procs")
        }
        kinds[str(proc_id)] = {"chance_kind_id": kind, "kind_used_in_1_2": kind in base_kinds}
        skill_id = ref_row.get("skill_id") or 0
        if skill_id:
            self.borrow_craft_skill(skill_id)

    def borrow_craft(self, craft_id):
        existing = self.base.row("select id from crafts where id = ?", (craft_id,))
        if existing is not None:
            self.report["refused"]["collisions"].append({"table": "crafts", "id": craft_id})
            return
        if craft_id >= CUSTOM_ID_MIN:
            self.report["refused"]["custom_crafts"].append({"table": "crafts", "id": craft_id})
            return
        ref_row = self.ref.row("select * from crafts where id = ?", (craft_id,))
        if ref_row is None:
            # 3.0 keeps the products/materials of some crafts whose crafts row is gone
            # (the base recipe of the Crazed sets): nothing a loader could start.
            self.report.setdefault("orphan_crafts_in_3_0", []).append(craft_id)
            return
        self._plan_row("crafts", ref_row, craft_id)
        for table in ("craft_products", "craft_materials", "craft_pack_crafts"):
            if not self.ref.has(table):
                continue
            for row in self.ref.rows(
                f'select * from "{table}" where craft_id = ? order by id', (craft_id,)
            ):
                if table == "craft_materials" and row.get("item_id") == MONEY_ITEM_ID:
                    # 3.0 lists the coin cost as a material; 1.2 has no such rows (the
                    # server would need item 500 in the bag, which cannot exist).
                    skipped = self.report.setdefault("money_materials_skipped", [])
                    skipped.append(
                        {"craft_id": craft_id, "amount": row.get("amount")}
                    )
                    continue
                if table == "craft_pack_crafts" and self.skip_craft_pack_crafts:
                    # 1.2 reads this table as "the craft is a pack"; 3.0 uses it for
                    # craft-pack membership. Importing it would change the craft kind.
                    self.report.setdefault("craft_pack_crafts_skipped", []).append(
                        {
                            "craft_id": craft_id,
                            "craft_pack_id": row.get("craft_pack_id"),
                        }
                    )
                    continue
                self.plan_row(table, row)
        if self.ref.has("craft_pack_crafts") and not self.skip_craft_pack_crafts:
            count = len(
                self.ref.rows(
                    "select id from craft_pack_crafts where craft_id = ?", (craft_id,)
                )
            )
            if count:
                self.report["craft_pack_crafts_semantics"] = (
                    "borrowed craft_pack_crafts rows: the 1.2 loader sets is_pack=true "
                    "for every listed craft; 3.0 uses the table for craft-pack membership"
                )
        skill_id = ref_row.get("skill_id") or 0
        if skill_id > 0:
            self.borrow_craft_skill(skill_id)

    def borrow_craft_skill(self, skill_id):
        if skill_id in self.seen_ids.get("skills", set()):
            return
        if skill_id in self.base.ids("skills"):
            base_row = self.base.row("select name from skills where id = ?", (skill_id,))
            ref_row = self.ref.row("select name from skills where id = ?", (skill_id,))
            if (
                base_row is not None
                and ref_row is not None
                and not names_compatible(base_row["name"], ref_row["name"])
            ):
                self.report["refused"]["name_conflicts"].append(
                    {
                        "table": "skills",
                        "id": skill_id,
                        "base_name": base_row["name"],
                        "ref_name": ref_row["name"],
                    }
                )
            return
        if skill_id >= CUSTOM_ID_MIN:
            self.report["refused"]["custom_items"].append(
                {"table": "skills", "id": skill_id}
            )
            return
        ref_row = self.ref.row("select * from skills where id = ?", (skill_id,))
        if ref_row is None:
            self.report["refused"]["missing_references"].append(
                {"table": "skills", "id": skill_id}
            )
            return
        self._plan_row("skills", ref_row, skill_id)
        for skill_effect in self.ref.rows(
            "select * from skill_effects where skill_id = ? order by id", (skill_id,)
        ):
            self.plan_row("skill_effects", skill_effect)
            effect_id = skill_effect.get("effect_id") or 0
            effect = self.ref.row("select * from effects where id = ?", (effect_id,))
            if effect is None:
                self.report["refused"]["missing_references"].append(
                    {"table": "effects", "id": effect_id}
                )
                continue
            if self.plan_row("effects", effect) is None:
                self.report["refused"]["missing_references"].append(
                    {"table": "effects", "id": effect_id, "reason": "id collision"}
                )
                continue
            actual_type = effect.get("actual_type") or ""
            actual_id = effect.get("actual_id") or 0
            detail_table = self._detail_table(actual_type, actual_id)
            if detail_table is None:
                self.report["refused"]["missing_references"].append(
                    {"table": actual_type, "id": actual_id}
                )
                continue
            detail = self.ref.row(
                f'select * from "{detail_table}" where id = ?', (actual_id,)
            )
            if detail is None:
                self.report["refused"]["missing_references"].append(
                    {"table": detail_table, "id": actual_id}
                )
                continue
            self.plan_row(detail_table, detail)
            if detail_table == "buff_effects":
                buff_id = detail.get("buff_id") or 0
                if buff_id:
                    self.borrow_buff(buff_id, source="buff_effect")

    def _detail_table(self, actual_type, actual_id):
        snake = camel_to_snake(actual_type)
        candidates = [snake + "s", snake + "es", snake]
        if snake.endswith("y"):
            candidates.insert(2, snake[:-1] + "ies")
        for candidate in candidates:
            if not self.ref.has(candidate):
                continue
            if self.ref.row(f'select 1 from "{candidate}" where id = ?', (actual_id,)):
                return candidate
        for candidate in candidates:
            if self.ref.has(candidate):
                return candidate
        return None

    def plan_localized_text(self, tbl_name, column, idx, ko, en_us):
        """Insert a localized text, or update the existing `(tbl, column, idx)` row.

        `LocalizationManager` keys `localized_texts` by `(tbl_name, tbl_column_name,
        idx)` and keeps the first row, so inserting a second row for a key the 1.2
        database already has only logs a duplicate and leaves the placeholder in
        place. Existing rows get the new `ko`/`en_us` values; their other locale
        columns are kept.
        """
        key = (tbl_name, column, idx)
        if key in self.seen_localized_keys:
            return
        self.seen_localized_keys.add(key)
        base_row = None
        if self.base.has("localized_texts"):
            base_row = self.base.row(
                "select * from localized_texts where tbl_name = ?"
                " and tbl_column_name = ? and idx = ? order by id limit 1",
                (tbl_name, column, idx),
            )
        if base_row is not None:
            values = dict(base_row)
            if (
                (values.get("ko") or "") == (ko or "")
                and (values.get("en_us") or "") == (en_us or "")
                and (values.get("ko_ver") or 0) == 0
                and (values.get("en_us_ver") or 0) == 0
            ):
                self.report["already_present"].append(
                    {"table": "localized_texts", "id": base_row["id"]}
                )
                return
            values["ko"] = ko or ""
            values["ko_ver"] = 0
            values["en_us"] = en_us or ""
            values["en_us_ver"] = 0
            self._plan_replacement(
                "localized_texts", {}, base_row["id"], values=values
            )
            return
        row = {
            "id": self.next_synth_id("localized_texts"),
            "tbl_name": tbl_name,
            "tbl_column_name": column,
            "idx": idx,
            "ko": ko or "",
            "ko_ver": 0,
            "en_us": en_us or "",
            "en_us_ver": 0,
        }
        for locale in ("zh_cn", "ja", "ru", "zh_tw", "de", "fr"):
            row[locale] = ""
            row[locale + "_ver"] = 0
        self.rows.setdefault("localized_texts", []).append(row)
        self.seen_ids.setdefault("localized_texts", set()).add(row["id"])
        self.report["borrowed_rows"]["localized_texts"] += 1

    def borrow_localized_items(self, item_ids, item_names):
        if not self.ref.has("localized_texts"):
            return 0
        before = len(self.rows.get("localized_texts", []))
        for item_id in sorted(item_ids):
            rows = self.ref.rows(
                "select * from localized_texts where tbl_name = 'items'"
                " and idx = ? and tbl_column_name in ('name', 'description',"
                " 'buff_modifier_tooltip', 'skill_modifier_tooltip')"
                " order by tbl_column_name",
                (item_id,),
            )
            for ref_row in rows:
                column = ref_row.get("tbl_column_name")
                self.plan_localized_text(
                    "items",
                    column,
                    item_id,
                    item_names.get(item_id, "") if column == "name" else "",
                    ref_row.get("en_us") or "",
                )
        return len(self.rows.get("localized_texts", [])) - before

    EXTRA_LOCALIZED = (
        ("buffs", ("name", "desc")),
        ("crafts", ("title", "desc")),
        ("skills", ("name", "desc", "web_desc")),
        ("equip_item_sets", ("name",)),
        ("item_procs", ("description",)),
    )

    def borrow_localized_extra(self):
        """English texts for the buffs, crafts and skills the backport adds.

        Without them the client falls back to the Korean column of the row itself
        (equip effect line, recipe titles in the folio).
        """
        if not self.ref.has("localized_texts"):
            return 0
        before = len(self.rows.get("localized_texts", []))
        missing = self.report.setdefault("localized_missing", [])
        for table, columns in self.EXTRA_LOCALIZED:
            for source in sorted(self.rows.get(table, []), key=lambda r: r["id"]):
                row_id = source["id"]
                for column in columns:
                    if row_id >= CUSTOM_ID_MIN:
                        missing.append([table, column, row_id, "synthesized id"])
                        continue
                    ref_row = self.ref.row(
                        "select * from localized_texts where tbl_name = ?"
                        " and tbl_column_name = ? and idx = ?",
                        (table, column, row_id),
                    )
                    if not ref_row or not (ref_row.get("en_us") or "").strip():
                        missing.append([table, column, row_id, "no en_us in 3.0"])
                        continue
                    if "DO NOT TRANSLATE" in ref_row["en_us"]:
                        missing.append([table, column, row_id, "3.0 placeholder text"])
                        continue
                    self.plan_localized_text(
                        table,
                        column,
                        row_id,
                        source.get(column) or "",
                        ref_row["en_us"],
                    )
        return len(self.rows.get("localized_texts", [])) - before


def build_closure(base, ref, start_ids):
    items = set(start_ids)
    frontier = set(start_ids)
    crafts = set()
    refused_crafts = []
    refused_items = []
    producers = defaultdict(list)
    while frontier:
        ids = sorted(frontier)
        frontier = set()
        new_crafts = set()
        rows = ref.rows(
            "select distinct craft_id, item_id from craft_products where item_id in"
            f" ({','.join('?' for _ in ids)})",
            ids,
        )
        for row in rows:
            craft_id = row["craft_id"]
            if craft_id >= CUSTOM_ID_MIN:
                refused_crafts.append({"craft_id": craft_id, "item_id": row["item_id"]})
                continue
            crafts.add(craft_id)
            new_crafts.add(craft_id)
            producers[row["item_id"]].append(craft_id)
        if not new_crafts:
            continue
        craft_ids = sorted(new_crafts)
        materials = ref.rows(
            "select distinct item_id from craft_materials where craft_id in"
            f" ({','.join('?' for _ in craft_ids)})",
            craft_ids,
        )
        for material in materials:
            item_id = material["item_id"]
            if item_id >= CUSTOM_ID_MIN:
                refused_items.append({"item_id": item_id})
                continue
            if item_id in base.ids("items") or item_id in items:
                continue
            items.add(item_id)
            frontier.add(item_id)
    return {
        "items": sorted(items),
        "crafts": sorted(crafts),
        "producers": {k: sorted(set(v)) for k, v in producers.items()},
        "refused_crafts": refused_crafts,
        "refused_items": refused_items,
    }


def build_backport(base, ref, item_ids, skip_craft_pack_crafts=False):
    closure = build_closure(base, ref, item_ids)
    borrower = Borrower(
        base, ref, skip_craft_pack_crafts=skip_craft_pack_crafts
    )
    for item_id in closure["items"]:
        borrower.borrow_item(item_id, start=item_id in set(item_ids))
    for craft_id in closure["crafts"]:
        borrower.borrow_craft(craft_id)
    item_names = {
        row["id"]: row["name"]
        for row in borrower.rows.get("items", [])
    }
    borrower.borrow_localized_items(
        [row["id"] for row in borrower.rows.get("items", [])], item_names
    )
    borrower.borrow_localized_extra()
    material_ids = [
        item_id
        for item_id in closure["items"]
        if item_id not in set(item_ids) and item_id not in base.ids("items")
    ]
    for item_id in material_ids:
        producers = [
            c for c in closure["producers"].get(item_id, []) if c < CUSTOM_ID_MIN
        ]
        ref_item = ref.row("select name from items where id = ?", (item_id,))
        borrower.report["materials_without_1_2_source"].append(
            {
                "item_id": item_id,
                "name": ref_item["name"] if ref_item else None,
                "retail_crafts_in_3_0": producers,
                "custom_only": not producers
                and any(
                    refused["item_id"] == item_id
                    for refused in closure["refused_crafts"]
                ),
            }
        )
    for craft in closure["refused_crafts"]:
        borrower.report["refused"]["custom_crafts"].append(
            {"table": "crafts", "id": craft["craft_id"], "produces_item_id": craft["item_id"]}
        )
    for item in closure["refused_items"]:
        borrower.report["refused"]["custom_items"].append(
            {"table": "items", "id": item["item_id"]}
        )
    for buff_id in sorted(borrower._buff_effect_buff_ids):
        if buff_id not in base.ids("buffs"):
            borrower.report["buff_effects_referenced_buffs_missing_in_1_2"].append(buff_id)
    model_paths = set()
    for ref_id in borrower.item_asset_map:
        row = ref.row("select path from item_assets where id = ?", (ref_id,))
        if row and row["path"]:
            model_paths.add(row["path"])
    icon_files = set()
    for ref_id in borrower.icon_map:
        row = ref.row("select filename from icons where id = ?", (ref_id,))
        if row and row["filename"]:
            icon_files.add(row["filename"])
    borrower.report["assets_needed"] = {
        "model_paths": sorted(model_paths),
        "icon_files": sorted(icon_files),
    }
    obsidian_assets = set()
    for item_id in item_ids:
        for row in ref.rows(
            "select asset_id from item_armors where item_id = ?", (item_id,)
        ):
            if row["asset_id"]:
                obsidian_assets.add(row["asset_id"])
    obsidian_models = set()
    for armor_asset_id in sorted(obsidian_assets):
        obsidian_models.update(borrower._chain_paths(ref, armor_asset_id))
    obsidian_icons = set()
    icon_columns = [
        column
        for column in ("icon_id", "over_icon_id", "male_icon_id")
        if column in ref.columns("items")
    ]
    for item_id in item_ids:
        if not icon_columns:
            break
        row = ref.row(
            f'select {", ".join(icon_columns)} from items where id = ?',
            (item_id,),
        )
        if row is None:
            continue
        for column in icon_columns:
            value = row.get(column) or 0
            if value:
                icon = ref.row("select filename from icons where id = ?", (value,))
                if icon and icon["filename"]:
                    obsidian_icons.add(icon["filename"])
    borrower.report["assets_needed"]["obsidian_model_paths"] = sorted(obsidian_models)
    borrower.report["assets_needed"]["obsidian_icon_files"] = sorted(obsidian_icons)
    report = dict(borrower.report)
    report["borrowed_rows"] = dict(report["borrowed_rows"])
    report["synthesized_ids"] = dict(report["synthesized_ids"])
    report["boolean_value_conversions"] = {
        t: dict(cols) for t, cols in report["boolean_value_conversions"].items()
    }
    report["columns_filled_with_1_2_defaults"] = {
        t: dict(cols) for t, cols in report["columns_filled_with_1_2_defaults"].items()
    }
    report["columns_dropped_from_3_0"] = {
        t: {col: dict(entry) for col, entry in cols.items()}
        for t, cols in report["columns_dropped_from_3_0"].items()
    }
    report["replaced_rows"] = dict(report["replaced_rows"])
    report["_replacements"] = {
        table: list(rows) for table, rows in borrower.replacements.items()
    }
    return closure, borrower.rows, report


def render_patch(
    rows,
    base_path,
    ref_path,
    command,
    base_sha,
    ref_sha,
    extra_header=None,
    title="version_gaps backport: borrowed gear",
    replacements=None,
):
    lines = [
        f"-- {title}",
        f"-- base: {base_path} (sha256 {base_sha})",
        f"-- source: {ref_path} (sha256 {ref_sha})",
        f"-- command: {command}",
        "-- documented guess: borrowed from 3.0.4.2",
    ]
    for line in extra_header or ():
        lines.append(f"-- {line}" if line else "--")
    lines.append(
        "-- Idempotent: every INSERT is guarded by NOT EXISTS on its id, every UPDATE is"
        " guarded by the row's natural key (item_id or the localized_texts key) and"
        " writes the same values again, so a rerun is a no-op."
    )
    lines.append("BEGIN;")
    replacements = replacements or {}
    extra_tables = [
        t for t in list(rows) + list(replacements) if t not in TABLE_ORDER
    ]
    for table in list(TABLE_ORDER) + extra_tables:
        table_rows = rows.get(table)
        if not table_rows:
            continue
        columns = list(table_rows[0].keys())
        column_sql = ", ".join(f'"{c}"' for c in columns)
        for row in sorted(table_rows, key=lambda r: (r.get("id", 0),)):
            values = ", ".join(sql_literal(row.get(c)) for c in columns)
            lines.append(
                f'INSERT INTO "{table}" ({column_sql}) SELECT {values}'
                f' WHERE NOT EXISTS (SELECT 1 FROM "{table}" WHERE "id" ='
                f" {sql_literal(row.get('id'))});"
            )
    for table in list(TABLE_ORDER) + [
        t for t in replacements if t not in TABLE_ORDER
    ]:
        table_rows = replacements.get(table)
        if not table_rows:
            continue
        for row in sorted(table_rows, key=lambda r: (r.get("id", 0),)):
            columns = [c for c in row if c != "id"]
            assignments = ", ".join(
                f'"{c}" = {sql_literal(row.get(c))}' for c in columns
            )
            guard = ""
            if table == "localized_texts":
                guard = (
                    f' AND "tbl_name" = {sql_literal(row.get("tbl_name"))}'
                    f' AND "tbl_column_name" = {sql_literal(row.get("tbl_column_name"))}'
                    f' AND "idx" = {sql_literal(row.get("idx"))}'
                )
            elif "item_id" in row:
                guard = f' AND "item_id" = {sql_literal(row["item_id"])}'
            lines.append(
                f'UPDATE "{table}" SET {assignments}'
                f' WHERE "id" = {sql_literal(row["id"])}{guard};'
            )
    lines.append("COMMIT;")
    lines.append("")
    return "\n".join(lines)


def apply_patch(base_path, out_db, sql):
    shutil.copy2(base_path, out_db)
    conn = sqlite3.connect(out_db)
    try:
        conn.executescript(sql)
        conn.commit()
        integrity = conn.execute("PRAGMA integrity_check").fetchone()[0]
    finally:
        conn.close()
    return integrity


def weapons_comparison(base, ref):
    obsidian = "\ud751\uc694\uc11d"
    base_obsidian = sorted(
        row["id"]
        for row in base.rows(
            "select i.id from items i join item_weapons w on w.item_id = i.id"
            " where i.name like ?",
            ("%" + obsidian + "%",),
        )
    )
    ref_obsidian_items = [
        row["id"]
        for row in ref.rows("select id from items where name like ?", ("%" + obsidian + "%",))
    ]

    def closure_products(conn, start_ids):
        seen = set(start_ids)
        frontier = set(start_ids)
        products = set()
        while frontier:
            ids = sorted(frontier)
            frontier = set()
            rows = conn.rows(
                "select distinct cp.item_id from craft_materials cm"
                " join craft_products cp on cp.craft_id = cm.craft_id"
                f" where cm.item_id in ({','.join('?' for _ in ids)})",
                ids,
            )
            for row in rows:
                item_id = row["item_id"]
                products.add(item_id)
                if item_id not in seen:
                    seen.add(item_id)
                    frontier.add(item_id)
        names = {}
        if products:
            ids = sorted(products)
            for row in conn.rows(
                f"select id, name from items where id in ({','.join('?' for _ in ids)})",
                ids,
            ):
                names[row["id"]] = row["name"]
        non_obsidian = sorted(
            item_id
            for item_id in products
            if names.get(item_id) and obsidian not in names[item_id]
        )
        return products, non_obsidian, names

    base_products, base_non_obsidian, base_names = closure_products(base, base_obsidian)
    ref_products, ref_non_obsidian, ref_names = closure_products(ref, ref_obsidian_items)
    ref_weapons = {
        row["item_id"] for row in ref.rows("select item_id from item_weapons")
    }
    ref_armors = {
        row["item_id"] for row in ref.rows("select item_id from item_armors")
    }
    recipes = {}
    for weapon_id in OBSIDIAN_WEAPON_RECIPE_IDS:
        crafts = sorted(
            row["craft_id"]
            for row in ref.rows(
                "select craft_id from craft_products where item_id = ?", (weapon_id,)
            )
        )
        recipes[str(weapon_id)] = {
            "crafts": crafts,
            "all_custom": bool(crafts) and all(c >= CUSTOM_ID_MIN for c in crafts),
            "name": ref.row("select name from items where id = ?", (weapon_id,))["name"]
            if ref.row("select name from items where id = ?", (weapon_id,))
            else None,
        }
    return {
        "base_obsidian_weapons": {"count": len(base_obsidian), "ids": base_obsidian},
        "ref_obsidian_named_items": len(ref_obsidian_items),
        "non_obsidian_named_upgrade_products": {
            "base": {"count": len(base_non_obsidian), "ids": base_non_obsidian},
            "ref": {
                "count": len(ref_non_obsidian),
                "weapons": len([i for i in ref_non_obsidian if i in ref_weapons]),
                "armor": len([i for i in ref_non_obsidian if i in ref_armors]),
                "ids": ref_non_obsidian,
            },
        },
        "provided_note": {
            "ref_non_obsidian_named_upgrades": 177,
            "base_count": 51,
            "note": (
                "provided note said 3.0 has 177 non-obsidian-named upgrades and 1.2 has 51;"
                f" computed 1.2 {len(base_non_obsidian)} and 3.0 {len(ref_non_obsidian)}"
                " by craft closure from all obsidian-named items"
            ),
        },
        "weapons_without_recipe_in_1_2": {
            "ids": list(OBSIDIAN_WEAPON_RECIPE_IDS),
            "ref_crafts": recipes,
            "note": "reported only, not borrowed in this task",
        },
    }


def canon_pak_path(path):
    path = path.replace("\\", "/").lower()
    if path.startswith("game/"):
        return path
    return "game/" + path


def parse_asset_refs(file_path):
    if not file_path.lower().endswith((".cdf", ".mtl")):
        return set()
    with open(file_path, "rb") as handle:
        text = handle.read().decode("utf-8", errors="replace")
    refs = set()
    for match in re.finditer(r'(?:File|Material)="([^"]+)"', text):
        ref = match.group(1)
        if ref.lower().endswith(".tif"):
            ref = ref[:-4] + ".dds"
        refs.add(canon_pak_path(ref))
    return refs


def walk_asset_closure(paths, extract_dir, pak10):
    closure = {canon_pak_path(p) for p in paths}
    missing = set()
    frontier = set(closure)
    while frontier:
        new = set()
        for path in sorted(frontier):
            if extract_dir is None:
                continue
            local = os.path.join(extract_dir, path.replace("/", os.sep))
            if not os.path.exists(local):
                continue
            for ref in parse_asset_refs(local):
                if ref in closure:
                    continue
                closure.add(ref)
                new.add(ref)
                if pak10 is not None and ref not in pak10:
                    missing.add(ref)
        frontier = new
    return closure, missing


def read_listing(path):
    if path is None:
        return None
    lines = set()
    with open(path, encoding="utf-8", errors="replace") as handle:
        for line in handle:
            # Pak listings are either bare paths or `path;size` rows.
            line = line.strip().lower().split(";", 1)[0].strip()
            if line:
                lines.add(line)
    return lines


def read_path_list(path):
    """A list of pak paths (overlay input or unresolved roots), with # comments."""
    if path is None:
        return set()
    paths = set()
    with open(path, encoding="utf-8", errors="replace") as handle:
        for line in handle:
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            paths.add(_pak_asset_name(line))
    return paths


def build_assets_section(assets_needed, extract_dir, pak10, pak12):
    model_paths = set(assets_needed.get("model_paths") or [])
    icon_files = set(assets_needed.get("icon_files") or [])
    direct = [canon_pak_path(p) for p in sorted(model_paths)]
    direct += [canon_pak_path("ui/icon/" + f) for f in sorted(icon_files)]
    closure, missing = walk_asset_closure(direct, extract_dir, pak10)
    by_extension = defaultdict(int)
    for path in closure:
        by_extension[path.rsplit(".", 1)[-1]] += 1
    section = {
        "model_paths": sorted(model_paths),
        "icon_files": sorted(icon_files),
        "direct_files": len(direct),
        "closure_files": len(closure),
        "by_extension": dict(sorted(by_extension.items())),
        "missing_in_pak10": sorted(missing),
        "paths": sorted(closure),
    }
    if pak12 is not None:
        shared = sorted(p for p in closure if p in pak12)
        section["shared_with_1_2_pak"] = shared
        section["only_in_10x_pak"] = sorted(p for p in closure if p not in pak12)
    obsidian_models = assets_needed.get("obsidian_model_paths") or []
    if obsidian_models:
        obsidian_icons = assets_needed.get("obsidian_icon_files") or []
        obsidian_direct = [canon_pak_path(p) for p in sorted(obsidian_models)]
        obsidian_direct += [
            canon_pak_path("ui/icon/" + f) for f in sorted(obsidian_icons)
        ]
        obsidian_closure, obsidian_missing = walk_asset_closure(
            obsidian_direct, extract_dir, pak10
        )
        obsidian_ext = defaultdict(int)
        for path in obsidian_closure:
            obsidian_ext[path.rsplit(".", 1)[-1]] += 1
        section["obsidian_only"] = {
            "model_paths": sorted(obsidian_models),
            "icon_files": sorted(obsidian_icons),
            "direct_files": len(obsidian_direct),
            "closure_files": len(obsidian_closure),
            "by_extension": dict(sorted(obsidian_ext.items())),
            "missing_in_pak10": sorted(obsidian_missing),
        }
        if pak12 is not None:
            section["obsidian_only"]["shared_with_1_2_pak"] = sorted(
                p for p in obsidian_closure if p in pak12
            )
            section["obsidian_only"]["only_in_10x_pak"] = sorted(
                p for p in obsidian_closure if p not in pak12
            )
    return section


def write_asset_list(out_path, section):
    Path(out_path).write_text("\n".join(section["paths"]) + "\n", encoding="utf-8")


def overlay_path_list(assets_needed, pak12):
    """New overlay files: playable-race look paths and icons absent from 1.2.

    Dwarf/Warborn look paths stay data-only (no art in this batch). The overlay
    tool's `--closure` adds the materials/textures/models those files reference.
    """
    pak12 = pak12 or set()
    models = set()
    for path in assets_needed.get("model_paths") or []:
        name = _pak_asset_name(path)
        if name in pak12 or _non_playable_race_of_path(name) is not None:
            continue
        models.add(name)
    icons = set()
    for filename in assets_needed.get("icon_files") or []:
        name = _pak_asset_name("ui/icon/" + filename)
        if name not in pak12:
            icons.add(name)
    return {
        "models": sorted(models),
        "icons": sorted(icons),
        "paths": sorted(models | icons),
    }


def write_overlay_list(out_path, overlay, item_count):
    lines = [
        "# 3.0 gear batch 1b-costumes overlay input (four playable races only).",
        f"# {item_count} costumes: {len(overlay['models'])} new look paths"
        f" + {len(overlay['icons'])} new item icons; run with --closure --new-only.",
        "# Build: cargo xtask overlay-pak --source <3.0.4.2 game_pak> --list <this file>",
        "#        --closure --new-only --exclude-present-in <1.2 game_pak>",
        "#        --exclude-path /characters/dwarf/ --exclude-path /characters/warborn/",
        "#        --out <pak>",
    ]
    Path(out_path).write_text(
        "\n".join(lines + overlay["paths"]) + "\n", encoding="utf-8"
    )


def selection_header(selection, borrowed_rows, report):
    counts = selection["counts"]
    lines = [
        "3.0 gear batch 1a (inventory report section 7). Retail 3.0 content only.",
        "Selection: item id absent from 1.2 and below 8,000,000; not an emulator-custom",
        "  Dawnsdrop item (set id >= 9,000,001 or item id 34936..34984); item level <= 62",
        "  (the Eanad power line); costume, armor or cloak; no item proc, no set proc, no",
        "  set bonus unit attribute above 186, no 3.0-only use skill; a look for each of",
        "  the four playable races, all already in the 1.2 pak.",
        f"Selected: {counts['costume']} costumes + {counts['armor']} armor +"
        f" {counts['cloak']} cloaks = {counts['total']} items.",
    ]
    skipped = [
        f"{reason} {entry['count']}"
        for reason, entry in selection["skipped"].items()
        if entry["count"]
    ]
    if skipped:
        lines.append("Skipped candidates: " + ", ".join(skipped) + ".")
    missing = selection["looks"]["missing_from_1_2_pak"]
    if missing:
        lines.append(
            f"{len(missing)} non-playable (Dwarf/Warborn) look paths have no 1.2 pak"
            " file; their item_assets rows are data-only and no playable-race file is"
            " needed."
        )
    if borrowed_rows:
        lines.append(
            "Rows inserted: "
            + ", ".join(
                f"{table} {count}" for table, count in sorted(borrowed_rows.items())
            )
            + "."
        )
    replaced = {
        table: count
        for table, count in (report.get("replaced_rows") or {}).items()
        if count
    }
    if replaced:
        lines.append(
            "Rows replaced (existing-key rows updated in place, not duplicated): "
            + ", ".join(f"{table} {count}" for table, count in sorted(replaced.items()))
            + "."
        )
    lines.append(
        "Keyed-child checks: localized_texts by (tbl_name, tbl_column_name, idx);"
        " item_armors/item_weapons/item_accessories and item_grade_enchanting_supports,"
        " item_enchanting_gems, dyeable_items, item_body_parts, item_summon_mates,"
        " item_summon_slaves, item_backpacks by item_id."
    )
    lines.append(
        "  unit_modifiers (owner_id), craft_products/craft_materials (craft_id),"
        " tagged_buffs, skill_modifiers and equip_item_set_bonuses are multi-row"
        " children; checked, none had an existing row for the imported parents."
    )
    if report.get("orphan_crafts_in_3_0"):
        lines.append(
            "3.0 orphan crafts (products without a crafts row): "
            + ", ".join(str(c) for c in report["orphan_crafts_in_3_0"])
            + "."
        )
    return lines


def selection_header_1b(selection, borrowed_rows, report, overlay):
    counts = selection["counts"]
    looks = selection["looks"]
    lines = [
        "3.0 gear batch 1b-costumes (inventory report section 7). Retail 3.0 content only.",
        "Base: the 1.2 database with 2026-10-21-gear-3.0-batch-1a.sql applied (patches run",
        "  in file-name order), so batch 1a items and their synthesized look ids are present.",
        "Selection: item id absent from 1.2 and batch 1a and below 8,000,000; not an",
        "  emulator-custom Dawnsdrop item (set id >= 9,000,001 or item id 34936..34984);",
        "  item level <= 62 (the Eanad power line); costume only (category 10/173 or armor",
        "  slot 31, category not 94/96/100); no item proc, no set proc, no set bonus unit",
        "  attribute above 186, no 3.0-only use skill; a look for each of the four playable",
        "  races, at least one playable look path absent from the 1.2 pak (the batch 1a",
        "  complement) and every playable look path present in the 3.0.4.2 pak (the overlay",
        "  source). Dwarf/Warborn look paths are imported as data-only item_assets rows.",
        f"Selected: {counts['costume']} costumes = {counts['total']} items.",
    ]
    skipped = [
        f"{reason} {entry['count']}"
        for reason, entry in selection["skipped"].items()
        if entry["count"]
    ]
    if skipped:
        lines.append("Skipped candidates: " + ", ".join(skipped) + ".")
    missing_textures = selection.get("skipped_missing_textures") or []
    if missing_textures:
        ids = ", ".join(str(entry["item_id"]) for entry in missing_textures)
        lines.append(
            f"Skipped {len(missing_textures)} costumes with texture references no source"
            f" pak can satisfy (unresolved after the overlay repair pass): {ids}."
        )
        for entry in missing_textures[:12]:
            lines.append(
                f"  item {entry['item_id']}: " + ", ".join(entry["paths"])
            )
        if len(missing_textures) > 12:
            lines.append(f"  ... {len(missing_textures) - 12} more in the report.")
    lines.append(
        "Texture repair: the overlay pass rewrites a missing texture reference in a new"
        " 3.0 .mtl to the unambiguous same-suffix texture beside the material or model"
        " (e.g. the necro001 materials use their own _df/_sp/_ddn); the skipped ids"
        " above have no such replacement for at least one race."
    )
    lines.append(
        f"Looks: {looks['distinct_paths']} distinct paths = {looks['playable_paths']}"
        f" playable-race + {looks['dwarf_warborn_paths']} Dwarf/Warborn"
        f" + {looks['other_paths']} race-neutral."
    )
    lines.append(
        f"  {len(looks['shared_playable_paths'])} playable paths already exist in the"
        f" 1.2 pak and are reused; {len(looks['new_playable_paths'])} are new in the 3.0"
        f" chains, {len(overlay['models'])} of them are imported primary looks and carried"
        " by the overlay (the rest are 3.0 asset2_id secondary looks, which 1.2 cannot"
        " store)."
    )
    lines.append(
        "  Changed-shared-file rule: the overlay only ADDS files. The five direct look"
        " paths whose 3.0 bytes differ from 1.2 (the four female award001 .chr files and"
        " nu_m_sk_lucius001.chr) keep the 1.2 file: they are the same costume models"
        " (lucius001 is the 1.2 item 16066 art), so the imported costumes render that"
        " outfit and no existing 1.2 art changes. Changed closure materials/textures are"
        " skipped by the overlay tool's --new-only and listed in the manifest."
    )
    lines.append(
        f"Overlay input: {len(overlay['models'])} new playable-race look paths +"
        f" {len(overlay['icons'])} new item icons; cargo xtask overlay-pak --closure"
        " --new-only --exclude-path /characters/dwarf/ --exclude-path /characters/warborn/"
        " adds their materials/textures/models and enforces the add-only, four-race rule."
    )
    if borrowed_rows:
        lines.append(
            "Rows inserted: "
            + ", ".join(
                f"{table} {count}" for table, count in sorted(borrowed_rows.items())
            )
            + "."
        )
    replaced = {
        table: count
        for table, count in (report.get("replaced_rows") or {}).items()
        if count
    }
    if replaced:
        lines.append(
            "Rows replaced (existing-key rows updated in place, not duplicated): "
            + ", ".join(f"{table} {count}" for table, count in sorted(replaced.items()))
            + "."
        )
    lines.append(
        "Keyed-child checks: localized_texts by (tbl_name, tbl_column_name, idx);"
        " item_armors/item_weapons/item_accessories and item_grade_enchanting_supports,"
        " item_enchanting_gems, dyeable_items, item_body_parts, item_summon_mates,"
        " item_summon_slaves, item_backpacks by item_id."
    )
    lines.append(
        "  unit_modifiers (owner_id), craft_products/craft_materials (craft_id),"
        " tagged_buffs, skill_modifiers and equip_item_set_bonuses are multi-row"
        " children; checked, none had an existing row for the imported parents."
    )
    if report.get("orphan_crafts_in_3_0"):
        lines.append(
            "3.0 orphan crafts (products without a crafts row): "
            + ", ".join(str(c) for c in report["orphan_crafts_in_3_0"])
            + "."
        )
    return lines


def run_backport(
    base_path,
    ref_path,
    items_spec,
    out_sql,
    out_db,
    command=None,
    assets_out=None,
    assets_extract_dir=None,
    pak10_listing=None,
    pak12_listing=None,
    pak30_listing=None,
    overlay_list_out=None,
    skip_art_paths=None,
):
    started = time.time()
    base = Db(path=base_path)
    ref = Db(path=ref_path)
    try:
        selection = None
        pak12 = read_listing(pak12_listing)
        overlay = None
        if items_spec in GEAR_FAMILIES:
            if not pak12_listing:
                raise SystemExit(
                    f"--items {items_spec} needs --pak12-listing (the 1.2 pak paths)"
                )
            if items_spec == "gear-1b-costumes":
                if not pak30_listing:
                    raise SystemExit(
                        "--items gear-1b-costumes needs --pak30-listing"
                        " (the 3.0.4.2 pak paths, the overlay source)"
                    )
                selection = select_gear_1b_costumes(
                    base,
                    ref,
                    pak12 or (),
                    read_listing(pak30_listing) or (),
                    read_path_list(skip_art_paths),
                )
            else:
                selection = select_gear_1a(base, ref, pak12 or ())
            item_ids = selection["items"]
            if not item_ids:
                raise SystemExit(f"--items {items_spec} selected no items")
        else:
            item_ids = resolve_item_set(items_spec)
        closure, rows, report = build_backport(
            base, ref, item_ids, skip_craft_pack_crafts=selection is not None
        )
        if selection is not None and selection["family"] == "gear-1b-costumes":
            overlay = overlay_path_list(report.get("assets_needed", {}), pak12)
        base_sha = sha256_file(base_path)
        ref_sha = sha256_file(ref_path)
        if command is None:
            command = (
                "python -I -B -X utf8 backport.py"
                f" --base {base_path} --ref {ref_path} --items {items_spec}"
            )
            if items_spec in GEAR_FAMILIES:
                command += f" --pak12-listing {pak12_listing}"
            if items_spec == "gear-1b-costumes":
                command += f" --pak30-listing {pak30_listing}"
                if overlay_list_out:
                    command += f" --overlay-list-out {overlay_list_out}"
                if skip_art_paths:
                    command += f" --skip-art-paths {skip_art_paths}"
            command += f" --out-sql {out_sql} --out-db {out_db}"
        replacements = report.pop("_replacements", {})
        extra_header = None
        title = "version_gaps backport: borrowed gear"
        if selection is not None:
            if selection["family"] == "gear-1b-costumes":
                extra_header = selection_header_1b(
                    selection, report["borrowed_rows"], report, overlay
                )
                title = "version_gaps backport: 3.0 gear batch 1b-costumes"
            else:
                extra_header = selection_header(selection, report["borrowed_rows"], report)
                title = "version_gaps backport: 3.0 gear batch 1a"
        sql = render_patch(
            rows,
            base_path,
            ref_path,
            command,
            base_sha,
            ref_sha,
            extra_header=extra_header,
            title=title,
            replacements=replacements,
        )
        Path(out_sql).write_text(sql, encoding="utf-8")
        integrity = apply_patch(base_path, out_db, sql)
        report["inputs"] = {
            "base": str(base_path),
            "ref": str(ref_path),
            "items": items_spec,
        }
        if selection is not None:
            report["selection"] = selection
        if overlay is not None:
            report["overlay_list"] = {
                "model_paths": len(overlay["models"]),
                "icons": len(overlay["icons"]),
                "paths": len(overlay["paths"]),
                "list": str(overlay_list_out) if overlay_list_out else None,
            }
            if overlay_list_out:
                write_overlay_list(overlay_list_out, overlay, len(item_ids))
        report["title"] = f"gear backport: {items_spec}"
        report["sha256"] = {"base": base_sha, "ref": ref_sha}
        report["closure"] = {
            "item_count": len(closure["items"]),
            "craft_count": len(closure["crafts"]),
            "items": closure["items"],
            "crafts": closure["crafts"],
        }
        report["weapons_comparison"] = weapons_comparison(base, ref)
        report["outputs"] = {"sql": str(out_sql), "db": str(out_db)}
        report["integrity_check"] = integrity
        if assets_out:
            pak10 = read_listing(pak10_listing)
            pak12 = read_listing(pak12_listing)
            section = build_assets_section(
                report.get("assets_needed", {}),
                assets_extract_dir,
                pak10,
                pak12,
            )
            write_asset_list(assets_out, section)
            section["list"] = str(assets_out)
            report["assets"] = section
        report["elapsed_seconds"] = round(time.time() - started, 3)
        return report
    finally:
        base.close()
        ref.close()


def print_summary(report):
    print("=" * 72)
    print("Gear backport (3.0.4.2 -> 1.2 copy, off by default)")
    print("=" * 72)
    print(f"elapsed: {report['elapsed_seconds']}s")
    print(f"items: {report['closure']['item_count']}, crafts: {report['closure']['craft_count']}")
    selection = report.get("selection")
    if selection:
        counts = selection["counts"]
        if selection["family"] == "gear-1b-costumes":
            print(
                f"selection {selection['family']}: {counts['total']} costumes"
            )
        else:
            print(
                f"selection {selection['family']}: {counts['costume']} costumes +"
                f" {counts['armor']} armor + {counts['cloak']} cloaks ="
                f" {counts['total']} items"
            )
        for reason, entry in selection["skipped"].items():
            if entry["count"]:
                print(f"  skipped {reason}: {entry['count']}")
        for entry in selection.get("skipped_missing_textures") or []:
            print(f"  missing texture item {entry['item_id']}: {entry['paths']}")
        looks = selection["looks"]
        print(
            "  look paths:"
            f" {looks['distinct_paths']} distinct,"
            f" {looks['playable_paths']} playable,"
            f" {looks['dwarf_warborn_paths']} dwarf/warborn,"
            f" {len(looks['missing_from_1_2_pak'])} not in the 1.2 pak"
            + (
                f", {len(looks['missing_from_3_0_pak'])} not in the 3.0 pak"
                if "missing_from_3_0_pak" in looks
                else ""
            )
        )
        overlay = report.get("overlay_list")
        if overlay:
            print(
                f"  overlay input: {overlay['model_paths']} look paths +"
                f" {overlay['icons']} icons = {overlay['paths']} files"
            )
    print("borrowed rows per table:")
    for table, count in sorted(report["borrowed_rows"].items()):
        print(f"  {table}: {count}")
    synthesized = report["synthesized_ids"]
    if synthesized:
        print("synthesized 1.2 ids (3.0 rows without id):")
        for table, info in sorted(synthesized.items()):
            print(f"  {table}: {info['count']} ids {info['first']}..{info['last']}")
    conversions = report.get("boolean_value_conversions") or {}
    if conversions:
        print("boolean value conversions (3.0 0/1 <-> 1.2 't'/'f'):")
        for table, cols in sorted(conversions.items()):
            for col, count in sorted(cols.items()):
                print(f"  {table}.{col}: {count} rows")
    look = report.get("look_borrows") or {}
    if look:
        armor = look["armor_assets"]
        assets = look["item_assets"]
        chain = look["item_armor_assets"]
        icons = look["icon_borrows"]
        print("look chain borrows:")
        print(
            f"  armor_assets: new {len(armor['new_ids'])}"
            f" {armor['new_ids'][:3]}..., reused {len(armor['reused_ids'])}"
        )
        print(
            f"  item_assets: new {len(assets['new_ids'])}"
            f" {assets['new_ids'][:3]}..., reused by path {len(assets['reused_ids'])}"
        )
        print(
            f"  item_armor_assets: new {len(chain['new_ids'])}"
            f" {chain['new_ids'][:3]}..."
        )
        print(
            f"  icons: borrowed {len(icons['borrowed_ids'])}"
            f" {icons['borrowed_ids'][:3]}..., reused {len(icons['reused_ids'])}"
            f" (same filename), remapped {len(icons['remapped'])}"
        )
        print(f"  server reads look tables: {look['server_reads_look_tables']}")
    assets = report.get("assets")
    if assets:
        print(
            f"client assets: {assets['closure_files']} files"
            f" {assets['by_extension']}, shared with 1.2 pak"
            f" {len(assets.get('shared_with_1_2_pak') or [])}, only 10.x"
            f" {len(assets.get('only_in_10x_pak') or [])}, missing in 10.x"
            f" {len(assets['missing_in_pak10'])}"
        )
        obsidian = assets.get("obsidian_only")
        if obsidian:
            print(
                f"  obsidian-only subset: {obsidian['closure_files']} files"
                f" {obsidian['by_extension']}, shared"
                f" {len(obsidian.get('shared_with_1_2_pak') or [])}, only 10.x"
                f" {len(obsidian.get('only_in_10x_pak') or [])}, missing in 10.x"
                f" {len(obsidian['missing_in_pak10'])}"
            )
    dropped = report["columns_dropped_from_3_0"]
    if dropped:
        print("3.0-only columns with non-default values (not representable in 1.2):")
        for table, cols in sorted(dropped.items()):
            for col, entry in sorted(cols.items()):
                example = entry["examples"][0] if entry["examples"] else None
                print(f"  {table}.{col}: {entry['count']} rows, e.g. {example}")
    materials = report["materials_without_1_2_source"]
    print(f"materials borrowed without a 1.2 source: {len(materials)}")
    for entry in materials[:6]:
        print(f"  item {entry['item_id']} {entry['name']!r} crafts {entry['retail_crafts_in_3_0']}")
    print(f"icons missing in 1.2: {len(report['icons_missing_in_1_2'])}")
    print(
        "buffs referenced by borrowed buff_effects but missing in 1.2:"
        f" {report['buff_effects_referenced_buffs_missing_in_1_2']}"
    )
    refused = report["refused"]
    print(
        "refused: custom items {custom_items}, custom crafts {custom_crafts},"
        " collisions {collisions}, name conflicts {name_conflicts},"
        " missing references {missing_references}".format(
            custom_items=len(refused["custom_items"]),
            custom_crafts=len(refused["custom_crafts"]),
            collisions=len(refused["collisions"]),
            name_conflicts=len(refused["name_conflicts"]),
            missing_references=len(refused["missing_references"]),
        )
    )
    weapons = report["weapons_comparison"]
    print(
        "weapons: 1.2 obsidian weapons {base}, 3.0 non-obsidian-named upgrade products"
        " {ref} (weapons {weapons})".format(
            base=weapons["base_obsidian_weapons"]["count"],
            ref=weapons["non_obsidian_named_upgrade_products"]["ref"]["count"],
            weapons=weapons["non_obsidian_named_upgrade_products"]["ref"]["weapons"],
        )
    )
    print(f"integrity_check: {report['integrity_check']}")
    print(f"sql: {report['outputs']['sql']}")
    print(f"db copy: {report['outputs']['db']}")


# ---------------------------------------------------------------------------
# Client-side patch: a COPY of the 1.2 client (loose game/db/compact.sqlite3 +
# game_pak) gets the same rows and the 10.x-only asset files.
# ---------------------------------------------------------------------------

PROTECTED_CLIENT_ROOTS = ("d:/aa1.2",)


def _norm_path(path):
    return os.path.realpath(path).replace("\\", "/").lower().rstrip("/")


def refuse_protected_client(client_root):
    """The read-only originals must never be patched."""
    norm = _norm_path(client_root)
    if "/archeage returns" in norm:
        raise SystemExit(f"refusing to patch the 10.x client at {client_root}")
    # D: is a subst on the reference machine, so compare both spellings.
    for protected in PROTECTED_CLIENT_ROOTS:
        for candidate in (protected, _norm_path(protected)):
            if norm == candidate or norm.startswith(candidate + "/"):
                raise SystemExit(
                    f"refusing to patch the read-only client at {client_root}"
                )
    if not os.path.isfile(os.path.join(client_root, "game_pak")):
        raise SystemExit(f"{client_root} has no game_pak")


def _columns(conn, schema, table):
    return [r[1] for r in conn.execute(f'PRAGMA {schema}.table_info("{table}")')]


def _mostly_null_columns(conn, table, cols, threshold=0.95, min_rows=100):
    """Positions of columns the client's own rows leave NULL (>= threshold).

    The 3.0 data stores 0 where the 1.2 data stores NULL (exp_date, more_asset_id,
    ...). The 1.2 client reads a 0 there as a real value: exp_date 0 makes the
    tooltip say "Item expired.", so those zeros are written as NULL.
    """
    total = conn.execute(f'SELECT COUNT(*) FROM main."{table}"').fetchone()[0]
    result = []
    if total < min_rows:
        return result
    for pos, col in enumerate(cols):
        if col.lower() == "id":
            continue
        nulls = conn.execute(
            f'SELECT COUNT(*) FROM main."{table}" WHERE "{col}" IS NULL'
        ).fetchone()[0]
        if nulls / total >= threshold:
            result.append(pos)
    return result


def patch_client_db(client_db, base_db, patched_db):
    """Insert the backported rows into the loose client db (idempotent).

    A row is "backported" when its id exists in `patched_db` but not in
    `base_db` (the 1.2 server db the backport started from). Only the client
    table's columns are written (case-insensitive intersection); rows whose id
    already exists in the client db are skipped (identical) or reported
    (different). Icons get name = file stem when blank (the client needs it),
    and every icon_id/male_icon_id/over_icon_id of a patched item must resolve.
    """
    conn = sqlite3.connect(client_db)
    conn.execute("ATTACH DATABASE ? AS base", (str(base_db),))
    conn.execute("ATTACH DATABASE ? AS src", (str(patched_db),))
    report = {"inserted": {}, "conflicts": {}, "already_present": {}, "missing_icons": []}
    try:
        for table in TABLE_ORDER:
            ccols = _columns(conn, "main", table)
            scols = _columns(conn, "src", table)
            bcols = {c.lower() for c in _columns(conn, "base", table)}
            lower = {c.lower() for c in scols}
            cols = [c for c in ccols if c.lower() in lower]
            low = [c.lower() for c in cols]
            if not ccols or not scols or "id" not in low or "id" not in bcols:
                continue
            col_list = ", ".join(f'"{c}"' for c in cols)
            sel = ", ".join(f's."{c}"' for c in cols)
            rows = conn.execute(
                f'SELECT {sel} FROM src."{table}" s '
                f'WHERE s.id NOT IN (SELECT id FROM base."{table}") ORDER BY s.id'
            ).fetchall()
            inserted = present = 0
            conflicts = []
            idx = low.index("id")
            null_cols = _mostly_null_columns(conn, table, cols)
            normalized = report.setdefault("zero_to_null", {})
            for row in rows:
                row = list(row)
                for pos in null_cols:
                    if row[pos] in (0, ""):
                        row[pos] = None
                        key = f"{table}.{cols[pos]}"
                        normalized[key] = normalized.get(key, 0) + 1
                if table == "icons" and "name" in low and "filename" in low:
                    ni, fi = low.index("name"), low.index("filename")
                    if not row[ni] and row[fi]:
                        row[ni] = os.path.splitext(os.path.basename(row[fi]))[0]
                existing = conn.execute(
                    f'SELECT {col_list} FROM main."{table}" WHERE id = ?', (row[idx],)
                ).fetchone()
                if existing is None:
                    marks = ", ".join("?" for _ in cols)
                    conn.execute(
                        f'INSERT INTO main."{table}" ({col_list}) VALUES ({marks})', row
                    )
                    inserted += 1
                elif list(existing) == row:
                    present += 1
                else:
                    conflicts.append(row[idx])
            report["inserted"][table] = inserted
            report["already_present"][table] = present
            if conflicts:
                report["conflicts"][table] = conflicts
        item_cols = {c.lower(): c for c in _columns(conn, "main", "items")}
        for col in ("icon_id", "male_icon_id", "over_icon_id"):
            if col not in item_cols:
                continue
            name = item_cols[col]
            for item_id, icon_id in conn.execute(
                f'SELECT i.id, i."{name}" FROM main.items i WHERE i.id IN '
                "(SELECT id FROM src.items WHERE id NOT IN (SELECT id FROM base.items)) "
                f'AND i."{name}" IS NOT NULL AND i."{name}" != 0 '
                f'AND i."{name}" NOT IN (SELECT id FROM main.icons)'
            ).fetchall():
                report["missing_icons"].append([item_id, col, icon_id])
        conn.commit()
    finally:
        conn.close()
    return report


def pak_names_to_write(assets_list, pak12):
    """Asset paths the 1.2 pak does not have (the asset list minus the 1.2 listing)."""
    names = set()
    for line in Path(assets_list).read_text(encoding="utf-8").splitlines():
        name = line.strip().lower()
        if name and name not in pak12:
            names.add(name)
    return sorted(names)


def patch_client_pak(client_root, names, source_root, pakwrite_exe):
    missing = [n for n in names if not os.path.isfile(os.path.join(source_root, n))]
    # Files the 10.x pak itself lacks (dangling mtl references) cannot be written;
    # they are reported, not fatal.
    names = [n for n in names if n not in set(missing)]
    list_path = os.path.join(client_root, "pakwrite-names.tmp")
    Path(list_path).write_text("\n".join(names) + "\n", encoding="utf-8")
    try:
        result = subprocess.run(
            [
                pakwrite_exe,
                "add",
                os.path.join(client_root, "game_pak"),
                source_root,
                list_path,
            ],
            capture_output=True,
            text=True,
            check=False,
        )
    finally:
        os.remove(list_path)
    if result.returncode != 0:
        raise SystemExit(f"pakwrite failed: {result.stderr.strip()}")
    counts = defaultdict(int)
    for line in result.stdout.splitlines():
        counts[line.split(" ", 1)[0]] += 1
    counts = dict(counts)
    if missing:
        counts["MISSING_SOURCE"] = len(missing)
        counts["missing_source_files"] = sorted(missing)
    return counts


def patch_client(
    client_root, base_db, patched_db, assets_list, pak12_listing, source_root, pakwrite_exe
):
    refuse_protected_client(client_root)
    db_report = patch_client_db(
        os.path.join(client_root, "game", "db", "compact.sqlite3"), base_db, patched_db
    )
    names = pak_names_to_write(assets_list, read_listing(pak12_listing))
    pak_report = patch_client_pak(client_root, names, source_root, pakwrite_exe)
    return {"db": db_report, "pak": pak_report, "pak_files_needed": len(names)}


def patch_client_main(argv):
    parser = argparse.ArgumentParser(prog="backport.py patch-client")
    parser.add_argument("--client-root", required=True, help="COPY of the 1.2 client")
    parser.add_argument("--base", required=True, help="1.2 server compact.sqlite3")
    parser.add_argument("--patched", required=True, help="backported compact copy")
    parser.add_argument("--assets-list", required=True)
    parser.add_argument("--pak12-listing", required=True)
    parser.add_argument("--assets-root", required=True, help="extracted 10.x files")
    parser.add_argument("--pakwrite", required=True, help="pakwrite example executable")
    args = parser.parse_args(argv)
    report = patch_client(
        args.client_root,
        args.base,
        args.patched,
        args.assets_list,
        args.pak12_listing,
        args.assets_root,
        args.pakwrite,
    )
    print(json.dumps(report, ensure_ascii=False, indent=2))
    return 0


def main(argv=None):
    if argv is None:
        argv = sys.argv[1:]
    if argv and argv[0] == "patch-client":
        return patch_client_main(argv[1:])
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", required=True, help="1.2 compact.sqlite3 (read-only)")
    parser.add_argument("--ref", required=True, help="3.0.4.2 compact.sqlite3 (read-only)")
    parser.add_argument(
        "--items",
        required=True,
        help="item ids (comma separated) or a named set: obsidian-armor",
    )
    parser.add_argument("--out-sql", required=True, help="SQL patch output path")
    parser.add_argument("--out-db", required=True, help="patched database copy path")
    parser.add_argument("--report", required=True, help="JSON report output path")
    parser.add_argument(
        "--assets-out",
        help="client asset list output path (pak paths, one per line)",
    )
    parser.add_argument(
        "--assets-extract-dir",
        help="directory with 10.x files already extracted (for the dependency walk)",
    )
    parser.add_argument("--pak10-listing", help="10.x pak listing (one path per line)")
    parser.add_argument("--pak12-listing", help="1.2 pak listing (one path per line)")
    parser.add_argument(
        "--pak30-listing",
        help="3.0.4.2 pak listing (one path per line; gear-1b-costumes overlay source)",
    )
    parser.add_argument(
        "--overlay-list-out",
        help="overlay input list (new playable-race look paths + new icons)",
    )
    parser.add_argument(
        "--skip-art-paths",
        help=(
            "unresolved texture roots from overlay-pak --unresolved-list;"
            " costumes using them are skipped"
        ),
    )
    parser.add_argument(
        "--command",
        help="command line recorded in the SQL header (default: this invocation)",
    )
    args = parser.parse_args(argv)

    report = run_backport(
        args.base,
        args.ref,
        args.items,
        args.out_sql,
        args.out_db,
        command=args.command,
        assets_out=args.assets_out,
        assets_extract_dir=args.assets_extract_dir,
        pak10_listing=args.pak10_listing,
        pak12_listing=args.pak12_listing,
        pak30_listing=args.pak30_listing,
        overlay_list_out=args.overlay_list_out,
        skip_art_paths=args.skip_art_paths,
    )
    with open(args.report, "w", encoding="utf-8") as handle:
        json.dump(report, handle, ensure_ascii=False, indent=2, default=str)
    print_summary(report)
    print()
    print(f"report written to {args.report}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
