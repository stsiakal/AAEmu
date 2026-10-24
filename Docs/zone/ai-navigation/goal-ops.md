# Goal ops — opcodes, constructors, Execute and fields

All addresses are VAs in `cryaisystem_dedicate.dll` (base `0x31000000`). Sizes are from
`D:/aa-re/zone/out/cryaisystem_dedicate.dll.funcs.csv`. `CGoalOp` result codes (see MAP §2.4):
`1` = continue/in progress, `2` = succeeded, `3` = failed, `4` = pending/blocking wait
(`[I]` from the return paths; `COPLocate` returns 2/3, `COPTimeout` 4 then 1).

## 1. Name table

40 `(char* name, int code)` pairs at `0x311FF4B8`; `sub_310F5994` (`CAI.c:186212`) resolves a name,
`sub_310F59A4` (`CAI.c:186240`) resolves a code; 40 = not found = subpipe. Codes read directly from
the table:

| code | name | code | name | code | name | code | name |
|---|---|---|---|---|---|---|---|
| 1 | acqtarget | 11 | pathfind | 21 | clear | 31 | usecover |
| 2 | lookaround | 12 | locate | 22 | branch | 32 | wait |
| 3 | approach | 13 | trace | 23 | random | 33 | adjustaim |
| 4 | follow | 14 | signal | 24 | lookat | 34 | proximity |
| 5 | followpath | 15 | ignoreall | 25 | continuous | 35 | movetowards |
| 6 | backoff | 16 | devalue | 26 | move | 36 | dodge |
| 7 | strafe | 17 | forget | 27 | charge | 37 | useskill |
| 8 | bodypos | 18 | hide | 28 | waitsignal | 38 | flyuseskill |
| 9 | run | 19 | form | 29 | animation | 39 | bind |
| 10 | timeout | 20 | stick | 30 | animtarget | | ("bindedUseskill" string exists at `0x311E4490`) |

`CGoalPipe::PushGoal 0x310F6574` (3765 B) creates the op object; the switch case observed for each
constructor is noted below. The case numbers are one less than the table code for approach…stick and
equal for clear/branch (MAP §2.3, unknown).

`CGoalPipe::PushSubPipe 0x310F7434` handles code 40. Label fix-up `sub_310F7604` computes the
relative `branch` jump into the pipe entry at `+68` for entry opcodes 21/22.

## 2. Operations in scope

### 2.1 `run` (code 9, factory case 8)

* ctor `sub_310D6DD8` (0x20 B object, vt `0x311E10E0`), Execute `sub_310D6E18` (103 B).
* fields: `+16` fValue (speed m/s), `+24` fValueAux (duration, s).
* Execute: `agent+1192 = agent+1196 = fValue`; if duration > 1e-5 and the actor is runnable, calls
  `CAIObject` action helper `0x3116783C` (writes `+4432/+4436/+4440`, `+4444=0`); always 4.
* Lua speed values: `common_DoNothing` 0, `idle_*` 0, `followUnit` 1, `attack_Base` 2,
  `phase_*` 0/2, `followPath` `entity.AI.followPathSpeed` (`lua10x/goalpipes/pipe_manager_x2.lua:22-54`,
  `attack.lua:18`, `follow_unit.lua:28`, `follow_path.lua:33`).
* the engine clamps speed ≥ 1.0 to **0.4** when the actor may not run (`0x3100675C`).

### 2.2 `timeout` (code 10, case 9)

* ctor `sub_310DAC48` (0x28, vt `0x311E1AA0`), Execute `sub_310D6BB8` (110 B).
* fields: `+16` start timestamp (µs), `+24` min time, `+28` max time (`max<=0` → min).
* Execute: initialise start if unset; `elapsed <= +32` → 4; else 1. `+32` is set from
  `sub_310D6B58` (RandomFloat(min,max)).

### 2.3 `signal` (code 14, case 13)

* ctor `sub_310F8150` (0x38, vt `0x311E4750`), Execute `sub_310D71D8` (296 B).
* fields: `+16` signal id, `+24` name, `+32` filter/block mode, `+40` target object, `+48` sent
  flag, `+52` priority.
* Execute: once (`+48`), builds a `0x40`-byte signal (`0x3104B388`), sets id/priority, then sends it
  either through the pipe user's entity script (`a2+29` vtable+32 with `a2+232`) or the AI system
  (`qword_31209C60` vtable+624 with `a2` as sender). Returns 1.

### 2.4 `branch` / `clear` / `devalue` (codes 21/22/16)

* both are built as 0x18-byte objects with vt `0x311E4690` (`sub_310D6658` base), Execute
  `sub_310D7308` (148 B); `ignoreall` uses vt `0x311E4630`, Execute `sub_310D5E18` (16 B).
