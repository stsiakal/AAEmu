# 10.x zone server: the aggro table and the NPC spawner engine

Function-level map of the retail ArcheAge **10.0.2.13 r575** zone server (`x2game-dev_dedicate.dll`,
image base `0x39000000`), for comparison with ArcheAge 1.2 behaviour and with our Rust world server.
This is the zone-side half of what AAEmu implements itself: retail splits these systems between a
**world server** (units, stats, damage, skills, persistence — our role) and a **zone server**
(physics, AI, aggro, spawning). Everything here is derived from the shipped binary and its
decompile; the published CRYENGINE source was not consulted.

**Provenance**

| | |
|---|---|
| Zone logic decompile | `D:/ArcheAge Returns 10.0.2.13 - 8yx - r575 - 2026-06-18/Bin64/decompiled/x2game-dev_dedicate.dll.c` ("DD.c", 3,184,397 lines) |
| Function index / sizes | `D:/aa-re/zone/bin/x2game-dev_dedicate.dll.funcs.csv` (89,574 functions; sizes below are `end-start` from it) |
| Strings / RTTI | `D:/aa-re/zone/bin/x2game-dev_dedicate.dll.strings.txt`, the `X2::*::` log prefixes in DD.c |
| 10.x server DB (read-only) | `D:/aa-re/db-references/10.0-server/server_decrypted.sqlite3` (1,374 tables) |
| 1.2 DB (read-only) | `C:/Users/stsiakal/AAEmu/rust/crates/aaemu-game/app/Data/compact.sqlite3` |
| Prior studies (built on, not redone) | `D:/aa-re/zone/feasibility/FEASIBILITY.md`, `D:/aa-re/zone/ai/AI-ORACLE.md`, `D:/aaemu-wt/zone-map/rust/docs/physics/ZONE-PHYSICS-USAGE.md`, `D:/aaemu-wt/zone-map/REVERSING.md` |
| Rust comparison | `rust/crates/aaemu-game/src/systems/ai/`, `rust/crates/aaemu-game/src/systems/npcs/`, `rust/crates/aaemu-game/src/systems/skills/` in this worktree |

**Conventions.** `sub_<VA>` is the decompiler name; the VA is also the function address (all
addresses are VAs in the zone DLL). A name in quotes is the binary's own log string
(`X2::GameServer::NpcAggroTable::AddAggro`); a name without quotes is inferred from the code and
marked **(inferred)**. Confidence tags used inline: **[V]** read in code, **[I]** inferred from code
shape/constants, **[?]** unresolved. `DD.c:line` is given where a line number is more useful than
an address.

**Every mismatch with our Rust server is a SUSPECT, not a bug** (same rule as `AI-ORACLE.md`).
The zone is 10.x, our target is 1.2, and the 1.2 zone binary was never shipped; a difference can be
10.x-only, a C#-era invention in our code, or a real parity break. Section 4 ranks the differences
worth reproducing against the running zone.

Scope: (A) `NpcAggroTable` and target choice, and (B) the `npc_spawners`/`npc_spawner_npcs`/
`npc_groups`/`npc_group_members`/`npc_spawner_*_effects` engine. Not covered: skill picking,
pathing, physics (see `ZONE-PHYSICS-USAGE.md` and `AI-ORACLE.md`).

**Function count:** 77 functions in the detailed tables (A: 44, B: 33) and 123 in
`functions.csv` including the supporting entries (A: 62, B: 61).

---

## 0. Summary

* **Aggro.** The zone owns a per-NPC table of up to **100** entries (`NpcAggroTable`, 240 bytes,
  ctor `0x3931EF20`), re-sorted whenever the entry set changes so `entry[0]` is the current target. Entries carry three
  integer channels (`damage`, `heal`, `etc` in our terms), an NPC-side multiplier (stat 62), a
  distance coefficient (0.909 below 10 m, 0.833 at or above), a taunt timestamp, both the pull
  position and the target position, creation and last-hit times. The world feeds it with
  `WZUpdateAggro` (external aggro, channel 2) and `WZUnitDamaged` (damage, channel 0); the zone
  adds targets it perceives itself (`Npc::FindNewTargetIfItNeed`), replicates entries to NPC group
  members and along NPC aggro links, and answers with `ZWChangeTarget`, `ZWAggroRemove`,
  `ZWRegisterNpcAbuser`, `ZWMakeAggroTargetHostile` and the periodic `ZWAiAggro` debug dump.
  **The 1.2 data carries every column this code reads** (`npcs`, `npc_groups`, `npc_ai_params`,
  `buffs.taunt*`), so the behaviour transfers; the 1.2 Lua drove the same signals and thresholds.
* **Spawners.** Placement comes from the zone's own `npc_spawners.g` (`spawnAreaType` `point` or
  `area`, point list or triangle list, per-path names), while all *rules* come from the DB:
  `npc_spawners` (population, delays, start/end/destroy time, activation, save-indun),
  `npc_spawner_npcs` (member Npc/NpcGroup + weight), `npc_groups` + `npc_group_members`
  (leader, formation offsets), `npc_spawner_spawn_effects` / `npc_spawner_despawn_effects`.
  Runtime: a per-spawner priority queue, one spawn per update, weighted member choice biased to the
  least-represented member, population between `min_population` and `maxPopulation`, spacing tests
  around `test_radius_npc`/`test_radius_pc`, group spawning in formation with a move-leader, and
  respawn once the whole group is gone. Both halves exist in 1.2 except the `.g` placement data (the 1.2 pak
  has no `zone_server` directory, `FEASIBILITY.md` §4) and `use_summoner_faction` (missing column in
  1.2).
* **Our Rust server** has the same three aggro channels and the same spawner tables, but the aggro
  *rules* come from the C# AAEmu port (immediate leash, raw-total target sort, player-scaled
  spawner population) and are **not** the zone's rules. The spawner engine is point-spawner only
  and has no group formation. Section 4 lists the concrete suspects.

---

## 1. The zone/world split for these systems

| Data / action | Where |
|---|---|
| Unit ids, stats (incl. stat 61/62/90), damage, heal, skill use | world (we implement it) |
| Aggro **values** on damage and on external effects | world sends `WZUnitDamaged` (`0x39363580`) and `WZUpdateAggro` (`0x39363890`) |
| Aggro **table**, target choice, leash, alerts, group/link share | zone (`NpcAggroTable`, `Npc::FindNewTargetIfItNeed`) |
| Target change reports | zone sends `ZWChangeTarget` (23), `ZWAggroTargetChanged` (46), `ZWAggroRemove` (65), `ZWRegisterNpcAbuser` (61), `ZWMakeAggroTargetHostile` (13), `ZWAiAggro` (52) |
| Spawn rules (population, delays, groups, effects) | DB, loaded by the zone (`npc_spawners` etc.) |
| Spawn **placement** (points/triangles/paths) | zone level design `npc_spawners.g` (10.x only) |
| NPC creation from a spawn request | world; the zone sends `ZWSpawnNpc` (2) and receives `WZNpcState` (2) |
| NPC removal | zone decides (`NpcSpawner::RemoveSpawnedNpcs`, `0x39346050`) and sends `ZWRemoveNpc` (3); the world deletes the unit |
| Despawn effect buff | zone sends `ZWCreateBuff` (10) with a hardcoded buff id (`0x39346050`) |

The 10.x zone **does not compute damage**; it consumes the result. So the *formulas* in
`skills::combat` cannot be validated against the zone — only the aggro bookkeeping and the spawner
rules can.

---

# Part A — the aggro table

## A.1 Object layouts

### A.1.1 `NpcAggroTable` (240 bytes = `0xF0`; allocated at `0x39346960`, ctor `0x3931EF20`)

