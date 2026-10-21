# area_package

Builds the fork's optional later-version area packages. The first package is `sunbite-wilds`
(Sunbite Wilds 3+4, `zone_key` 286/287), enabled with `Areas:Enabled`.

| Script | Purpose |
| --- | --- |
| `build_sunbite.py` | Reads the 3.0.4.2 client pak and DBs plus the 1.2 pak, writes the compact patches, `npc_spawns.json`, the overlay manifest and (optionally) the terrain overlay pak |
| `client_pak.py` | Minimal read-only reader / overlay writer for the `game_pak` (AAPak type A) format |
| `test_build_sunbite.py` | Unit tests (SQL, ids, world merge, npc_map records, pak round-trip, renderer) |

```sh
python -I rust/tools/area_package/build_sunbite.py \
    --pak30 "C:/Users/stsiakal/Downloads/Archeage Client v.3.0.4.2 (r336598)/game_pak" \
    --pak12 D:/AA1.2/game_pak \
    --db12 <1.2 compact.sqlite3> \
    --db30 D:/aa-re/db-references/3.0.4.2-20240828/compact.sqlite3 \
    --db30s D:/aa-re/db-references/3.0.4.2-20240828/compact.server.table.sqlite3 \
    --overlay-dir <scratch dir>
#   -> writes <scratch>/query.json; run the height sampler:
AAEMU_GAME_PAK=<scratch dir> AAEMU_HEIGHT_QUERY=<scratch>/query.json \
AAEMU_HEIGHT_OUT=<scratch>/heights.json \
    cargo test -p aaemu-game dump_terrain_heights -- --ignored
#   -> rerun with --heights <scratch>/heights.json and build the overlay:
python -I rust/tools/area_package/build_sunbite.py ... \
    --heights <scratch>/heights.json \
    --build-overlay rust/crates/aaemu-game/app/Data/Areas/sunbite-wilds/sunbite-wilds.pak
python -I -m unittest discover -s rust/tools/area_package -p "test_*.py"
```

`--check` compares the outputs without writing them. The script never writes a database or a source
pak. The overlay pak is generated locally and ignored by git.

## Sources and rules

- 3.0 `game/worlds/main_world/map_data/npc_map/e_sunny_wilderness.dat`: retail NPC placements
  (`u32 npc id`, `f32 x/y/z`). The 3.0 pak has no `zone_server/npc_spawners.g` for these zones.
- 3.0 `world.xml`: zone sector ownership; zone nodes 286/287 and 157/190 are merged into the 1.2
  `world.xml`. 157/190 are needed for the early quest givers; their terrain stays 1.2.
- 3.0 `compact.sqlite3` / `compact.server.table.sqlite3`: NPCs (levels/stats), quests, items,
  doodads, loot and the spawner templates used as parameter donors.
- 3.0 terrain: the six 286/287 cells are copied into the overlay. 3.0 and 1.2 terrain differ (up
  to ~200 m), so no 1.2-only package can place this content.
- 1.2 ids are never overwritten: a colliding 3.0 NPC id with a different name/level is remapped
  to `30000000+`; quest component/act/detail ids are allocated above the 1.2 maxima.
- Every placement is pinned to a one-member spawner template. 7 safe 1.2 templates are reused;
  141 one-member synthetic templates (`100000000+`) are added for the rest.
- Z is snapped to the terrain sampled by the server; `--heights` also reports authored-vs-terrain
  (on-ground within 3 m), water and slope counts.
- Quests: `zone_id = 227`, level <= 55, and every accept/report NPC has a 3.0 `npc_map` record.
  Items are capped at level 62 and no new item id is >= 8,000,000.