* fields: `+16` bool value, `+17` bool "clear" (the two ctor cases set them differently).
* Execute (0x310D7308): if `+17`, calls vtable+144 on `a2+2304` (clear); else if the pipe user exists
  (`a2+8 & 0x20`), calls its vtable+784 with `+16`. Always 4.
* the actual conditional jump is the `+68` offset in the pipe entry (label fix-up above), so the op
  object does not know the label.

### 2.5 `bodypos` (code 8, case 7)

* ctor `sub_310D6D68` (0x18, vt `0x311E1080`), Execute `sub_310D6D88` (71 B).
* fields: `+16` BODYPOS_* bitmask, `+20` bool "force".
* Execute: if `+20` → `0x3116786C(agent, +16)`; else `agent+244 = +16` and
  `0x3116786C(agent, -1)`. `0x3116786C` writes `CAIObject+4448` (request) and `+4452=0`.

### 2.6 `lookat` (code 24, case 23)

* ctor `sub_310D7848` (0x18, vt `0x311E1320`), Execute `sub_310D5F38` (15 B).
* Execute: `CAIObject+3188 = +16` (bool). The direction/angles of the Lua 5-argument form are applied
  by the look-at system, not by this op.

### 2.7 `strafe` (code 7, case 6)

* ctor `sub_310D6C38` (0x20, vt `0x311E1020`), Execute `sub_310D6C88` (203 B).
* fields: `+16` float distance, `+20` float second limit, `+24` bool.
* Execute: resolves the pipe user's actor (`qword_31209C98+8` vtable+912, pipe user vtable+256/328)
  and writes a direction pair via `0x3116771C` (`CAIObject+4416/+4420/+4424`); without an actor uses
  the op's own two floats. Always 4.
* 10.x: `npc_strafe_params` table and `ServerAIUnitSystem::GetNpcStrafeParamDesc` (`0x39327260`).

### 2.8 `locate` (code 12, case 11)

* ctor `sub_310F8054` (0x20, vt `0x311E46F0`), Execute `sub_310DBE38` (385 B).
* fields: `+16` name string, `+24` locate type, `+28` max distance.
* Execute: named target through pipe user vtable+376 (name, max distance → zero if farther); or typed
  point at the agent position through the AI system vtable+184. Stores via `0x31158734`.
  Returns `3 - found` (2 found, 3 not).

### 2.9 `approach` (code 3, case 2)

* ctor `sub_310E06A8` (object 0x68, ctor 133 B, vt `0x311E2470`), Execute `sub_310E4F88` (2610 B).
* fields: `+16` last path length, `+20` computed approach distance, `+24` bool (randomise/anim?),
  `+25` bool, `+26` bool (use attention target), `+27` bool, `+28` end distance, `+32` duration,
  `+36` approach distance, `+40` anim-target flag, `+48` subpipe name string, `+56` target id,
  `+64` stall timer, `+80` approaching flag, `+88` trace, `+96` pathfind.
* Execute: target → if none fail (3); snap to anim target; if `+24 && +20==0` compute
  `+20 = dist * +28`; if no pathfinder (`a2+1601==0`) and within end distance → 2, else create
  pathfind `0x310E1298(name="", target, fValue=max or +32, nValue=distance, blocking=0)`; otherwise
  create/step trace `0x310DBFC8(distance=+32, flags=+40/+27)`; stall timer > **1.5 s** → raise
  `"OnPathStuck"` (with sender, filter `dword_31209E90`) and reset; trace != 1 → return its result.
* path length `+16` is updated; `+20` reset on completion.
* `-1` duration (roaming) becomes `float::max` → no cap.

### 2.10 `stick` (code 20, case 19)

* ctor `sub_310E3C88` (object 0x4F8 = 1272 B, ctor 720 B, vt `0x311E25E0`), Execute `sub_310F24D8` (5720 B).
* object fields: `+1048` 1.0, `+1052/56/64/68` scratch, `+1096` 10.0, `+1100` 20.0, `+1104` 3.0,
  `+1108..1116` last point, `+1128` stick target, `+1136` secondary target, `+1144` stick distance,
  `+1148` trace/path limit, `+1152` speed-by-distance factor, `+1156` continuous (break bit 0 clear),
  `+1157/1158` break flags, `+1159` ignore attention, `+1160` try teleport, `+1161` anim flag,
  `+1162/1163` trace flags, `+1168/1176` path flags, `+1172` fValue (clamped 0.5), `+1180` 1.0
  (target-speed scale), `+1192` (u64 velocity-window time), `+1200..1220` velocity smoothing,
  `+1224` ?, `+1228` ?, `+1229` exact-position flag, `+1232` trace, `+1240` pathfind, `+1248` last
  send time, `+1260` teleport flag.
