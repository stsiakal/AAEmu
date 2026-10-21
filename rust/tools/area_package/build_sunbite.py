#!/usr/bin/env python3
"""Build the optional `sunbite-wilds` area package.

The 1.2 client already contains the Sunbite Wilds 3+4 terrain and the 1.2 `world.xml`
declares the two zones (`zone_key` 286 / 287), but the 1.2 server data has no `zones` rows
for them and no content. This tool imports the later-version server data as a **separate**
package:

* zones 242 (`e_sunny_wilderness_3`, key 286) and 227 (`e_sunny_wilderness_4`, key 287);
* the 3.0.4.2 NPC / item / quest / loot templates whose ids are absent from 1.2 (a colliding
  id is remapped into a free range, never overwritten);
* the NPC spawner templates and placements. 3.0 ships the spawner *tables* but no placement
  files for these zones; the placements are the 10.x client's authored
  `level_design/zone/{286,287}/zone_server/npc_spawners.g`, translated from the 10.x zone
  frame to the 1.2 `world.xml` frame (documented guess, see `Docs/wiki/Backports.md`);
* quests at or below level 55 with their givers / rewards; items are capped at level 62;
* loot packs of the newly imported NPCs, filtered to the same cap.

Data sources (pass them explicitly; nothing is downloaded):

    --db12   1.2 compact.sqlite3 (target; read-only)
    --db30   3.0.4.2 compact.sqlite3
    --db30s  3.0.4.2 compact.server.table.sqlite3
    --db10   10.x game DB
    --pak10  10.x client tree (or its extracted `game/` directory)
    --w10    10.x world.xml tree (contains `game/worlds/main_world/world.xml`)
    --w12    1.2 world.xml tree

Optional `--heights <json>` from the ignored test
`AAEMU_GAME_PAK=<1.2 pak> AAEMU_HEIGHT_QUERY=<q> AAEMU_HEIGHT_OUT=<h>
cargo test -p aaemu-game dump_terrain_heights -- --ignored`
snaps the spawn Z to the 1.2 terrain when it is within 5 m; otherwise the authored 10.x Z is
kept (the 1.2 and 10.x cells differ in places).

Regeneration command (from the repository root):

    python -I rust/tools/area_package/build_sunbite.py \
        --db12 <1.2 compact.sqlite3> \
        --db30 D:/aa-re/db-references/3.0.4.2-20240828/compact.sqlite3 \
        --db30s D:/aa-re/db-references/3.0.4.2-20240828/compact.server.table.sqlite3 \
        --db10 D:/aa-re/db-references/server-db-probe-20261019/extracted/aa10x__game_db_game.sqlite3.decrypted.sqlite3 \
        --pak10 D:/aaemu-audits/research/spawns/pak10 \
        --w10 D:/aaemu-audits/research/spawns/w10 \
        --w12 D:/aaemu-audits/research/spawns/w12

The patch files under `SQL/patches/compact/` and the spawn JSON are overwritten atomically;
the tool does not touch a database.
"""
import argparse
import collections
import json
import math
import os
import re
import sqlite3
import sys
import xml.etree.ElementTree as ET

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
SPAWN_IMPORT = os.path.join(REPO, "rust", "tools", "spawn_import")
sys.path.insert(0, SPAWN_IMPORT)
from spawn_pak import read_spawner_file, spawner_files, zone_origins  # noqa: E402

PACKAGE = "sunbite-wilds"
ZONE_ROWS = [  # (1.2 target id, name, zone_key, group_id, faction_id, climate_id)
    (242, "e_sunny_wilderness_3", 286, 13, 149, 5),
    (227, "e_sunny_wilderness_4", 287, 13, 149, 5),
]
QUEST_ZONE_IDS = [227, 242]
WORLD = "main_world"
ITEM_LEVEL_CAP = 62
PLAYER_LEVEL_CAP = 55
ITEM_ID_CAP = 8_000_000
HEIGHT_TOL = 5.0
CLIP = 1023.5

PATCH_NAMES = {
    "zones": "2026-10-21-sunbite-wilds-1-zones.sql",
    "npcs": "2026-10-21-sunbite-wilds-2-npcs.sql",
    "items": "2026-10-21-sunbite-wilds-3-items.sql",
    "quests": "2026-10-21-sunbite-wilds-4-quests.sql",
    "loot": "2026-10-21-sunbite-wilds-5-loot.sql",
    "doodads": "2026-10-21-sunbite-wilds-6-doodads.sql",
}


# ---------------------------------------------------------------------------------------------
# SQLite helpers
# ---------------------------------------------------------------------------------------------
def connect_ro(path):
    return sqlite3.connect("file:%s?mode=ro" % path.replace("\\", "/"), uri=True)


def rows(conn, sql, *args):
    return conn.execute(sql, args).fetchall()


def columns(conn, table):
    return [r[1] for r in rows(conn, "PRAGMA table_info(%s)" % quote_ident(table))]


def quote_ident(name):
    return '"%s"' % name.replace('"', '""')


def sql_value(v):
    if v is None:
        return "NULL"
    if isinstance(v, bool):
        return "1" if v else "0"
    if isinstance(v, (int, float)):
        if isinstance(v, float) and not math.isfinite(v):
            return "NULL"
        return repr(v)
    if isinstance(v, bytes):
        return "X'%s'" % v.hex()
    s = str(v).replace("\r\n", "\n").replace("\r", "\n").replace("'", "''")
    return "'%s'" % s


_TARGET = None
_NULL_DEFAULTS = {}


def _null_default(table, col):
    key = (table, col)
    if key not in _NULL_DEFAULTS:
        kind = "".join(
            r[2] or ""
            for r in _TARGET.execute("PRAGMA table_info(%s)" % quote_ident(table))
            if r[1] == col
        ).upper()
        if "REAL" in kind or "FLOA" in kind or "DOUB" in kind:
            _NULL_DEFAULTS[key] = 0.0
        elif "BLOB" in kind:
            _NULL_DEFAULTS[key] = b""
        elif "CHAR" in kind or "TEXT" in kind or "CLOB" in kind:
            _NULL_DEFAULTS[key] = ""
        else:
            _NULL_DEFAULTS[key] = 0
    return _NULL_DEFAULTS[key]


