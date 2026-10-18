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
import shutil
import sqlite3
import sys
import time
from collections import OrderedDict, defaultdict
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from gaps import CUSTOM_ID_MIN, Db, camel_to_snake, names_compatible

NAMED_SETS = {"obsidian-armor": list(range(35581, 35626))}

TABLE_ORDER = (
    "items",
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
            self.synth_next[table] = self.base_max_id(table)
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
        self._plan_row("items", ref_row, item_id)
        for table in ITEM_CLASS_TABLES:
            if not self.ref.has(table):
                continue
            for row in self.ref.rows(
                f'select * from "{table}" where item_id = ? order by id', (item_id,)
            ):
                self.plan_row(table, row)
                if table == "item_armors":
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
        icon_id = ref_row.get("icon_id") or 0
        if icon_id and icon_id not in self.base.ids("icons"):
            self.report["icons_missing_in_1_2"].append(
                {"item_id": item_id, "icon_id": icon_id}
            )

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
        asset_id = row.get("asset_id") or 0
        if asset_id and asset_id not in self.base.ids("item_assets"):
            self.report["refused"]["missing_references"].append(
                {"table": "item_assets", "id": asset_id, "item_id": row.get("item_id")}
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


def run_backport(base_path, ref_path, items_spec, out_sql, out_db, command=None):
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


def main(argv=None):
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
    args = parser.parse_args(argv)

    report = run_backport(
        args.base, args.ref, args.items, args.out_sql, args.out_db
    )
    with open(args.report, "w", encoding="utf-8") as handle:
        json.dump(report, handle, ensure_ascii=False, indent=2, default=str)
    print_summary(report)
    print()
    print(f"report written to {args.report}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
