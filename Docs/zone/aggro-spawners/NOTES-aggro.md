# Working notes — aggro table (10.x zone)

Raw notes behind `MAP.md` §A. Addresses are VAs in `x2game-dev_dedicate.dll`; sizes from
`D:/aa-re/zone/bin/x2game-dev_dedicate.dll.funcs.csv`. Decompile line numbers are DD.c lines in
`D:/ArcheAge Returns 10.0.2.13 - 8yx - r575 - 2026-06-18/Bin64/decompiled/x2game-dev_dedicate.dll.c`.

## 1. How the functions were read

Helper used (scratch, not committed): a Python script that
1. loads `funcs.csv` into `{start: (end, size)}`;
2. finds the `//----- (00000000<VA>) ---` marker for a function and slices to the next marker;
3. greps `sub_<VA>(` in DD.c and maps the hit line back to the nearest preceding marker.

Sizes and line numbers below come from that; nothing was run or built.

## 2. Struct offsets, verified one by one

### 2.1 Constructor `0x3931EF20` (DD.c ~819xxx)

```
*(a1+8)  = owner NPC unit
*(a1+32/40/48) = 0            ; vector begin/end/cap
sub_394A2F40(a1+64)           ; map ctor
eh vector ctor (a1+136, 0x20, 3) ; 3 x 32-byte std::set
*(a1+0)   = dword_3A606298    ; "no group"
*(a1+96)  = dword_3A6062A8    ; top target = -1
*(a1+112) = -1.0f             ; minAPS off
*(a1+116) = 0 (word)          ; dirty = 0, lock/empty = 0
*(a1+124) = qword_3A4DC090; *(a1+132) = flt_3A4DC098  ; combat-start pos
*(a1+16) = a3*a3              ; return_distance^2
*(a1+20) = a4                 ; absolute_return_distance
*(a1+24) = a4*a4
*(a1+100/104/108) = -1
*(a1+120) = 0                 ; last remove reason
*(a1+232) = 0                 ; abuser report throttle
```
Called from `0x39346960` with `a3 = *((float*)npcDesc + 53)` (= desc+212) and
`a4 = *((float*)npcDesc + 1)` (= desc+4).

### 2.2 Entry creation `0x3931F040`

The 64-byte entry is assembled on the stack as `v23..v26` (4 × 16 bytes) and memcpy'd, so the
offset map is `v23` = +0, `v24` = +16, `v25` = +32, `v26` = +48:

| Write | Entry offset |
|---|---|
| `HIDWORD(v24) = a2[2]` (unit id) | +28 |
| `*(float*)&v24 = (stat62+100)/100` | +16 |
| `*(QWORD*)((char*)&v24+4) = float(distcoef)` | +20 |
| `*(QWORD*)&v25 = groupAvgPos.xy; DWORD2(v25) = z` | +32..43 |
| `HIDWORD(v25) = *v8` (target x) | +44 |
| `*(QWORD*)&v26 = *(v8+1)` (target y,z) | +48..55 |
| `DWORD2(v26) = now` / `HIDWORD(v26) = now` | +56, +60 |

`v8 = sub_392589F0(a2, &v22)` writes the target position into `v22`; `sub_3931D650` writes the
group average position into `v20/v21`.

Cap check: `((end-begin) & ~0x3F) == 0x1900` → 6400 bytes = 100 entries; over the cap one entry is
dropped first (`sub_3931FF80(a1, &v30, end-64)`).

Self check: `a1+8` is the owner unit; the code compares the pointer at `*(owner+8)` with the target
`a2` → log `Can't add myself to my aggro list table` (DD.c:821329).
Group check: `sub_3931E000(a1, a2)` → log `Can't add my group members...` (DD.c:821336). That helper
resolves the table's group (`*(a1+0)`) through the AI system and searches the given unit id in its
member list.
Validity: `sub_39261710(a2, owner)` must be non-zero (hostile/alive pair).

First entry (`v11 = begin == end` before insertion): `*(a1+124) = v20; *(a1+132) = v21` (pull
position), and if the NPC is a boss (`npcDesc+168` in 3/4/5/8) it logs
`HP at create first aggro = %d`.

