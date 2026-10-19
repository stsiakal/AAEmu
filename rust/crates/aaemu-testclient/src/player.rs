//! Player mode of the quest runner (`aaemu-testclient quest --player`): the bot plays like a
//! client instead of a GM.
//!
//! * **Movement** is walked: [`walk_to`] routes over the server's `.bai` navmesh
//!   ([`Terrain::route_with_source`]) and sends paced `CSMoveUnitPacket`s at the 1.2 run speed
//!   ([`crate::walk::Walker`]). Every GM `/move` that is still needed (no path, a cliff, the
//!   server not moving us, an instance world) is a **counted fallback** with its reason.
//! * **Combat** is fought: [`fight`] runs the learned class skills of [`crate::fight::Session`]
//!   against the target, loots the corpse and revives the character after a death. A mob that
//!   attacks while the bot walks is fought too ([`threat`]).
//! * **GM shortcuts** are refused (`godmode`, `level`, `kill`) or counted (everything else the
//!   old runner still uses: `item add`, `spawn`, `gold add`, ...) by [`PlayerMode::gm_command`].
//! * **Checkpoint**: [`Checkpoint`] is a per-character JSON file (character, current quest and
//!   step, blocker, fallback tally, plan hash) rewritten at every step, so `--resume` continues
//!   after a server restart; quests the server reports completed are skipped.
//! * **Report**: [`render_summary`] turns the result lines and the checkpoint into the
//!   `summary.md` of a run.
//!
//! Evidence standard: a quest the bot cannot complete is a blocker with its reason and the
//! observed packets; it is never a server bug by itself (see the project's bug-evidence rule).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::fight::{self, Approach, KillOutcome, Session};
use crate::game::{observation_ms, GameClient};
use crate::questrun::{Blocker, Fallback, QuestOutcome, QuestResult};
use crate::walk::{dist_2d, near_point, pump, send_move, Step, Terrain, Walker, WalkerConfig};

/// A hostile that hit the character this recently is answered while walking.
const THREAT_WINDOW_MS: u64 = 3_000;
/// Short hops (fight stand points, NPC approaches) skip the navmesh and walk straight.
const STRAIGHT_HOP_M: f32 = 8.0;
/// A walk that needs more than this many re-plans (deaths, fights) gives up and teleports.
const MAX_REPLANS: u32 = 12;

/// How a failed quest ends the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOn {
    /// Stop at the first quest that cannot be completed.
    Any,
    /// Stop at the first failed prologue / story quest; a failed side quest is skipped.
    Story,
}

/// The player-mode settings (`quest --player ...`).
#[derive(Debug, Clone)]
pub struct PlayerOptions {
    /// The run ends when the character reaches this level.
    pub to_level: u8,
    pub game_pak: PathBuf,
    pub app_path: PathBuf,
    /// Checkpoint file (`--state`); defaults next to `--out`.
    pub state_path: PathBuf,
    /// Continue from the checkpoint of an earlier run.
    pub resume: bool,
    pub stop_on: StopOn,
    /// Overall wall-clock budget of the run.
    pub max_runtime: Duration,
    /// `--plan` path (hashed into the checkpoint).
    pub plan_path: PathBuf,
    /// Where the blocker report (`<out>.blocker.json`) goes.
    pub blocker_path: PathBuf,
    /// `--select-character`: the character name to continue with (resume).
    pub character: Option<String>,
    /// `--strict-order`: play the quests exactly in plan order (default: the nearest playable
    /// quest among the next few).
    pub strict_order: bool,
    /// The `--out` JSONL the summary is built from.
    pub out: Option<PathBuf>,
    /// Where `summary.md` goes.
    pub summary_path: PathBuf,
}

/// Counters of the walks and fights of a run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlayerStats {
    pub walks: u32,
    pub walked_m: f32,
    pub walk_secs: f32,
    pub routes_navmesh: u32,
    pub routes_heightmap: u32,
    pub routes_straight: u32,
    pub defends: u32,
    pub revives: u32,
    /// Counted teleport fallbacks by block reason.
    pub teleports: BTreeMap<String, u32>,
}

