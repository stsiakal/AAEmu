//! Quest-audit runner v1 (`aaemu-testclient quest`): drives one GM bot through a
//! `rust/tools/quest_audit/plan.py` plan, one quest at a time, and appends one JSON result
//! line per quest.
//!
//! The plan lists the quest components / acts in the server's own load order, so the runner
//! can walk them in sequence: accept at the giver NPC, do the objectives (talk / hunt),
//! report, and let the passive reward acts play out. Quest state is read back from
//! [`crate::game::GameState`] (filled by the packet observers); every bounded wait fails the
//! step with `timeout` instead of hanging.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use aaemu_game::opcodes::{pp, sc};
use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};

use crate::game::{GameClient, Packet, UnitInfo, LEVEL_PROXY};
use crate::login::LoginClient;
use crate::quest::{self, QuestInfo};
use crate::record::Recorder;
use crate::scenario::BotConfig;

/// Unit-appearance wait after a teleport.
const APPEAR_WAIT: Duration = Duration::from_secs(15);
/// Default per-step timeout (`--step-timeout-secs`).
pub const DEFAULT_STEP_TIMEOUT: Duration = Duration::from_secs(60);
/// `/kill` is instant; its reply (if any) is the error text.
const KILL_WAIT: Duration = Duration::from_millis(800);
/// Packet-pump chunk used by every bounded wait.
const PUMP: Duration = Duration::from_millis(250);
/// Quiet / max drain after a step or teleport, to process removals and replies.
const SETTLE_QUIET: Duration = Duration::from_millis(200);
const SETTLE_MAX: Duration = Duration::from_secs(1);
/// Quiet / max drain after the last step, before the result is recorded.
const FINAL_QUIET: Duration = Duration::from_millis(500);
const FINAL_MAX: Duration = Duration::from_secs(2);
/// The world the bot spawns in; v1 only teleports within it.
const MAIN_WORLD: &str = "main_world";

// ----- plan model (`plan.py` output) -----

/// One quest of the plan file (the file is a top-level JSON array).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct PlanQuest {
    pub quest_id: u32,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub category_id: u32,
    /// `quest_contexts.LEVEL`; null for quests without one.
    #[serde(default)]
    pub level: Option<u32>,
    #[serde(default)]
    pub steps: Vec<PlanStep>,
    /// `"ok"` / `"partial"` / `"blocked"`.
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub blockers: Vec<String>,
}

/// One act of a quest component, in the loader's order.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct PlanStep {
    pub component_id: u32,
    pub component_kind: String,
    pub act_id: u32,
    pub act_type: String,
    #[serde(default)]
    pub target: PlanTarget,
    #[serde(default)]
    pub locations: Vec<PlanLocation>,
    /// `"ok"` / `"no_spawn"` / `"dynamic_spawn"` / `"unsupported"`.
    #[serde(default)]
    pub status: String,
    /// Present for `dynamic_spawn`: the skills whose `SpawnEffect` summons the target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<PlanSource>>,
}

/// The act's target ids (kind-specific extras are ignored).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct PlanTarget {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub ids: Vec<u32>,
    #[serde(default = "default_count")]
    pub count: i32,
}

impl Default for PlanTarget {
    fn default() -> Self {
        Self {
            kind: String::new(),
            ids: Vec::new(),
            count: default_count(),
        }
    }
}

fn default_count() -> i32 {
    1
}

/// One spawn location of a target.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct PlanLocation {
    pub world: String,
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

/// A runtime spawn source (`spawn_effects` / skills).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct PlanSource {
    #[serde(default)]
    pub effect_id: u32,
    #[serde(default)]
    pub skill_ids: Vec<u32>,
}

/// Reads a plan file (the JSON array `plan.py` writes).
pub fn load_plan(path: &Path) -> anyhow::Result<Vec<PlanQuest>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading quest plan {}", path.display()))?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    serde_json::from_str(text).with_context(|| format!("parsing quest plan {}", path.display()))
}

// ----- result lines -----

/// How a quest ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestOutcome {
    Completed,
    Failed,
    Stuck,
    BlockedPlan,
    RunnerUnsupported,
    NeedsTrigger,
    Error,
}

/// What one step did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepOutcomeKind {
    Ok,
    Passive,
    Skipped,
    Timeout,
    Unsupported,
    NeedsTrigger,
    Failed,
    Error,
}

/// One plan step's execution record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StepOutcome {
    pub act_id: u32,
    pub act_type: String,
    pub outcome: StepOutcomeKind,
    pub ms: u64,
}

/// The step a quest stopped on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FailedStep {
    pub act_id: u32,
    pub act_type: String,
    pub reason: String,
}

/// One `--out` JSONL line.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QuestResult {
    pub quest_id: u32,
    pub name: String,
    pub result: QuestOutcome,
    pub failed_step: Option<FailedStep>,
    pub steps: Vec<StepOutcome>,
    pub final_status: Option<u8>,
    pub final_objectives: Option<[i32; 5]>,
    pub messages: Vec<String>,
    pub elapsed_ms: u64,
}

/// The whole run's result.
#[derive(Debug, Default)]
pub struct RunReport {
    pub results: Vec<QuestResult>,
    /// The connection was lost; the run stopped after the last written line.
    pub aborted: bool,
}

// ----- step dispatch -----

/// What the runner does for a plan step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepAction {
    /// Fail / Drop branch: the completed quest never executes them.
    Skip,
    Accept,
    Talk,
    Kill,
    Report,
    /// `QuestActConAutoComplete` / `QuestActSupply*` / `QuestActCheck*`: nothing to send.
    Passive,
    /// Dynamic spawn whose `SpawnEffect` skills v1 does not trigger.
    NeedsTrigger,
    /// Any other act type.
    Unsupported,
}

