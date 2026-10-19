"""Step 4 of the spawn import: place authored 10.x spawners in the 1.2 world files.

The 10.x client pak is the only source with NPC spawner positions:
`game/worlds/<world>/level_design/zone/<zone>/zone_server/npc_spawners.g`. This tool matches
those placements against the 1.2 `compact.sqlite3` and the 1.2 `world.xml` zone origins and
writes spawn entries into `app/Data/Worlds/<world>/npc_spawns.json`.

Two modes:

* `events` - spawn/despawn effects (`npc_spawner_spawn_effects` / `..._despawn_effects`) name an
  event spawner id (`spawnerId`) but the 1.2 data has no placement for most of them. Emit one
  entry per NPC member of the placed template with `NpcSpawnerIds: [spawnerId]`.
* `unplaced` - ambient / dungeon / schedule spawners that no world file places (the audit's
  `fill_unplaced_spawners.json`, minus the disabled "비활성" rows). Emit one ordinary entry per
  placement, pinned with `SpawnerId` so the exact template is spawned (`NpcGroup` members are
  expanded by the server from `npc_group_members`).

Only zones whose `originX/originY` are identical in the 1.2 and 10.x `world.xml` are used (ten
zones moved between the versions; their 10.x frame does not map onto 1.2). Templates and NPC
members must exist in the 1.2 compact DB. Heights are re-snapped to the 1.2 server heightmap:
the query file produced here is fed to the ignored test `dump_terrain_heights`
(`AAEMU_GAME_PAK`, `AAEMU_HEIGHT_QUERY`, `AAEMU_HEIGHT_OUT`) and the accepted terrain height
replaces the 10.x Z. When the 1.2 terrain disagrees by more than 5 m, an existing spawn of the
world within 40 m / 8 m (interiors, caves, bridges) keeps the authored Z, and on clipped ground
(heightmap `u16 / 64` ceiling) the clip height is used. Otherwise the authored 10.x Z is kept:
the shipped event entries show the same large terrain differences in instances (Howling Abyss),
where the heightmap is the outdoor shell, not the room floor. `--strict` drops those instead.

    python -I fill_spawns.py events --pak10 <10.x tree> --w10 <10.x world.xml tree> \
        --w12 <1.2 world.xml tree> --compact <db> --worlds <Data/Worlds> --queries queries.json
    AAEMU_GAME_PAK=<1.2 game_pak> AAEMU_HEIGHT_QUERY=queries.json AAEMU_HEIGHT_OUT=heights.json \
        cargo test -p aaemu-game dump_terrain_heights -- --ignored
    python -I fill_spawns.py events ... --heights heights.json --apply

The emitted `Title`s name the source; the loader ignores them.
"""
import argparse
import collections
import json
import math
import os
import re
import sqlite3
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from lib import Grid, load_spawn_file, tolerant_json  # noqa: E402
from spawn_pak import read_spawner_file, spawner_files, transform, zone_origins  # noqa: E402

TERRAIN_TOL = 5.0
CLIP = 1023.5
NEIGHBOR_R = 40.0
NEIGHBOR_DZ = 8.0
DUP = 2.0


def load_compact(path):
    conn = sqlite3.connect("file:%s?mode=ro" % path.replace("\\", "/"), uri=True)
    q = lambda sql, *a: conn.execute(sql, a).fetchall()
    spawners = {
        r[0]: {"category": r[1], "name": r[2], "comment": r[3] or ""}
        for r in q("select id,npc_spawner_category_id,name,comment from npc_spawners")
    }
    members = collections.defaultdict(list)
    for sid, mid, mtype in q(
        "select npc_spawner_id,member_id,member_type from npc_spawner_npcs order by id"
    ):
        members[sid].append((mid, mtype))
    npcs = {r[0] for r in q("select id from npcs")}
    groups = collections.defaultdict(list)
    for gid, npc, leader in q(
        "select npc_group_id,npc_id,is_leader from npc_group_members order by id"
    ):
        groups[gid].append((npc, leader in (1, "1", "t", "true", True)))
    effect_ids = {
        r[0]
        for r in q(
            "select distinct spawner_id from npc_spawner_spawn_effects "
            "union select distinct spawner_id from npc_spawner_despawn_effects"
        )
    }
    return spawners, members, npcs, groups, effect_ids


