# Backports

- Audience: Contributors
- Last verified against: `rust-rewrite-bots` on October 20, 2026 (static research; no backport landed yet)
- Target: ArcheAge 1.2 (`r208022`)

## Goal

Take the later-version quality-of-life (QoL) and the small amount of content that makes a 1.2 server
nicer to play, without changing what 1.2 feels like: same classes, gear progression, skill points and
a recognizable level curve, minus the grind wall. The Bevy client reads the server's `compact.sqlite3`,
so server data patches are visible without a client DB rebuild; the 1.2 client reads its own encrypted
DB and does not see them. Counts and citations below come from static, read-only inspection of the 1.2
server, Bevy, the 3.0.4.2 reference DB and the decrypted 10.x DB; nothing here is implemented.

## Decisions

| # | Decision (2026-10-20) | Consequence |
| --- | --- | --- |
| 1 | Level 55 means the 1.2 cap unlocked | Unhide the ten capstone skills; do not import 10.x level-55 content |
| 2 | Use the 3.0 retail XP curve for 51-55 (17,453,110 total at 55) | Copy the 3.0 `levels` and `quest_supplies` rows; keep 1.2 `skill_points` (24-28) |
| 3 | Keep the 1.2 Eanad level-53 sets as best-in-slot | No level-55 gear import and no rebalance pass |
| 4 | Add all later-version costumes | 3.0 and 10.x costumes, delivered in audited batches rather than one import |
| 5 | Client gap order: farming/housing first, transport later | Housing/farming UI lands first and unlocks the planting snap grid; mounts and vehicles follow |
| 6 | Skip 10.x level-55 inflation and the 3.0 Abyssal skills | No 782-item/866-quest import; no 34090-34314 branch |

## Ranked candidates

Effort: **S** under a day, **M** a few days, **L** a week or more for one worker. "Feel" is the
effect on 1.2 balance and feel. Status follows the decisions above.

| # | Candidate | Status | Effort | Feel | Notes |
| --- | --- | --- | --- | --- | --- |
| 1 | Level 55 and the ten capstone skills | Decided | S | None (unlock) | Data-only: `SHOW='t'` for ten skill rows |
| 2 | 51-55 XP curve and quest exp | Decided | S | None if tuned | 3.0 `levels` and 3.0 `quest_supplies`; 1.2 skill points stay |
| 3 | Costumes, all later-version | Decided | S-M per batch | Cosmetic only | Batch by asset closure; 779 are new in 3.0 alone |
| 4 | Planting snap/grid | Decided | S-M after the farming UI | QoL | Client-side; default on |
| 5 | Farming/housing UI in Bevy | Planned | M-L | QoL | First client gap and the prerequisite for #4 |
| 6 | UI scale | Planned | S | None | Client-only; default 1.0 preserves the look |
| 7 | Auction window with filters | Planned | M | None | Client gap, not a backport; the 1.2 protocol already filters |
| 8 | Saved ability sets ("dual spec") | Later | M-L | None | Custom server state and packets; worthwhile once 55 makes builds matter |
| 9 | Mounts: data-only 3.0 items with 1.2 models | Later | S data, L client | Cosmetic/utility | Needs local mount control first |
| 10 | New mounts, vehicles and later slaves (10.x-only assets) | Later | M data, L client | Content | Only after #9; 3.0 slave rows need conversion and Bevy cannot assemble prefabs yet |
| 11 | Level-55 gear (10.x Archive sets) | No | M | Content/balance | Eanad level-53 sets stay best-in-slot |
| 12 | 10.x level-55 inflation (782 items, 866 quests) | No | L | Changes the game | Later stats (resources, awaken/reinforce) come with it |
| 13 | 3.0 Abyssal skills (34090-34314) | No | S data | Changes the game | 3.0-era branch, not the ten 1.2 capstones |

## Level 55

### What 1.2 already has

- **Cap and table.** `PLAYER_LEVEL_CAP = 55` (`data/experience.rs:6-8`), loaded from every `levels`
  row (`data/mod.rs:155`). 1.2 rows 51-55 total 8.08M / 16.70M / 36.28M / 80.35M / 179.31M XP with
  24-28 skill points; 3.0 ends at 17,453,110 with the same points, 10.x at 7,784,000 with 18-20.
