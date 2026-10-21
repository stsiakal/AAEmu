# area_package

Builds the fork's optional later-version area packages. The first package is `sunbite-wilds`
(Sunbite Wilds 3+4, `zone_key` 286/287): server data only, enabled with `Areas:Enabled`.

| Script | Purpose |
| --- | --- |
| `build_sunbite.py` | Reads the 1.2 / 3.0.4.2 / 10.x databases and the 10.x spawner placements, then writes the compact patches and `app/Data/Areas/sunbite-wilds/npc_spawns.json` |
| `test_build_sunbite.py` | Unit tests for SQL escaping, id allocation, frame translation and the patch renderer |

```sh
python -I rust/tools/area_package/build_sunbite.py \
    --db12 <1.2 compact.sqlite3> \
    --db30 D:/aa-re/db-references/3.0.4.2-20240828/compact.sqlite3 \
    --db30s D:/aa-re/db-references/3.0.4.2-20240828/compact.server.table.sqlite3 \
    --db10 D:/aa-re/db-references/server-db-probe-20261019/extracted/aa10x__game_db_game.sqlite3.decrypted.sqlite3 \
    --pak10 D:/aaemu-audits/research/spawns/pak10 \
    --w10 D:/aaemu-audits/research/spawns/w10 \
    --w12 D:/aaemu-audits/research/spawns/w12
python -I -m unittest discover -s rust/tools/area_package -p "test_*.py"
```

`--check` compares the outputs without writing them. The script never writes a database.

## Sources and rules

- Zones, NPCs, items, quests and doodad templates: 3.0.4.2 (the closest retail generation). 1.2
  ids are never overwritten: an id that already exists is skipped, and a colliding quest detail /
  component / act id is remapped into a free range above the 1.2 maximum.
- NPC spawner placements: the 10.x client `level_design/zone/{286,287}/zone_server/npc_spawners.g`.
  3.0 has no placement files for these zones, so this is a documented guess. The 10.x zone origin
  is translated to the 1.2 `world.xml` origin (`(21,4)` -> `(18,4)` / `(18,5)`), and the authored
  Z is kept unless `--heights` (from `aaemu-game`'s ignored `dump_terrain_heights` test) has a 1.2
  terrain height within 5 m.
- Spawner templates: the 28 ids already in 1.2 (same members as 3.0) are reused; the 78 missing
  ids are imported from 10.x.
- Quests: `quest_contexts.zone_id = 227` and level <= 55. A quest whose direct NPC (model must
  exist in 1.2), item or doodad reference cannot be resolved is skipped and listed in the summary.
- Items: 3.0 rows, level <= 62; ids >= 8,000,000 are remapped below the cap (the owner rule).
- Loot: only for the newly imported NPCs; neither source links these NPCs to a pack, so the
  current package emits none.
