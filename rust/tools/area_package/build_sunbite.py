#!/usr/bin/env python3
"""Build the optional `sunbite-wilds` area package from the 3.0.4.2 client.

The 1.2 client contains dormant Sunbite Wilds 3+4 terrain, but the 3.0 client re-terraformed
those zones and moved them; the 3.0 `map_data/npc_map/e_sunny_wilderness.dat` is the retail
NPC placement file (record: `<u32 npc id><f32 x><f32 y><f32 z>`).

The package contains:

* zones 242 (`e_sunny_wilderness_3`, key 286) and 227 (`e_sunny_wilderness_4`, key 287);
* 3.0 NPCs from the npc_map (all placements in 286/287 plus every quest accept/report NPC in
  the surrounding 1.2 zones), with 3.0 stats. A colliding 1.2 NPC id with a different name /
  level is remapped into a free range; 1.2 rows are never overwritten;
* 3.0 quests at or below level 55 whose start/end NPCs are placed (others are skipped);
* 3.0 items / doodad templates used by those quests, filtered to level <= 62 and no new item
  id >= 8,000,000;
* a terrain overlay: 3.0 heightmap/object/vegetation cells for the six 286/287 cells and a
  merged `world.xml` that replaces only the two zone nodes. The pak is generated locally
  (`--build-overlay`), never committed;
* an overlay manifest with the cell/file list and per-cell 3.0-vs-1.2 height differences.

Inputs (nothing is downloaded):

    --pak30 3.0.4.2 `game_pak`
    --pak12 1.2 `game_pak` (terrain comparison and giver-zone heights)
    --db12 / --db30 / --db30s  (1.2 target, 3.0 compact, 3.0 server tables)

`--overlay-dir` is a scratch source directory (merged world.xml + 3.0 cells + the 1.2
heightmaps of the giver cells). The tool writes `query.json` there; generate `heights.json`
with the ignored `dump_terrain_heights` test:

    AAEMU_GAME_PAK=<overlay-dir> AAEMU_HEIGHT_QUERY=<overlay-dir>/query.json \
    AAEMU_HEIGHT_OUT=<overlay-dir>/heights.json \
    cargo test -p aaemu-game dump_terrain_heights -- --ignored

Then rerun the tool with `--heights <overlay-dir>/heights.json`. The tool validates every
placement (on-ground within 3 m, water, slope) and snaps the final Z to the sampled terrain.
"""
import argparse
import collections
import hashlib
import json
import math
import os
import re
import sqlite3
import struct
import sys
import xml.etree.ElementTree as ET

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
sys.path.insert(0, HERE)
import client_pak  # noqa: E402

PACKAGE = "sunbite-wilds"
ZONE_ROWS = [
    (242, "e_sunny_wilderness_3", 286, 13, 149, 5),
    (227, "e_sunny_wilderness_4", 287, 13, 149, 5),
]
ZONE_IDS = {286: 242, 287: 227}
MERGE_ZONES = (157, 190, 286, 287)
QUEST_ZONE_IDS = [227, 242]
WORLD = "main_world"
ITEM_LEVEL_CAP = 62
PLAYER_LEVEL_CAP = 55
ITEM_ID_CAP = 8_000_000
NPC_ID_BASE = 30_000_000
ON_GROUND_TOL = 3.0
OCEAN_LEVEL = 100.0

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
    """Idempotent insert: every row is guarded by its id (or the caller's natural key)."""
    if _TARGET is not None:
        values = [v if v is not None else _null_default(table, c) for c, v in zip(cols, values)]
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
    if col in row:
        return row[col]
    lower = col.lower()
    for key, value in row.items():
        if key.lower() == lower:
            return value
    return None


def all_columns(target, source, table):
    """Every target column; target-only columns get a type default in `insert_statement`."""
    return columns(target, table)


def copy_rows(target, source, table, where, args, remap=None, natural_key=None,
              source_table=None):
    source_table = source_table or table
    cols = all_columns(target, source, table)
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
        if key in self.map:
            return self.map[key]
        if not exists(key):
            self.map[key] = key
            return key
        return self.get(key)


# ---------------------------------------------------------------------------------------------
# Client pak / world.xml / npc_map
# ---------------------------------------------------------------------------------------------
def zone_block(text, key):
    m = re.search(r'<Zone\b[^>]*\bid="%d"[^>]*>.*?</Zone>' % key, text, re.S)
    if not m:
        raise SystemExit("world.xml has no Zone id=%s" % key)
    return m.group(0)


def merged_world_xml(pak12, pak30):
    base = pak12.read("game/worlds/main_world/world.xml").decode("utf-8-sig")
    donor = pak30.read("game/worlds/main_world/world.xml").decode("utf-8-sig")
    merged = base
    for key in MERGE_ZONES:
        merged = merged.replace(zone_block(base, key), zone_block(donor, key))
    ET.fromstring(merged.encode("utf-8"))
    return merged


def sector_owners(xml_text):
    root = ET.fromstring(xml_text.encode("utf-8"))
    owners = {}
    for zone in root.iter("Zone"):
        key = int(zone.attrib["id"])
        for cell in zone.iter("cell"):
            cx, cy = int(cell.attrib["x"]), int(cell.attrib["y"])
            for sector in cell.iter("sector"):
                owners[
                    (int(sector.attrib["x"]) + cx * 16, int(sector.attrib["y"]) + cy * 16)
                ] = key
    return owners


def zone_cells(xml_text, key, owned_only):
    root = ET.fromstring(xml_text.encode("utf-8"))
    for zone in root.iter("Zone"):
        if int(zone.attrib["id"]) != key:
            continue
        cells = []
        for cell in zone.iter("cell"):
            if owned_only and not list(cell.iter("sector")):
                continue
            cells.append((int(cell.attrib["x"]), int(cell.attrib["y"])))
        return sorted(cells)
    return []