/// What a walk needs; also handed to the fight loop's [`Approach`].
pub struct WalkCtx {
    pub terrain: Arc<Terrain>,
    pub cfg: WalkerConfig,
    /// Free text naming the quest / step the walk belongs to (the fallback reason).
    pub context: String,
    /// Walk fallbacks not yet attached to a quest result.
    pub fallbacks: Vec<Fallback>,
    pub stats: PlayerStats,
    pub trace: bool,
}

/// The per-character player-mode state hung on `GameClient::player`.
pub struct PlayerMode {
    pub walk: WalkCtx,
    pub combat: Session,
    /// GM commands issued while in player mode, by verb (`gm_command` cross-check of the
    /// fallbacks in the result lines).
    pub gm_commands: BTreeMap<String, u32>,
    pub checkpoint: Checkpoint,
    checkpoint_path: PathBuf,
    started: Instant,
}

impl PlayerMode {
    pub fn new(
        terrain: Arc<Terrain>,
        mut cfg: WalkerConfig,
        combat: Session,
        checkpoint: Checkpoint,
        checkpoint_path: PathBuf,
        trace: bool,
    ) -> Self {
        if cfg.water_level.is_none() {
            cfg.water_level = Some(terrain.ocean_level);
        }
        Self {
            walk: WalkCtx {
                terrain,
                cfg,
                context: String::new(),
                fallbacks: Vec::new(),
                stats: PlayerStats::default(),
                trace,
            },
            combat,
            gm_commands: BTreeMap::new(),
            checkpoint,
            checkpoint_path,
            started: Instant::now(),
        }
    }

    /// Player mode never uses `/godmode`, `/level` or `/kill`; every other GM command is a
    /// counted fallback (the quest journal records the semantic ones, `gm_<verb>` the rest).
    pub fn gm_command(&mut self, command: &str) -> anyhow::Result<()> {
        let text =
            command.trim_start_matches(aaemu_game::systems::services::commands::COMMAND_PREFIX);
        let verb = gm_verb(command)?;
        *self.gm_commands.entry(verb.clone()).or_insert(0) += 1;
        // `item add` and `spawn` are marked by the quest journal with their real reason.
        if !matches!(verb.as_str(), "item" | "spawn") {
            self.walk.fallbacks.push(Fallback::new(
                &format!("gm_{verb}"),
                text.to_string(),
                format!("{} used the /{verb} GM command", self.walk.context),
            ));
        }
        Ok(())
    }

    /// The walk / GM fallbacks not yet attached to a result line.
    pub fn take_fallbacks(&mut self) -> Vec<Fallback> {
        std::mem::take(&mut self.walk.fallbacks)
    }

    /// Sets the quest / step the checkpoint and the fallback reasons refer to.
    pub fn at_step(&mut self, quest: u32, name: &str, act: Option<(u32, &str)>) {
        self.walk.context = match act {
            Some((id, kind)) => format!("quest {quest} {name} step {id} {kind}"),
            None => format!("quest {quest} {name}"),
        };
        self.checkpoint.current_quest = Some(quest);
        self.checkpoint.current_step = act.map(|(id, kind)| format!("{id} {kind}"));
        self.save();
    }

    /// Rewrites the checkpoint (best effort).
    pub fn save(&mut self) {
        self.checkpoint.deaths = self.combat.deaths;
        self.checkpoint.kills = self.combat.kills;
        self.checkpoint.updated_unix = unix_now();
        for (kind, n) in self.walk.stats.teleports.iter() {
            self.checkpoint
                .fallbacks
                .insert(format!("teleport:{kind}"), *n);
        }
        if let Err(e) = write_json(&self.checkpoint_path, &self.checkpoint) {
            tracing::warn!("checkpoint write failed: {e:#}");
        }
    }

    /// Seconds since this run started.
    pub fn elapsed_s(&self) -> f32 {
        self.started.elapsed().as_secs_f32()
    }
}