Packet: `ZWMakeAggroTargetHostilePacket` vtable, op 13, `sub_39362CA0(pkt, npcUid, unitId)`,
enqueue via `sub_3936EDC0()` + `unknown_libname_2920` (DD.c ~8214xx).

### 2.3 `AddAggro` `0x39321340`

```
if (*(a1+117)) return;
if (!a2) log "No unit";
if (sub_3931DD40(a1, a2)) return;              // unit in an abuser set
find entry by *(entry+28) == a2[2];
mul = (stat90(attacker=a2) + 100) / 100;
if (entry) { entry[i] += int(ch[i]*mul); entry+60 = now; }
else {
  entry = CreateAggro(a1,a2);
  entry[i] += int(ch[i]*mul);
  if (*(float*)(a1+112) > GetAPS(entry)) RemoveFromTable(a1, a2[2], 9);
}
if (ch0+ch1+ch2 != 0) {
  *(float*)(entry+20) = GetDistanceCoefficient(a1, a2);
  sub_3931FCE0(a1, entry+28);   // dirty + manager notify
  sub_3931E240(a1, a2);         // abuser re-report (1 s throttle)
}
```
Callers: `0x39321740`, `0x39321540`, `0x39321A10`, `FindNewTarget 0x39345770`,
`InitializeAggroOnSpawn 0x39346960`.

### 2.4 `GetDistanceCoefficient` `0x3931D6F0`

Distance between the group average position (or the owner NPC, when the table has no group) and the
target, 3D; `dist² >= 100.0` → `0.83333331`, else `0.90909088`.

### 2.5 `GetAggroPerSecond` `0x39322950`

```
elapsed = now - *(entry+56)          // creation time, NOT last hit
aps = (ch0+ch1+ch2) * *(float*)(entry+16)
if (elapsed > 1000) aps /= elapsed/1000.0
```
So a long fight dilutes APS; this feeds the `minAPS` rule.

### 2.6 `UpdateAggro` `0x3931FD30`

Find entry; `newMul = (stat62(npc)+100)/100`; if `entry+16 != newMul` → store, mark dirty, notify.
This is how a buff that changes the NPC's stat 62 reaches an existing entry.

### 2.7 `CopyTopAggro` `0x3931FE00`

Find/create the entry; compute `maxTotal = max(ch0+ch1+ch2)` over all entries; set
`entry[2] += maxTotal - (entry[0]+entry[1]+entry[2])` so this entry's total equals `maxTotal`;
mark dirty and notify.

### 2.8 `TauntedBy` / `CancelTauntedBy`

`0x39320FE0`: `entry+24 = now`; if arg3 (`taunt_with_top_aggro`), `CopyTopAggro`.
Callers are in the buff manager (`0x3944FEB0`, `0x394501E0`, RTTI `TauntBuffFinder`). The columns
`buffs.taunt` and `buffs.taunt_with_top_aggro` exist in both 1.2 and 10.x DBs.
`0x3931F710`: `entry+24 = 0`, dirty = 1.

### 2.9 Sort `0x3931EB70`

Comparator walks from `begin+64` and moves an entry forward while
`tauntTime(entry) < tauntTime(other) || (tauntTime(other)==0 && total(other)*mul*distcoef > total(this)*mul)`.
In practice: taunted entries first (lower/zero timestamp sorts first), then the highest
`total × mul × distcoef`. Uses `sub_393229B0` for the swap (sets `entry+12 |= byte_3A5177F0`
during the move).

## 3. Removal and its packets

### 3.1 `RemoveEntryInternal 0x3931FF80`

```
unit = unit(entry+28)
erase entry from the sleep map (key unit) via sub_39AEFDF0
sub_39322AB0(entry+64, end, entry)   // clear taunt/watch bookkeeping
end -= 64
if (unit) sub_3935FAC0(unit, npcUid)  // remove the NPC from the unit's aggro-list-of
dirty = 1; manager notify (sub_3944DE00); share (sub_3931FA10)
```
No packet.