/// The dispatch decision. Dynamic sources win over the act type: a target that only exists
/// after a `SpawnEffect` skill cannot be reached, whatever the act would do with it.
pub fn step_action(step: &PlanStep) -> StepAction {
    if matches!(step.component_kind.as_str(), "Fail" | "Drop") {
        return StepAction::Skip;
    }
    if let Some(sources) = &step.sources {
        if sources.iter().any(|s| !s.skill_ids.is_empty()) {
            return StepAction::NeedsTrigger;
        }
        return StepAction::Unsupported;
    }
    match step.act_type.as_str() {
        "QuestActConAcceptNpc" => StepAction::Accept,
        "QuestActObjTalk" => StepAction::Talk,
        "QuestActObjMonsterHunt" | "QuestActObjMonsterGroupHunt" | "QuestActConAcceptNpcKill" => {
            StepAction::Kill
        }
        "QuestActConReportNpc" => StepAction::Report,
        t if t == "QuestActConAutoComplete"
            || t.starts_with("QuestActSupply")
            || t.starts_with("QuestActCheck") =>
        {
            StepAction::Passive
        }
        _ => StepAction::Unsupported,
    }
}

/// A blocked plan quest is recorded without running (`blocked_plan`).
fn plan_gate(quest: &PlanQuest) -> Option<QuestOutcome> {
    (quest.status == "blocked").then_some(QuestOutcome::BlockedPlan)
}

/// `QuestActTemplate::counts_as_objective` (`systems/quests/templates.rs`): the acts that own
/// one of the five objective counters, in the loader's order per component kind.
pub fn counts_as_objective(act_type: &str) -> bool {
    matches!(
        act_type,
        "QuestActConAcceptBuff"
            | "QuestActConAcceptItemEquip"
            | "QuestActConFail"
            | "QuestActObjAbilityLevel"
            | "QuestActObjAggro"
            | "QuestActObjCinema"
            | "QuestActObjCompleteQuest"
            | "QuestActObjCraft"
            | "QuestActObjDistance"
            | "QuestActObjExpressFire"
            | "QuestActObjInteraction"
            | "QuestActObjItemGather"
            | "QuestActObjItemGroupGather"
            | "QuestActObjItemGroupUse"
            | "QuestActObjItemUse"
            | "QuestActObjLevel"
            | "QuestActObjMateLevel"
            | "QuestActObjMonsterGroupHunt"
            | "QuestActObjMonsterHunt"
            | "QuestActObjSphere"
            | "QuestActObjTalkNpcGroup"
            | "QuestActObjTalk"
            | "QuestActObjZoneKill"
            | "QuestActObjZoneMonsterHunt"
    )
}

/// The objective counter index of every plan step, replicating
/// `QuestData::update_quest_component_acts`: the index resets when the component kind changes
/// and only `counts_as_objective` acts advance it.
pub fn objective_indices(steps: &[PlanStep]) -> Vec<Option<u8>> {
    let mut out = Vec::with_capacity(steps.len());
    let mut index: u8 = 0;
    let mut last_kind = String::new();
    for step in steps {
        if step.component_kind != last_kind {
            index = 0;
            last_kind = step.component_kind.clone();
        }
        if counts_as_objective(&step.act_type) {
            out.push(Some(index));
            index = index.wrapping_add(1);
        } else {
            out.push(None);
        }
    }
    out
}

// ----- run -----

enum Selected<'a> {
    Plan(&'a PlanQuest),
    Unknown(u32),
}

/// Runs the selected quests of `plan`; one JSON line per quest goes to stdout and, when
/// given, is appended to `out`.
pub async fn run(
    cfg: &BotConfig,
    plan: &[PlanQuest],
    requested: &[u32],
    all_ok: bool,
    out: Option<&Path>,
    step_timeout: Duration,
) -> anyhow::Result<RunReport> {
    let selected: Vec<Selected> = if !requested.is_empty() {
        let mut seen = HashSet::new();
        requested
            .iter()
            .filter(|id| seen.insert(**id))
            .map(|id| {
                plan.iter()
                    .find(|q| q.quest_id == *id)
                    .map_or(Selected::Unknown(*id), Selected::Plan)
            })
            .collect()
    } else if all_ok {
        plan.iter()
            .filter(|q| q.status == "ok")
            .map(Selected::Plan)
            .collect()
    } else {
        bail!("the quest command needs --quest <id> (repeatable) and/or --all-ok");
    };

    let mut writer = match out {
        Some(path) => Some(BufWriter::new(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("opening {} for append", path.display()))?,
        )),
        None => None,
    };

    let rec = Recorder::new(false);
    let mut game = setup_world(cfg, &rec).await?;
    // God mode keeps the bot alive while it stands in the spawns; `/level` is issued per quest.
    game.gm("godmode true", Duration::from_secs(1)).await.ok();

    let mut raised_level = 1u32;
    let mut report = RunReport::default();
    for target in selected {
        if game.state.disconnected {
            // The connection dropped between two quests: still write the error line.
            let result = match target {
                Selected::Plan(quest) => disconnected_result(quest),
                Selected::Unknown(id) => missing_quest_result(id),
            };
            emit(&mut writer, &result)?;
            report.results.push(result);
            report.aborted = true;
            break;
        }
        let result = match target {
            Selected::Plan(quest) => {
                run_quest(&mut game, quest, &mut raised_level, step_timeout).await
            }
            Selected::Unknown(id) => missing_quest_result(id),
        };
        emit(&mut writer, &result)?;
        report.results.push(result);
        if game.state.disconnected {
            report.aborted = true;
            break;
        }
    }
    leave(&mut game).await;
    Ok(report)
}

fn emit(out: &mut Option<BufWriter<File>>, result: &QuestResult) -> anyhow::Result<()> {
    let line = serde_json::to_string(result).context("serializing the quest result")?;
    println!("{line}");
    if let Some(w) = out {
        writeln!(w, "{line}")?;
        w.flush()?;
    }
    Ok(())
}