def npc_map_records(pak):
    data = pak.read("game/worlds/main_world/map_data/npc_map/e_sunny_wilderness.dat")
    if len(data) % 16:
        raise SystemExit("npc_map size %d is not a multiple of 16" % len(data))
    return [struct.unpack_from("<Ifff", data, i * 16) for i in range(len(data) // 16)]


def overlay_cells(xml30):
    cells = set()
    for key in ZONE_IDS:
        cells.update(zone_cells(xml30, key, owned_only=True))
    return sorted(cells)


def cell_rel(cell):
    return "game/worlds/main_world/cells/%03d_%03d" % cell


def overlay_files(pak30, cells):
    """Every 3.0 file under the six cell directories (terrain, objects, vegetation, ...)."""
    out = []
    prefixes = tuple(cell_rel(c).lower() + "/" for c in cells)
    for name, _off, _size in pak30.entries:
        if name.lower().startswith(prefixes):
            out.append(name)
    return sorted(out)


def build_overlay_source(pak30, pak12, out_dir, xml_merged, cells, giver_cells):
    """Scratch source dir for the server height sampler (3.0 cells + 1.2 giver cells)."""
    world_dir = os.path.join(out_dir, "game", "worlds", "main_world")
    os.makedirs(world_dir, exist_ok=True)
    with open(os.path.join(world_dir, "world.xml"), "w", encoding="utf-8", newline="\n") as f:
        f.write(xml_merged)
    for cell in cells:
        prefix = cell_rel(cell)
        for name, _off, _size in pak30.entries:
            if not name.lower().startswith(prefix.lower() + "/"):
                continue
            rel = name[len("game/"):] if name.lower().startswith("game/") else name
            dst = os.path.join(out_dir, "game", rel)
            os.makedirs(os.path.dirname(dst), exist_ok=True)
            with open(dst, "wb") as f:
                f.write(pak30.read(name))
    for cell in giver_cells:
        rel = "%s/client/terrain/heightmap.dat" % cell_rel(cell)
        dst = os.path.join(out_dir, "game", rel[len("game/"):])
        os.makedirs(os.path.dirname(dst), exist_ok=True)
        with open(dst, "wb") as f:
            f.write(pak12.read(rel))


def build_overlay_pak(pak30, path, xml_merged, cells):
    files = [("game/worlds/main_world/world.xml", xml_merged.encode("utf-8"))]
    for name in overlay_files(pak30, cells):
        files.append((name, pak30.read(name)))
    return client_pak.write_pak(path, files), files


# ---------------------------------------------------------------------------------------------
# Terrain / placement validation
# ---------------------------------------------------------------------------------------------
def height_query(records, owners, givers):
    """Query ids: `<index>` for the placement and `<index>:0..3` for the +/-3 m slope samples."""
    out = []
    selected = []
    for i, (npc, x, y, z) in enumerate(records):
        zone = owners.get((int(x // 64), int(y // 64)), 0)
        if zone not in ZONE_IDS and npc not in givers:
            continue
        selected.append((i, npc, x, y, z, zone))
        out.append({"id": str(i), "world": WORLD, "x": x, "y": y})
        for k, (dx, dy) in enumerate(((3, 0), (-3, 0), (0, 3), (0, -3))):
            out.append({"id": f"{i}:{k}", "world": WORLD, "x": x + dx, "y": y + dy})
    return selected, out


def nearest_sector(owners, x, y):
    best = None
    best_d = None
    for (sx, sy), key in owners.items():
        d = (sx * 64 + 32 - x) ** 2 + (sy * 64 + 32 - y) ** 2
        if best_d is None or d < best_d:
            best_d = d
            best = (sx, sy, key)
    return best


def analyze_placements(selected, heights, owners, givers):
    """Final placements with a terrain-snapped Z and the validation counters."""
    stats = collections.Counter()
    placements = []
    for i, npc, x, y, z, zone in selected:
        h = heights.get(str(i), {})
        if "h" not in h:
            stats["missing_height"] += 1
            continue
        hz = h["h"]
        slope_samples = []
        for k in range(4):
            s = heights.get(f"{i}:{k}", {})
            if "h" in s:
                slope_samples.append(abs(s["h"] - hz))
        slope = math.degrees(math.atan(max(slope_samples) / 3.0)) if slope_samples else 0.0
        if abs(z - hz) <= ON_GROUND_TOL:
            stats["authored_on_ground"] += 1
        else:
            stats["snapped"] += 1
            if abs(z - hz) > 50:
                stats["snapped_large"] += 1
        if hz < OCEAN_LEVEL:
            stats["water"] += 1
        if slope > 45:
            stats["steep"] += 1
        # A 3.0 placement in a sector the merged 1.2 map does not own (mostly quest givers in
        # the surrounding zones): move it to the nearest owned sector and keep the local offset.
        if zone == 0:
            sx, sy, _ = nearest_sector(owners, x, y)
            x = sx * 64 + (x % 64)
            y = sy * 64 + (y % 64)
            zone = owners[(sx, sy)]
            h2 = heights.get(f"g:{i}", {})
            if "h" in h2:
                hz = h2["h"]
            stats["reanchored"] += 1
        placements.append(
            {
                "npc": npc,
                "x": round(x, 3),
                "y": round(y, 3),
                "z": round(hz, 3),
                "zone": zone,
                "slope": round(slope, 1),
                "authored_z": round(z, 3),
            }
        )
    return placements, stats


def giver_query(selected, owners):
    """Extra query points for the nearest-sector centres of the zone-0 giver placements."""
    out = []
    for i, _npc, x, y, _z, zone in selected:
        if zone == 0:
            sx, sy, _ = nearest_sector(owners, x, y)
            out.append({"id": f"g:{i}", "world": WORLD, "x": sx * 64 + 32, "y": sy * 64 + 32})
    return out


# ---------------------------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------------------------
def build(args):
    global _TARGET
    c12, c30, c30s = (connect_ro(p) for p in (args.db12, args.db30, args.db30s))
    _TARGET = c12
    pak30, pak12 = client_pak.Pak(args.pak30), client_pak.Pak(args.pak12)
    stats = collections.Counter()
    skipped = collections.Counter()

    xml12 = pak12.read("game/worlds/main_world/world.xml").decode("utf-8-sig")
    xml30 = pak30.read("game/worlds/main_world/world.xml").decode("utf-8-sig")
    xml_merged = merged_world_xml(pak12, pak30)
    owners = sector_owners(xml_merged)
    cells = overlay_cells(xml30)
    # 1.2 heightmaps for the surrounding Sunbite cells (quest givers are placed there when the
    # merged 1.2 map owns their sector).
    giver_cells = [(x, y) for x in (18, 19, 20) for y in (4, 5, 6)]

    records = npc_map_records(pak30)

    # ----- quest givers (for placement selection) -----------------------------------------
    qids, comps, acts = quest_rows(c30)
    details = detail_rows(c30, acts)
    comp_quest = {
        dict(zip(columns(c30, "quest_components"), c))["id"]:
        dict(zip(columns(c30, "quest_components"), c))["quest_context_id"]
        for c in comps
    }
    givers = quest_givers(c30, comps, acts)
    quest_refs = set(givers)
    for (cls, did), row in details.items():
        for key, value in row.items():
            if not value:
                continue
            if key == "npc_id" or key.endswith("_npc_id"):
                quest_refs.add(value)
    selected, query = height_query(records, owners, quest_refs)
    # Re-anchored zone-0 givers need a second height sample at the nearest sector centre.
    query += giver_query(selected, owners)
    build_overlay_source(pak30, pak12, args.overlay_dir, xml_merged, cells, giver_cells)
    os.makedirs(args.overlay_dir, exist_ok=True)
    with open(os.path.join(args.overlay_dir, "query.json"), "w", encoding="utf-8") as f:
        json.dump(query, f)
    if not args.heights:
        print(
            "wrote %s/query.json; run the documented dump_terrain_heights test and rerun with "
            "--heights %s/heights.json" % (args.overlay_dir, args.overlay_dir)
        )
        return None
    with open(args.heights, encoding="utf-8") as f:
        heights = {r["id"]: r for r in json.load(f)}

    placements, placement_stats = analyze_placements(selected, heights, owners, givers)
    stats.update(placement_stats)
    stats["placements"] = len(placements)
    stats["placement_npcs"] = len({p["npc"] for p in placements})
    stats["overlay_cells"] = len(cells)
    stats["overlay_files"] = len(overlay_files(pak30, cells))
    stats["overlay_source_bytes"] = sum(
        pak30.by_name[n.lower()][1] for n in overlay_files(pak30, cells)
    ) + len(xml_merged.encode("utf-8"))

    # ----- ID maps: 3.0 NPC id -> package id ----------------------------------------------
    n12 = {r[0]: dict(zip(columns(c12, "npcs"), r)) for r in rows(c12, "SELECT * FROM npcs")}
    n30 = {r[0]: dict(zip(columns(c30, "npcs"), r)) for r in rows(c30, "SELECT * FROM npcs")}
    m12 = {r[0] for r in rows(c12, "SELECT id FROM models")}
    npc_alloc = Ids(NPC_ID_BASE)
    npc_map = {}
    npc_rows = {}
    fallback_models = []
    for npc in sorted({p["npc"] for p in placements}):
        row = dict(n30.get(npc) or {})
        if not row:
            skipped["npc absent in 3.0 %s" % npc] += 1
            continue
        old = n12.get(npc)
        different = old is not None and (
            old.get("name") != row.get("name")
            or old.get("level", old.get("LEVEL")) != row.get("level", row.get("LEVEL"))
        )
        if old is not None and not different:
            npc_map[npc] = npc
            continue
        new_id = npc_alloc.get(npc) if (old is not None or different) else npc
        if old is None and npc not in n12:
            new_id = npc
        if row.get("model_id") not in m12:
            # The 1.2 client has no 3.0 model. For a colliding 1.2 id keep its old model as a
            # visible placeholder; for a 3.0-only npc (not in any quest) skip it.
            if old is not None:
                row["model_id"] = old.get("model_id")
                fallback_models.append((npc, new_id, old.get("model_id")))
            else:
                skipped["npc with model absent in 1.2 %s" % npc] += 1
                continue
        npc_map[npc] = new_id
        npc_rows[new_id] = row
    stats["npc_remapped"] = sum(1 for k, v in npc_map.items() if k != v)
    stats["npc_fallback_models"] = len(fallback_models)
    placements = [p for p in placements if p["npc"] in npc_map]
    seen = set()
    unique = []
    for p in placements:
        key = (p["npc"], round(p["x"], 1), round(p["y"], 1), round(p["z"], 1))
        if key in seen:
            continue
        seen.add(key)
        unique.append(p)
    placements = unique
    stats["placements"] = len(placements)
    stats["placement_npcs"] = len({p["npc"] for p in placements})

    # ----- zones patch --------------------------------------------------------------------
    zone_patch = [
        "-- The 1.2 zone group 13 (`e_sunny_wilderness`) is reused; only the zone rows differ.",
    ]
    zcols = ["id", "name", "zone_key", "group_id", "closed", "faction_id", "zone_climate_id"]
    en_zones = localized(c30, "zones", "display_text", [z[0] for z in ZONE_ROWS])
    for zid, name, key, group, faction, climate in ZONE_ROWS:
        zone_patch.append(
            insert_statement("zones", zcols, [zid, en_zones.get(zid, name), key, group, 0, faction, climate])
        )
    stats["zones"] = len(ZONE_ROWS)

    # ----- NPC patch ----------------------------------------------------------------------
    npc_patch = []
    npc_cols = all_columns(c12, c30, "npcs")
    for new_id, row in sorted(npc_rows.items()):
        cols = list(npc_cols)
        values = []
        for c in cols:
            values.append(new_id if c.lower() == "id" else row_value(row, c))
        npc_patch.append(insert_statement("npcs", cols, values))
    stats["npcs"] = len(npc_rows)

    # NPC linked rows (3.0 only), keyed by the old -> new id.
    if npc_rows:
        old_ids = [old for old, new in npc_map.items() if old in npc_rows.values() or True]
        # Linked tables are keyed by the source (3.0) NPC id; rewrite to the package id.
        for table, key in (
            ("npc_initial_buffs", ("npc_id", "buff_id")),
            ("npc_postures", None),
            ("npc_posture_sets", None),
            ("npc_nicknames", None),
            ("npc_ai_params", None),
            ("ai_files", None),
        ):
            if table not in {r[0] for r in rows(c30, "SELECT name FROM sqlite_master WHERE type='table'")} and table not in {
                r[0] for r in rows(c30s, "SELECT name FROM sqlite_master WHERE type='table'")
            }:
                continue
            src = c30 if table in {r[0] for r in rows(c30, "SELECT name FROM sqlite_master WHERE type='table'")} else c30s
            src_ids = sorted({old for old, new in npc_map.items() if new in npc_rows})
            if not src_ids:
                continue
            marks = ",".join("?" * len(src_ids))
            rev = {old: new for old, new in npc_map.items()}
            if key is not None:
                npc_patch += copy_rows(
                    c12, src, table, "npc_id IN (%s)" % marks, src_ids,
                    remap=lambda r, rev=rev: {**r, "npc_id": rev.get(r["npc_id"], r["npc_id"])},
                    natural_key=key,
                )
            elif table == "npc_postures":
                sets = {r[0] for r in rows(c30, "SELECT DISTINCT npc_posture_set_id FROM npcs WHERE id IN (%s)" % marks, *src_ids) if r[0]}
                if sets:
                    sm = ",".join("?" * len(sets))
                    npc_patch += copy_rows(c12, c30, table, "npc_posture_set_id IN (%s)" % sm, sorted(sets))
            elif table == "npc_posture_sets":
                sets = {r[0] for r in rows(c30, "SELECT DISTINCT npc_posture_set_id FROM npcs WHERE id IN (%s)" % marks, *src_ids) if r[0]}
                known = {r[0] for r in rows(c12, "SELECT id FROM npc_posture_sets")}
                missing = sorted(sets - known)
                if missing:
                    sm = ",".join("?" * len(missing))
                    npc_patch += copy_rows(c12, c30, table, "id IN (%s)" % sm, missing)
            elif table == "npc_nicknames":
                used = {r[0] for r in rows(c30, "SELECT DISTINCT npc_nickname_id FROM npcs WHERE id IN (%s)" % marks, *src_ids) if r[0]}
                known = {r[0] for r in rows(c12, "SELECT id FROM npc_nicknames")}
                missing = sorted(used - known)
                if missing:
                    sm = ",".join("?" * len(missing))
                    npc_patch += copy_rows(c12, c30, table, "id IN (%s)" % sm, missing)
            elif table == "npc_ai_params":
                used = {r[0] for r in rows(c30, "SELECT DISTINCT npc_ai_param_id FROM npcs WHERE id IN (%s)" % marks, *src_ids) if r[0]}
                if used:
                    sm = ",".join("?" * len(used))
                    npc_patch += copy_rows(c12, c30s, table, "id IN (%s)" % sm, sorted(used))
            elif table == "ai_files":
                used = {r[0] for r in rows(c30, "SELECT DISTINCT ai_file_id FROM npcs WHERE id IN (%s)" % marks, *src_ids) if r[0]}
                if used:
                    sm = ",".join("?" * len(used))
                    npc_patch += copy_rows(c12, c30, table, "id IN (%s)" % sm, sorted(used))

    # ----- spawn JSON ---------------------------------------------------------------------
    # Pin an existing 1.2 spawner template only when its members are exactly the 3.0 members;
    # otherwise the spawn loader generates one spawner for the placement (no template pin).
    t30 = collections.defaultdict(set)
    for sid, mid in rows(c30s, "SELECT npc_spawner_id, member_id FROM npc_spawner_npcs WHERE member_type='Npc'"):
        t30[mid].add(sid)
    t12 = collections.defaultdict(set)
    for sid, mid in rows(c12, "SELECT npc_spawner_id, member_id FROM npc_spawner_npcs WHERE member_type='Npc'"):
        t12[mid].add(sid)
    sp30 = {r[0]: set() for r in rows(c30s, "SELECT id FROM npc_spawners")}
    for sid, mid in rows(c30s, "SELECT npc_spawner_id, member_id FROM npc_spawner_npcs WHERE member_type='Npc'"):
        sp30.setdefault(sid, set()).add(mid)
    pin = {}
    for npc, new_id in npc_map.items():
        if new_id != npc:
            continue
        for sid in sorted(t30.get(npc, set()) & t12.get(npc, set())):
            m12 = {
                r[0]
                for r in rows(
                    c12,
                    "SELECT member_id FROM npc_spawner_npcs WHERE npc_spawner_id=?",
                    sid,
                )
            }
            m30 = set(sp30.get(sid, set()))
            # Pin only an unambiguous one-member template: otherwise the loader would clone
            # every game-data spawner that contains this NPC at the same point.
            if m12 == {npc} and m30 == {npc}:
                pin[npc] = sid
                break
    # Every placement is pinned. A safe existing one-member template is reused; otherwise a
    # one-member synthetic template is added so the loader cannot clone every template that
    # happens to contain the same NPC.
    synth_next = 100_000_000
    for old, new_id in sorted(npc_map.items()):
        if old in pin or new_id not in npc_rows:
            continue
        donors = sorted(t30.get(old, set()))
        donor = None
        if donors:
            donor = rows(c30s, "SELECT * FROM npc_spawners WHERE id=?", donors[0])
        if not donor:
            donor = rows(c12, "SELECT * FROM npc_spawners WHERE id=1")
        source = c30s if donors else c12
        d = dict(zip(columns(source, "npc_spawners"), donor[0])) if donor else {}
        d.update(
            {
                "id": synth_next,
                "npc_spawner_category_id": 0,
                "name": "sunbite-wilds %s" % old,
                "comment": "3.0 npc_map placement",
                "maxPopulation": 1,
                "min_population": 1,
                "startTime": 0.0,
                "endTime": 0.0,
                "destroyTime": 0.0,
                "spawn_delay_min": 0.0,
                "spawn_delay_max": 0.0,
                "activation_state": "t",
                "save_indun": "f",
                "test_radius_npc": 0.0,
                "test_radius_pc": 0.0,
                "suspend_spawn_count": 0,
            }
        )
        scols = all_columns(c12, source, "npc_spawners")
        npc_patch.append(insert_statement("npc_spawners", scols, [row_value(d, c) for c in scols]))
        mcols = all_columns(c12, source, "npc_spawner_npcs")
        m = {
            "id": synth_next,
            "npc_spawner_id": synth_next,
            "member_id": new_id,
            "member_type": "Npc",
            "weight": 1.0,
        }
        npc_patch.append(insert_statement("npc_spawner_npcs", mcols, [row_value(m, c) for c in mcols]))
        pin[old] = synth_next
        synth_next += 1
    stats["synthetic_spawners"] = synth_next - 100_000_000

    spawn_doc = []
    for p in sorted(placements, key=lambda p: (p["npc"], p["x"], p["y"])):
        entry = {
            "UnitId": npc_map[p["npc"]],
            "Title": "sunbite-wilds: 3.0 npc_map (%s)" % p["npc"],
            "Position": {
                "X": p["x"],
                "Y": p["y"],
                "Z": p["z"],
                "Yaw": 0.0,
            },
        }
        if p["npc"] in pin:
            entry["SpawnerId"] = pin[p["npc"]]
        spawn_doc.append(entry)
    stats["spawn_entries"] = len(spawn_doc)
    stats["pinned_spawners"] = len(pin)

    # ----- quests -------------------------------------------------------------------------
    placed_old = {p["npc"] for p in placements}
    accept = collections.defaultdict(set)
    report = collections.defaultdict(set)
    for aid, comp, did, cls in acts:
        table = {
            "QuestActConAcceptNpc": "quest_act_con_accept_npcs",
            "QuestActConReportNpc": "quest_act_con_report_npcs",
        }.get(cls)
        if not table:
            continue
        row = rows(c30, "SELECT * FROM %s WHERE id=?" % table, did)
        if not row:
            continue
        d = dict(zip(columns(c30, table), row[0]))
        if d.get("npc_id"):
            (accept if cls == "QuestActConAcceptNpc" else report)[comp_quest[comp]].add(
                d["npc_id"]
            )
    bad_giver = {
        q
        for q in qids
        if not (accept.get(q, set()) | report.get(q, set())).issubset(placed_old)
    }
    for q in sorted(bad_giver):
        skipped["quest giver not placed %s" % q] += 1
    qids = [q for q in qids if q not in bad_giver]

    n12_ids = set(n12)
    i12 = {r[0] for r in rows(c12, "SELECT id FROM items")}
    i30 = {
        r[0]: dict(zip(columns(c30, "items"), r))
        for r in rows(c30, "SELECT * FROM items")
    }
    d12 = {r[0] for r in rows(c12, "SELECT id FROM doodad_almighties")}
    d30 = {
        r[0]: dict(zip(columns(c30, "doodad_almighties"), r))
        for r in rows(c30, "SELECT * FROM doodad_almighties")
    }
    q_item_groups = collections.defaultdict(set)
    for r in rows(c30, "SELECT quest_item_group_id, item_id FROM quest_item_group_items"):
        q_item_groups[r[0]].add(r[1])
    q_monster_groups = collections.defaultdict(set)
    for r in rows(c30, "SELECT quest_monster_group_id, npc_id FROM quest_monster_npcs"):
        q_monster_groups[r[0]].add(r[1])

    def npc_ok(nid):
        return nid == 0 or nid in n12_ids or nid in npc_map

    def item_ok(iid):
        if iid == 0 or iid in i12:
            return True
        row = i30.get(iid)
        return row is not None and row.get("level", 0) <= ITEM_LEVEL_CAP

    def doodad_ok(did):
        return did == 0 or did in d12 or did in d30

    def refs_of(row):
        npcs, items_, doodads, ig, mg = set(), set(), set(), set(), set()
        for k, v in row.items():
            if not v:
                continue
            if k == "npc_id" or k.endswith("_npc_id"):
                npcs.add(v)
            elif k == "item_id" or k.endswith("_item_id"):
                items_.add(v)
            elif k == "doodad_id":
                doodads.add(v)
            elif k in ("item_group_id", "quest_item_group_id"):
                ig.add(v)
            elif k in ("npc_group_id", "quest_monster_group_id", "monster_group_id"):
                mg.add(v)
        return npcs, items_, doodads, ig, mg

    bad = set()
    req_items, req_doodads = set(), set()
    req_igroups, req_mgroups = set(), set()
    kept_comp = {c for c, q in comp_quest.items() if q in set(qids)}
    for aid, comp, did, cls in acts:
        if comp not in kept_comp:
            continue
        q = comp_quest[comp]
        row = details.get((cls, did))
        if row is None:
            continue
        npcs, items_, doodads, ig, mg = refs_of(row)
        req_items |= items_
        req_doodads |= doodads
        req_igroups |= ig
        req_mgroups |= mg
        if any(not npc_ok(n) for n in npcs):
            bad.add(q)
        if any(not item_ok(i) for i in items_):
            bad.add(q)
        if any(not doodad_ok(d) for d in doodads):
            bad.add(q)
        if any(not q_item_groups.get(g) or any(not item_ok(i) for i in q_item_groups[g]) for g in ig):
            bad.add(q)
        if any(
            not q_monster_groups.get(g)
            or all(not npc_ok(n) for n in q_monster_groups.get(g, set()))
            for g in mg
        ):
            bad.add(q)
    for g in req_igroups:
        req_items |= q_item_groups.get(g, set())
    for g in req_mgroups:
        # monster-group members are not imported unless they are placed/givers
        for n in q_monster_groups.get(g, set()):
            if n not in n12_ids and n not in npc_map:
                bad.add(next((q for q in qids if q in accept or q in report), qids[0]))
    for q in sorted(bad & set(qids)):
        skipped["quest with unresolved reference %s" % q] += 1
    qids = [q for q in qids if q not in bad]
    kept_comp = {c for c, q in comp_quest.items() if q in set(qids)}

    item_ids = {i for i in req_items if i not in i12 and i > 0 and i in i30}
    item_max = max(
        [r[0] for r in rows(c12, "SELECT max(id) FROM items")] + [0]
    )
    item_alloc = Ids(max(item_max, 50_000) + 1)
    item_map = {}
    for iid in sorted(item_ids):
        item_map[iid] = item_alloc.get(iid) if iid >= ITEM_ID_CAP else iid
    doodad_ids = {d for d in req_doodads if d not in d12 and d > 0 and d in d30}
    doodad_max = max([r[0] for r in rows(c12, "SELECT max(id) FROM doodad_almighties")] + [0])
    doodad_alloc = Ids(doodad_max + 1)
    doodad_map = {d: (d if d < 8_000_000 else doodad_alloc.get(d)) for d in doodad_ids}

    # ID allocation for quest rows.
    def maxid(conn, table, col="id"):
        r = rows(conn, "SELECT max(%s) FROM %s" % (col, quote_ident(table)))
        return r[0][0] or 0

    comp_alloc = Ids(maxid(c12, "quest_components") + 1)
    act_alloc = Ids(maxid(c12, "quest_acts") + 1)
    detail_max = max([maxid(c12, t) for t in CLASS_TABLE.values() if t in {r[0] for r in rows(c12, "SELECT name FROM sqlite_master WHERE type='table'")}] + [0])
    detail_alloc = Ids(detail_max + 1)
    req_alloc = Ids(maxid(c12, "unit_reqs") + 1)
    sphere_alloc = Ids(maxid(c12, "spheres") + 1)
    ig_alloc = Ids(maxid(c12, "quest_item_groups") + 1)
    mg_alloc = Ids(maxid(c12, "quest_monster_groups") + 1)
    loot_pack_alloc = Ids(maxid(c12, "loots", "loot_pack_id") + 1)
    loot_alloc = Ids(maxid(c12, "loots") + 1)
    lg_alloc = Ids(maxid(c12, "loot_groups") + 1)

    quest_map = {q: q for q in qids}
    comp_map = {c[0]: comp_alloc.get(c[0]) for c in comps if c[0] in kept_comp}
    act_map = {a[0]: act_alloc.get(a[0]) for a in acts if a[1] in kept_comp}
    detail_map = {}
    for a in acts:
        if a[1] not in kept_comp:
            continue
        cls, did = a[3], a[2]
        if (cls, did) not in detail_map:
            detail_map[(cls, did)] = detail_alloc.get((cls, did))

    def map_item(i):
        return item_map.get(i, i)

    def map_npc(n):
        return npc_map.get(n, n)

    def map_doodad(d):
        return doodad_map.get(d, d)

    ig_map = {
        g: ig_alloc.maybe(g, lambda x: any(r[0] == x for r in rows(c12, "SELECT id FROM quest_item_groups")))
        for g in req_igroups
    }
    mg_map = {
        g: mg_alloc.maybe(g, lambda x: any(r[0] == x for r in rows(c12, "SELECT id FROM quest_monster_groups")))
        for g in req_mgroups
    }
    sphere_ids = {s for s in req_igroups if False}
    # spheres referenced by detail rows are not in refs_of; collect explicitly
    sphere_refs = set()
    for aid, comp, did, cls in acts:
        if comp not in kept_comp:
            continue
        row = details.get((cls, did), {})
        if row.get("sphere_id"):
            sphere_refs.add(row["sphere_id"])
    sphere_known = {r[0] for r in rows(c12, "SELECT id FROM spheres")}
    sphere_map = {s: (s if s in sphere_known else sphere_alloc.get(s)) for s in sphere_refs}

    # ----- item patch ---------------------------------------------------------------------
    item_patch = []
    item_cols = all_columns(c12, c30, "items")
    en_item = localized(c30, "items", "name", sorted(item_ids))
    icons = set()
    for iid in sorted(item_ids):
        row = dict(i30[iid])
        if iid in en_item:
            row["name"] = en_item[iid]
        row["id"] = item_map[iid]
        item_patch.append(
            insert_statement("items", item_cols, [row_value(row, c) for c in item_cols])
        )
        icons.add(row.get("icon_id"))
    icols = all_columns(c12, c30, "icons")
    known_icons = {r[0] for r in rows(c12, "SELECT id FROM icons")}
    for icon in sorted(i for i in icons if i and i not in known_icons):
        src = rows(c30, "SELECT * FROM icons WHERE id=?", icon)
        if src:
            d = dict(zip(columns(c30, "icons"), src[0]))
            item_patch.append(insert_statement("icons", icols, [row_value(d, c) for c in icols]))
    stats["items"] = len(item_ids)

    # ----- doodad patch -------------------------------------------------------------------
    doodad_patch = []
    dcols = all_columns(c12, c30, "doodad_almighties")
    for did in sorted(doodad_ids):
        row = dict(d30[did])
        row["id"] = doodad_map[did]
        doodad_patch.append(
            insert_statement("doodad_almighties", dcols, [row_value(row, c) for c in dcols])
        )
    stats["doodads"] = len(doodad_ids)

    # ----- quest patch --------------------------------------------------------------------
    quest_patch = []
    qcols = all_columns(c12, c30, "quest_contexts")
    for qid in qids:
        row = dict(zip(columns(c30, "quest_contexts"), rows(c30, "SELECT * FROM quest_contexts WHERE id=?", qid)[0]))
        row["id"] = quest_map[qid]
        quest_patch.append(insert_statement("quest_contexts", qcols, [row_value(row, c) for c in qcols]))
    ccols = all_columns(c12, c30, "quest_components")
    for c in comps:
        cid = c[0]
        if cid not in comp_map:
            continue
        row = dict(zip(columns(c30, "quest_components"), c))
        row["id"] = comp_map[cid]
        row["quest_context_id"] = quest_map.get(row["quest_context_id"], row["quest_context_id"])
        if row.get("next_component") in comp_map:
            row["next_component"] = comp_map[row["next_component"]]
        quest_patch.append(insert_statement("quest_components", ccols, [row_value(row, c) for c in ccols]))
    acols = all_columns(c12, c30, "quest_acts")
    for a in acts:
        if a[1] not in kept_comp:
            continue
        row = {
            "id": act_map[a[0]],
            "quest_component_id": comp_map[a[1]],
            "act_detail_id": detail_map[(a[3], a[2])],
            "act_detail_type": a[3],
        }
        quest_patch.append(insert_statement("quest_acts", acols, [row_value(row, c) for c in acols]))
    for (cls, did), srcrow in sorted(details.items()):
        if (cls, did) not in detail_map:
            continue
        table = CLASS_TABLE.get(cls)
        if not table:
            continue
        d = dict(srcrow)
        d["id"] = detail_map[(cls, did)]
        for k, v in list(d.items()):
            if not v:
                continue
            if k == "quest_context_id":
                d[k] = quest_map.get(v, v)
            elif k in ("quest_component_id", "component_id", "next_component", "complete_component"):
                d[k] = comp_map.get(v, v)
            elif k == "quest_id":
                d[k] = quest_map.get(v, v)
            elif k == "item_id" or k.endswith("_item_id"):
                d[k] = map_item(v)
            elif k == "doodad_id":
                d[k] = map_doodad(v)
            elif k in ("item_group_id", "quest_item_group_id"):
                d[k] = ig_map.get(v, v)
            elif k in ("npc_group_id", "quest_monster_group_id", "monster_group_id"):
                d[k] = mg_map.get(v, v)
            elif k == "sphere_id":
                d[k] = sphere_map.get(v, v)
            elif k == "npc_id" or k.endswith("_npc_id"):
                d[k] = map_npc(v)
        dcols2 = all_columns(c12, c30, table)
        quest_patch.append(insert_statement(table, dcols2, [row_value(d, c) for c in dcols2]))

    # item groups / monster groups
    for gid in sorted(req_igroups):
        members = sorted(i for i in q_item_groups.get(gid, set()) if item_ok(i))
        if not members:
            continue
        new_g = ig_map[gid]
        if gid not in {r[0] for r in rows(c12, "SELECT id FROM quest_item_groups")}:
            src = rows(c30, "SELECT * FROM quest_item_groups WHERE id=?", gid)
            if src:
                d = dict(zip(columns(c30, "quest_item_groups"), src[0]))
                gcols = all_columns(c12, c30, "quest_item_groups")
                d["id"] = new_g
                quest_patch.append(insert_statement("quest_item_groups", gcols, [row_value(d, c) for c in gcols]))
        mcols = all_columns(c12, c30, "quest_item_group_items")
        for iid in members:
            d = {"id": detail_alloc.get(("item-group", gid, iid)), "quest_item_group_id": new_g, "item_id": map_item(iid)}
            quest_patch.append(insert_statement("quest_item_group_items", mcols, [row_value(d, c) for c in mcols]))
    for gid in sorted(req_mgroups):
        members = sorted(n for n in q_monster_groups.get(gid, set()) if npc_ok(n))
        if not members:
            continue
        new_g = mg_map[gid]
        if gid not in {r[0] for r in rows(c12, "SELECT id FROM quest_monster_groups")}:
            src = rows(c30, "SELECT * FROM quest_monster_groups WHERE id=?", gid)
            if src:
                d = dict(zip(columns(c30, "quest_monster_groups"), src[0]))
                gcols = all_columns(c12, c30, "quest_monster_groups")
                d["id"] = new_g
                quest_patch.append(insert_statement("quest_monster_groups", gcols, [row_value(d, c) for c in gcols]))
        mcols = all_columns(c12, c30, "quest_monster_npcs")
        for nid in members:
            d = {"id": detail_alloc.get(("monster-group", gid, nid)), "quest_monster_group_id": new_g, "npc_id": map_npc(nid)}
            quest_patch.append(insert_statement("quest_monster_npcs", mcols, [row_value(d, c) for c in mcols]))

    # spheres used by quest acts
    missing_spheres = [s for s in sphere_refs if s not in sphere_known]
    if missing_spheres:
        scols = all_columns(c12, c30, "spheres")
        for col, default in (("milestone_id", 0), ("name_tr", 0), ("team_msg_tr", 0)):
            if col in columns(c12, "spheres") and col not in scols:
                scols.append(col)
        for sid in sorted(missing_spheres):
            src = rows(c30, "SELECT * FROM spheres WHERE id=?", sid)
            if not src:
                continue
            d = dict(zip(columns(c30, "spheres"), src[0]))
            d["id"] = sphere_map[sid]
            for col, default in (("milestone_id", 0), ("trigger_condition_time", 0), ("name_tr", 0), ("team_msg_tr", 0)):
                if d.get(col) is None:
                    d[col] = default
            for col in ("name", "team_msg", "sphere_detail_type"):
                if d.get(col) is None:
                    d[col] = ""
            quest_patch.append(insert_statement("spheres", scols, [row_value(d, c) for c in scols]))
    sqcols = all_columns(c12, c30, "sphere_quests")
    if "sphere_quests" in {r[0] for r in rows(c30, "SELECT name FROM sqlite_master WHERE type='table'")}:
        for r in rows(c30, "SELECT * FROM sphere_quests WHERE quest_id IN (%s)" % ",".join("?" * len(qids)), *qids):
            d = dict(zip(columns(c30, "sphere_quests"), r))
            if d["quest_id"] not in quest_map:
                continue
            d["quest_id"] = quest_map[d["quest_id"]]
            d["id"] = detail_alloc.get(("sphere-quest", d["id"]))
            quest_patch.append(insert_statement("sphere_quests", sqcols, [row_value(d, c) for c in sqcols]))

    if "unit_reqs" in {r[0] for r in rows(c30, "SELECT name FROM sqlite_master WHERE type='table'")}:
        ucols = all_columns(c12, c30, "unit_reqs")
        if "id" not in ucols:
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
            if d.get("kind_id") == 0x1B:
                d["value1"] = map_npc(d["value1"])
            if d.get("kind_id") in (0x1F, 0x24, 0x25, 0x48, 0x49) and d.get("value1") in quest_map:
                d["value1"] = quest_map[d["value1"]]
            d["id"] = req_alloc.get((d["owner_type"], d["owner_id"], d["kind_id"], d["value1"], d["value2"]))
            quest_patch.append(insert_statement("unit_reqs", ucols, [row_value(d, c) for c in ucols]))

    if "item_accept_quests" in {r[0] for r in rows(c30, "SELECT name FROM sqlite_master WHERE type='table'")}:
        icols2 = all_columns(c12, c30, "item_accept_quests")
        for r in rows(c30, "SELECT * FROM item_accept_quests WHERE quest_id IN (%s)" % ",".join("?" * len(qids)), *qids):
            d = dict(zip(columns(c30, "item_accept_quests"), r))
            if d["quest_id"] not in quest_map:
                continue
            if d["item_id"] not in item_map and d["item_id"] not in i12:
                skipped["starter item missing %s" % d["item_id"]] += 1
                continue
            d["quest_id"] = quest_map[d["quest_id"]]
            d["item_id"] = map_item(d["item_id"])
            if "id" in icols2:
                d["id"] = req_alloc.get(("starter", d["item_id"]))
            quest_patch.append(insert_statement("item_accept_quests", icols2, [row_value(d, c) for c in icols2], natural_key=("item_id",)))

    # ----- loot patch (3.0 links of the newly imported NPCs) ------------------------------
    loot_patch = []
    old_by_new = {new: old for old, new in npc_map.items()}
    src_npcs = sorted({old_by_new.get(n, n) for n in npc_rows})
    if src_npcs and "loot_pack_dropping_npcs" in {r[0] for r in rows(c30s, "SELECT name FROM sqlite_master WHERE type='table'")}:
        marks = ",".join("?" * len(src_npcs))
        links = [
            dict(zip(columns(c30s, "loot_pack_dropping_npcs"), r))
            for r in rows(c30s, "SELECT * FROM loot_pack_dropping_npcs WHERE npc_id IN (%s)" % marks, *src_npcs)
        ]
        packs = sorted({d["loot_pack_id"] for d in links if d.get("loot_pack_id")})
        pack_map = {p: loot_pack_alloc.get(p) for p in packs}
        lcols = all_columns(c12, c30s, "loots")
        for p in packs:
            for r in rows(c30s, "SELECT * FROM loots WHERE loot_pack_id=? ORDER BY id", p):
                d = dict(zip(columns(c30s, "loots"), r))
                if d["item_id"] not in i12 and d["item_id"] not in item_map:
                    skipped["loot item missing %s" % d["item_id"]] += 1
                    continue
                d["item_id"] = map_item(d["item_id"])
                d["loot_pack_id"] = pack_map[p]
                d["id"] = loot_alloc.get(d["id"])
                loot_patch.append(insert_statement("loots", lcols, [row_value(d, c) for c in lcols]))
            gcols = all_columns(c12, c30s, "loot_groups")
            for r in rows(c30s, "SELECT * FROM loot_groups WHERE pack_id=? ORDER BY id", p):
                d = dict(zip(columns(c30s, "loot_groups"), r))
                d["pack_id"] = pack_map[p]
                d["id"] = lg_alloc.get(d["id"])
                loot_patch.append(insert_statement("loot_groups", gcols, [row_value(d, c) for c in gcols]))
        lkcols = all_columns(c12, c30s, "loot_pack_dropping_npcs")
        for d in links:
            if not d.get("loot_pack_id"):
                continue
            d = dict(d)
            d["npc_id"] = npc_map.get(d["npc_id"], d["npc_id"])
            d["loot_pack_id"] = pack_map[d["loot_pack_id"]]
            if "id" in lkcols:
                d["id"] = loot_alloc.get(("npc-link", d["npc_id"]))
            loot_patch.append(insert_statement("loot_pack_dropping_npcs", lkcols, [row_value(d, c) for c in lkcols]))
        stats["loot_packs"] = len(packs)
    stats["quests"] = len(qids)
    stats["quest_components"] = len(comp_map)
    stats["quest_acts"] = len(act_map)

    # ----- overlay ------------------------------------------------------------------------
    manifest = {
        "Package": PACKAGE,
        "World": WORLD,
        "SourceVersion": "3.0.4.2",
        "MergeZones": list(MERGE_ZONES),
        "Cells": ["%03d_%03d" % c for c in cells],
        "Files": [
            {
                "Name": name,
                "Size": pak30.by_name[name.lower()][1],
                "Sha256": hashlib.sha256(pak30.read(name)).hexdigest(),
            }
            for name in overlay_files(pak30, cells)
        ],
        "SectorCounts": {str(k): sum(1 for v in owners.values() if v == k) for k in ZONE_IDS},
        "WorldXmlSize": len(xml_merged.encode("utf-8")),
        "WorldXmlSha256": hashlib.sha256(xml_merged.encode("utf-8")).hexdigest(),
        "Pak": "sunbite-wilds.pak",
    }
    overlay_bytes = sum(f["Size"] for f in manifest["Files"]) + len(xml_merged.encode("utf-8"))
    manifest["SourceBytes"] = overlay_bytes
    stats["overlay_bytes"] = overlay_bytes

    if args.build_overlay:
        written, _files = build_overlay_pak(pak30, args.build_overlay, xml_merged, cells)
        stats["overlay_pak_bytes"] = written

    return {
        "patches": {
            "zones": zone_patch,
            "npcs": npc_patch,
            "items": item_patch,
            "quests": quest_patch,
            "loot": loot_patch,
            "doodads": doodad_patch,
        },
        "spawn_doc": spawn_doc,
        "manifest": manifest,
        "summary": {
            "placements": stats["placements"],
            "placement_npcs": stats["placement_npcs"],
            "npc_remapped": stats["npc_remapped"],
            "npc_fallback_models": stats["npc_fallback_models"],
            "npcs": stats["npcs"],
            "quests": stats["quests"],
            "quest_components": stats["quest_components"],
            "quest_acts": stats["quest_acts"],
            "items": stats["items"],
            "doodads": stats["doodads"],
            "loot_packs": stats["loot_packs"],
            "pinned_spawners": stats["pinned_spawners"],
            "synthetic_spawners": stats["synthetic_spawners"],
            "authored_on_ground": stats["authored_on_ground"],
            "snapped": stats["snapped"],
            "snapped_large": stats["snapped_large"],
            "water": stats["water"],
            "steep": stats["steep"],
            "reanchored": stats["reanchored"],
            "missing_height": stats["missing_height"],
            "overlay_cells": stats["overlay_cells"],
            "overlay_files": stats["overlay_files"],
            "overlay_bytes": stats["overlay_bytes"],
            "skipped": dict(skipped),
        },
    }


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
    acts = rows(conn30, "SELECT id, quest_component_id, act_detail_id, act_detail_type FROM quest_acts WHERE quest_component_id IN (%s) ORDER BY id" % cm, *cids)
    return ids, comps, acts


CLASS_TABLE = {}


def loader_tables(repo=REPO):
    path = os.path.join(REPO, "rust", "crates", "aaemu-game", "src", "systems", "quests", "data.rs")
    with open(path, encoding="utf-8") as f:
        text = f.read()
    pairs = re.findall(r'load_table\(\s*conn,\s*"([a-z0-9_]+)",\s*"([A-Za-z0-9_]+)"', text, re.S)
    return {cls: table for table, cls in pairs}


def detail_rows(conn30, acts):
    by_class = collections.defaultdict(set)
    for a in acts:
        cls, did = a[3], a[2]
        if CLASS_TABLE.get(cls):
            by_class[cls].add(did)
    out = {}
    for cls, ids in by_class.items():
        table = CLASS_TABLE[cls]
        marks = ",".join("?" * len(ids))
        cn = columns(conn30, table)
        for r in rows(conn30, "SELECT * FROM %s WHERE id IN (%s)" % (quote_ident(table), marks), *ids):
            d = dict(zip(cn, r))
            out[(cls, d["id"])] = d
    return out


def quest_givers(conn30, comps, acts):
    comp_quest = {
        dict(zip(columns(conn30, "quest_components"), c))["id"]:
        dict(zip(columns(conn30, "quest_components"), c))["quest_context_id"]
        for c in comps
    }
    out = set()
    for aid, comp, did, cls in acts:
        table = {
            "QuestActConAcceptNpc": "quest_act_con_accept_npcs",
            "QuestActConReportNpc": "quest_act_con_report_npcs",
        }.get(cls)
        if not table:
            continue
        row = rows(conn30, "SELECT * FROM %s WHERE id=?" % table, did)
        if not row:
            continue
        d = dict(zip(columns(conn30, table), row[0]))
        if d.get("npc_id"):
            out.add(d["npc_id"])
    return out


def atomic_write(path, text):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8", newline="\n") as f:
        f.write(text)
    os.replace(tmp, path)


def render_patch(header, statements, summary):
    lines = [
        "-- %s" % header,
        "-- Generated by rust/tools/area_package/build_sunbite.py (package %s)." % PACKAGE,
        "-- Do not edit by hand: change the tool and regenerate. Idempotent `NOT EXISTS` rows;",
        "-- no 1.2 row is replaced. Counts: %s" % json.dumps(summary, sort_keys=True),
    ]
    lines += statements
    return "\n".join(lines) + "\n"


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--pak30", required=True)
    ap.add_argument("--pak12", required=True)
    ap.add_argument("--db12", required=True)
    ap.add_argument("--db30", required=True)
    ap.add_argument("--db30s", required=True)
    ap.add_argument("--heights")
    ap.add_argument("--overlay-dir", default=os.path.join(HERE, ".sunbite-overlay"))
    ap.add_argument("--build-overlay")
    ap.add_argument("--patches-dir", default=os.path.join(REPO, "SQL", "patches", "compact"))
    ap.add_argument("--spawn-out", default=os.path.join(REPO, "rust", "crates", "aaemu-game", "app", "Data", "Areas", PACKAGE, "npc_spawns.json"))
    ap.add_argument("--manifest-out", default=os.path.join(REPO, "rust", "crates", "aaemu-game", "app", "Data", "Areas", PACKAGE, "overlay.manifest.json"))
    ap.add_argument("--check", action="store_true")
    args = ap.parse_args()
    CLASS_TABLE.update(loader_tables())

    result = build(args)
    if result is None:
        return
    summary = result["summary"]
    outputs = {}
    for key, statements in result["patches"].items():
        if statements:
            outputs[os.path.join(args.patches_dir, PATCH_NAMES[key])] = render_patch(
                key, statements, summary
            )
    outputs[args.spawn_out] = json.dumps(result["spawn_doc"], ensure_ascii=False, indent=2) + "\n"
    outputs[args.manifest_out] = json.dumps(result["manifest"], ensure_ascii=False, indent=2) + "\n"
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


