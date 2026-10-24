# 10.x zone AI: goal operations, skill choice, navigation and movement — function map

Static study of the retail **ArcheAge 10.0.2.13 (r575)** dedicated zone server, for comparison with
the 1.2 client protocol/server behaviour and with our Rust `aaemu-game` AI. Nothing was run; this is
decompile and data reading only. **The published CRYENGINE source was not used**; everything below
comes from the shipped binaries' code, their own log strings/RTTI, the shipped Lua scripts, and the
shipped databases.

Companion notes (same folder):

| file | contents |
|---|---|
| [`goal-ops.md`](goal-ops.md) | every goal op: opcode, constructor, Execute, struct fields, flag bits, result codes |
| [`movement.md`](movement.md) | NPC → `ZWUnitMovements`: send decision, CVars, ring buffer, 336-byte entry layout, speeds, cadence |
| [`functions.tsv`](functions.tsv) | one row per function: address, size, callers/callees, fields, verdict, Rust counterpart |

Builds on (does not redo): `D:/aa-re/zone/ai/AI-ORACLE.md` (the Lua behaviours, aggro table, target
acquisition), `D:/aa-re/zone/feasibility/FEASIBILITY.md` (WZ/ZW split, table loaders),
`C:/Users/stsiakal/AAEmu/rust/docs/physics/ZONE-PHYSICS-USAGE.md` (navmesh/physics). Corrections to
AI-ORACLE found while mapping are marked **[correction]**.

Conventions

* Addresses are VAs. `DD.c` = `D:/ArcheAge Returns 10.0.2.13 - 8yx - r575 - 2026-06-18/Bin64/decompiled/x2game-dev_dedicate.dll.c`
  (image base `0x39000000`); `CAI.c` = `D:/aa-re/zone/out/cryaisystem_dedicate.dll.c`
  (base `0x31000000`). `DD.c:LINE` is given for the large functions.
* `sub_VA` names are the decompiler's; class/op names in quotes/`code` come from the binaries' own
  log strings or Lua. A name without such an anchor is marked *(inferred)*.
* Evidence tags: **[V]** read in code/data, **[I]** inferred from code shape/constants, **[?]** unresolved.
* Verdict "transferable to 1.2?" is about the *function*: `yes` = the same behaviour/table exists in
  1.2, `likely` = strong but indirect evidence, `no` = 10.x-only, `unknown` = no 1.2 source to check.
  A 1.2 zone binary does not exist in our sources; 1.2 facts come from the 1.2 Lua and `compact.sqlite3`.
* Every difference from our Rust is a **SUSPECT**, not a bug, until reproduced against the running
  10.x zone (or a 1.2 client observation).

---

## 1. The two layers, and where the mapped code sits

```
 1.2 / 10.x Lua (game/scripts/ai)                 C++ (this document)
 behaviours, characters, pipes  ─── PushGoal ──►  CGoalPipe + COP* (cryaisystem_dedicate)
       ▲            │                                  │  Execute() moves the AI actor,
       │ events     │ SelectPipe/InsertSubpipe          │  raises goal-pipe signals,
       └────────────┘                                  ▼
 x2game Npc (DD.c): Npc::Update, FindNewTarget, CheckPos, _FollowPath/_FollowUnit,
 GotoAnchor, RunAICommandSet, ServerAIUnitSystem (skill pick/use) ──► ZW packets
```

The zone owns NPC movement, pathing, the aggro table and skill *choice*; the world owns stats,
damage, buffs and skill *execution*. The zone asks the world to cast with `ZWStartSkill` (op 24,
`0x39326730`) and gets the results back as WZ events (`FEASIBILITY.md` §4). Our server has no zone,
so both halves are in-process.

Two vtables matter for reading the calls:

| vtable | base | selected slots |
|---|---|---|
| `ServerAIUnitSystem` | `0x39FDD830` (RTTI `0x3A205DA8`, dtor `0x394A23D0`) | +72 UseSkill `0x39326730`, +88 StopSkill `0x39325940`, +96 CanUseSkill `0x39326980`, +104 IsCasting `0x39213680`, +112 StopCasting `0x393259F0`, +144 AddBuff `0x39325AF0`, +152 DelBuff `0x39325B80`, +312 PickFlySkill `0x393292E0`, +320 PickAiSkill `0x39329E00`, +328 PickSkill `0x393289C0` [V] |
| `CGoalPipe` | `0x311E4970` | +8 PushGoal `0x310F6574`, +16 PushSubPipe `0x310F7434`, +24 Label `0x310F6414`, +56 opcode-by-name `0x310F5994`, +64 name-by-opcode `0x310F59A4` [V] |

The AI system is reached from `gEnv->pAISystem` (`qword_31209C98`) through
`GetAIObjectManager()+704` and `+912`; the goal-op code reaches the game side only through these
vtable calls, so almost no `ServerAIUnitSystem` function has a direct call site (they are indirect).

---

## 2. Goal pipe engine

### 2.1 Name table and opcode space

* 40 `(name, code)` pairs at `0x311FF4B8`: `acqtarget=1, lookaround=2, approach=3, follow=4,
  followpath=5, backoff=6, strafe=7, bodypos=8, run=9, timeout=10, pathfind=11, locate=12, trace=13,
  signal=14, ignoreall=15, devalue=16, forget=17, hide=18, form=19, stick=20, clear=21, branch=22,
  random=23, lookat=24, continuous=25, move=26, charge=27, waitsignal=28, animation=29, animtarget=30,
  usecover=31, wait=32, adjustaim=33, proximity=34, movetowards=35, dodge=36, useskill=37,
  flyuseskill=38, bind=39` [V]. `sub_310F5994` (`CAI.c:186212`) is the linear name→code lookup (returns
  **40** = "not a goal op" = subpipe); `sub_310F59A4` is the reverse [V]. Codes are the ones the Lua
  parser uses to validate `AI.PushGoal`.

### 2.2 `CGoalPipe::PushGoal` — `0x310F6574` (3765 B, `CAI.c:186771`)

Builds the pipe entry and one COP object. The switch is on the goal struct's opcode; observed
constructors are listed in `goal-ops.md`. Points that matter:

* it writes the 104-byte entry into the pipe vector (`i+8` opcode, `i+68` relative jump offset,
  `i+96` blocking flag, `i+100` grouping; the rebuild reads them as `entry+24+72/+76`) and calls
  `sub_310F8D60` to append [V].
* code 40 is a subpipe (`sub_310F8D60` path) and is also the special case in `PushSubPipe`.
* the "blocking" and "grouping" flags come from the Lua call (`AI.PushGoal(name, blocking, ...)`).
  Blocking selects whether a `4` ("pending") result stalls the pipe.

### 2.3 Label fix-up and `branch` — `sub_310F7604` (483 B, `CAI.c:187660`)

