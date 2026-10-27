# Player Bots

- Audience: Contributors
- Last verified against: `feat/bots-phase0` on October 20, 2026 (phase 0 code and flow tests; live check pending)
- Target: ArcheAge 1.2 (`r208022`)

## Goal

Run a populated world of about **200 player bots** that play through the same systems as people.
A bot is an ordinary character: normal name tags, party frames, rankings, chat, trade and mail,
and no bot marker reaches a client.

This page is the build plan. The external headless bots in [Bots](Bots) stay the test harness;
the phase 0 in-server code exists in `systems/bots/` (see [Phase 0 status](#phase-0--connectionless-spike-s)).

## Decisions

| # | Decision (2026-10-20) | Consequence |
| --- | --- | --- |
| 1 | About 200 bots online at once | In-server bodies with a budgeted brain tick and an activity/LOD governor, not 200 sockets |
| 2 | Indistinguishable from players | Nothing bot-specific on the wire; normal character rows, packets, party frames, rankings, chat, trade and mail |
| 3 | The C# module's 7 archetypes for now | A small authored build table; no attempt at all 120 tree combinations |
| 4 | No GM help for bots | No teleport, summon, `/move` or GM command; stuck recovery uses player-legal actions only |
| 5 | Boss mechanics in dungeons and raids | A per-dungeon/boss behaviour layer is part of the product, not a later extra |

`/bot` lifecycle commands are operator tools. A bot itself never invokes a GM command and never
holds GM-only state.

## Architecture

### In-server players

Every bot is a normal `Player` (`world/mod.rs:66`) whose `ConnectionHandle` is detached
(`net/connection.rs:124-139`): outgoing packets are encoded and dropped. Character creation
already builds a connectionless `Player` for gear bonuses (`managers/character.rs:355-357`), so
the pattern is in production use.

The bot joins the grid with `world.try_add_character` (`world/mod.rs:1023`), is spawned with
`world::session::spawn` (`world/session.rs:666`) and is seen by clients through the normal
visibility broadcast (`world/mod.rs:1137`). Movement, skills, chat, party, loot and mail use the
same broadcasts as a player. The only missing piece is a socket.

**Why not a headless client.** The deciding factor is control, not realism. Quest progress,
party invites, instance membership, loot eligibility and boss state are authoritative server
functions; a socket bot can only infer them from packets. In-server code calls the engines
directly (`systems/quests/engine.rs:328`, `systems/social/team.rs:955`,
`systems/instances/indun.rs:240`). It also avoids 200 TCP connections, 200 client states and
per-input decoding. A bot costs one `Player` plus a small brain, the same object a player uses.
Note that a detached handle still encodes what is sent; the governor should skip pointless
self-sends once phase 0 measures the cost.

**Indistinguishability.** There is no `IsBot` field in the model or in any packet. Bots are
created, saved and loaded as normal characters; name tags, party frames, rankings, chat lines,
trades and mail come from the same data and handlers. The roster is server-side only. Chat and
trade follow the normal rules and fees; bots do not have a bypass. The first phases keep bots
quiet unless a player talks to them; synthetic conversation is not part of the plan.

### What must be emulated

Only client handshakes that have server-side effects need emulation:

- After `SCLoadInstancePacket`, the client sends `CSInstanceLoadedPacket`
  (`systems/instances/packets.rs:151-170`). The bot path calls `worlds::instance_loaded`
  directly.
- `select_loaded` mixes the world join with the client state burst
  (`world/session.rs:343-372`). The world half, including `try_add_character`, is now the
  reusable `enter_world` function (`session::enter_world`), used by both paths.
- `move_unit` is the client move-apply path (`world/session.rs:753-930`). Its unit branch is now
  the shared `apply_unit_move` used by the client handler and the bot mover.
- `notify_in_game` / `notify_in_game_completed` and `LeaveWorldTask` expose connectionless
  player halves (`notify_in_game_player`, `notify_in_game_completed_player`,
  `leave_world_player`); bots use the same world join, chat-channel, achievement and save code.
- Pure UI handshakes (lobby, character select, keep-alive pings) are skipped. The keep-alive
  sweep only sees real connections, so detached characters are not disconnected.

### Test harness

`aaemu-testclient` remains the real-protocol oracle. It already proves 200 concurrent logins
(`load`, `Bots.md:92`), runs quests in player mode (`quest --player`), fights and walks the same
`.bai` navmesh ([Bots](Bots)). The plan uses it three ways:

- As an external control next to an in-server bot, to compare visible packets and world state.
- As a regression harness for the player-visible flows the bot refactors touch (`move_unit`,
  `select_loaded`, instance entry).
- As a load baseline for the activity governor.

A real client plus `aaemu-clientdriver` checks what a person actually sees: name tags, party
frames, animations and boss telegraphs. In-server flow tests use `test_support::test_server()`;
bot runs write one JSONL telemetry line per bot per interval.

### Where the code lives

Proposed: a new `systems/bots/` subsystem in `aaemu-game`, following the subsystem layout in
`CONTRIBUTING-PORT.md` (runtime state on `System`, data in `data.rs`, commands under
`systems/services/commands/`). Shared-file hooks stay additive; the C# module needed a 2,681-line
patch across 41 host files, which is the warning to avoid. The upstream-policy question (gated
subsystem vs separate crate) is still open.

## Components to build

Paths are under `rust/crates/aaemu-game/src/` unless stated otherwise.

| Component | Have today | New |
| --- | --- | --- |
| Bot manager and roster | Normal character load (`db_character.rs:111-115`), periodic save (`managers/save.rs:462`), world add and spawn (`world/mod.rs:1023`, `world/session.rs:666`) | Bot identity/provisioning, roster, spawn/despawn, life state (Offline/Spawning/Idle/Active/Resting/Dead/Recovering/Despawning), operator commands |
| Brain tick and governor | World tick and visibility grid (`world/mod.rs:1137`) | Budgeted scheduler, round-robin slices, three LOD levels (active, coarse, dormant), promotion on visibility, per-bot telemetry |
| Build and skill plans | `ability1..3` (`db_character.rs:145-147`), learn/swap/reset (`systems/skills/progress.rs:434-688`), starting kit (`managers/character.rs:436-482`) | Seven-archetype table, skill-point priority, second/third tree at levels 5/10, per-level learn policy |
| Rotation engine and data | `SkillTemplate` (`data/skills.rs:43-59`), native `use_skill` (`systems/skills/skill.rs:356`) | JSON format, compiler, triggers (cooldown, range, health, party, buff, combo, chain), GCD scheduler |
| Movement | `.bai` A* (`systems/ai/geodata/astar.rs:135-167`), heights/cells, NPC move helper (`systems/ai/npc.rs:317`), client move apply (`world/session.rs:753`) | In-server mover, long routes across zones, shared `apply_move`, stuck detection and legal recovery |
| Combat and targeting | `use_skill` for any unit, target notify (`packets/world_objects.rs:207-219`), death (`systems/skills/combat.rs:246`) | Target selection, assist, threat-aware positioning, potions/rest, cast-reject handling |
| Loot and gear | Open bag/take loot (`systems/economy/looting.rs:608,647`), move/equip (`systems/economy/inventory.rs:740`), stat recompute | Loot policy by role, bag cleanup, gear scoring from the C# rules, vendor/repair loop |
| Questing and grinding | Full engine (`systems/quests/engine.rs:328,1573`), event hooks (`systems/quests/events.rs:26`), team-shared acts (`systems/quests/acts.rs:857`) | Kill/gather intake and lifecycle, objective loop, report, resume checkpoint, skip-and-grind fallback |
| Death and revive | `do_die` (`systems/skills/combat.rs:246`) with 15-240 s waits (`:340`), `resurrect_character` (`systems/skills/resurrect.rs:129`) | Release/rez policy, accept player rez, corpse recovery, avoid the 240 s loop, never GM revive |
| Party, roles, follow, assist | Teams, roles, raid (`systems/social/team.rs:955,973`), party quest sharing (`systems/quests/acts.rs:857`) | Invite observation/auto-accept (invitations are private, `team.rs:335`), formation/follow/stay, assist/attack/passive, leader routing |
| Dungeon and boss layer | Entry/queue (`systems/instances/indun.rs:240`, `systems/instances/dungeon.rs:152`), event data (`systems/instances/events.rs:34`, `systems/instances/data.rs:155-173`) | Instance-loaded ack, group entry, per-dungeon profiles, boss phase/interrupt/AoE/add rules, wipe/re-enter |
| Chat-command control | Access levels (`systems/services/commands/mod.rs:86-106`), `/` routing (`world/session.rs:947`) | Admin `/bot` lifecycle, party/whisper orders, permissions, no bot marker in replies |
| Persistence | Character save (`managers/save.rs:235`), save and remove (`world/session.rs:1281`) | Roster, archetype, plan and checkpoints; resume after restart; metrics counters |

### Builds

The first table is the module's seven archetypes: Abolisher (1/3/4), Darkrunner (1/4/8),
Primeval (6/4/8), Reaper (7/5/8), Daggerspell (7/2/8), Templar (10/3/4) and Cleric (10/4/9)
(`BotArchetypes.json:1-107`). Ability ids are 1 Battlerage, 2 Witchcraft, 3 Defense,
4 Auramancy, 5 Occultism, 6 Archery, 7 Sorcery, 8 Shadowplay, 9 Songcraft, 10 Vitalism and 11
None (`data/skills.rs:13`). The module calls 1/4/8 Darkrunner; the 1.2 client's own class name
for that combination is not verified yet. `compact.sqlite3` has no class-combination table, and
the tree names are in [Classes](Classes). At level 55 a character has 28 skill points for about
110 learnable class skills, so the learn order is a budget, not a full unlock.

## Reused from the C# module

The reference is the third-party PlayerBots module for the C# AAEmu server, GPL-3.0-or-later
(a local checkout is kept under `.server_files/references/ArcheAge-PlayerBots/`, not tracked).
The licence is compatible with this repository; reused data keeps its attribution and licence.

| Reuse | Rewrite | Leave behind |
| --- | --- | --- |
| The 7 archetypes, weights, level unlocks, skill learn order, passives, weapon/armor rules (`BotArchetypes.json:1-107`) | The brain (~30.5k lines in 207 C# files): state, combat, movement, questing and party logic | The 41-file host compatibility patch |
| The 7 rotation JSON files: named skills, weighted default lists, conditional rules | Skill gates and actions on top of our `use_skill` and `SkillTemplate` | C# singletons and host hooks |
| The design: life state machine, social order state, thin quest authority, party quest coordinator | Movement on our `.bai` navmesh, looting/inventory, teams, instances | The 10.x prototype assumptions and the optional client autopilot |
| The config schema and per-build tuning data | Quest intake/lifecycle against our quest engine | Fresh-character creation and the experimental leveling path |

The module is an alpha preview: its questing handles only monster-hunt and item-gather quests
(`BotQuestLifecycleController.cs:55-60`) and it has no dungeon support at all. Dungeons and
boss mechanics are new work here.

## Phased plan

Each phase ends with `lane.sh check`, a live run on the lane stack, and telemetry from the run.
The governor and the phase gates are calibrated in phase 0.

| Metric | Definition | Provisional gate |
| --- | --- | --- |
| Levels/hour | Character levels gained per wall-clock hour | Beat the player-mode testclient baseline (level 7, 28 deaths in 84 min, `Bots.md:274`) |
| Deaths per 100 kills | Deaths divided by kills, times 100 | Under 20 solo; under 5 with a healer |
| Stuck events | No 5 m progress for 30 s while travelling, or a recovery trigger | Under 1 per bot-hour, all recovered without GM help, zero operator fixes |
| Tick p95 with N bots | Server tick latency at N in-server bots | Within the existing budget; fix the number after the phase 0 baseline |
| Quest completion | Completed divided by attempted, per level band | No regression against the external quest runner |
| Cast rejects | Rejected casts divided by attempts | Under 5%, and explained by range/line of sight |

### Phase 0 — connectionless spike (S)

**Status (2026-10-20, `feat/bots-phase0`).** Implemented in `systems/bots/`; flow tests pass; live verification on a lane stack is still pending (another worker).

- **Works.**
  - `/bot add <character name>` loads an existing, offline character from MySQL (`db_character::load_character` + `load_character_details`), creates a `Player` with a detached `ConnectionHandle`, and joins it through `session::enter_world` → `finish_character_entry` → `notify_in_game_player` → `spawn` / `notify_in_game_completed_player`: the same path a login takes. The bot is a normal entry in `WorldManager` / `WorldInstance`, so clients receive the usual `SCUnitStatePacket` + `SCUnitPointsPacket`, name tag and visibility updates, and the periodic / departure saves include it.
  - `/bot remove <name>` runs the normal logout half (`session::leave_world_player`, shared with `LeaveWorldTask`): despawn, chat / party / family / vehicle cleanups, departure save.
  - `/bot list` reports the roster (name, character id, obj id, level, alive/dead, position).
  - `/bot walk <name> <x> <y>` walks a straight line at the player run speed (5.4 m/s, one `SCOneUnitMovementPacket` every 100 ms) through the shared `apply_unit_move`, so region changes, zone changes, buff-on-move and the collision fall check behave like a client move. The height follows the collision service's ground probe when it has data.
  - `/bot cast <name> <skill id> [target obj id]` calls the native `skills::skill::use_skill` path (`new_skill_for` + `SkillCaster::Unit`); cooldowns, mana, cast time, `SCSkillStarted` / `SCSkillFired` and effects are the normal ones. The target defaults to the bot itself.
  - Death / revive use normal mechanics only: a GM can `/kill` a bot, and once `rez_wait_duration` has elapsed the bot's life watcher calls `skills::resurrect::resurrect_character(server, bot, true)` — the request a client sends — with the normal `SCCharacterResurrectedPacket` broadcast. There is no `/bot kill` or `/bot revive`.
  - The roster is server-side only; no model field, packet or chat line marks a bot.
- **Tests.** `systems/bots/tests.rs`: spawn sends the same packets as a real select nearby, walk broadcasts a normal movement, a cast broadcasts the skill packets, remove despawns and saves, and the `/bot` command surface (list / walk / cast / remove) works on the command harness.
- **Next (phase 1).** `/bot create <name> <race> <archetype>`; build / skill plan and rotation JSON; `.bai` pathing instead of the straight line; obstacle and stuck recovery; loot, rest and revive policy; JSONL telemetry and the activity governor measurement (tick p95, memory, dropped-send cost with 1 / 10 / 50 bots).

**Original deliverables.** `systems/bots` skeleton; `/bot add <charId>` and `/bot remove`; load and spawn a detached `Player`; walk a 100 m `.bai` path, cast one skill, die/revive, save on remove.
- **Test.** A real client watches spawn, movement, skill, death and revive; packets and name tags look normal; relog restores the position. Record tick p95, memory and dropped-send cost with 1, 10 and 50 idle bots.
- **Risks.** `move_unit` and `select_loaded` are shared with real clients; a connectionless character may hit save/leave assumptions. Flow tests and the character-creation pattern mitigate both.

### Phase 1 — one solo bot levels 1 to 10/20 (M)

- **Deliverables.** `/bot create <name> <race> <archetype>`; two archetypes (Darkrunner and a caster/healer); build and skill plan; rotation JSON; kill/gather quest intake, objective loop and report; grind fallback; loot, rest and revive; `/bot status`; JSONL telemetry.
- **Test.** One bot from level 1 to 10, then 20, in a starter zone with no GM commands. Track levels/hour, XP/hour, deaths/100 kills, quest completion, stuck events, cast rejects and tick p95; gate on level 20 with no manual intervention and a better death rate than the baseline.
- **Risks.** Quest coverage is only kill/gather (no escort, vehicle or cutscene quests); zone-edge pathing and skill-point misbuilds are likely. Use one hand-checked chain plus known grind spots.

### Phase 2 — party followers with roles (M)

- **Deliverables.** Invite/auto-accept, follow/stay, assist/attack/passive, tank/healer/damage roles, formation, party loot and leader orders. Role assignment needs a team change: the setter accepts only the member's own id (`systems/social/team.rs:972-985`), and invitations need an accessor (`team.rs:335`).
- **Test.** A human plus two or three bots clear open-world camps. Measure follow distance, assist latency, heal coverage, deaths, loot distribution and stuck events; confirm normal party frames and role icons on a real client.
- **Risks.** Invite visibility and the self-only role setter; following through terrain; loot disputes. Party quest coordination is already server-side, which lowers the risk.

### Phase 3 — dungeon group with boss mechanics (L)

- **Deliverables.** Group instance entry (`systems/instances/indun.rs:240`, `systems/instances/dungeon.rs:152`), instance-loaded ack, roles, room pathing, and one dungeon profile with boss phases: interrupt list, AoE avoidance, add priority, stack/spread and kite points. Wipe, revive and re-enter use normal rules.
- **Test.** One human plus three bots in one 1.2 dungeon (Burntcastle Armory or Hadir Farm first). Record clear rate, wipes, time, boss-mechanic success, stuck events and tick p95; watch the same pull from a real client.
- **Risks.** There is no boss script layer today, so each mechanic is hand-coded and tested. Instance reset, lockout and the 240 s death timer can stall a group; this is the first phase with no C# precedent.

### Phase 4 — raids, PvP and 200-bot population (L/XL)

- **Deliverables.** Raid conversion (10-50 bots), focus fire, CC and kiting, duel and battleground scenarios, and the full activity governor for 200 bots: coarse simulation away from players, promotion on visibility, no position snap.
- **Test.** Duels and one battlefield; win rate against scripted and real opponents; 200 bots online with tick p95/p99, CPU, memory, bytes/s to a real client in view, and a subjective check that players do not notice a switch from coarse to active.
- **Risks.** PvP realism and class balance are open-ended; 200 brains plus broadcasts may exceed the tick budget; repetitive behaviour may still reveal bots. The C# README leaves reliable long PvP and large populations in development.

## Open questions

- **Class names.** Verify the seven archetype names against the 1.2 client strings; the Darkrunner mapping (1/4/8) is first.
- **Where the 200 bots live.** Which zones and level bands, all online at once, and whether they share zones with real players.
- **Quest scope.** Kill/gather only for now; later escort, vehicle and cutscene quests? May bots skip story quests and grind?
- **First dungeon and depth.** Which dungeon comes first, and how far the boss layer goes (interrupts, AoE, phases, hard modes).
- **Account and character ownership.** One bot account per six characters, a bot-account manager, or existing accounts? Are bot characters deletable?
- **Control surface.** `/bot` commands, party-chat keywords or an external API, and who may command a bot, without revealing it to nearby players.
- **Persistence shape.** MySQL table or per-bot JSON for archetype, plan, checkpoints and stuck counters; what must survive a restart.
- **Upstream policy.** Feature-gated `systems/bots` in `aaemu-game`, a separate crate or a sidecar; this decides how many shared-file hooks we accept.
- **Runtime envelope.** Tick/CPU/memory budget with 200 bots, and whether phase 1 success means level 20 or fixed levels/hour and death-rate targets.

## Related

- [Bots](Bots)
- [Server Backlog](Server-Backlog)
- [Dungeons](Dungeons)
- [Classes](Classes)
- [Developer Notes](Developer-Notes)
- [Documentation Maintenance](Documentation-Maintenance)
