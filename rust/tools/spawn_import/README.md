# spawn_import

Import NPC / doodad spawn placements into `app/Data/Worlds/<world>/*_spawns.json`.

| Script | Purpose |
| --- | --- |
| `collect.py` / `evaluate.py` / `apply.py` | The October 17 quest-NPC / doodad import: candidate JSON -> 1.2 terrain queries -> decisions -> world files. See `PORTING.md` ("Spawn data import"). |
| `spawn_pak.py` | Reader for the 10.x client `zone_server/npc_spawners.g` and `world.xml` zone origins. |
| `fill_spawns.py` | Later-version fill (October 19): event and ambient spawners from the 10.x editor placements. |
| `test_spawn_pak.py` | Parser / transform unit tests and the cross-check against the shipped event entries. |

## Later-version spawn fill

The 10.x client (`game/worlds/*/level_design/zone/*/zone_server/npc_spawners.g`) is the only
source with NPC spawner positions: `spawnerId`, `spawnerType` (= `npc_spawners.id`) and
zone-local points. World metres are `zone originX/originY * 1024 + local`.

`fill_spawns.py` has two modes:

* `events` - every `spawnerId` referenced by `npc_spawner_spawn_effects` /
  `npc_spawner_despawn_effects` gets one entry per NPC member with `NpcSpawnerIds: [id]`.
  Only zones whose 1.2 and 10.x `world.xml` origins are identical are used, and the template /
  member must exist in the 1.2 `compact.sqlite3`.
* `unplaced` - the audit's `fill_unplaced_spawners.json` ambient/dungeon/schedule spawners
  (the disabled "비활성" rows are skipped) get ordinary entries pinned with `SpawnerId`, so the
  exact template is spawned. `NpcGroup` members are expanded by the server from
  `npc_group_members`.

Heights are re-snapped to the 1.2 server heightmap with the ignored
`dump_terrain_heights` test (`AAEMU_GAME_PAK`, `AAEMU_HEIGHT_QUERY`, `AAEMU_HEIGHT_OUT`).
A 10.x Z within 5 m of the 1.2 terrain is replaced by the terrain height; interiors keep the
authored Z only when an existing spawn corroborates it (40 m / 8 m) or when the terrain is
clipped. The shipped event entries show the same >50 m differences in instances, so those
keep the authored Z (`--strict` drops them). `--apply` appends the entries; runs are
idempotent for ordinary placements and event entries are keyed by spawner id.

Generated data (October 19, 2026): 2,901 event entries / 1,214 event spawner ids. Effect-row
coverage: 1,456 of the 2,006 dangling spawn rows, 162/162 boss and 34/34 player-item rows.

**The `unplaced` (ambient) mode is opt-in (`--include-ambient`) and its output is NOT shipped.**
Our existing placements come from the 1.2 client `npc_map`, i.e. retail 1.2 data. The ambient
spawners (dungeon / schedule / field templates) are 10.x editor placements the 1.2 map never had;
adding 653 of them would change mob density in `main_world` without any evidence that retail
1.2 ran them. They need 1.2 evidence first. Event spawners are different: they only run when a
skill summons them.

```bash
python -I fill_spawns.py events \
  --pak10 D:/aaemu-audits/research/spawns/pak10 \
  --w10   D:/aaemu-audits/research/spawns/w10 \
  --w12   D:/aaemu-audits/research/spawns/w12 \
  --compact rust/crates/aaemu-game/app/Data/compact.sqlite3 \
  --worlds  rust/crates/aaemu-game/app/Data/Worlds \
  --queries queries.json
AAEMU_GAME_PAK='D:\AA1.2\game_pak' AAEMU_HEIGHT_QUERY=queries.json \
  AAEMU_HEIGHT_OUT=heights.json \
  cargo test -p aaemu-game dump_terrain_heights -- --ignored
python -I fill_spawns.py events ... --heights heights.json --apply
python -I fill_spawns.py unplaced ... --include-ambient --fill fill_unplaced_spawners.json --heights heights.json --apply
python -I -m unittest discover -s rust/tools/spawn_import -p "test_*.py"
```

The source is a later client version (10.0.2.13); positions are only used where the 1.2 zone
frame is identical, and templates/members are from the 1.2 compact DB. Recorded as a
deliberate later-version fill in `rust/crates/aaemu-game/PORTING.md`.