fn missing_quest_result(id: u32) -> QuestResult {
    QuestResult {
        quest_id: id,
        name: String::new(),
        result: QuestOutcome::BlockedPlan,
        failed_step: None,
        steps: Vec::new(),
        final_status: None,
        final_objectives: None,
        messages: vec![format!("quest {id} is not in the plan")],
        elapsed_ms: 0,
    }
}

/// The `error` line written when the connection is lost between two quests.
fn disconnected_result(quest: &PlanQuest) -> QuestResult {
    QuestResult {
        quest_id: quest.quest_id,
        name: quest.name.clone(),
        result: QuestOutcome::Error,
        failed_step: None,
        steps: Vec::new(),
        final_status: None,
        final_objectives: None,
        messages: vec!["connection lost".into()],
        elapsed_ms: 0,
    }
}

/// Character creation / selection like the `full` scenario: login, lobby burst, character
/// (create the first time), select, spawn and notify.
async fn setup_world(cfg: &BotConfig, rec: &Recorder) -> anyhow::Result<GameClient> {
    let mut login = LoginClient::connect(&cfg.login, rec.clone()).await?;
    let account_id = login.auth_trion(&cfg.user, &cfg.password).await?;
    anyhow::ensure!(account_id != 0, "account id 0");
    let worlds = login.list_worlds().await?;
    let world = worlds
        .worlds
        .iter()
        .find(|w| w.id == cfg.game_server_id)
        .with_context(|| format!("world {} not in ACWorldList", cfg.game_server_id))?;
    anyhow::ensure!(world.active, "world {} is not active", world.name);
    let cookie = login.enter_world(cfg.game_server_id).await?;

    let addr = format!("{}:{}", cookie.ip, cookie.port);
    let mut game = GameClient::connect(&addr, rec.clone()).await?;
    game.enter_world(account_id, cookie.cookie).await?;
    game.wait_sc(sc::X2EnterWorldResponsePacket).await?;
    game.wait_for("ChangeStatePacket(0)", |p| {
        p.is(LEVEL_PROXY, pp::ChangeStatePacket)
    })
    .await?;
    for state in 0..=7 {
        game.finish_state(state).await?;
        if state < 7 {
            game.wait_for("ChangeStatePacket", |p| {
                p.is(LEVEL_PROXY, pp::ChangeStatePacket)
            })
            .await?;
        } else {
            game.wait_sc(sc::SCUpdatePremiumPointPacket).await?;
        }
    }
    game.list_characters().await?;
    game.wait_for("last SCCharacterListPacket", |p| {
        p.is_sc(sc::SCCharacterListPacket) && p.body.first() == Some(&1)
    })
    .await?;
    game.drain(Duration::from_millis(300), Duration::from_secs(2))
        .await?;
    if game.state.characters.is_empty() {
        game.create_character(&cfg.character).await?;
        let p = game
            .wait_for("character creation result", |p| {
                p.is_sc(sc::SCCreateCharacterResponsePacket)
                    || p.is_sc(sc::SCCharacterCreationFailedPacket)
            })
            .await?;
        anyhow::ensure!(
            !p.is_sc(sc::SCCharacterCreationFailedPacket),
            "character creation failed with reason {:?}",
            p.body.first()
        );
    }
    let ch = game
        .state
        .characters
        .first()
        .cloned()
        .context("no character after the list / create handshake")?;
    game.select_character(ch.id).await?;
    game.wait_sc(sc::SCCharacterStatePacket).await?;
    game.drain(Duration::from_millis(1500), Duration::from_secs(10))
        .await?;
    game.spawn_character().await?;
    game.wait_for("own SCUnitStatePacket", |p| p.is_sc(sc::SCUnitStatePacket))
        .await?;
    game.notify_in_game().await?;
    game.notify_in_game_completed().await?;
    game.drain(Duration::from_millis(1500), Duration::from_secs(15))
        .await?;
    anyhow::ensure!(game.state.self_obj != 0, "own unit state not received");
    Ok(game)
}

/// Best-effort clean leave at the end of the run.
async fn leave(game: &mut GameClient) {
    if game.state.disconnected || game.leave_world(0).await.is_err() {
        return;
    }
    if let Ok(prep) = game.wait_sc(sc::SCPrepareLeaveWorldPacket).await {
        let timer_ms = prep.stream().read_i32().unwrap_or(0);
        if timer_ms <= 10_000 {
            let _ = game.wait_sc(sc::SCLeaveWorldGrantedPacket).await;
        }
    }
    let _ = game
        .drain(Duration::from_millis(300), Duration::from_secs(2))
        .await;
}

// ----- one quest -----

