//! `aaemu-testclient` — headless 1.2 client bot.
//!
//! ```text
//! aaemu-testclient run    [--config F] [--login H:P] [--user U] [--password P] [--character C]
//!                         [--scenario login|lobby|full|combat|npc-return|mate-revive|questitem|doodad|plant|stonethrow|ghost|load]
//!                         [--out capture.jsonl]
//!                         [--npc-return-distance M] [--npc-return-away-secs S]
//!                         [--revive-method repair|skill|none]   (mate-revive; GM account needed)
//! aaemu-testclient parity [--config F] --a H:P --b H:P [--a-label cs] [--b-label rs]
//!                         [--user U] [--scenario full] [--masks masks.json] [--out-dir D]
//!                         [--rounds N]
//! aaemu-testclient diff   --a capA.jsonl --b capB.jsonl [--masks masks.json]
//! aaemu-testclient load   [--config F] [--login H:P] --bots N [--prefix load] [--ramp-ms 20]
//!                         [--scenario load]
//! aaemu-testclient quest  [--config F] [--login H:P] --plan plan.json
//!                         (--quest ID...|--all-ok) [--with-prereqs] [--new-character]
//!                         [--race name|id] [--gender m|f] [--compact-db path]
//!                         [--out results.jsonl] [--step-timeout-secs N] [--summary]
//! aaemu-testclient craft  [--config F] [--login H:P] (--craft ID...|--sample N|--all)
//!                         [--limit N] [--new-character] [--race name|id] [--gender m|f]
//!                         [--compact-db path] [--out results.jsonl] [--step-timeout-secs N]
//!                         [--keep-products] [--summary]
//! ```
//!
//! Settings come from `--config` (JSON, see `testclient.example.json`), then the flags. Keep
//! real test credentials in a git-ignored `testclient.local.json`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use aaemu_testclient::craftrun;
use aaemu_testclient::diff::{self, DiffConfig};
use aaemu_testclient::load;
use aaemu_testclient::questrun::{self, QuestOutcome};
use aaemu_testclient::record::{self, Recorder};
use aaemu_testclient::scenario::{self, BotConfig, Kind, Outcome};
use anyhow::{bail, Context};
use tracing_subscriber::EnvFilter;

struct Args {
    cmd: String,
    /// Every occurrence of a flag, in order (`--quest` is repeatable).
    flags: HashMap<String, Vec<String>>,
}

impl Args {
    fn parse() -> anyhow::Result<Self> {
        let argv: Vec<String> = std::env::args().skip(1).collect();
        let cmd = argv.first().cloned().unwrap_or_else(|| "help".into());
        let mut flags: HashMap<String, Vec<String>> = HashMap::new();
        let mut i = 1;
        while i < argv.len() {
            let a = &argv[i];
            let Some(k) = a.strip_prefix("--") else {
                bail!("unexpected argument '{a}'");
            };
            i += 1;
            // A flag followed by another flag (or the end) is a boolean switch (`--all-ok`).
            let v = if i < argv.len() && !argv[i].starts_with("--") {
                let v = argv[i].clone();
                i += 1;
                v
            } else {
                String::new()
            };
            flags.entry(k.to_string()).or_default().push(v);
        }
        Ok(Self { cmd, flags })
    }
    fn get(&self, k: &str) -> Option<&str> {
        self.flags
            .get(k)
            .and_then(|v| v.first())
            .map(String::as_str)
    }
    fn get_all(&self, k: &str) -> Vec<&str> {
        self.flags
            .get(k)
            .map_or(Vec::new(), |v| v.iter().map(String::as_str).collect())
    }
    fn has(&self, k: &str) -> bool {
        self.flags.contains_key(k)
    }
}

