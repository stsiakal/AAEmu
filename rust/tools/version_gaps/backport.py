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

NAMED_SETS = {"obsidian-armor": list(range(35581, 35626))}

TABLE_ORDER = (
    "items",
    "icons",
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
    "buffs",
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
        if part:
            ids.append(int(part))
    if not ids:
        raise ValueError(f"empty item set: {spec!r}")
    return sorted(set(ids))


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
    def __init__(self, base, ref):
        self.base = base
        self.ref = ref
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
            "boolean_value_conversions": defaultdict(lambda: defaultdict(int)),
            "materials_without_1_2_source": [],
            "icons_missing_in_1_2": [],
            "buff_effects_referenced_buffs_missing_in_1_2": [],
            "craft_pack_crafts_semantics": None,
        }
        self._buff_effect_buff_ids = set()
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
            entry["above_custom_min"] = base_max >= CUSTOM_ID_MIN
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

    def _plan_row(self, table, ref_row, id_value):
        base_cols, mapped, defaults = self.colmap(table)
        row = {}
        for base_col in base_cols:
            ref_col = mapped.get(base_col)
            if ref_col is None:
                row[base_col] = self.default_value(table, base_col)
                self.report["columns_filled_with_1_2_defaults"][table][base_col] += 1
            else:
                row[base_col] = self._normalize_value(
                    table, base_col, ref_row.get(ref_col)
                )
        row["id"] = id_value
        ref_only = [
            c for c in self.ref.columns(table) if c.lower() not in {b.lower() for b in base_cols}
        ]
        for col in ref_only:
            value = ref_row.get(col)
            if not is_default_value(value):
                entry = self.report["columns_dropped_from_3_0"][table][col]
                entry["count"] += 1
                if len(entry["examples"]) < 20:
                    entry["examples"].append({"id": id_value, "value": value})
        self.rows.setdefault(table, []).append(row)
        self.seen_ids.setdefault(table, set()).add(id_value)
        self.report["borrowed_rows"][table] += 1
        return row

    def plan_row(self, table, ref_row):
        rid = ref_row.get("id")
        if rid is not None and rid in self.base.ids(table):
            self.report["refused"]["collisions"].append(
                {"table": table, "id": rid}
            )
            return None
        if rid is None:
            rid = self.next_synth_id(table)
        return self._plan_row(table, ref_row, rid)

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
            new_id = armor_asset_id
            self.report["look_borrows"]["armor_assets"]["reused_ids"].append(
                {"id": armor_asset_id, "reason": "identical item_armor_assets chain"}
            )
        else:
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
        item_row = self._plan_row("items", ref_row, item_id)
        for table in ITEM_CLASS_TABLES:
            if not self.ref.has(table):
                continue
            for row in self.ref.rows(
                f'select * from "{table}" where item_id = ? order by id', (item_id,)
            ):
                planned = self.plan_row(table, row)
                if table == "item_armors":
                    if planned is not None:
                        asset_id = row.get("asset_id") or 0
                        if asset_id:
                            planned["asset_id"] = self.borrow_armor_look(asset_id)
                    self._check_armor_references(row)
        for table, key, condition in ITEM_SCOPED_TABLES:
            if not self.ref.has(table):
                continue
            where = f'{key} = ?' + (f" and {condition}" if condition else "")
            for row in self.ref.rows(
                f'select * from "{table}" where {where} order by rowid', (item_id,)
            ):
                self.plan_row(table, row)
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
                self.plan_row("unit_modifiers", row)
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
            self.report["refused"]["missing_references"].append(
                {"table": "crafts", "id": craft_id}
            )
            return
        self._plan_row("crafts", ref_row, craft_id)
        for table in ("craft_products", "craft_materials", "craft_pack_crafts"):
            if not self.ref.has(table):
                continue
            for row in self.ref.rows(
                f'select * from "{table}" where craft_id = ? order by id', (craft_id,)
            ):
                self.plan_row(table, row)
        if self.ref.has("craft_pack_crafts"):
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

    def borrow_localized_items(self, item_ids, item_names):
        if not self.ref.has("localized_texts"):
            return 0
        borrowed = 0
        for item_id in sorted(item_ids):
            rows = self.ref.rows(
                "select * from localized_texts where tbl_name = 'items'"
                " and idx = ? and tbl_column_name in ('name', 'description')"
                " order by tbl_column_name",
                (item_id,),
            )
            for ref_row in rows:
                column = ref_row.get("tbl_column_name")
                row = {
                    "id": self.next_synth_id("localized_texts"),
                    "tbl_name": "items",
                    "tbl_column_name": column,
                    "idx": item_id,
                    "ko": item_names.get(item_id, "") if column == "name" else "",
                    "ko_ver": 0,
                    "en_us": ref_row.get("en_us") or "",
                    "en_us_ver": 0,
                }
                for locale in ("zh_cn", "ja", "ru", "zh_tw", "de", "fr"):
                    row[locale] = ""
                    row[locale + "_ver"] = 0
                self.rows.setdefault("localized_texts", []).append(row)
                self.seen_ids.setdefault("localized_texts", set()).add(row["id"])
                self.report["borrowed_rows"]["localized_texts"] += 1
                borrowed += 1
        return borrowed


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