- **Ten capstones, one per ability**: 23587 (1), 23588 (2), 23589 (3), 23934 (4), 23591 (5), 23592
  (6), 23593 (7), 23594 (8), 23595 (9), 23596 (10). Each has `ability_level=55`, `need_learn=t`, one
  point and `SHOW='f'` in 1.2 (`show=1` in 3.0); four are plot-based (1195/1226/1266/1202), six have
  `skill_effects`. Learn accepts them (`systems/skills/progress.rs:444-483`); Bevy reads `SHOW` and
  `need_learn` (`crates/aa-client/src/hud/skillbook.rs:100,107`).
- **Content is present.** 268 quests at 51-55 (100/79/45/38/6), all with components; the
  `instance_library` world with spawns (boss 14330) and enter-instance doodads for zones 209 and
  212-214; 311 NPCs at 51-55; 331 items at 51-54 (300 at 53), including the Eanad sets 206-217. No
  item requires level 55.
- **The blocker is reward size.** `quest_supplies` gives 5,100-11,900 XP at 51-55, so the 268 quests
  supply about 2M against 179.3M. 3.0 pays 9,023-27,512 per quest, 10.x 27,070-82,536.

### Plan

1. **Data patch.** `UPDATE skills SET SHOW='t'` for the ten ids in the patched server
   `compact.sqlite3` via `SQL/patches/compact/`; Bevy sees them, the 1.2 client does not.
2. **XP choice.** Copy the 3.0 `levels.total_exp` rows for 51-55 and the 3.0 `quest_supplies`; keep
   the 1.2 `skill_points`. Do not keep 179M or import the 10.x 18-20 point spread.
3. **Bevy in lockstep.** `crates/aa-net/src/model.rs:48,51-57` hardcodes the 1.2 table and
   `level_for_total` drives ability levels and the EXP bar; update it or load `levels` from `AA_DB`.
4. **Gear.** Keep the Eanad 206-217 sets as practical best-in-slot; no level-55 gear patch.
5. **Test.** `/level 55`, learn and cast all ten (four plot-based), check the EXP bar and ranks. Stored
   ranks are computed at learn time (`systems/skills/progress.rs:472-482`); a stale rank is unverified.

## Feature notes

### Planting snap and housing grid

The server accepts the world `x, y, z, z_rot, scale` of a `CSCreateDoodadPacket`
(`systems/npcs/doodad/manager.rs:544-611`), checks item/seed, house permission, zone and a 30 m radius
(`PLACE_RANGE_SANITY`, `:542`), then attaches the crop and saves a local transform (`:393`,
`:423-463`). Farms list seed categories (368 rows) and `housings.garden_radius` is 4-22 m; no grid exists.

10.x has the grid in the client (`OverlappedGridChecker<...Builder>`, `hs_simple_grid_draw`,
`gardenSize`; `out-10x-client-x2game-strings.txt:51012-51024`,
`x2game-dev_dedicate.dll.strings.txt:9415-9426`, 21681, 21722-21731), not a server table. 1.2's Lua
housing has no grid (`SetSnap` is widget anchoring; inferred) and Bevy has no farming UI
(`docs/research/remaining.md` H2/G3/H7). The snap is S once the UI exists; server overlap is optional.

### Mounts

1.2 has 176 mate items and 351 mount skills; 3.0 has 428/1,263, 10.x 552/2,137. Many of the 255 new
3.0 mate items are pets or dolls. Dragon baby, pegasus, snowlion, stormstrider and orange donkey are
in 1.2; carrot donkey 35302, pirate/daru/cow dolls, landsnails and black puppy are 10.x-only.

Bevy renders remote mounts and riders (`docs/aa-client-units.md:52-63`) but has no own
summon/mount/dismiss flow; riders use `AA_FAKE_MOUNT=1` (`:93`). The server summons and revives mates
(`systems/vehicles/mates.rs`). Order: local mount control (L), then 3.0 data-only mounts (S), then 10.x.

### Vehicles

1.2 has 140 slaves/78 items in nine kinds, including cars (model 1293, tractor 1008), ships, rowboats
and cannons; 3.0 has 644/177 (514 new: cars, buses, skateboards, a lava car, fish boats, battle
ships), 10.x 1,605/283. 3.0 `slaves` rows add capacity, collision, level and kinds 10/11, so they
need conversion. New cars are prefab XML plus CGF, and Bevy resolves only single-object/effect
prefabs (`crates/aa-client/src/ingame/data.rs:169-180,344-363`), not assembly; no vehicle driving
exists (`README.md:74`). Make 1.2 cars and ships work first, then port one test vehicle.

### Costumes