async fn run_quest(
    game: &mut GameClient,
    quest: &PlanQuest,
    raised_level: &mut u32,
    step_timeout: Duration,
) -> QuestResult {
    let start = Instant::now();
    let chat_start = game.state.chat_count;
    let mut steps = Vec::new();
    let mut failed_step = None;
    let mut last_info = None;
    let mut outcome = None;

    if let Some(gate) = plan_gate(quest) {
        return result_line(game, quest, gate, None, steps, last_info, chat_start, start);
    }

    // Meet the quest's level first; `/level` only adds the missing experience.
    if let Some(level) = quest.level {
        if level >= 2 && level > *raised_level {
            game.gm(&format!("level {level}"), Duration::from_secs(2))
                .await
                .ok();
            *raised_level = level;
        }
    }

    let indices = objective_indices(&quest.steps);
    for (i, step) in quest.steps.iter().enumerate() {
        let t0 = Instant::now();
        let deadline = t0 + step_timeout;
        let res = match step_action(step) {
            StepAction::Skip => Ok(StepOutcomeKind::Skipped),
            StepAction::Passive => Ok(StepOutcomeKind::Passive),
            StepAction::Accept => step_accept(game, quest.quest_id, step, deadline).await,
            StepAction::Talk => step_talk(game, quest.quest_id, step, indices[i], deadline).await,
            StepAction::Kill => step_kill(game, quest.quest_id, step, indices[i], deadline).await,
            StepAction::Report => step_report(game, quest.quest_id, step, deadline).await,
            StepAction::Unsupported => Err(StepFailure::unsupported(&step.act_type)),
            StepAction::NeedsTrigger => Err(StepFailure::plan(
                QuestOutcome::NeedsTrigger,
                StepOutcomeKind::NeedsTrigger,
                format!(
                    "{} target is only spawned by a SpawnEffect skill (v1 does not trigger it)",
                    step.act_type
                ),
            )),
        };
        let ms = t0.elapsed().as_millis() as u64;
        match res {
            Ok(kind) => {
                steps.push(StepOutcome {
                    act_id: step.act_id,
                    act_type: step.act_type.clone(),
                    outcome: kind,
                    ms,
                });
                snapshot(game, quest.quest_id, &mut last_info);
            }
            Err(f) => {
                steps.push(StepOutcome {
                    act_id: step.act_id,
                    act_type: step.act_type.clone(),
                    outcome: f.step,
                    ms,
                });
                outcome = Some(f.quest);
                failed_step = Some(FailedStep {
                    act_id: step.act_id,
                    act_type: step.act_type.clone(),
                    reason: f.reason,
                });
                break;
            }
        }
        if quest_failed(game, quest.quest_id) {
            outcome = Some(QuestOutcome::Failed);
            failed_step = Some(FailedStep {
                act_id: step.act_id,
                act_type: step.act_type.clone(),
                reason: "quest entered FAILED".into(),
            });
            break;
        }
    }

    // Settle: give the server a moment to finish passive / auto-completed quests.
    let drain_failed = game.drain(FINAL_QUIET, FINAL_MAX).await.is_err();
    snapshot(game, quest.quest_id, &mut last_info);

    let outcome = match outcome {
        Some(o) => o,
        None if quest_completed(game, quest.quest_id) => QuestOutcome::Completed,
        None if quest_failed(game, quest.quest_id) => QuestOutcome::Failed,
        None if drain_failed => QuestOutcome::Error,
        None => {
            if let Some(step) = quest
                .steps
                .iter()
                .rev()
                .find(|s| !matches!(step_action(s), StepAction::Skip | StepAction::Passive))
            {
                failed_step = Some(FailedStep {
                    act_id: step.act_id,
                    act_type: step.act_type.clone(),
                    reason: "quest not completed after all steps".into(),
                });
            }
            QuestOutcome::Stuck
        }
    };
    result_line(
        game,
        quest,
        outcome,
        failed_step,
        steps,
        last_info,
        chat_start,
        start,
    )
}

#[allow(clippy::too_many_arguments)]
fn result_line(
    game: &GameClient,
    quest: &PlanQuest,
    outcome: QuestOutcome,
    failed_step: Option<FailedStep>,
    steps: Vec<StepOutcome>,
    last_info: Option<QuestInfo>,
    chat_start: u64,
    start: Instant,
) -> QuestResult {
    let final_info = game
        .state
        .quests
        .get(&quest.quest_id)
        .cloned()
        .or(last_info);
    QuestResult {
        quest_id: quest.quest_id,
        name: quest.name.clone(),
        result: outcome,
        failed_step,
        steps,
        final_status: final_info.as_ref().map(|q| q.status),
        final_objectives: final_info.as_ref().map(|q| q.objectives),
        messages: window_messages(game, chat_start),
        elapsed_ms: start.elapsed().as_millis() as u64,
    }
}

/// The chat texts captured since `chat_start` (bounded by the last 64 kept by the state).
fn window_messages(game: &GameClient, chat_start: u64) -> Vec<String> {
    let new = game.state.chat_count.saturating_sub(chat_start) as usize;
    let take = new.min(game.state.chat_log.len());
    game.state
        .chat_log
        .iter()
        .skip(game.state.chat_log.len() - take)
        .cloned()
        .collect()
}

fn snapshot(game: &GameClient, quest_id: u32, last: &mut Option<QuestInfo>) {
    if let Some(q) = game.state.quests.get(&quest_id) {
        *last = Some(q.clone());
    }
}

// ----- state readers -----

/// Completion seen live in this session (never from the character-select snapshot, which would
/// mark quests that were already completed before the run).
fn quest_completed(game: &GameClient, quest_id: u32) -> bool {
    game.state.last_completed == Some(quest_id)
}

/// Failure seen live in this session (the select snapshot can also carry `FAILED` rows).
fn quest_failed(game: &GameClient, quest_id: u32) -> bool {
    game.state.last_failed == Some(quest_id)
}

fn objective_count(game: &GameClient, quest_id: u32, index: u8) -> i32 {
    game.state
        .quests
        .get(&quest_id)
        .and_then(|q| q.objectives.get(usize::from(index)))
        .copied()
        .unwrap_or(0)
}

fn nearest_target(game: &GameClient, ids: &[u32], avoid: &HashSet<u32>) -> Option<UnitInfo> {
    let me = game.state.self_pos;
    game.state
        .units
        .values()
        .filter(|u| {
            u.unit_type == 1
                && u.obj_id != game.state.self_obj
                && ids.contains(&u.id)
                && !avoid.contains(&u.obj_id)
        })
        .min_by(|a, b| dist2(a.pos, me).total_cmp(&dist2(b.pos, me)))
        .cloned()
}

fn dist2(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
    let (dx, dy, dz) = (a.0 - b.0, a.1 - b.1, a.2 - b.2);
    dx * dx + dy * dy + dz * dz
}

// ----- step failures -----

struct StepFailure {
    quest: QuestOutcome,
    step: StepOutcomeKind,
    reason: String,
}