After the matrix is built, every entry whose opcode is **21 or 22** gets a label lookup; the
difference between the label's goal index and the current index is written to the entry at `+68`
(backward/forward jump), and an empty loop is logged and ignored
(`"Label %s not found in goal pipe %s."`, `"Empty loop at label %s ignored..."`) [V]. **The `branch`
op's control flow therefore lives in the pipe (precomputed relative jump), not in the COP object.**
The COP object created for clear/branch/devalue (vtable `0x311E4690`, Execute `0x310D7308`) only
performs the op's side effect.

**[?]** the exact stored opcode differs by one from the table code for most ops (approach table 3 →
factory case 2, stick 20 → case 19, useskill 37 → case 36) while branch/clear are tested as 21/22
(`CAI.c:187700-187735`). The parser is at `CAI.c:2635xx`; the store was not resolved.

### 2.4 Result codes [I]

Observed across the OPs: `1` = keep going (in progress / continue), `2` = succeeded (`COPLocate`
`0x310DBE38` returns 2 on found), `3` = failed (`COPLocate` 3, `COPApproach` no target 3),
`4` = pending/blocking (`COPTimeout` returns 4 until the time elapsed, then 1). Non-blocking ops
(e.g. `run`, pushed with blocking=0 in `common_DoNothing`) return 4 and the pipe continues.

---

## 3. Goal operations in scope

Full field/flag maps are in [`goal-ops.md`](goal-ops.md). Summary of behaviour, with the movement
side emphasised:

### 3.1 `run` — ctor `0x310D6DD8`, Execute `0x310D6E18` (103 B)

Writes the AI actor's speed fields `+1192` and `+1196` to `fValue` (0, 1 or 2 in every shipped pipe),
then, when the duration `+24` is > 0 and the actor has the runnable flag, calls `0x3116783C`
(`CAIObject` action vector `+4432..4440`) [V]. The fields are consumed by the movement/animation
side: `0x3100675C` clamps a value ≥ 1.0 to the walk level **0.4** when the actor may not run, CPuppet
maps a required path speed to the gait levels **0 / 0.21 / 0.4 / 1.0 / 1.4** and writes `+1192`
(`0x3116847C`), and the path follower `0x3113E5D0` advances by that value [V]. So the pipe's `run`
value is the AI's requested movement speed (0 idle, 1 follow, 2 combat, followPathSpeed for paths:
`pipe_manager_x2.lua:22-54`, `attack.lua:18`, `follow_unit.lua:28`, `follow_path.lua:33`); the exact
m/s mapping of the gait levels is **[I]**, and `0.4` is the engine's walk cap.

### 3.2 `timeout` — ctor `0x310DAC48`, Execute `0x310D6BB8` (110 B)

Two floats (`min`, `max`; `max<=0` means `min` = fixed). Returns 4 until `elapsed >= fValue`, then 1.
Used as pause (roaming 0.5-2 / 5-6 s, hold 2 s, alert 1 s, return 1 s, dead 2 s).

### 3.3 `signal` — ctor `0x310F8150`, Execute `0x310D71D8` (296 B)

Builds a 0x40-byte signal object (`0x3104B388`), copies the signal id (`a1+16`) and filter (`a1+52`),
and raises it through the script/AI event path (entity vtable+32 at `a2+232`, or the AI system
vtable+624 for the non-entity case), once (`a1+48` guards). Return 1. This is the only way Lua
behaviours receive goal-pipe control (`OnAITimeEvent`, `UpdateIdleEnd`, `OnUseSkillDone`,
`GO_TO_*`, `OnPathStuck`…) — our Rust has no pipe signals and calls the behaviour functions directly.

### 3.4 `branch`, `clear`, `devalue` — object vtable `0x311E4690`, Execute `0x310D7308` (148 B)

The object carries only two bytes (`+16`, `+17`); Execute either clears/sets the pipe user's
"ignore all" state at `a2+2304` vtable+144/+64 or calls vtable+784 with `+16`. The conditional jump
itself is the precomputed `+68` offset (§2.3). `ignoreall` is a separate class (`0x311E4630`,
Execute `0x310D5E18`) that writes the pipe user's byte `+41` [V].

### 3.5 `bodypos` — ctor `0x310D6D68`, Execute `0x310D6D88` (71 B)

`BODYPOS_*` bitmask: either writes it to `a2+244` and issues body position `-1`, or forwards the
mask to `0x3116786C` (`CAIObject+4448` body request, `+4452` counter) [V]. Our Rust maps this to
`GameStance`/posture broadcasts (different mechanism).

### 3.6 `lookat` — ctor `0x310D7848`, Execute `0x310D5F38` (15 B)

Sets `CAIObject+3188` to the argument bool. The four-argument form used by Lua
(`lookat, 1, 0, 180, false, AI_LOOKAT_USE_BODYDIR`) is handled by the engine's look-at later; the
COP only toggles the "look at target" flag [V/I].

### 3.7 `strafe` — ctor `0x310D6C38`, Execute `0x310D6C88` (203 B)

Resolves the pipe user and its actor, then writes the strafe request (`0x3116771C`:
`CAIObject+4416` distance, `+4420` second limit, `+4424` bool) or a local direction pair when no
actor exists. Lua calls `strafe, 2, <angle>, <time>` every combat pipe (`attack.lua:16`,
`pipe_manager_x2.lua:109-168`) [V]. `npc_strafe_params` is a 10.x-only table (DB check: absent in 1.2
`compact.sqlite3`, present in the 10.x server DB), and `ServerAIUnitSystem::GetNpcStrafeParamDesc`
is a 10.x vtable slot `0x39327260` [V].

### 3.8 `locate` — ctor `0x310F8054`, Execute `0x310DBE38` (385 B)

If the name is non-empty, asks the pipe user for the object/refpoint by name (vtable+376), with an
optional max distance (`+28`, zero if farther); otherwise resolves a "formation"/typed point at the
agent position through the AI system (vtable+184). Writes the located target (`sub_31158734`).
Returns 2 found / 3 not found [V]. The Lua `roaming` pipe does `locate refpoint` after the roaming
position was written by `AI.SetRefPointPosition` (`pipe_manager_x2.lua:100`). Our Rust has no
refpoint registry; it computes positions directly.

### 3.9 `approach` — ctor `0x310E06A8`, Execute `0x310E4F88` (2610 B, `CAI.c:174950`)

Fields: `+16` path length, `+20` computed approach distance, `+24/+25/+26/+27` booleans
(use target/anim target/…), `+28` end distance, `+32` duration, `+36` approach distance,
`+40` anim-target flag, `+48` subpipe name, `+56` target id, `+64` stall timer, `+88` trace,
`+96` pathfind. Behaviour [V]:

1. target = attention/attack target (`a2+2304` vtable+64, fallback `a2+2368`); none → Reset, return 3.
2. if an approach/animation target, snap to its position; if `+24` and `+20==0`, set
   `+20 = distance(target) * +28`.
3. if no pathfinder exists (`a2+1601 == 0`): when already within the end distance, return 2
   ("No pathfinder and reached end"); otherwise create a **pathfind** goal (`0x310E1298`) with the
   end distance and duration and return.
