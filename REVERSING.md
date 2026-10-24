# Clients, reference binaries and reverse-engineering knowledge

Where everything is for porting **zone-server features into the 1.2 Rust stack**, using the
**10.x client/zone binaries as the reference**. Start the stack itself with
[`AGENT-QUICKSTART.md`](AGENT-QUICKSTART.md).

Paths are for this machine. Both client folders are **read-only**: never modify, add or delete
files in them (a client writing its own logs at runtime is fine). Put all RE output under `D:\aa-re\`.

## The two clients

| | 1.2 client (the one we serve) | 10.x client + zone server (reference only) |
|---|---|---|
| Path | `D:\AA1.2` | `D:\ArcheAge Returns 10.0.2.13 - 8yx - r575 - 2026-06-18` |
| Version / arch | 1.2 Trion `r208022`, **x86** (`bin32\`) | 10.0.2.13 `r575`, **x64** (`Bin64\`) |
| Game logic DLL | `bin32\x2game.dll` | `Bin64\x2game.dll`, dev build `x2game-dev.dll` |
| Physics | `bin32\cryphysics.dll` (VMProtect-packed entry stubs only; unpacked runtime dump and Ghidra decompile in `D:\aa-re\cryphysics\x86_12\`) | `Bin64\cryphysics.dll` (client), `cryphysics_dedicate.dll` (zone) |
| Zone server | — | `dedicatedserver.exe` + `Bin64\*_dedicate.dll` (`x2game-dev_dedicate`, `cry3dengine_`, `cryaction_`, `cryaisystem_`, `cryentitysystem_`, `crynetwork_`, `cryphysics_dedicate`) |
| Content | `game_pak` (worlds, models, navmesh) | `game_pak` (68 GB) |
| Start it | `Scripts/live-stack.sh client` (our `aaemu-launcher`, ticket login) | `launch_aaemu.bat` (`-devmode`, targets an older local AAEmu setup); see `FAST_START.md` |
| Notes | HackShield does not start under our launcher. Lua can be run inside it with `aaemu-clientdriver`. | `Bin64\xlcommon.dll` is the `aa_debugger` proxy (`aa_debugger\`; the original is needed for standalone loading). `x2game-dev.dll` carries a 2-byte fast-start patch (`patch_fast_start.py status/restore`, backup `.faststart.bak`). |

### Other clients (reference only)

- **ArcheAge Classic 3.0.4.2 client (r336598)**: `C:\Users\stsiakal\Downloads\ArcheAge Classic 2024-03-19 (r336598)\ArcheAge Classic 2024-03-19`
  (**read-only**). 32-bit, FileVersion 3,0,4,2, `game_pak` 36.8 GB (opens with the repo `aapak` reader).
  `x2game.dll`/`archeage.exe`/`crynetwork.dll` are **packed like 1.2** (unpacked copies and a Ghidra project exist, see
  Tools). `game\db\compact.sqlite` is **encrypted** (the cipher is in the packed `x2game.dll`; unlike the 10.x
  `game_pak` copy it is not plain SQLite). Its private-server guard (`daruguard.dll`) is only loaded by that
  project's own launcher, not by `archeage.exe` started directly. Static inventory: `D:\aa-re\aac\INVENTORY.md`.
- **AA Classic 3.5 install** `C:\Games\AAClassic`: its `x2game.dll`/`xlcommon.dll` import the private server's
  modules.

**Same engine, different game layer.** The CryPhysics in both clients is the same generation (48 of 55
matched vtables have identical slot counts; same contact dispatch tables and default tuning values), so 10.x physics behaviour
transfers to 1.2. The **network protocol differs**: different opcodes (see
`Bin64\decompiled\OPCODES_10x.md`), 10.x adds the encrypted level-5 (DD05) channel, and some packet
layouts changed. Data formats also drift (e.g. navmesh `areas` v22 in 10.x vs v21 in 1.2). Always
confirm a 10.x behaviour against what the 1.2 client sends/expects before porting it.

### The 10.x zone server (`dedicatedserver.exe`)

- `Bin64\dedicatedserver.exe -devmode +zone <name>` (default `w_gweonid_forest_1`); connects to a
  world server on `world_serveraddr:world_serverport` (default 1240).
- It **cannot fully boot**: it needs 41 server-only DB tables the client data lacks, and
  `game\db\game.sqlite3` is encrypted. It gets as far as engine/physics/AI init.
- It writes `ArcheAge.log` into the **real `Documents\ArcheAge`** (and moves the client log into
  `LogBackups`) no matter where it runs from — don't run it casually.
- Its physics DLL *can* be driven standalone: that is the oracle below.
- **Feasibility** (static study, `D:\aa-re\zone\feasibility\FEASIBILITY.md`): booting the zone against a fake
  world server is feasible but expensive (41 server-only tables, 30 of them derivable from the 1.2/3.0.4.2 DBs;
  small WZ/ZW join handshake, transport framing unverified) and only a partial oracle. The zone covers NPC
  AI/pathing, aggro, spawners and physics/geometry; the **world** computes damage/heal formulas, buffs, loot, XP
  and quests, so those formulas are **not in the zone binary**.
- The 10.x client `game_pak` `game\db\compact.sqlite3` is **plain SQLite** (233 MB, 1003 tables); only the zone's
  own `game.sqlite3` is encrypted. The 1.2 `game_pak` ships AI behaviour Lua source under
  `game/scripts/ai/behaviors` (a static oracle for AI that needs no running zone).
- **10.0 server database, decrypted**: `D:\aa-re\db-references\10.0-server\server_decrypted.sqlite3` (552 MB,
  1,374 tables; provenance and hash in the folder's `README.md`). It is the zone's `game.sqlite3` in plain form and
  holds **all 41 server-only tables** above (spawners, spawn effects, NPC groups, loot groups, buff rules, siege
  plans, indun rooms), so the zone no longer lacks its data. Use it read-only as retail evidence where 1.2 server
  data is missing or broken (quests, spawns, loot, AI params) **and the system did not change since 1.2**;
  NickMesser fixed quests this way. Do not copy 10.x values into later-rebalanced systems (item stats, skill
  numbers), and never commit it.

## Tools

- **IDA Pro 9.4**: `C:\Program Files\IDA Professional 9.4` (`ida.exe` GUI, `idat.exe` headless).
  Only the **x64** Hex-Rays decompiler is installed (no x86): IDA gives disassembly, xrefs and
  `.funcs.csv`/strings for x86 DLLs but no C. **Use Ghidra for 1.2 (x86) decompiles.**
  - Headless batch: `idat.exe -A -B "-S<script.py>" "-L<log>" <dll-or-.i64>`. Pass output paths through
    environment variables (IDA's `-S` mangles paths with spaces). Copy a DLL into a work dir under
    `D:\aa-re\` first so the `.i64` lands there. Reopening a saved `.i64` skips analysis.
    The export script used for the current databases is `D:\aa-re\scratch\ida\export.py`
    (env `AA_OUT`, `AA_NAME`, `AA_DECOMP`; driven by `scratch\ida\run.sh`): it writes
    `.funcs.csv`, `.strings.txt`, `.imports.txt`, `.exports.txt` and, for x64, the Hex-Rays `.c`.
  - **ida-pro-mcp** (MCP tools `mcp__ida-pro-mcp__*`): serves the one database open in an IDA GUI
    instance (Ctrl-Alt-M, port 13337). Good for interactive look-ups; use headless idat for bulk work.
- **Ghidra 12.1.4 PUBLIC**: `D:\Tools\ghidra_12.1.4_PUBLIC`, running on the JDK in `D:\Tools\jdk`
  (Temurin 25; Ghidra 12.1.4 needs JDK 21+). Archives are kept in `D:\Tools\_dl`. This is the tool for the
  **1.2 client DLLs** (and the 3.0.4.2 client): the unpacked 1.2 `x2game.dll` lives in Ghidra.
  - **Ghidra project directories must be given in the `C:\AAEMU_DISK\...` form, not `D:\...`**:
    `D:` is a `subst` of `C:\AAEMU_DISK`, and the subst path breaks Ghidra's OSGi (felix) start-up.
  - Launch headless through the wrapper `D:\aa-re\aac\ghidra\hl.cmd` (also `ghidra-headless.cmd` there); it
    pins `JAVA_HOME` and redirects `APPDATA` to `aac\ghidra\appdata12|appdata30|appdata`.
    Run files: `aac\ghidra\run_aa12u.cmd` (1.2 x2game + crynetwork), `client12\ghidra\run_archeage.cmd`
    (1.2 archeage.exe), `run_aa30u.cmd` / `run_import.cmd` (3.0.4.2). Post-scripts are in
    `D:\aa-re\aac\ghidra\scripts\` (`CountFunctions.java`, `ResolveExternals.java`). The repo-local
    `ghidra-work\` folder described in older notes **does not exist**.
  - Project directories must have **no dot-prefixed path element** (Ghidra rejects them).
    Do not use `Start-Process -RedirectStandardOutput` (sandbox denies it); invoke with `cmd /c`.
- **MSVC** (for the oracle and native client-driver parts): found via the `build.bat` files; the physics
  oracle needs MSVC 2022.
- **Python** for the extraction tools: `c:\python312\python.exe` (run with `-I`).

Saved IDA databases (open these instead of re-analysing):

| Binary | `.i64` |
|---|---|
| 10.x zone DLLs (`cry3dengine_`, `cryaction_`, `cryaisystem_`, `cryentitysystem_`, `crynetwork_`, `cryphysics_dedicate`) and `x2game-dev_dedicate.dll` (89,574 functions) | `D:\aa-re\zone\bin\` (decompiles of the six `_dedicate` DLLs in `D:\aa-re\zone\out\`; for `x2game-dev_dedicate` only indexes sit in `zone\bin\`, its `.c` is in `<10.x>\Bin64\decompiled\`) |
| 10.x client `crynetwork.dll`, `xlcommon.dll`, `xlpack.dll` | `D:\aa-re\zone\client10\` (with `.c`) |
| 1.2 `crysystem.dll`, `cryscriptsystem.dll` (not packed; no `.c`, IDA has no x86 decompiler) | `D:\aa-re\client12\ida\` |
| 1.2 `cryphysics.dll` | Ghidra project `C:\AAEMU_DISK\aa-re\cryphysics\x86_12\ghidra` (`cp12`, `cryphysics_unpacked.dll`); decompile `D:\aa-re\cryphysics\x86_12\out\cryphysics.c` (2,841 functions); unpack steps in `D:\aa-re\cryphysics\NOTES.md` |

Saved Ghidra projects (open these instead of re-analysing; the path is the `C:\AAEMU_DISK` form):

| Project | Binaries | State |
|---|---|---|
| `C:\AAEMU_DISK\aa-re\client12\ghidra\proj-aa12u` (`aa12u`) | `x2game_unpacked.dll`, `crynetwork_unpacked.dll`, `archeage_unpacked.exe` (1.2) | **The 1.2 client logic oracle.** `x2game` fully analysed: 65,437 functions (18,961 labelled, but only 99 real names: the rest are compiler `Unwind@`/`Catch_All@` labels and thunks, so the game logic is unnamed); `crynetwork` 11,698 / 1,957; `archeage` 176 / 32 (mostly packer data). The dumps keep the original import tables, so Win32/CRT names resolve. `.data` is absent from the runtime dump, so globals/RTTI descriptors resolve by reference only |
| `C:\AAEMU_DISK\aa-re\aac\ghidra\proj-aa30u` (`aa30u`) | `x2game_unpacked.dll`, `crynetwork_unpacked.dll` (3.0.4.2) | `x2game` 56,263 functions (131 named; external calls resolved with `aac\unpack\extmap_30.txt`), `crynetwork` 9,778 / 245 (externals unresolved: dumped too late) |
| `C:\AAEMU_DISK\aa-re\client12\ghidra\proj-aa12-engine` (`aa12engine`) | `cryaction.dll` (15,108 functions / 5,753 named), `cryanimation.dll` (7,163 / 2,329), `cryentitysystem.dll` (4,282 / 1,626), `crygame.dll` (5,390 / 43) from `D:\AA1.2\bin32` (unpacked originals, copies in `D:\aa-re\client12\engine\`) | **The 1.2 engine/animation oracle** (CAnimatedCharacter, animation graph runtime, CExactPositioning, movement controller). Exports (functions CSV, strings, imports, exports, Ghidra decompile `.c`) in `D:\aa-re\client12\engine\out\`. Run files `client12\ghidra\run_engine*.cmd`. Separate from `proj-aa12u` on purpose. |
| `C:\AAEMU_DISK\aa-re\client12\ghidra\proj-aa12-engine2` (`aa12engine2`) | 11 remaining 1.2 engine DLLs (unpacked originals): `x2ui`, `cryfont`, `crysoundsystem`, `crymovie`, `cryinput`, `x2common`, `xlcommon`, `xlleveldb`, `cryaisystem`, `cry3dengine`, `cryrenderd3d10` | Fully analysed and decompiled: 41,433 functions / 15,655 named, 0 decompile failures (40,954 emitted; thunks/externals skipped). Exports in `D:\aa-re\client12\engine2\out\<dll>.{c,funcs.csv,strings.txt,imports.txt,exports.txt}`; run files `client12\ghidra\run_engine2.cmd` / `run_engine2_post.cmd`; logs `engine2_<dll>.log`. `x2ui` (UI widget framework) done first |
| `C:\AAEMU_DISK\aa-re\client12\out-x2game\` | Plain-file export of the whole 1.2 `x2game` (from `proj-aa12u`, read-only): `x2game.dll.c` (65,230 functions decompiled, 0 failures), `funcs.csv`, `strings.txt`, `strings-xrefs.tsv` (string -> functions), `calls.tsv` (caller -> callee). Run file `client12\ghidra\run_x2game_export.cmd`. Use these instead of opening the Ghidra project, so many workers can search in parallel |

`proj-aa12` (packed 1.2 `x2game.dll` as shipped) and `proj-smoke` no longer exist.

### Unpacking a packed client DLL (1.2 and 3.0.4.2)

`x2game.dll`, `crynetwork.dll` and `archeage.exe` are packed by in-place section encryption (no
VMProtect/Themida marks, imports intact, preferred base honoured). The scripts are
`D:\aa-re\aac\unpack\{start_client.py, dump_modules.py, rebuild.py}` (notes: `aac\unpack\NOTES.md`):

1. `start_client.py <archeage.exe> <secs>` starts `archeage.exe` directly (no `aaemu-launcher`, no
   injection) with a dummy ticket and dummy user against `127.0.0.1` (it creates the inheritable
   `archeage_auth_ticket_map`/`_event` objects). The client dies on its own after ~25 s because the login fails.
2. Poll the loaded-module list and **stop if any guard module is loaded** (`daruguard`, `hshield`, `ahn`,
   `xigncode`, `mrac`, `nprotect`); on the stock clients none is.
3. About 3 s after start (target DLL loaded) run `dump_modules.py <PID> <outdir> x2game.dll archeage.exe ...`
   (read-only `ReadProcessMemory`). Later dumps differ by per-run call-site rewriting. `crynetwork.dll`
   loads later: poll for it and dump immediately.
4. `rebuild.py <orig> <mem> <out>` splices the dumped sections into a copy of the original PE
   (`x2game_unpacked.dll`, etc.), then import it into Ghidra.

Existing results: 1.2 in `D:\aa-re\client12\` (`x2game_unpacked.dll`, `crynetwork_unpacked.dll`,
`archeage_unpacked.exe`, `capture\`, `unpacked\`), 3.0.4.2 in `D:\aa-re\aac\unpack\`. The older
`regrade\db-auth\runtime` dumps and `rebuild_unpacked.py` are gone (these scripts replace them). Never do
any of this inside the client folders; outputs go to `D:\aa-re\`.

## Knowledge base

### In the repo

| Doc | What |
|---|---|
| `rust/docs/physics/ZONE-PHYSICS-USAGE.md` | **Start here for zone features**: every physics use in the 10.x zone (with x2game addresses), the ZW*/WZ* results the world server receives, world geometry loading, prioritized feature list |
| `rust/docs/physics/CRYPHYSICS-MAP.md` + `cryphysics/{world,entities,geometry,dynamics}.md` | CryPhysics architecture, 1.2 vs 10.x comparison, function addresses in both DLLs, open-question answers |
| `rust/crates/aaemu-physics/README.md` | The bit-exact port, fidelity policy, module map, the **oracle** (`physoracle.exe`, scenarios, goldens, `AAEMU_PHYS_ORACLE`) |
| `rust/crates/aaemu-game/PORTING.md` | Per-subsystem status, deviations, C# and engine bugs mirrored; "systems::ai collision" = how physics is wired into the game server |
| `rust/crates/aaemu-game/CONTRIBUTING-PORT.md` | Game-crate module layout, object model, locking, test conventions |
| `rust/docs/client/LUA-DRIVER.md` | Running Lua inside the 1.2 client (addresses, design, usage) |
| `D:\aa-re\aac\RE-MAP-30.md` | 3.0.4.2 client RE map: 1.2 -> 3.0 function matches (`map30\map30.csv`, `m12_<addr>` names in `proj-aa30u`), subsystem coverage, 3.0-only clusters. Note: the 1.2 project's "18,961 named" are almost all `Unwind@`/`Catch_All@` labels (99 real names) |
| `rust/LIVE-TESTING.md` | Private MySQL, live servers, headless test client, C#-vs-Rust parity method |
| `rust/PORT-AUDIT.md` (+ `rust/tools/port_audit/`) | Every former C# file mapped to its Rust counterpart |
| `rust/CLIENT-DB-GAP-AUDIT.md` | Server vs. 1.2 client + databases gap audit (packets, unused tables, hardcoded values, quests, Ghidra unpack evidence); supersedes `rust/CLIENT-FEATURE-GAPS.md` |
| `FEATURE-GAP-TASKS.md` | The prioritized task list (P0–P3) implementing those gaps, with per-task oracles |

### On disk (`D:\aa-re\`, plus the 10.x folder)

Present on this machine:

| Path | Contents |
|---|---|
| `<10.x>\Bin64\decompiled\x2game-dev_dedicate.dll.c` | Full decompile of the zone game logic (~89k functions, ~3.2 M lines) — the main oracle for zone behaviour. The other `decompiled\` files older notes list (`crynetwork.c`, `xlcommon.c`, `OPCODES_10x.md`, indexes) are **not there**; use `zone\client10\` and `zone\bin\` below |
| `D:\aa-re\zone\bin\` | The `.i64` databases and DLL copies of the six `_dedicate` DLLs and `x2game-dev_dedicate.dll` (`.funcs.csv`, `.strings.txt`, `.imports.txt`, `.exports.txt`) |
| `D:\aa-re\zone\out\` | Hex-Rays decompiles (`.c`) and indexes of the six `_dedicate` DLLs: `cry3dengine`, `cryaction`, `cryaisystem`, `cryentitysystem`, `crynetwork`, `cryphysics` |
| `D:\aa-re\zone\client10\` | 10.x client `crynetwork.dll`, `xlcommon.dll`, `xlpack.dll`: `.i64`, `.c` and indexes |
| `D:\aa-re\zone\feasibility\` | Zone-server feasibility study (`FEASIBILITY.md`) with helper data: WZ/ZW handler and opcode tables rebuilt from the decompile (`wz_handlers.txt`, `zw_ops.txt`, `pkt_names.txt`), the 41 server-only tables (`server_tables.json`, `table_rows.md`), every zone `SELECT` (`sql_all.tsv`), pak listings (`pak10.txt`, `pak12.txt`) |
| `D:\aa-re\client12\` | 1.2 client work: `x2game_unpacked.dll`, `crynetwork_unpacked.dll`, `archeage_unpacked.exe` (inputs in `capture\`, `unpacked\`), Ghidra logs and run file (`ghidra\`; the project itself is `C:\AAEMU_DISK\aa-re\client12\ghidra\proj-aa12u`), `ida\` (`crysystem`, `cryscriptsystem`), UI/Lua extraction outputs (`out-*`, `ui*`, `x2ui-*`, `tools\`) |
| `D:\aa-re\aac\` | ArcheAge Classic 3.0.4.2 client work: `INVENTORY.md`, `pak_list.txt`, `pe_summary.txt`, `paktool\` (pak reader wrapper), `pakx\` (extracted samples), `unpack\` (scripts, memory dumps, `x2game_unpacked.dll`), `ghidra\` (wrapper, scripts, `proj-aa30u`) |
| `D:\aa-re\scratch\ida\` | The IDA headless export script and job runner |
| `D:\aa-re\REBUILD-NOTES.md` | What was rebuilt after the original RE folders were lost, and what is still missing |

**Not on this machine** (older notes mention them; rebuild if needed):

| Path | How to get it back |
|---|---|
| `D:\aa-re\cryphysics\` (`x64_10x`, `x86_12`, `compare\func_match.csv`, `unpack_x86`, `harness`, `work_x64`, `work_x86`) | Rebuilt 2026-10-19 (`NOTES.md`): `x86_12\` = 1.2 unpacked DLL + Ghidra decompile (2,841 functions), `unpack_x86\` = runtime-dump unpack tools and captures, `x64_10x\` = 10.x client DLL + IDA decompile, `compare\func_match.csv` = 1,281 matched 1.2/10.x function pairs. `harness`, `work_x64`, `work_x86` were not rebuilt (oracle sources are in the repo) |
| `D:\aa-re\zone\agent_proto\`, `zone\agent_geom\` (WZ/ZW protocol notes, `sub_cgf.md`, `sub_navmesh.md`, `pakx.py`, `cgf.py`, `baiparse.py`, per-cell data) | Gone. The WZ/ZW tables are rebuilt in `zone\feasibility\`; for pak extraction use `aac\paktool` / `zone\feasibility\paktool` (wrappers over `aaemu_commons::aapak`) |
| `D:\aaemu-target\phys-bvtree\cgf_corpus\` (1,500 models with physics blobs, `AAEMU_PHYS_CGF_DIR`) | Rebuild the corpus by extracting models from the 1.2 `D:\AA1.2\game_pak` |
| `physoracle.exe` build (`AAEMU_PHYS_ORACLE`) | Build from `rust/crates/aaemu-physics/oracle` with MSVC 2022 (`build.bat`) |
| `D:\aa-re\regrade\db-auth\runtime\`, repo `ghidra-work\` | Replaced by `aac\unpack\` and `aac\ghidra\` (see Tools) |

## Porting a zone feature into the 1.2 stack

1. **Find the behaviour**: `ZONE-PHYSICS-USAGE.md` → the zone-protocol notes → search
   `x2game-dev_dedicate.strings.txt` / `.funcs.csv` for the feature's strings or RTTI names, then
   read the function in `x2game-dev_dedicate.c` (open the `.i64` in IDA for xrefs).
2. **Check the 1.2 side**: does the 1.2 client send/expect the same packets and fields? Read the
   handler in the unpacked 1.2 `x2game.dll` first (Ghidra project `proj-aa12u` — see
   Tools above; its strings include the client's own `Load*Descs` SQL and feature API names).
   1.2 opcode tables are in `rust/crates/aaemu-game/src/opcodes.rs`; packed-DLL strings in
   `D:\aa-re\client12\` are the fallback. 10.x-only mechanics should be skipped or adapted,
   not ported blindly. For client-data features, [`FEATURE-GAP-TASKS.md`](FEATURE-GAP-TASKS.md)
   is the prioritized work list and `rust/CLIENT-DB-GAP-AUDIT.md` the evidence behind it.
3. **Implement** in the owning `aaemu-game` subsystem (see `CONTRIBUTING-PORT.md`), using the
   physics query service / `aaemu-physics` World API rather than new geometry code. Cite the 10.x
   address in a comment, as the existing code does.
4. **Validate**:
   - engine-level behaviour → an `aaemu-physics` oracle scenario (bit-exact against the real DLL);
   - game behaviour → unit/flow tests, then the real 1.2 client
     (`Scripts/live-stack.sh start` + `client`, drive it with screenshots or the Lua driver, and
     confirm each step in `D:\aaemu-live\logs\rs-game.log`).
5. **Document** deviations and mirrored original bugs in `PORTING.md` (fidelity policy: match the
   original, including its bugs, unless the 1.2 client disagrees).

## Rules

- Never use the published CRYENGINE source code (license incompatible with this GPL project) —
  derive everything from the binaries.
- Reference folders are read-only; RE outputs go to `D:\aa-re\`; C: is nearly full.
- The 10.x `game_pak` is 68 GB: never copy it. If a tool needs it next to an exe, don't hard-link
  it either — a write through the link would modify the original.
