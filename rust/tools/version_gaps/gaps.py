#!/usr/bin/env python3
"""Report 1.2 content gaps that 3.0.4.2 data could fill.

Compares the server's 1.2 ``compact.sqlite3`` against the 3.0.4.2 reference
``compact.sqlite3`` (client/common data) and ``compact.server.table.sqlite3``
(server tables such as loots and npc spawners). All databases are opened
read-only. The output is a JSON report of gaps with the rows that would be
borrowed (table + id + row contents), for hand-written patches later; this
tool never writes to any database and never writes a patch.

Gap kinds:
  1. quest contexts with no components in 1.2 that have components (and acts
     and act detail rows) in 3.0
  2. item use_skill_id pointing to a skill missing in 1.2 where 3.0 has a
     different or existing skill
  3. quest act / quest detail rows referencing npc/item/doodad ids missing
     from 1.2 that exist in 3.0
  4. loot packs empty in 1.2 that have loot rows in 3.0, and npcs that drop
     nothing in 1.2 but have loot_pack_dropping_npcs rows in 3.0
  5. quest-needed npcs without a spawner in 1.2 that have npc_spawner data in
     the 3.0 server tables, plus raw spawn-table differences

Two exclusion rules keep the default counts trustworthy:

* custom ids: the 3.0.4.2 emulator data contains rows with ids at or above
  ``CUSTOM_ID_MIN`` (8,000,000) that are not retail. Any gap whose borrowed
  rows carry such an id (row id, loot_pack_id, spawner id, npc/item/skill ids
  inside) gets ``custom_origin: true`` and is counted separately.
* same-concept names: a reused id may mean different content. Entries whose
  names differ between 1.2 and 3.0 get ``name_mismatch: true`` and are
  counted separately. Kind 1 compares the quest context names, kind 2 the
  item names, kind 3 the names of the quest contexts that reference the
  missing target (unverifiable references also count as a mismatch).

Usage:
    python -I gaps.py --base <1.2 compact.sqlite3> \
        --ref <3.0.4.2 compact.sqlite3> \
        --ref-server <3.0.4.2 compact.server.table.sqlite3> \
        --output report.json
"""

import argparse
import json
import re
import sqlite3
import sys
import time
from collections import defaultdict
from pathlib import Path
from urllib.parse import quote

CUSTOM_ID_MIN = 8_000_000
CUSTOM_ID_COLUMNS = (
    "id",
    "npc_id",
    "timer_npc_id",
    "item_id",
    "doodad_id",
    "skill_id",
    "buff_id",
    "model_id",
    "loot_pack_id",
    "npc_spawner_id",
    "member_id",
    "quest_context_id",
    "quest_component_id",
    "act_detail_id",
    "effect_id",
    "sphere_id",
)
TARGET_COLUMNS = {
    "npc_id": "npcs",
    "timer_npc_id": "npcs",
    "item_id": "items",
    "doodad_id": "doodad_almighties",
}
ASSET_TABLES = {"npcs", "items", "doodad_almighties", "skills", "buffs"}
REFERENCE_COLUMNS = {
    "npc_id": "npcs",
    "timer_npc_id": "npcs",
    "item_id": "items",
    "doodad_id": "doodad_almighties",
    "skill_id": "skills",
    "buff_id": "buffs",
    "model_id": "actor_models",
}
QUEST_ROW_SOURCES = (
    ("quest_components", "npc_id", "quest_context_id"),
    ("quest_chat_bubbles", "npc_id", "quest_component_id"),
)
LOCATION_TABLES = (
    "quest_components",
    "quest_acts",
    "loots",
    "loot_pack_dropping_npcs",
    "doodad_func_loot_packs",
    "npc_spawners",
    "npc_spawner_npcs",
    "npc_spawner_spawn_effects",
    "npc_spawner_despawn_effects",
)