fn bot_config(args: &Args) -> anyhow::Result<BotConfig> {
    let mut v: serde_json::Value = match args.get("config") {
        Some(p) => serde_json::from_str(
            &std::fs::read_to_string(p).with_context(|| format!("reading {p}"))?,
        )?,
        None => serde_json::json!({}),
    };
    let obj = v.as_object_mut().context("config must be a JSON object")?;
    for (flag, key) in [
        ("login", "login"),
        ("user", "user"),
        ("password", "password"),
        ("character", "character"),
    ] {
        if let Some(x) = args.get(flag) {
            obj.insert(key.into(), x.into());
        }
    }
    for (flag, key) in [
        ("skill", "skill_id"),
        ("quest", "quest_id"),
        ("gs", "game_server_id"),
    ] {
        if let Some(x) = args.get(flag) {
            obj.insert(key.into(), x.parse::<u64>()?.into());
        }
    }
    if let Some(x) = args.get("npc-return-distance") {
        obj.insert("npc_return_distance".into(), x.parse::<f64>()?.into());
    }
    if let Some(x) = args.get("npc-return-away-secs") {
        obj.insert("npc_return_away_secs".into(), x.parse::<u64>()?.into());
    }
    if let Some(x) = args.get("revive-method") {
        obj.insert(
            "revive_method".into(),
            x.parse::<scenario::ReviveMethod>()
                .map(|_| x.to_string())?
                .into(),
        );
    }
    obj.entry("login").or_insert("127.0.0.1:1237".into());
    obj.entry("password").or_insert("testpass".into());
    if !obj.contains_key("character") {
        let user = obj
            .get("user")
            .and_then(|u| u.as_str())
            .unwrap_or("bot")
            .to_string();
        obj.insert("character".into(), user.into());
    }
    Ok(serde_json::from_value(v)?)
}

fn print_outcome(label: &str, o: &Outcome) {
    for s in &o.steps {
        println!(
            "[{label}] {:<5} {:<20} {:>6} ms  {}",
            if s.ok { "ok" } else { "FAIL" },
            s.name,
            s.ms,
            s.note
        );
    }
}

fn vars_path(capture: &Path) -> PathBuf {
    capture.with_extension("vars.json")
}

async fn run_one(
    kind: Kind,
    cfg: &BotConfig,
    out: Option<&Path>,
    label: &str,
) -> anyhow::Result<Outcome> {
    let rec = Recorder::new(true);
    let o = scenario::run(kind, cfg, &rec).await;
    print_outcome(label, &o);
    if let Some(p) = out {
        rec.save(p)?;
        std::fs::write(vars_path(p), serde_json::to_string_pretty(&o.vars)?)?;
        println!(
            "[{label}] {} frames captured to {}",
            rec.events().len(),
            p.display()
        );
    }
    Ok(o)
}

fn load_masks(args: &Args) -> anyhow::Result<DiffConfig> {
    Ok(match args.get("masks") {
        Some(p) => serde_json::from_str(&std::fs::read_to_string(p)?)?,
        None => DiffConfig::default(),
    })
}