def insert_statement(table, cols, values, natural_key=None):
    """Idempotent insert: PK tables use OR IGNORE, join tables a NOT EXISTS guard."""
    if _TARGET is not None:
        values = [v if v is not None else _null_default(table, c) for c, v in zip(cols, values)]
    # Several 1.2 server tables (npc_spawners, loots, ...) have no PRIMARY KEY, so
    # `INSERT OR IGNORE` does not make them rerun-safe. Guard every row by its id (or the
    # caller's natural key) with NOT EXISTS.
    if natural_key is None and "id" in cols:
        natural_key = ("id",)
    names = ", ".join(quote_ident(c) for c in cols)
    vals = ", ".join(sql_value(v) for v in values)
    if natural_key:
        cond = " AND ".join(
            "%s = %s" % (quote_ident(c), sql_value(values[cols.index(c)]))
            for c in natural_key
        )
        return (
            "INSERT INTO %s (%s) SELECT %s WHERE NOT EXISTS "
            "(SELECT 1 FROM %s WHERE %s);" % (quote_ident(table), names, vals, quote_ident(table), cond)
        )
    return "INSERT OR IGNORE INTO %s (%s) VALUES (%s);" % (quote_ident(table), names, vals)


def row_value(row, col):
    """Case-insensitive column read (1.2 spells some columns `LEVEL`, `REPEATABLE`)."""
    if col in row:
        return row[col]
    lower = col.lower()
    for key, value in row.items():
        if key.lower() == lower:
            return value
    return None


def common_columns(target, source, table):
    """Every target column (target-only columns get the type default in `insert_statement`).

    The 1.2 loaders read some columns without a NULL default (`recruiting_battle_field_id`,
    `milestone_id`, ...); those columns do not exist in the later source tables, so they must be
    written explicitly.
    """
    return columns(target, table)


def copy_rows(target, source, table, where, args, remap=None, natural_key=None,
              source_table=None):
    """Rows of `source.table` -> INSERT statements against `target` (common columns)."""
    source_table = source_table or table
    cols = common_columns(target, source, table)
    out = []
    for r in rows(source, "SELECT * FROM %s WHERE %s" % (quote_ident(source_table), where), *args):
        row = dict(zip(columns(source, source_table), r))
        if remap:
            row = remap(row)
        out.append(insert_statement(table, cols, [row_value(row, c) for c in cols], natural_key))
    return out


def localized(conn, table, column, ids):
    if not ids:
        return {}
    marks = ",".join("?" * len(ids))
    return {
        r[0]: r[1]
        for r in rows(
            conn,
            "SELECT idx, en_us FROM localized_texts WHERE tbl_name=? AND tbl_column_name=? "
            "AND idx IN (%s)" % marks,
            table, column, *ids,
        )
        if r[1]
    }


class Ids:
    def __init__(self, start):
        self.next = start
        self.map = {}

    def get(self, key):
        if key not in self.map:
            self.map[key] = self.next
            self.next += 1
        return self.map[key]

    def maybe(self, key, exists):
        """Keep `key` when it is free in 1.2, else allocate."""
        if key in self.map:
            return self.map[key]
        if not exists(key):
            self.map[key] = key
            return key
        return self.get(key)


# ---------------------------------------------------------------------------------------------
# Data collection
# ---------------------------------------------------------------------------------------------
CLASS_TABLE = {}


def loader_tables(repo=REPO):
    """The `quest_act_*` tables the Rust loader actually reads (class -> table)."""
    path = os.path.join(REPO, "rust", "crates", "aaemu-game", "src", "systems", "quests", "data.rs")
    with open(path, encoding="utf-8") as f:
        text = f.read()
    pairs = re.findall(
        r'load_table\(\s*conn,\s*"([a-z0-9_]+)",\s*"([A-Za-z0-9_]+)"', text, re.S
    )
    return {cls: table for table, cls in pairs}


def detail_table_for(cls):
    return CLASS_TABLE.get(cls)


def quest_rows(conn30):
    ids = [r[0] for r in rows(conn30, "SELECT id FROM quest_contexts WHERE zone_id IN (%s) AND level <= %d ORDER BY id" % (",".join("?" * len(QUEST_ZONE_IDS)), PLAYER_LEVEL_CAP), *QUEST_ZONE_IDS)]
    if not ids:
        return [], [], []
    qm = ",".join("?" * len(ids))
    comps = rows(conn30, "SELECT * FROM quest_components WHERE quest_context_id IN (%s) ORDER BY id" % qm, *ids)
    cids = [c[0] for c in comps]
    if not cids:
        return ids, comps, []
    cm = ",".join("?" * len(cids))
    acts = rows(
        conn30,
        "SELECT id, quest_component_id, act_detail_id, act_detail_type "
        "FROM quest_acts WHERE quest_component_id IN (%s) ORDER BY id" % cm,
        *cids,
    )
    return ids, comps, acts


def detail_rows(conn30, acts):
    """`{(class, detail_id): row}` for the classes the Rust loader reads."""
    by_class = collections.defaultdict(set)
    for a in acts:
        cls, did = a[3], a[2]
        if detail_table_for(cls):
            by_class[cls].add(did)
    out = {}
    for cls, ids in by_class.items():
        table = detail_table_for(cls)
        marks = ",".join("?" * len(ids))
        cn = columns(conn30, table)
        for r in rows(conn30, "SELECT * FROM %s WHERE id IN (%s)" % (quote_ident(table), marks), *ids):
            d = dict(zip(cn, r))
            out[(cls, d["id"])] = d
    return out


def spawn_template_members(c12, c30, c10, template_id):
    for conn in (c12, c30, c10):
        tabs = {r[0] for r in rows(conn, "SELECT name FROM sqlite_master WHERE type='table'")}
        if "npc_spawner_npcs" not in tabs:
            continue
        mem = rows(conn, "SELECT member_id, member_type FROM npc_spawner_npcs WHERE npc_spawner_id=? ORDER BY id", template_id)
        if mem:
            return mem, conn
    return [], None


def collect_spawns(c10, pak10, w10, w12):
    o10 = zone_origins(w10)
    o12 = zone_origins(w12)
    blocks = []
    for world, zone, path in spawner_files(pak10):
        if world != WORLD or zone not in (286, 287):
            continue
        if ("main_world", zone) not in o10 or ("main_world", zone) not in o12:
            raise SystemExit("missing world.xml origin for zone %s" % zone)
        ox10, oy10, _ = o10[(WORLD, zone)]
        ox12, oy12, _ = o12[(WORLD, zone)]
        for s in read_spawner_file(path):
            if not s["points"]:
                continue
            p = s["points"][0]
            x = ox10 * 1024 + p[0] + (ox12 - ox10) * 1024
            y = oy10 * 1024 + p[1] + (oy12 - oy10) * 1024
            yaw = math.degrees(s["zrot"][0]) if s["zrot"] else 0.0
            blocks.append(
                {
                    "zone": zone,
                    "template": s["template"],
                    "x": round(x, 3),
                    "y": round(y, 3),
                    "z": round(p[2], 3),
                    "yaw": round(yaw, 4),
                }
            )
    return blocks


def world_xml(root):
    for base in (root, os.path.join(root, "game")):
        path = os.path.join(base, "worlds", "main_world", "world.xml")
        if os.path.exists(path):
            return path
    raise SystemExit("no main_world world.xml under %s" % root)