* ctor arg → field map: `a2`=+1144 (abs), `a3`=+1172 (clamped 0.5), `a4`=+1148, `a5`=+1152,
  `a6`=flags (+1159 bit1, +1160 bit2, +1162 bit0x400, +1163 bit0x8000, +1168 bit0x2000/0x20000,
  +1176 bit0x80000, +1229 bit8), `a7`=break flags (+1156 bit0 clear, +1157 bit2, +1158 bit4).
* Execute highlights:
  - take the stick target from `+1128`, else attention target; none → 3.
  - anim target (type 13) → zero the stick distance.
  - create pathfind `0x310E1298(name="", target, max=+1148 or max, distance=+1172, 0, 0)`
    at `+1240`; create trace `0x310DBFC8(distance=+1148, flags +1162/+1163)` at `+1232`; then
    `RegeneratePath 0x310F18E8`.
  - when the actor's model kind is 5 (swimming; `entity+98`) and the trace is idle, set its speed to
  **0.5** (or **0.7** when a flag is unset).
  - `+1176` → exact-position/speed adjustment path (`0x310DC6A8`).
  - returns 1 while the trace runs; continuous mode restarts, otherwise 2/3 at the end.
* `RegeneratePath 0x310F18E8` (1203 B): `+1148` is copied to the trace's `+156`; destination stored
  at `+1108..1116`; uses `0x310D5EA8` for the speed-adjusted distance, may generate a new path point
  at `+1184`; errors `"No path"` / `"m_Path"`.
* `Reset 0x310DC558` (321 B), `OnObjectRemoved 0x310D75A8` (144 B), `TryToTeleport` in Execute
  (`"COPStick::TryToTeleport teleporting %s to (...)"`, `CAI.c:183864`).

### 2.11 `backoff` (code 6, case 5)

* ctor `sub_310E8748` (0xC0, vt `0x311E3810`), Execute `sub_310EAF38` (3798 B).
* fields: `+16` try-teleport? (flag 0x800), `+20` flag mask (default 8), `+64` fValue/distance,
  `+68` duration, `+88` use attention target, `+89` flag 4, `+116` 8, `+120` nValueAux,
  `+124..144` target point/state, `+148` flag 0x2000, `+149` flag 0x40000, `+150` flag 0x100000,
  `+152` nValue stop distance, `+160` target, `+168` trace, `+176` pathfind, `+184` refpoint.
* Execute:
  - if target distance > `+152` → stop movement, Reset, return 2 (no backoff needed).
  - `"_BackoffPoint"` refpoint; for anim target offset by facing × 10 m.
  - pathfind `0x310E1298(name="Backoff", refpoint, max=float::max, nValue=+64+distance, 0, ...)`;
    if it fails or `+136 <= 0` → `"OnBackOffFailed"`, 3.
  - trace `0x310DBFC8(distance=0.1, 0, 0)` at `+168`; `"OnPathFound"` raised when the path is ready.
  - duration `+68` expiry → complete (2/1); tracks the farthest point at `+136`.
* `OnObjectRemoved 0x310D6828` (135 B) logs `"COPBackoff::OnObjectRemoved (%p) resetting due backoff
  target removed"` and resets.

### 2.12 `followpath` (code 5, case 4)

* ctor `sub_310E07A8` (0x88, vt `0x311E24D0`), Execute `sub_310E95A8` (2025 B).
* fields: `+64` trace, `+72` pathfind, `+80` start refpoint, `+88/89/90` loop flags, `+91` road flag,
  `+92` point-list flag, `+104` stall timer, `+124` trace distance (10.0), `+128` point-list active.
* Execute: build `"<pipeuser>_PathStartPoint"` refpoint via `sub_3105AF48(..., 2)`; pathfind
  `0x310E1298(name="FollowPath", refpoint, 0, 0, 0)`; trace `0x310DBFC8(distance=+124)`;
  stall >1.5 s → `"OnPathStuck"`; loop flags restart; errors
  `"Unable to use follow path ... check path is not marked as a road"`,
  `"Unable to use point list"`, `"Unable to find path entry point"`.

### 2.13 `follow` (code 4, case 3) — legacy follow-unit op

* ctor `sub_310D6798` (0x70, vt `0x311E0F50`), Execute `sub_310E59C8` (1362 B).
* fields: `+16` refpoint, `+24` target, `+36` bool, `+56` time, `+72` follow helper.
* Execute: target from `+24`, else attention target; creates helper `0x3114EE8C`, refpoint
  `"<pipeuser>_FollowTarget"` (`sub_3105AF48(...,0)`), averages positions, then follows the helper
  (`0x3114F36C`), returning 3 when no target. **Not** the op used by `follow_unit.lua` (which uses
  `stick`).

