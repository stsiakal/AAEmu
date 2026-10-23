# AAEmu probe (ArcheAge Classic addon)

A passive, read-only addon that records what the Classic addon API exposes, so a
full recorder can be designed from real data. It never targets, casts, chats,
moves or uses items. The only things it writes are its own observation files,
its own settings entry and its own two windows.

Version 0.2.0 adds the passive recorder on top of the 0.1.1 probe: quest getter
transitions, kill/loot/container/quest-reward derivations, NPC and item
catalogues, own pet dumps and the fixed API member list.

## Install

The loader (`game/scripts/x2ui/addons/addons.lua`, read from `game_pak`) looks in
`<Documents>\AAClassic\Addon\` (the folder the client already created next to
`UCC`), reads `addons.txt` there, then loads `<name>/main.lua` per line.

1. Close the game. Unzip `aaemu_probe.zip` so you get
   `Documents\AAClassic\Addon\aaemu_probe\main.lua` (and `core.lua`, ...).
2. Create `Documents\AAClassic\Addon\addons.txt` if missing and add one line:

   ```
   aaemu_probe
   ```

3. Start the game and log in. A small "AAEmu probe" window appears (top left);
   the chat shows "AAEmu probe 0.2.0 loaded". If it does not, check the addon
   list in the game options (addons may need enabling there) and the log
   `Documents\AAClassic\ArcheAge.log` for `[Lua Error]` lines.
4. Play about 10 minutes: take and turn in a quest, kill and loot, open a
   container, mount, target a mob twice (and your mount). Press **Save**, then
   log out.
5. Collect `Documents\AAClassic\Addon\aaemu_probe\observations\probe_*.lua`
   (the path is `api.File:Write`-relative to the addons folder; if the files
   land elsewhere, search for `probe_0001_001.lua`).

## Updating from 0.1.1

Close the game and copy the new `aaemu_probe` folder over the old one (keep
`addons.txt`; `addon_settings` already enables it). The boot trail still writes
`observations/boot_0.lua` ... `boot_4.lua`; each step has `ok` and any `err`, so
a failure is visible even if no window appears.

## What it records

Files are `probe_<session>_<chunk>.lua`, one Lua table per file with a list of
records. Written at most every 15 s, on **Save**, on **Stop** and on unload.
Every record has `k` (kind), `seq`, `ms` (`Time:GetUiMsec`) and the derived
records also carry position and zone where useful.

Raw and registration records:

- `registrations` / `registrations_api_on`: for each of the 206 `UI_EVENTS`
  (plus any global `UI_EVENTS` the sandbox exposes) whether `RegisterEvent`
  raised; and the `api.On` events (`CHAT_MESSAGE`, `TEAM_MEMBERS_CHANGED`,
  `UI_RELOADED`, `UPDATE_PING_INFO`, `UPDATE`).
- `event`: name, source (`widget` or `api.On`), `Time:GetUiMsec`, `GetLocalTime`,
  all arguments (depth 3, tables expanded, strings cut at 300 characters, at
  most 40 keys per table, `_more_keys` shows the rest), player world position,
  zone group, and the target id, name and `UnitInfo`. Context is cached for
  200 ms. At most 40 records per event name per 10 s; the rest are counted in
  the `summary` record (`suppressed`), never dropped silently.
- `summary` (end of every file): event counts, suppressed counts, record counts
  by kind, registration totals.

Ignored as noise (counted in the summary only, never stored raw):

- `END_QUEST_CHAT_BUBBLE`, `TARGET_OVER`.

Snapshots and catalogues:

- `bag`: every bag slot (`GetBagItemInfo(1, slot)`), count, capacity, gold and
  bank gold, on load, every 60 s and 1 s after `BAG_UPDATE`/`ADDED_ITEM`/
  `REMOVED_ITEM`. Unchanged contents are marked `unchanged`.
- `target`: on target change (polled once per second) every unit getter for
  `"target"`, the by-id getters (`GetUnitInfoById`, ...), buffs, debuffs,
  `id_like_fields` (fields whose name contains id/type/templ/kind/grade),
  `fingerprint` and `own_pet` when the target is the current `playerpet1`/2.
- `pet` / `pet_absent`: `playerpet1` and `playerpet2` are dumped on every target
  change, so the own mount/pet identity is recorded even when it is not the
  current target. Pet identity uses the same fingerprint.
- `npc_catalogue`: fingerprint (`name|level|faction|max health`) to the
  runtime ids, positions seen (deduplicated, up to 20) and every
  `DYNAMIC_ACTION_BAR_SHOW` `N` seen for it. A new id under the same
  fingerprint is a respawn; comparing `N` across respawns answers whether `N`
  is stable.
- `item_catalogue`: item type id from `|i<id>,...` links to the bag item's
  name, grade, category, stack and cost. Filled from every bag snapshot and
  from `ADDED_ITEM`.
- `api_surface`, `api_globals`, `api_inventory` (on load, after 10 s and at the
  first `ENTERED_WORLD`): the old enumeration (empty in the sandbox), which
  globals exist, and the result or error of every read-only getter.
- `api_members`: the fixed documented/known member list probed per library
  (`members` = name to type, `missing` = names that do not exist). This
  replaces the enumeration that needs `next`.

Derived records (passive, computed from events already received):

- `quest_state`: for every questTypeId in `CHAT_MSG_QUEST` arg 9 and
  `QUEST_LEFT_TIME_UPDATED` arg 1, `IsCompleted`, `GetActiveQuestTitle`,
  `GetQuestContextMainTitle` and `GetQuestContextBody` are called on first
  sight and again on every later event for that id. `transition` is
  `accepted` / `active` / `completed` / `uncompleted` / `changed` /
  `unchanged`; changed records include the previous state.
- `kill`: combat start/end for a unit plus `EXP_CHANGED`: target fingerprint
  (name, level, faction, max health), position, exp amount and timing.
- `loot`: `ADDED_ITEM` with source 10 within 3 s of a kill: item type id from
  the link, count, NPC fingerprint and kill position.
- `container_open`: an `Open` cast, the following `consume` and the
  `ADDED_ITEM`s in the same millisecond, flushed after a 250 ms grace.
- `quest_reward`: `ADDED_ITEM` source 35, the last quest id seen, the
  `PLAYER_MONEY` delta and `EXP_CHANGED` amount (same tick or within 5 s).
- `action_bar`: every `DYNAMIC_ACTION_BAR_SHOW` with its `N`, kind
  (`npc`/`doodad`) and the target fingerprint.

## Window

Status (events seen, distinct, suppressed, records buffered, files written,
last error), **Save**, **Stop/Start**, and a hint line. If 500 records are
buffered and cannot be written, logging **pauses**, the window says PAUSED, and
the number missed is counted and recorded when logging resumes.

## Importing recordings as evidence

`rust/tools/classic_quest_guide/import_observations.py` parses these files
without executing Lua and writes deterministic JSON evidence (quests, rewards,
item sources, NPC positions and kills), stamped with the probe version, session,
file, seq and local time:

    python import_observations.py --input <observations dir> \
        --output evidence.json [--db <1.2 compact.sqlite3>]

`--input` may be repeated. Quest completion is inferred from a completed
`quest_state`, a quest turn-in item removal (source 35) or a quest reward, and
the evidence kinds are listed. `completed: false` means no completion evidence
was recorded, not that the quest was proven incomplete. With `--db`,
quest-related records also carry the 1.2 expected quest/supply/reward data and a
match/mismatch flag; a mismatch is a suspect, not proof of a bug. Tests:
`python -m unittest test_import` from the same folder.

## Privacy

The files contain your character name and position, target and pet names, NPC
and item names, bag contents and gold, quest titles and text, kill/loot history,
and chat text from `CHAT_MESSAGE` (including whispers and other players'
names). Read them before sharing and delete what you do not want to send.

## Sending the files

Zip the whole `observations` folder (or attach the `probe_*.lua` and
`boot_*.lua` files) and send it to the AAEmu developer you are working with, or
attach it to the relevant issue/PR. Do not include account credentials or
anything outside `observations`.

## Limits

- The loader may block some events; `RegisterEvent` does not report that, so
  an event that never fires may be blocked rather than quiet.
- The sandbox has no `_G`, `getfenv`, `setmetatable`, `select`, `error`, `os`
  or `io`; the probe uses none of them. There is no `next`, so libraries are
  probed by name (`api_members`), not enumerated.
- There is no API for the active quest list: quest functions need a
  `questTypeId`, so quest getters run only for ids that appear in quest events.
  Quest id 0 (flavour dialogue) is skipped.
- There is no unit template id: the fingerprint is name + level + faction + max
  health. Same-name mobs can share it; that is exactly what the NPC catalogue
  tests across respawns.
- Kill correlation requires a combat state change (start then end, or the
  player/target id) before `EXP_CHANGED`; an exp change with no combat context
  is not reported as a kill.
- Undocumented getters are called by name (`UnitLevel`, `GetNpcInfo`, ...); if
  they do not exist the result is recorded as `missing method`.
- Window layout uses the same widget calls as the quest guide addon; the style
  may differ from the stub used in tests.
