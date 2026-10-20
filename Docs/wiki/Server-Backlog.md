# Server Backlog

- Audience: Contributors
- Last verified against: `rust-rewrite-bots` at `009172b05` (static code review), October 20, 2026
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

### In progress

| Item | Severity | What is wrong | Where | Fix direction |
| --- | --- | --- | --- | --- |
| Bank deposit/withdraw (lane `money-guard`) | HIGH | A negative amount mints gold; the bank works from anywhere | `systems/economy/inventory.rs:1505` (`change_money`, no sign check) | Reject negative amounts; require bank NPC range |
| `CSSellItems` (lane `money-guard`) | HIGH | Duplicate entries are paid N times; no merchant distance | `systems/economy/store.rs:573-670`, `:656` | Reject duplicates; check distance |
| `CSBuyItems` doodad shop (lane `money-guard`) | HIGH | `npc_obj_id` 0 skips the catalog filter | `systems/economy/store.rs` ~281-372 | Apply the filter for doodad shops |
| Auction (lane `money-guard`) | MED | Usable from anywhere | `systems/economy/auction_packets.rs:52-70` | Require auctioneer range |

### Fixed

- **Guild invitations and ranks** (reproduced, fixed in `12336d422`): a guild invite reply needs a
  pending, unexpired invitation to that guild and a replier without a guild; kick and role change
  need a strictly higher rank than the target, so the leader can be neither kicked nor demoted.
- **Loot master** (reproduced, fixed in `12336d422`): only 0 or a team member is accepted. Party
  markers target units, not members, so they need no check.

### Open

| Item | Severity | What is wrong | Where | Fix direction |
| --- | --- | --- | --- | --- |
| Skill cast as another unit | HIGH | `SkillCaster::Mount` lets a client cast as an arbitrary unit; only mates are checked | `systems/skills/handlers.rs:85-135` | Verify the caster is the player's own mount |
| Corpse loot | HIGH | No eligibility or range check | `systems/economy/looting.rs:1176-1236`, ~765-900 | Check eligibility and distance |
| Movement fully trusted | MED | `MoveVerifier` is off by default; `sus.rs` only logs above 25 m/s | `world/session.rs:753-905`, `app/Configurations/Physics.json`, `systems/services/sus.rs` | Enable and tune the verifier; needs retail speeds |
| Skill range waived | MED | Range is skipped for doodad and slave targets | `systems/skills/validation.rs` | Apply range to those targets |
| Undefined-use skill fallback | MED | Runs any ability-0 skill | `systems/skills/handlers.rs` ~225-245 | Restrict to known use skills |
| Portal and navi teleport | MED | `CSUsePortal` and navi teleport skip the distance check | `systems/npcs/portal/mod.rs:850`, `:1012`, `:1069` | Check distance to the gate |
| `CSSaveTutorial` | MED | Writes any quest id as completed; rows grow without bound | `systems/quests/packets.rs` ~491-523 | Validate ids; cap rows |
| In-place resurrect | MED | Works without an offer | `systems/skills/resurrect.rs:128-150` | Require a pending offer |
| `CSSaveUIData` | LOW | Unbounded keys and strings | `systems/services/character_packets.rs` ~683 | Cap sizes |
| Mount seat | LOW | A passenger boards a stranger's mount seat from any distance | `systems/vehicles/mates.rs:312` | Check distance |
| `CSChangeDoodadData` | LOW | No range check | `systems/npcs/doodad/manager.rs:670` | Check distance |
| `CSUseTeleport` | LOW | Fare is sent by the client; unreachable on stock data | `systems/npcs/portal/mod.rs:1123-1186` | Compute the fare server side |
| `CSBuyItems` quantity | LOW, SUSPECT | Per-item quantity cap | `systems/economy/store.rs` | Confirm, then cap |

## B. Gameplay and integrity

Static revalidation.

| Item | Severity | What is wrong | Where | Fix direction |
| --- | --- | --- | --- | --- |
| Trade | HIGH | `add_item` accepts bound or duplicate items; money moves before items on finish | `systems/economy/trade.rs:307-345`, ~660 | Validate on add; make the finish atomic |
| Craft materials | HIGH | Counts materials in all containers but consumes from the bag, and ignores the result | `systems/economy/craft.rs:319` vs `:688` | Count and consume from the same containers; check the result |
| Auction settlement | HIGH | The lot is removed when the mail fails | `systems/economy/auction.rs:553-556` | Remove the lot only after the mail is stored |
| Ship death | MED | `repair_start` 0 allows an instant resummon; a zero-HP load becomes max HP | `systems/vehicles/slaves.rs:2017-2018`, `:1243-1250`, `:1367` | Set `repair_start` on death; keep zero HP on load |
| Death transition | MED | Not once-only under concurrent lethal hits | `systems/skills/combat.rs:215`, `:246` | Make the transition atomic |
| Enter world | MED | Ignores `try_add_character`; no per-account atomic reservation | `world/session.rs:369` | Check the result; reserve per account |
| Disconnect cleanup | MED | No unwind guard if cleanup panics | `net/listener.rs:95` | Add a guard |
| ID loader | MED | Fails open on a DB error | `managers/id.rs:244` | Fail startup |
| `despawn_on_creator_death` | MED | Loaded but unused | `systems/skills/data/effects.rs:293-303` | Implement in summon despawn |
| Summon effects | MED | Reference missing event spawner ids (data gap) | summon effect data | Fix the data or the loader |
| Doodad funcs | LOW | `EvidenceItemLoot`, `AreaTrigger` and `Bubble` only log | `systems/npcs/doodad/funcs/mod.rs:165-170` | Implement per retail |
| Shipyard | LOW | Runtime state is not persisted | `systems/vehicles/` | Persist it |
| Plot caster | LOW | `execute_tree` live-caster check; recycled `caster_obj_id` in buff events (needs a reuse test) | `systems/skills/` | Write the reuse test first |
| Aggro help | LOW | Rule-0 shortcut (needs retail evidence) | `systems/ai/behaviors.rs:641` | Get a retail capture |
| Dungeon reconnect | LOW | Reconnect to the same instance, and the lockout and reset policy; after the instance-exit fix lands | [Dungeons](Dungeons) | Decide the policy |
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
| `/move <name> <place>` on another player answers "Moved" but the player stays put | PvP check | 1 | SUSPECT, not reproduced in isolation |
| No sign sphere for the quest component | Quest sweep; quest 1525 | 378 | Data gap |
| `mother_faction` requirement not met | Quest sweep | 232 | Quest prerequisite |
| Using an item did not update quest 261 | Player mode | 1 quest | Open |
| NPC 7491 never spawned, so its leash cannot be measured | AI sweep | 1 NPC | Spawn data |

## Related

- [Bots](Bots)
- [Quest Technical Audit](Quest-Audit)
- [Developer Notes](Developer-Notes)
