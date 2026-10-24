# NPC movement → ZWUnitMovements: cadence, speeds, wire state

Addresses are VAs in `x2game-dev_dedicate.dll` (base `0x39000000`); `DD.c` line references point at
`D:/ArcheAge Returns 10.0.2.13 - 8yx - r575 - 2026-06-18/Bin64/decompiled/x2game-dev_dedicate.dll.c`.

## 1. Per-NPC tick and the stagger

`Npc::Update` `0x393482D0` (368 B, `DD.c:856932`). The NPC id is mixed with the engine frame counter
byte at `gEnv+284` (`(id >> 8) ^ counter`), giving a per-NPC phase. The tick calls:

| mask | every | functions | role |
|---|---|---|---|
| — | frame | `0x3933B490` (if `ServerUnit+34194 == 0`) | aggro housekeeping |
| `&3` | 4 | `0x3933BE40` (time event dispatch), `0x39345770` (FindNewTargetIfItNeed) | AI timer + target acquisition |
| `&7` | 8 | `0x3933BEA0`, `0x3933C020`, `0x39343250` | unidentified housekeeping |
| `&0xF` | 16 | `0x3933FB20` | unidentified |
| `&0x3F` | 64 | `0x3933C3B0`, `0x39343CF0`, `0x3933C270`, `0x39347060` | talk-end check, follow-unit lost check, others |
| `&0x7F` | 128 | `0x3933D0E0` (CheckPos) | idle-position leash |
| — | frame | `0x393405B0` (if NPC manager `+216 != 0`) | movement send decision |

`0x3933B490` (128 B): if `Npc+112` (aggro enabled) — every 8 frames `NpcAggroTable::ProcessAggroCancel`
`0x393204B0`; every 64 frames `ProcessReturn 0x393211E0` + `0x3931E570` (shared aggro members); every
frame `0x3931F790` (UpdateAggro). **This corrects AI-ORACLE §2.2**, where the 8/64 calls were listed as
separate entries of the stagger table instead of sub-cadences of the every-frame aggro call.

Frame length is not established (MAP §9); the movement *period* below is milliseconds.

## 2. Send decision `0x393405B0` (1328 B, `DD.c:850095`)

Per NPC and frame:

1. If the NPC is "AI disabled" (`Npc+53 != 0`) or its per-frame counter (`Npc+268`) reached
   `npc_move_skip_disabledAI` (2000), continue; else return.
2. `elapsed = now_ms - last_send` (`Npc+264`). If `elapsed >= npc_move_sync_period` (100 ms):
   * if `npc_movement_skip` == 0, send unconditionally;
   * else compute a budget from the current NPC count (`unknown_libname_2919(qword_3A607A40)`,
     `dword_3A518E30`, clamped by `dword_3A518E34`) and require `elapsed >= period + budget`.
3. Copy the current state (unit id `Npc+8`, position `Npc+276`, rotation `Npc+288`, velocity
   `Npc+300`, stance/alertness `Npc+316..325`) and compare it with the last sent copy; each changed
   component adds 1, a special state mismatch adds 10. If `Npc+53 != 0`, or the change score != 0, or
   the per-NPC forced counter (`Npc+272`) reached `npc_move_skip_standing` (400), enqueue.
4. Flags: change score ≥ 10 → movement flag `4`; `Npc+325` → flag `8`
   (`a1+325` is the per-unit "fast sync" byte).
5. `sub_3936D860(zoneClient, &state)`; then reset the counters (`Npc+264 = now`).

## 3. Queue and flush

* `sub_3936D860` (96 B, `DD.c:885999`): appends to `ZoneClient+200` (count) / `+208` (entries of
  **336 bytes**), maximum **1000**; on overflow logs
  `"dropped movement unit %u - exceeded %d"` and drops the entry.
* `sub_3936A380` (448 B, `DD.c:883959`): per server update, drains the two buffers
  (`GameServer+336216` and `GameServer+672248`, both `ZWUnitMovementsBuffer`, ctor `0x3937F430`).
  When the rate limiter (`GameServer+1008280`, initialised 0) is 0 it sends everything; otherwise it
  sends `round(queued * dt / limit)` (at least 1).
* `sub_393656D0` (240 B, `DD.c:881088`): writes the packet via `ZoneClient vtable+32`, records the
  send time and size in a 2048-entry stats ring (`GameServer+1008292`), and passes a type `1` (normal),
  `0` (partial), `3`/`4` (priority) [I: type values observed at call sites].
* `ZWUnitMovementsPacket` ctor `0x3937F430` (112 B): op id **8** at `+8`, 1000 slots of 336 bytes,
  defaults from `dword_3A6390F8/13C/140`.

## 4. The 336-byte movement state

Serializer `sub_3938B0B0` (3920 B, `DD.c:910686`); the outer buffer adds a unit header before the
state: `sub_3938C000` (304 B) marshals `"unitId"` into the entry at `+24` (a 3-dword marshal, so the
exact header/state split at `+24..35` vs the state start at `+32` is **[I]**; reads as an 8-byte
header plus the state). Relative to the 336-byte state:

| off | field (marshal name) | type / encoding |
|---|---|---|
| +0 | `type` | u8 move type |
| +4 | `time` | compressed u32 |
| +8 | position | Vector3 (serializer `0x39B11410`) |
| +32 | direction | direction vector (`sub_3938AFC0`) |
| +48 | `vel.x/y/z` | 3× s16 / 32767 × 60 → **±60 m/s** |
| +60 | `flags` | u32; bit 0x10 = move type 1 extra data |
| +64 | `scType`, +68 `phase` | present when `flags & 0x10` |
| +72 | `actor.deltaMovement` | 3× s8 / 127 |
| +84 | `actor.fallVel` | u16 / 65535 × 128 |
| +88 | `actor.stance` | u8 |
| +89 | `actor.alertness` | u8 |
| +90 | `actor.flags` | u8 bitfield (0x80 = fallVel present, 0x20 = gc data, 0x40 = climb) |
| +92 | `actor.gcFlags` | u32 |
| +94 | `actor.gcPart` | u8 |
| +96 | `actor.gcPartId` | u32 |
| +104 / +128 | gc position / direction | serializers |
| +144 | `actor.climbData` | u32 |
| +148 | climb vector | `sub_39389080` |
| +176 | `actor.maxPushedUnitId` | u32 |

Movement type 1 is the actor/character form; types 2/3 have their own subsets in the serializer
(ship/vehicle state), see `sub_3938B0B0` branches.

## 5. Speeds

* The AI actor's requested movement speed is `CAIObject+1192`, mirrored to `+1196` (read as m/s [I]):
  * `run <fValue>` sets both (`0x310D6E18`); pipe values are 0 (idle), 1 (follow unit), 2 (combat,
    phase), `followPathSpeed` (paths) — `lua10x/goalpipes/pipe_manager_x2.lua:22-54`,
    `attack.lua:18`, `follow_unit.lua:28`, `follow_path.lua:33`.
  * `COPStick` sets 0.5 / 0.7 in water/anim cases (`0x310F24D8`, `CAI.c:182250-182267`).
  * `CAIObject::Update` (`0x3100675C`) clamps a speed ≥ 1.0 to the walk level **0.4** when the actor
    may not run; CPuppet (`0x3116847C`) maps a required path speed to gait levels
    0 / 0.21 / 0.4 / 1.0 / 1.4 and writes `+1192`, and the path follower `0x3113E5D0` consumes that
    value as the speed (supports m/s [I]).
* The NPC actor model itself comes from `game_stances`
  (`ai_move_speed_walk/run/slow/sprint`, `normal_speed`, `max_speed`; SELECT `0x39BFCB00`,
  `DD.c:2626458`); the 10.x rows in the first page are walk 1.0-1.8 / run 2.0-5.4 m/s. The AI `run`
  value is what the shipped pipes actually use while moving.
* Velocity on the wire is the resulting movement velocity, quantised to ±60 m/s.

## 6. CVars (descriptor table `0x3A518320..0x3A5183A8`)

| CVar | string VA | object | default |
|---|---|---|---|
| `npc_movement_skip` | — | `qword_3A607D08` | 1 |
| `npc_move_sync_period` | `0x39FDCA90` | `qword_3A607D10` | 100 (ms) |
| `npc_move_skip_disabledAI` | `0x39FDCA70` | `qword_3A607D18` | 2000 |
| `npc_move_skip_standing` | `0x39FDCA58` | `qword_3A607D20` | 400 |
| `npc_enable_fastest_sync` | `0x39FDC9E8` | `qword_3A607D28` | 0 |

Also present in the same string block: `npc_move_net_delay` (`0x39FDCA40`),
`npc_move_skip_standing`, `npc_move_skip_disabledAI`. The skip values are counted in **frames** in
the code (`Npc+268`/`Npc+272` counters), the period in **milliseconds** (the same `ITimer` clock at
`gEnv+40` that `ActivateAITimeEvent` uses, whose Lua interval is converted to ms at `DD.c:866545`);
this is why their wall-clock effect needs the unknown frame length. The goal-op timers in
`cryaisystem` use the AI system clock `qword_31209C60+80` (microseconds; e.g. `COPTimeout 0x310D6BB8`
multiplies the delta by 1e-6 and compares seconds) — a different clock from the movement period.

## 7. Client-side counterpart (for comparison, not analysed)

The world would fan the zone's movements out to clients as the 1.2/10.x world→client movement
packets. Our server instead emits `SCOneUnitMovement` (0x06c) from the AI step itself
(`rust/crates/aaemu-game/src/systems/ai/npc.rs:295-470`) and never uses `SCUnitMovements` (0x06b)
(`models/movement.rs:700-712`), so there is no batching/cadence layer.