### 3.2 `RemoveFromTable 0x393210B0`

```
npcDesc = npc desc of the table owner
if (npcDesc && grade in {3,4,5,8}) {
  if (unit != -1 && reason < 18) {
    *(a1+120) = reason
    if (gEnv->pSystem->IsDedicated?) ZWAggroRemove(unit, npcUid, reason)   // op 65
  }
}
remove entry (swap), if empty -> *(a1+117)=1
```
**Only boss grades produce `ZWAggroRemove` here.**

### 3.3 `RemoveAll 0x393200D0`

Stores the reason if the table is non-empty and reason < 18; logs the target list for boss grades
(`Removing all aggros. Npc : %s(%d), Behavior..., AggroCount, AggroTarget`); removes every entry and
clears the sleep map. Called from the four `ProcessReturn` sub-checks with reasons 2, 4 and 1.

### 3.4 `RefreshTopTarget 0x3931F790`

```
if (entries non-empty) {
  if (dirty) { SortEntries(); dirty = 0; }
  top = entry[0].unitId
}
if (top != *(a1+96)) {
  if (top == -1 && boss) {
      old = unit(*(a1+96));
      if (old && old kind == 0) ZWAggroRemove(old, npcUid, *(a1+120));   // op 65
      log "AGGRO => Npc(...) will be return : aggroNum, last target, reason"
      *(a1+120) = 0
  }
  *(a1+96) = top
  CollectSharedAggroMembers(list)
  for (npc in list) Npc::_OnAggroTableChanged(npc)
}
*(a1+117) = 0
```

### 3.5 `ProcessAggroCancel 0x393204B0` (order of checks)

1. `CollectSharedAggroMembers` → list (owner NPC at least).
2. `v56 = cv(qword_3A607D60) && npcDesc && (npcDesc+657 & 0x10)` — the under-terrain rule.
3. For each entry:
   - unit lookup fails → log `aggro remove : unit missing : unitId(%u)`, remove.
   - unit is not hostile (`sub_39261710`) to any member of the shared list → log
     `unit isn't a valid target`, remove.
   - unit is asleep (`unit+32081`): if total > 1, stash the entry in the +64 map and set
     `{0,0,1}`; on wake, add the stashed channels back and erase the stash.
   - APS: `(sum*mul)` / elapsed if > 1 s; if `*(float*)(a1+112) > aps` → log
     `APS of the unit(%u) is too low`, remove.
   - under-terrain: `v56 && (now - entry+60) > 2000` and `unit.z < terrainHeight - 1.0` → remove.
   - leash (only when `IAISystem->vt+904` exists and its `vt+16` is non-zero, and
     `*(float*)(a1+16) > 0.1`):
     * CVar `qword_3A607CD0` (position method): 0 → compare entry+32..40 with the current group
       average position; 1 → compare entry+44..52 with the unit's current position.
     * If `dist² > *(float*)(a1+16)` and `now - entry+60 > (qword_3A607CD8 ? its value : 3000)` →
       log `unit is aggro cancelled`, remove.
4. No packets.

Note: the "return distance" used here is `table+16` = `return_distance²`; `table+24`
(`absolute_return_distance²`) is used by `ProcessReturn`.

### 3.6 `ProcessReturn 0x393211E0`

```
if (boss grade) {
  spawnPos = sub_3933C9C0(npc)
  if (dist2(npcPos, spawnPos) > (2*abs)^2) { log "AbsoluteReturnDistance name, pos"; RemoveAll(2) }
}
if (entries non-empty && (!IAISystem->vt904 || vt904->vt16 != 0)) {
  sub_39320C90(table, npcPos, table+24)     // abs_return from pull pos
  sub_39320440(table, npcPos)               // housing area
  sub_39320DF0(table, npcPos)               // not combat area
}
```
`sub_39320C90` compares `table+124..132` (combat-start) with the NPC position, so it is "distance
travelled since the pull", not "distance to the target".

