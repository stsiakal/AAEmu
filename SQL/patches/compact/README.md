# compact.sqlite3 patches

These files are the **single source of truth** for intentional fixes to the read-only game data
(`compact.sqlite3`). The database itself is never committed (`.gitignore`: `*.sqlite3`); a server
builds its copy from one base database plus these patches.

Two sets live here:

- `*.sql` in this directory — 1.2 fixes and corrections, applied **always** and in file-name order.
- `backports/*.sql` — opt-in 3.0 data backports (level 51-55, 3.0 gear). They are **off by
  default** and only run after the 1.2 fixes when `--backports` (or `AAEMU_BACKPORTS=1`) is set.
  A pure 1.2 server therefore runs the same data with or without the files present.

## Rules

- **One concern per file**, named `YYYY-MM-DD-<short-description>.sql`; files run in file-name
  order (the 1.2 fixes first, then `backports/` in its own file-name order when enabled).
- **Idempotent and guarded**: running a file twice must not change the result, and every `UPDATE`
  is guarded by the original value (or an insert is guarded with `WHERE NOT EXISTS` /
  `INSERT OR IGNORE`), so an already-fixed or customized row is left alone. A whole-table rebuild
  (the `npc_groups` / `npc_group_members` imports) is allowed because it is deterministic.
- **Immutable once applied**: the applier records each file's SHA-256 in the database's
  `aaemu_compact_patches` table (`name`, `sha256`, `applied_at`). A recorded file whose content
  changed is reported and makes the run fail; it is never silently re-run. Change an applied patch
  by adding a new dated file (and, if the old one must stop running on pristine databases, by
  leaving that new file to guard/fix up the values it set).
- Each file runs in its own transaction; a failure rolls the file back and exits non-zero.
  Transaction-control statements in a file are removed by the applier, so `BEGIN;`/`COMMIT;` are
  optional and kept only for people applying a file by hand with `sqlite3`.
- The shipped game server opens the database read-only and queries specific tables; the extra
  `aaemu_compact_patches` table is ignored by the loaders.

## Automatic application

`cargo xtask compact-patches [<db>] [--patches <dir>] [--backports]` applies all `*.sql` files in
this directory to `<db>` (default `rust/crates/aaemu-game/app/Data/compact.sqlite3`), prints what
was applied and what was already recorded, and exits non-zero on any error or changed file.
`--backports` (or the environment variable `AAEMU_BACKPORTS=1`) additionally applies
`backports/*.sql`; without it the backports are not read at all. Backports are recorded in
`aaemu_compact_patches` with their `backports/` path (`backports/2026-10-20-level-55-retail-3.0.sql`),
so a later opt-in applies each one once even if the database already had the 1.2 fixes.

It runs automatically:

- in `Scripts/live-stack.sh sync`, right after the compact database is copied into the lane
  runtime directory (set `AAEMU_BACKPORTS=1` for the lane stack to include the backports);
- in `cargo xtask publish` (used by `Scripts/publish.sh`) on the database being shipped, unless
  publishing with `--no-compact-db`; pass `--backports` (or set `AAEMU_BACKPORTS=1`) to ship the
  backports.

## Enabling the 3.0 backports

Both halves must be switched on; each is off by default:

1. **Data** — apply `backports/*.sql` to the server's `compact.sqlite3`:
   `cargo xtask compact-patches <db> --backports`.
2. **Server behavior** — set `"Backports": { "Enabled": true }` in `Config.Local.json` (the
   `Backports__Enabled=true` environment override or `--Backports:Enabled=true` on the command
   line works too). This raises the player level cap from the 1.2 level 50 to 55; with it false,
   level 51-55 data is ignored and the server behaves like 1.2.

The 3.0 costume art is a client-side overlay pak (`10-gear-3.0-costumes.pak`); a server run needs
it as the first entry of `ClientData.Sources` to serve those files. See
[`Docs/wiki/Backports.md`](../../../Docs/wiki/Backports.md).

## Applying to a production server by hand

Stop the game server, back up `Data/compact.sqlite3`, then run from a repository checkout with the
same version of the patches:

```sh
cargo xtask compact-patches /path/to/AAEmu.Game/Data/compact.sqlite3
# with the opt-in 3.0 backports:
cargo xtask compact-patches /path/to/AAEmu.Game/Data/compact.sqlite3 --backports
```

Do not use a bare `sqlite3 < file.sql` on a server unless you also record the file in
`aaemu_compact_patches`; otherwise the next automatic run will apply it again (the files are
guarded, so that is safe, but the record is what keeps the source of truth verifiable).

## Current patches

### 1.2 fixes (always applied)

| File | Effect | Rerun-safe |
| --- | --- | --- |
| `2024-07-07 - Fix Quest - To the Land for Eternity (2907).sql` | `unit_reqs.value1` 581 → 1152 for sphere quest 2907 | guarded by original value |
| `2025-01-13 - Fix Quest - Closing the Gate (3889).sql` | `sphere_quests.quest_id` 4280 → 3889 for row 789 | guarded by original value |
| `2026-01-23-npc_groups.sql` | imports the retail `npc_groups` table | deterministic full rebuild |
| `2026-01-23-npc_group_members.sql` | imports the retail `npc_group_members` table | deterministic full rebuild |
| `2026-03-25-fix-schooners-slave-kind.sql` | merchant schooners (75, 80) `slave_kind_id` 2 → 10 | guarded by original value |
| `2026-10-14-fix-npc-ai-param-syntax.sql` | repairs malformed Lua in `npc_ai_params` 443, 835, 1975 | guarded by exact original text |
| `2026-10-15-halloween-gourd-respawn.sql` | gourd doodad 7422 final row respawns | guarded by original value |

### 3.0 backports (opt-in, `backports/`)

| File | Effect | Rerun-safe |
| --- | --- | --- |
| `backports/2026-10-20-level-55-retail-3.0.sql` | 3.0 level 51–55 capstones, experience curve and quest supplies | guarded by original values |
| `backports/2026-10-21-gear-3.0-batch-1a.sql` | 3.0 gear batch 1a: 288 retail costumes/armor/cloaks (≤ Eanad) + 7 craft materials, armor looks re-keyed above the 1.2 range | inserts guarded by `NOT EXISTS` on id; replacement `UPDATE`s rewrite the same values |
| `backports/2026-10-21-gear-3.0-batch-1b-costumes.sql` | 3.0 gear batch 1b-costumes: 300 retail costumes + 2 craft materials with art in the `10-gear-3.0-costumes.pak` overlay (5 material refs repaired, 13 costumes skipped); apply after batch 1a | inserts guarded by `NOT EXISTS` on id; no replacement rows |