def world_entries(path):
    try:
        with open(path, encoding="utf-8-sig") as f:
            return tolerant_json(f.read())
    except FileNotFoundError:
        return []


def event_ids_in(path):
    ids = set()
    for e in world_entries(path):
        ids.update(e.get("NpcSpawnerIds") or [])
    return ids


def pinned_ids_in(path):
    return {e.get("SpawnerId") for e in world_entries(path) if e.get("SpawnerId")}


def fmt(v):
    r = round(float(v), 3)
    return repr(int(r)) if r == int(r) else repr(r)


def render_entry(e, ind, unit, nl):
    inner, deep = ind + unit, ind + unit + unit
    lines = [ind + "{", f'{inner}"UnitId": {e["unit_id"]},']
    if e.get("title"):
        lines.append(f'{inner}"Title": {json.dumps(e["title"], ensure_ascii=False)},')
    if e.get("npc_spawner_ids"):
        ids = ", ".join(str(i) for i in e["npc_spawner_ids"])
        lines.append(f'{inner}"NpcSpawnerIds": [ {ids} ],')
    if e.get("pinned"):
        lines.append(f'{inner}"SpawnerId": {e["pinned"]},')
    lines += [
        inner + '"Position": {',
        f'{deep}"X": {fmt(e["x"])},',
        f'{deep}"Y": {fmt(e["y"])},',
        f'{deep}"Z": {fmt(e["z"])},',
        f'{deep}"Yaw": {fmt(e.get("yaw", 0.0))}',
        inner + "}",
        ind + "}",
    ]
    return nl.join(lines)


def append_entries(path, entries):
    raw = open(path, "rb").read().decode("utf-8")
    nl = "\n" if ("\n" in raw and "\r\n" not in raw) else "\r\n"
    m = re.search(r"\n([ \t]*)\{", raw)
    ind = m.group(1) if m else "  "
    m2 = re.search(r'\n([ \t]*)"(?:UnitId|Id)"', raw)
    unit = (
        m2.group(1)[len(ind):]
        if m2 and m2.group(1).startswith(ind) and len(m2.group(1)) > len(ind)
        else "  "
    )
    end = raw.rindex("]")
    head = raw[:end].rstrip()
    body = ("," + nl).join(render_entry(e, ind, unit, nl) for e in entries)
    sep = nl if head.endswith("[") else ("," if not head.endswith(",") else "") + nl
    out = head + sep + body + nl + raw[end:]
    tolerant_json(out)
    open(path, "wb").write(out.encode("utf-8"))


def grids(worlds, world):
    g = Grid()
    for name in ("npc_spawns.json", "doodad_spawns.json"):
        for uid, x, y, z in load_spawn_file(os.path.join(worlds, world, name)):
            g.add(uid, x, y, z)
    return g


def resolve_z(cand, height, grid, strict):
    z = cand["z"]
    h = height.get(cand["id"])
    if h and "h" in h:
        hz = h["h"]
        if hz < CLIP and abs(z - hz) <= TERRAIN_TOL:
            return round(hz, 3), "terrain"
        if hz >= CLIP and z >= hz - TERRAIN_TOL:
            return round(hz, 3), "clip"
    if any(abs(e[3] - z) <= NEIGHBOR_DZ for e in grid.near(cand["x"], cand["y"], NEIGHBOR_R)):
        return z, "neighbor"
    if strict:
        return None, "skipped"
    # Dungeon interiors / caves: the 1.2 terrain disagrees with the authored floor. The
    # existing 199 event entries show the same >50 m differences (Howling Abyss), so keep the
    # authored Z instead of dropping the placement.
    return z, "authored"