### 2.14 `trace` (code 13, case 12) — used by approach/stick/followpath

* ctor `sub_310DBFC8` (0xA8, vt `0x311E11E0`), Execute `sub_310F1878` (97 B), core `sub_310EE988`
  (3283 B).
* Execute: calls the core with `(a1, a2, 1)`; if `a2[798]` (trace started flag) == 0 → 1; else
  compute `(core_result != 0) + 1` and return 3 when the core says "finished" and the actor state is
  7. The core drives the path polyline and the "wait for speed to fall" landing state
  (`"COPTrace reached end but waiting for speed..."`, `CAI.c` strings `0x311E3DF0`).

### 2.15 `useskill` (code 37, case 36) — detail in MAP §4.4

* ctor `sub_310EA0B8` (object 0x60, ctor 230 B, vt `0x311E3D10`), Execute `sub_310E6B68` (508 B).
* fields: `+12` state (0..5), `+16` last position valid, `+20/24/28` last target position,
  `+32..35` USF flags, `+40` event name (`OnRequestSkillInfo[_tag]`), `+48` state, `+56` start/last
  time, `+64` target-changed flag, `+72` stick sub-goal.
* states: 0 raise event (`0x310D9788`), 1 wait + pick (`0x310E4838`), 2 stick (`0x310DF478`),
  3 can-use + use (`0x310DF568`), 4 wait for start/end (`0x310DF928`), 5 finish (`0x310DFBD8`).
* `IsTargetInSkillRange 0x310D9B08`: min range `+3544`, max `+3548`; 14 too close, 0 in range at
  `dist <= a3 + a4*max`, 15 too far; callers use 0.85 (state 1/3) and 7.0/1.0 (state 4).

### 2.16 `pathfind` (code 11, case 10) and `waitsignal`/`charge`/others

* `pathfind`: ctor `sub_310E1298` (0x50, vt `0x311E2560`), Execute `sub_310DBA08` (1066 B). It is the
  helper every pathing op instantiates; `Execute` resolves the target/refpoint and asks the AI system
  for a path (`CAISystem::UpdatePathFinder`), with `name`, `target`, `fValue` (max distance/duration)
  and `nValue`. `byte_311CE9C8` = empty string is used when the target is an object rather than a
  refpoint.
* `waitsignal` `sub_310E1E58` (0x28, vt `0x311E2640`, Execute `0x310D8178`), `charge`
  `sub_310E6648/66E8/67C8/6868` (0x40, vt `0x311E32B0`, Execute `0x310E85D8`), `continuous`
  `sub_310D7888`, `move` `sub_310D7C08`, `random` `sub_310DAC98`, `dodge` `sub_310E8D48`,
  `usecover` `sub_310D8C88`, `wait` `sub_310DEA28`, `adjustaim`/`proximity`/`movetowards`
  `sub_310E1FB8`, `animation`/`animtarget` `sub_310E1EE8` are enumerated but not analysed here
  (outside the requested scope).

## 3. COPUseSkill flags and the Lua pairs

`AI.PushGoal("useskill", 1, flags[, tag])` → ctor `0x310EA0B8` → bytes `+32..35`
(bit1/2/4/8). The flag names come from the Lua (no symbols in the DLL); the mapping is inferred:

| Lua combination | where | flags | effect |
|---|---|---|---|
| `USF_AUTO_STICK` | hold/roam/idle pipes, alert, UseSkill calls | 4 | vertical stick before cast, no base fallback |
| `USF_PICK_BASE_SKILL_IN_BAD_CASE + USF_ALLOW_STICK_STOP_CHECK + USF_AUTO_STICK` | `attack.lua:20`, archer, big monster, phase pipes | 1+2+4 | base-skill fallback + retry on out-of-range + vertical stick |
| `USF_AUTO_STICK + USF_WAIT_UNIT_STATUS_PROBLEM` | wild boar spurt (`wild_boar_attack.lua:85`) | 4+8 | retry instead of fail while the unit status is a problem |
| `0` | dragon `phase_dragon_*`, `hold_fight` | 0 | plain pick/use, no stick, no fallback |
| `USF_UNDERWATER_CREATURE_WAY` | leviathan `phase_leviathan_approach` | (fly path) | only on `flyuseskill` |

Pathology checkpoints in the binary: `"No response for skill info request or No skills"` (2 s),
`"start skill request timed out"` (2 s), `"skill end or skill failure timed out"` (60 s),
`"#123757 AI maybe stopped"` (30 s), `"can't use skill %u. Reason: %u"`.