def zone_cells(path, zone_key, owned_only):
    """Cells of a zone in `world.xml`; `owned_only` keeps cells with at least one sector."""
    root = ET.parse(path).getroot()
    for zone in root.iter("Zone"):
        if int(zone.attrib.get("id", "-1")) != zone_key:
            continue
        cells = []
        for cell in zone.iter("cell"):
            if owned_only and not list(cell.iter("sector")):
                continue
            cells.append((int(cell.attrib["x"]), int(cell.attrib["y"])))
        return sorted(cells)
    return []


def cell_map_spawns(blocks, w10, w12):
    """Map each 10.x placement into the 1.2 zone's owned cells.

    The 10.x zones moved (origin (21,4) instead of (18,4)/(18,5)) and their cell shapes differ;
    a plain origin shift drops many placements into `e_sylvina_region` / `e_sunny_wilderness_2`
    sectors, which the 1.2 `world.xml` owns instead. Map the 10.x cell index proportionally onto
    the 1.2 zone's cells that actually carry sectors, keeping the local offset inside the cell.
    """
    o10 = zone_origins(w10)
    o12 = zone_origins(w12)
    xml10 = world_xml(w10)
    xml12 = world_xml(w12)
    cells10 = {z: zone_cells(xml10, z, owned_only=False) for z in (286, 287)}
    cells12 = {z: zone_cells(xml12, z, owned_only=True) for z in (286, 287)}
    for b in blocks:
        zone = b["zone"]
        ox10, oy10, _ = o10[(WORLD, zone)]
        ox12, oy12, _ = o12[(WORLD, zone)]
        # `collect_spawns` stored the 1.2-translated coordinates; undo the origin delta first.
        wx10 = b["x"] - (ox12 - ox10) * 1024
        wy10 = b["y"] - (oy12 - oy10) * 1024
        lx = wx10 - ox10 * 1024
        ly = wy10 - oy10 * 1024
        cx = int(math.floor(lx / 1024))
        cy = int(math.floor(ly / 1024))
        src = cells10[zone]
        try:
            i = src.index((cx, cy))
        except ValueError:
            i = 0
        dst = cells12[zone]
        if not dst:
            continue
        j = round(i * (len(dst) - 1) / (len(src) - 1)) if len(src) > 1 else 0
        tx, ty = dst[j]
        b["x"] = round(tx * 1024 + (lx - cx * 1024), 3)
        b["y"] = round(ty * 1024 + (ly - cy * 1024), 3)
        b["frame"] = "12x"
    # Cells are only partially owned: the zone's sectors inside a cell are a subset. Clamp a
    # placement that still lands in another zone's sector to the nearest sector the zone owns.
    owners = sector_owners(w12)
    owned = collections.defaultdict(list)
    for (sx, sy), key in owners.items():
        owned[key].append((sx, sy))
    for b in blocks:
        sector = (int(b["x"] // 64), int(b["y"] // 64))
        if owners.get(sector) == b["zone"]:
            continue
        nearest = min(
            owned[b["zone"]],
            key=lambda s: (s[0] * 64 + 32 - b["x"]) ** 2 + (s[1] * 64 + 32 - b["y"]) ** 2,
        )
        b["x"] = round(nearest[0] * 64 + (b["x"] % 64), 3)
        b["y"] = round(nearest[1] * 64 + (b["y"] % 64), 3)
        b["zone_adjusted"] = True
    return blocks


def sector_owners(w12):
    """Sector -> zone key with the server's last-zone-wins rule."""
    root = ET.parse(world_xml(w12)).getroot()
    owners = {}
    for zone in root.iter("Zone"):
        key = int(zone.attrib.get("id", "0"))
        for cell in zone.iter("cell"):
            cx, cy = int(cell.attrib["x"]), int(cell.attrib["y"])
            for sector in cell.iter("sector"):
                owners[
                    (int(sector.attrib["x"]) + cx * 16, int(sector.attrib["y"]) + cy * 16)
                ] = key
    return owners


def assert_spawns_in_zone(blocks, w12):
    owners = sector_owners(w12)
    bad = []
    for b in blocks:
        sector = (int(b["x"] // 64), int(b["y"] // 64))
        owner = owners.get(sector, 0)
        if owner != b["zone"]:
            bad.append((b["zone"], owner, round(b["x"], 1), round(b["y"], 1)))
    if bad:
        raise SystemExit(
            "%d placements are outside their 1.2 zone sectors (first: %s)"
            % (len(bad), bad[:5])
        )


def resolve_z(entry, heights):
    h = heights.get(entry["id"])
    if not h or "h" not in h:
        return entry["z"], "authored"
    hz = h["h"]
    if hz < CLIP and abs(entry["z"] - hz) <= HEIGHT_TOL:
        return round(hz, 3), "terrain"
    return entry["z"], "authored"


# ---------------------------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------------------------
def build(args):
    global _TARGET
    c12, c30, c30s, c10 = (connect_ro(p) for p in (args.db12, args.db30, args.db30s, args.db10))
    _TARGET = c12
    CLASS_TABLE.update(loader_tables())
    heights = {}
    if args.heights:
        heights = {r["id"]: r for r in json.load(open(args.heights, encoding="utf-8"))}
    stats = collections.Counter()
    skipped = collections.Counter()

    t12 = {r[0] for r in rows(c12, "SELECT name FROM sqlite_master WHERE type='table'")}
    t30 = {r[0] for r in rows(c30, "SELECT name FROM sqlite_master WHERE type='table'")}
    t30s = {r[0] for r in rows(c30s, "SELECT name FROM sqlite_master WHERE type='table'")}
    t10 = {r[0] for r in rows(c10, "SELECT name FROM sqlite_master WHERE type='table'")}

    # ----- spawns -------------------------------------------------------------------------
    blocks = collect_spawns(c10, args.pak10, args.w10, args.w12)
    blocks = cell_map_spawns(blocks, args.w10, args.w12)
    assert_spawns_in_zone(blocks, args.w12)
    for i, b in enumerate(blocks):
        b["id"] = "sb:%s" % i
        b["z"], b["z_basis"] = resolve_z(b, heights)
    stats["spawn_blocks"] = len(blocks)
    stats["spawn_zone_adjusted"] = sum(1 for b in blocks if b.get("zone_adjusted"))

    templates = sorted({b["template"] for b in blocks})
    members = {}
    for tid in templates:
        m, source = spawn_template_members(c12, c30, c10, tid)
        if not m:
            raise SystemExit("spawner %s has no members in any source" % tid)
        members[tid] = [(mid, mt) for mid, mt in m if mt == "Npc"]
        if tid in {r[0] for r in rows(c12, "SELECT id FROM npc_spawners")}:
            source_kind = "1.2"
        elif tid in {r[0] for r in rows(c10, "SELECT id FROM npc_spawners")}:
            source_kind = "10.x"
        elif tid in {r[0] for r in rows(c30s, "SELECT id FROM npc_spawners")}:
            source_kind = "3.0"
        else:
            source_kind = "generated"
        stats["spawner_templates_" + source_kind] += 1

    spawn_npcs = {mid for m in members.values() for mid, _ in m}
    spawn_entries = []
    for b in blocks:
        mem = members[b["template"]]
        if not mem:
            skipped["spawn block without NPC member"] += 1
            continue
        spawn_entries.append(
            {
                "title": "sunbite-wilds: 10.x spawner %s (zone %s)" % (b["template"], b["zone"]),
                "unit_id": mem[0][0],
                "spawner_id": b["template"],
                "x": b["x"],
                "y": b["y"],
                "z": b["z"],
                "yaw": b["yaw"],
            }
        )
    stats["spawn_entries"] = len(spawn_entries)
    stats["spawn_z_terrain"] = sum(1 for b in blocks if b["z_basis"] == "terrain")

    # ----- quests -------------------------------------------------------------------------
    qids, comps, acts = quest_rows(c30)
    details = detail_rows(c30, acts)
    stats["quest_candidates"] = len(qids)
    stats["quest_components"] = len(comps)
    stats["quest_acts"] = len(acts)

    # NPC / item / doodad / group references per quest (resolved later).
    comp_quest = {}
    for c in comps:
        cc = dict(zip(columns(c30, "quest_components"), c))
        comp_quest[cc["id"]] = cc["quest_context_id"]


    n12 = {r[0] for r in rows(c12, "SELECT id FROM npcs")}
    i12 = {r[0] for r in rows(c12, "SELECT id FROM items")}
    d12 = {r[0] for r in rows(c12, "SELECT id FROM doodad_almighties")}
    m12 = {r[0] for r in rows(c12, "SELECT id FROM models")}
    n30 = {r[0]: dict(zip(columns(c30, "npcs"), r)) for r in rows(c30, "SELECT * FROM npcs")}
    n10 = {r[0]: dict(zip(columns(c10, "npcs"), r)) for r in rows(c10, "SELECT * FROM npcs")}
    i30 = {r[0]: dict(zip(columns(c30, "items"), r)) for r in rows(c30, "SELECT * FROM items")}
    i10 = {r[0]: dict(zip(columns(c10, "items"), r)) for r in rows(c10, "SELECT * FROM items")}
    d30 = {r[0]: dict(zip(columns(c30, "doodad_almighties"), r)) for r in rows(c30, "SELECT * FROM doodad_almighties")}
    d10 = {r[0]: dict(zip(columns(c10, "doodad_almighties"), r)) for r in rows(c10, "SELECT * FROM doodad_almighties")}

    def npc_ok(nid):
        if nid in n12:
            return True
        row = n30.get(nid) or n10.get(nid)
        return (
            row is not None
            and row.get("model_id") in m12
            and row.get("level", 0) <= PLAYER_LEVEL_CAP
        )

    def item_ok(iid):
        if iid == 0 or iid in i12:
            return True
        row = i30.get(iid) or i10.get(iid)
        return row is not None and row.get("level", 0) <= ITEM_LEVEL_CAP

    def doodad_ok(did):
        return did == 0 or did in d12 or did in d30 or did in d10

    # item / npc groups (3.0)
    q_item_groups = collections.defaultdict(set)
    for r in rows(c30, "SELECT quest_item_group_id, item_id FROM quest_item_group_items"):
        q_item_groups[r[0]].add(r[1])
    q_monster_groups = collections.defaultdict(set)
    for r in rows(c30, "SELECT quest_monster_group_id, npc_id FROM quest_monster_npcs"):
        q_monster_groups[r[0]].add(r[1])
    def act_refs(row):
        npcs, items, doodads, igroups, mgroups, spheres = set(), set(), set(), set(), set(), set()
        for k, v in row.items():
            if not v:
                continue
            if k == "npc_id" or k.endswith("_npc_id"):
                npcs.add(v)
            elif k == "item_id" or k.endswith("_item_id"):
                items.add(v)
            elif k == "doodad_id":
                doodads.add(v)
            elif k == "item_group_id" or k == "quest_item_group_id":
                igroups.add(v)
            elif (
                k == "quest_monster_group_id"
                or k == "monster_group_id"
                or k == "npc_group_id"
            ):
                # Quest act "NPC groups" are `quest_monster_npcs` groups (QuestData.group_npcs);
                # the `npc_groups` table is spawner formation data and is not used by quests.
                mgroups.add(v)
            elif k == "sphere_id":
                spheres.add(v)
        return npcs, items, doodads, igroups, mgroups, spheres

    bad_quests = set()
    req_npcs, req_items, req_doodads = set(), set(), set()
    req_igroups, req_mgroups, req_spheres = set(), set(), set()
    for (cls, did), row in details.items():
        npcs, items, doodads, ig, mg, sp = act_refs(row)
        req_npcs |= npcs
        req_items |= items
        req_doodads |= doodads
        req_igroups |= ig
        req_mgroups |= mg
        req_spheres |= sp
    req_npcs |= spawn_npcs
    for a in acts:
        cls, did = a[3], a[2]
        row = details.get((cls, did))
        if row is None:
            continue
        for npc in act_refs(row)[0]:
            if not npc_ok(npc):
                bad_quests.add(comp_quest[a[1]])
        for item in act_refs(row)[1]:
            if not item_ok(item):
                bad_quests.add(comp_quest[a[1]])
        for dd in act_refs(row)[2]:
            if not doodad_ok(dd):
                bad_quests.add(comp_quest[a[1]])
        for g in act_refs(row)[3]:
            if not q_item_groups.get(g) or any(not item_ok(i) for i in q_item_groups[g]):
                bad_quests.add(comp_quest[a[1]])
        for g in act_refs(row)[4]:
            if not q_monster_groups.get(g):
                bad_quests.add(comp_quest[a[1]])
    # group members are candidate refs too
    for g in req_igroups:
        req_items |= q_item_groups.get(g, set())
    for g in req_mgroups:
        req_npcs |= q_monster_groups.get(g, set())

    for q in sorted(bad_quests):
        skipped["quest with unresolvable reference %s" % q] += 1
    qids = [q for q in qids if q not in bad_quests]
    good_comp = {c for c, q in comp_quest.items() if q in set(qids)}
    comps = [c for c in comps if c[0] in good_comp]
    acts = [a for a in acts if a[1] in good_comp]
    stats["quests"] = len(qids)
    stats["quest_components_kept"] = len(comps)
    stats["quest_acts_kept"] = len(acts)

    # ----- item / npc / doodad import sets ------------------------------------------------
    item_ids = {i for i in req_items if i not in i12 and i > 0}
    npc_ids = {n for n in req_npcs if n not in n12 and npc_ok(n)}
    doodad_ids = {d for d in req_doodads if d not in d12 and d > 0}
    stats["items_new"] = len(item_ids)
    stats["npcs_new"] = len(npc_ids)
    stats["doodads_new"] = len(doodad_ids)

    # remap a colliding item id or an id >= 8,000,000 into a free range below the cap
    item_max = max(
        [r[0] for r in rows(c12, "SELECT max(id) FROM items")]
        + [r[0] for r in rows(c30, "SELECT max(id) FROM items")]
        + [0]
    )
    item_ids_alloc = Ids(item_max + 1)
    item_map = {}
    for iid in sorted(item_ids):
        if iid in i12:
            continue
        if iid >= ITEM_ID_CAP:
            item_map[iid] = item_ids_alloc.get(iid)
        else:
            item_map[iid] = iid
    stats["items_remapped"] = len(item_map) - len(set(item_map.values()) & set(item_map.keys()))

    # Doodad ids can collide: keep free ones, remap the rest.
    dmax = max([r[0] for r in rows(c12, "SELECT max(id) FROM doodad_almighties")] + [0])
    doodad_alloc = Ids(dmax + 1)
    doodad_map = {}
    for did in sorted(doodad_ids):
        if did not in d12:
            doodad_map[did] = did
        else:
            doodad_map[did] = doodad_alloc.get(did)
    # (the current package has no colliding doodads; the branch is future-proof)
    for did in sorted(doodad_ids):
        if did in d12:
            doodad_map[did] = doodad_map.get(did, did)

    # ----- ID allocation ------------------------------------------------------------------
    def maxid(conn, table, col="id"):
        r = rows(conn, "SELECT max(%s) FROM %s" % (col, quote_ident(table)))
        return r[0][0] or 0

    comp_alloc = Ids(maxid(c12, "quest_components") + 1)
    act_alloc = Ids(maxid(c12, "quest_acts") + 1)
    detail_max = max([maxid(c12, t) for t in CLASS_TABLE.values() if t in t12] + [0])
    detail_alloc = Ids(detail_max + 1)
    req_alloc = Ids(maxid(c12, "unit_reqs") + 1)
    sphere_alloc = Ids(maxid(c12, "spheres") + 1)
    ig_alloc = Ids(maxid(c12, "quest_item_groups") + 1)
    mg_alloc = Ids(maxid(c12, "quest_monster_groups") + 1)
    loot_pack_alloc = Ids(maxid(c12, "loots", "loot_pack_id") + 1)
    loot_alloc = Ids(maxid(c12, "loots") + 1)
    lg_alloc = Ids(maxid(c12, "loot_groups") + 1)

    quest_map = {q: q for q in qids}
    comp_map = {c: comp_alloc.get(c) for c in good_comp}
    act_map = {a[0]: act_alloc.get(a[0]) for a in acts}
    detail_map = {}
    for a in acts:
        cls, did = a[3], a[2]
        if (cls, did) not in detail_map:
            detail_map[(cls, did)] = detail_alloc.get((cls, did))

    def map_item(i):
        return item_map.get(i, i)

    def map_npc(n):
        return n

    def map_doodad(d):
        return doodad_map.get(d, d)

    ig_map = {g: ig_alloc.maybe(g, lambda x: any(r[0] == x for r in rows(c12, "SELECT id FROM quest_item_groups"))) for g in req_igroups}
    mg_map = {g: mg_alloc.maybe(g, lambda x: any(r[0] == x for r in rows(c12, "SELECT id FROM quest_monster_groups"))) for g in req_mgroups}
    sphere_ids = {s for s in req_spheres if s not in {r[0] for r in rows(c12, "SELECT id FROM spheres")}}
    sphere_map = {s: sphere_alloc.get(s) for s in sphere_ids}
    sphere_map.update({s: s for s in req_spheres if s not in sphere_ids})

    # ----- zones patch --------------------------------------------------------------------
    zone_patch = [
        "-- The 1.2 client already declares both zones in world.xml; only the server rows are",
        "-- missing. The 1.2 zone group 13 (`e_sunny_wilderness`) already exists and is reused.",
    ]
    zcols = ["id", "name", "zone_key", "group_id", "closed", "faction_id", "zone_climate_id"]
    for zid, name, key, group, faction, climate in ZONE_ROWS:
        zone_patch.append(
            insert_statement(
                "zones", zcols, [zid, name, key, group, 0, faction, climate]
            )
        )
    stats["zones"] = len(ZONE_ROWS)

    # ----- NPC patch ----------------------------------------------------------------------
    npc_patch = []
    npc_cols = common_columns(c12, c30, "npcs")
    en_npc = localized(c30, "npcs", "name", sorted(npc_ids))
    for nid in sorted(npc_ids):
        row = dict(n30.get(nid) or n10[nid])
        if nid in en_npc:
            row["name"] = en_npc[nid]
        npc_patch.append(insert_statement("npcs", npc_cols, [row_value(row, c) for c in npc_cols]))
    stats["npcs"] = len(npc_ids)

    # NPC linked rows
    if npc_ids:
        marks = ",".join("?" * len(npc_ids))
        for table, key in (
            ("npc_initial_buffs", ("npc_id", "buff_id")),
            ("npc_postures", None),
            ("npc_posture_sets", None),
            ("npc_nicknames", None),
            ("npc_ai_params", None),
            ("ai_files", None),
        ):
            if table not in t30 and table not in t30s:
                continue
            src = c30 if table in t30 else c30s
            if table == "npc_initial_buffs":
                npc_patch += copy_rows(
                    c12, src, table, "npc_id IN (%s)" % marks, npc_ids, natural_key=key
                )
            elif table == "npc_postures":
                sets = {r[0] for r in rows(c30, "SELECT DISTINCT npc_posture_set_id FROM npcs WHERE id IN (%s)" % marks, *npc_ids) if r[0]}
                if sets:
                    sm = ",".join("?" * len(sets))
                    npc_patch += copy_rows(c12, c30, table, "npc_posture_set_id IN (%s)" % sm, sorted(sets))
            elif table == "npc_posture_sets":
                sets = {r[0] for r in rows(c30, "SELECT DISTINCT npc_posture_set_id FROM npcs WHERE id IN (%s)" % marks, *npc_ids) if r[0]}
                known = {r[0] for r in rows(c12, "SELECT id FROM npc_posture_sets")}
                missing = sorted(sets - known)
                if missing:
                    sm = ",".join("?" * len(missing))
                    npc_patch += copy_rows(c12, c30, table, "id IN (%s)" % sm, missing)
            elif table == "npc_nicknames":
                used = {r[0] for r in rows(c30, "SELECT DISTINCT npc_nickname_id FROM npcs WHERE id IN (%s)" % marks, *npc_ids) if r[0]}
                known = {r[0] for r in rows(c12, "SELECT id FROM npc_nicknames")}
                missing = sorted(used - known)
                if missing:
                    sm = ",".join("?" * len(missing))
                    npc_patch += copy_rows(c12, c30, table, "id IN (%s)" % sm, missing)
            elif table == "npc_ai_params":
                ids = {r[0] for r in rows(c30, "SELECT DISTINCT npc_ai_param_id FROM npcs WHERE id IN (%s)" % marks, *npc_ids) if r[0]}
                if ids:
                    sm = ",".join("?" * len(ids))
                    npc_patch += copy_rows(c12, c30s, table, "id IN (%s)" % sm, sorted(ids))
            else:
                ids = {r[0] for r in rows(c30, "SELECT DISTINCT ai_file_id FROM npcs WHERE id IN (%s)" % marks, *npc_ids) if r[0]}
                if ids:
                    sm = ",".join("?" * len(ids))
                    npc_patch += copy_rows(c12, c30, table, "id IN (%s)" % sm, sorted(ids))
    # Spawner templates absent from 1.2 (10.x placement, 3.0 has no rows for those ids).
    sp12 = {r[0] for r in rows(c12, "SELECT id FROM npc_spawners")}
    sp_cols = common_columns(c12, c10, "npc_spawners")
    snp_cols = common_columns(c12, c10, "npc_spawner_npcs")
    for tid in templates:
        if tid in sp12:
            continue
        if tid not in {r[0] for r in rows(c10, "SELECT id FROM npc_spawners")}:
            skipped["spawner template not in 10.x %s" % tid] += 1
            continue
        row = dict(zip(columns(c10, "npc_spawners"), rows(c10, "SELECT * FROM npc_spawners WHERE id=?", tid)[0]))
        npc_patch.append(insert_statement("npc_spawners", sp_cols, [row_value(row, c) for c in sp_cols]))
        for r in rows(c10, "SELECT * FROM npc_spawner_npcs WHERE npc_spawner_id=? ORDER BY id", tid):
            d = dict(zip(columns(c10, "npc_spawner_npcs"), r))
            npc_patch.append(insert_statement("npc_spawner_npcs", snp_cols, [row_value(d, c) for c in snp_cols]))
    stats["spawner_templates_imported"] = sum(
        1 for t in templates if t not in sp12 and t in {r[0] for r in rows(c10, "SELECT id FROM npc_spawners")}
    )

    # ----- item patch ---------------------------------------------------------------------
    item_patch = []
    item_cols = common_columns(c12, c30, "items")
    en_item = localized(c30, "items", "name", sorted(item_ids))
    icon_ids = set()
    for iid in sorted(item_ids):
        row = dict(i30.get(iid) or i10[iid])
        if iid in en_item:
            row["name"] = en_item[iid]
        row["id"] = item_map[iid]
        item_patch.append(insert_statement("items", item_cols, [row_value(row, c) for c in item_cols]))
        icon_ids.add(row.get("icon_id"))
    stats["items"] = len(item_ids)
    if icon_ids:
        icols = common_columns(c12, c30, "icons")
        for icon in sorted(i for i in icon_ids if i):
            src = rows(c30, "SELECT * FROM icons WHERE id=?", icon)
            if not src:
                continue
            if icon in {r[0] for r in rows(c12, "SELECT id FROM icons")}:
                continue
            d = dict(zip(columns(c30, "icons"), src[0]))
            item_patch.append(insert_statement("icons", icols, [row_value(d, c) for c in icols]))

    # ----- doodad patch (templates only; main_world has no doodad placements in the pak) ---
    doodad_patch = []
    dcols = common_columns(c12, c30, "doodad_almighties")
    for did in sorted(doodad_ids):
        if did in d12:
            continue
        row = dict(d30.get(did) or d10.get(did) or {})
        if not row:
            skipped["doodad template missing %s" % did] += 1
            continue
        row["id"] = doodad_map[did]
        doodad_patch.append(insert_statement("doodad_almighties", dcols, [row_value(row, c) for c in dcols]))
    stats["doodads"] = len(doodad_patch)

    # ----- quest patch --------------------------------------------------------------------
    quest_patch = []
    qcols = common_columns(c12, c30, "quest_contexts")
    for qid in qids:
        row = dict(zip(columns(c30, "quest_contexts"), rows(c30, "SELECT * FROM quest_contexts WHERE id=?", qid)[0]))
        row["id"] = quest_map[qid]
        # zone_id is the DB zone id (227 = e_sunny_wilderness_4 in 3.0)
        quest_patch.append(insert_statement("quest_contexts", qcols, [row_value(row, c) for c in qcols]))

    ccols = common_columns(c12, c30, "quest_components")
    for c in comps:
        row = dict(zip(columns(c30, "quest_components"), c))
        row["id"] = comp_map[row["id"]]
        row["quest_context_id"] = quest_map[row["quest_context_id"]]
        if row.get("next_component") in comp_map:
            row["next_component"] = comp_map[row["next_component"]]
        quest_patch.append(insert_statement("quest_components", ccols, [row_value(row, c) for c in ccols]))

    acols = common_columns(c12, c30, "quest_acts")
    for a in acts:
        row = {
            "id": act_map[a[0]],
            "quest_component_id": comp_map[a[1]],
            "act_detail_id": detail_map[(a[3], a[2])],
            "act_detail_type": a[3],
        }
        quest_patch.append(insert_statement("quest_acts", acols, [row_value(row, c) for c in acols]))

    # detail rows (one table per class)
    for (cls, did), row in sorted(details.items()):
        if (cls, did) not in detail_map:
            continue
        table = detail_table_for(cls)
        dcols2 = common_columns(c12, c30, table)
        d = dict(row)
        d["id"] = detail_map[(cls, did)]
        for k, v in list(d.items()):
            if not v:
                continue
            if k in ("quest_context_id",):
                d[k] = quest_map.get(v, v)
            elif k in ("quest_component_id", "component_id", "next_component", "complete_component"):
                d[k] = comp_map.get(v, v)
            elif k in ("quest_id",):
                d[k] = quest_map.get(v, v)
            elif k in ("item_id",) or k.endswith("_item_id"):
                d[k] = map_item(v)
            elif k in ("doodad_id",):
                d[k] = map_doodad(v)
            elif k == "item_group_id" or k == "quest_item_group_id":
                d[k] = ig_map.get(v, v)
            elif k == "quest_monster_group_id" or k == "monster_group_id":
                d[k] = mg_map.get(v, v)
            elif k == "npc_group_id":
                d[k] = mg_map.get(v, v)
            elif k == "sphere_id":
                d[k] = sphere_map.get(v, v)
        quest_patch.append(insert_statement(table, dcols2, [row_value(d, c) for c in dcols2]))

    # item groups / monster groups
    for gid in sorted(req_igroups):
        members_ok = sorted(i for i in q_item_groups.get(gid, set()) if item_ok(i))
        if not members_ok:
            continue
        new_g = ig_map[gid]
        if gid not in {r[0] for r in rows(c12, "SELECT id FROM quest_item_groups")}:
            src = rows(c30, "SELECT * FROM quest_item_groups WHERE id=?", gid)
            if src:
                d = dict(zip(columns(c30, "quest_item_groups"), src[0]))
                gcols = common_columns(c12, c30, "quest_item_groups")
                d["id"] = new_g
                quest_patch.append(insert_statement("quest_item_groups", gcols, [row_value(d, c) for c in gcols]))
        mcols = common_columns(c12, c30, "quest_item_group_items")
        for iid in members_ok:
            d = {"id": None, "quest_item_group_id": new_g, "item_id": map_item(iid)}
            # member ids are not referenced; a free synthetic id keeps the row idempotent
            d["id"] = detail_alloc.get(("item-group", gid, iid))
            quest_patch.append(insert_statement("quest_item_group_items", mcols, [row_value(d, c) for c in mcols]))

    for gid in sorted(req_mgroups):
        members_ok = sorted(n for n in q_monster_groups.get(gid, set()) if npc_ok(n))
        if not members_ok:
            continue
        new_g = mg_map[gid]
        if gid not in {r[0] for r in rows(c12, "SELECT id FROM quest_monster_groups")}:
            src = rows(c30, "SELECT * FROM quest_monster_groups WHERE id=?", gid)
            if src:
                d = dict(zip(columns(c30, "quest_monster_groups"), src[0]))
                gcols = common_columns(c12, c30, "quest_monster_groups")
                d["id"] = new_g
                quest_patch.append(insert_statement("quest_monster_groups", gcols, [row_value(d, c) for c in gcols]))
        mcols = common_columns(c12, c30, "quest_monster_npcs")
        for nid in members_ok:
            d = {
                "id": detail_alloc.get(("monster-group", gid, nid)),
                "quest_monster_group_id": new_g,
                "npc_id": nid,
            }
            quest_patch.append(insert_statement("quest_monster_npcs", mcols, [row_value(d, c) for c in mcols]))

    # spheres used by acts
    missing_spheres = [s for s in req_spheres if s not in {r[0] for r in rows(c12, "SELECT id FROM spheres")}]
    if missing_spheres:
        scols = common_columns(c12, c30, "spheres")
        # 1.2 has these three columns, 3.0 does not; the loader reads milestone_id without a
        # NULL default, so fill the 1.2-only columns explicitly.
        for col, default in (("milestone_id", 0), ("name_tr", 0), ("team_msg_tr", 0)):
            if col in columns(c12, "spheres") and col not in scols:
                scols.append(col)
        for sid in sorted(missing_spheres):
            src = rows(c30, "SELECT * FROM spheres WHERE id=?", sid)
            if not src:
                continue
            d = dict(zip(columns(c30, "spheres"), src[0]))
            d["id"] = sphere_map[sid]
            # 1.2 reads these with a NULL-less getter; 3.0 leaves some of them NULL.
            for col in ("milestone_id", "trigger_condition_time", "name_tr", "team_msg_tr"):
                if d.get(col) is None:
                    d[col] = 0
            for col in ("name", "team_msg", "sphere_detail_type"):
                if d.get(col) is None:
                    d[col] = ""
            quest_patch.append(insert_statement("spheres", scols, [row_value(d, c) for c in scols]))
    sqcols = common_columns(c12, c30, "sphere_quests")
    if "sphere_quests" in t30:
        for r in rows(c30, "SELECT * FROM sphere_quests WHERE quest_id IN (%s)" % ",".join("?" * len(qids)), *qids):
            d = dict(zip(columns(c30, "sphere_quests"), r))
            if d["quest_id"] not in quest_map:
                continue
            d["quest_id"] = quest_map[d["quest_id"]]
            d["id"] = detail_alloc.get(("sphere-quest", d["id"]))
            quest_patch.append(insert_statement("sphere_quests", sqcols, [row_value(d, c) for c in sqcols]))

    # unit requirements (quest / component owners only)
    if "unit_reqs" in t30:
        ucols = common_columns(c12, c30, "unit_reqs")
        if "id" not in ucols:
            # 1.2 has an id column, 3.0 does not: add the allocated one explicitly.
            ucols.insert(0, "id")
        for r in rows(c30, "SELECT * FROM unit_reqs WHERE owner_type IN ('QuestContext','QuestComponent')"):
            d = dict(zip(columns(c30, "unit_reqs"), r))
            if d["owner_type"] == "QuestContext":
                if d["owner_id"] not in quest_map:
                    continue
                d["owner_id"] = quest_map[d["owner_id"]]
            else:
                if d["owner_id"] not in comp_map:
                    continue
                d["owner_id"] = comp_map[d["owner_id"]]
            # value1 of quest-completion requirements is a quest id
            if d.get("kind_id") in (0x21, 0x24, 0x25, 0x48, 0x49) and d.get("value1") in quest_map:
                d["value1"] = quest_map[d["value1"]]
            # 3.0 dropped the id column; allocate one from the 1.2 free range.
            d["id"] = req_alloc.get(
                (
                    "req",
                    d["owner_type"],
                    d["owner_id"],
                    d["kind_id"],
                    d["value1"],
                    d["value2"],
                )
            )
            quest_patch.append(insert_statement("unit_reqs", ucols, [row_value(d, c) for c in ucols]))

    # starter items
    if "item_accept_quests" in t30:
        icols2 = common_columns(c12, c30, "item_accept_quests")
        id_col = "id" in columns(c12, "item_accept_quests")
        if id_col:
            icols2 = ["id"] + [c for c in icols2 if c != "id"]
        for r in rows(c30, "SELECT * FROM item_accept_quests WHERE quest_id IN (%s)" % ",".join("?" * len(qids)), *qids):
            d = dict(zip(columns(c30, "item_accept_quests"), r))
            if d["quest_id"] not in quest_map:
                continue
            if d["item_id"] not in item_map and d["item_id"] not in i12:
                skipped["starter item missing %s" % d["item_id"]] += 1
                continue
            d["quest_id"] = quest_map[d["quest_id"]]
            d["item_id"] = map_item(d["item_id"])
            if id_col:
                d["id"] = req_alloc.get(("starter", d["item_id"]))
            quest_patch.append(insert_statement("item_accept_quests", icols2, [row_value(d, c) for c in icols2], natural_key=("item_id",)))

    # ----- loot patch (new NPCs only) -----------------------------------------------------
    loot_patch = []
    loot_src = c30s if "loot_pack_dropping_npcs" in t30s else c10
    if "loot_pack_dropping_npcs" in ({r[0] for r in rows(loot_src, "SELECT name FROM sqlite_master WHERE type='table'")}):
        links = []
        for nid in sorted(npc_ids):
            links += [dict(zip(columns(loot_src, "loot_pack_dropping_npcs"), r)) for r in rows(loot_src, "SELECT * FROM loot_pack_dropping_npcs WHERE npc_id=?", nid)]
        packs = sorted({d["loot_pack_id"] for d in links if d.get("loot_pack_id")})
        pack_map = {p: loot_pack_alloc.get(p) for p in packs}
        # loots + loot_groups of those packs
        loot_item_ids = set()
        for p in packs:
            loot_item_ids |= {r[0] for r in rows(loot_src, "SELECT item_id FROM loots WHERE loot_pack_id=?", p)}
        loot_item_ids = {i for i in loot_item_ids if i and i in i12 or i in item_map}
        if packs:
            marks = ",".join("?" * len(packs))
            lcols = common_columns(c12, loot_src, "loots")
            for r in rows(loot_src, "SELECT * FROM loots WHERE loot_pack_id IN (%s) ORDER BY id" % marks, *packs):
                d = dict(zip(columns(loot_src, "loots"), r))
                if d["item_id"] not in i12 and d["item_id"] not in item_map:
                    skipped["loot item missing %s" % d["item_id"]] += 1
                    continue
                d["item_id"] = map_item(d["item_id"])
                d["loot_pack_id"] = pack_map[d["loot_pack_id"]]
                d["id"] = loot_alloc.get(d["id"])
                loot_patch.append(insert_statement("loots", lcols, [row_value(d, c) for c in lcols]))
            gcols = common_columns(c12, loot_src, "loot_groups")
            for r in rows(loot_src, "SELECT * FROM loot_groups WHERE pack_id IN (%s) ORDER BY id" % marks, *packs):
                d = dict(zip(columns(loot_src, "loot_groups"), r))
                d["pack_id"] = pack_map[d["pack_id"]]
                d["id"] = lg_alloc.get(d["id"])
                loot_patch.append(insert_statement("loot_groups", gcols, [row_value(d, c) for c in gcols]))
        lkcols = common_columns(c12, loot_src, "loot_pack_dropping_npcs")
        for d in links:
            if not d.get("loot_pack_id"):
                continue
            d = dict(d)
            d["loot_pack_id"] = pack_map[d["loot_pack_id"]]
            if d.get("id") is not None:
                d["id"] = loot_alloc.get(("npc-link", d["npc_id"]))
            loot_patch.append(insert_statement("loot_pack_dropping_npcs", lkcols, [row_value(d, c) for c in lkcols]))
        stats["loot_packs"] = len(packs)
        stats["loot_rows"] = len(loot_patch)
    else:
        skipped["loot tables missing in every source"] += 1

    # ----- output -------------------------------------------------------------------------
    spawn_doc = [
        {
            "UnitId": e["unit_id"],
            "Title": e["title"],
            "SpawnerId": e["spawner_id"],
            "Position": {"X": e["x"], "Y": e["y"], "Z": e["z"], "Yaw": e["yaw"]},
        }
        for e in spawn_entries
    ]

    patches = {
        "zones": zone_patch,
        "npcs": npc_patch,
        "items": item_patch,
        "quests": quest_patch,
        "loot": loot_patch,
        "doodads": doodad_patch,
    }
    headers = {
        "zones": "Sunbite Wilds zones (zone_key 286/287)",
        "npcs": "Sunbite Wilds NPC templates, spawner templates and placements",
        "items": "Sunbite Wilds item templates referenced by quests/loot",
        "quests": "Sunbite Wilds quests (level <= %d)" % PLAYER_LEVEL_CAP,
        "loot": "Sunbite Wilds loot of the newly imported NPCs",
        "doodads": "Sunbite Wilds doodad templates referenced by quests",
    }
    summary = {
        "spawn_blocks": stats["spawn_blocks"],
        "spawn_entries": stats["spawn_entries"],
        "spawn_z_terrain": stats["spawn_z_terrain"],
        "spawn_zone_adjusted": stats["spawn_zone_adjusted"],
        "spawner_templates": len(templates),
        "spawner_templates_imported": stats["spawner_templates_imported"],
        "npcs": len(npc_ids),
        "items": len(item_ids),
        "doodads": stats["doodads"],
        "quests": len(qids),
        "components": len(comps),
        "acts": len(acts),
        "loot_packs": stats.get("loot_packs", 0),
        "skipped": dict(skipped),
        "patches": {k: len(v) for k, v in patches.items()},
    }
    return patches, headers, spawn_doc, summary


def render_patch(name, header, statements, summary):
    lines = [
        "-- %s" % header,
        "-- Generated by rust/tools/area_package/build_sunbite.py (package %s)." % PACKAGE,
        "-- Do not edit by hand: change the tool and regenerate. Idempotent `INSERT OR IGNORE`",
        "-- / `NOT EXISTS` rows; no 1.2 row is replaced. Counts: %s" % json.dumps(summary, sort_keys=True),
    ]
    lines += statements
    return "\n".join(lines) + "\n"


def atomic_write(path, text):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8", newline="\n") as f:
        f.write(text)
    os.replace(tmp, path)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--db12", required=True)
    ap.add_argument("--db30", required=True)
    ap.add_argument("--db30s", required=True)
    ap.add_argument("--db10", required=True)
    ap.add_argument("--pak10", required=True)
    ap.add_argument("--w10", required=True)
    ap.add_argument("--w12", required=True)
    ap.add_argument("--heights")
    ap.add_argument("--patches-dir", default=os.path.join(REPO, "SQL", "patches", "compact"))
    ap.add_argument("--spawn-out", default=os.path.join(REPO, "rust", "crates", "aaemu-game", "app", "Data", "Areas", PACKAGE, "npc_spawns.json"))
    ap.add_argument("--check", action="store_true", help="fail when an output would change")
    args = ap.parse_args()

    patches, headers, spawn_doc, summary = build(args)
    outputs = {}
    for key, statements in patches.items():
        if not statements:
            continue
        outputs[os.path.join(args.patches_dir, PATCH_NAMES[key])] = render_patch(
            PATCH_NAMES[key], headers[key], statements, summary
        )
    spawn_text = json.dumps(spawn_doc, ensure_ascii=False, indent=2) + "\n"
    outputs[args.spawn_out] = spawn_text

    changed = []
    for path, text in outputs.items():
        old = open(path, "rb").read() if os.path.exists(path) else None
        if old != text.encode("utf-8"):
            changed.append(path)
            if not args.check:
                atomic_write(path, text)
    print(json.dumps({"summary": summary, "changed": changed, "check": args.check}, indent=2, ensure_ascii=False))
    if args.check and changed:
        raise SystemExit(1)


if __name__ == "__main__":
    main()