fn read_vars(capture: &Path) -> BTreeMap<String, u64> {
    std::fs::read_to_string(vars_path(capture))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn diff_files(a: &Path, b: &Path, cfg: &DiffConfig, la: &str, lb: &str) -> anyhow::Result<usize> {
    let ea = record::load(a)?;
    let eb = record::load(b)?;
    let r = diff::diff(&ea, &read_vars(a), &eb, &read_vars(b), cfg);
    println!("{}", r.render(la, lb));
    Ok(r.findings.len())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .init();
    let args = Args::parse()?;
    let kind: Kind = args.get("scenario").unwrap_or("full").parse()?;
    match args.cmd.as_str() {
        "run" => {
            let cfg = bot_config(&args)?;
            let out = args.get("out").map(PathBuf::from);
            let o = run_one(kind, &cfg, out.as_deref(), "run").await?;
            if !o.ok() {
                bail!("scenario failed: {}", o.error.unwrap_or_default());
            }
        }
        "parity" => {
            let base = bot_config(&args)?;
            let dir = PathBuf::from(args.get("out-dir").unwrap_or("."));
            std::fs::create_dir_all(&dir)?;
            let la = args.get("a-label").unwrap_or("cs").to_string();
            let lb = args.get("b-label").unwrap_or("rs").to_string();
            let scen = args.get("scenario").unwrap_or("full");
            // `--rounds 2` runs the scenario again with the same account: the second round
            // covers the "existing character, fresh session" path.
            let rounds: usize = args.get("rounds").unwrap_or("1").parse()?;
            let masks = load_masks(&args)?;
            let mut total = 0;
            for round in 1..=rounds {
                let mut paths = Vec::new();
                for (label, addr) in [(&la, args.get("a")), (&lb, args.get("b"))] {
                    let mut cfg = base.clone();
                    cfg.login = addr
                        .context("--a and --b login addresses are required")?
                        .to_string();
                    let name = if rounds > 1 {
                        format!("{scen}-r{round}-{label}.jsonl")
                    } else {
                        format!("{scen}-{label}.jsonl")
                    };
                    let p = dir.join(name);
                    let o = run_one(kind, &cfg, Some(&p), label).await?;
                    if !o.ok() {
                        println!("[{label}] scenario failed: {}", o.error.unwrap_or_default());
                    }
                    paths.push(p);
                }
                println!("--- round {round}");
                total += diff_files(&paths[0], &paths[1], &masks, &la, &lb)?;
            }
            if total > 0 {
                std::process::exit(2);
            }
        }
        "diff" => {
            let a = PathBuf::from(args.get("a").context("--a capture")?);
            let b = PathBuf::from(args.get("b").context("--b capture")?);
            let n = diff_files(&a, &b, &load_masks(&args)?, "a", "b")?;
            if n > 0 {
                std::process::exit(2);
            }
        }
        "load" => {
            let cfg = bot_config(&args)?;
            let n: usize = args.get("bots").unwrap_or("50").parse()?;
            let prefix = args.get("prefix").unwrap_or("load");
            let ramp = Duration::from_millis(args.get("ramp-ms").unwrap_or("20").parse()?);
            let kind: Kind = args.get("scenario").unwrap_or("load").parse()?;
            let r = load::run(kind, &cfg, prefix, n, ramp).await;
            println!("{}", r.render());
            if r.ok != r.bots {
                std::process::exit(2);
            }
        }
        "quest" => {
            let cfg = bot_config(&args)?;
            let plan_path =
                PathBuf::from(args.get("plan").context("--plan <plan.json> is required")?);
            let plan = questrun::load_plan(&plan_path)?;
            let requested: Vec<u32> = args
                .get_all("quest")
                .iter()
                .map(|v| v.parse::<u32>().context("--quest needs a numeric quest id"))
                .collect::<anyhow::Result<_>>()?;
            let race = match args.get("race") {
                Some(value) => questrun::parse_race(value)?,
                None => 1,
            };
            let gender = match args.get("gender") {
                Some(value) => questrun::parse_gender(value)?,
                None => 1,
            };
            let opts = questrun::RunOptions {
                all_ok: args.has("all-ok"),
                with_prereqs: args.has("with-prereqs"),
                new_character: args.has("new-character"),
                race,
                gender,
                step_timeout: Duration::from_secs(
                    args.get("step-timeout-secs").unwrap_or("60").parse()?,
                ),
                compact_db: args.get("compact-db").map(PathBuf::from),
                out: args.get("out").map(PathBuf::from),
            };
            let report = questrun::run(&cfg, &plan, &requested, &opts).await?;
            let completed = report
                .results
                .iter()
                .filter(|r| r.result == QuestOutcome::Completed)
                .count();
            println!(
                "[quest] {} result line(s), {completed} completed",
                report.results.len()
            );
            if args.has("summary") {
                println!("{}", questrun::render_summary(&report));
            }
            if report.aborted {
                bail!("connection lost during the quest run");
            }
        }
        "craft" => {
            let cfg = bot_config(&args)?;
            let race = match args.get("race") {
                Some(value) => questrun::parse_race(value)?,
                None => 1,
            };
            let gender = match args.get("gender") {
                Some(value) => questrun::parse_gender(value)?,
                None => 1,
            };
            let opts = craftrun::RunOptions {
                craft_ids: args
                    .get_all("craft")
                    .iter()
                    .map(|v| v.parse::<u32>().context("--craft needs a numeric craft id"))
                    .collect::<anyhow::Result<_>>()?,
                all: args.has("all"),
                sample: args
                    .get("sample")
                    .map(|v| v.parse::<usize>().context("--sample needs a number"))
                    .transpose()?,
                limit: args
                    .get("limit")
                    .map(|v| v.parse::<usize>().context("--limit needs a number"))
                    .transpose()?,
                new_character: args.has("new-character"),
                race,
                gender,
                step_timeout: Duration::from_secs(
                    args.get("step-timeout-secs").unwrap_or("90").parse()?,
                ),
                compact_db: args.get("compact-db").map(PathBuf::from),
                out: args.get("out").map(PathBuf::from),
                keep_products: args.has("keep-products"),
            };
            let report = craftrun::run(&cfg, &opts).await?;
            let crafted = report
                .results
                .iter()
                .filter(|r| r.result == craftrun::CraftOutcome::Crafted)
                .count();
            println!(
                "[craft] {} result line(s), {crafted} crafted",
                report.results.len()
            );
            if args.has("summary") {
                println!("{}", craftrun::render_summary(&report));
            }
            if report.aborted {
                bail!("connection lost during the craft run");
            }
        }
        _ => {
            println!(
                "usage: aaemu-testclient run|parity|diff|load|quest|craft [--flags]; see the crate docs"
            );
        }
    }
    Ok(())
}