4. otherwise create/continue a **trace** (`0x310DBFC8`) with `+32` as the distance limit, feed the
   stall timer (`0x310D64E8`), and if the actor has not moved for **1.5 s** raise `OnPathStuck` on the
   script (`0x3103AD40` + signal at `a2+232` vtable+32), reset the timer, and clamp the stall timer
   to 3 s.
5. finishes when the trace result != 1 (or returns 1 while running).

Note the pathfinder's end distance inside the trace is `max(float)` when `+32 < 0`
(`pipe_manager_x2.lua:102` passes `-1` for roaming to mean "no cap") [V].

### 3.10 `stick` — ctor `0x310E3C88` (0x4F8 = 1272 B), Execute `0x310F24D8` (5720 B, `CAI.c:183927`)

Ctor fields: `+1144` stick distance, `+1148` trace distance/time (`fValueAux`), `+1172` fValue
(clamped to 0.5 if ≤0), `+1152` speed-by-distance factor, `+1156` continuous (break flag bit 0
clear), `+1157` break bit 2, `+1158` break bit 4, `+1159` flag bit 1 (ignore attention target),
`+1160` flag bit 2 (try teleport), `+1162` bit 0x400, `+1163` bit 0x8000, `+1168` (continuous: bit
0x2000; else bit 0x20000 clear and 0x80000 clear), `+1176` bit 0x80000, `+1229` bit 8 clear;
defaults `+1048=1.0`, `+1096=10.0`, `+1100=20.0`, `+1104=3.0` (speed scale / stick distance /
far-teleport threshold / near distance) [V/I]. Execute:

1. takes the stick target (`+1128`) from the attack target or attention target; none → reset, 3.
2. if the target is an anim target (type 13), zeroes the stick distance; creates a **pathfind** to it
   (`+1240`) and a **trace** (`+1232`), then `RegeneratePath` (`0x310F18E8`).
3. near the target (within `+1172` or a target-distance query `0x310D5A18`) it may set the actor's
   speed to **0.5** (or **0.7** when a flag is set) — swimming/anim cases — and passes the stick
   distance to the trace.
4. returns 1 while tracing, 2/3 when done; continuous mode restarts instead of finishing.
5. `RegeneratePath` (`0x310F18E8`, 1203 B) re-issues the pathfind when the target moved, caps the
   path length with `+1148`, stores the destination at `+1108..1116`, and handles partial paths
   (`"COPStick::RegeneratePath No path"`).

`Reset 0x310DC558`, `OnObjectRemoved 0x310D75A8` are the lifecycle hooks; the object is also used
as a *sub-goal* by `useskill` (§4.4).

### 3.11 `backoff` — ctor `0x310E8748`, Execute `0x310EAF38` (3798 B, `CAI.c:179036`)

Ctor: `+64` = fValue (distance; abs when negative, overwritten in Execute as wanted distance),
`+68` = fValueAux (duration), `+152` = nValue (stop distance, 0.5 in `attack.lua:22`),
`+120` = nValueAux, flags → `+16/+88/+89/+148/+150/+149`; `+116=8` and `+20` = flag mask (default 8).
Execute [V]:

1. target = attack/attention target (`+160`); if the target is farther than `+152` it **completes at
   once** (2) and stops movement.
2. creates a refpoint named `"_BackoffPoint"` (`+184`), a **pathfind** goal named `"Backoff"`
   (`+176`) to it, and a **trace** (`+168`) with distance **0.1 m** (`dword_311FF428 = 0.1`); for an
   anim target it offsets the point by the target's facing × **10 m**.
3. if the path fails or the duration (`+68`) expires → `OnBackOffFailed` and return 3/2.
4. otherwise it tracks the farthest point from the target (`+136`) and returns 1.
This is the overlap fix of `attack.lua:21-22`: the `branch IF_TARGET_DIST_GREATER 0.3` skips it unless
the target is within 0.3 m, in which case the NPC backs off 0.5 m over 0.5 s (`backoffTime/backoffDist`,
`x2ai.lua:8-10`).

### 3.12 `followpath` — ctor `0x310E07A8`, Execute `0x310E95A8` (2025 B, `CAI.c:177896`)

Fields: `+88/+89/+90` loop/point-list flags, `+91` road flag, `+92` arg, `+124` trace distance
(10.0 in `follow_path.lua:34`), `+128` point-list flag. Execute [V]:

1. uses a **pathfind** named `"FollowPath"` with the path's start refpoint
   (`"<pipeuser>_PathStartPoint"`, `sub_3105AF48` type 2) and a trace; start point resolved through
   the AI path (`_PathStartPoint` set by `AI.SetPathToFollow`).
2. stuck detection: the same stall timer; **1.5 s** without progress → `OnPathStuck`.
3. loop mode (`+88/+89/+90`) resets the trace and starts over; end → returns 2/3.
4. errors: `"Unable to use follow path ... check path is not marked as a road"`,
   `"Unable to use point list ..."`, `"Unable to find path entry point ..."`.

The Lua behaviour supplies speed with `run followPathSpeed` and uses `PT_LOOP` to set the loop flag
(`follow_path.lua:26-35`).

### 3.13 `follow` (follow unit / leader) — ctor `0x310D6798`, Execute `0x310E59C8` (1362 B)

A separate, older follow op (not the one used by `follow_unit.lua`): resolves the follow target
(`+24`, attention fallback), creates a follow helper `0x3114EE8C` and a refpoint
`"<pipeuser>_FollowTarget"` (`+16`), averages the positions and traces to it (`+56` timestamp,
`+72` helper). The shipped `follow_unit` uses `stick`, not this op (`follow_unit.lua:20-35`).

### 3.14 `useskill` — see §4 (it is the only op that asks Lua for data).

---

## 4. NPC skill selection

### 4.1 `ScriptBindUnit::StartAiSkillLists` — `0x39354BB0` (2656 B, `DD.c:867154`)

Entry from Lua `entity.unit:StartAiSkillLists(aiPhaseChangeType, aiSkillLists)` (both versions:
`lua12/.../almighty_attack.lua:15`, `lua10x/.../almighty_attack.lua:15`). It parses the Lua tables into
the AI unit's phase list [V]:

* phase change type `a3` must be ≤ 3 (`if (a3 > 3)` falls through without setting lists). **10.x has
  no `ProcessAiPhase`**: the comment in 10.x Lua says "Selecting goal pipe have done in
  StartAiSkillLists() automatically depend on phase selection." (`lua10x almighty_attack.lua:16`).
  1.2 instead calls `entity.unit:ProcessAiPhase()` after the call and on `OnUseSkillDone`; that 1.2
  C++ entry point is not in the 10.x binary (grep: 0 hits) [V].
* it reads the per-phase skill list (`"skillLists"`), the options map (bool/float/int values,
  `"phase option error(%s)"` for other types), `startDelAiBuff`/`startAddAiBuff`/`endDelAiBuff`/
  `endAddAiBuff`, and logs `"Failed to phaseChangeType(%d)"` for out-of-range types [V].