def open_ro(path):
    uri = "file:" + quote(Path(path).resolve().as_posix(), safe="/:")
    conn = sqlite3.connect(uri + "?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    return conn


def in_clause(values):
    return "(" + ",".join("?" for _ in values) + ")"


def camel_to_snake(name):
    return re.sub(r"(?<=[a-z0-9])(?=[A-Z])", "_", name).lower()


def detail_table_candidates(act_type):
    snake = camel_to_snake(act_type)
    candidates = [snake + "s", snake + "es", snake, snake + "es"]
    if snake.endswith("y"):
        candidates.insert(2, snake[:-1] + "ies")
    seen = []
    for candidate in candidates:
        if candidate not in seen:
            seen.append(candidate)
    return seen


class Db:
    def __init__(self, path=None, conn=None):
        if conn is None:
            conn = open_ro(path)
        self.conn = conn
        self.path = str(path) if path else "<memory>"
        self._tables = None
        self._columns = {}
        self._detail_tables = {}
        self._ids = {}
        self._names = {}

    def close(self):
        self.conn.close()

    def tables(self):
        if self._tables is None:
            rows = self.conn.execute(
                "select name from sqlite_master where type='table'"
            ).fetchall()
            self._tables = {r[0] for r in rows}
        return self._tables

    def has(self, table):
        return table in self.tables()

    def columns(self, table):
        if table not in self._columns:
            if not self.has(table):
                self._columns[table] = []
            else:
                rows = self.conn.execute(f'pragma table_info("{table}")').fetchall()
                self._columns[table] = [r[1] for r in rows]
        return self._columns[table]

    def count(self, table):
        if not self.has(table):
            return None
        return self.conn.execute(f'select count(*) from "{table}"').fetchone()[0]

    def rows(self, sql, args=()):
        return [dict(r) for r in self.conn.execute(sql, args).fetchall()]

    def row(self, sql, args=()):
        found = self.conn.execute(sql, args).fetchone()
        return dict(found) if found else None

    def ids(self, table, column="id"):
        key = (table, column)
        if key not in self._ids:
            if not self.has(table):
                self._ids[key] = set()
            else:
                rows = self.conn.execute(
                    f'select "{column}" from "{table}" where "{column}" is not null'
                ).fetchall()
                self._ids[key] = {r[0] for r in rows}
        return self._ids[key]

    def names(self, table):
        if table not in self._names:
            if not self.has(table) or "name" not in self.columns(table):
                self._names[table] = {}
            else:
                self._names[table] = {
                    r["id"]: r["name"]
                    for r in self.rows(f'select id, name from "{table}"')
                }
        return self._names[table]

    def resolve_detail_table(self, act_type, detail_id=None):
        if act_type not in self._detail_tables:
            candidates = [
                c for c in detail_table_candidates(act_type) if self.has(c)
            ]
            if not candidates:
                resolved = None
            elif len(candidates) == 1:
                resolved = candidates[0]
            elif detail_id is not None:
                resolved = candidates[0]
                for candidate in candidates:
                    found = self.conn.execute(
                        f'select 1 from "{candidate}" where id = ?', (detail_id,)
                    ).fetchone()
                    if found:
                        resolved = candidate
                        break
            else:
                resolved = candidates[0]
            self._detail_tables[act_type] = resolved
        return self._detail_tables[act_type]

    def collect_detail_acts(self):
        if not self.has("quest_acts"):
            return {}
        components = {
            r["id"]: r
            for r in self.rows("select id, quest_context_id from quest_components")
        }
        result = defaultdict(lambda: defaultdict(list))
        for act in self.rows(
            "select id, quest_component_id, act_detail_id, act_detail_type"
            " from quest_acts"
        ):
            table = self.resolve_detail_table(act["act_detail_type"], act["act_detail_id"])
            if not table or act["act_detail_id"] <= 0:
                continue
            component = components.get(act["quest_component_id"])
            result[table][act["act_detail_id"]].append(
                {
                    "act_id": act["id"],
                    "act_detail_type": act["act_detail_type"],
                    "component_id": act["quest_component_id"],
                    "quest_context_id": component["quest_context_id"] if component else None,
                }
            )
        return result


class BaseIndex:
    def __init__(self, db):
        self.db = db

    def ids(self, table):
        return self.db.ids(table)

    def names(self, table):
        return self.db.names(table)


def normalize_name(value):
    return " ".join((value or "").split()).casefold()


def names_compatible(first, second):
    a = normalize_name(first)
    b = normalize_name(second)
    return bool(a) and bool(b) and a == b


def has_custom_ids(borrowed):
    for borrowed_row in borrowed:
        row_id = borrowed_row.get("id")
        if isinstance(row_id, int) and row_id >= CUSTOM_ID_MIN:
            return True
        row = borrowed_row.get("row") or {}
        for column in CUSTOM_ID_COLUMNS:
            value = row.get(column)
            if isinstance(value, int) and value >= CUSTOM_ID_MIN:
                return True
    return False


def entry_bucket(entry):
    if entry.get("custom_origin"):
        return "custom"
    if entry.get("name_mismatch"):
        return "name_mismatch"
    return "default"


def bucket_counts(entries):
    counts = {"default": 0, "custom": 0, "name_mismatch": 0}
    for entry in entries:
        counts[entry_bucket(entry)] += 1
    return counts


def bucket_entries(entries, bucket):
    return [entry for entry in entries if entry_bucket(entry) == bucket]


def client_visible(borrowed, index):
    for borrowed_row in borrowed:
        table = borrowed_row.get("table")
        row_id = borrowed_row.get("id")
        if table in ASSET_TABLES and row_id not in index.ids(table):
            return True
        row = borrowed_row.get("row") or {}
        for column, target in REFERENCE_COLUMNS.items():
            value = row.get(column)
            if isinstance(value, int) and value > 0 and value not in index.ids(target):
                return True
        if table == "npc_spawner_npcs":
            member = row.get("member_id")
            member_type = (row.get("member_type") or "").casefold()
            if member_type == "npc" and member and member not in index.ids("npcs"):
                return True
    return False


def borrowed_row(db, table, row_id):
    if not db.has(table):
        return None
    row = db.row(f'select * from "{table}" where id = ?', (row_id,))
    if row is None:
        return None
    return {"table": table, "id": row_id, "row": row}


def find_quest_context_gaps(base, ref, index):
    entries = []
    skipped = []
    if not base.has("quest_contexts") or not base.has("quest_components"):
        return entries, skipped
    empty_contexts = base.rows(
        "select id, name from quest_contexts where id not in"
        " (select distinct quest_context_id from quest_components)"
    )
    for context in empty_contexts:
        context_id = context["id"]
        ref_context = ref.row(
            "select id, name from quest_contexts where id = ?", (context_id,)
        )
        if ref_context is None:
            orphaned = ref.rows(
                "select id from quest_components where quest_context_id = ?",
                (context_id,),
            )
            if orphaned:
                skipped.append(
                    {
                        "ids": {"quest_context_id": context_id},
                        "reason": "3.0 has orphaned components but no quest_contexts row",
                    }
                )
            continue
        components = ref.rows(
            "select * from quest_components where quest_context_id = ? order by id",
            (context_id,),
        )
        if not components:
            continue
        component_ids = [c["id"] for c in components]
        acts = ref.rows(
            f"select * from quest_acts where quest_component_id in {in_clause(component_ids)}",
            component_ids,
        )
        detail_rows = []
        for act in acts:
            table = ref.resolve_detail_table(act["act_detail_type"], act["act_detail_id"])
            if table:
                detail_rows.append((act, table))
        if not acts or not detail_rows:
            skipped.append(
                {
                    "ids": {"quest_context_id": context_id},
                    "reason": "3.0 components have no acts or no resolvable act detail rows",
                }
            )
            continue
        borrowed = [
            {"table": "quest_components", "id": c["id"], "row": c} for c in components
        ]
        borrowed += [
            {"table": "quest_acts", "id": a["id"], "row": a} for a in acts
        ]
        for act, table in detail_rows:
            row = borrowed_row(ref, table, act["act_detail_id"])
            if row:
                borrowed.append(row)
        for table, column in (
            ("quest_component_texts", "quest_component_id"),
            ("quest_chat_bubbles", "quest_component_id"),
        ):
            if ref.has(table):
                for row in ref.rows(
                    f'select * from "{table}" where {column} in'
                    f" {in_clause(component_ids)}",
                    component_ids,
                ):
                    borrowed.append({"table": table, "id": row["id"], "row": row})
        for table, column in (
            ("quest_context_texts", "quest_context_id"),
            ("quest_names", "quest_context_id"),
        ):
            if ref.has(table):
                for row in ref.rows(
                    f'select * from "{table}" where {column} = ?', (context_id,)
                ):
                    borrowed.append({"table": table, "id": row["id"], "row": row})
        detail_tables = sorted({table for _, table in detail_rows})
        entries.append(
            {
                "kind": "quest_context_without_components",
                "ids": {"quest_context_id": context_id},
                "base": {
                    "name": context["name"],
                    "components": 0,
                    "acts": 0,
                    "detail_rows": 0,
                },
                "ref": {
                    "name": ref_context["name"],
                    "components": len(components),
                    "acts": len(acts),
                    "detail_rows": len(detail_rows),
                    "detail_tables": detail_tables,
                },
                "name_mismatch": not names_compatible(
                    context["name"], ref_context["name"]
                ),
                "borrowed": borrowed,
                "client_visible": client_visible(borrowed, index),
                "custom_origin": has_custom_ids(borrowed),
            }
        )
    return entries, skipped


def find_item_use_skill_gaps(base, ref, index):
    entries = []
    if not base.has("items") or not base.has("skills") or not ref.has("items"):
        return entries
    base_skills = index.ids("skills")
    ref_skills = ref.ids("skills")
    for item in base.rows(
        "select id, name, use_skill_id from items where use_skill_id > 0 order by id"
    ):
        skill_id = item["use_skill_id"]
        if skill_id in base_skills:
            continue
        ref_item = ref.row(
            "select id, name, use_skill_id from items where id = ?", (item["id"],)
        )
        if ref_item is None:
            continue
        ref_skill_id = ref_item["use_skill_id"] or 0
        if ref_skill_id <= 0:
            continue
        if ref_skill_id == skill_id:
            if skill_id not in ref_skills:
                continue
        elif ref_skill_id not in ref_skills:
            continue
        skill = ref.row("select * from skills where id = ?", (ref_skill_id,))
        borrowed = [{"table": "skills", "id": ref_skill_id, "row": skill}]
        if ref.has("skill_effects"):
            for row in ref.rows(
                "select * from skill_effects where skill_id = ?", (ref_skill_id,)
            ):
                borrowed.append({"table": "skill_effects", "id": row["id"], "row": row})
        entries.append(
            {
                "kind": "item_use_skill_missing",
                "ids": {
                    "item_id": item["id"],
                    "base_skill_id": skill_id,
                    "ref_skill_id": ref_skill_id,
                },
                "base": {
                    "item_name": item["name"],
                    "use_skill_id": skill_id,
                    "skill_exists": False,
                    "skill_name": None,
                },
                "ref": {
                    "item_name": ref_item["name"],
                    "use_skill_id": ref_skill_id,
                    "skill_name": skill["name"] if skill else None,
                },
                "name_mismatch": not names_compatible(item["name"], ref_item["name"]),
                "borrowed": borrowed,
                "client_visible": client_visible(borrowed, index),
                "custom_origin": has_custom_ids(borrowed),
            }
        )
    return entries


def _missing_target_references(db, index, ref_ids):
    references = []
    detail_acts = db.collect_detail_acts()
    for table in sorted(detail_acts):
        columns = [c for c in TARGET_COLUMNS if c in db.columns(table)]
        if not columns:
            continue
        selected = ", ".join(f'"{c}"' for c in columns)
        for row in db.rows(f'select id, {selected} from "{table}"'):
            for column in columns:
                value = row[column]
                if not isinstance(value, int) or value <= 0:
                    continue
                target = TARGET_COLUMNS[column]
                if value in index.ids(target) or value not in ref_ids[target]:
                    continue
                references.append(
                    {
                        "target_kind": target,
                        "target_id": value,
                        "source": {
                            "table": table,
                            "row_id": row["id"],
                            "column": column,
                            "acts": detail_acts[table].get(row["id"], []),
                        },
                    }
                )
    for table, column, context_column in QUEST_ROW_SOURCES:
        if not db.has(table) or column not in db.columns(table):
            continue
        selected = f"id, {column}"
        if context_column in db.columns(table):
            selected += f", {context_column}"
        for row in db.rows(f'select {selected} from "{table}" where "{column}" > 0'):
            value = row[column]
            if value in index.ids("npcs") or value not in ref_ids["npcs"]:
                continue
            source = {"table": table, "row_id": row["id"], "column": column, "acts": []}
            if context_column in row:
                source[context_column] = row[context_column]
            references.append(
                {"target_kind": "npcs", "target_id": value, "source": source}
            )
    return references


def find_quest_reference_gaps(base, ref, index):
    entries = []
    ref_ids = {table: ref.ids(table) for table in set(TARGET_COLUMNS.values())}
    base_contexts = base.names("quest_contexts")
    ref_contexts = ref.names("quest_contexts")
    components = (
        {
            row["id"]: row["quest_context_id"]
            for row in base.rows("select id, quest_context_id from quest_components")
        }
        if base.has("quest_components")
        else {}
    )
    grouped = defaultdict(list)
    for reference in _missing_target_references(base, index, ref_ids):
        grouped[(reference["target_kind"], reference["target_id"])].append(
            reference["source"]
        )
    for (target, target_id) in sorted(grouped, key=lambda key: (key[0], key[1])):
        borrowed = []
        target_row = borrowed_row(ref, target, target_id)
        if target_row:
            borrowed.append(target_row)
        if target == "npcs":
            model_id = (target_row["row"].get("model_id") if target_row else 0) or 0
            if model_id and model_id not in index.ids("actor_models"):
                model_row = borrowed_row(ref, "actor_models", model_id)
                if model_row:
                    borrowed.append(model_row)
        context_ids = set()
        for source in grouped[(target, target_id)]:
            for act in source.get("acts", []):
                if act.get("quest_context_id"):
                    context_ids.add(act["quest_context_id"])
            if source.get("quest_context_id"):
                context_ids.add(source["quest_context_id"])
            if source.get("quest_component_id"):
                context_id = components.get(source["quest_component_id"])
                if context_id:
                    context_ids.add(context_id)
        name_check = []
        for context_id in sorted(context_ids):
            base_name = base_contexts.get(context_id)
            ref_name = ref_contexts.get(context_id)
            name_check.append(
                {
                    "quest_context_id": context_id,
                    "base_name": base_name,
                    "ref_name": ref_name,
                    "match": names_compatible(base_name, ref_name),
                }
            )
        comparable = [
            pair
            for pair in name_check
            if pair["base_name"] is not None and pair["ref_name"] is not None
        ]
        name_mismatch = bool(name_check) and (
            not comparable or any(not pair["match"] for pair in comparable)
        )
        entry = {
            "kind": "quest_reference_missing_target",
            "ids": {"target_kind": target, "target_id": target_id},
            "base": {"target_exists": False, "references": grouped[(target, target_id)]},
            "ref": {
                "name": (target_row["row"].get("name") if target_row else None),
                "model_id": (target_row["row"].get("model_id") if target_row else None),
            },
            "name_check": name_check,
            "name_mismatch": name_mismatch,
            "borrowed": borrowed,
            "client_visible": client_visible(borrowed, index),
            "custom_origin": has_custom_ids(borrowed),
        }
        entries.append(entry)
    return entries


def find_loot_gaps(base, ref, refsrv, index):
    empty_packs = []
    npcs_without_drops = []
    skipped = []
    examined = {}
    if not refsrv.has("loots"):
        return {
            "empty_loot_packs": empty_packs,
            "npcs_without_drops": npcs_without_drops,
            "skipped": skipped,
            "examined": examined,
        }
    base_packs = {p for p in base.ids("loots", "loot_pack_id") if p}
    refsrv_packs = refsrv.ids("loots", "loot_pack_id")
    for table in ("loots", "loot_pack_dropping_npcs", "doodad_func_loot_packs"):
        if not base.has(table):
            continue
        packs = base.ids(table, "loot_pack_id") - {0}
        candidates = sorted(p for p in packs if p not in base_packs)
        examined[table] = len(candidates)
        for pack in candidates:
            if pack not in refsrv_packs:
                continue
            loots_rows = refsrv.rows(
                "select * from loots where loot_pack_id = ? order by id", (pack,)
            )
            borrowed = [
                {"table": "loots", "id": row["id"], "row": row} for row in loots_rows
            ]
            references = []
            for source in ("loot_pack_dropping_npcs", "doodad_func_loot_packs"):
                if base.has(source):
                    for row in base.rows(
                        f'select * from "{source}" where loot_pack_id = ?', (pack,)
                    ):
                        references.append(
                            {"table": source, "id": row["id"], "row": row}
                        )
            empty_packs.append(
                {
                    "kind": "empty_loot_pack",
                    "ids": {"loot_pack_id": pack},
                    "base": {"loot_rows": 0, "referenced_by": references},
                    "ref": {"loot_rows": len(loots_rows), "server_table_db": True},
                    "borrowed": borrowed,
                    "client_visible": client_visible(borrowed, index),
                    "custom_origin": has_custom_ids(borrowed),
                }
            )
    if base.has("loot_pack_dropping_npcs"):
        base_drops = defaultdict(list)
        for row in base.rows("select * from loot_pack_dropping_npcs"):
            base_drops[row["npc_id"]].append(row)
        ref_drops = defaultdict(list)
        for row in refsrv.rows("select * from loot_pack_dropping_npcs"):
            ref_drops[row["npc_id"]].append(row)
        base_names = index.names("npcs")
        ref_names = ref.names("npcs")
        for npc_id in sorted(ref_drops):
            if any(
                row["loot_pack_id"] in base_packs for row in base_drops.get(npc_id, [])
            ):
                continue
            filled = [r for r in ref_drops[npc_id] if r["loot_pack_id"] in refsrv_packs]
            if not filled:
                continue
            if npc_id not in base_names or npc_id not in ref_names:
                skipped.append(
                    {
                        "ids": {"npc_id": npc_id},
                        "reason": "npc row missing in 1.2 or 3.0 npcs table",
                    }
                )
                continue
            if not names_compatible(base_names[npc_id], ref_names[npc_id]):
                name_mismatch = True
            else:
                name_mismatch = False
            borrowed = [
                {"table": "loot_pack_dropping_npcs", "id": row["id"], "row": row}
                for row in filled
            ]
            for pack in sorted({row["loot_pack_id"] for row in filled}):
                for row in refsrv.rows(
                    "select * from loots where loot_pack_id = ? order by id", (pack,)
                ):
                    borrowed.append({"table": "loots", "id": row["id"], "row": row})
            npcs_without_drops.append(
                {
                    "kind": "npc_without_drops",
                    "ids": {"npc_id": npc_id},
                    "base": {
                        "name": base_names[npc_id],
                        "dropping_rows": base_drops.get(npc_id, []),
                    },
                    "ref": {
                        "name": ref_names[npc_id],
                        "dropping_rows": filled,
                        "filled_packs": sorted({r["loot_pack_id"] for r in filled}),
                    },
                    "name_mismatch": name_mismatch,
                    "borrowed": borrowed,
                    "client_visible": client_visible(borrowed, index),
                    "custom_origin": has_custom_ids(borrowed),
                }
            )
    return {
        "empty_loot_packs": empty_packs,
        "npcs_without_drops": npcs_without_drops,
        "skipped": skipped,
        "examined": examined,
    }


def quest_needed_npcs(base):
    needed = set()
    for table in base.collect_detail_acts():
        columns = base.columns(table)
        for column in ("npc_id", "timer_npc_id"):
            if column not in columns:
                continue
            needed |= {
                row[column]
                for row in base.rows(
                    f'select distinct "{column}" from "{table}"'
                    f' where "{column}" > 0'
                )
            }
    if base.has("quest_components"):
        needed |= {
            row["npc_id"]
            for row in base.rows("select distinct npc_id from quest_components")
            if row["npc_id"] and row["npc_id"] > 0
        }
    return needed


def find_spawn_gaps(base, ref, refsrv, index):
    result = {"quest_npc_spawn_gaps": [], "spawn_data_differences": [], "skipped": []}
    if not refsrv.has("npc_spawner_npcs") or not refsrv.has("npc_spawners"):
        return result
    needed = quest_needed_npcs(base)
    direct_spawned = set()
    group_spawned = set()
    if base.has("npc_spawner_npcs"):
        for row in base.rows("select member_id, member_type from npc_spawner_npcs"):
            if (row["member_type"] or "").casefold() == "npc":
                direct_spawned.add(row["member_id"])
            elif (row["member_type"] or "").casefold() == "npcgroup":
                group_spawned.add(row["member_id"])
    groups_by_npc = defaultdict(set)
    if base.has("quest_monster_npcs"):
        for row in base.rows(
            "select quest_monster_group_id, npc_id from quest_monster_npcs"
        ):
            groups_by_npc[row["npc_id"]].add(row["quest_monster_group_id"])
    unspawned = {
        npc_id
        for npc_id in needed
        if npc_id not in direct_spawned
        and not (groups_by_npc.get(npc_id, set()) & group_spawned)
    }
    ref_spawned = {
        row["member_id"]
        for row in refsrv.rows(
            "select member_id from npc_spawner_npcs"
            " where lower(member_type) = 'npc'"
        )
    }
    for npc_id in sorted(unspawned & ref_spawned):
        rows = refsrv.rows(
            "select * from npc_spawner_npcs"
            " where member_id = ? and lower(member_type) = 'npc'",
            (npc_id,),
        )
        spawner_ids = sorted({row["npc_spawner_id"] for row in rows})
        borrowed = [
            {"table": "npc_spawner_npcs", "id": row["id"], "row": row} for row in rows
        ]
        for spawner_id in spawner_ids:
            spawner = borrowed_row(refsrv, "npc_spawners", spawner_id)
            if spawner:
                borrowed.append(spawner)
        ref_name = ref.names("npcs").get(npc_id)
        result["quest_npc_spawn_gaps"].append(
            {
                "kind": "quest_npc_without_spawner",
                "ids": {"npc_id": npc_id},
                "base": {
                    "name": index.names("npcs").get(npc_id),
                    "spawner_rows": [],
                },
                "ref": {"name": ref_name, "spawner_ids": spawner_ids},
                "borrowed": borrowed,
                "client_visible": client_visible(borrowed, index),
                "custom_origin": has_custom_ids(borrowed),
            }
        )
    base_spawner_ids = base.ids("npc_spawners")
    base_spawner_npc_ids = base.ids("npc_spawner_npcs")
    for spawner_id in sorted(refsrv.ids("npc_spawners") - base_spawner_ids):
        rows = refsrv.rows(
            "select * from npc_spawner_npcs where npc_spawner_id = ?", (spawner_id,)
        )
        borrowed = [borrowed_row(refsrv, "npc_spawners", spawner_id)]
        borrowed = [row for row in borrowed if row]
        borrowed += [
            {"table": "npc_spawner_npcs", "id": row["id"], "row": row} for row in rows
        ]
        members = sorted({row["member_id"] for row in rows})
        result["spawn_data_differences"].append(
            {
                "kind": "spawn_data_difference",
                "ids": {
                    "npc_spawner_id": spawner_id,
                    "npc_ids": members,
                },
                "base": {"spawner_exists": False},
                "ref": {
                    "spawner_name": (
                        borrowed_row(refsrv, "npc_spawners", spawner_id) or {}
                    ).get("row", {}).get("name"),
                    "member_ids": members,
                },
                "quest_linked": any(member in needed for member in members),
                "borrowed": borrowed,
                "client_visible": client_visible(borrowed, index),
                "custom_origin": has_custom_ids(borrowed),
            }
        )
    for row in refsrv.rows("select * from npc_spawner_npcs"):
        if row["id"] in base_spawner_npc_ids:
            continue
        if any(
            diff["ids"].get("npc_spawner_id") == row["npc_spawner_id"]
            for diff in result["spawn_data_differences"]
        ):
            continue
        result["spawn_data_differences"].append(
            {
                "kind": "spawn_data_difference",
                "ids": {
                    "npc_spawner_npc_id": row["id"],
                    "npc_spawner_id": row["npc_spawner_id"],
                    "npc_ids": [row["member_id"]],
                },
                "base": {"spawner_npc_exists": False},
                "ref": {"member_id": row["member_id"], "member_type": row["member_type"]},
                "quest_linked": row["member_id"] in needed,
                "borrowed": [
                    {"table": "npc_spawner_npcs", "id": row["id"], "row": row}
                ],
                "client_visible": client_visible(
                    [{"table": "npc_spawner_npcs", "id": row["id"], "row": row}], index
                ),
                "custom_origin": has_custom_ids(
                    [{"table": "npc_spawner_npcs", "id": row["id"], "row": row}]
                ),
            }
        )
    return result


def table_locations(base, ref, refsrv):
    locations = {}
    for table in LOCATION_TABLES:
        locations[table] = {
            "base_compact": base.has(table),
            "ref_compact": ref.has(table),
            "ref_server_table": refsrv.has(table),
            "base_rows": base.count(table),
            "ref_compact_rows": ref.count(table),
            "ref_server_table_rows": refsrv.count(table),
        }
    return locations


def build_report(base_path, ref_path, refsrv_path):
    started = time.time()
    base = Db(path=base_path)
    ref = Db(path=ref_path)
    refsrv = Db(path=refsrv_path)
    try:
        index = BaseIndex(base)
        quest_context_gaps, quest_context_skipped = find_quest_context_gaps(
            base, ref, index
        )
        item_skill_gaps = find_item_use_skill_gaps(base, ref, index)
        quest_reference_gaps = find_quest_reference_gaps(base, ref, index)
        loot = find_loot_gaps(base, ref, refsrv, index)
        spawn = find_spawn_gaps(base, ref, refsrv, index)
        summary = {
            "kind1_quest_contexts_without_components": bucket_counts(quest_context_gaps),
            "kind2_item_use_skill_missing": bucket_counts(item_skill_gaps),
            "kind3_quest_reference_missing_target": bucket_counts(quest_reference_gaps),
            "kind4a_empty_loot_packs_filled_in_ref": bucket_counts(
                loot["empty_loot_packs"]
            ),
            "kind4b_npcs_without_drops_filled_in_ref": bucket_counts(
                loot["npcs_without_drops"]
            ),
            "kind5_quest_npc_without_spawner": bucket_counts(
                spawn["quest_npc_spawn_gaps"]
            ),
            "kind5_spawn_data_differences": bucket_counts(
                spawn["spawn_data_differences"]
            ),
        }
        return {
            "generated_at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
            "elapsed_seconds": round(time.time() - started, 3),
            "rules": {
                "custom_id_min": CUSTOM_ID_MIN,
                "default_counts_exclude": ["custom_origin", "name_mismatch"],
            },
            "inputs": {
                "base": str(base_path),
                "ref": str(ref_path),
                "ref_server": str(refsrv_path),
            },
            "table_locations": table_locations(base, ref, refsrv),
            "gaps": {
                "kind1_quest_contexts_without_components": quest_context_gaps,
                "kind2_item_use_skill_missing": item_skill_gaps,
                "kind3_quest_reference_missing_target": quest_reference_gaps,
                "kind4_loot": {
                    "empty_loot_packs": loot["empty_loot_packs"],
                    "npcs_without_drops": loot["npcs_without_drops"],
                    "examined_empty_pack_candidates": loot["examined"],
                },
                "kind5_npc_spawn": {
                    "quest_npc_spawn_gaps": spawn["quest_npc_spawn_gaps"],
                    "spawn_data_differences": spawn["spawn_data_differences"],
                },
            },
            "skipped": {
                "kind1": quest_context_skipped,
                "kind4": loot["skipped"],
                "kind5": spawn["skipped"],
            },
            "summary": summary,
        }
    finally:
        base.close()
        ref.close()
        refsrv.close()


def example_lines(kind, entries):
    lines = []
    for entry in entries[:3]:
        ids = entry["ids"]
        if kind == "kind1":
            lines.append(
                "ctx {quest_context_id}: 1.2 '{base_name}' 0 components ->"
                " 3.0 '{ref_name}' {components} components/{acts} acts"
                " ({detail_tables})".format(
                    quest_context_id=ids["quest_context_id"],
                    base_name=entry["base"]["name"],
                    ref_name=entry["ref"]["name"],
                    components=entry["ref"]["components"],
                    acts=entry["ref"]["acts"],
                    detail_tables=", ".join(entry["ref"]["detail_tables"]),
                )
            )
        elif kind == "kind2":
            lines.append(
                "item {item_id} '{base_name}': skill {base_skill_id} missing in"
                " 1.2 -> 3.0 skill {ref_skill_id} '{ref_skill_name}'".format(
                    item_id=ids["item_id"],
                    base_name=entry["base"]["item_name"],
                    base_skill_id=ids["base_skill_id"],
                    ref_skill_id=ids["ref_skill_id"],
                    ref_skill_name=entry["ref"]["skill_name"],
                )
            )
        elif kind == "kind3":
            sources = ", ".join(
                sorted({ref["table"] for ref in entry["base"]["references"]})
            )
            lines.append(
                "{target_kind} {target_id} '{ref_name}' missing in 1.2,"
                " referenced by {sources}".format(
                    target_kind=ids["target_kind"],
                    target_id=ids["target_id"],
                    ref_name=entry["ref"]["name"],
                    sources=sources,
                )
            )
        elif kind == "kind4a":
            lines.append(
                "loot pack {loot_pack_id} empty in 1.2 -> {rows} loot rows in 3.0".format(
                    loot_pack_id=ids["loot_pack_id"], rows=entry["ref"]["loot_rows"]
                )
            )
        elif kind == "kind4b":
            lines.append(
                "npc {npc_id} '{name}' no drops in 1.2 -> packs {packs} in 3.0".format(
                    npc_id=ids["npc_id"],
                    name=entry["base"]["name"],
                    packs=entry["ref"]["filled_packs"],
                )
            )
        elif kind == "kind5":
            lines.append(
                "npc {npc_id} '{name}' no spawner in 1.2 -> spawners {spawners}".format(
                    npc_id=ids["npc_id"],
                    name=entry["ref"]["name"],
                    spawners=entry["ref"]["spawner_ids"],
                )
            )
        elif kind == "kind5diff":
            lines.append(
                "spawner {spawner_id} '{name}' new in 3.0 for npcs {npcs}"
                " (quest-linked: {linked})".format(
                    spawner_id=ids.get("npc_spawner_id"),
                    name=entry["ref"].get("spawner_name"),
                    npcs=ids.get("npc_ids"),
                    linked=entry["quest_linked"],
                )
            )
    return lines


def print_summary(report):
    gaps = report["gaps"]
    summary = report["summary"]
    print("=" * 72)
    print("Version-gap report: 1.2 base vs 3.0.4.2 reference")
    print("=" * 72)
    print(f"elapsed: {report['elapsed_seconds']}s")
    print(f"custom id threshold: {report['rules']['custom_id_min']}")
    print()
    print("Table locations:")
    for table, info in report["table_locations"].items():
        present = [
            name
            for name, key in (
                ("base", "base_compact"),
                ("ref", "ref_compact"),
                ("ref-server", "ref_server_table"),
            )
            if info[key]
        ]
        missing = [
            name
            for name, key in (
                ("base", "base_compact"),
                ("ref", "ref_compact"),
                ("ref-server", "ref_server_table"),
            )
            if not info[key]
        ]
        print(
            f"  {table}: present in {', '.join(present) or 'none'};"
            f" missing in {', '.join(missing) or 'none'}"
        )
    print()
    kind_sections = [
        (
            "kind1 quest contexts without components in 1.2",
            "kind1_quest_contexts_without_components",
            "kind1",
            gaps["kind1_quest_contexts_without_components"],
        ),
        (
            "kind2 item use_skill_id missing in 1.2",
            "kind2_item_use_skill_missing",
            "kind2",
            gaps["kind2_item_use_skill_missing"],
        ),
        (
            "kind3 quest rows referencing npc/item/doodad ids missing in 1.2",
            "kind3_quest_reference_missing_target",
            "kind3",
            gaps["kind3_quest_reference_missing_target"],
        ),
        (
            "kind4a empty loot packs filled in 3.0",
            "kind4a_empty_loot_packs_filled_in_ref",
            "kind4a",
            gaps["kind4_loot"]["empty_loot_packs"],
        ),
        (
            "kind4b npcs without drops in 1.2 filled in 3.0",
            "kind4b_npcs_without_drops_filled_in_ref",
            "kind4b",
            gaps["kind4_loot"]["npcs_without_drops"],
        ),
        (
            "kind5 quest npcs without spawner in 1.2",
            "kind5_quest_npc_without_spawner",
            "kind5",
            gaps["kind5_npc_spawn"]["quest_npc_spawn_gaps"],
        ),
        (
            "kind5 raw spawn data differences (not quest-linked)",
            "kind5_spawn_data_differences",
            "kind5diff",
            gaps["kind5_npc_spawn"]["spawn_data_differences"],
        ),
    ]
    for title, key, example_kind, entries in kind_sections:
        counts = summary[key]
        print(
            f"{title}: default {counts['default']}, custom {counts['custom']},"
            f" name_mismatch {counts['name_mismatch']}"
        )
        for line in example_lines(example_kind, bucket_entries(entries, "default")):
            print(f"    - {line}")
        for bucket in ("custom", "name_mismatch"):
            excluded = bucket_entries(entries, bucket)
            if not excluded:
                continue
            print(f"    ({bucket} excluded: {len(excluded)})")
            for line in example_lines(example_kind, excluded)[:2]:
                print(f"    - [{bucket}] {line}")
    examined = gaps["kind4_loot"]["examined_empty_pack_candidates"]
    if examined:
        print()
        print(
            "kind4a candidates examined (empty packs per 1.2 source table):"
            f" {examined}"
        )
    skipped = report["skipped"]
    if any(skipped.values()):
        print()
        print("Skipped candidates:")
        for key, items in skipped.items():
            for item in items[:3]:
                print(f"  {key} {item['ids']}: {item['reason']}")
    print()
    print("Summary counts (default excludes custom_origin and name_mismatch):")
    for key, value in summary.items():
        print(
            f"  {key}: default {value['default']}, custom {value['custom']},"
            f" name_mismatch {value['name_mismatch']}"
        )


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", required=True, help="1.2 compact.sqlite3")
    parser.add_argument("--ref", required=True, help="3.0.4.2 compact.sqlite3")
    parser.add_argument(
        "--ref-server", required=True, help="3.0.4.2 compact.server.table.sqlite3"
    )
    parser.add_argument("--output", required=True, help="JSON report path")
    args = parser.parse_args(argv)
    report = build_report(args.base, args.ref, args.ref_server)
    output_path = Path(args.output)
    with output_path.open("w", encoding="utf-8") as handle:
        json.dump(report, handle, ensure_ascii=False, indent=2, default=str)
    print_summary(report)
    print()
    print(f"report written to {output_path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