impl StepFailure {
    fn plan(quest: QuestOutcome, step: StepOutcomeKind, reason: impl Into<String>) -> Self {
        Self {
            quest,
            step,
            reason: reason.into(),
        }
    }

    fn unsupported(act_type: &str) -> Self {
        Self::plan(
            QuestOutcome::RunnerUnsupported,
            StepOutcomeKind::Unsupported,
            format!("{act_type} is not handled by runner v1"),
        )
    }

    fn timeout(reason: impl Into<String>) -> Self {
        Self::plan(QuestOutcome::Stuck, StepOutcomeKind::Timeout, reason)
    }

    fn quest_failed(quest_id: u32) -> Self {
        Self::plan(
            QuestOutcome::Failed,
            StepOutcomeKind::Failed,
            format!("quest {quest_id} entered FAILED"),
        )
    }

    fn rejected(quest_id: u32, what: &str, p: &Packet) -> Self {
        let code = p.stream().read_i16().unwrap_or(0);
        Self::plan(
            QuestOutcome::Stuck,
            StepOutcomeKind::Failed,
            format!("server rejected {what} for quest {quest_id} (SCErrorMsg {code})"),
        )
    }

    fn io(e: anyhow::Error) -> Self {
        Self::plan(
            QuestOutcome::Error,
            StepOutcomeKind::Error,
            format!("{e:#}"),
        )
    }
}

// ----- shared waits -----

/// The first main-world location of a step (v1 teleports nowhere else).
fn step_location(step: &PlanStep) -> Result<(f32, f32, f32), StepFailure> {
    if let Some(loc) = step
        .locations
        .iter()
        .find(|l| l.world.eq_ignore_ascii_case(MAIN_WORLD))
    {
        return Ok((loc.x, loc.y, loc.z));
    }
    Err(match step.locations.first() {
        Some(loc) => StepFailure::plan(
            QuestOutcome::RunnerUnsupported,
            StepOutcomeKind::Unsupported,
            format!(
                "target is in world '{}' (v1 only teleports within {MAIN_WORLD})",
                loc.world
            ),
        ),
        None => StepFailure::plan(
            QuestOutcome::RunnerUnsupported,
            StepOutcomeKind::Unsupported,
            "step has no spawn location",
        ),
    })
}

/// ~2 m beside a spawn point.
fn near(pos: (f32, f32, f32)) -> (f32, f32, f32) {
    (pos.0 + 2.0, pos.1, pos.2)
}

async fn settle(game: &mut GameClient, deadline: Instant) {
    let max = (Instant::now() + SETTLE_MAX).min(deadline);
    let left = max.saturating_duration_since(Instant::now());
    if !left.is_zero() {
        let _ = game.drain(SETTLE_QUIET, left).await;
    }
}

/// Reads until `pred` matches; `None` on the deadline.
async fn wait_for_packet<F>(
    game: &mut GameClient,
    deadline: Instant,
    mut pred: F,
) -> anyhow::Result<Option<Packet>>
where
    F: FnMut(&Packet) -> bool,
{
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        if let Some(p) = game.recv_within(left.min(PUMP)).await? {
            if pred(&p) {
                return Ok(Some(p));
            }
        }
    }
}

/// Template id of the quest row in an `SCQuestContextStarted` / `Updated` body.
fn quest_row_template(p: &Packet) -> Option<u32> {
    if !p.is_sc(sc::SCQuestContextStartedPacket) && !p.is_sc(sc::SCQuestContextUpdatedPacket) {
        return None;
    }
    quest::read_quest_row(&mut p.stream())
        .ok()
        .map(|q| q.template_id)
}

fn started_for(p: &Packet, quest_id: u32) -> bool {
    p.is_sc(sc::SCQuestContextStartedPacket) && quest_row_template(p) == Some(quest_id)
}

fn updated_for(p: &Packet, quest_id: u32) -> bool {
    p.is_sc(sc::SCQuestContextUpdatedPacket) && quest_row_template(p) == Some(quest_id)
}

fn completed_for(p: &Packet, quest_id: u32) -> bool {
    p.is_sc(sc::SCQuestContextCompletedPacket) && p.stream().read_u32().ok() == Some(quest_id)
}

/// Waits (bounded by [`APPEAR_WAIT`]) for an NPC of `template_id` to become visible.
async fn wait_for_npc(
    game: &mut GameClient,
    template_id: u32,
    deadline: Instant,
) -> Result<UnitInfo, StepFailure> {
    let appear = (Instant::now() + APPEAR_WAIT).min(deadline);
    loop {
        if let Some(npc) = game.find_unit(1, template_id) {
            return Ok(npc);
        }
        let left = appear.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(StepFailure::timeout(format!(
                "npc {template_id} did not appear"
            )));
        }
        game.recv_within(left.min(PUMP))
            .await
            .map_err(StepFailure::io)?;
    }
}

