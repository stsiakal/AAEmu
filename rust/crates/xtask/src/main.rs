//! Repository automation, run as `cargo xtask <command>` from anywhere below the repository root
//! (the alias lives in the repository's `.cargo/config.toml`; without it use
//! `cargo run -p xtask -- <command>` inside `rust/`).
//!
//! * `publish` — release build plus a distributable folder (binaries, configuration, data, SQL),
//!   also used by `Scripts/publish.sh`.
//! * `dev` — local orchestration: MySQL in Docker
//!   (the `db` service of `docker-compose.yaml`), then the login server, then the game server
//!   once the login server accepts game-server registrations, all wired through environment
//!   variables.
//! * `ci` — the checks CI runs: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`, and
//!   the S->C packet coverage ratchet and its tests (`tools/opcode_coverage/`, needs python).
//! * `compact-patches` — apply `SQL/patches/compact/*.sql` to a `compact.sqlite3` copy and record
//!   each one in its `aaemu_compact_patches` table (also run by `publish` and live-stack `sync`);
//!   `--backports` (or `AAEMU_BACKPORTS=1`) also applies `SQL/patches/compact/backports/*.sql`.
//! * `overlay-pak` — build a small overlay `game_pak` from later-version paks, with optional
//!   reference closure and an "already present in the stock pak" filter.

mod overlay_pak;

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};
#[cfg(test)]
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use rusqlite::OptionalExtension;

const USAGE: &str = "\
Usage: cargo xtask <command> [options]