/// The verb of a GM command line; `godmode`, `level` and `kill` are refused in player mode.
pub fn gm_verb(command: &str) -> anyhow::Result<String> {
    let text = command.trim_start_matches(aaemu_game::systems::services::commands::COMMAND_PREFIX);
    let verb = text.split_whitespace().next().unwrap_or("").to_lowercase();
    if matches!(verb.as_str(), "godmode" | "level" | "kill") {
        bail!("player mode does not use the /{verb} GM command");
    }
    Ok(verb)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(value)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// -------------------------------------------------------------------------------------------
// Checkpoint
// -------------------------------------------------------------------------------------------

/// The blocker that stopped a run (also `<out>.blocker.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunBlocker {
    pub quest_id: u32,
    pub name: String,
    pub result: QuestOutcome,
    pub failed_step: Option<crate::questrun::FailedStep>,
    pub blocker: Blocker,
    pub fallbacks: Vec<Fallback>,
    pub level: u8,
    pub position: [f32; 3],
    /// The last chat lines / observations around the failure.
    pub messages: Vec<String>,
}

/// Per-character resume state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Checkpoint {
    pub character: String,
    pub race: u8,
    pub gender: u8,
    pub plan_path: String,
    pub plan_sha256: String,
    /// The quest / step being run (`act_id act_type`) when the file was written.
    pub current_quest: Option<u32>,
    pub current_step: Option<String>,
    /// Quests completed by the runs recorded here (the server's own list decides on resume).
    pub completed: Vec<u32>,
    pub blocker: Option<RunBlocker>,
    /// Fallback tally by kind (`teleport:<reason>`, `gm_item`, ...), summed over resumed runs.
    pub fallbacks: BTreeMap<String, u32>,
    pub deaths: u32,
    pub kills: u32,
    pub level: u8,
    pub runs: u32,
    pub started_unix: u64,
    pub updated_unix: u64,
    pub elapsed_s: f32,
}

impl Checkpoint {
    pub fn load(path: &Path) -> anyhow::Result<Option<Self>> {
        if !path.is_file() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading checkpoint {}", path.display()))?;
        Ok(Some(serde_json::from_str(&text).with_context(|| {
            format!("parsing checkpoint {}", path.display())
        })?))
    }

    pub fn fresh(path: &Path) -> Self {
        Self {
            plan_path: path.display().to_string(),
            started_unix: unix_now(),
            ..Self::default()
        }
    }

    /// Adds a fallback of a result line to the tally.
    pub fn count_fallback(&mut self, fallback: &Fallback) {
        let key = if fallback.kind == "teleport" {
            // `teleport:<reason>` is kept by `save` from the walk stats; do not double count.
            return;
        } else {
            fallback.kind.clone()
        };
        *self.fallbacks.entry(key).or_insert(0) += 1;
    }
}

/// SHA-256 of the plan file (hex).
pub fn plan_hash(path: &Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

// -------------------------------------------------------------------------------------------
// Walking
// -------------------------------------------------------------------------------------------

/// The fight loop's way of moving: real walking, never a GM teleport unless counted.
struct WalkApproach<'a> {
    ctx: &'a mut WalkCtx,
}

impl Approach for WalkApproach<'_> {
    fn approach<'a>(
        &'a mut self,
        game: &'a mut GameClient,
        pos: (f32, f32, f32),
        face: Option<(f32, f32, f32)>,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            walk_inner(game, self.ctx, None, pos).await?;
            let now = game.state.self_pos;
            if let Some(target) = face {
                game.move_to_facing(now, target, 600).await?;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
            Ok(())
        })
    }
}

/// Walks the character to `target` (player mode's `teleport`).
pub async fn walk_to(game: &mut GameClient, target: (f32, f32, f32)) -> anyhow::Result<()> {
    let Some(mut pm) = game.player.take() else {
        return game.teleport_gm(target).await;
    };
    let result = {
        let pm = &mut *pm;
        walk_inner(game, &mut pm.walk, Some(&mut pm.combat), target).await
    };
    game.player = Some(pm);
    result
}

/// Fights `obj` with the character's skills (player mode's `/kill`). `Ok(true)` = it died.
pub async fn fight(game: &mut GameClient, obj: u32) -> anyhow::Result<bool> {
    let Some(mut pm) = game.player.take() else {
        bail!("fight() outside player mode");
    };
    let result = {
        let pm = &mut *pm;
        let mut approach = WalkApproach { ctx: &mut pm.walk };
        pm.combat.kill(game, obj, &mut approach).await
    };
    game.player = Some(pm);
    Ok(matches!(result?, KillOutcome::Killed))
}

