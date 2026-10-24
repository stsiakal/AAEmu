# Working notes — NPC spawner engine (10.x zone)

Raw notes behind `MAP.md` §B. Addresses are VAs in `x2game-dev_dedicate.dll`; sizes from
`D:/aa-re/zone/bin/x2game-dev_dedicate.dll.funcs.csv`; DD.c lines from
`D:/ArcheAge Returns 10.0.2.13 - 8yx - r575 - 2026-06-18/Bin64/decompiled/x2game-dev_dedicate.dll.c`.

## 1. Boot path

```
"zone loaded" event handler 0x39332730
  -> 0x3934B180 (thin wrapper) -> 0x3934B0E0
       -> 0x3934A8D0   (clear manager registry: vector + two id maps at +56/+88/+120)
       -> 0x3934AD80   (load npc_spawners.g)
       -> sub_399CA750 (sort the spawner vector by ...?, comparator 0x3933D680)
       log "[NPC Manager] total %ld npc spawners are loaded"
```
`0x3934B0E0` skips the `.g` load when the map is loaded with `gEnv+336` and CVar `qword_3A607CF0`
says so (client/editor path). DD.c ~858996: `loading npc spawners ...` / `... done!`.

## 2. `npc_spawners.g` (10.x only)

Path built at DD.c:858980: `%s/level_design/zone/%d/zone_server/npc_spawners.g` where `%s` is the
level name and `%d` the zone id (`I3DEngine->vt+120/112`; `0x3934AD80`).

Parsed fields (`0x3934F380`):

| `.g` attribute | Stored at | Notes |
|---|---|---|
| `spawnerId` | +0 (uid) | |
| `spawnAreaType` | +4 | `"point"` → 0, `"area"` → 1, anything else fails the load |
| `spawnerType` | +8 | `npc_spawners.id` (desc key) |
| `roamingArea` | +12 (100 chars) | |
| `combatArea` | +112 (100 chars) | |
| `points[]` | +216 vector, 16 B each | child `pos {x,y,z}` and `zRot` |
| `triInfos[]` | +248 vector, 40 B each | children `v1`, `v2`, `v3` (each a position) |
| `paths[]` | +280 vector, 104 B each | `pathName` (100 chars) + `pointNo` at +100 |
| `anchors[]` | +312 vector | `pos` children; layout not decoded |

The zone checks `Size()` on the root and each child; a missing `points` block or (for `area`) a
missing `triInfos` block fails the spawner load (`0x3934F380` returns 0).

`npc_spawners.g` exists only in the 10.x pak (`FEASIBILITY.md` §4: the 1.2 pak has no
`zone_server` directory), so this placement data has no 1.2 counterpart we hold.

## 3. Descriptor tables (in-memory layout after load)

### `NpcSpawnerDesc` (`npc_spawners`), 104 bytes, loader `0x39AFEFF0`

```
+0   id
+4   destroyTime (f32, hours)
+8   endTime (f32, hours)
+12  maxPopulation (int)
+16  min_population (int)
+20  (padding)
+24  name (const char* from the string pool)
+32  npc_spawner_category_id
+36  spawn_delay_max (f32, seconds)
+40  spawn_delay_min (f32, seconds)
+44  startTime (f32, hours)
+48  suspend_spawn_count (int)
+52  test_radius_npc (f32)
+56  test_radius_pc (f32)
+64  std::vector<NpcSpawnerNpcDesc*> members (begin/end/cap)
+96  flags: bit0 activation_state, bit1 save_indun
```
The loader reads `activation_state`/`save_indun` as strings (`T`/`t`/`1` → true).

### `NpcSpawnerNpcDesc` (`npc_spawner_npcs`), 24 bytes, loader `0x39AFECF0`

```
+0  member_type: 1 = "Npc", 2 = "NpcGroup"  (anything else: "invalid polymorphic type")
+4  (padding / row high dword)
+8  pointer to the NpcDesc or NpcGroupDesc
+16 npc_spawner_id (the owning spawner desc id)
+20 weight (f32)
```
Rows whose member desc does not resolve are dropped. After all tables are loaded,
`0x39AFEFF0`'s tail walks this list and appends each member pointer to `spawnerDesc+64`
(count incremented at `a2+136072`).