* the default phase pipe name is the global `off_3A518F50 = "phase_base"` (`DD.c:137869`, used at
  `DD.c:867324` and `867732`); the selection of the pipe itself continues in the AI-unit phase
  machinery that this function feeds (the 10.x Lua comment relies on it), not in the loop here [V/I].
* pairs with `EndAiSkillLists` (`DD.c:867811` function) and the Lua `Destructor`.

**[correction to AI-ORACLE §1.5]**: the 10.x replacement is *inside* `StartAiSkillLists`, not a
renamed `ProcessAiPhase`; the automatic pipe selection is the `"phase_base"` default plus the phase
pipes created in Lua (`pipe_manager_x2.lua:104-180`).

### 4.2 `ServerAIUnitSystem::PickAiSkill` — `0x39329E00` (2048 B, `DD.c:829969`)

The skill-list picker used by `COPUseSkill` (vtable +320). It [V]:

* resolves unit, target and the AI-unit skill-list object; reads a phase state from the AI unit
  (vtable+88; values 0,1,3,4; 1/4 return early) and the list at +24 or +36.
* when the phase has a start/end skill and the phase changed, it tests the skill
  (`"into phase start skill logic"`, `"Tested phase start/end skill failed. skill=%u, result=%u, phase=%d"`).
* calls `sub_39327A20` (skill list) and, when nothing is found, `sub_39327CF0`
  (`"--AI Skill : no skill Found. find Nearest"`, `"check sequence list"` / `"check random list"`).
* list entries have a **time window, HP% window and useType (sequence/random)**
  (`AI-ORACLE` §2.7; logs `try/wait/fail sequenceSkill` at `DD.c:828174-828205`). When nothing is
  castable, the entry with the smallest remaining range gap is returned so the NPC approaches.
* `PickFlySkill 0x393292E0` is the underwater/fly variant (vtable +312), `PickSkill 0x393289C0` is
  the simple list+base-skill picker (vtable +328).

### 4.3 Candidate test and random pick — `sub_39327430` (1232 B, `DD.c:827772`), `sub_39328290`

Each candidate id is resolved and tested with its own use rules, including a predicted target
position for skill types 6/11/15/23 (`sub_39B0EF0/sublead` style, `DD.c:827867-827883`); survivors
are sampled **uniformly with `CryRandom`** (`DD.c:827999`: `CryRandom()*count >> 32`) [V]. `PickSkill`
then optionally retries the other aggro targets (`sub_39328290` at `DD.c:828451`) and finally the
NPC's `base_skill_id` when the "pick base in bad case" flag is set (`DD.c:829029-829081`, log
`"Can't find base skill desc"`). This matches AI-ORACLE §2.7; the 10.x addition is the predicted
target position.

### 4.4 `COPUseSkill` — ctor `0x310EA0B8` (230 B), Execute `0x310E6B68` (508 B, `CAI.c:176046`)

Ctor builds the request event name: **`"OnRequestSkillInfo"`**, plus `"_<tag>"` when a tag string is
given (`CAI.c:178423`). The flag dword is split into bytes `+32..+35`:
`bit1→+32`, `bit2→+33`, `bit4→+34`, `bit8→+35` [V]. From their use (below) and from the flag names
in the Lua, the mapping is **[I]**:

| byte | bit | USF flag | use |
|---|---|---|---|
| +32 | 1 | `USF_PICK_BASE_SKILL_IN_BAD_CASE` | passed as `a5` to PickSkill/PickAiSkill (base-skill fallback) |
| +33 | 2 | `USF_ALLOW_STICK_STOP_CHECK` | on "too far" with a changed target, re-request skill info instead of failing |
| +34 | 4 | `USF_AUTO_STICK` | vertical stick when `|dz| > 1.5 m` before casting |
| +35 | 8 | `USF_WAIT_UNIT_STATUS_PROBLEM` | when no skill could be picked, keep waiting instead of failing |

The six-state machine (`a1+48`):

| state | function | what |
|---|---|---|
| 0 | `0x310D9788` | raises the request event on the pipe user's AI script (`a2+29` vtable+32), stores the start time, state=1. Returns 1. |
| 1 | `0x310E4838` | waits for the skill list (`sub_311588E4`, **2 s** timeout → `"No response for skill info request or No skills"`), then PickFlySkill (+312) or PickAiSkill (+320) with the list and flags. If no skill and no valid target → fail (3). If **|target z − self z| > 1.5 m** and bit4 (AUTO_STICK): stop auto-attack and create a **COPStick(9999.0, 0.5, 0.5, 0, 12, 7)** as a sub-goal, state=2 (`CAI.c:174760-174774`). Otherwise state=3. |
| 2 | `0x310DF478` | runs the stick; if it takes >2 s or fails, sets the target's preferred combat distance to `(minRange+maxRange)/2` (if >0.5) and drops the stick; state=3. |
| 3 | `0x310DF568` | if the target changed and `IsTargetInSkillRange(...) == 15` (too far): with bit2 re-request skill info, else fail. Calls **CanUseSkill** (+96) and **UseSkill** (+80, `0x39326730`), logs `"use skill goal : unit ... skill ..."`, then state=4 (or fail 3 if the use was rejected). |
| 4 | `0x310DF928` | waits for start (2 s, `"start skill request timed out"`) and end (60 s, `"skill end or skill failure timed out"`) events; checks range again with tolerance 7.0/factor 1.0; state=5. |
| 5 | `0x310DFBD8` | finish/reset. |

A **30 s watchdog** logs `"#123757 AI maybe stopped. Goal:COPUseSkill..."` with the state, phase,
skill controller type and picked skill (`CAI.c:176062-176093`) [V].

### 4.5 `COPUseSkill::IsTargetInSkillRange` — `0x310D9B08` (403 B, `CAI.c:167496`)

Computes the distance to the target (through the AI proxies) and compares it with the picked skill's
range (`a2+3544` min, `a2+3548` max): returns **14** when distance < min range, **0** when
`distance <= a3 + a4 * maxRange`, else **15**. `COPUseSkill` calls it with `a3=0, a4=0.85` in state 3
(`CAI.c:171291`), so **retail NPCs stop at 85 % of the picked skill's range** [V].
State 4 calls it with `7.0, 1.0` (a 7 m tolerance before giving up).

### 4.6 `ServerAIUnitSystem::UseSkill` — `0x39326730` (592 B, `DD.c:827139`) and `CanUseSkill` — `0x39326980`

* UseSkill resolves unit/skill; if the unit has the skill's "always-on" buff it sends
  `ZWRemoveBuff` (op 11) instead; otherwise builds **`ZWStartSkillPacket` (op 24)** and sends it to
  the world (`sub_39362920`, `"Sending skill(%u) request to world. Target(%u)"`), and notifies the
  skill controller (`vtable+304`). Return 1 sent / 0 not sent [V].
* CanUseSkill runs the full cast check `sub_39457B60` and writes the result struct (byte reason);
  logs `"Can-Use test for skill(%u), target(%u). Result: %d"`; missing unit/invalid skill return
  bytes 33/1/34 [V].
* `GetBaseSkillInfoResult 0x393260E0` is the base-skill fallback helper.