/// Revives the character when the death was seen outside a fight; `Ok(true)` = it was dead.
pub async fn revive_if_dead(game: &mut GameClient) -> anyhow::Result<bool> {
    let me = game.state.self_obj;
    if me == 0 || !game.state.dead_units.contains(&me) {
        return Ok(false);
    }
    let Some(mut pm) = game.player.take() else {
        return Ok(false);
    };
    let result = pm.combat.revive(game).await;
    pm.walk.stats.revives += 1;
    game.player = Some(pm);
    result?;
    Ok(true)
}

/// A hostile NPC that hit the character within [`THREAT_WINDOW_MS`] and is still alive.
fn threat(game: &GameClient) -> Option<u32> {
    let now = observation_ms();
    game.state
        .attackers
        .iter()
        .filter(|(obj, at)| {
            now.saturating_sub(**at) <= THREAT_WINDOW_MS
                && !game.state.dead_units.contains(obj)
                && game.state.units.get(obj).is_some_and(|u| u.unit_type == 1)
        })
        .max_by_key(|(_, at)| **at)
        .map(|(obj, _)| *obj)
}

fn note_teleport(
    ctx: &mut WalkCtx,
    reason: &str,
    at: (f32, f32, f32),
    from: (f32, f32, f32),
    detail: &str,
) {
    *ctx.stats.teleports.entry(reason.to_string()).or_insert(0) += 1;
    ctx.fallbacks.push(Fallback::new(
        "teleport",
        format!("{:.1},{:.1},{:.1}", at.0, at.1, at.2),
        format!(
            "{reason}: {} (from {:.0},{:.0},{:.0}){}",
            ctx.context,
            from.0,
            from.1,
            from.2,
            if detail.is_empty() {
                String::new()
            } else {
                format!(" {detail}")
            }
        ),
    ));
}

fn global_ms() -> u32 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    // The landing move of a GM teleport reports 1000; stay above it.
    1_000 + START.get_or_init(Instant::now).elapsed().as_millis() as u32
}