Ordering note: because the generic check is the `else` of the boss check and uses `abs` (1×) from
the pull position, it normally fires before the boss's 2× rule; the 2× check only matters when the
first aggro entry was created while the boss was already far from its spawn point.

## 4. Abuser sets

* Sets: `std::set<u32>` at `table+136`, `+168`, `+200`, 32 bytes each (`eh vector ctor` count 3 in the
  ctor).
* `IsUnitInAbuserSet 0x3931DD40`: linear over the 3 sets; hits make all mutators skip the unit.
* `CanRegisterAbuser 0x3931DDC0`: returns 1 when the unit is **not** in any set, gated by
  CVar `qword_3A607D58`, `npcDesc+660` sign bit (`use_abuser_list`), unit type 0 (character) and a
  non-empty table.
* `UpdateAbuserRegistration 0x3931E240`: at most once per second (`+232`); requires
  `CanRegisterAbuser`; registers channel 0 when the unit is the current top target and
  `sub_3931D890` says it is visible/in perception; registers channel 2 when
  `sub_3931D000` says it is under the terrain.
* `RegisterAbuser 0x3931E180`: inserts into the chosen set, sends `ZWRegisterNpcAbuser` (op 61,
  `sub_39362CD0`: npcUid, unitId). Also called from `ServerAIUnitSystem::RegisterAbuser`
  (`0x39327360`), which is the Lua-facing entry point.

`0x3931D890` uses `IAISystem->vt+872` (returns 2 → `vt+600`) and `IEntity`/physics flags; most
plausibly a visibility ray test. `0x3931D000` compares the unit's z with `GetGroundHeight()-1.0`
and a npc flag (`+657 & 0x10`).

## 5. Feeding paths

### 5.1 `WZUpdateAggro` (slot 68) handler `0x39363890`

`a2` = packet object: `+16` npc unit, `+20` source unit, `+24` abuser unit, `+28` amount,
`+32` hostile flag, `+36` 16-byte aggro-source descriptor (+ `dword_3A606330` id at +16? — copied by
`sub_3931D0B0` into `xmmword_3A606320` / `dword_3A606330`).

Hostile → `0x39322500`: if the NPC's AI link state is 1, find its group (`*(npc+32)` → manager map
`qword_3A607A40+16`) and `sub_39321740(groupTable+104, src, abuser, &{0,0,amount})`; then
`0x39321C80(npc, src, abuser)`.
Friendly → `0x39321830`: iterate a visitor set built by `0x392A4020`, find each spawner-player entry
(`0x393410B0`), `sub_39321740(entry+104, src, abuser, &{0,0,amount})`.

The `int[3]` passed is `{0, 0, amount}` → **channel 2**. The source descriptor strings seen in the
logs are `By UpdateAggroHostile` / `By UpdateAggroFriendly` (`sub_3931D0B0`).

### 5.2 `WZUnitDamaged` (slot 48) `0x39363580` → `0x393225F0(body, mult)`

Body offsets: `+24` source type, `+32` / `+44` owner ids, `+64` attacker, `+68` damaged unit,
`+88` damage. Flow: resolve the damaged unit (kind `+24`); for kind 1 look up the owner record
(`sub_393410B0(unit+32)`) and call `OnAffectedByDamage(owner, body, mult)`; kinds 2 and 5 (and 0)
skip that; other kinds return. Then `0x39321C80(damaged, unit(source), unit(attacker))`.
`OnAffectedByDamage 0x3933C8D0` builds `{int(damage * mult), 0, 0}` → **channel 0**, and adds the
damager plus its pet/doodad owner (`source type 2 → +48`, `5 → +40`).

## 6. Target acquisition

`0x39345770` preconditions: `npc+112 != 0` (AI active), `npc+53 != 0`, current top target invalid
(`unit(*(npc+104)+96) == 0`), npc entity/actor valid.

`CollectTargetCandidates 0x39345570`: CryAI perception iteration (`IAIObject->vt+136` StartIter,
`vt+144` Next), for each: `entity->vt+256` → unit id; keep if `sub_3932C4B0` says relation bit 0
(hostile) and `sub_39261710(unit, npc)`; append `{distSq, unit*}`; sort ascending (`sub_390E15B0`).