### 4.7 `NpcSetSkillList` — `sub_39354860` (848 B, `DD.c:866967`), registration `0x39355610`

Lua `OnRequestSkillInfo` does `entity.unit:NpcSetSkillList(entity.AI.skills.<list>)`
(`attack.lua:44-46`, `roaming.lua:73-75`), which replaces the AI unit's candidate vector
(`sub_39354860` reads the Lua table and writes the unit's list). The `OnRequestSkillInfo_<tag>`
variant lets per-tag (`onCombat`, `onCombatStartSkill`, `onSpurtSkill`, `onAlert`, `alert`) lists be
requested without disturbing the normal list [V].

---

## 5. Navigation and pathing

### 5.1 Roaming position — Lua, not C++

`X2AI:CalcNextRoamingPosition` is Lua in both versions (`lua10x/.../x2ai.lua:43-73`,
`lua12/.../x2ai.lua:15-40`):

* 1.2: `idlePos.x/y + uniform(-6, +6)`; z snapped to `GetTerrainElevation` only when the point is
  below terrain and `terrain - 6 < z` (45° approximation); underwater creatures accept the point
  only when the water depth is > half the stance height [V].
* 10.x: same shape with `roamingMinDist/roamingMaxDist` (defaults 1/6, clamped min=1, max≥min+1),
  `GetRoamingDistance = (rand-0.5)*(max-min) ± min` (`x2ai.lua:18-41`) [V]. 31 `npc_ai_params` rows
  use these keys; the 1.2 DB has none (counted with a script over both DBs).
* the position is stored with `AI.SetRefPointPosition(..., "refpoint")`; the roaming pipe then
  `locate refpoint` + non-blocking `approach` (`pipe_manager_x2.lua:94-103`).
* `[?]` the 1.2 slope test keeps the *idle z* when the terrain is much higher; our Rust always uses
  the terrain height (`reference_height_in`, `npc.rs:515`), so a roaming NPC on a cliff edge may snap
  differently.

### 5.2 `Npc::CheckPos` — `0x3933D0E0` (1440 B, `DD.c:847069`)

The 10.x replacement for the 1.2 Lua "teleport home" checks (AI-ORACLE §2.3). Called every **128
frames** from `Npc::Update`. Behaviour [V]:

* only when a check-pos context is set (`Npc+156 != 0`), the NPC is not the local player's possessed
  actor, and the point's sector equals the NPC's current sector (`gEnv+264` vtable+176 / +160).
* reads the script point via `idlePos` / `talkPos` (script type 10 = Vector3), then compares the
  3D squared distance with `radius²`:

| context | point | radius | Lua caller (`lua10x`) |
|---|---|---|---|
| 1 | idlePos | 15 | `roaming.lua:26` `AICPC_ROAMING` |
| 2 | idlePos | 7 (+posture reset) | `hold_position.lua:35` |
| 3 | talkPos | 7 (+posture reset) | `talk.lua:36` |
| 4,5 | — | none | unused |
| 6 | idlePos | 50 if group member [I] else 15 | `alert.lua:30` |
| 7 | idlePos | 3 | `flytrap_alert.lua:41`, `flytrap_attack.lua:25` |

* when exceeded and the point is in the same sector: teleport the actor to the point (actor
  vtable+168), `sub_3933B510` resets the movement/time-event counters, and for hold/talk a posture
  reset (`v10` flag → `sub_390B34D0`) [V].
* when the sectors differ (NPC outside its sector) and the actor movement type != 2: teleport back to
  the script `spawnPos` (`sub_3933C9C0` reads `"spawnPos"`; `sub_3933BCD0` gets the AI script) [V].
* contexts 4/5 are unreachable in the shipped Lua [V/I].

The Lua binds the context with `NpcSetCheckPosContext` (`0x39354640`), set to `AICPC_NONE` on every
behaviour exit. Our Rust re-checks inside each behaviour instead (§7).

### 5.3 `Npc::_FollowPath` — `0x3933CA70` (624 B, `DD.c:846717`)

Resolves the AI script and sets the script fields `followPathType` (`a3`, int), `followPathName`
(`a2`, string) and `followPathSpeed` (`a4`, float; if < 0 it takes the AI system's default speed via
`pAISystem->vtable+448`), then raises `GO_TO_FOLLOW_PATH` (`sub_3933B970`). Failure paths log
`"No path to follow(pathName:%s)"` and `"Failed to get ai script of NPC"`. Lua does the rest:
`AI.SetPathToFollow`, `run followPathSpeed`, `followpath` (`follow_path.lua:14-36`).

### 5.4 `Npc::_FollowUnit` — `0x3933F880` (576 B, `DD.c:849365`)

Resolves the followed unit and its entity, writes the script field `followUnit` (entity pointer) and
raises `GO_TO_FOLLOW_UNIT`; sets `Npc+114 = 1`. If the unit is gone it either raises `GO_TO_IDLE`
or sends `ZWRemoveNpc` (op 3, reason 3) depending on the command set (`sub_3933E2D0`), matching
`follow_unit.lua:61-80` (command-set completion or `NpcRemoveAfter(5)`). The lost-unit check runs
every **64 frames** in `0x3933C270` (320 B): sets the `followUnit` script param, raises
`OnLostFollowUnit` (`DD.c:846293`).

### 5.5 Anchors (10.x only) — `GotoAnchor 0x39341E00` (240 B), `ChangeNewAnchor 0x3933CDF0` (608 B)

* anchor list `Npc+120..128`, entry stride **116 bytes**, anchor id at `entry+100`; with one anchor
  ChangeNewAnchor logs "only one anchor to go" and clears `Npc+152`; otherwise it tries up to 20
  random picks (`CryRandom`) different from the current anchor, writes the script field
  `anchorType` and returns [V].
* `GotoAnchor` builds a command list with command type **5** and `dword_3A638B58`, then
  `RunAICommandSet` with the default set id `dword_3A607D80`; errors `"No anchor to go."` [V].
* `ai_commands` has no type-5 rows in the shipped 10.x DB (counted: only 1..4), and neither the
  1.2 nor the 10.x `npc_ai_params` has a `goto_anchor` key; the 10.x-only `goto_anchor.lua`
  behaviour uses the script fields. **Verdict: 10.x-only.**

### 5.6 `Npc::RunAICommandSet` — `0x3933D790` (2448 B, `DD.c:847474`)

The C++ half of `run_command_set.lua`: validates the command list (`v34 = (end-start)/8`,
`"Failed to run ai command set becaused the set have 0 command(No command)"`), resolves the
`commanderUnit` (entity script `unit`), builds the Lua table (`"commands"`, `"commanderUnit"`,
`"runType"=4`, `"canInteract"`), calls the entity's `AI.commandSet.RunCommandSet`
(`sub_390AD8F0`), and then notifies the AI/command-set objects through their `+192`/`+136` vtable
methods [V]. The
`ai_commands`/`ai_command_sets` tables exist in 1.2 with the same columns and cmd ids 1..4
(1.2: 3×follow_unit, 290×follow_path, 706×use_skill, 454×timeout; 10.x adds nothing new) [V].

### 5.7 Stuck handling and `OnPathStuck`

`OnPathStuck` is **not** an engine replan. It is a named script signal raised by the goal ops when
the actor made no progress (`COPApproach 0x310E4F88` at `CAI.c:175286`, `COPFollowPath 0x310E95A8`
at `CAI.c:178166`, both after the stall timer `0x310D64E8` exceeds **1.5 s**). The stall timer
accumulates the frame time while the actor is not moving and is clamped to **[0, 3] s**
(`CAI.c:165130-165174`) [V]. The Lua handlers just restart the current pipe
(`roaming.lua:60-62`, `idle.lua:102-104`, `follow_path.lua:86`) except `return_state.lua:87-91`,
which resets `move_timeoutCount` and re-runs the time event (after 20 ticks it teleports home,
`return_state.lua:75-91`). `COPStick` does **not** raise `OnPathStuck`; it has `RegeneratePath` and
`TryToTeleport` (`"COPStick::TryToTeleport teleporting %s to (...)"`, `CAI.c:183864`) [V].

### 5.8 Pathfinder

* goal ops create a **pathfind** (`COPPathFind 0x310E1298`, Execute `0x310DBA08`) with a unit/refpoint
  target and a distance/duration, and a **trace** (`COPTrace 0x310DBFC8`, Execute `0x310F1878`,
  core `0x310EE988` 3283 B) that walks the returned polyline.
* `CAISystem::UpdatePathFinder 0x310656B8` (7047 B, `CAI.c:76487`) processes the queue: A* star
  start/continue (`"A* SetupAStar returned no path for %s"`, `"A* ContinueAStar returned path for %s"`),
  then beautifies the path (`PATHFINDER_BEAUTIFYINGPATH`), and on no path uses a **partial path** or
  a **straight path** (`PATHFINDER_NOPATH using partial/straight path`), with a per-AI time limit
  (`ai_PathfindTimeLimit` help "1.5", `ai_PathfinderUpdateTime/Count`) [V].
* the A* solver is `CAStarSolver` (RTTI/strings; functions around `0x31047054`/`0x310479B4`) over
  `CGraph` (navmesh `.bai`; the shipped 10.x areas are v22, 1.2 v21 per ZONE-PHYSICS-USAGE §2.5) [V].
* the zone's NPC AI does **not** use `MoveSkillController::RequestPathFinding`
  (`0x39227ED0`, 208 B): that is for skill movement controllers (its only caller is
  `0x392428F0`), and it logs `"ai system is not ready to perform path finding. maybe ai nav data is
  missing"` [V].

---

## 6. NPC movement → `ZWUnitMovements`

Details and the byte layout are in [`movement.md`](movement.md). Summary:

* **`Npc::Update 0x393482D0`** (368 B, `DD.c:856932`) is the per-NPC tick. It mixes the NPC id with
  the engine frame counter (`(id>>8) ^ *(BYTE*)(gEnv+284)`) and runs:
  every frame: aggro housekeeping `0x3933B490` (when `ServerUnit+34194 == 0`); every 4: AI time event
  dispatcher `0x3933BE40` + `FindNewTargetIfItNeed 0x39345770`; every 8: `0x3933BEA0/0x3933C020/
  0x39343250`; every 16: `0x3933FB20`; every 64: `0x3933C3B0/0x39343CF0/0x3933C270/0x39347060`;
  every 128: `CheckPos`; every frame also the movement send decision `0x393405B0` when the NPC
  manager's sync flag is on [V]. **[correction to AI-ORACLE §2.2]**: aggro cancel (every 8 frames),
  `ProcessReturn` + shared-aggro collection (every 64 frames) and `UpdateAggro` (every frame) are all
  inside `0x3933B490`, not the 8/64-frame calls above.
* **`0x393405B0`** (1328 B, `DD.c:850095`) decides whether this NPC is sent this frame: it compares
  position, rotation, velocity, stance/alertness and state with the last sent copy (`Npc+276..325`),
  scores changes (a teleport-like change adds 10 → flag 4), and calls
  `sub_3936D860(zoneClient, &state)` when a send is due [V].
* **`sub_3936D860`** (96 B, `DD.c:885999`) appends the state to the ZoneClient ring: 1000 entries of
  **336 bytes** at `ZoneClient+208`, count at `+200`; overflow is dropped with
  `"dropped movement unit %u - exceeded %d"` [V].
* **`sub_3936A380`** (448 B, `DD.c:883959`) flushes the buffers every server update: when the rate
  limiter is 0 (initialised to 0, `DD.c:886757`) it sends all queued entries; otherwise it sends a
  proportional amount (`>=1`). `sub_393656D0` (240 B, `DD.c:881088`) writes the packet through the
  ZoneClient vtable+32 and records the send time/size in a 2048-entry stats ring
  (`ZoneClient+1008292`) [V].
* **CVars** (descriptor table at `0x3A5183xx`, defaults in the descriptor): `npc_move_sync_period`
  = **100 ms** (`qword_3A607D10`, at `0x3A518350`), `npc_move_skip_standing` = **400** frames
  (`qword_3A607D20`), `npc_move_skip_disabledAI` = **2000** frames (`qword_3A607D18`),
  `npc_movement_skip` = 1 (`qword_3A607D08`), `npc_enable_fastest_sync` = 0 (`qword_3A607D28`);
  strings at `0x39FDCA40..0x39FDCAA0` [V].
* **Speeds**: the movement entry carries a velocity (quantised to ±60) and the actor stance; the
  actual speed is the AI actor's speed set by `run`/`stick` (§3.1, §3.10), with the stance's
  `ai_move_speed_*` values as the actor model defaults (`game_stances` in both DBs; 10.x walk
  1.0-1.8, run 2.0-5.4 m/s in the first rows) [V/I].

---

## 7. Where our Rust server does the same, and whether it matches

Paths are under `rust/crates/aaemu-game/src/`. "match" = same observable rule; "differs" = both do it
but with different numbers/mechanism; "missing" = absent.

| area | retail (address / Lua) | our Rust | status |
|---|---|---|---|
| behaviour graph / defaults per AI file | Lua `aicharacter.lua`, `characters/x2` | `framework.rs:366` build, `systems/ai/npc.rs` | match (structure, AI-ORACLE #1) |
| per-NPC tick | `Npc::Update 0x393482D0`, stagger 4/8/16/64/128 | `manager.rs:652` `tick_all`, 100 ms, only NPCs near players or woken (`manager.rs:1-22,450-500`) | **differs** (no stagger; far NPCs not ticked) |
| aggro housekeeping cadence | `0x3933B490`: UpdateAggro every frame, cancel every 8, return+share every 64 | `behaviors.rs` aggro helpers (`update_aggro_help`, `should_return`) | differs (timers/tick model); see AI-ORACLE §3 |
| `run` speed | `0x310D6E18`; pipe values 0/1/2 m/s, walk cap 0.4 | `npc.rs:123-155` `base_move_speed` uses `game_stances` walk/run (1.8/5.4) | **differs (top suspect)** |
| `timeout` | `0x310D6BB8` | per-behaviour `next_*` timestamps (`behaviors.rs:1833` etc.) | match (rule) |
| `signal` | `0x310D71D8` + `sub_3933B970` | direct calls/transitions (`framework.rs:575`) | differs (mechanism only) |
| `branch`/label jump | `0x310F7604`, `0x310D7308` | `framework.rs:575` `transition` | differs (mechanism) |
| `bodypos`/`lookat` | `0x310D6D88`, `0x310D5F38` | `npc::set_game_stance`, `npc::look_towards` | differs (mechanism) |
| `strafe` | `0x310D6C88`, `npc_strafe_params` (10.x) | only `strafe_during_delay` flag (`behaviors.rs:718`) | **missing** |
| `approach` | `0x310E4F88` (pathfind+trace, 1.5 s stuck) | `move_in_range` (`behaviors.rs:975-1060`) + `path_handler` | differs (see stand-off) |
| `stick` | `0x310F24D8` (+RegeneratePath `0x310F18E8`) | chase path with replan hysteresis (`behaviors.rs:865-955`) | differs (route rules; no stick distance/teleport) |
| `backoff` | `0x310EAF38` (0.5 m / 0.5 s) | none in `attack_tick` (`behaviors.rs:1386-1411`) | **missing** |
| `followpath` | `0x310E95A8`, `run followPathSpeed` | `behaviors.rs:1457-1553`, `path_handler.rs:155` | differs (speed source, stuck) |
| `follow` (unit) | `0x310E59C8`; Lua `follow_unit` uses stick (4.5 m skip, speed 1) | `behaviors.rs:1555-1592` (walk to 1 m, speed ×1.5..5) | **differs** |
| `locate`/refpoints | `0x310DBE38` | none (direct positions) | differs (mechanism) |
| roaming position | Lua `x2ai.lua:43-73` (10.x min/max 1..6; 1.2 ±6) | `behaviors.rs:1870` (±6 fixed, terrain height always) | differs (10.x params, slope rule) |
| roaming cadence | `pipe_manager_x2.lua:82-103` (70 % 0.5-2 s, else 5-6 s; capped approach 2..6 s) | `behaviors.rs:1833-1868` (3-5 s pause, no 70/30, no cap) | differs |
| CheckPos leash | `0x3933D0E0` contexts 1/2/3/6/7 (15/7/7/50-15/3, every 128 frames) | `check_idle_leash` (`behaviors.rs:1595-1622`): hold 7/3 every 2 s, roaming/alert 15 every 1-2 s, flytrap 3 | match on thresholds, differs on timing/group 50 m/out-of-sector |
| out-of-sector recovery | `0x3933D0E0` + `0x3933C9C0` (spawnPos) | none | **missing** |
| OnPathStuck → behaviour restart | `0x310D64E8` + signals; Lua handlers | `path_handler.rs:70` drops a stalled waypoint after 5 s | **differs** |
| anchors / goto_anchor | `0x39341E00`, `0x3933CDF0` (10.x only) | none | n/a (10.x-only) |
| command sets | `0x3933D790`; `ai_commands` 1.2/10.x | `behaviors.rs:1917-2009`; `params.rs:72` | differs (FollowUnit command warns, no stack, no goto_anchor) |
| skill request event | `COPUseSkill 0x310EA0B8/0x310D9788/0x310E4838` + `NpcSetSkillList 0x39354860` | `skills.rs:56` `template_skills` + `npc_events.rs` | differs (no per-tag request/list pick) |
| skill pick | `PickAiSkill 0x39329E00`, candidates `0x39327430` (uniform), `PickSkill 0x393289C0` | `behaviors.rs:319-403` uniform over castable ids | mostly match (no phase/list windows/predicted target) |
| stand-off | `IsTargetInSkillRange 0x310D9B08` 0.85×range, vertical stick 1.5 m | `move_in_range` stops at `attack_start_range_scale` (`behaviors.rs:975-1010`) | **differs** |
| almighty phases | `StartAiSkillLists 0x39354BB0`, `PickAiSkill`, phase pipes | `behaviors.rs:1301-1375` skill queue | **differs** (no phase machine) |
| movement packet | `ZW op 8`, 336-byte state, 1000 ring, 100 ms period, change detection | `SCOneUnitMovement` 0x06c per tick (`npc.rs:295-470`, `world/session.rs:1040`); `SCUnitMovements` 0x06b unused (`models/movement.rs:700-712`) | **differs** (no batching/period) |
| movement physics | CryPhysics/CryAI actor, ground/water/sweeps | direct step + terrain snap + capsule sweep (`npc.rs:317-444`) | differs by design |

---

## 8. Ranked differences worth checking against the running zone

Rank by (a) player-visible effect, (b) ease of measuring with the zone + a bot, (c) confidence that
the retail behaviour is 1.2-compatible.

1. **NPC movement speed (`run`).** Retail pipes request an AI speed (idle 0, follow 1, combat 2,
   path `followPathSpeed`) written into the CAIObject speed fields (`0x310D6E18`); the engine clamps
   it to the walk level 0.4 in walk-only states (`0x3100675C`) and CPuppet maps required speed to
   gait levels 0/0.21/0.4/1.0/1.4 (`0x3116847C`, follower `0x3113E5D0`). Ours ignores the pipe value
   and uses `game_stances` walk/run (typically 1.8/5.4) (`npc.rs:123-155`). The exact m/s mapping is
   uncertain, but the two models are structurally different. Measure: pull a melee NPC and record its
   position every 100 ms (the 336-byte state carries the velocity); compare closing speed and the idle
   walk speed (retail cap 0.4).
2. **Combat stand-off / cast range (`IsTargetInSkillRange` 0.85, vertical stick 1.5 m).** Retail picks
   the skill first and stops at 85 % of its range (`0x310D9B08`, `0x310DF568`); ours stops at
   `attack_start_range_scale`/weapon range and then picks (`behaviors.rs:975-1010`). Measure: archer
   NPCs (1.2 10258, 7337) and casters; record the distance during casts.
3. **Attack loop backoff/strafe.** Retail does `strafe(2,50,1)`, `run 2`, `useskill`, then a
   conditional `branch` that runs `backoff(0.5 m, 0.5 s)` only when the target is within 0.3 m
   (`attack.lua:16-23`, `0x310EAF38`); ours never strafes or backs off (`behaviors.rs:1386-1411`).
   Measure: let a melee NPC overlap a player and record positions; retail steps back 0.5 m, ours stays
   inside the player.
4. **`OnPathStuck` handling.** Retail raises the signal after 1.5 s without progress and the
   behaviour restarts the pipe; `return_state` teleports after 20 ticks (`0x310E4F88`,
   `0x310D64E8`, `return_state.lua:75-91`). Ours drops the waypoint after 5 s (`path_handler.rs:70`).
   Measure: pull an NPC into a wall/corner and watch return/teleport timing.
5. **Follow-unit behaviour.** Retail: skip stick within 4.5 m, stick with follow-side and speed 1,
   lost unit → command-set completion or remove after 5 s (`follow_unit.lua:20-80`,
   `0x3933F880`, `0x3933C270`). Ours walks to 1 m with a 1.5..5× speed multiplier and goes idle on
   loss (`behaviors.rs:1555-1592`). Measure: escort quest NPCs / pets following a player.
6. **Roaming cadence and distance.** Retail: 70 % short pause 0.5-2 s, 30 % 5-6 s, approach capped
   `max(2,minDist/2)..6`, 10.x distance 1..6 m (`pipe_manager_x2.lua:82-103`, `x2ai.lua:43-73`).
   Ours: 3-5 s pause, no split, no approach cap, fixed ±6, always terrain z
   (`behaviors.rs:1833-1896`). Measure: a roaming NPC's stop/start distribution over 5 minutes.
7. **Almighty/boss phase skills.** Retail `StartAiSkillLists` + `PickAiSkill` + phase pipes
   (`0x39354BB0`, `0x39329E00`, `pipe_manager_x2.lua:104-180`); ours a flat skill queue
   (`behaviors.rs:1301-1375`). Measure: a dragon/almighty NPC's phase transitions and pipe changes.
8. **Movement packet batching.** Retail: a 336-byte state per NPC, 1000-entry ring, change detection,
   `npc_move_sync_period` 100 ms, forced resend 400 frames standing / 2000 disabled, then one
   `ZWUnitMovements` per server update (`0x393405B0`, `0x3936D860`, `0x3936A380`, CVars at
   `0x3A518350`). Ours: one `SCOneUnitMovement` (0x06c) per AI tick per NPC, no `SCUnitMovements`
   batching (`npc.rs:295-470`, `models/movement.rs:700`). Measure: packet counts/CPU with many NPCs;
   client-side smoothness.
9. **Out-of-sector / spawn recovery.** Retail teleports an NPC that left its sector back to
   `spawnPos` (`0x3933D0E0`, `0x3933C9C0`). Ours has no sector ownership; NPCs can be pulled across
   cell borders and stay. Measure: pull an NPC across a sector border (256 m) and release.
10. **Group/formation leashes.** Retail group members use their leader's idle position and a 50 m
    alert leash (`CheckPos` context 6, 50 m when group member; `idle.lua`/`roaming.lua`
    `IsGroupMember`), and `formation_GoToOnPoint` (`pipe_manager_x2.lua:57-65`). Ours has no
    formation/group AI (AI-ORACLE #32). Measure: group spawns (`npc_groups`, 571 members in 1.2).

Lower priority/not 1.2: goto_anchor/bind (10.x-only), `npc_strafe_params` (10.x-only), skill
condition 10 `onCombat` (10.x-only; our Rust matches 1.2), the 30 s `COPUseSkill` watchdog (10.x
diagnostic).

---

## 9. Unknowns

* **Frame length** of the zone (`gEnv+284`). All stagger numbers are frames; 4/8/16/64/128 frames and
  the 400/2000-frame resend thresholds cannot be converted to seconds without it. The movement
  *period* (`npc_move_sync_period` 100 ms) is in milliseconds.
* **CVar defaults**: `npc_move_sync_period`/`npc_move_skip_standing`/`npc_move_skip_disabledAI`/
  `npc_movement_skip`/`npc_enable_fastest_sync` defaults were read from the descriptor table
  `0x3A518320-0x3A5183A8`; the *unit* (ms vs frames) of the skip values is inferred.
* **USF flag values**: bits 1/2/4/8 are read from the ctor (`0x310EA0B8`); the mapping to the names
  `USF_PICK_BASE_SKILL_IN_BAD_CASE`/`ALLOW_STICK_STOP_CHECK`/`AUTO_STICK`/`WAIT_UNIT_STATUS_PROBLEM`
  is inferred from their use and the Lua flag combinations, not from a symbol.
* **Opcode numbering**: the name table (§2.1) and the factory cases differ by one for most ops, while
  branch/clear are tested as 21/22 in the pipe entry (`CAI.c:187700-187735`). The parser's stored
  value was not resolved.
* **CheckPos contexts 4 and 5** (unreachable in the shipped Lua): meaning unknown.
* **Staggered housekeeping functions** `0x3933BEA0`, `0x3933C020`, `0x39343250`, `0x3933FB20`,
  `0x39347060`: addresses and cadence known, roles not identified (no log strings).
* **`0x310F7DD4`** (opcode 17/18 candidate `hide`/`forget`), **`0x310E1E58`** (`waitsignal`),
  **`0x310E6648..6868`** (`charge`) constructors were enumerated but not read in detail; they are
  outside the requested scope.
* **COPStick defaults** `+1096=10.0`, `+1100=20.0`, `+1104=3.0` are initialised in the ctor but only
  `+1096`/`+1100` were seen used (speed scale / far-path threshold); the exact semantics of each are
  **[I]**, not proven.
* **Walk cap 0.4** (`0x3100675C`): the condition under which the engine clamps speed to 0.4 is read
  as "actor may not run", but the flag was not traced to an entity property.
* **Speed units**: `run` writes `+1192/+1196` and CPuppet rewrites `+1192` with the gait levels
  0/0.21/0.4/1.0/1.4 (`0x3116847C`), which the follower `0x3113E5D0` uses; whether these are m/s or a
  multiplier on a stance speed was not resolved. The difference against our `game_stances` speeds is
  structural either way, but its magnitude needs measurement.
* **1.2 C++**: `ProcessAiPhase`, the 1.2 `CheckPos` equivalent and the 1.2 goal-op
  implementations cannot be checked (no 1.2 zone binary). Verdicts for them rely on the 1.2 Lua and
  the identical engine generation (REVERSING.md / ZONE-PHYSICS-USAGE).
* **Doodad/house/terrain colliders** used by the trace sweeps are outside this map (see
  ZONE-PHYSICS-USAGE §2).

---

## 10. Suggested next checks (cheap → expensive)

1. Re-measure suspects 1-3 against the **running 10.x zone** with a headless bot and the zone's own
   logs (the `#123757 AI maybe stopped` watchdog and `Sending skill(%u) request to world` lines give
   a timestamped oracle). No 1.2 client needed.
2. Compare the 1.2 client's `SCUnitMovements`/`SCOneUnitMovement` traffic (our server already sends
   0x06c) against retail 1.2 packet captures if any exist; if not, at least batch our NPC moves per
   tick to match the world-side shape.
3. Use the 10.x zone's `ai_DebugDrawAStarOpenList` / `ai_DebugPathfinding` CVars (`CAI` strings
   `0x311D7368`, `0x311D7280`) to inspect the pathfinder while reproducing suspects 4/9.
4. Once the zone boots against a stub world (`FEASIBILITY.md` M1-M4), add a bot scenario per suspect
   and record `ZWUnitMovements` entries with timestamps; the 336-byte state gives position, velocity
   and stance, enough to compare speeds directly.