/// The walk itself. `combat` (when given) answers attackers and revives deaths on the way.
async fn walk_inner(
    game: &mut GameClient,
    ctx: &mut WalkCtx,
    mut combat: Option<&mut Session>,
    target: (f32, f32, f32),
) -> anyhow::Result<()> {
    let cfg = ctx.cfg;
    if game.state.world_instance != 0 {
        // The walker only knows the main world's terrain and navmesh.
        let from = game.state.self_pos;
        note_teleport(
            ctx,
            "instance_world",
            target,
            from,
            "(the walker only has main_world geodata)",
        );
        return game.teleport_gm(target).await;
    }
    let mut replans = 0u32;
    'plan: loop {
        let from = game.state.self_pos;
        if dist_2d(from, target) <= cfg.arrive_radius {
            return Ok(());
        }
        replans += 1;
        if replans > MAX_REPLANS {
            note_teleport(ctx, "replan_limit", target, from, "");
            return game.teleport_gm(target).await;
        }
        let direct = dist_2d(from, target) < STRAIGHT_HOP_M;
        let (route, source) = if direct {
            (vec![target], "straight")
        } else {
            ctx.terrain
                .route_with_source(from, target, &cfg)
                .filter(|(r, _)| !r.is_empty())
                .unwrap_or((vec![target], "straight"))
        };
        if source.starts_with("navmesh") {
            ctx.stats.routes_navmesh += 1;
        } else if source == "heightmap" {
            ctx.stats.routes_heightmap += 1;
        } else {
            ctx.stats.routes_straight += 1;
        }
        let route_m: f32 = std::iter::once(from)
            .chain(route.iter().copied())
            .collect::<Vec<_>>()
            .windows(2)
            .map(|w| dist_2d(w[0], w[1]))
            .sum();
        if ctx.trace && !direct {
            tracing::info!(
                "[walk] {} -> {:.0},{:.0} via {source}: {} waypoint(s), {route_m:.0} m",
                ctx.context,
                target.0,
                target.1,
                route.len()
            );
        }
        let mut walker = Walker::with_route(ctx.terrain.clone(), cfg, from, route)?;
        let budget = Duration::from_secs_f32(route_m / cfg.speed_mps * 1.7 + 40.0);
        let started = Instant::now();
        ctx.stats.walks += 1;
        let mut last_pos = from;
        loop {
            if game.state.disconnected {
                bail!("disconnected while walking");
            }
            let me = game.state.self_obj;
            if game.state.dead_units.contains(&me) {
                let Some(c) = combat.as_deref_mut() else {
                    bail!("the character died while approaching");
                };
                c.revive(game).await?;
                ctx.stats.revives += 1;
                ctx.stats.walked_m += dist_2d(from, last_pos);
                ctx.stats.walk_secs += started.elapsed().as_secs_f32();
                continue 'plan;
            }
            if let Some(c) = combat.as_deref_mut() {
                if let Some(obj) = threat(game) {
                    ctx.stats.defends += 1;
                    ctx.stats.walked_m += dist_2d(from, last_pos);
                    ctx.stats.walk_secs += started.elapsed().as_secs_f32();
                    let mut approach = WalkApproach { ctx: &mut *ctx };
                    Box::pin(c.kill(game, obj, &mut approach)).await?;
                    continue 'plan;
                }
            }
            let now = started.elapsed();
            if now > budget {
                let at = walker.pos();
                note_teleport(ctx, "walk_timeout", target, at, "");
                game.teleport_gm(near_point(at, target)).await?;
                ctx.stats.walked_m += dist_2d(from, last_pos);
                ctx.stats.walk_secs += now.as_secs_f32();
                return Ok(());
            }
            match walker.tick(now) {
                Step::Move(m) => {
                    send_move(game, m, global_ms()).await?;
                    last_pos = m.pos;
                    pump(game, Duration::from_millis(10)).await?;
                    if let Some(unit) = game.state.units.get(&game.state.self_obj) {
                        walker.observe_server(unit.pos, now);
                    }
                }
                Step::Wait { until } => {
                    let left = until.saturating_sub(started.elapsed());
                    pump(game, left.min(Duration::from_millis(100))).await?;
                }
                Step::Blocked { reason } => {
                    if ctx.trace {
                        tracing::info!(
                            "[walk] blocked {} at {:.1},{:.1},{:.2}",
                            reason.as_str(),
                            walker.pos().0,
                            walker.pos().1,
                            walker.pos().2
                        );
                    }
                    pump(game, Duration::from_millis(25)).await?;
                }
                Step::Arrived { .. } => {
                    ctx.stats.walked_m += dist_2d(from, last_pos);
                    ctx.stats.walk_secs += started.elapsed().as_secs_f32();
                    return Ok(());
                }
                Step::Teleport { at, reason } => {
                    let pos = walker.pos();
                    note_teleport(ctx, reason.as_str(), at, pos, "");
                    game.teleport_gm(near_point(pos, at)).await?;
                    walker.teleported(game.state.self_pos, started.elapsed());
                }
            }
        }
    }
}

// -------------------------------------------------------------------------------------------
// Summary
// -------------------------------------------------------------------------------------------