1.2 has 444 `item_categories` 10 items; 3.0 has 1,223 and 10.x 1,423, so 779 are new in 3.0 alone.
No wardrobe system was found in 10.x: costumes stay items with dyeing and the item-look converter
(1.2 has 27 `item_look_converts`), and Bevy renders costumes and body replacement.

The decision takes **all** later-version costumes, wider than the earlier curated-subset
recommendation. Batches follow the gear chain (items -> `item_armors` -> `armor_assets` ->
`item_armor_assets` -> `item_assets` -> icons) plus 10.x assets, with an audit per batch; S-M per
batch, cosmetic only.

### Other QoL

Several named QoL features are already 1.2-native and should not be re-ported:

| Feature | Where it already works |
| --- | --- |
| Auction keyword, category A/B/C, grade, min/max item level, sort and page | `systems/economy/auction.rs:190-202`, filters at `:1282-1336` |
| Bag and bank sorting | Client-local sort, no packet (`docs/aa-net.md:1067-1076`, `docs/aa-client-hud.md:1156`) |
| Mail take-all | `systems/economy/mail_packets.rs:43`; Bevy `aa-net/src/mail.rs:157,246` |
| Loot-all | `systems/economy/looting.rs:607-641` |
| Quest tracker, map pings, character select | 1.2/Bevy already |

Later-only QoL:

- **UI scale.** 10.x has `SetUIScale`/`GetUIScale`/`ApplyUIScale` and `ui_scale`
  (`out-10x-client-x2game-strings.txt:27942-27944,28051,30825,37522`). 1.2 fixes UI units
  (`docs/aa-ui.md:179-181`) and Bevy does not apply it yet (`README.md:90`); client-only, S, default 1.0.
- **Saved ability sets / dual spec.** 10.x has `GetSavedSkillSet`, `Save/Delete/SelectAbilitySet` and
  SCAbilitySet packets (`out-10x-client-x2game-strings.txt:30947-30978,47895-47897,48814-48818,
  51480-51482`). 1.2 has only ability swap. Needs custom state, packets and a Bevy window (M-L);
  opt-in, no balance change, worth it once 55 makes builds matter.
- **Minor 10.x-only extras** are not needed: bulk auction posting, server inventory sort, auto-loot and
  claim-all (`out-10x-client-x2game-strings.txt:39466,41172,41276,41549`) are covered by local sort.

## Asset feasibility