def gen_events(args, spawners, members, npcs, effect_ids):
    o10 = zone_origins(args.w10)
    o12 = zone_origins(args.w12)
    worlds = {d for d in os.listdir(args.worlds) if os.path.isdir(os.path.join(args.worlds, d))}
    repo = {}
    for world in worlds:
        repo[world] = event_ids_in(os.path.join(args.worlds, world, "npc_spawns.json"))
    out = []
    skipped = collections.Counter()
    seen = set()
    for world, zone, path in spawner_files(args.pak10):
        if world not in worlds:
            skipped["world not in 1.2 data"] += 1
            continue
        a, b = o10.get((world, zone)), o12.get((world, zone))
        if not a or not b or a[:2] != b[:2]:
            skipped["zone origin differs / missing"] += 1
            continue
        for s in read_spawner_file(path):
            sid, tmpl = s["id"], s["template"]
            if sid not in effect_ids:
                continue
            if not s["points"]:
                skipped["no point"] += 1
                continue
            if sid in repo[world]:
                skipped["already in world file"] += 1
                continue
            # One entry per (spawner id, world): the event effect spawns every entry with that
            # id, so repeated editor placements of the same id must not stack copies.
            if (world, sid) in seen:
                skipped["duplicate (sid, world)"] += 1
                continue
            t = spawners.get(tmpl)
            if not t:
                skipped["template not in 1.2"] += 1
                continue
            mem = [m for m, mt in members.get(tmpl, []) if mt == "Npc" and m in npcs]
            if not mem:
                skipped["no 1.2 NPC members"] += 1
                continue
            seen.add((world, sid))
            x, y = transform(a, s["points"][0][0], s["points"][0][1])
            z = s["points"][0][2]
            yaw = math.degrees(s["zrot"][0]) if s["zrot"] else 0.0
            for m in mem:
                out.append(
                    {
                        "mode": "event",
                        "world": world,
                        "spawner_id": sid,
                        "template": tmpl,
                        "unit_id": m,
                        "x": round(x, 3),
                        "y": round(y, 3),
                        "z": round(z, 3),
                        "yaw": round(yaw, 4),
                        "npc_spawner_ids": [sid],
                        "id": f"ev:{world}:{zone}:{sid}:{m}",
                        "title": f"spawn-fill: 10.x event spawner {sid} (member {m})",
                    }
                )
    return out, skipped


def resolve_group_unit(group_id, groups, npcs):
    rows = groups.get(group_id, [])
    for npc, leader in rows:
        if leader and npc in npcs:
            return npc
    for npc, _ in rows:
        if npc in npcs:
            return npc
    return None