/// Groups a fallback reason into a stable key (`path_blocked: quest 330 ...` -> `path_blocked`).
fn reason_head(reason: &str) -> String {
    reason
        .split([':', ' '])
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

/// The `summary.md` of a run, from the result lines of its `--out` file and the checkpoint.
pub fn render_summary(
    results: &[QuestResult],
    cp: &Checkpoint,
    stats: &PlayerStats,
    final_level: u8,
    elapsed_s: f32,
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let completed: Vec<&QuestResult> = results
        .iter()
        .filter(|r| r.result == QuestOutcome::Completed)
        .collect();
    let _ = writeln!(out, "# Player-mode run: {}\n", cp.character);
    let _ = writeln!(
        out,
        "- race {} gender {}; level {} reached (started at {}); deaths {}; kills {}",
        cp.race,
        cp.gender,
        final_level,
        results.first().map_or(1, |r| r.start_level),
        cp.deaths,
        cp.kills
    );
    let _ = writeln!(
        out,
        "- time: this run {:.0} min, all runs {:.0} min ({} run(s))",
        elapsed_s / 60.0,
        cp.elapsed_s / 60.0,
        cp.runs
    );
    let _ = writeln!(
        out,
        "- quests: {} result line(s), {} completed",
        results.len(),
        completed.len()
    );
    let _ = writeln!(
        out,
        "- walking: {} walk(s), {:.0} m in {:.0} s; routes: {} navmesh, {} heightmap, {} straight; \
         {} attacker fight(s) while walking; {} revive(s)\n",
        stats.walks,
        stats.walked_m,
        stats.walk_secs,
        stats.routes_navmesh,
        stats.routes_heightmap,
        stats.routes_straight,
        stats.defends,
        stats.revives
    );
    let mut by_outcome: BTreeMap<&str, u32> = BTreeMap::new();
    for r in results {
        *by_outcome.entry(r.result.name()).or_insert(0) += 1;
    }
    let _ = writeln!(out, "## Quest outcomes\n");
    for (k, n) in &by_outcome {
        let _ = writeln!(out, "- {k}: {n}");
    }
    let _ = writeln!(out, "\n## Completed quests\n");
    for r in &completed {
        let _ = writeln!(
            out,
            "- {} {} ({:.0} s, {} fallback(s))",
            r.quest_id,
            r.name,
            r.elapsed_ms as f32 / 1000.0,
            r.fallbacks.len()
        );
    }
    let mut by_kind: BTreeMap<String, u32> = BTreeMap::new();
    let mut by_reason: BTreeMap<String, u32> = BTreeMap::new();
    for r in results {
        for f in &r.fallbacks {
            *by_kind.entry(f.kind.clone()).or_insert(0) += 1;
            if f.kind == "teleport" {
                *by_reason.entry(reason_head(&f.reason)).or_insert(0) += 1;
            }
        }
    }
    let _ = writeln!(out, "\n## Fallbacks by kind\n");
    if by_kind.is_empty() {
        let _ = writeln!(out, "- none");
    }
    for (k, n) in &by_kind {
        let _ = writeln!(out, "- {k}: {n}");
    }
    if !by_reason.is_empty() {
        let _ = writeln!(out, "\nTeleport fallbacks by reason:\n");
        for (k, n) in &by_reason {
            let _ = writeln!(out, "- {k}: {n}");
        }
    }
    let _ = writeln!(out, "\n## Value-check suspects\n");
    let mut suspects = 0;
    for r in results {
        if let Some(v) = &r.value_checks {
            for line in &v.suspects {
                suspects += 1;
                let _ = writeln!(out, "- quest {} {}: {line}", r.quest_id, r.name);
            }
        }
    }
    if suspects == 0 {
        let _ = writeln!(out, "- none (or no completed quest was value-checked)");
    }
    let _ = writeln!(out, "\n## Not completed\n");
    let mut any = false;
    for r in results {
        if matches!(
            r.result,
            QuestOutcome::Completed | QuestOutcome::AlreadyCompleted
        ) {
            continue;
        }
        any = true;
        let _ = writeln!(
            out,
            "- {} {}: {} [{}] {}",
            r.quest_id,
            r.name,
            r.result.name(),
            r.blocker.class,
            r.failed_step
                .as_ref()
                .map(|f| format!("{} {}: {}", f.act_id, f.act_type, f.reason))
                .unwrap_or_default()
        );
    }
    if !any {
        let _ = writeln!(out, "- none");
    }
    let _ = writeln!(out, "\n## Blocker\n");
    match &cp.blocker {
        Some(b) => {
            let _ = writeln!(
                out,
                "Quest {} {} ended `{}` at level {} near {:.0},{:.0},{:.0}.\n",
                b.quest_id,
                b.name,
                b.result.name(),
                b.level,
                b.position[0],
                b.position[1],
                b.position[2]
            );
            let _ = writeln!(out, "- class: {} ({})", b.blocker.class, b.blocker.code);
            let _ = writeln!(out, "- reason: {}", b.blocker.reason);
            if !b.blocker.evidence.is_empty() {
                let _ = writeln!(out, "- evidence: {}", b.blocker.evidence);
            }
            if let Some(f) = &b.failed_step {
                let _ = writeln!(
                    out,
                    "- failed step: {} {}: {}",
                    f.act_id, f.act_type, f.reason
                );
            }
            for m in b.messages.iter().take(12) {
                let _ = writeln!(out, "  - chat: {m}");
            }
        }
        None => {
            let _ = writeln!(out, "- none: the run ended on its own limit.");
        }
    }
    out
}

/// Reads every result line of a JSONL file.
pub fn read_results(path: &Path) -> Vec<QuestResult> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str::<QuestResult>(l).ok())
        .collect()
}