- **Pak format.** 1.2 `game_pak` has 218,066 files; 3.0.4.2 is 36.8 GB/301,369 and opens with the
  repository `aapak` reader (`D:/aa-re/aac/INVENTORY.md` section 2); the 10.x pak is 68.5 GB with
  523,291 entries in `pak10.txt`, read by `D:/aa-re/zone/feasibility/paktool` over
  `aaemu_commons::aapak`. Bevy's `aa-pak` reads the same layout (`crates/aa-pak/src/lib.rs:3-6,
  73-79`) but is wired to one pak (`Pak::global()`, `AA_PAK`); a second path is S.
- **Proven compatibility.** The gear backport writes 10.x `.cgf`/`.dds` (185 files, 15 armor looks)
  into a 1.2 pak copy and the 1.2 client reads them (`PORTING.md:2314-2320`), a safe source for Bevy.
- **Missing assets.** Baby dragon, pegasus, snowlion, stormstrider and orange donkey are in 1.2;
  carrot donkey 35302, pirate/daru/cow dolls, landsnails and black puppy are 10.x-only. New cars and
  ships use prefabs, which Bevy does not assemble
  (`crates/aa-client/src/ingame/data.rs:169-180,344-363`).
- **Risk.** 1.2/3.0 share an engine generation; 10.x animations and effects may be newer. Test new
  `.caf`/`.dba` files and effect libraries per model.
- **Licensing and disk.** Assets belong to XL Games/Kakao; the repo license covers code only, and pak
  redistribution is a separate decision. Never copy the 68 GB pak; extract only the audited closure.

## Order of work

1. **Level 55** (S): flip the ten `SHOW` flags, patch the 3.0 51-55 XP and quest exp, sync Bevy's
   level table, test all ten capstones and the library. No server code.
2. **UI scale** (S, client-only) and the **first costume batches** (S-M each).
3. **Farming/housing UI plus the planting snap** (M-L; the snap is client-only), the first client
   content gap.
4. **Auction window** (M): the existing 1.2 filters, not the 10.x category tree; a client gap.
5. **Transport**: local mount control (L), then data-only 3.0 mounts (S), then vehicles and 10.x-only
   mounts.
6. **Saved ability sets** (M-L) after 55; **not doing**: level-55 gear, 10.x inflation, Abyssal skills.

## Remaining open questions

The decisions answered the XP curve, level-55 scope, 55 gear, costume volume and client order; these remain open:

- **Pak distribution.** Merge audited 10.x files into a 1.2 pak copy shipped with the client, or
  support a second pak path? Will the patched pak be redistributed, or kept local?
- **Saved ability sets protocol.** Custom opcodes for Bevy only, or server-command-only? Is it wanted
  at all, or is ability swap enough at 55?
- **Costume batches.** Which batch comes first, and should batches follow the 3.0 set or the
  10.x-only assets? "All later-version" fixes volume, not order.
- **Planting enforcement.** Client-side snap only, or also a server-side overlap check (M)?
- **Bevy level source.** Load `levels` from `AA_DB` at startup, or keep the hardcoded table?

## Sources

Server paths are under `rust/crates/aaemu-game/src/` unless stated otherwise; Bevy paths are under
`D:/aaemu-client/AABevy1.2`; reference data and string dumps are under `D:/aa-re/`.

- Server: `data/experience.rs:6-8`; `data/mod.rs:155`; `managers/db_character.rs:668-685`;
  `systems/skills/progress.rs:433-483`; `systems/skills/skill.rs:88`;
  `systems/npcs/doodad/manager.rs:393,423-463,536-611`;
  `systems/economy/auction.rs:190-202,1282-1336`; `systems/economy/mail_packets.rs:43`;
  `systems/economy/looting.rs:607-641`; `PORTING.md:2314-2320`.
- Server data: `rust/crates/aaemu-game/app/Data/compact.sqlite3` (`levels`, `skills`, quests, NPCs,
  items, armors, housings, farms) and `app/Data/Worlds/instance_library/{npc_spawns,doodad_spawns}.json`.
- Bevy: `crates/aa-net/src/model.rs:48,51-57`; `crates/aa-client/src/hud/skillbook.rs:100,107`;
  `crates/aa-pak/src/lib.rs:3-6,73-79`; `crates/aa-client/src/ingame/data.rs:169-180,344-363`;
  `docs/aa-client-units.md:52-63,93`; `docs/aa-ui.md:179-181`; `docs/aa-net.md:1067-1076`;
  `docs/aa-client-hud.md:1156`; `docs/research/remaining.md`; `README.md:74-79,90`.
- Reference: 3.0.4.2 `compact.sqlite3` and `compact.server.table.sqlite3`; the decrypted 10.x DB;
  `client12/out-10x-client-x2game-strings.txt`; `zone/bin/x2game-dev_dedicate.dll.strings.txt`;
  `zone/feasibility/pak10.txt`, `pak12.txt`, `paktool/src/main.rs`; `aac/INVENTORY.md`.
- Tooling/provenance: `rust/tools/version_gaps/backport.py`, `gaps.py`, and the 2026-10-20 static review.

## Related

- [Home](Home)
- [Client](Client)
- [Server Backlog](Server-Backlog)
- [Player Bots](Player-Bots)
- [Fork Deviations](Fork-Deviations)
- [Dungeons](Dungeons)
- [Classes](Classes)
- [Documentation Maintenance](Documentation-Maintenance)

## Sunbite Wilds pilot (optional area)

The first optional-area package makes the 1.2 client's dormant Sunbite Wilds 3+4 live. The 3.0.4.2
client is now the source: its `game_pak` has no `zone_server/npc_spawners.g`, but it keeps the retail
placement map at `game/worlds/main_world/map_data/npc_map/e_sunny_wilderness.dat` (16-byte records:
`u32 npc id`, `f32 x`, `f32 y`, `f32 z`). That file, the 3.0 `world.xml`, and the 3.0
`compact.sqlite3` / `compact.server.table.sqlite3` provide all content. 10.x is not used.

Important: the 3.0 client re-terraformed and moved these zones. Its `world.xml` puts `zone_key`
286/287 at origin `(21, 4)` with six sector-owning cells (21/22,4-6), while the 1.2 `world.xml`
declares them at `(18, 4)`/`(18, 5)` and their cells do not match the 3.0 heightmaps. A stable
1.2-terrain package is therefore impossible: the package ships a **terrain overlay**.

| Content | Count | Source |
| --- | ---: | --- |
| Zones (`e_sunny_wilderness_3` / `_4`) | 2 | 3.0 `zones` (ids 242 / 227, keys 286 / 287; group 13 reuses the 1.2 row) |
| Quests at level <= 55 | 78 | 3.0 `quest_contexts.zone_id = 227` (levels 2-29), 338 components / 465 acts |
| NPC templates imported | 141 | 3.0 `npcs` (levels 3-50); 31 colliding 3.0 ids remapped into `30000000+`; 6 use a 1.2 model as a visual fallback |
| NPCs placed | 148 | 3.0 `npc_map`; 449 placements (362 in 286/287, the rest are quest givers/talk NPCs in 157/190) |
| Spawner templates | 148 | 7 safe 1.2 one-member templates reused, 141 one-member synthetic templates (100000000+) so every placement pins exactly one NPC |
| Item templates (new) | 102 | 3.0 `items`; all level 1, no new id >= 8,000,000 |
| Doodad templates (new) | 36 | 3.0 `doodad_almighties` referenced by the quests (no placements exist in the pak) |
| Loot | 13 packs / 34 rows | 3.0 server `loot_pack_dropping_npcs` / `loots` / `loot_groups`, filtered to items that exist |

**Terrain overlay (`Data/Areas/sunbite-wilds/sunbite-wilds.pak`)**

- 3.0 cells `021_004`, `021_005`, `021_006`, `022_004`, `022_005`, `022_006`: all 73 files
  (heightmap, object, vegetation, cover, ...), 33,376,333 bytes of source data; the packed overlay
  is 33,402,368 bytes (~31.9 MiB).
- A merged `world.xml` replaces the zone nodes 157, 190, 286 and 287. 157/190 are needed because
  3.0 places the early Sunbite quest NPCs there; their **terrain stays 1.2** (only the zone
  boundaries come from 3.0). `world.g` is unchanged (1.2 already lists 286/287).
- The pak is generated, never committed:
  `python -I rust/tools/area_package/build_sunbite.py ... --build-overlay
  rust/crates/aaemu-game/app/Data/Areas/sunbite-wilds/sunbite-wilds.pak`.
- Server: the package descriptor lists it under `ClientSources`; `Areas:Enabled` prepends it to
  `ClientData.Sources`, first source wins. Bevy: copy/symlink it into the base pak's `overlays/`
  folder or add it to `AA_PAK_OVERLAYS`.
- Measured 3.0-vs-1.2 terrain difference (raw heightmap nodes): cell 021_004 max 199.8 m (212,530
  of 278,784 samples differ by >3 m), 021_005 max 167.8 m (220,656), 021_006 max 99.2 m (53,174);
  the 022_004/005/006 node layouts differ too. At the 362 placements in 286/287 the authored Z was
  within 3 m of the 3.0 terrain for 343 (94.8%).

**Placement validation (offline, server heightmap sampler)**

- 511 candidate placements; 378 (74%) authored Z within 3 m of the server terrain. Every final
  placement is **snapped to the sampled terrain** (509 candidate entries after dropping 2 unplaceable NPCs, then 449 unique placements after deduplicating repeated npc_map records),
  so the package is 100% on-ground by construction.
- 133 placements moved by >3 m when snapping, 19 by >50 m (interiors/cliffs); 0 in water; 19 on
  slopes >45 deg. 0 needed sector re-anchoring after merging the 157/190 zone nodes.
- The validation command uses the ignored `dump_terrain_heights` test with the tool's
  `--overlay-dir` scratch source; the generator refuses to write output without `--heights`.

**Quest reachability**

- 78 of 82 candidate quests remain. Quests 8152-8155 are skipped because their start/end NPCs have
  no 3.0 `npc_map` record. Every kept quest's accept/report NPC is placed.
- NPC levels 3-50. The six level-50 NPCs are 3.0 guards/service NPCs (guard, farm manager, priest,
  auctioneer, merchant recruiter), not 10.x data.
- 2 3.0-only NPCs (17077/17079, model 2118 absent from the 1.2 client) have no placement and are
  not imported. 6 colliding 3.0 NPCs whose models are absent from 1.2 keep a 1.2 model as a visual
  fallback; their 3.0 level/stats are kept.

`cargo xtask compact-patches` applies the patches twice in the ignored
`sunbite_area_package_real_data` test and then loads the zone, quest and full subsystem data with
`Areas:Enabled` on and off. The overlay itself is checked by
`sunbite_overlay_pak_is_read_first` (`AAEMU_OVERLAY_PAK` + `AAEMU_GAME_PAK`).
