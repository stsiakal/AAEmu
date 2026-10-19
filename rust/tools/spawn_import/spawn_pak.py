"""Reader for the 10.x client spawner files (`zone_server/npc_spawners.g`) and zone origins.

The 10.x client stores authored NPC spawner placements in
`game/worlds/<world>/level_design/zone/<zone>/zone_server/npc_spawners.g` (the `editor/`
copy is identical). Each block holds the event spawner id (`spawnerId`), the spawner template
(`spawnerType` = `npc_spawners.id`), the area type and zone-local points. World coordinates are
`world.xml` zone `originX/originY * 1024 + local` (the 1024 m zone resolution).

Both the 10.x pak and the 1.2 pak can be passed either as the extracted `game/` tree or as a
directory that contains it (the audit's `pak10` / `w10` / `w12` folders are extracted trees).
"""
import glob
import os
import re

RESOLUTION = 1024

_FIELD = re.compile(r"^\s*(spawnerId|spawnAreaType|spawnerType|roamingArea)\s+(\S+)\s*$")
_POS = re.compile(
    r"^\s*pos\s*\(\s*x\s+([-+0-9.eE]+)\s*,\s*y\s+([-+0-9.eE]+)\s*,\s*z\s+([-+0-9.eE]+)\s*\)"
)
_ZROT = re.compile(r"^\s*zRot\s+([-+0-9.eE]+)\s*$")
_ZONE = re.compile(r"<Zone\b([^>]*)>")
_ATTR = re.compile(r'(\w+)\s*=\s*"([^"]*)"')


def _game_dir(root):
    root = root.replace("\\", "/").rstrip("/")
    if os.path.isdir(root + "/game/worlds"):
        return root + "/game"
    if os.path.isdir(root + "/worlds"):
        return root
    return root + "/game"


def parse_spawners(text):
    """The `spawner` blocks of one `.g` file.

    Returns a list of `{id, area, template, points: [[x, y, z]], zrot: [radians]}` in file
    order. Unknown lines are ignored (paths, comments, other blocks).
    """
    rows = []
    cur = None
    for line in text.replace("\r", "").split("\n"):
        if line.strip() == "spawner":
            if cur is not None:
                rows.append(cur)
            cur = {"id": None, "area": None, "template": None, "points": [], "zrot": []}
            continue
        if cur is None:
            continue
        m = _FIELD.match(line)
        if m:
            key, value = m.group(1), m.group(2)
            if key == "spawnerId":
                cur["id"] = int(value)
            elif key == "spawnerType":
                cur["template"] = int(value)
            elif key == "spawnAreaType":
                cur["area"] = value
            else:
                cur["roaming"] = value
            continue
        m = _POS.match(line)
        if m:
            cur["points"].append([float(m.group(1)), float(m.group(2)), float(m.group(3))])
            continue
        m = _ZROT.match(line)
        if m:
            cur["zrot"].append(float(m.group(1)))
    if cur is not None:
        rows.append(cur)
    return [r for r in rows if r["id"] is not None]


def zone_origins(root):
    """`{(world, zone_id): (origin_x, origin_y, name)}` from every `world.xml`."""
    game = _game_dir(root)
    out = {}
    for path in glob.glob(game + "/worlds/*/world.xml"):
        world = path.replace("\\", "/").split("/")[-2]
        with open(path, encoding="utf-8", errors="replace") as f:
            text = f.read()
        for m in _ZONE.finditer(text):
            attrs = dict(_ATTR.findall(m.group(1)))
            try:
                zone = int(attrs["id"])
                ox = int(attrs["originX"])
                oy = int(attrs["originY"])
            except (KeyError, ValueError):
                continue
            out[(world, zone)] = (ox, oy, attrs.get("name", ""))
    return out


def transform(origin, local_x, local_y):
    """Zone-local point -> world metres: `origin * 1024 + local`."""
    return origin[0] * RESOLUTION + local_x, origin[1] * RESOLUTION + local_y


def spawner_files(root):
    """`(world, zone, path)` of the `zone_server/npc_spawners.g` files."""
    game = _game_dir(root)
    for path in sorted(
        glob.glob(game + "/worlds/*/level_design/zone/*/zone_server/npc_spawners.g")
    ):
        parts = path.replace("\\", "/").split("/")
        world = parts[parts.index("worlds") + 1]
        zone = int(parts[parts.index("zone") + 1])
        yield world, zone, path


def read_spawner_file(path):
    with open(path, encoding="utf-8", errors="replace") as f:
        return parse_spawners(f.read())
