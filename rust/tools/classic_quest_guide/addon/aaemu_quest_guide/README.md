# AAEmu 1.2 Quest Guide (AAClassic addon)

Version 0.1.0. An ArcheAge Classic Lua addon for browsing **expected 1.2 quest
data** from the AAEmu static database. It never treats those values as Classic
observations and never performs game actions.

## Manual installation (exact)

1. Create `Documents/AAClassic/addons` if it does not exist.
2. Extract the ZIP so the folder is exactly:
   `Documents/AAClassic/addons/aaemu_quest_guide`
   (`aaemu_quest_guide/main.lua` must exist).
3. Open `Documents/AAClassic/addons/addons.txt` and add this line once:
   `aaemu_quest_guide`
4. Restart the client if this is a new addon, then enable **AAEmu 1.2 Quest
   Guide** in the Addons button on the character-select screen.
5. Log in. The guide window opens; a small **Quest Guide** button remains at
   the top right if you hide the window.

This package is **not published or listed in the Addon Manager catalog**. The
manager's remote/custom URL behavior is not claimed here; install this ZIP
manually. A marketplace listing would need separate publishing and review.

## Controls

- **Quest ID + Open**: open any catalog quest by numeric ID.
- **Filter + Find**: bounded search by ID/name/status/category/level; up to 200
  matches, then Prev/Next Quest browses the filtered list.
- **Prev/Next Quest**: move through the current browser list.
- **Route 330>6198**: opens the candidate starter list. It is **unverified, not
  validated and not optimal**. Quest 330 should be taken before 6198.
- **Step / Requirements / Rewards / Sources / Branches / Info** pages, with
  `<` and `>` text paging. No page is thousands of lines.
- **Prev/Next Step** advances the displayed step. Next Step records a **manual
  acknowledgement only**; it is not completion. Classic completion comes only
  from `api.Quest:IsCompleted`.
- **Start/Stop recording**, **Snapshot player/target**, **Save/Export**, and
  report buttons **Blocked / Missing NPC / Missing doodad / Wrong outcome** with
  an optional note. Reports are user annotations, not confirmed game events.
- **Use 1.2 candidate**: explicitly opts in to the selected step's first 1.2
  world location. You must type and confirm the Classic world label; the addon
  cannot see the current world. A permanent **unverified 1.2 placement** banner
  is shown while active. Moving behind the camera shows *Turn to find waypoint*
  and no invented direction.
- **Capture player/target**: saves current client coordinates as a separate
  **observed Classic** waypoint.
- **Clear nav**: stops waypoint display. Character/context/step/world changes
  also clear it.
- Server, build, context and world fields are optional declared labels used for
  recording/resume scoping. The note field is used by the report buttons.

## Saved files and resume

- Observations: `Documents/AAClassic/addons/aaemu_quest_guide/observations/`
  with names like `session_000001_chunk_001.lua`.
- Samples are taken at most once per second, buffered up to 128 records, and
  written at most every 30 seconds or on Save/Stop/Unload.
- If a write fails, records stay in memory for **Retry** via Save/Export. When
  the buffer is full, recording pauses instead of silently dropping. If the
  client exits while writes keep failing, unsaved records can be lost.
- Addon settings (session counter, form labels, per-character/context resume)
  are stored by the client under the addon ID `aaemu_quest_guide`.
- Resume is scoped to player name + declared server/context + catalog database
  hash. A manual step acknowledgement is restored; completion is always re-read
  from the Classic client.

## Data and API limits

- Expected data is **1.2** from `compact.sqlite3` + `Data/Worlds`; provenance
  (DB hash, generator commit) is in `data/meta.lua` and the package manifest.
- Compact quest names and context texts are Korean **internal 1.2 data**. The
  Classic localized title/body are read separately from `api.Quest`; the
  fallback is `Quest#<id>`.
- 1.2 `Data/Worlds` positions are **unverified candidates**, never automatic
  Classic coordinates. There is no heading API and no proximity completion.
- There is no current-world API; candidate world matching is a manual label
  confirmation. `no_spawn`/`dynamic_spawn`/`unsupported` steps have no candidate
  waypoint. Scheduled locations stay labelled conditional.
- Requirements are displayed, not evaluated as pass/fail. The addon does not
  assume objective counters; only documented reads are sampled.
- No global world/doodad enumeration, no telemetry, no network upload, no chat
  or game actions.

## Live smoke checklist (run in Classic; not yet performed by the builder)

1. Extract, add `aaemu_quest_guide` to `addons.txt`, enable, log in.
2. Open quest `330`; confirm Classic title/body and internal Korean name are
   shown as separate labels.
3. Open `Route 330>6198`; confirm the warning that 330 precedes 6198.
4. Open step requirements, rewards and sources; confirm paging stays bounded.
5. Default navigation must be off. Type `main_world` (or the candidate's world),
   press **Use 1.2 candidate**, confirm the permanent unverified banner.
6. Turn the camera until the waypoint is behind it; confirm **Turn to find
   waypoint** appears without a direction.
7. Capture player/target, then clear navigation.
8. Start recording, wait 2+ seconds, save/export, and check the exact relative
   path shown and the file under `observations/`.
9. Click a report button with a note; confirm it is tagged as a user annotation.
10. Hide the main window; confirm the **Quest Guide** button restores it.

## Build (developers)

The package contains no Python and no SQLite/client assets. It is produced from
the builder and this runtime source; see `rust/tools/classic_quest_guide/`.