Radii: `v12 = actor->vt+48` (AI params); `*(float*)v12` = sight range, `*(float*)(v12+4)` = attack
range; `*(byte*)(v12+108)` = "aggressive/attack on sight" flag.
* Inside `attackRadius²`: if flag +108 → `AddAggro(unit, "new target")` + `SetAggroTarget`
  (`sub_393226C0`), done. Else (no aggro) → potential **alert** target.
* Outside: if no aggro and `candidate+4` (LOS) and inside `sightRadius²` → alert target.
At the end, if no target and there is an alert candidate, take the nearest and raise
`OnAlertTargetChanged` with the entity id (`sub_3933B970`).

`GetEffectiveAggroRange 0x3933BDC0`: `base * (100 + stat61)/100`, then `sub_39B1B950` (a range/LOS
query around the NPC).

## 7. Spawn-time aggro (`0x39346960`, called from `NpcManager::Create 0x39348440`)

* Lua AI params written: `spawnState = 4`, `spawningEffectTime = <int arg>` (packet +17248),
  `Properties.groupid = spawnerParam.groupId`, `formationTension` = the member's formation tension;
  when the member is the move-leader, `IAISystem->vt+816(entity, index, count)`.
* Table ctor with `npcDesc+212` (return) and `npcDesc+4` (absolute).
* If the spawner member is an `NpcGroup` (group desc type 2), `table+0 = spawnerParam.groupId` so the
  table joins the AI group.
* If `npcDesc+656 & 8` and the unit has a "creator" (`npc+40`) and that unit is hostile
  (`sub_3932C4B0 == 1`):
  * `spawnState == 0` → write `reservedAttackTargetEntity` = the creator entity;
  * else → `AddAggro(creator, "By spawn-hostility")`.
  If the relation is 3 (friendly) and `npc+45 != 0`: take the creator's current target (or its
  group leader's target, `0x39346340`) and `AddAggro(target, "By spawn-friendly")`.

## 8. Exact packet bodies seen

| Packet | Op | Body | Evidence |
|---|---|---|---|
| `ZWMakeAggroTargetHostile` | 13 | npcUid, unitId | `CreateAggro`, `sub_39362CA0` |
| `ZWChangeTarget` | 23 | npcUid, targetUid | `0x3935CCC0`, `sub_39362910` |
| `ZWAggroTargetChanged` | 46 | npcUid, targetUid | `0x3935CCC0`, `sub_39362B00` |
| `ZWAiAggro` | 52 | 128 × `unitAiAggro` records | `0x3944DA20` |
| `ZWRegisterNpcAbuser` | 61 | npcUid, unitId | `0x3931E180`, `sub_39362CD0` |
| `ZWAggroRemove` | 65 | removedUnitId, npcUid, reason | `0x393210B0`, `0x3931F790`, `sub_39362D00` |

`ZWAggroRemovePacket` vtable `0x39FD9EB0`; `ZWRegisterNpcAbuserPacket` `0x39FD9E38`;
`ZWMakeAggroTargetHostilePacket` `0x39FD9E10`; `ZWChangeTargetPacket` `0x39FE4F08`;
`ZWAggroTargetChanged` `0x39FE4F30`.

## 9. Things that looked surprising (and how they were resolved)

* `ProcessAggroCancel` uses `+16` (`return_distance²`) not `+24`; the log calls it "aggro
  cancelled", which is the per-target leash. `ProcessReturn` uses `+24`.
* `RemoveFromTable` sends `ZWAggroRemove` only for boss grades — corrected `AI-ORACLE.md` §2.4 in
  `MAP.md`.
* `GetAggroPerSecond` divides by **creation** age, not last-hit age, although the removal rule uses
  last-hit age. Two different clocks in one function.
* The three `std::set`s are not "aggro by channel": they are the abuser registry, and the same sets
  are consulted by every mutator.
* `sub_3931E2F0` falls back to appending the owner NPC when the table has no group, which is what
  keeps the "valid target" check from removing everything for a solo NPC.