Commands:
  publish   Build release binaries and lay out a distributable server folder
              --out <dir>        output directory (default: <repo>/publish)
              --target <triple>  cross-compile for this target (default: host)
              --profile <name>   cargo profile (default: release)
              --name <name>      folder name (default: aaemu-<version>-<target>)
              --no-compact-db    do not copy Data/*.sqlite3 into the game folder
              --backports        also apply SQL/patches/compact/backports/*.sql (or AAEMU_BACKPORTS=1)
              --archive          also pack the folder (.zip on Windows, .tar.gz elsewhere)
  dev       Start MySQL (docker compose service `db`), the login server and the game server
              --release          run release builds
              --no-db            do not start MySQL; use --db-host/--db-port or Config.Local.json
              --db-host <host>   MySQL host passed to the servers (default: 127.0.0.1)
              --db-port <port>   MySQL port (default: DB_PORT from .env, else 3306)
  ci        cargo fmt --check, clippy -D warnings, test, S->C coverage ratchet + tests (the CI checks)
  compact-patches [<db>]  Apply SQL/patches/compact/*.sql in name order, recording each file
              --patches <dir>    patch directory (default: <repo>/SQL/patches/compact)
              --backports        also apply <dir>/backports/*.sql after them (or AAEMU_BACKPORTS=1)
              (default db: rust/crates/aaemu-game/app/Data/compact.sqlite3)
  overlay-pak Build an overlay game_pak from later-version sources
              --source <pak>     source pak, repeatable, first one wins
              --list <file>      pak paths to include (one per line, # comments)
              --out <pak>        output overlay pak (TypeA/WIBO)
              --report <file>    optional JSON report
              --closure          follow .cdf/.chr/.cgf/.skin/.mtl references
              --closure-anim     also add .caf/.dba from .cal/.chrparams lists
              --exclude-present-in <pak>  skip files identical in this pak (repeatable)
              --exclude-path <text>      skip paths containing this text (repeatable)
              --new-only         skip every file present in an exclude pak, changed or not
              --repair-missing-textures  rewrite missing .mtl texture refs to a same-suffix match
              --unresolved-list <file>   write the roots whose texture refs could not be repaired
";

/// The login server's internal (game-server registration) port in the shipped `Config.json`.
const LOGIN_INTERNAL_PORT: u16 = 1234;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("publish") => publish(&args[1..]),
        Some("dev") => dev(&args[1..]),
        Some("ci") => ci(),
        Some("compact-patches") => compact_patches(&args[1..]),
        Some("overlay-pak") => overlay_pak::run(&args[1..]),
        Some("help" | "-h" | "--help") | None => {
            print!("{USAGE}");
            Ok(())
        }
        Some(other) => Err(anyhow::anyhow!("unknown command `{other}`\n\n{USAGE}")),
    };
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

/// The repository root (`rust/crates/xtask/../../..`).
fn repo_root() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    std::path::absolute(&root).unwrap_or(root)
}

/// The cargo workspace (`<repo>/rust`).
fn workspace_dir() -> PathBuf {
    repo_root().join("rust")
}

fn cargo() -> Command {
    let mut cmd = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cmd.current_dir(workspace_dir());
    cmd
}

fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("failed to start {cmd:?}"))?;
    if !status.success() {
        bail!("{cmd:?} failed ({status})");
    }
    Ok(())
}

/// Simple `--flag` / `--key value` option parser.
struct Opts {
    flags: Vec<String>,
    values: HashMap<String, String>,
}

impl Opts {
    fn parse(args: &[String], with_value: &[&str], flags: &[&str]) -> Result<Self> {
        let mut o = Opts {
            flags: Vec::new(),
            values: HashMap::new(),
        };
        let mut it = args.iter();
        while let Some(a) = it.next() {
            if with_value.contains(&a.as_str()) {
                let v = it.next().with_context(|| format!("{a} needs a value"))?;
                o.values.insert(a.clone(), v.clone());
            } else if flags.contains(&a.as_str()) {
                o.flags.push(a.clone());
            } else {
                bail!("unknown option `{a}`\n\n{USAGE}");
            }
        }
        Ok(o)
    }

    fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|f| f == name)
    }

    fn value(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }
}

// ---------------------------------------------------------------------------------------------
// publish
// ---------------------------------------------------------------------------------------------

/// Packages built by `publish` and the folder their binaries go to.
const PUBLISH_PACKAGES: &[(&str, &str)] = &[
    ("aaemu-login", "AAEmu.Login"),
    ("aaemu-game", "AAEmu.Game"),
    ("aaemu-tool-worldconverter", "Tools"),
    ("aaemu-tool-updatesfortransform", "Tools"),
    ("aaemu-testclient", "Tools"),
    ("aaemu-launcher", "Tools"),
];

fn host_triple() -> Result<String> {
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let out = Command::new(rustc)
        .arg("-vV")
        .output()
        .context("running rustc -vV")?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("host: ").map(str::to_string))
        .context("rustc -vV printed no host triple")
}

/// Builds the packages and returns `(package name, executable path)` for every binary.
fn build_binaries(profile: &str, target: Option<&str>) -> Result<Vec<(String, PathBuf)>> {
    let mut cmd = cargo();
    cmd.args(["build", "--bins", "--profile", profile])
        .arg("--message-format=json-render-diagnostics");
    for (pkg, _) in PUBLISH_PACKAGES {
        cmd.args(["-p", pkg]);
    }
    if let Some(t) = target {
        cmd.args(["--target", t]);
    }
    cmd.stdout(Stdio::piped());
    let mut child = cmd.spawn().context("starting cargo build")?;
    let mut bins = Vec::new();
    for line in BufReader::new(child.stdout.take().expect("piped stdout")).lines() {
        let line = line?;
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if msg["reason"] != "compiler-artifact" {
            continue;
        }
        let Some(exe) = msg["executable"].as_str() else {
            continue;
        };
        let pkg = msg["package_id"].as_str().unwrap_or_default();
        if let Some((name, _)) = PUBLISH_PACKAGES
            .iter()
            .find(|(name, _)| package_id_names(pkg, name))
        {
            bins.push((name.to_string(), PathBuf::from(exe)));
        }
    }
    let status = child.wait()?;
    if !status.success() {
        bail!("cargo build failed ({status})");
    }
    Ok(bins)
}

/// Whether a cargo package id (`path+file:///…/aaemu-game#0.1.0` or the older
/// `aaemu-game 0.1.0 (path+file:///…)`) names `name`.
fn package_id_names(id: &str, name: &str) -> bool {
    if id.starts_with(&format!("{name} ")) {
        return true;
    }
    let before_version = id.rsplit_once('#').map_or(id, |(p, _)| p);
    before_version
        .rsplit(['/', '\\'])
        .next()
        .is_some_and(|last| last == name || last.starts_with(&format!("{name}@")))
}

/// Copies `src` into `dst` recursively; `skip` gets the path relative to `src`.
fn copy_tree(src: &Path, dst: &Path, skip: &dyn Fn(&Path) -> bool) -> Result<()> {
    fn walk(root: &Path, dir: &Path, dst: &Path, skip: &dyn Fn(&Path) -> bool) -> Result<()> {
        for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let entry = entry?;
            let path = entry.path();
            let rel = path.strip_prefix(root).expect("below root");
            if skip(rel) {
                continue;
            }
            let target = dst.join(rel);
            if entry.file_type()?.is_dir() {
                std::fs::create_dir_all(&target)?;
                walk(root, &path, dst, skip)?;
            } else {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(&path, &target).with_context(|| {
                    format!("copying {} to {}", path.display(), target.display())
                })?;
            }
        }
        Ok(())
    }
    std::fs::create_dir_all(dst)?;
    walk(src, src, dst, skip)
}

/// Files of an `app/` folder that are never published: local overrides (may hold
/// credentials) and the client files (`game_pak` is several GB; `ClientData/readme.txt` stays).
fn skip_app_file(rel: &Path, skip_compact_db: bool) -> bool {
    let name = rel
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let mut parts = rel.components().map(|c| c.as_os_str().to_string_lossy());
    let top = parts.next().unwrap_or_default();
    if top == "ClientData" && rel.components().count() > 1 {
        return name != "readme.txt";
    }
    name == "config.local.json" || (skip_compact_db && name.ends_with(".sqlite3"))
}

fn publish(args: &[String]) -> Result<()> {
    let o = Opts::parse(
        args,
        &["--out", "--target", "--profile", "--name"],
        &["--no-compact-db", "--backports", "--archive"],
    )?;
    let root = repo_root();
    let profile = o.value("--profile").unwrap_or("release");
    let target = o.value("--target");
    let triple = match target {
        Some(t) => t.to_string(),
        None => host_triple()?,
    };
    let out = o
        .value("--out")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("publish"));
    let name = o.value("--name").map_or_else(
        || format!("aaemu-{}-{triple}", env!("CARGO_PKG_VERSION")),
        str::to_string,
    );
    let dist = out.join(&name);

    println!("Building ({profile}, {triple}) ...");
    let bins = build_binaries(profile, target)?;
    for (pkg, _) in PUBLISH_PACKAGES {
        if !bins.iter().any(|(p, _)| p == pkg) {
            bail!("cargo build produced no binary for {pkg}");
        }
    }

    if dist.exists() {
        std::fs::remove_dir_all(&dist)
            .with_context(|| format!("removing the old {}", dist.display()))?;
    }
    println!("Laying out {} ...", dist.display());
    for (pkg, exe) in &bins {
        let folder = PUBLISH_PACKAGES
            .iter()
            .find(|(p, _)| p == pkg)
            .map(|(_, f)| *f)
            .expect("known package");
        let dir = dist.join(folder);
        std::fs::create_dir_all(&dir)?;
        let file = exe.file_name().expect("executable file name");
        std::fs::copy(exe, dir.join(file)).with_context(|| format!("copying {}", exe.display()))?;
    }
    let skip_db = o.flag("--no-compact-db");
    let crates = workspace_dir().join("crates");
    copy_tree(
        &crates.join("aaemu-login/app"),
        &dist.join("AAEmu.Login"),
        &|rel| skip_app_file(rel, skip_db),
    )?;
    copy_tree(
        &crates.join("aaemu-game/app"),
        &dist.join("AAEmu.Game"),
        &|rel| skip_app_file(rel, skip_db),
    )?;
    // The servers search `SQL/updates` upwards from their executable's directory.
    copy_tree(&root.join("SQL"), &dist.join("SQL"), &|_| false)?;
    for f in ["README.md", "LICENSE.GPL", "LICENSE.MIT"] {
        if root.join(f).is_file() {
            std::fs::copy(root.join(f), dist.join(f))?;
        }
    }
    let compact = dist.join("AAEmu.Game/Data/compact.sqlite3");
    if !skip_db && compact.is_file() {
        let backports = o.flag("--backports") || env_flag("AAEMU_BACKPORTS");
        let outcome =
            apply_compact_patches(&compact, &root.join("SQL/patches/compact"), backports)?;
        report_compact_patches(&outcome, &format!("{}", compact.display()))?;
    } else if !compact.is_file() {
        println!(
            "note: no compact.sqlite3 was published; copy it to {} before starting the game server",
            dist.join("AAEmu.Game/Data").display()
        );
    }

    if o.flag("--archive") {
        let archive = if cfg!(windows) {
            out.join(format!("{name}.zip"))
        } else {
            out.join(format!("{name}.tar.gz"))
        };
        let _ = std::fs::remove_file(&archive);
        let mut tar = Command::new("tar");
        if cfg!(windows) {
            tar.arg("-a").arg("-cf");
        } else {
            tar.arg("-czf");
        }
        tar.arg(&archive).arg("-C").arg(&out).arg(&name);
        run(&mut tar)?;
        println!("Archive: {}", archive.display());
    }
    println!("Published to {}", dist.display());
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// compact-patches
// ---------------------------------------------------------------------------------------------

/// Table recording the compact patches applied to a database.
const COMPACT_PATCH_TABLE: &str = "aaemu_compact_patches";

/// What happened to one patch file during a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatchStatus {
    Applied,
    AlreadyRecorded,
    Changed,
}

/// Result of one applier run, in file-name order.
#[derive(Debug, Default)]
struct PatchOutcome {
    statuses: Vec<(String, PatchStatus)>,
}

impl PatchOutcome {
    fn names(&self, status: PatchStatus) -> impl Iterator<Item = &str> {
        self.statuses
            .iter()
            .filter(move |(_, s)| *s == status)
            .map(|(name, _)| name.as_str())
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

/// Removes transaction-control statements so every file runs in the applier's own transaction
/// (SQLite does not nest transactions). Only whole lines are matched, so comments and strings
/// are left alone.
fn strip_transaction_control(sql: &str) -> String {
    sql.lines()
        .filter(|line| {
            let statement = line.trim().trim_end_matches(';').trim();
            !statement.eq_ignore_ascii_case("begin")
                && !statement.eq_ignore_ascii_case("begin transaction")
                && !statement.eq_ignore_ascii_case("commit")
                && !statement.eq_ignore_ascii_case("commit transaction")
                && !statement.eq_ignore_ascii_case("end")
                && !statement.eq_ignore_ascii_case("end transaction")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The subdirectory of the patch directory that holds the opt-in 3.0 backports.
const BACKPORTS_DIR: &str = "backports";

/// Whether a `--backports`-style switch value means enabled (`1`, `true`, `yes` or `on`).
fn flag_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// `--backports`-style environment switch (`AAEMU_BACKPORTS=1`).
fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .is_some_and(|value| flag_value(&value))
}

/// The `*.sql` files of `dir` in file-name order, with `prefix` prepended to the recorded name.
fn patch_files(dir: &Path, prefix: &str) -> Result<Vec<(String, PathBuf)>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("reading patch directory {}", dir.display()))?
    {
        let path = entry?.path();
        if path.is_file()
            && path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("sql"))
        {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            files.push((format!("{prefix}{name}"), path));
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

/// Applies every patch in `dir` that is not recorded in the database yet and records it. The
/// opt-in 3.0 backports live in `dir/backports` and only run (after the regular patches) when
/// `backports` is set; they are recorded with their `backports/` prefix, so enabling them later
/// applies each one once.
///
/// A recorded patch whose file content changed is reported as [`PatchStatus::Changed`] and is
/// never re-run: compact patches are immutable, so a fix must arrive as a new dated file.
fn apply_compact_patches(db: &Path, dir: &Path, backports: bool) -> Result<PatchOutcome> {
    let mut conn =
        rusqlite::Connection::open(db).with_context(|| format!("opening {}", db.display()))?;
    conn.busy_timeout(Duration::from_secs(30))?;
    // The npc_groups/npc_group_members patches rebuild tables; keep FK checks out of the way.
    conn.execute_batch("PRAGMA foreign_keys = OFF;")?;
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS {COMPACT_PATCH_TABLE} (\
         name TEXT PRIMARY KEY, sha256 TEXT NOT NULL, applied_at TEXT NOT NULL);"
    ))?;

    let mut files = patch_files(dir, "")?;
    if backports {
        files.extend(patch_files(
            &dir.join(BACKPORTS_DIR),
            &format!("{BACKPORTS_DIR}/"),
        )?);
    }

    let mut outcome = PatchOutcome::default();
    for (name, path) in files {
        let sql = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let hash = sha256_hex(sql.as_bytes());
        let recorded: Option<String> = conn
            .query_row(
                &format!("SELECT sha256 FROM {COMPACT_PATCH_TABLE} WHERE name = ?1"),
                [&name],
                |row| row.get(0),
            )
            .optional()?;
        let status = match recorded {
            Some(recorded) if recorded == hash => PatchStatus::AlreadyRecorded,
            Some(_) => PatchStatus::Changed,
            None => {
                let tx = conn.transaction()?;
                tx.execute_batch(&strip_transaction_control(&sql))
                    .with_context(|| format!("applying {name}"))?;
                tx.execute(
                    &format!(
                        "INSERT INTO {COMPACT_PATCH_TABLE} (name, sha256, applied_at) \
                         VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))"
                    ),
                    rusqlite::params![name, hash],
                )?;
                tx.commit()?;
                PatchStatus::Applied
            }
        };
        outcome.statuses.push((name, status));
    }
    Ok(outcome)
}

/// Prints a run's outcome and fails when a recorded patch file changed.
fn report_compact_patches(outcome: &PatchOutcome, db: &str) -> Result<()> {
    for name in outcome.names(PatchStatus::Applied) {
        println!("compact patches: applied {name}");
    }
    for name in outcome.names(PatchStatus::AlreadyRecorded) {
        println!("compact patches: already recorded {name}");
    }
    for name in outcome.names(PatchStatus::Changed) {
        eprintln!("warning: compact patches: {name} changed after it was recorded; not re-run");
    }
    let changed = outcome.names(PatchStatus::Changed).count();
    if changed > 0 {
        bail!(
            "{db}: {changed} compact patch file(s) changed after being applied; patches are \
             immutable - revert the file or add a new dated patch"
        );
    }
    Ok(())
}

fn compact_patches(args: &[String]) -> Result<()> {
    let mut db: Option<PathBuf> = None;
    let mut dir: Option<PathBuf> = None;
    let mut backports = env_flag("AAEMU_BACKPORTS");
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--patches" => {
                let value = it.next().context("--patches needs a value")?;
                dir = Some(PathBuf::from(value));
            }
            "--backports" => backports = true,
            other if other.starts_with("--") => bail!("unknown option `{other}`\n\n{USAGE}"),
            other if db.is_none() => db = Some(PathBuf::from(other)),
            other => bail!("unexpected argument `{other}`\n\n{USAGE}"),
        }
    }
    let db =
        db.unwrap_or_else(|| workspace_dir().join("crates/aaemu-game/app/Data/compact.sqlite3"));
    let dir = dir.unwrap_or_else(|| repo_root().join("SQL/patches/compact"));
    if !db.is_file() {
        bail!("compact database not found: {}", db.display());
    }
    let outcome = apply_compact_patches(&db, &dir, backports)?;
    report_compact_patches(&outcome, &db.display().to_string())
}

// ---------------------------------------------------------------------------------------------
// dev
// ---------------------------------------------------------------------------------------------

/// `KEY=VALUE` lines of a `.env` file (comments and blank lines ignored).
fn read_env_file(path: &Path) -> Result<HashMap<String, String>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(parse_env(&text))
}

fn parse_env(text: &str) -> HashMap<String, String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| {
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or(v);
            (k.trim().to_string(), v.to_string())
        })
        .collect()
}

fn wait_for_port(addr: SocketAddr, timeout: Duration, child: &mut Child, what: &str) -> Result<()> {
    let start = Instant::now();
    loop {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok() {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            bail!("{what} exited before it was ready ({status})");
        }
        if start.elapsed() > timeout {
            bail!("{what} did not open {addr} within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn dev(args: &[String]) -> Result<()> {
    let o = Opts::parse(args, &["--db-host", "--db-port"], &["--release", "--no-db"])?;
    let root = repo_root();

    // .env (the docker compose settings, incl. the MySQL root password)
    let env_path = root.join(".env");
    if !env_path.exists() {
        std::fs::copy(root.join(".env.example"), &env_path)
            .context("creating .env from .env.example")?;
        println!("Created .env from .env.example");
    }
    let env = read_env_file(&env_path)?;

    let db_host = o.value("--db-host").unwrap_or("127.0.0.1").to_string();
    let db_port = o
        .value("--db-port")
        .map(str::to_string)
        .or_else(|| env.get("DB_PORT").cloned())
        .unwrap_or_else(|| "3306".into());
    let db_user = env.get("DB_USER").cloned().unwrap_or_else(|| "root".into());
    let db_password = env.get("DB_PASSWORD").cloned().unwrap_or_default();

    if !o.flag("--no-db") {
        println!("Starting MySQL (docker compose service `db`) ...");
        run(Command::new("docker")
            .args(["compose", "up", "-d", "--wait", "db"])
            .current_dir(&root))
        .context("starting MySQL; is Docker running? (use --no-db to use your own MySQL)")?;
    }

    let release = o.flag("--release");
    let profile_args: &[&str] = if release { &["--release"] } else { &[] };
    println!("Building aaemu-login and aaemu-game ...");
    run(cargo()
        .args(["build", "-p", "aaemu-login", "-p", "aaemu-game"])
        .args(profile_args))?;

    let mut db_env: Vec<(String, String)> = Vec::new();
    if !o.flag("--no-db") || o.value("--db-host").is_some() || o.value("--db-port").is_some() {
        db_env.extend([
            ("Connections__MySQLProvider__Host".into(), db_host),
            ("Connections__MySQLProvider__Port".into(), db_port),
        ]);
    }
    if !o.flag("--no-db") {
        db_env.extend([
            ("Connections__MySQLProvider__User".into(), db_user),
            ("Connections__MySQLProvider__Password".into(), db_password),
        ]);
    }
    db_env.push(("Connections__AutoApplyUpdates".into(), "true".into()));

    let server = |pkg: &str, extra: &[(&str, String)]| -> Result<Child> {
        let mut cmd = cargo();
        cmd.args(["run", "-q", "-p", pkg]).args(profile_args);
        for (k, v) in &db_env {
            cmd.env(k, v);
        }
        for (k, v) in extra {
            cmd.env(k, v);
        }
        cmd.spawn().with_context(|| format!("starting {pkg}"))
    };

    println!("Starting the login server ...");
    let mut login = server(
        "aaemu-login",
        &[
            (
                "Connections__MySQLProvider__Database",
                "aaemu_login".to_string(),
            ),
            ("GameServers__0__Id", "1".into()),
            ("GameServers__0__Name", "AAEmu.Game".into()),
            ("GameServers__0__Host", "127.0.0.1".into()),
            ("GameServers__0__Port", "1239".into()),
            // local development convenience; the shipped default is off
            ("AutoAccount", "true".into()),
        ],
    )?;
    let internal = SocketAddr::from(([127, 0, 0, 1], LOGIN_INTERNAL_PORT));
    if let Err(e) = wait_for_port(
        internal,
        Duration::from_secs(300),
        &mut login,
        "aaemu-login",
    ) {
        let _ = login.kill();
        return Err(e);
    }

    println!("Starting the game server ...");
    let game = match server(
        "aaemu-game",
        &[
            ("Connections__MySQLProvider__Database", "aaemu_game".into()),
            ("LoginNetwork__Host", "127.0.0.1".into()),
            ("LoginNetwork__Port", LOGIN_INTERNAL_PORT.to_string()),
        ],
    ) {
        Ok(g) => g,
        Err(e) => {
            let _ = login.kill();
            return Err(e);
        }
    };
    println!("Login: 127.0.0.1:1237, game: 127.0.0.1:1239. Press Ctrl+C to stop both servers.");

    // Ctrl+C reaches the servers directly (same console / process group) and they shut down
    // gracefully; wait for both.
    let mut running: Vec<(&str, Child)> = vec![("aaemu-login", login), ("aaemu-game", game)];
    while !running.is_empty() {
        let mut i = 0;
        while i < running.len() {
            if let Some(status) = running[i].1.try_wait()? {
                println!("{} exited ({status})", running[i].0);
                running.remove(i);
            } else {
                i += 1;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// ci
// ---------------------------------------------------------------------------------------------

fn ci() -> Result<()> {
    run(cargo().args(["fmt", "--all", "--check"]))?;
    run(cargo().args([
        "clippy",
        "--workspace",
        "--all-targets",
        "--",
        "-D",
        "warnings",
    ]))?;
    run(cargo().args(["test", "--workspace"]))?;
    sc_ratchet()
}

/// The S->C packet coverage ratchet (`rust/tools/opcode_coverage/sc_usage.py --check`): the lists
/// of opcodes without a Rust type and of packets never sent may only shrink. Needs only python and
/// the checked-in sources. Runs `test_sc_usage` with the same interpreter after the ratchet passes.
/// A missing python interpreter is an error; set `XTASK_SKIP_RATCHET=1` to skip both explicitly
/// with a warning (refused when `CI` is set).
fn sc_ratchet() -> Result<()> {
    sc_ratchet_with(
        std::env::var_os("XTASK_SKIP_RATCHET").is_some_and(|v| v == "1"),
        std::env::var_os("CI").is_some(),
        Command::status,
    )
}

fn sc_ratchet_with(
    skip_requested: bool,
    in_ci: bool,
    mut execute: impl FnMut(&mut Command) -> std::io::Result<ExitStatus>,
) -> Result<()> {
    let dir = workspace_dir().join("tools/opcode_coverage");
    let script = dir.join("sc_usage.py");
    for python in ["python3", "python"] {
        let mut cmd = Command::new(python);
        cmd.arg(&script).arg("--check").current_dir(&dir);
        match execute(&mut cmd) {
            Ok(status) if status.success() => {
                let mut tests = Command::new(python);
                tests
                    .args(["-m", "unittest", "test_sc_usage"])
                    .current_dir(&dir);
                let status =
                    execute(&mut tests).with_context(|| format!("failed to start {tests:?}"))?;
                if !status.success() {
                    bail!("{tests:?} failed ({status})");
                }
                return Ok(());
            }
            // Windows' App Execution Alias stub for a missing python exits with 9009.
            Ok(status) if cfg!(windows) && status.code() == Some(9009) => continue,
            Ok(status) => bail!("{cmd:?} failed ({status})"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("failed to start {cmd:?}")),
        }
    }
    skip_missing_python(skip_requested, in_ci)
}

/// No python interpreter was found: fail, unless the skip was requested explicitly and this is
/// not a CI run.
fn skip_missing_python(skip_requested: bool, in_ci: bool) -> Result<()> {
    if skip_requested && !in_ci {
        eprintln!(
            "warning: python not found; skipping the S->C coverage ratchet and its tests \
             (XTASK_SKIP_RATCHET=1)"
        );
        return Ok(());
    }
    bail!(concat!(
        "python (python3 or python) not found: the S->C coverage ratchet and its tests ",
        "(tools/opcode_coverage/sc_usage.py --check) cannot run. Install python, or set ",
        "XTASK_SKIP_RATCHET=1 to skip it explicitly (not allowed when CI is set)"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    fn exit_status(code: u32) -> ExitStatus {
        #[cfg(windows)]
        {
            use std::os::windows::process::ExitStatusExt;
            ExitStatus::from_raw(code)
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            ExitStatus::from_raw((code << 8) as i32)
        }
    }

    fn fake_sc_ratchet(
        skip_requested: bool,
        in_ci: bool,
        outcomes: Vec<std::io::Result<ExitStatus>>,
        programs: &[&str],
    ) -> Result<()> {
        assert_eq!(outcomes.len(), programs.len());
        let mut steps = programs.iter().zip(outcomes);
        let result = sc_ratchet_with(skip_requested, in_ci, |cmd| {
            let (program, outcome) = steps.next().expect("unexpected Python command");
            assert_eq!(cmd.get_program(), *program);
            outcome
        });
        assert!(
            steps.next().is_none(),
            "expected Python command did not run"
        );
        result
    }

    #[test]
    fn ratchet_runs_unittests_with_same_python_and_correct_paths() {
        let dir = workspace_dir().join("tools/opcode_coverage");
        let script = dir.join("sc_usage.py");
        let mut calls = 0;
        // The skip only applies when no interpreter is found.
        sc_ratchet_with(true, false, |cmd| {
            assert_eq!(cmd.get_program(), "python3");
            assert_eq!(cmd.get_current_dir(), Some(dir.as_path()));
            let args: Vec<_> = cmd.get_args().collect();
            match calls {
                0 => assert_eq!(args, [script.as_os_str(), "--check".as_ref()]),
                1 => assert_eq!(args, ["-m", "unittest", "test_sc_usage"]),
                _ => panic!("unexpected Python command"),
            }
            calls += 1;
            Ok(exit_status(0))
        })
        .unwrap();
        assert_eq!(calls, 2);
    }

    #[test]
    fn missing_python3_falls_back_to_python_for_ratchet_and_unittests() {
        fake_sc_ratchet(
            false,
            false,
            vec![
                Err(ErrorKind::NotFound.into()),
                Ok(exit_status(0)),
                Ok(exit_status(0)),
            ],
            &["python3", "python", "python"],
        )
        .unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_python_alias_falls_back_to_python_for_ratchet_and_unittests() {
        fake_sc_ratchet(
            false,
            false,
            vec![
                Ok(exit_status(9009)),
                Ok(exit_status(0)),
                Ok(exit_status(0)),
            ],
            &["python3", "python", "python"],
        )
        .unwrap();
    }

    #[test]
    fn ratchet_failure_does_not_fall_back_or_honor_skip() {
        let err = fake_sc_ratchet(true, false, vec![Ok(exit_status(1))], &["python3"]).unwrap_err();
        assert!(err.to_string().contains("sc_usage.py"));
        assert!(err.to_string().contains("failed"));
    }

    #[test]
    fn python_permission_denied_does_not_fall_back_or_honor_skip() {
        let err = fake_sc_ratchet(
            true,
            false,
            vec![Err(ErrorKind::PermissionDenied.into())],
            &["python3"],
        )
        .unwrap_err();
        assert!(err.to_string().contains("failed to start"));
        assert_eq!(
            err.downcast_ref::<Error>().unwrap().kind(),
            ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn no_python_skips_both_checks_only_when_explicit_and_outside_ci() {
        for skip_requested in [false, true] {
            for in_ci in [false, true] {
                let result = fake_sc_ratchet(
                    skip_requested,
                    in_ci,
                    vec![
                        Err(ErrorKind::NotFound.into()),
                        Err(ErrorKind::NotFound.into()),
                    ],
                    &["python3", "python"],
                );
                assert_eq!(result.is_ok(), skip_requested && !in_ci);
                if let Err(err) = result {
                    assert!(err.to_string().contains("not found"));
                }
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn missing_windows_aliases_obey_skip_policy() {
        for in_ci in [false, true] {
            let result = fake_sc_ratchet(
                true,
                in_ci,
                vec![Ok(exit_status(9009)), Ok(exit_status(9009))],
                &["python3", "python"],
            );
            assert_eq!(result.is_ok(), !in_ci);
        }
    }

    #[test]
    fn python_unittest_failure_propagates_without_fallback_or_skip() {
        let err = fake_sc_ratchet(
            true,
            false,
            vec![Ok(exit_status(0)), Ok(exit_status(1))],
            &["python3", "python3"],
        )
        .unwrap_err();
        assert!(err.to_string().contains("test_sc_usage"));
        assert!(err.to_string().contains("failed"));
    }

    #[test]
    fn python_unittest_start_errors_propagate_without_fallback_or_skip() {
        for kind in [ErrorKind::NotFound, ErrorKind::PermissionDenied] {
            let err = fake_sc_ratchet(
                true,
                false,
                vec![Ok(exit_status(0)), Err(kind.into())],
                &["python3", "python3"],
            )
            .unwrap_err();
            assert!(err.to_string().contains("test_sc_usage"));
            assert!(err.to_string().contains("failed to start"));
            assert_eq!(err.downcast_ref::<Error>().unwrap().kind(), kind);
        }
    }

    #[test]
    fn missing_python_fails_unless_skipped_explicitly_outside_ci() {
        assert!(skip_missing_python(false, false).is_err());
        assert!(skip_missing_python(false, true).is_err());
        assert!(skip_missing_python(true, true).is_err());
        assert!(skip_missing_python(true, false).is_ok());
    }

    #[test]
    fn package_ids() {
        assert!(package_id_names(
            "path+file:///C:/r/rust/crates/aaemu-game#0.1.0",
            "aaemu-game"
        ));
        assert!(!package_id_names(
            "path+file:///C:/r/rust/crates/aaemu-game#0.1.0",
            "aaemu-login"
        ));
        assert!(package_id_names(
            "path+file:///r/crates/xtask#xtask@0.1.0",
            "xtask"
        ));
        assert!(package_id_names(
            "aaemu-login 0.1.0 (path+file:///r/crates/aaemu-login)",
            "aaemu-login"
        ));
    }

    #[test]
    fn app_files_skipped_on_publish() {
        let p = Path::new;
        assert!(!skip_app_file(p("Config.json"), false));
        assert!(skip_app_file(p("Config.Local.json"), false));
        assert!(!skip_app_file(p("ClientData"), false));
        assert!(!skip_app_file(p("ClientData/readme.txt"), false));
        assert!(skip_app_file(p("ClientData/game_pak"), false));
        assert!(!skip_app_file(p("Data/compact.sqlite3"), false));
        assert!(skip_app_file(p("Data/compact.sqlite3"), true));
        assert!(!skip_app_file(p("Data/Worlds/world_spawns.json"), true));
    }

    #[test]
    fn env_file() {
        let env = parse_env("# c\nDB_USER=root\n\nDB_PASSWORD=\"p=w\"\n");
        assert_eq!(env["DB_USER"], "root");
        assert_eq!(env["DB_PASSWORD"], "p=w");
    }

    /// A private directory removed when the test ends (also on panic).
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let path = std::env::temp_dir()
                .join(format!("aaemu-xtask-{tag}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn setup(tag: &str) -> (TempDir, PathBuf, PathBuf) {
        let tmp = TempDir::new(tag);
        let patches = tmp.path().join("patches");
        std::fs::create_dir(&patches).expect("create patches dir");
        let db = tmp.path().join("compact.sqlite3");
        (tmp, patches, db)
    }

    fn count(conn: &rusqlite::Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |row| row.get(0)).expect("count")
    }

    #[test]
    fn transaction_control_is_stripped_but_comments_stay() {
        let sql = "BEGIN;\nUPDATE t SET a = 1; -- begin;\nCOMMIT TRANSACTION;\nEND TRANSACTION;";
        assert_eq!(
            strip_transaction_control(sql),
            "UPDATE t SET a = 1; -- begin;"
        );
    }

    #[test]
    fn compact_patches_apply_once_and_rerun_is_a_no_op() {
        let (_tmp, patches, db) = setup("compact-once");
        std::fs::write(
            patches.join("01-create.sql"),
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);\nINSERT INTO t VALUES (1, 'a');\n",
        )
        .unwrap();
        std::fs::write(
            patches.join("02-update.sql"),
            "BEGIN;\nUPDATE t SET v = 'b' WHERE id = 1 AND v = 'a';\nCOMMIT;\n",
        )
        .unwrap();

        let first = apply_compact_patches(&db, &patches, false).unwrap();
        assert_eq!(first.names(PatchStatus::Applied).count(), 2);
        assert_eq!(first.names(PatchStatus::AlreadyRecorded).count(), 0);

        let second = apply_compact_patches(&db, &patches, false).unwrap();
        assert_eq!(second.names(PatchStatus::Applied).count(), 0);
        assert_eq!(second.names(PatchStatus::AlreadyRecorded).count(), 2);
        assert!(report_compact_patches(&second, "test db").is_ok());

        let conn = rusqlite::Connection::open(&db).unwrap();
        let v: String = conn
            .query_row("SELECT v FROM t", [], |row| row.get(0))
            .unwrap();
        assert_eq!(v, "b");
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM aaemu_compact_patches"),
            2
        );
    }

    #[test]
    fn compact_patches_report_a_changed_patch_without_rerunning_it() {
        let (_tmp, patches, db) = setup("compact-changed");
        let file = patches.join("01.sql");
        std::fs::write(
            &file,
            "CREATE TABLE t (id INTEGER);\nINSERT INTO t VALUES (1);\n",
        )
        .unwrap();
        apply_compact_patches(&db, &patches, false).unwrap();

        std::fs::write(&file, "INSERT INTO t VALUES (2);\n").unwrap();
        let second = apply_compact_patches(&db, &patches, false).unwrap();
        assert_eq!(second.names(PatchStatus::Changed).count(), 1);
        assert!(report_compact_patches(&second, "test db").is_err());

        let conn = rusqlite::Connection::open(&db).unwrap();
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM t"), 1);
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM aaemu_compact_patches"),
            1
        );
    }

    #[test]
    fn compact_patches_roll_back_a_failing_file() {
        let (_tmp, patches, db) = setup("compact-rollback");
        std::fs::write(
            patches.join("01-bad.sql"),
            "CREATE TABLE u (id INTEGER);\nINSERT INTO no_such_table VALUES (1);\n",
        )
        .unwrap();
        assert!(apply_compact_patches(&db, &patches, false).is_err());

        let conn = rusqlite::Connection::open(&db).unwrap();
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'u'"
            ),
            0
        );
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM aaemu_compact_patches"),
            0
        );
    }

    #[test]
    fn flag_value_accepts_the_documented_switches() {
        for value in ["1", "true", "TRUE", "yes", "on", " 1 "] {
            assert!(flag_value(value), "{value:?} is enabled");
        }
        for value in ["", "0", "false", "no", "off"] {
            assert!(!flag_value(value), "{value:?} is disabled");
        }
    }

    #[test]
    fn compact_patches_apply_backports_only_when_opted_in_and_record_their_path() {
        let (_tmp, patches, db) = setup("compact-backports");
        let backports = patches.join(BACKPORTS_DIR);
        std::fs::create_dir(&backports).unwrap();
        std::fs::write(
            patches.join("01-base.sql"),
            "CREATE TABLE t (id INTEGER PRIMARY KEY);\nINSERT INTO t VALUES (1);\n",
        )
        .unwrap();
        std::fs::write(
            backports.join("10-level.sql"),
            "INSERT INTO t VALUES (10);\n",
        )
        .unwrap();
        std::fs::write(
            backports.join("20-gear.sql"),
            "INSERT INTO t VALUES (20);\n",
        )
        .unwrap();

        // The default run leaves the backports alone.
        let first = apply_compact_patches(&db, &patches, false).unwrap();
        assert_eq!(
            first.names(PatchStatus::Applied).collect::<Vec<_>>(),
            ["01-base.sql"]
        );
        let conn = rusqlite::Connection::open(&db).unwrap();
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM t"), 1);

        // Opting in applies them after the regular patches, in file-name order.
        let second = apply_compact_patches(&db, &patches, true).unwrap();
        assert_eq!(
            second.names(PatchStatus::Applied).collect::<Vec<_>>(),
            ["backports/10-level.sql", "backports/20-gear.sql"]
        );
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM t"), 3);
        let recorded: Vec<String> = conn
            .prepare("SELECT name FROM aaemu_compact_patches ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            recorded,
            [
                "01-base.sql",
                "backports/10-level.sql",
                "backports/20-gear.sql"
            ]
        );

        // A later run with backports still enabled applies each one only once.
        let third = apply_compact_patches(&db, &patches, true).unwrap();
        assert_eq!(third.names(PatchStatus::Applied).count(), 0);
        assert_eq!(third.names(PatchStatus::AlreadyRecorded).count(), 3);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM t"), 3);
    }
}