def build_backport(base, ref, item_ids):
    closure = build_closure(base, ref, item_ids)
    borrower = Borrower(base, ref)
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
    return closure, borrower.rows, report


def render_patch(rows, base_path, ref_path, command, base_sha, ref_sha):
    lines = [
        "-- version_gaps backport: obsidian armor",
        f"-- base: {base_path} (sha256 {base_sha})",
        f"-- source: {ref_path} (sha256 {ref_sha})",
        f"-- command: {command}",
        "-- documented guess: borrowed from 3.0.4.2",
        "BEGIN;",
    ]
    for table in list(TABLE_ORDER) + [t for t in rows if t not in TABLE_ORDER]:
        table_rows = rows.get(table)
        if not table_rows:
            continue
        columns = list(table_rows[0].keys())
        column_sql = ", ".join(f'"{c}"' for c in columns)
        for row in sorted(table_rows, key=lambda r: (r.get("id", 0),)):
            values = ", ".join(sql_literal(row.get(c)) for c in columns)
            lines.append(f'INSERT INTO "{table}" ({column_sql}) VALUES ({values});')
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
            line = line.strip().lower()
            if line:
                lines.add(line)
    return lines


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
):
    started = time.time()
    base = Db(path=base_path)
    ref = Db(path=ref_path)
    try:
        item_ids = resolve_item_set(items_spec)
        closure, rows, report = build_backport(base, ref, item_ids)
        base_sha = sha256_file(base_path)
        ref_sha = sha256_file(ref_path)
        if command is None:
            command = (
                "python -I -B -X utf8 backport.py"
                f" --base {base_path} --ref {ref_path} --items {items_spec}"
                f" --out-sql {out_sql} --out-db {out_db}"
            )
        sql = render_patch(rows, base_path, ref_path, command, base_sha, ref_sha)
        Path(out_sql).write_text(sql, encoding="utf-8")
        integrity = apply_patch(base_path, out_db, sql)
        report["inputs"] = {
            "base": str(base_path),
            "ref": str(ref_path),
            "items": items_spec,
        }
        report["title"] = "obsidian armor backport"
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
    print("Obsidian armor backport (3.0.4.2 -> 1.2 copy, off by default)")
    print("=" * 72)
    print(f"elapsed: {report['elapsed_seconds']}s")
    print(f"items: {report['closure']['item_count']}, crafts: {report['closure']['craft_count']}")
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
            for row in rows:
                row = list(row)
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
    args = parser.parse_args(argv)

    report = run_backport(
        args.base,
        args.ref,
        args.items,
        args.out_sql,
        args.out_db,
        assets_out=args.assets_out,
        assets_extract_dir=args.assets_extract_dir,
        pak10_listing=args.pak10_listing,
        pak12_listing=args.pak12_listing,
    )
    with open(args.report, "w", encoding="utf-8") as handle:
        json.dump(report, handle, ensure_ascii=False, indent=2, default=str)
    print_summary(report)
    print()
    print(f"report written to {args.report}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
