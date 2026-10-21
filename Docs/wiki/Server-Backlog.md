# Server Backlog

- Audience: Contributors
- Last verified against: `rust-rewrite-bots` at `770333e02` (code review, the 2026-10-20 bot checks and the 2026-10-21 live suites), October 21, 2026
- Target: ArcheAge 1.2 (`r208022`)

## Overview

Known gaps in the game server. Items come from static code review unless marked **reproduced**.
The bug evidence standard applies: a mismatch is a suspect until it is reproduced live and backed by
retail evidence. Only confirmed bugs get a server fix. Items marked **SUSPECT** were not confirmed
even in code.

Severity is the impact if a malicious or unlucky client hits the gap: HIGH (gold, items or other
players' state), MED (gameplay advantage or data damage), LOW (nuisance or unreachable on stock data).
Paths are under `rust/crates/aaemu-game/src/`.

## A. Client trust and security

Confirmed in code unless marked SUSPECT.

### Fixed

- **Bank deposit/withdraw** (reproduced, fixed in `334987a9a`, upstream PR NickMesser/AAEmu#23):
  non-positive amounts are refused (a negative deposit used to mint gold and drive the bank
  negative) and the bank needs a banker NPC within 5 m of the current interaction object. The 5 m
  range is a guess, not from retail data.
- **`CSSellItems`** (reproduced, fixed in `334987a9a`, upstream PR NickMesser/AAEmu#23): each item
  is listed once, the merchant must be within 3 m, and only items actually moved to buy-back are
  paid (repeated entries and failed moves used to pay, with wrapping arithmetic).
- **`CSBuyItems` doodad shop** (reproduced, fixed in `334987a9a`, upstream PR NickMesser/AAEmu#23):
  goods must come from the NPC's merchant pack or the doodad's `DoodadFuncStoreUi` pack; a purchase
  without a catalog is refused, so a zero `npc_obj_id` no longer makes every template buyable.
- **Auction distance** (reproduced, fixed in `334987a9a`, upstream PR NickMesser/AAEmu#23): requests
  need an auctioneer NPC within 5 m, named by the packet ids or the current interaction object (the
  ids were ignored).
- **Guild invitations and ranks** (reproduced, fixed in `12336d422`, upstream PR
  NickMesser/AAEmu#21): a guild invite reply needs a pending, unexpired invitation to that guild
  and a replier without a guild; kick and role change need a strictly higher rank than the target,
  so the leader can be neither kicked nor demoted.
- **Loot master** (reproduced, fixed in `12336d422`, upstream PR NickMesser/AAEmu#21): only 0 or a
  team member is accepted. Party markers target units, not members, so they need no check.
- **Skill cast authority** (fixed in `a6ca5fc4f`, upstream PR NickMesser/AAEmu#22): a Mount-caster
  needs a caster the player may use (self, a usable mate, a slave the player owns or rides), doodad
  and slave targets get a bounded range allowance instead of a blanket exemption, and the
  undefined-use fallback only accepts skills named by a player-facing source in the static data.
  This closes the former "Skill cast as another unit", "Skill range waived" and "Undefined-use
  skill fallback" rows.
- **Corpse loot range** (reproduced, fixed in `1a27e69f6`, evidence `b57e5275e`, upstream PR
  NickMesser/AAEmu#25): a take is refused with `TooFarAway` beyond 5 m from the loot owner,
  measured like `GetDistanceTo` (minus the model radii), and the loot stays in the corpse. Retail
  evidence: the 3.5 client enables looting at 3.0 m or less and walks the player to the corpse;
  the 2 m above that are slack. Suite `rust/verify/authority-guards.vtest`.
- **In-place resurrection** (reproduced, fixed in `1a27e69f6` and `c2d7593e2`, upstream PR
  NickMesser/AAEmu#25): needs a pending offer from the `Resurrection` effect or the free
  resurrection below `MinimumExpLossLevel`; any resurrection consumes it and each death clears a
  leftover one (a GM revive did not).
- **`CSSaveTutorial`** (reproduced, fixed in `1a27e69f6`, upstream PR NickMesser/AAEmu#25): only
  real tutorial entries (`quest_contexts.category_id = 45`), at most 256.
- **`CSSaveUIData`** (reproduced, fixed in `1a27e69f6`, upstream PR NickMesser/AAEmu#25): keys
  0..=16 and values up to 16 KiB (the 1.2 client's largest observed payload is 4,708 bytes).
- **Portal and navi teleport use range** (fixed in `1f06f399f`): `CSUsePortal` and
  `CSNaviTeleport` need the character within `World.PortalUseRange` (default 5 m) of the gate NPC
  / navi doodad, measured like `GetDistanceTo` (2D, minus the model radii). The default covers the
  gate placement (`open_portal_effects.distance` 3 m; the 1.2 tooltip says 5 m) and matches
  `NAVI_USE_RANGE`. Refusals send `SCErrorMsg(TooFarAway, 154)` and log one WARN; the refused navi
  func and teleport do not run.

### Open

| Item | Severity | What is wrong | Where | Fix direction |
| --- | --- | --- | --- | --- |
| Movement fully trusted | MED | `MoveVerifier` is off by default; `sus.rs` only logs above 25 m/s | `world/session.rs:753-905`, `app/Configurations/Physics.json`, `systems/services/sus.rs` | Enable and tune the verifier; needs retail speeds |
| Mount seat | LOW | A passenger boards a stranger's mount seat from any distance | `systems/vehicles/mates.rs:312` | Check distance |
| `CSChangeDoodadData` | LOW | No range check | `systems/npcs/doodad/manager.rs:670` | Check distance |
| `CSUseTeleport` | LOW | Fare is sent by the client; no 1.2 fare data exists (every `npcs.teleporter` row is false and there is no fare table) | `systems/npcs/portal/mod.rs` | Needs retail fare evidence |
| `CSBuyItems` quantity | LOW, SUSPECT | Per-item quantity cap | `systems/economy/store.rs` | Confirm, then cap |

## B. Gameplay and integrity

### Fixed

- **Trade** (fixed in `f740311d2` and `8a12189c3`, upstream PR NickMesser/AAEmu#19): only unbound
  bag items can be put up, each once; the finish re-validates every item before anything moves and
  moves the money only after every item moved.
- **Craft materials** (fixed in `f740311d2`, upstream PR NickMesser/AAEmu#19): materials are counted
  and consumed from the bag only, and the product hand-out re-checks them and cancels the craft
  when they are missing.
- **Auction settlement** (fixed in `4db88094f`, upstream PR NickMesser/AAEmu#17): a failed result
  mail keeps the lot (or outbid refund) as a pending settlement and the auction tick retries the
  missing mails until they are delivered.
- **Ship death** (fixed in `e9aedb49d`, upstream PR NickMesser/AAEmu#24): death stores the repair
  start time, and a loaded 0-HP vehicle is only reset to full HP once the repair time has passed.
- **Summon despawn** (fixed in `1fe501e80`, upstream PR NickMesser/AAEmu#18): summons flagged
  `despawn_on_creator_death` are registered under the caster and removed when the caster dies.
- **Account attribute saves** (reproduced, fixed in `388522ee1`, upstream PR NickMesser/AAEmu#26):
  overlapping background saves of one account failed with `Duplicate entry` and could store an
  older snapshot; saves now run one at a time under the account lock and each writes the newest
  cache.
- **Enter world** (fixed in `5c2c376eb`): the `WorldManager.TryAddCharacter` result is checked and
  driven by a per-character reservation taken before the select's MySQL load and released only
  after the departure save finished, so two concurrent enters of one character leave exactly one
  player and a second login of the account is refused atomically at `X2EnterWorld` (the existing
  `GLPlayerEnter` result 1 / `ACEnterWorldDenied` path). Refusals log one WARN with the character
  and the reason.
- **Disconnect cleanup** (fixed in `5c2c376eb`): the game-connection task runs `on_disconnect`
  under an unwind guard that still releases the session reservation, removes a half-left world
  entry and frees the account when the cleanup panics.
- **Death transition** (reproduced, fixed in `5828e35d6`): two concurrent lethal hits (a skill
  and a DoT tick, fall / drowning damage, `GM /kill`, PvP, an NPC death) could both run the death
  path after passing the unlocked HP check: two `SCUnitDeathPacket`s, double exp / durability
  loss, two loot generations, double quest kill credit and respawn timers. The alive-to-dead
  transition is now claimed atomically under the unit data lock, the damage write re-checks HP,
  and a second `DoDie` for the same life is refused. Restored HP (resurrection, repair) starts a
  new life. Test: `systems::skills::death_once_tests` (two racing hits on a player and an NPC,
  looped); it failed before the fix.

Static revalidation of the remaining rows.

| Item | Severity | What is wrong | Where | Fix direction |
| --- | --- | --- | --- | --- |
| ID loader | MED | Fails open on a DB error | `managers/id.rs:244` | Fail startup |
| Summon effects | MED | Reference missing event spawner ids (data gap) | summon effect data | Fix the data or the loader |
| Doodad funcs | LOW | `EvidenceItemLoot`, `AreaTrigger` and `Bubble` only log | `systems/npcs/doodad/funcs/mod.rs:165-170` | Implement per retail |
| Shipyard | LOW | Runtime state is not persisted | `systems/vehicles/` | Persist it |
| Plot caster | LOW | `execute_tree` live-caster check; recycled `caster_obj_id` in buff events (needs a reuse test) | `systems/skills/` | Write the reuse test first |
| Aggro help | LOW | Rule-0 shortcut (needs retail evidence) | `systems/ai/behaviors.rs:641` | Get a retail capture |
| Dungeon reconnect | LOW | Reconnect to the same instance, and the lockout and reset policy (the instance-exit fixes `ae5c73933` / `00ba92067` are merged) | [Dungeons](Dungeons) | Decide the policy |
| Item id reuse | LOW | An id can be reused before the deletion commit | `managers/item.rs:207` | Reserve until commit |
| Default premium | LOW | New connections default to premium through 2030 (mirrors the C# policy) | `net/connection.rs:45-62` | Decide the policy |
| Unarmed MeleeParry | LOW | Server gives 1.88; the original client shows 0 (level 10, no weapon). Found by a retail oracle capture | stat code | Find the formula |

## C. Research and evidence needed

No code change until the data exists.

- **Combat formulas.** `armor/(armor+5300)` is confirmed against retail captures. The others need
  authored formulas or captures.
- **Reference data.** Provenance of `compact.sqlite3` and orphan rows.
- **Physics at world scale.** Needs a load test with active-cell and thunk counters.
- **CI coverage.** The ignored real-data and MySQL tests do not run in CI.

## Found by the bots

From the [Bots](Bots) sweeps. Sizes are counts in the sweep that saw them.

| Finding | Seen in | Size | Likely cause |
| --- | --- | --- | --- |
| No `SCLoadInstancePacket` after the exit doodad 4535 (skill 16560) | Quest sweep (2026-10-19) | 1,477 Firran, 1,820 Harani, 344 Nuian, 1,264 Elf | Not reproduced on `rust-rewrite-bots`: the sweep predates the obstacle-ray fixes `ae5c73933` / `00ba92067` (`PORTING.md` names entrance 3674 and exit 4535); most failures cascade from one late enter. Re-run the sweep to close |
| A doodad did not appear after `/spawn doodad` | Quest sweep, Harani | 468 | Likely bot-side: a doodad spawned in a fresh instance is only visible after the bot sends `CSInstanceLoadedPacket` (script command `loaded`) and teleports once |
| `/move <name> <place>` on another player answers "Moved" but the player stays put | PvP check (2026-10-20) | 1 | SUSPECT, not reproduced in isolation; the target branch calls `teleport_player` for the target (`systems/services/commands/scripts/move_cmd.rs:94-111`) |
| No sign sphere for the quest component | Quest sweep; quest 1525 | 378 | Data gap |
| `mother_faction` requirement not met | Quest sweep | 232 | Quest prerequisite |
| Using an item did not update quest 261 | Player mode | 1 quest | Open |
| NPC 7491 never spawned, so its leash cannot be measured | AI sweep | 1 NPC | Spawn data |

Added 2026-10-20:

- **Party invitations** (SUSPECT, found live): an invitation is only expired when the target
  answers (`systems/social/team.rs:651`), and a new invitation to the same player is refused while
  any entry exists (`systems/social/team.rs:599`), so a stale invitation blocks later invitations
  until it is answered or the server restarts.
- **Dungeon 101 / zone 265 exit** (SUSPECT, quest sweep sample): the exit skill is refused with
  result 50, and entrance doodad 3674 still fails for some quests in the sweep sample. Rerun the
  quest sweep to see whether the merged obstacle-ray fixes cover this; the first row above is the
  earlier 4535 / skill 16560 sample.

## Related

- [Bots](Bots)
- [Quest Technical Audit](Quest-Audit)
- [Developer Notes](Developer-Notes)