| Off | Type | Meaning | Evidence |
|---|---|---|---|
| +0 | u32 | group id (`dword_3A606298` = none; set to the spawn group id when the spawner member is an `NpcGroup`) | ctor `0x3931EF20`; `0x39346960` (`*(v36) = *(v12+4)`) |
| +8 | ptr | owner NPC unit | ctor |
| +16 | f32 | `return_distance²` (from `npcs.return_distance`, desc `+212`) | ctor args (`a3*a3`); `0x39346960` |
| +20 | f32 | `absolute_return_distance` (`npcs.absolute_return_distance`, desc `+4`) | ctor; `0x39346960` |
| +24 | f32 | `absolute_return_distance²` | ctor |
| +32/+40/+48 | vector | `Aggro` entries (64 bytes each), max 100 (`0x1900/0x40`) | `CreateAggro 0x3931F040`, `RemoveAll 0x393200D0`, `DD.c:821329` |
| +64 | map | unit id → saved `Aggro` (sleep-status stash) | `0x393204B0`, `0x3931FF80` (`sub_39AEFDF0`) |
| +72/+80 | map node/count | map sentinel/count | `0x393200D0` |
| +96 | u32 | current top target unit id (`-1` = none) | `0x3931F790`, `_OnAggroTableChanged 0x3933BA80` |
| +100/+104/+108 | u32 | top target of the other two channels (init `-1`) | ctor (`dword_3A6062A8`) |
| +112 | f32 | `minAPS` floor (-1.0 = off) | ctor (`-1082130432`), `AddAggro`, `ProcessAggroCancel` |
| +116 | u8 | dirty flag (entry set changed) | `CreateAggro`, `CopyTopAggro`, `Remove*`, `0x3931F790` |
| +117 | u8 | mutation lock / "emptied" flag: every mutator early-outs while set; set when a removal empties the vector, cleared once per frame | all mutators; `0x3931F790` tail |
| +120 | u32 | last removal reason (0-17) | `RemoveFromTable`, `0x3931F790` |
| +124/+128/+132 | f32 | combat-start position (first entry's pull position) | `CreateAggro`, `ProcessReturn` |
| +136/+168/+200 | 3 × `std::set<u32>` (32 B each) | abuser sets, indexed 0/1/2; only 0 and 2 are written by the code | ctor (`eh vector ctor` 3×0x20), `0x3931E180`, `0x3931DD40` |
| +232 | u32 | last abuser-report time (1 s throttle) | ctor, `0x3931E240` |

### A.1.2 `Aggro` entry (64 bytes)

| Off | Type | Meaning | Evidence |
|---|---|---|---|
| +0 | i32 | channel 0 — **damage** aggro (`OnAffectedByDamage` writes it via `{amount,0,0}`) | `0x3933C8D0`, `AddAggro` |
| +4 | i32 | channel 1 — **heal** aggro (no zone producer found; `WZUnitHealed` path [?]) | `AddAggro` |
| +8 | i32 | channel 2 — **etc** aggro (external `WZUpdateAggro`, aggro-link adds `{0,0,1}`) | `0x39322500`, `0x39321C80` |
| +12 | u8 | flags (a byte OR'd with `byte_3A5177F0` while the entry is swapped during sort) | `0x3931EB70` |
| +16 | f32 | NPC aggro multiplier, `(100+stat62)/100` | `CreateAggro`, `UpdateAggro 0x3931FD30`, `GetAggroMultiplier 0x3931CF00` |
| +20 | f32 | distance coefficient: `0.90909088` if squared distance < 100, else `0.83333331` | `GetDistanceCoefficient 0x3931D6F0` |
| +24 | u32 | taunt timestamp (ms), 0 = not taunted; sorts the entry first | `TauntedBy 0x39320FE0`, `0x3931EB70` |
| +28 | u32 | unit id (key) | all entry loops (`entry[7]`) |
| +32/+36/+40 | f32 | position recorded at creation = **NPC/group average position** (pull position) | `CreateAggro`, `ProcessAggroCancel` method 0 |
| +44/+48/+52 | f32 | **target position** recorded at creation | `CreateAggro`, `ProcessAggroCancel` method 1 |
| +56 | u32 | creation time (ms) — denominator of the APS average | `CreateAggro`, `GetAggroPerSecond 0x39322950` |
| +60 | u32 | last-hit time (ms) | `AddAggro`, `ProcessAggroCancel`, `0x39322950`? (APS uses +56, see below) |

`GetAggroPerSecond` (`0x39322950`) computes
`aps = (ch0+ch1+ch2) * mul(+16)`, divided by `(now - creation)/1000` when the entry is older than
1 s (**not** by last-hit age). That detail matters for the `minAPS` rule.

### A.1.3 NPC descriptor offsets used by this code

The `npcs` SELECT is at `DD.c:2426766`. Descriptor offsets are the in-memory layout after load;
the DB column is named where it can be tied to a column unambiguously.

| Off | Field (column) | Evidence |
|---|---|---|
| +4 | `absolute_return_distance` | `0x39346960` |
| +8 | `aggro_link_help_dist` | `0x39321C80` |
| +12 | `aggro_link_special_rule_id` | `0x39321C80` |
| +152 | `name` (used in logs) | `0x393211E0`, `0x3931D120` |
| +168 | `npc_grade_id` (3/4/5/8 = boss) | `0x393210B0`, `0x3931F790`, `0x393211E0` |
| +212 | `return_distance` | `0x39346960` |
| +656 | flag byte: bit 1 `accept_aggro_link`, bit 2 aggro-at-spawn, bit 4 `aggro_link_sight_check`, bit 5 `aggro_link_special_guard`, bit 6 `aggro_link_special_ignore_npc_attacker` | `0x39321C80`, `0x39346960` |
| +657 | bit 0x10 `check_target_under_terrain` | `0x393204B0` |
| +659 | bit 0x10 `return_when_enter_housing_area` | `0x39320440` |
| +660 | sign bit `use_abuser_list` (gates abuser registration) | `0x3931DDC0` |

The `npcs` loader that fills them is `DD.c:2426766` (10.x) / `DD.c:2445689` (second loader).
The descriptor **offsets** are read directly from the functions listed; the column→offset/bit packing
is inferred from the column names (and agrees with `AI-ORACLE.md` §2.4-2.6).
**The 1.2 `npcs` table has all of these columns** (checked with `PRAGMA table_info(npcs)` on
`compact.sqlite3`: `return_distance`, `absolute_return_distance`, `accept_aggro_link`,
`aggro_link_help_dist`, `aggro_link_sight_check`, `aggro_link_special_rule_id`,
`aggro_link_special_guard`, `aggro_link_special_ignore_npc_attacker`, `npc_grade_id`,
`attack_start_range_scale`, `sight_range_scale`, `aggression` — all present).

## A.2 Function map

Sizes are `end-start` from `funcs.csv`. The "1.2?" column is the transfer verdict: **yes** = the 1.2
DB columns/data and the 1.2 Lua support this behaviour and it should be reproduced; **likely** = the
data exists but the exact 1.2 implementation is unverifiable; **unknown** = no 1.2 counterpart we can
check; **no** = a 10.x protocol/format feature that cannot be transferred verbatim.

### A.2.1 Table lifecycle and scoring

| Address | Size | Name | Callers → key callees | What it does | 1.2? |
|---|---|---|---|---|---|
| `0x3931EF20` | 267 | `NpcAggroTable::NpcAggroTable` [V class] | `0x39346960` → `0x394A2F40`, vector ctor | Initialises the 240-byte table: distances from the npc desc (`return_distance`, `absolute_return_distance`), combat-start pos sentinel, `-1` tops, `minAPS=-1`, 3 abuser sets. | yes |
| `0x3931F040` | 890 | `NpcAggroTable::CreateAggro` [V] | `AddAggro`, `UpdateAggro`, `CopyTopAggro`, `TauntedBy`, `0x3931FA10` → `0x3931E000`, `0x39261710`, `0x3931D6F0`, `0x3931D650`, `0x39362CA0` | New entry: refuses self (`0x821329`), group members (`0x3931E000`), invalid targets; builds the 64-byte entry (channels 0, unit id +28, npc multiplier, distance coefficient, pull position, target position, times); on the **first** entry stores the pull position at +124..+132; caps at 100 entries by dropping the last entry; sends **`ZWMakeAggroTargetHostile` (13: npc uid, target uid)**; notifies. | yes |
| `0x39321340` | 492 | `NpcAggroTable::AddAggro` [V] | `0x39321740`, `0x39321540`, `0x39321A10`, `0x39345770`, `0x39346960` → `0x3931DD40`, `0x3931F040`, `0x39322950`, `0x3931D6F0`, `0x3931FCE0`, `0x3931E240` | Adds `int[3]` to an existing entry (creating it if absent), each scaled by the **attacker's** stat 90 `(100+stat90)/100`; updates last-hit (+60); a brand-new entry whose APS is below `minAPS` is removed at once (reason 9); recomputes the distance coefficient when the total is non-zero; marks dirty and notifies. Early-outs when the table's mutation lock (+117) is set, and skips units already registered in an abuser set (`0x3931DD40`). | yes |
| `0x3931FD30` | 192 | `NpcAggroTable::UpdateAggro` [V] | `AddAggro`/`CopyTopAggro` → `0x3931CF00`, `0x3931FCE0` | Refreshes an existing entry's multiplier (+16) from the NPC's current stat 62. Covers buff-driven `AggroMul` changes that happen after the entry was created. | yes |
| `0x3931FE00` | 282 | `NpcAggroTable::CopyTopAggro` [V] | `AddAggro`, `TauntedBy` → `0x3931F040`, `0x3931FCE0` | Raises the entry's channel 2 so its total equals the current maximum total in the table, i.e. makes this unit the top target without disturbing channels 0/1. | yes |
| `0x39320FE0` | 189 | `NpcAggroTable::TauntedBy` [V] | `0x3944FEB0` (buff manager, `TauntBuffFinder`) → `CopyTopAggro` | Stamps +24 with `now`; with `taunt_with_top_aggro` also calls `CopyTopAggro`. The sort puts taunted entries first, so a taunt wins regardless of value. | yes (`buffs.taunt`, `buffs.taunt_with_top_aggro` in 1.2) |
| `0x3931F710` | 112 | `NpcAggroTable::CancelTauntedBy` [V] | `0x394501E0` (buff manager) | Clears the taunt timestamp (+24). | yes |
| `0x39322950` | 87 | `GetAggroPerSecond` **(inferred)** | `AddAggro` | `(sum channels) * mul(+16)`, averaged over `now-created` when the entry is older than 1 s. | likely |
| `0x3931CF00` | 52 | `GetAggroMultiplier` **(inferred)** | `UpdateAggro` | `(100 + stat62) / 100` of the NPC (stat 62 = `IncomingAggroMul` in our `statkind`; the zone uses it as the per-entry aggro scale). | yes |
| `0x3931D6F0` | 244 | `GetDistanceCoefficient` [V class strings] | `CreateAggro`, `AddAggro` → `0x392589F0`, `0x3931D650` | 3D distance from the NPC/group average position to the target; `0.90909088` if `dist² < 100`, else `0.83333331`. | unknown (not in 1.2 Lua) |
| `0x3931D650` | 146 | `NpcAggroTable::GetGroupAveragePosition` [V] | `CreateAggro`, `0x3931D6F0`, `0x39320C90` → IAISystem vt+240/56 or `0x392589F0` | Average position of the AI group the table belongs to (+0), or the owner NPC's position when there is no group. | likely (1.2 `npc_groups`) |
| `0x3931E2F0` | 627 | `NpcAggroTable::CollectSharedAggroMembers` [V] | `0x3931F790`, `ProcessAggroCancel` → IAISystem vt+240/128, `0x39340D90`, `0x39324FF0` | Fills a list with the units of the table's group (via the AI system), falling back to the owner NPC. Used by the cancel rule ("is this abuser still valid?"). | likely |

### A.2.2 Target selection, notification and removal

| Address | Size | Name | Callers → key callees | What it does | 1.2? |
|---|---|---|---|---|---|
| `0x3931F790` | 634 | `RefreshTopTarget` **(inferred; per-frame housekeeping)** | `0x3933B490` → `0x3931EB70`, `0x3931E2F0`, `0x3933BA80`, `0x39362D00` | If +116 is dirty, sorts the vector (`0x3931EB70`). Reads `entry[0].unitId`; when it changed, stores it at +96 and calls `Npc::_OnAggroTableChanged` for every shared member (Lua `OnAggroTargetChanged`/`OnNoAggroTarget`). When the table became empty and the NPC is **boss grade (3/4/5/8)**, sends `ZWAggroRemove` (65: old target, npc uid, last reason) and logs `AGGRO => Npc(...) will be return`. Clears +117. | yes |
| `0x3931EB70` | 931 | `SortEntries` **(inferred)** | `RefreshTopTarget` → `0x39325100`, `0x393229B0` | In-place sort of the 64-byte entries: taunted entries first (compare +24), then by `(ch0+ch1+ch2) * mul(+16) * distcoef(+20)`. Result: `entry[0]` is the target. | unknown (the 1.2 Lua never sorts; the 1.2 zone did) |
| `0x3933BA80` | 265 | `Npc::_OnAggroTableChanged` [V] | `RefreshTopTarget` → `0x3933B970` | Raises `OnNoAggroTarget` when the table is empty, otherwise resolves the top unit and raises `OnAggroTargetChanged` with the AI event value. This is the Lua-visible target-change signal. | yes |
| `0x3935CCC0` | 237 | `Npc::SetTargetAndNotify` **(inferred)** | `0x392D7100`, `0x39326B40` → `0x39362910`, `0x39362B00` | Sets the NPC's target and sends **`ZWChangeTarget` (23: npc uid, target uid)**; when the change is aggro-driven (arg 2) and the target is a character, also sends **`ZWAggroTargetChanged` (46)**. | no (protocol) / yes (semantics) |
| `0x393210B0` | 293 | `NpcAggroTable::RemoveFromTable` [V] | `AddAggro` (minAPS), `0x3931FA10` (invalid), AI system | Stores the reason (+120) and removes the entry. Sends **`ZWAggroRemove` (65: unit, npc uid, reason)** only when the NPC is a boss grade (3/4/5/8) and the reason is < 18. | yes (the report is 10.x protocol) |
| `0x3931FF80` | 256 | `RemoveEntryAt` **(inferred)** | all removers → `sub_39322AB0`, `0x3931FA10` | Swap-removes the entry, drops its sleep-stash copy, clears taunt bookkeeping, marks dirty and notifies. **No packet.** | yes |
| `0x393200D0` | 864 | `NpcAggroTable::RemoveAll` **(inferred)** | `ProcessReturn` (`0x39320D20`, `0x39320C90`, `0x39320440`, `0x39320DF0`) → `0x3931FF80` | Records the reason when the table is non-empty, logs the whole target list for boss grades (`Removing all aggros...`), removes every entry and clears the sleep stash. | yes |
| `0x39322850` | 177 | `GetAggroRemoveReasonName` [V] | `0x3931F790`, `RemoveFromTable` | Maps reasons 1-17 to `not combat area`, `absolute dist`, `faction changee`, `in housing`, `can't find unit`, `world forced`, `fake death`, `out of range`, `err aggro num`, `group aggressive`, `persistent stick`, `ai delete aggro`, `AI skill time over`, `GM`, `npc die`, `destructor`, `special effect(aggro reset)`. | yes |
| `0x3931E240` | 158 | `UpdateAbuserRegistration` **(inferred)** | `AddAggro`, `UpdateAggro`, `CopyTopAggro` → `0x3931DDC0`, `0x3931D890`, `0x3931D000`, `0x3931E180` | At most once per second per table: for a unit already flagged as an abuser, re-reports channel 0 if it is the visible top target and channel 2 if it is under the terrain. | likely |
| `0x3931E180` | 181 | `RegisterAbuser` **(inferred; `ZWRegisterNpcAbuser`)** | `0x3931E240`, `ServerAIUnitSystem::RegisterAbuser 0x39327360` → set insert, `0x39362CD0` | Inserts the unit into one of the 3 sets (+136/+168/+200) and sends **`ZWRegisterNpcAbuser` (61: npc uid, unit uid)**. | likely |
| `0x3931DD40` | 112 | `IsUnitInAbuserSet` **(inferred)** | every table mutator | Searches the 3 sets; a hit makes the mutator skip the unit. | likely |
| `0x3931DDC0` | 231 | `CanRegisterAbuser` **(inferred)** | `RegisterAbuser`, `UpdateAbuserRegistration` | Returns true when the unit is **not yet** in any set, gated by CVar `qword_3A607D58`, `use_abuser_list` (`npcs` +660 sign bit), unit type 0 (character) and a non-empty table. | likely |

### A.2.3 Cancellation and leash

| Address | Size | Name | Callers → key callees | What it does | 1.2? |
|---|---|---|---|---|---|
| `0x393204B0` | 2000 | `NpcAggroTable::ProcessAggroCancel` [V] | `0x3933B490` (every 8th frame) → `0x3931E2F0`, `0x39261710`, `0x392589F0`, `0x3931D650`, `0x3931FF80` | Per-entry removal (no packet): (a) unit missing/invalid; (b) unit is not hostile to any shared group member; (c) sleep status saves/restores channels (+64 map, channels become `{0,0,1}`); (d) APS below `minAPS`; (e) `check_target_under_terrain` and target > 1 m below the terrain for > 2000 ms since last hit; (f) **leash**: `return_distance² > 0.1` and either the NPC/group position now vs the pull position (+32) [method 0] or the target now vs the target position (+44) [method 1] is farther than `return_distance`, **and** more than `npc_aggro_cancel_timeout` (CVar, default 3000 ms) since the last hit. | yes (columns + `buffs.sleep`) |
| `0x393211E0` | 335 | `NpcAggroTable::ProcessReturn` [V] | `0x3933B490` (every 64th frame) → `0x39320D20`, `0x39320C90`, `0x39320440`, `0x39320DF0` | Whole-table clear: bosses (`npc_grade 3/4/5/8`) beyond `2 × absolute_return_distance` from their **spawn position**; otherwise all grades beyond `absolute_return_distance` from the **combat-start position** (+124..+132); then housing-area and "not combat area" tests. | yes |
| `0x39320D20` | 187 | `BossAbsoluteReturnCheck` **(inferred)** | `ProcessReturn` → `0x3933C9C0`, `0x393200D0` | If `2 × absolute_return_distance > 0.1` and the NPC is farther than that (squared) from its spawn position, log `AbsoluteReturnDistance` and `RemoveAll(reason 2)`. | likely |
| `0x39320C90` | 128 | `AbsoluteReturnCheck` **(inferred)** | `ProcessReturn` | If `absolute_return_distance² > 0.1` and the NPC is farther than that from +124..+132 (combat-start), `RemoveAll(reason 2)`. | yes |
| `0x39320440` | 96 | `HousingAreaCheck` **(inferred)** | `ProcessReturn` → IEntitySystem vt+360, `0x393200D0` | With `return_when_enter_housing_area`, clears all aggro when the NPC is inside a housing area (reason 4). | likely (10.x housing areas) |
| `0x39320DF0` | 106 | `CombatAreaCheck` **(inferred)** | `ProcessReturn` → `0x39340510`, `0x393200D0` | Clears all aggro when the NPC's name is not in a "combat area" (reason 1). | unknown |

### A.2.4 Feeding the table and choosing targets

| Address | Size | Name | Callers → key callees | What it does | 1.2? |
|---|---|---|---|---|---|
| `0x39363890` | 161 | `WZUpdateAggro` handler **(inferred)** | ZoneClient dispatch (slot 68) → `0x39322500` / `0x39321830`, `0x3931D0B0` | Fields: npc unit id (+16), source unit (+20), abuser unit (+24), amount (+28), hostile flag (+32), aggro-source descriptor (+36, 16 B) + description. Hostile → `0x39322500`, friendly → `0x39321830`; the source descriptor is copied to the global that the removal logs print (`From Skill/Plot/Buff/Passive/Doodad`, `0x3931D120`). | no (protocol) / yes (semantics) |
| `0x39322500` | 231 | `ApplyUpdateAggroHostile` **(inferred)** | `WZUpdateAggro` → `0x39321740`, `0x39321C80` | Shares the external aggro with the AI group's table first (when the NPC's AI-unit component returns 1 from its vt+56), then adds it through the aggro-link path. | likely |
| `0x39321830` | 460 | `ApplyUpdateAggroFriendly` **(inferred)** | `WZUpdateAggro` → `0x392A4020`, `0x393410B0`, `0x39321740` | Applies friendly aggro to the NPC's linked/related NPCs (the set is copied from the AI-unit component at `+34432` by `0x392A4020`). | unknown |
| `0x39321740` | 224 | `AddAggroForSource` **(inferred)** | all `WZUpdateAggro`/`WZUnitDamaged` paths → `AddAggro`, `0x3931DCE0`, `0x39321540` | Adds the abuser and, when the source is a pet/slave/doodad owner, also its owner; then applies the "aggro link" follow-ups (`0x39321540`). | likely |
| `0x39321C80` | 1957 | `ShareAggroWithLinkedNpcs` **(inferred; "aggro link")** | `WZUnitDamaged`, `UpdateAggroHostile`, `SetAggroTarget` → `0x39325250` (50 m scan), `0x3932C4B0`, `0x3932D2B0`, `0x39321740` | Scans NPCs within 50 m; for each candidate checks `accept_aggro_link` (+656 bit 1), `aggro_link_help_dist` (+8), `aggro_link_sight_check`/special-guard bits, and `aggro_link_special_rule_id` (+12: 1 same species/faction, 2 hostile relation, 3 neutral+, 4 always); appends `" + aggro link"` to the log context and adds `{0,0,1}` to the linked NPC's table. | yes (all columns in 1.2; `npc_groups` 158/571 rows) |
| `0x3933C8D0` | 227 | `Npc::OnAffectedByDamage` [V] | `0x393225F0` → `0x3935C830`, `0x39321740` | Damage feed: amount = body +88 (outer packet +104) × multiplier; adds channel 0 (`{amount,0,0}`) for the damager, plus the pet/doodad owner when the damage source is type 2 or 5. | yes |
| `0x393225F0` | 200 | `WZUnitDamaged` router **(inferred)** | `0x39363580` → `0x3933C8D0`, `0x39321C80` | For unit kind 1 it calls `OnAffectedByDamage` on the owner record (`0x393410B0`), then adds the damage aggro through the link path (`0x39321C80`). | yes |
| `0x39363580` | 28 | `WZUnitDamaged` handler **(inferred)** | ZoneClient dispatch (slot 48) → `0x393225F0` | Calls the router with the packet body and the multiplier at +504. | no (protocol) |
| `0x39345770` | 1632 | `Npc::FindNewTargetIfItNeed` [V] | `Npc::Update` (every 4th frame) → `0x39345570`, `0x3933BDC0`, `0x3931DBF0`, `AddAggro`, `0x393226C0`, `0x3933B970` | Target acquisition: builds the distance-sorted candidate list from the CryAI perception; if a candidate already has an entry it logs `...Impossible! Maybe code bug!!!` and stops scanning; otherwise compares the squared distance with the **attack radius** (effective radius from `attack_start_range`); inside and the AI param byte `+108` is set (inferred: aggression) → `AddAggro(unit, "new target")` + force target; inside but not aggressive → alert candidate; outside the attack radius but inside the **sight radius** and no current aggro → alert candidate. The nearest alert candidate raises `OnAlertTargetChanged`. | yes |
| `0x39345570` | 492 | `CollectTargetCandidates` **(inferred)** | `FindNewTargetIfItNeed` → CryAI `vt+136/144` (entities), `0x3935C830`, `0x3932C4B0`, `0x390E15B0` | Enumerates perceived entities, keeps hostile/live units, records `{dist², unit*}`, sorts ascending. | yes |
| `0x3933BDC0` | 112 | `GetEffectiveAggroRange` **(inferred)** | `FindNewTargetIfItNeed` → `0x39258CC0` (stat 61), `0x39B1B950` | `baseRange × (100 + stat61)/100` (stat 61 = `AggroRangeMul`), then a range query around the NPC. Basis (inferred from usage): the two radii are read from the entity AI params, **`+4`** is compared against the attack range and **`+0`** against the sight range. | yes (stat 61 + scales in 1.2) |
| `0x39346960` | 1774 | `Npc::InitializeAggroOnSpawn` **(inferred; "spawn-hostility")** | `NpcManager::Create 0x39348440` → `0x3931EF20`, `AddAggro`, `0x39346340`, `0x3932C4B0` | Creates the table with the npc's distances; when the npc desc has the "aggro at spawn" bit (+656 bit 2), the spawner's creator/summoner target is added (`"By spawn-hostility"`), or kept in `reservedAttackTargetEntity` while `spawnState == 0`; friendly creators reuse their current target (`"By spawn-friendly"`). Also sets the Lua AI params `spawnState=4`, `spawningEffectTime=<spawn effect time>`, `groupid`, `formationTension`, and the group's max member count. | yes (spawn effects 1.2) |
| `0x3933B490` | 116 | `Npc::UpdateAggroTick` **(inferred)** | `Npc::Update 0x393482D0` → `0x393204B0`, `0x393211E0`, `0x3931E570`, `0x3931F790` | Scheduling: every frame `RefreshTopTarget`; every 8th frame `ProcessAggroCancel`; every 64th frame `ProcessReturn` + `0x3931E570`. | yes (cadence is 10.x) |
| `0x393482D0` | 354 | `Npc::Update` [V] | NPC manager tick | Staggered by `(npcId>>8) ^ frameCounter`: 4 frames → time events + `FindNewTargetIfItNeed`; 8 → 3 NPC updates; 16 → 1; 64 → 4; 128 → `CheckPos` (leash to idle). | cadence 10.x |
| `0x3944DA20` | 986 | `SendAiAggroDebug` **(inferred; `ZWAiAggro` 52)** | `0x39333110` (zone tick), at most every 200 ms → manager maps | Sends up to 128 `unitAiAggro` records (unit id, character flag, aggro table) when CVar `qword_3A607D40` is on. | no (debug protocol) |

Supporting functions (address, size): `0x3931DF00` 240 (clear target bookkeeping), `0x3931DCE0` 90
(lookup player table?), `0x393226C0` 18 (`SetAggroTarget` → `0x39321C80`), `0x39321540` 402
(aggro-link follow-up), `0x39321A10` 610 (aggro-link variant), `0x3931E570` 1516 (per-64-frame
pass), `0x3933B970` 163 (raise AI signal), `0x3933C9C0` 169 (spawn position), `0x3933B460` 27
(teleport home), `0x3931D120` 1125 (aggro-source log switch), `0x3931D0B0` 91 (set aggro-source
context), `0x39322AB0` 158 (clear entry bookkeeping on removal), `0x3935FAC0` 162 (unit-side
aggro-list cleanup), `0x3935EC10` 54 (add unit to enemy list), `0x3932C4B0`/`0x3932D2B0` (relation
tests), `0x3931D890` 469 (visible-to-npc test), `0x3931D000` 163 (under-terrain test),
`0x39327360` (`ServerAIUnitSystem::RegisterAbuser`).

## A.3 Constants and defaults

| Constant | Value | Where |
|---|---|---|
| Max aggro entries | 100 (`0x1900 / 0x40`) | `CreateAggro 0x3931F040` |
| Distance coefficient | `0.90909088` if `dist² < 100`, else `0.83333331` | `0x3931D6F0` |
| Aggro value multiplier | attacker `(100 + stat90)/100` | `AddAggro 0x39321340` |
| Entry multiplier | NPC `(100 + stat62)/100` | `0x3931CF00`, `0x3931F040` |
| Effective attack/sight radius | `base × (100 + stat61)/100`; base at AI params `+4` (attack) and `+0` (sight) | `0x3933BDC0`, `0x39345770` |
| minAPS default | `-1.0` (off); 1.2 Lua sets 20 for tower-defence attackers | ctor; `lua12/game/scripts/ai/characters/x2/tower_defense_attacker.lua:17,26` |
| Leash grace | `npc_aggro_cancel_timeout`, default **3000 ms** since last hit | `ProcessAggroCancel 0x393204B0` (CVar `qword_3A607CD8`) |
| Under-terrain grace | **2000 ms** since last hit, margin **1.0 m** | `ProcessAggroCancel 0x393204B0` |
| Boss grades | 3, 4, 5, 8 | `0x393210B0`, `0x3931F790`, `0x393211E0` |
| Boss spawn leash | `2 × absolute_return_distance` | `0x39320D20` |
| Abuser report throttle | 1 s per table | `0x3931E240` |
| Aggro-link scan radius | **50 m** | `0x39321C80` |
| AI debug interval | 200 ms, 128 records | `0x3944DA20` |
| Tick schedule | 4 / 8 / 64 / 128 frames | `0x393482D0`, `0x3933B490` |
| Combat-start reset (zone) | none; the pull position lasts until the table is cleared | `ProcessReturn` |

> **Correction to `AI-ORACLE.md` §2.4:** it says "every removal sends `ZWAggroRemove`". In the code
> only `RemoveFromTable` (`0x393210B0`) and the top-target-loss branch of `RefreshTopTarget`
> (`0x3931F790`) send it, and both only for boss grades 3/4/5/8. The `ProcessAggroCancel` removals
> (`0x3931FF80`) send nothing.

## A.4 Rust comparison (aggro)

Paths are relative to `rust/crates/aaemu-game/src/`.

| # | Feature | Zone (address) | Rust (file:line) | Verdict |
|---|---|---|---|---|
| A1 | Three aggro channels | entry +0/+4/+8 | `systems/skills/state.rs:197-222` (`damage_aggro`/`heal_aggro`/`etc_aggro`) | **match** |
| A2 | Incoming damage multiplier | attacker stat 90 `(100+s)/100` (`0x39321340`); NPC stat 62 per entry (`0x3931CF00`) | `systems/skills/combat.rs:1086-1089` multiplies by attacker `AggroMul/100` **and** owner `IncomingAggroMul/100` into the stored amount | **differs (minor)**: the zone stores the NPC multiplier separately and can re-scale it with `UpdateAggro`; Rust bakes it into the value |
| A3 | Heal aggro | channel 1, no producer found in the zone | `state.rs:215-221` adds `60 %` of heal | **unknown** (world-side rule; not verifiable from the zone) |
| A4 | Target order | sort `(ch0+ch1+ch2) × mul × distcoef`, taunts first (`0x3931EB70`) | `systems/ai/behaviors.rs:847-860` sorts raw `total()` only | **differs**: 0.909/0.833 distance bias and taunt precedence missing |
| A5 | Leash (per target) | entry removed when `return_distance² > 0.1`, position (NPC or target) beyond it, **and** no hit for 3 s (`0x393204B0`) | `behaviors.rs:780-787` drops the target as soon as its distance > `return_distance`, no grace; `behaviors.rs:799-802` | **differs** (top suspect) |
| A6 | Leash (whole table) | `absolute_return_distance` from the **pull** position (`0x39320C90`), bosses also `2×` from **spawn** (`0x39320D20`) | `behaviors.rs:753-772` from `combat_start_pos` with a 5 s reset (`:775`); no boss/spawn rule | **differs (boss rule missing)** |
| A7 | `return_distance` = 0 | disables that check (`> 0.1` / `> 0.0`) | `behaviors.rs:781-783` treats `0` as disabled | **match** |
| A8 | Target acquisition radii | attack = AI+4 × `(100+stat61)/100`, sight = AI+0 × same (`0x39345770`, `0x3933BDC0`) | `behaviors.rs:549` attack `attack_start_range_scale*10`; `:609` sight `sight_range_scale*15`; **no stat 61** | **differs**: if AI-ORACLE's base 15 (attack) / 10 (sight) holds, the Rust bases are swapped; `AggroRangeMul` is not applied either |
| A9 | Alert threshold/behaviour | outside attack radius, inside sight and no aggro → `OnAlertTargetChanged`; Lua alert for `alertDuration` (`0x39345770` + `default.lua`) | `behaviors.rs:583-613`, `:601-613`: sight radius, `is_front`, model-height gap, `alertDuration` (incl. `< 0.1`), per-target alert-safe list (`:562-581`) | **match (structure)**, except A8's radius |
| A10 | Aggro cancel: minAPS | `0x393204B0` | none (grep `min_aps` in `systems/ai` → only the param is parsed) | **missing** (1.2 tower-defence data drives it) |
| A11 | Aggro cancel: under terrain | `check_target_under_terrain`, 2 s, 1 m (`0x393204B0`) | none | **missing** |
| A12 | Aggro cancel: sleep status | save/restore channels while the target sleeps (`0x393204B0`, stash at +64) | none | **missing** |
| A13 | Aggro cancel: not hostile to group | `0x393204B0` + `0x3931E2F0` | none | **missing** |
| A14 | Housing / not-combat-area clear | `0x39320440`, `0x39320DF0` | none | **missing** (10.x housing areas; 1.2 has housing too) |
| A15 | Aggro replication to NPC group | copies the whole entry to every NPC of the AI group (`0x3931FA10`) | none; `behaviors.rs:616-650` implements only the npc-column `aggro_link_*` help, with radius `attack_start_range_scale*200` | **differs / partly missing** |
| A16 | Aggro links | `0x39321C80`: 50 m scan, npc columns `accept_aggro_link`, `aggro_link_help_dist`, `aggro_link_sight_check`, `aggro_link_special_rule_id` (1..4), special guard/ignore | `behaviors.rs:616-680` reads the same columns; radius `attack_start_range_scale*200`, extra relation tests | **differs (radius/rules)** |
| A17 | Damage adds the pet/doodad owner too | `0x39321740` | `skills/combat.rs:1064-1104` adds the attacker only | **differs (minor)** |
| A18 | Taunt | `buffs.taunt` / `taunt_with_top_aggro` → +24 timestamp, sorted first (`0x39320FE0`) | grep `taunt` in `systems/skills` → none | **missing** |
| A19 | Abuser sets / reports | `0x3931E180`/`0x3931E240` + `ZWRegisterNpcAbuser` | n/a (same process; the world does not need the report) | **n/a** |
| A20 | Top-target change signal | `Npc::_OnAggroTableChanged` (`0x3933BA80`) to Lua | `systems/ai/behaviors.rs:792-845` + the graph in `framework.rs:349-411` | **adapted** (no Lua signal, direct state machine) |
| A21 | Spawn hostility | `0x39346960`: add the summoner target, or keep `reservedAttackTargetEntity` while spawning | spawn effects set faction/aggro (`systems/npcs/npc/spawner.rs:1637-1668`) but no `reservedAttackTargetEntity`; AI spawning goes through the normal behaviour | **differs / missing** |
| A22 | Tick model | staggered frames, 4/8/64/128 | fixed 100 ms tick (`systems/ai/manager.rs:1-2`) | **differs by design** |

---

# Part B — the spawner engine

## B.1 Tables, rows and descriptor layouts

The zone loads six DB tables plus `npc_spawners.g`.

| Table (zone SELECT) | Zone loader | Rows 1.2 | Rows 10.x | Columns used |
|---|---|---|---|---|
| `npc_group_members` (`DD.c:2436687`) | `0x39AFE6F0` | 571 | 1,568 | `id, formation_offset_x/y/z, formation_tension, is_leader, is_move_leader, npc_id, npc_group_id` |
| `npc_groups` (`DD.c:2436846`) | `0x39AFE9E0` | 158 | 382 | `id, aggro_rule_id, enable_respawn, name` |
| `npc_spawner_npcs` (`DD.c:2437017`) | `0x39AFECF0` | 15,967 | 23,394 | `id, member_type, member_id, npc_spawner_id, weight` |
| `npc_spawners` (`DD.c:2437216`) | `0x39AFEFF0` | 15,275 | 22,982 | `id, activation_state, destroyTime, endTime, maxPopulation, min_population, name, npc_spawner_category_id, save_indun, spawn_delay_max, spawn_delay_min, startTime, suspend_spawn_count, test_radius_npc, test_radius_pc` |
| `npc_spawner_spawn_effects` (`DD.c:2642054`) | `0x39C0F3E0` | 2,245 | 7,161 | `id, activation_state, despawn_on_creator_death, life_time, spawner_id, use_summoner_aggro_target, use_summoner_faction` (**1.2 lacks `use_summoner_faction`**) |
| `npc_spawner_despawn_effects` (`DD.c:2642188`) | `0x39C0F6D0` | 141 | 2,788 | `id, spawner_id` |
| `npc_spawners.g` | `0x3934AD80` → `0x3934F380` | **absent** (1.2 pak has no `zone_server`, `FEASIBILITY.md` §4) | per-zone | `spawnerId, spawnAreaType, spawnerType, roamingArea, combatArea, points[], triInfos[], paths[], anchors[]` |

**Descriptor layouts (in memory):**

| Struct | Size | Offsets | Evidence |
|---|---|---|---|
| `NpcGroupMemberDesc` | 40 | +0 id, +4/+8/+12 formation x/y/z, +16 formation_tension, +20 bit0 `is_leader` bit1 `is_move_leader`, +24 npc desc ptr, +32 npc_group_id, +36 flag byte | loader `0x39AFE6F0` |
| `NpcGroupDesc` | 56 | +0 id, +4 aggro_rule_id, +8 name, +16 vector of member ptrs (begin/end/cap), +48 `enable_respawn` | loader `0x39AFE9E0` |
| `NpcSpawnerNpcDesc` | 24 | +0 member_type (1 `Npc`, 2 `NpcGroup`), +8 member desc ptr, +16 npc_spawner_id, +20 weight (f32) | loader `0x39AFECF0` |
| `NpcSpawnerDesc` | 104 | +0 id, +4 destroyTime, +8 endTime, +12 maxPopulation, +16 min_population, +24 name, +32 npc_spawner_category_id, +36 spawn_delay_max, +40 spawn_delay_min, +44 startTime, +48 suspend_spawn_count, +52 test_radius_npc, +56 test_radius_pc, +64 vector of member ptrs, +96 bit0 activation_state bit1 save_indun | loader `0x39AFEFF0`, `0x3934A410` |
| `NpcSpawnerSpawnEffectDesc` | 16 | +0 id, +4 life_time, +8 spawner_id, +12 bit0 activation_state bit1 despawn_on_creator_death bit2 use_summoner_aggro_target bit3 use_summoner_faction | loader `0x39C0F3E0` |
| `NpcSpawnerDespawnEffectDesc` | 8 | +0 id, +4 spawner_id | loader `0x39C0F6D0` |
| `NpcSpawner` (runtime) | 528 (`0x210`) | see B.3 | `0x39350410` (allocation), `0x3934A410` |

`spawnAreaType`: `point` → 0, `area` → 1 (`0x3934F380`, `stricmp`). `points` entries are 16 bytes
`{x, y, z, zRot}`; `triInfos` entries are 40 bytes = three vertices (9 floats) + a dword at +36;
`paths` entries are 104 bytes (100-byte `pathName` + `pointNo` at +100).

## B.2 Loaders

| Address | Size | Name | Callers → key callees | What it does | 1.2? |
|---|---|---|---|---|---|
| `0x39AFE6F0` | 744 | `LoadNpcGroupMemberDescs` [V] | GameData loader → SQL, npc desc range map | Reads `npc_group_members`; resolves `npc_id` to the npc descriptor at load time (rows with an unknown npc are dropped); appends each member to its group later via `a2+35320`. | yes |
| `0x39AFE9E0` | 784 | `LoadNpcGroupDescs` [V] | GameData loader → SQL | Reads `npc_groups`; stores name via the string pool; after the table is read, resolves each member's `npc_group_id` and appends the member pointer to the group's vector. | yes |
| `0x39AFECF0` | 760 | `LoadNpcSpawnerNpcDescs` [V] | GameData loader → `XlCompareString("Npc"/"NpcGroup")` | Reads `npc_spawner_npcs`; polymorphic member type (`Npc` → npc desc, `NpcGroup` → group desc; anything else is a hard load error `invalid polymorphic type`); keeps rows only when the referenced desc exists. | yes |
| `0x39AFEFF0` | 1187 | `LoadNpcSpawnerDescs` [V] | GameData loader | Reads `npc_spawners`; after load, resolves each `npc_spawner_npcs` row into its spawner's member vector (`desc+64`). | yes |
| `0x39C0F3E0` | 748 | `LoadNpcSpawnerSpawnEffectDescs` [V] | GameData loader | Reads the spawn-effect table, packs four booleans into +12. | yes except `use_summoner_faction` |
| `0x39C0F6D0` | 427 | `LoadNpcSpawnerDespawnEffectDescs` [V] | GameData loader | Reads the despawn-effect table. | yes |
| `0x3934AD80` | 858 | `NpcManager::LoadSpawners` [V] | `0x3934B0E0` (zone-loaded event) → `Data::LoadFromFile`, `0x39350410`, `0x3934A7F0`, `0x39343580` | Builds `<world>/level_design/zone/<id>/zone_server/npc_spawners.g`, reads it as a `Data` tree, allocates one `NpcSpawner` per child, skips spawners listed in the zone-group config, deactivates a `.g` spawner shadowed by a world spawner (`*(node+32) == 0`), registers the rest. Logs `loading npc spawners ... done!`. | no (data absent in 1.2) |
| `0x3934A7F0` | 214 | `NpcSpawner::LoadFromData` [V class] | `LoadSpawners` → `0x3934F380`, `0x3934A410` | Wrapper: parse a `.g` child into a 348-byte param block, then `InitFromSpawnerParam`. | no |
| `0x3934F380` | 1823 | `ParseSpawnerData` **(inferred)** | `0x3934A7F0` → `Data::*Attribute`, `Data::Find` | Parses `spawnerId`, `spawnAreaType`, `spawnerType`, `roamingArea`, `combatArea`, `points[]` (`pos{x,y,z}`, `zRot`), `triInfos[]` (`v1/v2/v3`), `paths[]` (`pathName`, `pointNo`), `anchors[]`. | no |
| `0x3934A410` | 869 | `NpcSpawner::InitFromSpawnerParam` [V] | `0x3934A7F0` → `sub_3934FAD0`, `0x39AF0C30`, `0x39343990`, `0x3933ED40` | Links the runtime spawner to its `NpcSpawnerDesc`; sums member weights into +364 and normalises them into the per-member runtime records (+376); for `area` builds the triangle-index list (+408); sets active from `desc+96 & 1`; if active calls `QueueSpawnAll(1000 ms)` (100 ms in dev mode). | mechanism yes; data no |
| `0x39343580` | 209 | `NpcManager::RegisterSpawner` **(inferred)** | `LoadSpawners` → `0x397D00F0`, `0x398A7A90` | Appends the spawner to the manager vector and inserts it into the id maps (uid and desc id). | likely |

## B.3 Runtime object and queues

`NpcSpawner` = 528 bytes (`0x39350410`).

| Off | Type | Meaning | Evidence |
|---|---|---|---|
| +0 | u32 | spawner **uid** (instance id, from `.g` `spawnerId`) | `Spawn 0x39348AF0` (`*a1`), `LoadSpawners` |
| +4 | u32 | spawn area type: 0 `point`, 1 `area` | `0x3934F380`, `Spawn` |
| +8 | u32 | `spawnerType` = `npc_spawners.id` (desc id) | `0x3934A410` (`*(a1+8)`) |
| +12 | char[100] | `roamingArea` | `0x3934F380` |
| +112 | char[100] | `combatArea` | `0x3934F380` |
| +216/+224/+232 | vector | points (16 B: x,y,z,zRot) | `0x3934F380` |
| +248/+256 | vector | triangles (40 B: 3 vertices + dword) | `0x3934F380`, `Spawn` |
| +280/+288/+296 | vector | paths (104 B: name + `pointNo`) | `0x3934F380`, `0x39347B20` |
| +312… | vector | anchors | `0x3934F380` |
| +344 | u8 | persistent instance spawner (`save_indun`) | `0x3934A410`, `HandleSpawn`, `OnEvent` |
| +348 | u32 | instance id / zone-group instance **(inferred)** | `HandleSpawn` (`a1[87]`), `Spawn` |
| +352 | u8 | event handling enabled | `OnEvent` |
| +353 | u8 | tower-defence spawner | `OnEvent` |
| +354 | u8 | game-schedule spawner | `OnEvent` |
| +355 | u8 | **active** | `Update`, `OnEvent` |
| +356 | u32 | spawn placement retry counter (logs after 30) | `ValidateSpawn` |
| +364 | f32 | sum of member weights | `InitFromSpawnerParam` |
| +368 | u32 | currently spawned count | `Update`, `Spawn`, `RemoveSpawnedNpcs` |
| +372 | u32 | placement retry/suspend counter | `Spawn` (`a1[93]`) |
| +376/+384 | vector | per-member runtime records (48 B: +0 normalised weight, +8 vector<unitId>, +40 count) | `InitFromSpawnerParam`, `Spawn`, `HandleSpawn`, `HandleNpcRemoved` |
| +408/+416 | vector | per-triangle runtime records (48 B, same shape) | `InitFromSpawnerParam`, `Spawn`, `HandleSpawn` |
| +440/+448 | list | spawn records (npc id, path name, spawn effect time) used by `Save` | `0x39347B20`, `0x3934B7A0` |
| +464 | priority queue | pending spawns `{when_ms, next_delay_ms}` | `QueueSpawn 0x393438D0`, `QueueSpawnAll 0x39343990`, `SpawnAllOnce`, `Update` |
| +496 | u32 | queue size | `QueueSpawnAll`, `Update` |
| +512 | u32 | post-spawn wait (ms): 1000 from the queue, 100 after an activate event | `Update`, `Spawn` |
| +516 | u32 | last spawn / update time (ms) | `Update`, `Spawn` (`a1[129]`) |
| +520 | f32 | last time-of-day seen (day-wrap guard) | `Update` |

## B.4 Function map

| Address | Size | Name | Callers → key callees | What it does | 1.2? |
|---|---|---|---|---|---|
| `0x39349F80` | 490 | `NpcSpawner::Update` [V] | zone tick (`0x39344CE0` = base `Update`) → `0x39AF0C30`, `0x3933C590`, `0x39346050`, `0x39340000`, `0x39348AF0`, `0x3934CB90` | Once per spawner per tick: handles the `destroyTime` day-wrap (`desc+4`), then if the spawner is **active** (`+355`), inside `[startTime,endTime]` (`0x3933C590`), below `maxPopulation` (`desc+12`) and the spawn delay elapsed (`now - +516 >= +512`): pop the pending queue, or spawn (forced when the count is below `min_population`, `desc+16`). | yes (rules) |
| `0x393438D0` | 178 | `NpcSpawner::QueueSpawn` [V] | `HandleNpcSpawnFailed`, `HandleNpcRemoved` → `CryRandomFloat`, `0x39343810` | Enqueues one spawn at `now + uniform(spawn_delay_min, spawn_delay_max) × 1000`. | yes |
| `0x39343990` | 457 | `NpcSpawner::QueueSpawnAll` [V] | `InitFromSpawnerParam`, `OnEvent` → `0x3934D9A0` (heap push) | Enqueues `maxPopulation - spawned` spawns due **now**, each with the same *post-spawn* delay (1000 ms at load, 100 ms on an activate event). `Update` enforces that delay between two spawns from the same spawner, so the fill is staggered at one NPC per second (100 ms after an activate event). | yes |
| `0x39348AF0` | 3123 | `NpcSpawner::Spawn` [V] | `Update`, `SpawnAllOnce`, `OnEvent` (persistent) → `0x39AF0C30`, `0x3934F380` data, `0x39347470`, `0x39340E40`, `0x39348A40`, `0x39344530`, `CryRandom` | The core spawn: pick a member (least-represented when population > 0, else weighted random); resolve the representative npc (leader for a group); pick a point (uniform) or a triangle (baro­centric random point, terrain-snapped or water level), retry 5 times (50 when the representative NPC's brief-data flag word is 0, inferred: ground movement) with `test_radius_pc`/`test_radius_npc` spacing checks; build `ZWSpawnNpc` (uid, desc id, member/point/tri indices, npc id, position, yaw, scale 1.0); for a group call `SpawnNpcGroup`; on success `++count`, `++memberCount`, `++triCount`, next delay = the caller's value (1000 ms from most callers, 100 ms after an activate event); on failure retry in 1 s (60 s after a data error). | mechanism yes; placement data no |
| `0x39348A40` | 170 | `SendSpawnNpc` **(inferred)** | `Spawn` → `0x39347A00`, `0x3936EDC0` | Validates the spawn (`ValidateSpawn`) and the spawner's presence in the world map, then queues the `ZWSpawnNpc` packet. | no (protocol) |
| `0x39344530` | 958 | `NpcSpawner::SpawnNpcGroup` [V] | `Spawn` → group desc, `0x3934CA40` (formation), `0x39344350` (group id), `0x3934BFB0` (member pos), `0x3934B230`, `0x3936EDC0` | Spawns a whole `NpcGroup`: finds the **move-leader** (`is_move_leader`, `member+36 & 2`; no leader is a hard failure), builds a formation from the leader position/yaw, allocates a runtime group id, then for each member sets its formation offset, rewrites the `ZWSpawnNpc` and sends one packet per member; logs `Group(%u, "%s") Generated`. | yes (158 groups / 571 members in 1.2) |
| `0x39344900` | 406 | `NpcSpawner::HandleNpcSpawnFailed` [V] | `WZNpcSpawnFailed` handler → `0x39AF0C30`, `0x39343310`, `QueueSpawn` | The world could not create the NPC: decrements the per-member/triangle counters and the total, then re-queues a spawn. | no (protocol) / yes (policy) |
| `0x39347A00` | 269 | `NpcSpawner::ValidateSpawn` [V] | `SendSpawnNpc` → `0x39347670`, `0x3933F340` | Placement validity: rejects if the persistent/instance spawner is in a special state, otherwise asks the zone (`0x39347670`, terrain/triangle validity); tracks a "position occupied > 30 times" counter (+356) and logs it. | yes |
| `0x39347F90` | 818 | `NpcSpawner::HandleSpawn` [V] | `NpcManager::Create 0x39348440` → `0x3933B8F0`, `0x39347B20`, `0x39362700` | World confirmed an NPC: records the unit id in the per-member list (+376) and, for area spawners, in the per-triangle list (+408); records the "spawn info" (`0x39347B20`); for persistent instance spawners sends **`ZWSaveInstanceSpawner` (5: spawner uid, unit id, `instance id differs`)**. | yes (the save-indun rule; the packet is 10.x) |
| `0x39346050` | 744 | `NpcSpawner::RemoveSpawnedNpcs` [V] | `Update` (destroyTime), `OnEvent` (5/6) → unit lookup, `0x39345220`, `0x393626F0`, `0x39362830` | Removes every spawned NPC. For each unit: if it is a pet-like unit (final unit-kind test `== 2`, inferred: pet/summon) and the caller asked for the despawn effect, sends **`ZWCreateBuff` (10)** with the hardcoded despawn buff id (`dword_3A518F10`); otherwise sends **`ZWRemoveNpc` (3: unit id)**. | yes (despawn effect 1.2) / the buff id is 10.x |
| `0x39344AA0` | 560 | `NpcSpawner::HandleNpcRemoved` **(inferred)** | `0x39345220` (unit's spawner) → `0x3933B8F0`, `0x39343310`, `QueueSpawn` | NPC removed: clears the group member bit, drops the unit from the member/triangle lists, decrements +368; if the whole group is gone and the spawner is not persistent, queues a respawn (`QueueSpawn`); for persistent spawners deactivates (+355 = 0). | yes |
| `0x39345220` | 159 | `NotifySpawnerOfUnitRemoval` **(inferred)** | `RemoveSpawnedNpcs` → `0x39344AA0` | Looks up the spawner id stored on the unit (+32) and forwards the removal. | yes |
| `0x39347B20` | 1123 | `RecordSpawnInfo` **(inferred)** | `HandleSpawn` → `0x3934B230`, `CryRandom`, `0x39062AA0` | Finds the path entry whose `pointNo` matches the spawn's point index (random path if none) and appends a spawn record `{npc id, path name, ai flag, spawn effect time}` to the spawner list at +440 (serialised by `Save`). | unknown (1.2 has no `.g` paths) |
| `0x39349730` | 187 | `NpcSpawner::SpawnAllOnce` [V] | `OnEvent` 7/8 | Clears the queue and spawns up to `maxPopulation - count`, retrying each spawn up to 10 times. | yes |
| `0x39349800` | 900 | `NpcSpawner::OnEvent` [V] | `OnNpcSpawnerEvent 0x39364140`, zone tick, tower-def/schedule | The 9 spawner events: 1 activate + `QueueSpawnAll(100 ms)`, 2 deactivate + clear queue, 3 persistent-instance spawn (deactivate and disable on failure), 4 deactivate + disable, 5 activate + `RemoveSpawnedNpcs(0)` + `SpawnAllOnce`, 6 deactivate + `RemoveSpawnedNpcs(buff if owner type 2)`, 7 activate + `SpawnAllOnce`, 8 activate + `SpawnAllOnce` + deactivate, 9 "reset/group respawn" (`0x39342920`). Tower-defence and game-schedule spawners ignore non-matching owner types. | partly (tower-def/schedule differ) |
| `0x3933C590` | 168 | `NpcSpawner::IsSpawningTime` [V] | `Update` | Time-of-day window `[startTime, endTime)` with 24 h wrap (`desc+44` start, `desc+8` end). | yes |
| `0x3933D720` | 91 | `NpcSpawner::Save` [V] | no direct caller (console/save path) → `0x3934B7A0` | Rejects invalid types/point counts, then serialises the spawner to a `Data` tree. | mechanism yes / placement no |
| `0x3934B7A0` | 1948 | `SaveToData` **(inferred)** | `Save` | Writes the spawner back: id, type, points/triangles/paths/anchors, spawned records. | unknown |
| `0x3933EDF0` | 1119 | `NpcSpawner::SpawnForDev` [V] | dev console paths → `0x39348AF0` | Dev-mode force spawn at a clicked position; logs `Unknown spawn area type`. | no (dev) |
| `0x39341EF0` | 979 | `NpcGroupSupporter::RespawnLostGroupMembers` [V] | `0x39342920` (spawner removed) → `0x39340BF0`, `0x3934CA40`, `0x3934BFB0`, `0x3934B230`, `0x3936EDC0`, `0x39343310` | If a whole group is gone while other members of the "group supporter" survive, respawns the missing member(s) at formation positions around the surviving leader's position, with the stored spawner id, and sends `ZWSpawnNpc` with a `5` status field (respawn marker, inferred). | likely (group data in 1.2) |
| `0x39343310` | 328 | `NpcGroupSupporter::HandleGroupMemberRemoved` [V] | `HandleNpcSpawnFailed`, `HandleNpcRemoved`, `RespawnLostGroupMembers` → `PoolFree` | Clears the member's alive bit (`~(1<<idx)`); when the whole mask is 0, frees the runtime group, logs `Group(%u,"%s") removed` and returns true (the caller then decrements the spawner count). | likely |
| `0x39342920` | 450 | `OnSpawnerRemovedRespawnGroups` **(inferred)** | `OnEvent` 9 | For every spawned NPC, resolves its group and calls `RespawnLostGroupMembers`. | likely |

Supporting functions (address, size): `0x3934A2D0` 310 (spawner ctor), `0x39350310` 156 (spawner dtor),
`0x39350410` 77 (allocate 528 B), `0x3933FC60` 97 (spawner by uid), `0x393410B0` 108
(spawner-player/manager by id), `0x3933B8F0` 119 (unit's group info), `0x3933AFF0` 89 (NPC by id),
`0x39347470` 495 (`IsSpawnPointFree`: units within `test_radius_pc`), `0x39340E40` 532 (other NPCs
within `test_radius_npc`), `0x3934CA40` 290 (build formation), `0x3934BFB0` 340 (member formation
position), `0x39344350` 470 (allocate group id), `0x39340BF0` 107 / `0x39340AE0` 109 (group member
alive bookkeeping), `0x39340D90` 53, `0x3933C9C0` 169 (spawn position), `0x39340000` 125 (reset after
destroy time), `0x39344CE0` 550 (base `Update`: despawn/queue housekeeping), `0x3934CB90` 71 /
`0x3934CBE0` 57 (queue front/pop), `0x3934D9A0` 388 (heap push), `0x3933B700` 8 (heap comparator),
`0x3934B230` 339 (copy a 1288-byte spawn-info record), `0x3936E940` 1144 (spawner-list reconcile
**[I]**, see B.5), `0x39364140` 86 (`OnNpcSpawnerEvent`), `0x3938DF30` 272 (`WZNpcSpawnerEvent`
field schema), `0x3938E0E0` 767 (`ZWSpawnNpc` field schema), `0x39387FD0` 325 (`WZSpawnerList`
entry schema), `0x39494090` 6 (packet vtable stub).

## B.5 Runtime walkthrough

1. **Load.** `NpcManager::LoadSpawners` (`0x3934AD80`) reads the zone's `.g`, one `NpcSpawner` per
   child. `InitFromSpawnerParam` (`0x3934A410`) links each runtime spawner to its DB row
   (`npc_spawners.id` = `spawnerType`) and pre-computes member weights (normalised, in member
   order). An inactive row (`activation_state` false) stays inactive; an active one queues all its
   spawns as **due immediately** with a 1000 ms post-spawn wait (100 ms in dev mode), so it fills at
   one NPC per second (ten per second in dev mode).
2. **Tick.** `NpcSpawner::Update` (`0x39349F80`) runs for every registered spawner. It only acts
   while the spawner is active, inside the daily `[startTime,endTime)` window and below
   `maxPopulation`. A spawn happens when the next-delay deadline passed; if the count is below
   `min_population`, the spawn is forced (no delay). `destroyTime` (desc+4) triggers
   `RemoveSpawnedNpcs(1)` and the spawner resets.
3. **One spawn.** `Spawn` (`0x39348AF0`) chooses the member (least-represented, else weighted
   random), the point/triangle and the yaw, validates spacing (`test_radius_pc` for players,
   `test_radius_npc` for other NPCs of the same spawner; both disabled when < 0.01), and emits
   `ZWSpawnNpc`. A successful spawn sets the post-spawn wait to 1000 ms (100 ms after an activate
   event) and increments the counters. A failed placement retries after 1 s; a data error after 60 s.
4. **Groups.** For an `NpcGroup` member, `SpawnNpcGroup` (`0x39344530`) finds the move-leader,
   computes formation positions from the leader's position and yaw plus the member offsets, and
   sends one `ZWSpawnNpc` per member. The group gets a runtime id. When an NPC is removed, the
   member bit is cleared (`0x39343310`); the spawner only re-queues a spawn when the whole group is
   gone. `NpcGroupSupporter::RespawnLostGroupMembers` (`0x39341EF0`) can respawn missing members
   around a surviving leader when the spawner itself disappears.
5. **World round trip.** `ZWSpawnNpc` (2) → world creates the NPC → `WZNpcState` (2) → the zone
   matches the spawner by uid and the member/point/tri indices (`HandleSpawn`, `0x39347F90`),
   records the unit, and for a persistent instance spawner sends `ZWSaveInstanceSpawner` (5).
   `WZNpcSpawnFailed` (3) → `HandleNpcSpawnFailed` (`0x39344900`) rolls the counters back and
   re-queues.
6. **Removal.** The zone removes an NPC with `ZWRemoveNpc` (3) or, for a pet-like unit with the
   despawn effect, `ZWCreateBuff` (10). The world's removal notification comes back through the
   spawner id stored on the unit (`0x39345220` → `0x39344AA0`).
7. **Effects.** `npc_spawner_spawn_effects` (life_time, despawn-on-creator-death,
   use-summoner-aggro-target/faction) is loaded into the zone but the **runtime parameters arrive in
   `WZNpcSpawnerEvent`** (`0x3938DF30`: `spawnerId, spawnerEvent, ..., type,
   despawnOnCreatorDeath, useSummonerAggroTarget, lifeTime`); `OnNpcSpawnerEvent` (`0x39364140`)
   forwards only `spawnerId`/`event`/owner type to `NpcSpawner::OnEvent`. In other words, in 10.x
   the **world** owns the effect semantics and the zone only activates/deactivates the spawner.
   Our Rust server, being the world, implements the effect (`systems/skills` spawn effects) and
   applies it to the summoner's faction/aggro/life-time.
8. **Spawner list.** `WZSpawnerList` (5) carries `zoneId, zoneOrigin, spawnerId, spawnPos, pos` per
   entry (`0x39387FD0`). `0x3936E940` **[I]** deactivates/clears the manager's spawner registry and
   rebuilds it from a list of 16-byte entries, then re-activates the flagged ones. This is how the
   world adds/removes spawners for a zone at runtime (10.x-only mechanism).

## B.6 Rust comparison (spawners)

| # | Feature | Zone (address) | Rust (file:line) | Verdict |
|---|---|---|---|---|
| B1 | `npc_spawners` template | loader `0x39AFEFF0` | `systems/npcs/npc/spawner_data.rs:74-119` (`NpcSpawnerTemplate`) | **match** (same columns; `comment` added) |
| B2 | `npc_spawner_npcs` (type, weight) | `0x39AFECF0` | `spawner_data.rs:34-72`; weighted pick `systems/npcs/npc/spawner.rs:196-204` | **match** (weights; the zone also normalises per member, same effect) |
| B3 | `npc_groups` / `npc_group_members` | `0x39AFE9E0`, `0x39AFE6F0` | only in `instances`/`tower_def` as ids; **no group/formation runtime** | **missing** (1.2 data: 158 groups, 571 members) |
| B4 | `NpcGroup` member spawning | `SpawnNpcGroup 0x39344530` | `spawner.rs:1921-1935` treats `"NpcGroup"` as a single NPC | **differs / missing** (no leader, formation or group respawn) |
| B5 | Spawn placement | `.g` points (16 B) / triangles (40 B), 5/50 retries, terrain/water snap | one JSON position per spawner (`systems/npcs/spawn_manager.rs:1072-1145`); no triangle spawners | **differs** (data source and area spawners) |
| B6 | Spacing rules | `test_radius_pc` (players), `test_radius_npc` (own NPCs), disabled under 0.01 | `spawner.rs:768-790` `authored_radius`, player count inside the radius also **scales population** (`spawner.rs:668-683`) | **differs** (the zone never scales population by nearby players; players only block points) |
| B7 | Population | `maxPopulation`, `min_population` (force bypass), `suspend_spawn_count` (retry limit), no player scaling | `spawner.rs:668-683` target = `max(min_population, count, players)` clamped by `max_population` and `suspend_spawn_count` | **differs** (player-scaled target) |
| B8 | Spawn delay | after a **removal** `QueueSpawn` schedules `now + uniform(spawn_delay_min, spawn_delay_max)` s; after a **successful spawn** the spawner waits 1000 ms (100 ms after an activate event) | `spawner.rs:2005` `random_float(min,max)` as the respawn time; 1 s spawn tick (`spawn_manager.rs:73`) | **match (close)** |
| B9 | Initial fill | `QueueSpawnAll` marks every slot due, but the 1000 ms post-spawn wait staggers the fill at **one NPC per second** per spawner | `spawn_all` / activation sweep fills the whole target in one pass (`spawn_manager.rs:1545-1580`) | **differs** (start-up burst) |
| B10 | Daily window / destroy | `[startTime,endTime)` + `destroyTime` (`Update`, `0x3933C590`) | `spawner.rs:1429-1443` (`is_within_spawn_time`), `:1709` (`is_within_despawn_time`) | **match (close)** |
| B11 | On removal → respawn | `HandleNpcRemoved 0x39344AA0` → `QueueSpawn` (delay), group-aware | `spawner.rs:1068-1135` `do_despawn` → respawn time; `:1187` `despawn_with_respawn` | **match (policy differs only in group handling)** |
| B12 | Spawn/despawn effect tables | loaders `0x39C0F3E0`, `0x39C0F6D0`; parameters arrive via `WZNpcSpawnerEvent` | `systems/skills/data/effects.rs:988-1013` (loads both, no `use_summoner_faction` — 1.2 has no column), applied in `spawner.rs:1607-1692` | **match (division of labour differs by design)** |
| B13 | Persistent instance spawner | `save_indun` + `ZWSaveInstanceSpawner` (`HandleSpawn`) | persistence is world-side (`systems/instances`); not in the spawner | **n/a (world owns persistence)** |
| B14 | Spawner events | 9 event types (`OnEvent 0x39349800`) | `do_event_spawn` (`spawner.rs:1468`), management groups (`Shared`) | **partly** |
| B15 | Spawner list from the world | `WZSpawnerList` + `0x3936E940` | spawners loaded from files/DB; runtime add via `SpawnManager::add_npc_spawner` | **n/a (world side)** |
| B16 | `spawnState` / `spawningEffectTime` | `0x39346960` sets AI params; spawn effect time from the spawn packet | AI has a spawning behaviour (`behaviors.rs:2011-2060`) that uses the 0.1 s minimum and does not receive a spawn effect time (its own comment: 1.2 data has none) | **differs** |

---

## 4. Ranked differences worth checking against the running zone

Ranked by expected player-visible impact and by how cleanly a headless bot can measure them. Each
entry names the zone behaviour/address, the Rust site, and the test.

1. **Leash: grace + semantics (A5).** Zone drops a target only when it is beyond
   `return_distance` **and** has not hit for `npc_aggro_cancel_timeout` (3000 ms), and "beyond" is
   measured as the *NPC/group* (method 0) or the *target* (method 1) vs the pull position
   (`0x393204B0`). Rust drops immediately when the target is > `return_distance` from the NPC
   (`behaviors.rs:780-787`). Test with NPC 10008/100 (return 50, abs 200): pull, run 40/60/100 m,
   one run without hitting and one with a ranged hit; expect the retail NPC to keep chasing to
   ~absolute distance and only drop after 3 s without hits.
2. **Boss spawn leash and boss-only reports (A6/A19).** `ProcessReturn` first applies
   `2 × absolute_return_distance` from the **spawn position** for grades 3/4/5/8
   (`0x393211E0` → `0x39320D20`), then (when the boss is still inside 2×) the generic
   `absolute_return_distance` from the **pull position** (`0x39320C90`). Since the pull position is
   recorded at the NPC when the first entry is created, the 1× pull rule normally fires first; the
   boss rule only bites when the first aggro is created while the boss is already far from spawn.
   Rust has neither the boss rule nor its order. Test a grade-3/4 boss whose first aggro is created
   by a group member far from the spawn point.
3. **Target choice: distance coefficient and taunt (A4).** Zone score =
   `(ch0+ch1+ch2) × (100+stat62)/100 × 0.909|0.833` with taunted entries first (`0x3931EB70`).
   Rust sorts raw totals (`behaviors.rs:847`). Test two bots on one NPC, aggro totals 1000 vs 950,
   one at 5 m and one at 15 m: retail switches to the close one, Rust does not. Also a taunt skill
   must force the target regardless of aggro.
4. **Acquisition radii (A8).** Zone attack radius is AI param **+4** and sight radius **+0**, both
   scaled by `(100+stat61)/100` (`0x39345770`, `0x3933BDC0`). Rust uses `attack_start_range_scale×10`
   and `sight_range_scale×15` (`behaviors.rs:549,609`) and no `AggroRangeMul`. Walk a bot in 1 m
   steps from 4 m to 16 m from a default-scale hold-position NPC and record the alert and the
   attack distance.
5. **Aggro cancel rules (A10-A14).** minAPS (1.2 tower-defence `minAPS=20`, `NpcSetMinAps`),
   under-terrain > 2 s, sleep-status freeze, "no longer hostile to the group", housing area and
   "not combat area" clears (`0x393204B0`, `0x39320440`, `0x39320DF0`). Test: let a bot's DPS fall
   below 20 on a tower-defence NPC; stand under the terrain behind a wall; sleep the NPC; pull it
   into a housing plot.
6. **Group aggro replication and aggro links (A15/A16).** Zone copies the full entry to every NPC
   of the AI group (`0x3931FA10`) and adds `{0,0,1}` to NPCs within 50 m that pass the
   `accept_aggro_link` rules (`0x39321C80`); Rust has a different radius
   (`attack_start_range_scale×200`) and no group copy (`behaviors.rs:616-650`). Test one of the 158
   1.2 groups: pull the leader and see which members join and at what distance.
7. **Spawner population must not scale with nearby players (B6/B7).** Zone population is
   `[min_population, maxPopulation]` only; players merely block spawn points within
   `test_radius_pc` (`0x39347470`). Rust's `target_population` raises the target to the number of
   players in the radius (`spawner.rs:668-683`). Test: stand 1, 3, 10 players in a camp and count
   respawned NPCs. If retail does not scale, our Rust over-spawns crowded camps.
8. **Group spawning at all (B3/B4).** 147 of the 1.2 `npc_spawner_npcs` rows are `NpcGroup`; Rust
   spawns a single NPC for them (`spawner.rs:1921-1935`). Test a group spawner (group 1-158 with a
   leader): count spawned NPCs and check formation spacing and respawn of a single lost member.
9. **Area spawners and placement retries (B5).** 10.x area spawners pick a random point in a
   triangle, snap to terrain/water, retry 5/50 times, and use `test_radius_npc`/`test_radius_pc`
   (`0x39348AF0`). Rust has point spawners from JSON only. Not directly testable against 1.2 (no
   `.g`), but it means our spawn positions must come from the 1.2 world data; check a dense camp
   for the same NPC positions/distribution as the 1.2 client.
10. **Damage aggro: owner attribution and late multipliers (A2/A17).** Zone adds the pet/doodad
   owner too (`0x39321740`) and stores the NPC multiplier per entry, refreshing it on stat change
   (`0x3931FD30`). Rust adds the attacker only and bakes `IncomingAggroMul` into the amount
   (`combat.rs:1086-1089`). Test a pet hitting an NPC and an `AggroMul`/`IncomingAggroMul` buff
   applied/removed mid-fight.
11. **Spawn hostility and the spawn "busy" window (A21/B16).** Zone sets `spawnState=4`,
   `spawningEffectTime`, and either `reservedAttackTargetEntity` or an immediate
   `By spawn-hostility` aggro (`0x39346960`). Test an NPC with an `on_spawn` skill / spawn effect
   time: retail is busy and invincible for that time.
12. **Despawn-effect buff (B12).** Zone sends `ZWCreateBuff` with a hardcoded buff instead of
    removing pet-like units on spawner despawn (`0x39346050`). Check what buff id
    `dword_3A518F10` is and whether 1.2 has it; if so, our despawn path is missing it.

## 5. Unknowns

* **`0x3931DD40`/`0x3931DDC0` semantics.** The three 32-byte sets at +136/+168/+200 are used both to
  freeze aggro updates for a unit and for `ZWRegisterNpcAbuser`. Which abuse categories the three
  channels are, and whether the 1.2 zone had this at all, is **[?]**. The same DLL's GameClient side has
  `SCAiAggroPacket` (client-side aggro UI), so the zone-side report may be a 10.x debug/anti-cheat
  addition.
* **`WZUnitHealed` path.** No function writing aggro channel 1 was found; the zone may ignore heals
  for aggro (the 60 % heal weight in our Rust is a C#-era rule). **[?]**
* **`npc_aggro_cancel_position_method` (CVar `qword_3A607CD0`)**: method 0 = NPC/group position vs
  pull position, method 1 = target position vs target-at-pull. Default value not recoverable from
  the decompile (CVar default registered elsewhere); both branches were read. **[?]**
* **CheckPos contexts 4 and 5** (see `AI-ORACLE.md`) are still unmapped; they are not the aggro
  leash but the idle/talk leash.
* **`0x3936E940` (spawner-list reconcile).** Behaviour inferred from the shape (clear registry →
  send event 6 → rebuild from 16-byte entries → send event 1); the packet's exact field layout
  (`WZSpawnerList`, schema at `0x39387FD0`) has more fields than the handler's runtime entries, so
  the mapping is **[I]**.
* **`WZActivateNpcSpawnersInArea` (slot 66) handler** was not located; only the packet RTTI
  (`DD.c:58766`) and the functor base are known. Spawners can also be activated via `OnEvent`
  from the zone tick and the tower-defence/schedule code, so this is a gap, not a blocker. **[?]**
* **`0x39347B20` (spawn record / path selection)** is only partly decoded: it searches the `.g`
  paths for the point index and appends `{npc id, path name, ai flag, spawn effect time}` to the
  spawner's list at +440, but what consumes those records besides `Save` is not resolved. **[?]**
* **Boss `2 × absolute_return_distance`** is 10.x code; the 1.2 Lua `return_state` has no boss
  special case, and the 1.2 zone binary is not available. Transfer verdict "likely" only because
  the column exists. **[?]**
* **Spawn effect ownership.** The zone loads `npc_spawner_spawn_effects` but the runtime values
  arrive in `WZNpcSpawnerEvent`; whether the zone ever reads its own loaded rows (apart from the
  generic `effects` resolver, `0x39AD0220`) was not proven. Our world-side implementation may be
  the only consumer in practice. **[I]**

## 6. How this was produced (reproduction)

* Function bodies: `grep -n "^//----- (003931EF20)"` on DD.c / a small helper that slices from one
  `//----- (VA)` marker to the next, with sizes from `funcs.csv` (as described in
  `Docs/zone/aggro-spawners/NOTES-aggro.md`).
* Callers: `grep -n "sub_39321340(" DD.c` and map the line to the enclosing `//-----` marker.
* Column checks: `PRAGMA table_info(npcs)` and `SELECT count(*)` on both DBs (read-only).
* No building, no running, no client files modified. No CRYENGINE source was used.

## 7. Files

* `Docs/zone/aggro-spawners/MAP.md` (this file)
* `Docs/zone/aggro-spawners/NOTES-aggro.md` — raw per-function notes, offsets, and packet layouts
* `Docs/zone/aggro-spawners/NOTES-spawners.md` — spawner runtime offsets, `.g` schema, queue details
* `Docs/zone/aggro-spawners/functions.csv` — the machine-readable function list (address, size,
  name, scope, transfer, Rust site)
