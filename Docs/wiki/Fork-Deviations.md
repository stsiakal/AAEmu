# Fork Deviations

- Audience: Contributors
- Last verified against: `rust-rewrite-bots` HEAD (`e00299d88`) on October 21, 2026
- Target: ArcheAge 1.2 (`r208022`)

## Goal

This fork (`stsiakal/AAEmu`, branch `rust-rewrite-bots`) follows `NickMesser/AAEmu`'s `rust-rewrite`
for 1.2 parity and adds a small, deliberate layer on top. This page is the register of that layer:
what the fork changes on purpose, which parity fixes went upstream, and which values are still
guesses that retail evidence should replace.

## Policy

- Sync by merging `rust-rewrite` into `rust-rewrite-bots`; never modify `rust-rewrite` itself (see
  `AGENTS.md`).
- Where both sides carry the same fix, take **NickMesser's reviewed version**. Do not re-apply a
  fork copy on top.
- Keep everything only the fork has, and **never override an intentional deviation listed on this
  page**. A conflict that touches one is a stop-and-report item, not a merge decision.
- Parity fixes — including server-side validation that retail enforced but the old C# emulator
  lacked — are **not** deviations. Send them upstream as pull requests and record them under
  [Parity fixes upstreamed](#parity-fixes-upstreamed).
- Guessed values inside a parity fix are listed under
  [Guessed values to confirm against retail](#guessed-values-to-confirm-against-retail), so retail
  evidence can correct them later.

## Intentional deviations

| Deviation | Why | Files / data | Config switch | Status | Upstream? |
| --- | --- | --- | --- | --- | --- |
| Level 55: 3.0 retail 51-55 XP and quest XP, ten visible capstone skills | 1.2 hides the ten `ability_level = 55` capstones and ships a placeholder 51-55 curve (179.3M total at 55) that makes the last five levels unplayable retail-style; 3.0.4.2 is the closest retail data | `SQL/patches/compact/2026-10-20-level-55-retail-3.0.sql`; regression `rust/crates/aaemu-game/src/data/level55_patch_tests.rs`; [Backports](Backports) § Level 55; `PORTING.md` § Level 51-55 capstones and experience | Opt-in compact patch, applied by `cargo xtask compact-patches`, `Scripts/live-stack.sh sync` and `cargo xtask publish` (`--no-compact-db` skips it); no `Config.json` switch | Landed 2026-10-20. 1.2 `skill_points` (24-28) and `quest_supplies.copper` kept; `total_mate_exp` kept at the 1.2 values. The Bevy XP table is still open | No: intentional data deviation, not a parity fix |
| Optional later-version areas (`Areas:Enabled`) | The 1.2 client already contains dormant terrain (Sunbite Wilds 3+4) that 1.2 server data never used; a server-only, opt-in package keeps the 1.2 world untouched until an operator enables it | `Data/Areas/sunbite-wilds.json`; `SQL/patches/compact/2026-10-21-sunbite-wilds-*.sql`; `app/Data/Areas/sunbite-wilds/npc_spawns.json`; `rust/tools/area_package/build_sunbite.py`; [Backports](Backports) -> Sunbite Wilds pilot; `PORTING.md` -> optional areas | `Areas:Enabled` (default empty) in `Configurations/Areas.json` / `Config.Local.json` | Landed 2026-10-21: 2 zones, 81 quests, 111 NPCs, 105 items, 322 placements; disabled by default | No: fork product layer |
| All later-version costumes | [Backports](Backports) decision 4: cosmetic content, delivered in audited batches | None yet; [Backports](Backports) § Costumes (3.0/10.x item closure) | None yet | Planned. Art ships as a separate overlay pak read by the Bevy client (decided 2026-10-21); a test showed the Bevy loaders read the 10.x `.cdf`/`.chr`/`.mtl`/`.dds` files as-is | No |
| In-server player bots (about 200) | [Player Bots](Player-Bots) decision 2026-10-20: a populated world whose bots are indistinguishable from players | [Player Bots](Player-Bots) build plan; `rust/crates/aaemu-game/src/systems/bots/` (`/bot add\|remove\|list\|walk\|cast`); suite `rust/verify/bots-phase0.vtest` | GM command only; feature-gated subsystem vs separate crate still open (`Player-Bots.md` § Open questions) | Phase 0 landed 2026-10-21 (`948f804d6`): connectionless bots spawn, move, cast, die and resurrect through the player paths; behaviour (phase 1+) planned | No: fork product layer |
| Compact patch tooling | `compact.sqlite3` is read-only at runtime; the fork needs intentional data fixes applied repeatably and recorded | `SQL/patches/compact/` (README, level-55 patch); `cargo xtask compact-patches` (`rust/crates/xtask/src/main.rs`); `Scripts/live-stack.sh:105`; `cargo xtask publish` | Patch directory / `--patches <dir>`; `cargo xtask publish --no-compact-db` | Implemented 2026-10-20; each file's SHA-256 is recorded in `aaemu_compact_patches` | Could go upstream: generic tooling with no fork-only behavior |
| Live verification suite | Reproduce fixes end to end on a lane stack; regression coverage beyond unit and flow tests | `Scripts/verify.sh`; `rust/verify/*.vtest`; `aaemu-testclient verify`; [Verification](Verification) | `--only`, `--report`; lane-only (refuses outside a lane) | Implemented 2026-10-20; suites `emote-spoof`, `guild-guards`, `authority-guards`, `bots-phase0` | Yes: generic harness sent as [#27](https://github.com/NickMesser/AAEmu/pull/27) (open; the `bots-phase0` suite stays fork-only) |
| Client-side items (reference only) | The 1.2 client reads its own encrypted DB, so the Bevy client is the reference for server data changes such as level 55 | Bevy fork `stsiakal/AABevy1.2`: addon API (`crates/aa-client/src/addons/`), planting snap-grid option (planned); farm plumbing, farm phase visuals, XP table from the game database and the 3 m loot range | Client-side only | Addon API merged into the fork's `main` 2026-10-21 (fork-only); snap grid planned with the farming UI; upstream PRs: unit stats #6 and title-inline #9 merged, farm plumbing #12, farm visuals #13, XP table #14 and loot range #15 open | Not server deviations; client work, with parity fixes offered upstream as client PRs |

The client-side items are listed here only so a sync worker does not mistake them for server changes.
The addon API and the planting snap-grid option live in the Bevy client fork; the other client fixes
were sent to `NickMesser/AABevy1.2` as pull requests and are not deviations of this server.

## Parity fixes upstreamed

The fork's copies of these fixes are merged upstream and are no longer deviations: a sync takes
NickMesser's version. Numbers, titles and states were read on 2026-10-21 with
`gh pr list --repo NickMesser/AAEmu --author stsiakal --state all`.

| Fix | Fork commit | Upstream PR | State |
| --- | --- | --- | --- |
| Labor persists across saves and regeneration | `6b5b2a110` | [#4](https://github.com/NickMesser/AAEmu/pull/4) | Merged 2026-10-16 |
| Temporary duel / battlefield factions, data-driven battlefield scoring, duel leave, conflict-zone kills | `3175adc1a`, `d3edcad9f`, `98731a250`, `8b0d55037`, `4eb52f768` | [#5](https://github.com/NickMesser/AAEmu/pull/5) | Merged 2026-10-20 |
| Wire formats checked against the 1.2 client readers (protocol audit M1-M15 and the missing CS opcodes) | `cd702540a` … `5548d3fa2` (13 commits) | [#6](https://github.com/NickMesser/AAEmu/pull/6) | Merged 2026-10-20 |
| Cuboid AoE covers the whole rectangle; `xtask` Python fallback on Windows | `c01eef2a0`, `78fd519f1`, `4b04dc6e4` | [#7](https://github.com/NickMesser/AAEmu/pull/7) | Merged 2026-10-20 |
| Trade, cash shop, auction and loot-dice safety fixes | `5a2ac9f08` … `ebfda601c` (7 commits) | [#8](https://github.com/NickMesser/AAEmu/pull/8) | Merged 2026-10-20 |
| Every skill timeline id released, pool exhaustion survived; skills above the ability level rejected | `bd1d8535a`, `fa569eda2`, `15ae68da0`, `fd9cd70da` | [#9](https://github.com/NickMesser/AAEmu/pull/9) | Merged 2026-10-20 |
| Quest reward supply acts replace the level supplies | `e92ebb109` | [#10](https://github.com/NickMesser/AAEmu/pull/10) | Merged 2026-10-20 |
| Plot `ManaCost` uses the tooltip formula; client-matched health and mana regen | `9c8508acc`, `0fd581096` | [#11](https://github.com/NickMesser/AAEmu/pull/11) | Merged 2026-10-20 |
| Item ownership, merged-stack and loot id integrity | `d9515d1f8` … `4c8adba9d` (7 commits) | [#12](https://github.com/NickMesser/AAEmu/pull/12) | Merged 2026-10-20 |
| Socketing, lunastone and temper item details persisted | `ed1ad090f` | [#13](https://github.com/NickMesser/AAEmu/pull/13) | Merged 2026-10-20 |
| Linux clippy for `aaemu-clientdriver`; dash heading kept on the final step | `b892e1ee0`, `59cea9cb5` | [#14](https://github.com/NickMesser/AAEmu/pull/14) | Merged 2026-10-20 |
| Duel countdown at acceptance, refuse after the start ignored, challenge validation | `16d5e463a` (on the #5 branch) | [#15](https://github.com/NickMesser/AAEmu/pull/15) | Merged 2026-10-20 |
| Emote sent for another unit's object id ignored | `3dda89bcc` | [#16](https://github.com/NickMesser/AAEmu/pull/16) | Merged 2026-10-20 |
| Auction lots kept until their result mails are delivered | `4db88094f` | [#17](https://github.com/NickMesser/AAEmu/pull/17) | Merged 2026-10-20 |
| Summons with `despawn_on_creator_death` removed when the creator dies | `1fe501e80` | [#18](https://github.com/NickMesser/AAEmu/pull/18) | Merged 2026-10-20 |
| Bound and non-bag trade items refused; crafting counts and consumes bag materials only | `f740311d2`, `8a12189c3` | [#19](https://github.com/NickMesser/AAEmu/pull/19) | Merged 2026-10-20 |
| Testclient script command `loaded` | `b50699748` | [#20](https://github.com/NickMesser/AAEmu/pull/20) | Merged 2026-10-20 |
| Pending guild invitations, kick / role-change rank checks, loot master membership | `12336d422` | [#21](https://github.com/NickMesser/AAEmu/pull/21) | Merged 2026-10-20 |
| Client skill cast authority checks | `a6ca5fc4f` | [#22](https://github.com/NickMesser/AAEmu/pull/22) | Merged 2026-10-20 |
| Bank, shop and auction request guards | `334987a9a` | [#23](https://github.com/NickMesser/AAEmu/pull/23) | Merged 2026-10-20 |
| Slave repair timer started on death | `e9aedb49d` | [#24](https://github.com/NickMesser/AAEmu/pull/24) | Merged 2026-10-20 |
| Loot take range, in-place resurrection needs an offer (cleared on death), tutorial ids, UI data size | `1a27e69f6`, `c2d7593e2`, `b57e5275e` | [#25](https://github.com/NickMesser/AAEmu/pull/25) | Open |
| Account attribute saves serialised per account (duplicate-key race) | `388522ee1` | [#26](https://github.com/NickMesser/AAEmu/pull/26) | Open |
| Retire loot before reusing NPC object ids | `1fc588a2a` | [#3](https://github.com/NickMesser/AAEmu/pull/3) | Open |

## Guessed values to confirm against retail

These are server-side guards that retail enforced but the C# emulator did not, or constants with no
1.2 oracle. They are part of parity fixes or intentional checks, so they are recorded here instead of
being treated as deviations. Replace each with retail evidence and update this table with the result.

| Value | Where | Current choice | Evidence needed |
| --- | --- | --- | --- |
| Bank / auction NPC service range | `systems/economy/store.rs:205-208` (`SERVICE_RANGE`); used by `systems/economy/item_handlers.rs:307` and `auction_packets.rs:44`; `PORTING.md:855-864` | 5 m, matching the repair and pet-revive interactions | A retail capture or client check of the banker / auctioneer interaction range. The shop range (`SHOP_RANGE`, 3 m) is separate and came from the C# |
| Corpse loot take range | `systems/economy/looting.rs` (`LOOT_TAKE_RANGE`); `PORTING.md` § Loot take range | 5 m measured like `GetDistanceTo` (minus the model radii): retail 3 m plus 2 m slack | **Confirmed** 2026-10-21: the retail 3.5 client enables looting at 3.0 m or less on the target frame's distance and walks the player to the corpse; the 10.x decompile has a matching 3 m check. The Bevy client uses 3 m (PR #15) |
| Doodad skill-target allowance | `systems/skills/validation.rs:10-13` (`DOODAD_RANGE_ALLOWANCE`); `PORTING.md` § Client skill cast authority | Skill `max_range` plus 15 m, standing in for the missing doodad bounding boxes | Doodad model or bounding-box data, or a retail cast at a large doodad's edge |
| Slave skill-target allowance | `systems/skills/validation.rs:10-13` (`SLAVE_RANGE_ALLOWANCE`); `PORTING.md` § Client skill cast authority | Skill `max_range` plus 40 m, standing in for the zero `obb_*` columns in the 1.2 data | Ship `obb_*` values or a retail cast at a large ship's edge |
| Guild invitation expiry | `systems/social/expedition.rs:198-199` (`INVITATION_TIMEOUT_MS`); `PORTING.md` § Guild invitation and rank guards | 60 s, matching the party / raid invitations | A retail invitation expiry capture, or the client UI timer |
| Duel challenge timeout | `systems/social/duel.rs:46-50` (`REQUEST_TIMEOUT`); `PORTING.md` § Duel fixes | 30 s; recalled from 10.x, no 1.2 or compact source | 10.x `DuelManager` source or a retail capture |
| Duel start chat-bubble type | `systems/social/duel.rs:51-52` (`DUEL_AREA_BUBBLE_TYPE`); `PORTING.md` § Duel fixes | 543, carried over from a packet capture; `bubble_effects` 543 is unrelated lore text | A retail duel capture showing the bubble id |
| PvP honor and kill point kinds | `systems/skills/combat.rs:643-660`; `packets/g2c_misc.rs:103-109` | `SCUnitPvPPointsChangedPacket` kind 0 = honor gained, kind 1 = hostile-faction kills | **Confirmed** against the original 1.2 client on 2026-10-20; no further evidence needed, keep the client as the oracle |

## Adding an entry

1. Decide whether the change is an **intentional deviation** or a **parity fix**. Retail behavior the
   C# lacked belongs upstream, not in the deviation table.
1. Add a deviation row with the reason, the file or data it touches, any config switch, its status,
   and whether it is upstreamable. Link the design page ([Backports](Backports),
   [Player Bots](Player-Bots), [Verification](Verification)).
1. Record the behavior in `PORTING.md` in the same change, and add a `rust/verify/*.vtest` suite when
   the change is testable live (see [Verification](Verification)).
1. If the change carries a guessed value, add a row to the guessed table with the evidence needed.
   Mark a value confirmed only when the evidence is named (client build, capture, data table).
1. For a parity fix, open the upstream pull request first, then add its row to the parity table with
   the fork commit, PR number and state.
1. Update `Last verified against` here and the `Related` sections of the pages that link here.

## Using this page when syncing

Before merging `rust-rewrite` into `rust-rewrite-bots`:

1. Read the intentional deviations and the guessed-value list.
1. Take upstream's version wherever both sides carry the same fix.
1. Keep fork-only code; do not drop it to make the merge quiet.
1. Stop and report any conflict that would override a listed deviation, with the file and both
   commits. Do not resolve it by choosing a side.
1. After the merge, re-check the parity table (a fork copy may now be upstream) and the deviation
   statuses. Record new parity fixes in the table when their PRs are opened.

## Sources

- Policy and sync rules: `AGENTS.md`; the 2026-10-20 fork decision.
- Deviations: `SQL/patches/compact/2026-10-20-level-55-retail-3.0.sql`; `Docs/wiki/Backports.md`;
  `Docs/wiki/Player-Bots.md`; `SQL/patches/compact/README.md`;
  `rust/crates/xtask/src/main.rs`; `Scripts/verify.sh`; `Docs/wiki/Verification.md`.
- Bevy client: `D:/aaemu-client/AABevy1.2` (`crates/aa-ui/src/content.rs`,
  `crates/aa-ui/src/lua/globals.rs`, `feat/farm-plumbing`, `feat/farm-visual`); open client PRs
  `NickMesser/AABevy1.2` #6 and #9.
- Parity fixes: `gh pr list --repo NickMesser/AAEmu --author stsiakal --state all` on 2026-10-20;
  `Docs/wiki/Server-Backlog.md`.
- Guessed values: `PORTING.md` sections dated 2026-10-20; constants cited in the table above.

## Related

- [Home](Home)
- [Backports](Backports)
- [Player Bots](Player-Bots)
- [Verification](Verification)
- [Server Backlog](Server-Backlog)
- [Developer Notes](Developer-Notes)
- [Documentation Maintenance](Documentation-Maintenance)
