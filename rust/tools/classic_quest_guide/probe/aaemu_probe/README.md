# AAEmu probe (ArcheAge Classic addon)

A passive, read-only addon that records what the Classic addon API exposes, so a
full recorder can be designed from real data. It never targets, casts, chats,
moves or uses items. The only things it writes are its own observation files,
its own settings entry and its own two windows.

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
   the chat shows "AAEmu probe 0.1.1 loaded". If it does not, check the addon
   list in the game options (addons may need enabling there) and the log
   `Documents\AAClassic\ArcheAge.log` for `[Lua Error]` lines.
4. Play about 10 minutes: take and turn in a quest, kill and loot, use a
   doodad, open a merchant, mount. Press **Save**, then log out.
5. Collect `Documents\AAClassic\Addon\aaemu_probe\observations\probe_*.lua`
   (the path is `api.File:Write`-relative to the addons folder; if the files
   land elsewhere, search for `probe_0001_001.lua`).

## Updating from 0.1.0

0.1.0 never started: it used `_G`/`getfenv`/`setmetatable`/`select`, which the
Classic addon sandbox does not provide. To update, close the game and copy the
new `aaemu_probe` folder over the old one (keep `addons.txt`; `addon_settings`
already enables it). Then check `observations/boot_0.lua` ... `boot_4.lua`:
each step writes its own file with `ok` and any `err`, so a failure is visible
even if no window appears. Chat also shows "AAEmu probe 0.1.1: main.lua read".

## What it records

Files are `probe_<session>_<chunk>.lua`, one Lua table per file with a list of
records. Written at most every 15 s, on **Save**, on **Stop** and on unload.

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
- `bag`: every bag slot (`GetBagItemInfo(1, slot)`), count, capacity, gold and
  bank gold, on load, every 60 s and 1 s after `BAG_UPDATE`/`ADDED_ITEM`/
  `REMOVED_ITEM`. Unchanged contents are marked `unchanged`.
- `target`: on target change (polled once per second) every unit getter for
  `"target"`, the by-id getters (`GetUnitInfoById`, ...), buffs, debuffs and
  `id_like_fields`, a scan of every returned field whose name contains
  id/type/templ/kind/grade. Look there for NPC or doodad template ids.
- `api_surface`, `api_globals`, `api_inventory` (on load and again at the first
  `ENTERED_WORLD`): member names of every API library (finds undocumented
  functions), which sandbox globals exist, and the result or error of every
  read-only getter that takes no argument or `"player"`.
- `summary` (end of every file): event counts, suppressed counts, registration
  totals.

## Window

Status (events seen, distinct, suppressed, records buffered, files written,
last error), **Save**, **Stop/Start**, and a hint line. If 500 records are
buffered and cannot be written, logging **pauses**, the window says PAUSED, and
the number missed is counted and recorded when logging resumes.

## Privacy

The files contain your character name, position, target names, bag contents,
gold, and chat text from `CHAT_MESSAGE` (including whispers and other players'
names). Read them before sharing and delete what you do not want to send.

## Limits

- The loader may block some events; `RegisterEvent` does not report that, so
  an event that never fires may be blocked rather than quiet. COMBAT_MSG,
  COMBAT_TEXT, UNIT_DEAD and TARGET_CHANGED are used by working addons.
- The sandbox has no `_G`, `getfenv`, `setmetatable`, `select`, `error`, `os`
  or `io`; the probe uses none of them.
- There is no API for the active quest list: quest functions need a
  `questTypeId`, so no quest count is recorded; quest events are logged.
- Undocumented getters are called by name (`UnitLevel`, `GetNpcInfo`, ...); if
  they do not exist the result is recorded as `missing method`.
- Window layout uses the same widget calls as the quest guide addon; the style
  may differ from the stub used in tests.