### `NpcGroupDesc` (`npc_groups`), 56 bytes, loader `0x39AFE9E0`

```
+0  id
+4  aggro_rule_id
+8  name (string pool)
+16 std::vector<NpcGroupMemberDesc*> members (begin/end/cap)
+48 enable_respawn (bit)
```
After the load, every `npc_group_members` row resolves its `npc_group_id` and appends its pointer to
that group's vector (DD.c ~2436900).

### `NpcGroupMemberDesc` (`npc_group_members`), 40 bytes, loader `0x39AFE6F0`

```
+0  id
+4  formation_offset_x
+8  formation_offset_y
+12 formation_offset_z
+16 formation_tension
+20 bits: 1 = is_leader, 2 = is_move_leader
+24 NpcDesc* (resolved at load; row dropped when unknown)
+32 npc_group_id
+36 flag byte (copied from +20; the zone tests bit 1 for is_leader and bit 2 for is_move_leader)
```

### Effects

`npc_spawner_spawn_effects` (`0x39C0F3E0`), 16 B:
```
+0 id, +4 life_time, +8 spawner_id, +12 bits: 1 activation_state, 2 despawn_on_creator_death,
    4 use_summoner_aggro_target, 8 use_summoner_faction
```
`npc_spawner_despawn_effects` (`0x39C0F6D0`), 8 B: `+0 id, +4 spawner_id`.

The zone's only xrefs to these two storages (`a2+14472`, `a2+14608`) are the loaders and the global
`effects` polymorphic resolver (`0x39AD0220`, type 19/20) — see `MAP.md` §5 "Unknowns". The runtime
parameters in `WZNpcSpawnerEvent` (`0x3938DF30`) duplicate the columns, so the world is the
effective owner of the effect behaviour in 10.x.

## 4. Runtime object (528 bytes, `0x39350410` → ctor `0x3934A2D0`)

Offsets used in this document (see also `MAP.md` §B.3):

```
+0    uid                       +4    area type (0 point, 1 area)
+8    desc id (spawnerType)     +12   roamingArea[100]
+112  combatArea[100]           +216  points vector (16 B)
+248  triangles vector (40 B)   +280  paths vector (104 B)
+312  anchors                   +344  persistent (save_indun)
+348  instance id (inferred)    +352  event-enabled
+353  tower-def spawner         +354  game-schedule spawner
+355  active                    +356  placement retry counter
+364  total member weight       +368  spawned count
+372  placement suspend counter
+376  per-member runtime records (48 B: +0 weight, +8 vector<unitId>, +40 count)
+408  per-triangle runtime records (48 B, same shape)
+440  spawn records list (npc id, path name, ai flag, spawn effect time)
+464  spawn queue (priority queue of {when_ms, next_delay_ms})
+496  queue size
+512  post-spawn wait (ms)      +516  last spawn/update time (ms)
+520  last time-of-day seen
```

`0x39350410` allocates `0x210` bytes. `0x39350310` destroys it (vector/list/dtors).

## 5. `NpcSpawner::Update 0x39349F80`

