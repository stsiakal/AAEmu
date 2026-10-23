# oracle_suite — original 1.2 client vs Bevy `aa-client`

Runs the same scenario on the original ArcheAge 1.2 client and on the Bevy client
against one lane server, then reports packet-field and screenshot differences.

The original client runs **hidden** and is driven through `aaemu-clientdriver`
(Lua, keys, pointer, drag — no desktop input). Bevy runs `aa-client.exe` with
`--offscreen-shot --mute` and its line-based script runner. Both talk to the same
lane stack with `RUST_LOG=info,aaemu_game::net::fields=trace`.

## Run

```powershell
python rust/tools/oracle_suite/oracle_suite.py --lane oracle-suite
python rust/tools/oracle_suite/oracle_suite.py --lane oracle-suite mount_ride house_place
python rust/tools/oracle_suite/oracle_suite.py list          # scenario summaries
python rust/tools/oracle_suite/oracle_suite.py check         # parse all scenario files
```

Useful options:

| Option | Meaning |
| --- | --- |
| `--bevy-client PATH` | Bevy client (default `D:/aaemu-target/bevy/debug/aa-client.exe`) |
| `--original-client PATH` | 1.2 client (default `D:/AA1.2/bin32/archeage.exe`) |
| `--out DIR` | report directory (default `D:/aaemu-oracle-runs/<lane>-<time>`) |
| `--build auto\|always\|never` | build the lane when binaries are missing (default `auto`) |
| `--keep-stack` | leave the lane stack running afterwards |
| `--reuse-stack` | keep an already running stack instead of restarting it |
| `--bevy-size WxH` | offscreen Bevy size (default `1280x720`) |

The tool creates two throwaway accounts on the lane (the login server's
`AutoAccount`), writes their shared password only to a temporary `--password-file`
inside the report directory, and deletes it when the run ends. It gives the two
lane characters access level 100 in `lane_<lane>_game.characters` (GM commands
need it), positions them before login (the offscreen Bevy renderer crashes on a
far in-game teleport), and stops only the clients it launched and its own lane
stack; MySQL and other lanes are untouched.

## What a run produces

```
<out>/REPORT.md                  index with SAME/DIFFERENT/NOT RUN counts
<out>/run.json                   machine-readable checks and accounts
<out>/<scenario>/report.md       per-check evidence
<out>/<scenario>/sheets/*.png    original (left) / Bevy (right) contact sheets
<out>/<scenario>/original/trace.log, shots/
<out>/<scenario>/bevy/script.txt, trace.log, client.log, shots/
```

## Scenario files

One JSON file per feature in `scenarios/`. Each has `setup` (GM commands and a
teleport on the lane character), `steps`, `screenshots` (step names) and `checks`.

Steps use abstract actions; each adapter translates them to the client's own API:

| Action | Original | Bevy |
| --- | --- | --- |
| `gm COMMAND` | say-chat `/COMMAND` | `gm COMMAND` |
| `move X Y Z` | `/move X Y Z` | `tp X Y Z` |
| `key`, `key_down`, `key_up` | `client_input` (auto-release deadline) | `key`, `keydown`, `keyup` |
| `use_item` | `client_use_item` | `use` |
| `mount` | `X2Mate:MountMate()` | `X2Mate:MountMate()` |
| `bind_skill` | `X2Action:SetActionSpell((page-1)*48+cell, skill)` | `HUD.drag_action_set(cell,'skill',skill)` |
| `lua` | `client_lua` (UI state) | `lua print(__ORACLE_n ...)` |
| `click POINT` | `client_pointer` at POINT × client pixels | `cursor POINT` + `click left` |
| `drag FROM TO` | `client_drag` | not supported (NOT RUN) |
| `house_place` | `client_use_item` (starts the ghost) | `house_place item dx dy` |
| `build` | click the build button at POINT | `house_build` |
| `wait` | sleep | `wait` |

`POINT` coordinates are fractions of the client picture (0..1), so the same
scenario works at the original's 2560-wide back buffer and Bevy's 1280×720.

Checks:

* `packet` — count/extract fields of a packet in named steps
  (`CSUpdateActionSlotPacket` fields come from the body; decoded packets from the
  `fields:` trace line).
* `lua` — compare one Lua query per named step (`steps`, optional `expect`).
* `mount_metrics` — derive run speed, acceleration time, deceleration time, turn
  rate, backward speed, jump height and the packet-velocity/actual-speed ratio
  from `CSMoveUnit` field lines.
* `shot` — require a screenshot pair; the contact sheet is the evidence.

## Known limits

* The original uses its own Lua entry points for binding and mounting; the
  Bevy adapter uses the same request path the UI drop/hotkey uses, not a real
  drag. A generic UI `drag` has no Bevy script command yet.
* Both clients are relaunched per scenario; the original was previously kept in
  world, but its character row position then went stale for mate summons and the
  house placement ghost. Relaunching also isolates scenario state.
* House placement on the original test spot reports "obstacles block the way",
  so `house_place` currently has original packet checks NOT RUN; Bevy requests
  the quote and sends `CSCreateHouse`, but the server refuses the build (error
  481). The screenshots carry the evidence.
* Bevy's script timestamps have one-second server-log resolution, so step
  attribution near a boundary can include a neighbouring movement packet.
* The tool never rebuilds the Bevy client. Point `--bevy-client` at the build
  under test and record its revision in the report.