/// Test hook: fight module re-export so callers need one import.
pub use fight::Session as CombatSession;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn godmode_level_and_kill_are_refused_everything_else_is_a_verb() {
        for cmd in ["godmode true", "/level 10", "kill", "/KILL"] {
            assert!(gm_verb(cmd).is_err(), "{cmd}");
        }
        assert_eq!(gm_verb("item add 123 1").unwrap(), "item");
        assert_eq!(gm_verb("/spawn npc 5").unwrap(), "spawn");
        assert_eq!(gm_verb("gold add self 1 0 0").unwrap(), "gold");
    }

    #[test]
    fn checkpoint_round_trips_and_resume_data_survives() {
        let dir = std::env::temp_dir().join(format!("aaemu-pm-cp-{}", std::process::id()));
        let path = dir.join("bot.state.json");
        let mut cp = Checkpoint::fresh(Path::new("plan.json"));
        cp.character = "Botty".into();
        cp.current_quest = Some(330);
        cp.current_step = Some("12 QuestActConAcceptNpc".into());
        cp.completed = vec![16];
        cp.fallbacks.insert("teleport:path_blocked".into(), 2);
        write_json(&path, &cp).unwrap();
        let back = Checkpoint::load(&path).unwrap().expect("checkpoint");
        assert_eq!(back.character, "Botty");
        assert_eq!(back.current_quest, Some(330));
        assert_eq!(back.completed, vec![16]);
        assert_eq!(back.fallbacks["teleport:path_blocked"], 2);
        assert!(Checkpoint::load(&dir.join("missing.json"))
            .unwrap()
            .is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn teleports_are_counted_in_the_checkpoint_not_per_result() {
        let mut cp = Checkpoint::default();
        cp.count_fallback(&Fallback::new("teleport", "1,2,3", "path_blocked: x"));
        cp.count_fallback(&Fallback::new("gm_item", "123", "no source"));
        assert_eq!(cp.fallbacks.get("gm_item"), Some(&1));
        assert!(!cp.fallbacks.contains_key("teleport"));
    }

    #[test]
    fn plan_hash_changes_with_the_plan() {
        let dir = std::env::temp_dir().join(format!("aaemu-pm-hash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.json");
        std::fs::write(&a, "[1]").unwrap();
        let h1 = plan_hash(&a).unwrap();
        std::fs::write(&a, "[2]").unwrap();
        let h2 = plan_hash(&a).unwrap();
        assert_ne!(h1, h2);
        assert_eq!(h1.len(), 64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reason_head_groups_teleport_reasons() {
        assert_eq!(
            reason_head("path_blocked: quest 1 x (from 1,2,3)"),
            "path_blocked"
        );
        assert_eq!(reason_head("instance_world"), "instance_world");
    }

    #[test]
    fn summary_lists_the_blocker_and_fallbacks() {
        let mut cp = Checkpoint::default();
        cp.character = "Botty".into();
        cp.blocker = Some(RunBlocker {
            quest_id: 330,
            name: "q".into(),
            result: QuestOutcome::Stuck,
            failed_step: None,
            blocker: Blocker::new("runner_gap", "no path", "walk.rs"),
            fallbacks: Vec::new(),
            level: 3,
            position: [1.0, 2.0, 3.0],
            messages: vec!["hello".into()],
        });
        let text = render_summary(&[], &cp, &PlayerStats::default(), 3, 60.0);
        assert!(text.contains("Quest 330"), "{text}");
        assert!(text.contains("runner_gap"), "{text}");
        assert!(text.contains("## Fallbacks by kind"), "{text}");
    }
}