def gen_unplaced(args, spawners, members, npcs, groups):
    o10 = zone_origins(args.w10)
    o12 = zone_origins(args.w12)
    worlds = {d for d in os.listdir(args.worlds) if os.path.isdir(os.path.join(args.worlds, d))}
    with open(args.fill, encoding="utf-8") as f:
        fill = json.load(f)
    out = []
    skipped = collections.Counter()
    for entry in fill:
        sid = entry["spawner_id"]
        if entry.get("class_") == "inactive(disabled)":
            continue
        t = spawners.get(sid)
        if not t:
            skipped["template not in 1.2"] += 1
            continue
        member_rows = members.get(sid, [])
        unit_id = None
        for m, mt in member_rows:
            if mt == "Npc" and m in npcs:
                unit_id = m
                break
            if mt == "NpcGroup":
                unit_id = resolve_group_unit(m, groups, npcs)
                if unit_id:
                    break
        if unit_id is None:
            skipped["no usable NPC member"] += 1
            continue
        for i, placement in enumerate(entry.get("placements", [])):
            world, zone = placement["world"], placement["zone"]
            if world not in worlds:
                skipped["world not in 1.2 data"] += 1
                continue
            a, b = o10.get((world, zone)), o12.get((world, zone))
            if not a or not b or a[:2] != b[:2]:
                skipped["zone origin differs / missing"] += 1
                continue
            points = placement.get("points") or []
            if not points:
                skipped["no point"] += 1
                continue
            # `fill_unplaced_spawners.json` points are already world metres
            # (`zone_origin * 1024 + local`).
            x, y = points[0][0], points[0][1]
            z = points[0][2]
            out.append(
                {
                    "mode": "unplaced",
                    "world": world,
                    "spawner_id": sid,
                    "template": sid,
                    "unit_id": unit_id,
                    "x": round(x, 3),
                    "y": round(y, 3),
                    "z": round(z, 3),
                    "yaw": 0.0,
                    "pinned": sid,
                    "id": f"un:{world}:{sid}:{i}",
                    "title": f"spawn-fill: 10.x ambient spawner {sid}",
                }
            )
    return out, skipped


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("mode", choices=("events", "unplaced"))
    ap.add_argument("--pak10", required=True)
    ap.add_argument("--w10", required=True)
    ap.add_argument("--w12", required=True)
    ap.add_argument("--compact", required=True)
    ap.add_argument("--worlds", required=True)
    ap.add_argument("--fill", help="fill_unplaced_spawners.json (unplaced mode)")
    ap.add_argument("--queries")
    ap.add_argument("--heights")
    ap.add_argument("--apply", action="store_true")
    ap.add_argument("--strict", action="store_true")
    ap.add_argument("--summary")
    args = ap.parse_args()

    spawners, members, npcs, groups, effect_ids = load_compact(args.compact)
    if args.mode == "events":
        cands, skipped = gen_events(args, spawners, members, npcs, effect_ids)
    else:
        if not args.fill:
            ap.error("unplaced mode needs --fill")
        cands, skipped = gen_unplaced(args, spawners, members, npcs, groups)
    before = len(cands)

    if args.queries:
        json.dump(
            [{"id": c["id"], "world": c["world"], "x": c["x"], "y": c["y"]} for c in cands],
            open(args.queries, "w", encoding="utf-8"),
        )

    if not args.heights:
        print(json.dumps({"candidates": before, "skipped": skipped}, indent=1, ensure_ascii=False))
        return

    with open(args.heights, encoding="utf-8") as f:
        heights = {r["id"]: r for r in json.load(f)}
    gcache = {}
    accepted = []
    basis = collections.Counter()
    for c in cands:
        if c["world"] not in gcache:
            gcache[c["world"]] = grids(args.worlds, c["world"])
        z, why = resolve_z(c, heights, gcache[c["world"]], args.strict)
        if z is None:
            basis["skipped"] += 1
            continue
        c["z"], c["basis"] = z, why
        basis[why] += 1
        accepted.append(c)

    summary = {
        "candidates": before,
        "accepted": len(accepted),
        "basis": dict(basis),
        "per_world": dict(collections.Counter(c["world"] for c in accepted)),
        "skipped": skipped,
    }
    if args.summary:
        json.dump(summary, open(args.summary, "w", encoding="utf-8"), indent=1, ensure_ascii=False)
    print(json.dumps(summary, indent=1, ensure_ascii=False))
    if not args.apply:
        return

    by_world = collections.defaultdict(list)
    for c in accepted:
        by_world[c["world"]].append(c)
    added = {}
    for world, rows in sorted(by_world.items()):
        path = os.path.join(args.worlds, world, "npc_spawns.json")
        existing = load_spawn_file(path)
        new = []
        for c in sorted(rows, key=lambda r: (r["spawner_id"], r["unit_id"], r["x"], r["y"])):
            # Event entries are keyed by spawner id and must all stay (several waves can share
            # an anchor); ordinary placements keep the loader's UnitId/position duplicate rule.
            if not c.get("npc_spawner_ids") and any(
                abs(x - c["x"]) < DUP and abs(y - c["y"]) < DUP and z == c["z"]
                for uid, x, y, z in existing
                if uid == c["unit_id"]
            ):
                continue
            new.append(c)
            existing.append((c["unit_id"], c["x"], c["y"], c["z"]))
        added[world] = len(new)
        if new:
            append_entries(path, new)
    print(json.dumps({"added": added}, indent=1, ensure_ascii=False))


if __name__ == "__main__":
    main()