/// Waits for any of the step's target ids to become visible.
async fn wait_for_target(
    game: &mut GameClient,
    ids: &[u32],
    avoid: &HashSet<u32>,
    deadline: Instant,
) -> Result<Option<UnitInfo>, StepFailure> {
    loop {
        if let Some(npc) = nearest_target(game, ids, avoid) {
            return Ok(Some(npc));
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        game.recv_within(left.min(PUMP))
            .await
            .map_err(StepFailure::io)?;
    }
}

/// Pump until the objective counter reaches `want`, the quest completes, or the deadline.
async fn wait_progress(
    game: &mut GameClient,
    quest_id: u32,
    objective: Option<u8>,
    want: i32,
    deadline: Instant,
) -> anyhow::Result<bool> {
    loop {
        if quest_completed(game, quest_id) {
            return Ok(true);
        }
        if let Some(index) = objective {
            if objective_count(game, quest_id, index) >= want {
                return Ok(true);
            }
        }
        if quest_failed(game, quest_id) {
            return Ok(false);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(false);
        }
        if let Some(p) = game.recv_within(left.min(PUMP)).await? {
            if objective.is_none() && updated_for(&p, quest_id) {
                return Ok(true);
            }
        }
    }
}

// ----- step implementations -----

async fn step_accept(
    game: &mut GameClient,
    quest_id: u32,
    step: &PlanStep,
    deadline: Instant,
) -> Result<StepOutcomeKind, StepFailure> {
    let npc_template = *step
        .target
        .ids
        .first()
        .ok_or_else(|| StepFailure::unsupported("QuestActConAcceptNpc without a target"))?;
    let pos = step_location(step)?;
    game.teleport(near(pos)).await.map_err(StepFailure::io)?;
    settle(game, deadline).await;
    let npc = wait_for_npc(game, npc_template, deadline).await?;
    game.start_quest(quest_id, npc.obj_id)
        .await
        .map_err(StepFailure::io)?;
    let found = wait_for_packet(game, deadline, |p| {
        started_for(p, quest_id) || p.is_sc(sc::SCErrorMsgPacket)
    })
    .await
    .map_err(StepFailure::io)?;
    match found {
        Some(p) if started_for(&p, quest_id) => Ok(StepOutcomeKind::Ok),
        Some(p) => Err(StepFailure::rejected(quest_id, "start", &p)),
        None => Err(StepFailure::timeout(format!(
            "no SCQuestContextStarted for quest {quest_id}"
        ))),
    }
}

async fn step_talk(
    game: &mut GameClient,
    quest_id: u32,
    step: &PlanStep,
    objective: Option<u8>,
    deadline: Instant,
) -> Result<StepOutcomeKind, StepFailure> {
    let npc_template = *step
        .target
        .ids
        .first()
        .ok_or_else(|| StepFailure::unsupported("QuestActObjTalk without a target"))?;
    let pos = step_location(step)?;
    game.teleport(near(pos)).await.map_err(StepFailure::io)?;
    settle(game, deadline).await;
    let npc = wait_for_npc(game, npc_template, deadline).await?;
    game.quest_talk(quest_id, npc.obj_id, step.component_id, step.act_id)
        .await
        .map_err(StepFailure::io)?;
    let want = step.target.count.max(1);
    let advanced = wait_progress(game, quest_id, objective, want, deadline)
        .await
        .map_err(StepFailure::io)?;
    if advanced || quest_completed(game, quest_id) {
        return Ok(StepOutcomeKind::Ok);
    }
    if quest_failed(game, quest_id) {
        return Err(StepFailure::quest_failed(quest_id));
    }
    Err(StepFailure::timeout(format!(
        "talk act {} did not update quest {quest_id}",
        step.act_id
    )))
}

async fn step_report(
    game: &mut GameClient,
    quest_id: u32,
    step: &PlanStep,
    deadline: Instant,
) -> Result<StepOutcomeKind, StepFailure> {
    let npc_template = *step
        .target
        .ids
        .first()
        .ok_or_else(|| StepFailure::unsupported("QuestActConReportNpc without a target"))?;
    let pos = step_location(step)?;
    game.teleport(near(pos)).await.map_err(StepFailure::io)?;
    settle(game, deadline).await;
    let npc = wait_for_npc(game, npc_template, deadline).await?;
    game.complete_quest(quest_id, npc.obj_id, 0)
        .await
        .map_err(StepFailure::io)?;
    let found = wait_for_packet(game, deadline, |p| {
        completed_for(p, quest_id) || p.is_sc(sc::SCErrorMsgPacket)
    })
    .await
    .map_err(StepFailure::io)?;
    match found {
        Some(p) if completed_for(&p, quest_id) => Ok(StepOutcomeKind::Ok),
        Some(p) => Err(StepFailure::rejected(quest_id, "report", &p)),
        None if quest_completed(game, quest_id) => Ok(StepOutcomeKind::Ok),
        None if quest_failed(game, quest_id) => Err(StepFailure::quest_failed(quest_id)),
        None => Err(StepFailure::timeout(format!(
            "no SCQuestContextCompleted for quest {quest_id}"
        ))),
    }
}

/// Kills the target NPCs with the `/kill` GM command: `kill.rs` credits the bot as the killer
/// (`combat::reduce_current_hp(..., Some(&self_obj), KillReason::Gm)`), which reaches
/// `combat::do_die` and the quest engine's `on_kill` with the victim, so the normal kill
/// objectives count. The bot re-targets with `CSChangeTargetPacket` before every kill.
async fn step_kill(
    game: &mut GameClient,
    quest_id: u32,
    step: &PlanStep,
    objective: Option<u8>,
    deadline: Instant,
) -> Result<StepOutcomeKind, StepFailure> {
    if step.target.ids.is_empty() {
        return Err(StepFailure::unsupported("kill step without target ids"));
    }
    let positions: Vec<(f32, f32, f32)> = step
        .locations
        .iter()
        .filter(|l| l.world.eq_ignore_ascii_case(MAIN_WORLD))
        .map(|l| (l.x, l.y, l.z))
        .collect();
    if positions.is_empty() {
        // Reuse the "other world" / "no location" reason.
        return Err(step_location(step).err().unwrap_or_else(|| {
            StepFailure::unsupported("kill step without a main-world location")
        }));
    }
    let want = step.target.count.max(1);
    let mut avoid: HashSet<u32> = HashSet::new();
    let mut next_location = 0usize;
    loop {
        if quest_completed(game, quest_id) {
            return Ok(StepOutcomeKind::Ok);
        }
        if let Some(index) = objective {
            if objective_count(game, quest_id, index) >= want {
                return Ok(StepOutcomeKind::Ok);
            }
        }
        if quest_failed(game, quest_id) {
            return Err(StepFailure::quest_failed(quest_id));
        }
        if Instant::now() >= deadline {
            return Err(StepFailure::timeout(format!(
                "objective of quest {quest_id} not reached"
            )));
        }
        let npc = match nearest_target(game, &step.target.ids, &avoid) {
            Some(npc) => npc,
            None => {
                let pos = positions[next_location % positions.len()];
                next_location += 1;
                game.teleport(near(pos)).await.map_err(StepFailure::io)?;
                settle(game, deadline).await;
                let appear = (Instant::now() + APPEAR_WAIT).min(deadline);
                match wait_for_target(game, &step.target.ids, &avoid, appear).await? {
                    Some(npc) => npc,
                    None => continue,
                }
            }
        };
        game.change_target(npc.obj_id)
            .await
            .map_err(StepFailure::io)?;
        let messages = game.gm("kill", KILL_WAIT).await.map_err(StepFailure::io)?;
        let rejected = messages
            .iter()
            .any(|m| m.contains("Cannot kill") || m.contains("already dead"));
        avoid.insert(npc.obj_id);
        if rejected {
            continue;
        }
        // Give the batched objective update a moment; the loop re-checks the counter.
        let until = (Instant::now() + Duration::from_secs(8)).min(deadline);
        let mut advanced = false;
        while Instant::now() < until {
            if quest_completed(game, quest_id) || quest_failed(game, quest_id) {
                break;
            }
            if let Some(index) = objective {
                if objective_count(game, quest_id, index) >= want {
                    advanced = true;
                    break;
                }
            }
            let left = until.saturating_duration_since(Instant::now());
            if let Some(p) = game
                .recv_within(left.min(PUMP))
                .await
                .map_err(StepFailure::io)?
            {
                if objective.is_none() && updated_for(&p, quest_id) {
                    advanced = true;
                    break;
                }
            }
        }
        if advanced || quest_completed(game, quest_id) {
            return Ok(StepOutcomeKind::Ok);
        }
        if quest_failed(game, quest_id) {
            return Err(StepFailure::quest_failed(quest_id));
        }
        if objective.is_none() {
            // A `QuestActConAcceptNpcKill` has no objective counter: the kill must advance the
            // quest (step change / completion), otherwise retrying cannot help.
            return Err(StepFailure::timeout(format!(
                "kill did not advance quest {quest_id}"
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(kind: &str, ids: &[u32], count: i32) -> PlanTarget {
        PlanTarget {
            kind: kind.into(),
            ids: ids.to_vec(),
            count,
        }
    }

    fn step(kind: &str, act_type: &str) -> PlanStep {
        PlanStep {
            component_id: 1,
            component_kind: kind.into(),
            act_id: 2,
            act_type: act_type.into(),
            target: PlanTarget::default(),
            locations: Vec::new(),
            status: "ok".into(),
            sources: None,
        }
    }

    fn quest(id: u32, status: &str) -> PlanQuest {
        PlanQuest {
            quest_id: id,
            name: format!("quest {id}"),
            category_id: 9,
            level: Some(2),
            steps: Vec::new(),
            status: status.into(),
            blockers: Vec::new(),
        }
    }

    #[test]
    fn plan_json_deserializes_the_plan_shapes() {
        let json = r#"[
          {
            "quest_id": 10, "name": "Test quest", "category_id": 9, "level": 2,
            "steps": [
              {"component_id": 54, "component_kind": "Start", "act_id": 68,
               "act_type": "QuestActConAcceptNpc",
               "target": {"kind": "npc", "ids": [7708], "count": 1},
               "locations": [{"world": "main_world", "x": 10350.18, "y": 16035.07, "z": 357.6}],
               "status": "ok"}
            ],
            "status": "ok", "blockers": []
          },
          {
            "quest_id": 11, "name": "Dynamic", "category_id": 9, "level": null,
            "steps": [
              {"component_id": 8200, "component_kind": "Progress", "act_id": 11697,
               "act_type": "QuestActObjMonsterHunt",
               "target": {"kind": "npc", "ids": [721], "count": 2, "emotion_id": 5},
               "locations": [], "status": "dynamic_spawn",
               "sources": [{"effect_id": 72, "skill_ids": [10732]},
                           {"effect_id": 569, "skill_ids": []}]}
            ],
            "status": "ok", "blockers": []
          },
          {
            "quest_id": 12, "name": "Blocked", "category_id": 9, "level": 5,
            "steps": [], "status": "blocked",
            "blockers": ["no spawn for npc 999 (QuestActConAcceptNpc, component 1)"]
          }
        ]"#;
        let plan: Vec<PlanQuest> = serde_json::from_str(json).unwrap();
        assert_eq!(plan.len(), 3);

        let q10 = &plan[0];
        assert_eq!(q10.quest_id, 10);
        assert_eq!(q10.name, "Test quest");
        assert_eq!(q10.level, Some(2));
        assert_eq!(q10.status, "ok");
        assert_eq!(q10.steps.len(), 1);
        let s = &q10.steps[0];
        assert_eq!(s.component_kind, "Start");
        assert_eq!(s.target, target("npc", &[7708], 1));
        assert_eq!(s.locations[0].world, "main_world");
        assert_eq!(s.locations[0].z, 357.6);
        assert!(s.sources.is_none());

        let q11 = &plan[1];
        assert_eq!(q11.level, None);
        let s = &q11.steps[0];
        assert_eq!(s.status, "dynamic_spawn");
        // Unknown target members (`emotion_id`) are ignored; `count` survives.
        assert_eq!(s.target, target("npc", &[721], 2));
        let sources = s.sources.as_ref().unwrap();
        assert_eq!(sources[0].effect_id, 72);
        assert_eq!(sources[0].skill_ids, [10732]);
        assert!(sources[1].skill_ids.is_empty());

        let q12 = &plan[2];
        assert_eq!(q12.status, "blocked");
        assert_eq!(q12.blockers.len(), 1);
    }

    #[test]
    fn step_dispatch_covers_every_v1_act_group() {
        let cases = [
            ("Start", "QuestActConAcceptNpc", StepAction::Accept),
            ("Progress", "QuestActObjTalk", StepAction::Talk),
            ("Progress", "QuestActObjMonsterHunt", StepAction::Kill),
            ("Progress", "QuestActObjMonsterGroupHunt", StepAction::Kill),
            ("Start", "QuestActConAcceptNpcKill", StepAction::Kill),
            ("Ready", "QuestActConReportNpc", StepAction::Report),
            ("Ready", "QuestActConAutoComplete", StepAction::Passive),
            ("Reward", "QuestActSupplyItem", StepAction::Passive),
            ("Reward", "QuestActSupplyCopper", StepAction::Passive),
            ("Progress", "QuestActCheckTimer", StepAction::Passive),
            ("Progress", "QuestActObjItemGather", StepAction::Unsupported),
            ("Ready", "QuestActConReportJournal", StepAction::Unsupported),
            ("Fail", "QuestActConAcceptNpc", StepAction::Skip),
            ("Drop", "QuestActObjMonsterHunt", StepAction::Skip),
        ];
        for (kind, act_type, want) in cases {
            assert_eq!(
                step_action(&step(kind, act_type)),
                want,
                "{kind}/{act_type}"
            );
        }
    }

    #[test]
    fn dynamic_sources_win_over_the_act_type() {
        let mut dynamic = step("Progress", "QuestActObjMonsterHunt");
        dynamic.target = target("npc", &[721], 1);
        dynamic.sources = Some(vec![PlanSource {
            effect_id: 72,
            skill_ids: vec![10732],
        }]);
        assert_eq!(step_action(&dynamic), StepAction::NeedsTrigger);

        // Without a skill to trigger, the target cannot be reached either.
        dynamic.sources = Some(vec![PlanSource {
            effect_id: 569,
            skill_ids: Vec::new(),
        }]);
        assert_eq!(step_action(&dynamic), StepAction::Unsupported);

        // A dynamic step in a Fail / Drop branch is still skipped.
        let mut skipped = step("Drop", "QuestActObjMonsterHunt");
        skipped.sources = Some(vec![PlanSource {
            effect_id: 72,
            skill_ids: vec![10732],
        }]);
        assert_eq!(step_action(&skipped), StepAction::Skip);
    }

    #[test]
    fn blocked_plan_quests_are_gated_without_running() {
        assert_eq!(
            plan_gate(&quest(10, "blocked")),
            Some(QuestOutcome::BlockedPlan)
        );
        assert_eq!(plan_gate(&quest(10, "ok")), None);
        assert_eq!(plan_gate(&quest(10, "partial")), None);
    }

    #[test]
    fn objective_indices_match_the_server_component_walk() {
        let steps = vec![
            step("Start", "QuestActConAcceptNpc"),
            step("Progress", "QuestActObjMonsterHunt"),
            step("Progress", "QuestActObjMonsterGroupHunt"),
            step("Progress", "QuestActObjTalk"),
            step("Progress", "QuestActObjItemGather"),
            step("Ready", "QuestActConReportNpc"),
            step("Reward", "QuestActSupplyItem"),
        ];
        assert_eq!(
            objective_indices(&steps),
            vec![None, Some(0), Some(1), Some(2), Some(3), None, None]
        );
        // The counter resets when the component kind changes.
        let steps = vec![
            step("Progress", "QuestActObjMonsterHunt"),
            step("Ready", "QuestActObjTalk"),
        ];
        assert_eq!(objective_indices(&steps), vec![Some(0), Some(0)]);
    }

    #[test]
    fn result_lines_serialize_to_the_documented_json() {
        let result = QuestResult {
            quest_id: 10,
            name: "Test quest".into(),
            result: QuestOutcome::RunnerUnsupported,
            failed_step: Some(FailedStep {
                act_id: 13979,
                act_type: "QuestActObjItemGather".into(),
                reason: "QuestActObjItemGather is not handled by runner v1".into(),
            }),
            steps: vec![
                StepOutcome {
                    act_id: 68,
                    act_type: "QuestActConAcceptNpc".into(),
                    outcome: StepOutcomeKind::Ok,
                    ms: 12,
                },
                StepOutcome {
                    act_id: 13979,
                    act_type: "QuestActObjItemGather".into(),
                    outcome: StepOutcomeKind::Unsupported,
                    ms: 3,
                },
            ],
            final_status: Some(1),
            final_objectives: Some([0, 0, 0, 0, 0]),
            messages: vec!["[Quest] accepted".into()],
            elapsed_ms: 1234,
        };
        assert_eq!(
            serde_json::to_value(&result).unwrap(),
            serde_json::json!({
                "quest_id": 10,
                "name": "Test quest",
                "result": "runner_unsupported",
                "failed_step": {
                    "act_id": 13979,
                    "act_type": "QuestActObjItemGather",
                    "reason": "QuestActObjItemGather is not handled by runner v1"
                },
                "steps": [
                    {"act_id": 68, "act_type": "QuestActConAcceptNpc", "outcome": "ok", "ms": 12},
                    {"act_id": 13979, "act_type": "QuestActObjItemGather",
                     "outcome": "unsupported", "ms": 3}
                ],
                "final_status": 1,
                "final_objectives": [0, 0, 0, 0, 0],
                "messages": ["[Quest] accepted"],
                "elapsed_ms": 1234
            })
        );
        // Null final state and no failed step are valid lines too (blocked / dynamic quests).
        let blocked = missing_quest_result(99);
        let value = serde_json::to_value(&blocked).unwrap();
        assert_eq!(value["result"], "blocked_plan");
        assert_eq!(value["failed_step"], serde_json::json!(null));
        assert_eq!(
            value["messages"],
            serde_json::json!(["quest 99 is not in the plan"])
        );
    }
}