```
0x39344CE0(a1)                      // base update: queued despawns / timers
if (!*(a1+355)) return;             // inactive
now = time_of_day_seconds()
if (now - *(a1+520) < -0.1) *(a1+520) -= 24;   // midnight wrap
*(a1+520) = now
desc = NpcSpawnerDesc[*(a1+8)]
if (!desc) log "Failed to find spawner desc(%u)"
if (destroyTime crossed in the last frame) { RemoveSpawnedNpcs(a1, 1); 0x39340000(a1); }
else if (IsSpawningTime() && count(+368) < desc+12) {
    if (now_ms - *(a1+516) < 0 || now_ms - *(a1+516) - *(a1+512) >= 0) {
        force = count < desc+16;                       // min_population
        if (queue non-empty && (force || front.when <= now_ms)) {
            spawn(force, 0, front.next_delay); pop(queue);
        }
    }
}
```
Consequences:
* at most **one spawn per spawner per tick**;
* the population is **not** scaled by nearby players (only the placement test uses players);
* a spawner below `min_population` spawns immediately, ignoring the delay;
* the spawn delay is a stored value (`+512`), refreshed by every spawn (`Spawn`, arg 4: the queue
  entry's 1000/100 ms). The `spawn_delay_min..max` variation only applies to the queue entry made by
  `QueueSpawn` after a removal.

## 6. Queue

```
QueueSpawn 0x393438D0:
  d = uniform(spawn_delay_min, spawn_delay_max) * 1000
  push({now + d, 1000})

QueueSpawnAll 0x39343990(a1, delay_ms):
  for i in 0 .. maxPopulation - spawned: push({now, delay_ms})   // due now; delay_ms becomes
                                                                // the post-spawn wait (via +512)

push = 0x3934D9A0 (binary heap), comparator 0x3933B700 (when_ms)
front = 0x3934CB90, pop = 0x3934CBE0
```
`InitFromSpawnerParam` calls `QueueSpawnAll(1000)` (100 in dev mode, `gEnv+290`). The entries are due
immediately, but every successful `Spawn` sets the next-delay to the entry's 1000 ms, so a spawner
fills at **one NPC per second** (10 per second after an activate event, which uses 100 ms). `OnEvent` 1/5/7/8 call `QueueSpawnAll(100)` (`OnEvent` 1) or
`SpawnAllOnce`.

## 7. `Spawn 0x39348AF0` in order

```
*a1+516 = now_ms
desc = NpcSpawnerDesc[a1[2]]; if none -> retry 60 s
members = desc+64 vector; if empty -> retry 60 s
if (persistent(+344) && instance id(+348) == current instance id dword_3A607DB4 && members ...)
    force the member index from the saved instance record
else if (count(+368) != 0) pick the member with max(weight[member] - count[member]/count)
else pick a weighted-random member
member = members[idx]
representative npc = member+8 (Npc) or the first member of the group with is_leader (member+36&1) -> +24
brief = GetNpcBriefData(rep)         // 0x3933B680: fills {flags, scale, ...}
if (area type == 0) {
    point = random(points)
    if (!force && (IsSpawnPointFree(point) || otherNpcNear(point))) { suspend/retry }
} else if (area type == 1) {
    tri = pick triangle with max(weight - filled/area_count), random*0.001 as tie-break
    attempts = (brief.LOWORD == 0 ? 50 : 5)
    for each attempt:
        barycentric random point from the 3 vertices
        if (brief.LOWORD == 0) z = terrainHeight(point) + 0.1
        else { waterZ = AI->vt+568(point); if (waterZ > z) z += 0.1 ... }
        if (!IsSpawnPointFree(point) && !otherNpcNear(point)) break
    yaw = random 0..2π
}
if (member type == 1) SendSpawnNpc(pkt)         // ValidateSpawn + queue
else                  SpawnNpcGroup(pkt)        // per-member packets
on success: ++count, ++member.count, (area) ++triangle.count, *(a1+512) = arg4
on placement failure: *(a1+512)=0 and return 0
on data failure:      *(a1+512)=60000 and return 0
```
* `IsSpawnPointFree 0x39347470` uses `desc+56` (`test_radius_pc`): scans units around the point and
  returns 0 (free) when the radius is < 0.01; otherwise 1 when a player is inside.
* `0x39340E40` uses `desc+52` (`test_radius_npc`): counts live NPCs of this spawner within the
  radius; also returns 0 when the radius is < 0.01.
* `ValidateSpawn 0x39347A00` calls `0x39347670` (terrain/triangle validity) and logs
  `Spawn pos occupied over 30 times test` when `+356` exceeds 30.

## 8. Group spawn `0x39344530`

* Group desc from `packet+32` (ZWSpawnNpc `SpawnInfo.type`? — see §10; it is the group id of the
  member chosen by `Spawn`).
* Member list = `group+16` vector of 40-byte member descs.
* Leader = first member with `member+36 & 2` (`is_move_leader`). Missing leader →
  `Failed to find the leader of NPC Group(%u) - spawner ID (%u)` and the spawn fails.
* Formation setup `0x3934CA40(&ctx, leaderPos, leaderYaw, leaderIdx)`: stores position, yaw
  `cos/sin(yaw*0.5)`, the leader's own offset, and ground-snaps (`terrain - 0.1`) by +0.4.
* `0x3934BFB0(&ctx, out, memberIdx)`: `out = leaderPos + rotYaw(memberOffset - leaderOffset)`, then
  optional AI ground snap (`IAISystem->vt+1552`).
* Group runtime id: pool `0x39344350`, max 50,000, alive mask `~(-1 << memberCount)`, member count,
  `enable_respawn` (+25868), 20 per-instance spawner-id slots.
* For each member: rewrite `ZWSpawnNpc` (position from the formation, member index, npc id, type 5)
  and send; record the spawner id in the group entry.
* `HandleGroupMemberRemoved 0x39343310` clears the alive bit; when the mask reaches 0 it logs
  `Group(%u, "%s") removed`, frees the group and returns 1 (the spawner then decrements its count).

## 9. Spawn/despawn messages

### `ZWSpawnNpc` (op 2), class `ZWSpawnNpcPacket` (vtable `0x39FDFFD8`)
Serializer (bidirectional field schema) `0x3938E0E0`, embedded `SpawnInfo` at packet+16:
```
+0   sid        u32   spawner uid
+4   sType      u32   spawner desc id (npc_spawners.id)
+8   mIdx       u8    member index
+9   pIdx       u8    point index
+10  tIdx       u16   triangle index
+12  type       u32   npc template id
+16  grpId      u32   (sub_3938E040: "grpId", optional flag "grpMemIdx")
+20  grpMemIdx  u8
+28  pos        x,y,z
+40  zRot       f32
+44  scale      f32   (Spawn sets 1.0)
+48  (40-byte sub-object, sub_39387CA0)
+88  name[1025]
+1113 masterName[129]
+1244..1280 extras (fields copied from the saved instance record)
```
`Spawn` writes exactly these fields (DD.c ~857700-857900); the `packet+16` view matches `mIdx` at
+24, `pIdx` +25, `tIdx` +26 (the WZNpcState echo uses +20/+21/+22 in its own header).

### `WZNpcState` echo (op 2 world→zone) — `HandleSpawn 0x39347F90`
Reads `+20` member index, `+22` triangle index, `+17248` spawn effect time, `+17260` a byte, and the
created unit (`a3`); `NpcManager::Create 0x39348440` reads `+1192` (member/obj id?), `+1196` npc id,
`+17248` spawner id, `+17252` spawner param block (group id/member index/position).

### `ZWRemoveNpc` (op 3) / `ZWCreateBuff` (op 10)
`RemoveSpawnedNpcs 0x39346050`: for each unit, if it is a pet-like unit and the despawn effect was
requested → `ZWCreateBuff(unit, dword_3A518F10)`; else → `ZWRemoveNpc(unit)`. The buff id is a
hardcoded global (10.x; whether 1.2 has it is unknown).

### `ZWSaveInstanceSpawner` (op 5)
`HandleSpawn`: `sub_39362700(pkt, spawnerUid, unitId, instanceId != savedInstanceId)`.

### `WZNpcSpawnerEvent` (slot 112)
`OnNpcSpawnerEvent 0x39364140`: spawner id at packet+16, then `NpcSpawner::OnEvent`. Schema
`0x3938DF30`: `spawnerId, spawnerEvent, <sub_3938DE50>, <sub_39387CA0>, type,
despawnOnCreatorDeath, useSummonerAggroTarget, lifeTime`. The zone's handler uses only
`spawnerId`/`spawnerEvent`/(owner type, tower-def id).

### `WZSpawnerList` (slot 5)
Entry schema `0x39387FD0`: `zoneId, zoneOrigin, spawnerId, spawnPos, pos`. The handler is not
directly referenced (packet functor vtable only); `0x3936E940` is the best candidate: it deactivates
and clears the manager's spawner registry, rebuilds it from a list of 16-byte entries
`{id, type, active}`, and re-activates the flagged ones. Marked **[I]**.

## 10. Event table (`OnEvent 0x39349800`)

| Event | Action | Notes |
|---|---|---|
| 1 | `+355 = 1`; clear queue (`sub_39D2FE70(+464)`); `QueueSpawnAll(100 ms)` | activate |
| 2 | `+355 = 0`; clear queue | deactivate (no despawn) |
| 3 | `+355 = 1`; clear queue; `Spawn(force=1, savedInfo, 0)`; on failure `+355 = 0`, `+352 = 0` | persistent-instance spawn |
| 4 | `+355 = 0`; clear queue; `+352 = 0` | disable |
| 5 | `+355 = 1`; if owner type != 4 `RemoveSpawnedNpcs(0)`; `SpawnAllOnce` | reactivate + refill without despawn buff |
| 6 | `+355 = 0`; `RemoveSpawnedNpcs(owner type == 2)`; clear queue | deactivate + despawn (buff for owner type 2) |
| 7 | `+355 = 1`; `SpawnAllOnce` | |
| 8 | `+355 = 1`; `SpawnAllOnce`; `+355 = 0` | one-shot spawn |
| 9 | `0x39342920` (for each spawned npc, respawn its lost group members) | reset/groups |

Gates: `+352` must be set (event handling enabled); `+353` (tower-def) only handles owner type 1
and logs the tower-def id; `+354` (game-schedule) only handles owner type 2; the rest log
`ignored. Cuz it's the spawner for ...`.

## 11. Despawn / respawn

`HandleNpcRemoved 0x39344AA0`:
```
HandleGroupMemberRemoved(group, groupId, memberIdx) -> v8 = group fully gone
reset the unit's AI properties (AI vt+512 with {type, -1, 0})
remove the unit id from per-member list[memberIdx]; if (v8) --memberCount
if area spawner: remove from per-triangle list, --triangleCount
if (v8) {
   --count(+368)
   if (persistent(+344)) active(+355) = 0
   else QueueSpawn()                  // respawn after spawn_delay_min..max
}
```
So a normal spawner **does not respawn per NPC**: it respawns only when the whole group is gone
(or when the NPC is a single `Npc` member, where the group is that one member). A group that lost
one member waits until the whole group is dead before a new group spawn.

`HandleNpcSpawnFailed 0x39344900` is the error path: decrement the member/triangle counters and
re-queue.

## 12. Data checks run (read-only)

```
1.2 compact.sqlite3:
  npc_spawners 15,275; npc_spawner_npcs 15,967 (Npc 15,820 / NpcGroup 147);
  npc_groups 158 (aggro_rule_id 0:31, 1:101, 2:26; enable_respawn t:44);
  npc_group_members 571 (sizes 2..12; is_move_leader 157);
  npc_spawner_spawn_effects 2,245; npc_spawner_despawn_effects 141;
  activation_state t 14,645; min_population>0 956; start/end non-zero 142;
  destroyTime non-zero 140; save_indun t 366; suspend_spawn_count>0 41.
10.x server_decrypted.sqlite3:
  npc_spawners 22,982; npc_spawner_npcs 23,394; npc_groups 382;
  npc_group_members 1,568; spawn effects 7,161; despawn effects 2,788;
  aggro_links 180; npc_aggro_links 973.
```
`npcs` in 1.2 has every aggro-link column the zone reads (see `NOTES-aggro.md` §1); the spawn-effect
table lacks `use_summoner_faction` in 1.2 (`FEASIBILITY.md` §2).

## 13. Open points

* The exact meaning of the scan in `0x3934B0E0`/`sub_399CA750` after loading (sort comparator
  `0x3933D680`) is not decoded; it does not affect the spawn rules above.
* `0x39347B20`'s spawn records at +440: only `Save` (`0x3934B7A0`) is a proven consumer.
* `0x3936E940` is inferred (see §9).
* `WZActivateNpcSpawnersInArea` (slot 66) handler not located; the zone can also activate
  spawners through events and the tower-def/schedule code, so this is a coverage gap.
* The persistent-instance branch in `Spawn` (member index from the saved instance record) is only
  partly decoded; the code path is `persistent(+344) && +348 == dword_3A607DB4`.
