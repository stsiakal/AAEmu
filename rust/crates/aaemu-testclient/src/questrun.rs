//! Quest-audit runner v1 (`aaemu-testclient quest`): drives one GM bot through a
//! `rust/tools/quest_audit/plan.py` plan, one quest at a time, and appends one JSON result
//! line per quest.
//!
//! The plan lists the quest components / acts in the server's own load order, so the runner
//! can walk them in sequence: accept at the giver NPC, do the objectives (talk / hunt),
//! report, and let the passive reward acts play out. Quest state is read back from
//! [`crate::game::GameState`] (filled by the packet observers); every bounded wait fails the
//! step with `timeout` instead of hanging.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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
/// The accept answer is sent by the `CSStartQuestContextPacket` handler itself, so 5 s is
/// already generous; when it stays empty the accept was silently ignored (see `step_accept`).
const ACCEPT_WAIT: Duration = Duration::from_secs(5);
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
/// The login server's `MaxCharactersPerAccount` (`aaemu-login/src/network/login.rs`).
pub const MAX_CHARACTERS_PER_ACCOUNT: u32 = 6;
/// `CharacterNameRegex` / `NameManager::DEFAULT_CHARACTER_NAME_REGEX` length
/// (`aaemu-game/src/managers/name.rs`).
pub const CHARACTER_NAME_MAX: usize = 18;

/// The playable 1.2 races: id and the C# `Race` enum name (`AAEmu.Game/Models/Game/Char/
/// Race.cs`, mirrored by the `characters` rows of compact.sqlite3).
pub const PLAYABLE_RACES: [(u8, &str); 4] =
    [(1, "nuian"), (4, "elf"), (5, "harani"), (6, "firran")];

/// Every `characters` template race id (`Race` enum), playable or not.
fn race_name(id: u8) -> &'static str {
    match id {
        1 => "nuian",
        2 => "fairy",
        3 => "dwarf",
        4 => "elf",
        5 => "harani",
        6 => "firran",
        7 => "returned",
        8 => "warborn",
        _ => "unknown",
    }
}

/// `--race <name|id>`: the playable 1.2 races only. Fairy / Dwarf / Returned / Warborn have
/// templates in the data but were not playable in 1.2 and are rejected with the reason.
pub fn parse_race(value: &str) -> anyhow::Result<u8> {
    let trimmed = value.trim();
    let id = match trimmed.parse::<u8>() {
        Ok(id) => id,
        Err(_) => match trimmed.to_ascii_lowercase().as_str() {
            "nuian" => 1,
            "elf" => 4,
            "harani" | "hariharan" => 5,
            "firran" | "ferre" => 6,
            "fairy" => 2,
            "dwarf" => 3,
            "returned" => 7,
            "warborn" => 8,
            _ => bail!(
                "unknown race '{value}'; playable races: nuian(1), elf(4), harani(5), firran(6)"
            ),
        },
    };
    if PLAYABLE_RACES.iter().any(|(race, _)| *race == id) {
        return Ok(id);
    }
    bail!(
        "race {id} ({}) is not playable in 1.2; playable races: nuian(1), elf(4), harani(5), firran(6)",
        race_name(id)
    )
}

/// `--gender <m|f>` (`Character.gender`: 1 = male, 2 = female).
pub fn parse_gender(value: &str) -> anyhow::Result<u8> {
    match value.trim().to_ascii_lowercase().as_str() {
        "m" | "male" | "1" => Ok(1),
        "f" | "female" | "2" => Ok(2),
        other => bail!("unknown gender '{other}' (use m or f)"),
    }
}

/// The server's `CharacterNameRegex` (`^[a-zA-Z0-9а-яА-Я]{1,18}$`): ASCII alphanumerics or
/// Cyrillic, 1..=18 characters.
pub fn valid_character_name(name: &str) -> bool {
    !name.is_empty() && name.chars().count() <= CHARACTER_NAME_MAX && name.chars().all(is_name_char)
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || ('а'..='я').contains(&c) || ('А'..='Я').contains(&c)
}

/// `prefix` + `suffix` under the name regex: the prefix is truncated (by characters) so the
/// suffix fits into 18. A prefix with characters the regex rejects is an operator config error
/// and fails instead of being silently cleaned.
pub fn compose_character_name(prefix: &str, suffix: &str) -> anyhow::Result<String> {
    if !valid_character_name(suffix) {
        bail!("generated name suffix '{suffix}' does not match the server's CharacterNameRegex");
    }
    if prefix.chars().any(|c| !is_name_char(c)) {
        bail!(
            "character name prefix '{prefix}' does not match the server's CharacterNameRegex \
             (^[a-zA-Z0-9а-яА-Я]{{1,18}}$)"
        );
    }
    let room = CHARACTER_NAME_MAX - suffix.chars().count();
    let kept: String = prefix.chars().take(room).collect();
    Ok(format!("{kept}{suffix}"))
}

/// A unique `--new-character` name: `prefix` plus a short suffix from the millisecond clock
/// and a process-local counter (hex), so repeated runs and concurrent bots do not collide.
pub fn generated_character_name(
    prefix: &str,
    timestamp_ms: u64,
    sequence: u64,
) -> anyhow::Result<String> {
    let suffix = format!("{:06x}{:02x}", timestamp_ms & 0xff_ffff, sequence & 0xff);
    compose_character_name(prefix, &suffix)
}

/// [`generated_character_name`] with the real clock and counter.
fn new_character_name(prefix: &str) -> anyhow::Result<String> {
    static NAME_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let sequence = NAME_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    generated_character_name(prefix, millis, sequence)
}

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
    /// `unit_reqs` rows of the quest's start components, when the plan carries them.
    #[serde(default)]
    pub requires: Vec<PlanRequirement>,
    /// Preferred level demand (`min_level` wins over `level` when present).
    #[serde(default)]
    pub min_level: Option<u32>,
    /// Quest contexts that must already be completed to accept this one.
    #[serde(default)]
    pub prereq_quests: Vec<u32>,
    #[serde(default)]
    pub steps: Vec<PlanStep>,
    /// `"ok"` / `"partial"` / `"blocked"`.
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub blockers: Vec<String>,
}

/// One `unit_reqs` row of a quest (`systems/quests/unit_reqs.rs`). `kind_id` is the
/// `UnitReqsKindType`; `kind` is its name when the plan provides one. `or_group` marks
/// alternative requirements (any member satisfies the group); `false` / null / absent means no
/// group.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct PlanRequirement {
    #[serde(default)]
    pub kind_id: u32,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub value1: i64,
    #[serde(default)]
    pub value2: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub or_group: Option<serde_json::Value>,
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
    /// The server answered the accept with an error, or stayed silent (v2 answer window).
    Rejected,
    /// A prerequisite quest is neither in the select snapshot nor completed live in this run.
    PrereqMissing,
    /// A `unit_reqs` requirement the runner can check is not met.
    RequirementUnmet,
    BlockedPlan,
    RunnerUnsupported,
    NeedsTrigger,
    Error,
}

impl QuestOutcome {
    /// Every variant, for stable summary counts.
    pub const ALL: [QuestOutcome; 10] = [
        QuestOutcome::Completed,
        QuestOutcome::Failed,
        QuestOutcome::Stuck,
        QuestOutcome::Rejected,
        QuestOutcome::PrereqMissing,
        QuestOutcome::RequirementUnmet,
        QuestOutcome::BlockedPlan,
        QuestOutcome::RunnerUnsupported,
        QuestOutcome::NeedsTrigger,
        QuestOutcome::Error,
    ];

    /// The `snake_case` serialization name.
    pub fn name(self) -> &'static str {
        match self {
            QuestOutcome::Completed => "completed",
            QuestOutcome::Failed => "failed",
            QuestOutcome::Stuck => "stuck",
            QuestOutcome::Rejected => "rejected",
            QuestOutcome::PrereqMissing => "prereq_missing",
            QuestOutcome::RequirementUnmet => "requirement_unmet",
            QuestOutcome::BlockedPlan => "blocked_plan",
            QuestOutcome::RunnerUnsupported => "runner_unsupported",
            QuestOutcome::NeedsTrigger => "needs_trigger",
            QuestOutcome::Error => "error",
        }
    }
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
    /// The server answered the action with an error, or ignored it (accepts).
    Rejected,
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
    /// Character the bot drove (so lines of several bots can be merged).
    pub character: String,
    pub race: u8,
    pub gender: u8,
    /// The character's level when the run started.
    pub start_level: u8,
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
    /// Quest count appended in plan order because prerequisite edges form a cycle.
    pub ordering_cyclic: usize,
}

/// CLI options of the `quest` command.
#[derive(Debug, Clone)]
pub struct RunOptions {
    pub all_ok: bool,
    /// Pull prerequisite chains of the requested quests into the run (`--with-prereqs`).
    pub with_prereqs: bool,
    /// Create a fresh character instead of selecting an existing one (`--new-character`).
    pub new_character: bool,
    /// Race for character creation (`parse_race`); ignored when an existing character is used.
    pub race: u8,
    /// Gender for character creation (`parse_gender`); ignored for existing characters.
    pub gender: u8,
    pub step_timeout: Duration,
    pub out: Option<PathBuf>,
}

/// The character a run drives: recorded in every result line and used for the requirement
/// facts (race / gender / starting level come from the server's own character data).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunCharacter {
    pub name: String,
    pub race: u8,
    pub gender: u8,
    /// The level at character creation / select, before any `/level` of this run.
    pub start_level: u8,
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

// ----- prerequisites and unit requirements -----

/// Prerequisite quest ids that are neither in the character-select snapshot nor completed live
/// in this run. [`GameState::completed_quests`] is exactly that union (the select
/// `SCCompletedQuestsPacket` ids plus the live `SCQuestContextCompletedPacket` ids).
pub fn missing_prerequisites(prereq_quests: &[u32], completed: &BTreeSet<u32>) -> Vec<u32> {
    prereq_quests
        .iter()
        .copied()
        .filter(|p| !completed.contains(p))
        .collect()
}

/// What the runner knows about the character when it checks `unit_reqs`.
pub struct KnownFacts<'a> {
    /// Highest level the runner ensured, a lower bound of the real level.
    pub level: u32,
    /// Race / gender when the bot created the character (`None` = unknown, attempt anyway).
    pub race: Option<u8>,
    pub gender: Option<u8>,
    /// Completed quests (select snapshot + live completions).
    pub completed: &'a BTreeSet<u32>,
}

/// The result of checking one `unit_reqs` entry; `Unknown` never blocks (the server decides).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReqState {
    Met,
    Unmet,
    Unknown,
}

/// `UnitReqsKindType` ids the runner can check (`systems/quests/unit_reqs.rs::req_kind`).
const REQ_LEVEL: u32 = 0x01;
const REQ_RACE: u32 = 0x03;
const REQ_GENDER: u32 = 0x04;
const REQ_COMPLETE_QUEST_CONTEXT: u32 = 0x1F;
const REQ_EXCEPT_COMPLETE_QUEST_CONTEXT: u32 = 0x24;
const REQ_MAX_LEVEL: u32 = 0x45;

/// The `kind` name without case, spaces or underscores (`"CompleteQuestContext"` ->
/// `"completequestcontext"`).
fn kind_key(kind: &str) -> String {
    kind.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// The level the quest demands: `min_level` when present, else `level`, plus any `Level`
/// requirement value (the runner can satisfy those by raising).
pub fn level_demand(quest: &PlanQuest) -> u32 {
    let mut level = quest.min_level.or(quest.level).unwrap_or(0);
    for req in &quest.requires {
        if (req.kind_id == REQ_LEVEL || kind_key(&req.kind) == "level") && req.value1 > 0 {
            level = level.max(req.value1.min(i64::from(u32::MAX)) as u32);
        }
    }
    level
}

/// Checks one requirement against the facts. Level uses `known >= value1` (and the `value2`
/// cap as an upper bound); race / gender only when the bot created the character (Nuian male);
/// quest contexts use the completed set.
pub fn requirement_state(req: &PlanRequirement, facts: &KnownFacts<'_>) -> ReqState {
    let key = kind_key(&req.kind);
    let (v1, v2) = (req.value1, req.value2);
    if req.kind_id == REQ_LEVEL || key == "level" {
        return if v2 > 0 && i64::from(facts.level) > v2 {
            ReqState::Unmet
        } else if i64::from(facts.level) >= v1 && v2 <= 0 {
            ReqState::Met
        } else {
            ReqState::Unknown
        };
    }
    if req.kind_id == REQ_MAX_LEVEL || key == "maxlevel" {
        return if i64::from(facts.level) > v1 {
            ReqState::Unmet
        } else {
            ReqState::Unknown
        };
    }
    if req.kind_id == REQ_RACE || key == "race" {
        return match facts.race {
            Some(race) if i64::from(race) == v1 => ReqState::Met,
            Some(_) => ReqState::Unmet,
            None => ReqState::Unknown,
        };
    }
    if req.kind_id == REQ_GENDER || key == "gender" {
        return match facts.gender {
            Some(gender) if i64::from(gender) == v1 => ReqState::Met,
            Some(_) => ReqState::Unmet,
            None => ReqState::Unknown,
        };
    }
    if req.kind_id == REQ_COMPLETE_QUEST_CONTEXT || key == "completequestcontext" {
        let Some(id) = u32::try_from(v1).ok() else {
            return ReqState::Unknown;
        };
        return if facts.completed.contains(&id) {
            ReqState::Met
        } else {
            ReqState::Unmet
        };
    }
    if req.kind_id == REQ_EXCEPT_COMPLETE_QUEST_CONTEXT || key == "exceptcompletequestcontext" {
        let Some(id) = u32::try_from(v1).ok() else {
            return ReqState::Unknown;
        };
        return if facts.completed.contains(&id) {
            ReqState::Unmet
        } else {
            ReqState::Met
        };
    }
    ReqState::Unknown
}

/// A stable key for `or_group`: absent / null / `false` = no group, any other JSON value groups
/// by its text (so a boolean `true` or an integer both work).
fn or_group_key(group: Option<&serde_json::Value>) -> Option<String> {
    match group {
        None | Some(serde_json::Value::Null) | Some(serde_json::Value::Bool(false)) => None,
        Some(v) => Some(v.to_string()),
    }
}

fn requirement_reason(req: &PlanRequirement) -> String {
    let kind = if req.kind.is_empty() {
        format!("kind_id {}", req.kind_id)
    } else {
        req.kind.clone()
    };
    format!(
        "requirement {kind} (value1 {}, value2 {}) not met",
        req.value1, req.value2
    )
}

/// Checks every `unit_reqs` entry the runner can decide. Entries it cannot decide never block
/// (the accept attempt then lets the server decide; an unanswerable requirement shows up as a
/// `rejected` result with the silent-server reason). `or_group` entries only block when every
/// alternative in the group is checkably unmet.
pub fn requirement_gate(quest: &PlanQuest, facts: &KnownFacts<'_>) -> Option<String> {
    let mut groups: BTreeMap<String, Vec<&PlanRequirement>> = BTreeMap::new();
    for req in &quest.requires {
        match or_group_key(req.or_group.as_ref()) {
            None => {
                if requirement_state(req, facts) == ReqState::Unmet {
                    return Some(requirement_reason(req));
                }
            }
            Some(key) => groups.entry(key).or_default().push(req),
        }
    }
    for (key, reqs) in groups {
        if reqs
            .iter()
            .all(|r| requirement_state(r, facts) == ReqState::Unmet)
        {
            let alternatives: Vec<String> = reqs.iter().map(|r| requirement_reason(r)).collect();
            return Some(format!(
                "or_group {key}: no alternative met ({})",
                alternatives.join("; ")
            ));
        }
    }
    None
}

/// A stable topological order of the selected plan indices: prerequisites first, ties by plan
/// order (independent of the order `selection` is given in). `cyclic` is the number of quests
/// appended in plan order because prerequisite edges form a cycle.
pub struct TopoOrder {
    pub order: Vec<usize>,
    pub cyclic: usize,
}

/// Stable topological order over the plan indices in `selection`. The input order does not
/// matter: ready quests are always taken in plan order, so explicit `--quest` lists and
/// `--all-ok` produce the same order for the same set.
pub fn topological_order(plan: &[PlanQuest], selection: &[usize]) -> TopoOrder {
    let mut selected: Vec<usize> = selection.to_vec();
    selected.sort_unstable();
    selected.dedup();
    let position: BTreeMap<u32, usize> = selected.iter().map(|&i| (plan[i].quest_id, i)).collect();
    let mut in_degree = vec![0usize; plan.len()];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); plan.len()];
    for &i in &selected {
        for prereq in &plan[i].prereq_quests {
            if let Some(&q) = position.get(prereq) {
                if q != i {
                    in_degree[i] += 1;
                    dependents[q].push(i);
                }
            }
        }
    }
    let mut ready = BinaryHeap::new();
    for &i in &selected {
        if in_degree[i] == 0 {
            ready.push(Reverse(i));
        }
    }
    let mut order = Vec::with_capacity(selected.len());
    while let Some(Reverse(i)) = ready.pop() {
        order.push(i);
        for &next in &dependents[i] {
            in_degree[next] -= 1;
            if in_degree[next] == 0 {
                ready.push(Reverse(next));
            }
        }
    }
    let mut cyclic = 0;
    if order.len() < selected.len() {
        // Cycle: append the remaining quests in plan order and note it.
        let done: HashSet<usize> = order.iter().copied().collect();
        for &i in &selected {
            if !done.contains(&i) {
                order.push(i);
                cyclic += 1;
            }
        }
    }
    TopoOrder { order, cyclic }
}

/// `--with-prereqs`: the selected plan indices plus every prerequisite of theirs that exists in
/// the plan, recursively. The result is sorted by plan index (the topological run order is
/// computed afterwards); prerequisites outside the plan cannot be pulled.
pub fn expand_prerequisites(plan: &[PlanQuest], selection: &[usize]) -> Vec<usize> {
    let by_id: BTreeMap<u32, usize> = plan
        .iter()
        .enumerate()
        .map(|(i, q)| (q.quest_id, i))
        .collect();
    let mut seen: HashSet<usize> = selection.iter().copied().collect();
    let mut queue: Vec<usize> = selection.to_vec();
    while let Some(i) = queue.pop() {
        for prereq in &plan[i].prereq_quests {
            if let Some(&j) = by_id.get(prereq) {
                if seen.insert(j) {
                    queue.push(j);
                }
            }
        }
    }
    let mut out: Vec<usize> = seen.into_iter().collect();
    out.sort_unstable();
    out
}

// ----- summary -----

/// `--summary`: counts by result and the top failure reasons (digit runs collapsed).
pub fn render_summary(report: &RunReport) -> String {
    let mut out = String::from("[quest] results:");
    for outcome in QuestOutcome::ALL {
        let n = report
            .results
            .iter()
            .filter(|r| r.result == outcome)
            .count();
        if n > 0 {
            out.push_str(&format!(" {}={n}", outcome.name()));
        }
    }
    let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
    for r in &report.results {
        if r.result == QuestOutcome::Completed {
            continue;
        }
        let reason = r
            .failed_step
            .as_ref()
            .map_or_else(|| r.result.name().to_string(), |f| f.reason.clone());
        *reasons.entry(reason_key(&reason)).or_default() += 1;
    }
    let mut top: Vec<(&String, &usize)> = reasons.iter().collect();
    top.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    if !top.is_empty() {
        out.push_str("\n[quest] top failure reasons:");
        for (reason, n) in top.iter().take(5) {
            out.push_str(&format!("\n  {n}x {reason}"));
        }
    }
    if report.ordering_cyclic > 0 {
        out.push_str(&format!(
            "\n[quest] prerequisite cycles: kept plan order for {} quest(s)",
            report.ordering_cyclic
        ));
    }
    out
}

/// Collapses digit runs so per-quest reasons (`quest 63`, `quest 61`) count together.
fn reason_key(reason: &str) -> String {
    let mut out = String::with_capacity(reason.len());
    let mut digit_run = false;
    for c in reason.chars() {
        if c.is_ascii_digit() {
            if !digit_run {
                out.push('#');
            }
            digit_run = true;
        } else {
            out.push(c);
            digit_run = false;
        }
    }
    out
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
/// given, is appended to `opts.out`.
pub async fn run(
    cfg: &BotConfig,
    plan: &[PlanQuest],
    requested: &[u32],
    opts: &RunOptions,
) -> anyhow::Result<RunReport> {
    let ordering_cyclic: usize;
    let selected: Vec<Selected> = if !requested.is_empty() {
        // Explicit lists run in the same stable prerequisite order as `--all-ok`:
        // resolve the ids, optionally pull the prerequisite chain, then sort.
        let mut seen = HashSet::new();
        let mut known = Vec::new();
        let mut unknown = Vec::new();
        for id in requested {
            if !seen.insert(*id) {
                continue;
            }
            match plan.iter().position(|q| q.quest_id == *id) {
                Some(i) => known.push(i),
                None => unknown.push(*id),
            }
        }
        let known = if opts.with_prereqs {
            expand_prerequisites(plan, &known)
        } else {
            known
        };
        let topo = topological_order(plan, &known);
        ordering_cyclic = topo.cyclic;
        topo.order
            .into_iter()
            .map(|i| Selected::Plan(&plan[i]))
            .chain(unknown.into_iter().map(Selected::Unknown))
            .collect()
    } else if opts.all_ok {
        // Prerequisites first: stable topological order over the plan order.
        let selected_indices: Vec<usize> = plan
            .iter()
            .enumerate()
            .filter(|(_, q)| q.status == "ok")
            .map(|(i, _)| i)
            .collect();
        let topo = topological_order(plan, &selected_indices);
        ordering_cyclic = topo.cyclic;
        topo.order
            .into_iter()
            .map(|i| Selected::Plan(&plan[i]))
            .collect()
    } else {
        bail!("the quest command needs --quest <id> (repeatable) and/or --all-ok");
    };
    if ordering_cyclic > 0 {
        tracing::warn!("prerequisite cycles: kept plan order for {ordering_cyclic} quest(s)");
    }

    let mut writer = match &opts.out {
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
    let (mut game, character) = setup_world(cfg, &rec, opts).await?;
    // God mode keeps the bot alive while it stands in the spawns; `/level` is issued per quest.
    game.gm("godmode true", Duration::from_secs(1)).await.ok();

    let mut raised_level = u32::from(character.start_level).max(1);
    let mut report = RunReport {
        ordering_cyclic,
        ..RunReport::default()
    };
    for target in selected {
        if game.state.disconnected {
            // The connection dropped between two quests: still write the error line.
            let result = match target {
                Selected::Plan(quest) => disconnected_result(quest, &character),
                Selected::Unknown(id) => missing_quest_result(id, &character),
            };
            emit(&mut writer, &result)?;
            report.results.push(result);
            report.aborted = true;
            break;
        }
        let result = match target {
            Selected::Plan(quest) => {
                run_quest(
                    &mut game,
                    quest,
                    &character,
                    &mut raised_level,
                    opts.step_timeout,
                )
                .await
            }
            Selected::Unknown(id) => missing_quest_result(id, &character),
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

fn missing_quest_result(id: u32, character: &RunCharacter) -> QuestResult {
    QuestResult {
        quest_id: id,
        name: String::new(),
        character: character.name.clone(),
        race: character.race,
        gender: character.gender,
        start_level: character.start_level,
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
fn disconnected_result(quest: &PlanQuest, character: &RunCharacter) -> QuestResult {
    QuestResult {
        quest_id: quest.quest_id,
        name: quest.name.clone(),
        character: character.name.clone(),
        race: character.race,
        gender: character.gender,
        start_level: character.start_level,
        result: QuestOutcome::Error,
        failed_step: None,
        steps: Vec::new(),
        final_status: None,
        final_objectives: None,
        messages: vec!["connection lost".into()],
        elapsed_ms: 0,
    }
}

/// The `failed_step` of a quest-level gate (`prereq_missing`, `requirement_unmet`): no act
/// ran, so `act_id` / `act_type` stay empty and the reason carries the detail.
fn gate_failed_step(reason: String) -> FailedStep {
    FailedStep {
        act_id: 0,
        act_type: String::new(),
        reason,
    }
}

/// `ErrorMessageType` name of the errors the quest code can answer an accept with
/// (`systems/quests/engine.rs` `send_error`), else the raw code.
pub fn error_reason(code: i16) -> String {
    match code {
        315 => "BackpackOccupied".into(), // engine.rs `can_accept_supply_items`
        621 => "QuestDailyLimit".into(),  // engine.rs already completed, not repeatable
        c => format!("SCErrorMsg {c}"),
    }
}

/// `CharacterCreateError` reason names (`aaemu-game/src/managers/name.rs`).
fn creation_error_reason(reason: u8) -> &'static str {
    match reason {
        3 => "the name already exists or is pending deletion",
        4 => "the name already exists",
        5 => "the name does not match the server's CharacterNameRegex",
        _ => "unknown failure",
    }
}

/// Sends `CSCreateCharacterPacket` and waits for the answer; a failure is an operator error.
async fn create_and_wait(
    game: &mut GameClient,
    name: &str,
    race: u8,
    gender: u8,
) -> anyhow::Result<()> {
    game.create_character_as(name, race, gender).await?;
    let p = game
        .wait_for("character creation result", |p| {
            p.is_sc(sc::SCCreateCharacterResponsePacket)
                || p.is_sc(sc::SCCharacterCreationFailedPacket)
        })
        .await?;
    if p.is_sc(sc::SCCharacterCreationFailedPacket) {
        let reason = p.body.first().copied().unwrap_or(0);
        bail!(
            "character creation failed: {} (name {name:?})",
            creation_error_reason(reason)
        );
    }
    Ok(())
}

/// Character creation / selection like the `full` scenario: login, lobby burst, character
/// (created for `--new-character` or when the account is empty), select, spawn and notify.
/// The returned [`RunCharacter`] is the selected character's race / gender / level as the
/// server reports them.
async fn setup_world(
    cfg: &BotConfig,
    rec: &Recorder,
    opts: &RunOptions,
) -> anyhow::Result<(GameClient, RunCharacter)> {
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

    let listed = game.state.character_count;
    let mut character: Option<RunCharacter> = None;
    if opts.new_character {
        if listed >= MAX_CHARACTERS_PER_ACCOUNT {
            bail!(
                "the account already has {listed} characters; the login server allows \
                 {MAX_CHARACTERS_PER_ACCOUNT} per account (MaxCharactersPerAccount). Delete a \
                 character from the character-select screen before using --new-character; this \
                 runner never deletes characters."
            );
        }
        let name = new_character_name(&cfg.character)?;
        create_and_wait(&mut game, &name, opts.race, opts.gender).await?;
        character = Some(RunCharacter {
            name,
            race: opts.race,
            gender: opts.gender,
            start_level: 1,
        });
    } else if listed == 0 {
        // The account is empty: create the configured character; `--race` / `--gender` apply.
        if !valid_character_name(&cfg.character) {
            bail!(
                "config character '{}' does not match the server's CharacterNameRegex \
                 (^[a-zA-Z0-9а-яА-Я]{{1,18}}$)",
                cfg.character
            );
        }
        create_and_wait(&mut game, &cfg.character, opts.race, opts.gender).await?;
        character = Some(RunCharacter {
            name: cfg.character.clone(),
            race: opts.race,
            gender: opts.gender,
            start_level: 1,
        });
    }

    let target = match &character {
        Some(created) => game
            .state
            .characters
            .iter()
            .find(|c| c.name == created.name)
            .cloned()
            .with_context(|| format!("created character {:?} is not in the list", created.name))?,
        None => game
            .state
            .characters
            .first()
            .cloned()
            .context("the account has no character to select")?,
    };
    let mut character = character.unwrap_or_else(|| RunCharacter {
        name: target.name.clone(),
        race: target.race,
        gender: target.gender,
        start_level: target.level,
    });
    game.select_character(target.id).await?;
    game.wait_sc(sc::SCCharacterStatePacket).await?;
    // The select snapshot is authoritative for race / gender / level.
    if let Some(info) = game.state.self_info.clone() {
        character = RunCharacter {
            name: info.name,
            race: info.race,
            gender: info.gender,
            start_level: info.level,
        };
    }
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
    Ok((game, character))
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
    character: &RunCharacter,
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
        return result_line(
            game, quest, character, gate, None, steps, last_info, chat_start, start,
        );
    }

    // Prerequisites first: a quest whose prerequisite is neither in the select snapshot nor
    // completed live is skipped without touching the server.
    let missing = missing_prerequisites(&quest.prereq_quests, &game.state.completed_quests);
    if !missing.is_empty() {
        let reason = format!(
            "prerequisite quests not completed: {}",
            missing
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
        return result_line(
            game,
            quest,
            character,
            QuestOutcome::PrereqMissing,
            Some(gate_failed_step(reason)),
            steps,
            last_info,
            chat_start,
            start,
        );
    }

    // Meet the quest's level demand (`min_level` when present, else `level`; `Level`
    // requirements included); `/level` only adds the missing experience, never lowers.
    let demand = level_demand(quest);
    if demand >= 2 && demand > *raised_level {
        game.gm(&format!("level {demand}"), Duration::from_secs(2))
            .await
            .ok();
        *raised_level = demand;
    }

    // `unit_reqs` the runner can decide (race / gender / level from the selected character,
    // quest contexts). Unknown kinds do not block: the accept attempt lets the server decide
    // and a silent rejection shows up as `rejected` / "no answer from server".
    let facts = KnownFacts {
        level: (*raised_level).max(u32::from(character.start_level)),
        race: Some(character.race),
        gender: Some(character.gender),
        completed: &game.state.completed_quests,
    };
    if let Some(reason) = requirement_gate(quest, &facts) {
        return result_line(
            game,
            quest,
            character,
            QuestOutcome::RequirementUnmet,
            Some(gate_failed_step(reason)),
            steps,
            last_info,
            chat_start,
            start,
        );
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
        character,
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
    character: &RunCharacter,
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
        character: character.name.clone(),
        race: character.race,
        gender: character.gender,
        start_level: character.start_level,
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
    // The `CSStartQuestContextPacket` handler answers synchronously, with `SCErrorMsgPacket`
    // for an already-completed (non-repeatable) quest or a full backpack and *silence* for a
    // duplicate / invalid id / failed start-component requirement (`engine.rs::add_quest`).
    // `SCQuestUnitReqFailedPacket` exists in the 1.2 opcode table but no server sends it, so the
    // only answers are started / SCErrorMsg / nothing.
    let answer_deadline = (Instant::now() + ACCEPT_WAIT).min(deadline);
    let found = wait_for_packet(game, answer_deadline, |p| {
        started_for(p, quest_id) || p.is_sc(sc::SCErrorMsgPacket)
    })
    .await
    .map_err(StepFailure::io)?;
    match found {
        Some(p) if started_for(&p, quest_id) => Ok(StepOutcomeKind::Ok),
        Some(p) => {
            let code = p.stream().read_i16().unwrap_or(0);
            Err(StepFailure::plan(
                QuestOutcome::Rejected,
                StepOutcomeKind::Rejected,
                format!("accept rejected: {}", error_reason(code)),
            ))
        }
        None => Err(StepFailure::plan(
            QuestOutcome::Rejected,
            StepOutcomeKind::Rejected,
            "no answer from server (accept silently ignored; see engine.rs::add_quest)",
        )),
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
            requires: Vec::new(),
            min_level: None,
            prereq_quests: Vec::new(),
            steps: Vec::new(),
            status: status.into(),
            blockers: Vec::new(),
        }
    }

    fn requirement(kind_id: u32, kind: &str, value1: i64, value2: i64) -> PlanRequirement {
        PlanRequirement {
            kind_id,
            kind: kind.into(),
            value1,
            value2,
            or_group: None,
        }
    }

    fn quest_with(
        prereq_quests: &[u32],
        requires: Vec<PlanRequirement>,
        min_level: Option<u32>,
    ) -> PlanQuest {
        PlanQuest {
            requires,
            min_level,
            prereq_quests: prereq_quests.to_vec(),
            ..quest(10, "ok")
        }
    }

    fn line(id: u32, result: QuestOutcome, reason: Option<&str>) -> QuestResult {
        QuestResult {
            quest_id: id,
            name: format!("quest {id}"),
            character: "Tester".into(),
            race: 1,
            gender: 1,
            start_level: 1,
            result,
            failed_step: reason.map(|r| FailedStep {
                act_id: 0,
                act_type: String::new(),
                reason: r.into(),
            }),
            steps: Vec::new(),
            final_status: None,
            final_objectives: None,
            messages: Vec::new(),
            elapsed_ms: 0,
        }
    }

    #[test]
    fn plan_json_deserializes_the_plan_shapes() {
        let json = r#"[
          {
            "quest_id": 10, "name": "Test quest", "category_id": 9, "level": 2,
            "requires": [
              {"kind_id": 1, "kind": "Level", "value1": 2, "value2": 0},
              {"kind_id": 3, "kind": "Race", "value1": 1, "or_group": 1},
              {"kind_id": 42, "value1": 7, "or_group": true}
            ],
            "min_level": 2,
            "prereq_quests": [5, 6],
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
        assert_eq!(q10.min_level, Some(2));
        assert_eq!(q10.prereq_quests, [5, 6]);
        assert_eq!(q10.status, "ok");
        assert_eq!(q10.requires.len(), 3);
        assert_eq!(q10.requires[0], requirement(1, "Level", 2, 0));
        assert_eq!(q10.requires[1].or_group, Some(serde_json::json!(1)));
        // Missing kind / kind_id fall back to the defaults; a boolean group is kept.
        assert_eq!(q10.requires[2].kind, "");
        assert_eq!(q10.requires[2].kind_id, 42);
        assert_eq!(q10.requires[2].value1, 7);
        assert_eq!(q10.requires[2].or_group, Some(serde_json::json!(true)));
        assert_eq!(q10.steps.len(), 1);
        let s = &q10.steps[0];
        assert_eq!(s.component_kind, "Start");
        assert_eq!(s.target, target("npc", &[7708], 1));
        assert_eq!(s.locations[0].world, "main_world");
        assert_eq!(s.locations[0].z, 357.6);
        assert!(s.sources.is_none());

        // A plan without the new fields still loads (old plan files).
        let q11 = &plan[1];
        assert_eq!(q11.level, None);
        assert!(q11.requires.is_empty());
        assert_eq!(q11.min_level, None);
        assert!(q11.prereq_quests.is_empty());
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
    fn topological_order_puts_prerequisites_first() {
        let mut plan = vec![quest(10, "ok"), quest(11, "ok"), quest(12, "ok")];
        plan[1].prereq_quests = vec![12]; // 11 requires 12
        let topo = topological_order(&plan, &[0, 1, 2]);
        assert_eq!(topo.order, vec![0, 2, 1]);
        assert_eq!(topo.cyclic, 0);

        // Prerequisites outside the selection are ignored; ties keep plan order.
        let topo = topological_order(&plan, &[1]);
        assert_eq!(topo.order, vec![1]);
        assert_eq!(topo.cyclic, 0);
    }

    #[test]
    fn topological_order_falls_back_on_cycles() {
        let mut plan = vec![quest(10, "ok"), quest(11, "ok")];
        plan[0].prereq_quests = vec![11];
        plan[1].prereq_quests = vec![10];
        let topo = topological_order(&plan, &[0, 1]);
        // Cycle: plan order kept and noted.
        assert_eq!(topo.order, vec![0, 1]);
        assert_eq!(topo.cyclic, 2);

        // Only the cycle members fall back; the acyclic part still sorts.
        let mut plan = vec![quest(10, "ok"), quest(11, "ok"), quest(12, "ok")];
        plan[0].prereq_quests = vec![11];
        plan[1].prereq_quests = vec![10];
        let topo = topological_order(&plan, &[0, 1, 2]);
        assert_eq!(topo.order, vec![2, 0, 1]);
        assert_eq!(topo.cyclic, 2);
    }

    #[test]
    fn missing_prerequisites_check_snapshot_and_live_completions() {
        let completed: BTreeSet<u32> = [61u32].into_iter().collect();
        assert_eq!(missing_prerequisites(&[61], &completed), Vec::<u32>::new());
        assert_eq!(missing_prerequisites(&[61, 62], &completed), vec![62]);
        assert_eq!(missing_prerequisites(&[], &completed), Vec::<u32>::new());
    }

    #[test]
    fn level_demand_prefers_min_level_and_counts_level_requirements() {
        let mut quest = quest(10, "ok");
        quest.level = Some(2);
        assert_eq!(level_demand(&quest), 2);
        quest.min_level = Some(14);
        assert_eq!(level_demand(&quest), 14);
        quest.requires.push(requirement(1, "Level", 20, 0));
        assert_eq!(level_demand(&quest), 20);
        // `MaxLevel` is a cap, not a demand.
        quest.requires.push(requirement(0x45, "MaxLevel", 30, 0));
        assert_eq!(level_demand(&quest), 20);
    }

    #[test]
    fn requirement_gate_checks_known_kinds() {
        let completed: BTreeSet<u32> = [61u32].into_iter().collect();
        let facts = KnownFacts {
            level: 14,
            race: Some(1),
            gender: Some(1),
            completed: &completed,
        };

        // Race mismatch blocks; a match passes; an unknown race attempts anyway.
        let q = quest_with(&[], vec![requirement(3, "Race", 2, 0)], None);
        assert!(requirement_gate(&q, &facts).unwrap().contains("Race"));
        let q = quest_with(&[], vec![requirement(3, "Race", 1, 0)], None);
        assert!(requirement_gate(&q, &facts).is_none());
        let unknown = KnownFacts {
            race: None,
            ..facts
        };
        let q = quest_with(&[], vec![requirement(3, "Race", 2, 0)], None);
        assert!(requirement_gate(&q, &unknown).is_none());

        // Completed quest contexts use the snapshot + live set.
        let q = quest_with(
            &[],
            vec![requirement(0x1F, "CompleteQuestContext", 61, 0)],
            None,
        );
        assert!(requirement_gate(&q, &facts).is_none());
        let q = quest_with(
            &[],
            vec![requirement(0x1F, "CompleteQuestContext", 62, 0)],
            None,
        );
        assert!(requirement_gate(&q, &facts)
            .unwrap()
            .contains("CompleteQuestContext"));

        // Kinds the runner cannot decide never block.
        let q = quest_with(&[], vec![requirement(2, "Ability", 1, 50)], None);
        assert!(requirement_gate(&q, &facts).is_none());

        // A level cap the ensured level is already above blocks; one above it stays unknown.
        let q = quest_with(&[], vec![requirement(1, "Level", 1, 10)], None);
        assert!(requirement_gate(&q, &facts).unwrap().contains("Level"));
        let q = quest_with(&[], vec![requirement(1, "Level", 1, 20)], None);
        assert!(requirement_gate(&q, &facts).is_none());
    }

    #[test]
    fn or_group_only_blocks_when_every_alternative_is_unmet() {
        let completed = BTreeSet::new();
        let facts = KnownFacts {
            level: 5,
            race: Some(1),
            gender: Some(1),
            completed: &completed,
        };
        let mut unmet = requirement(3, "Race", 2, 0);
        unmet.or_group = Some(serde_json::json!(1));
        let mut unknown = requirement(2, "Ability", 1, 10);
        unknown.or_group = Some(serde_json::json!(1));
        let q = quest_with(&[], vec![unmet.clone(), unknown], None);
        assert!(requirement_gate(&q, &facts).is_none());

        let mut also_unmet = requirement(0x1F, "CompleteQuestContext", 62, 0);
        also_unmet.or_group = Some(serde_json::json!(1));
        let q = quest_with(&[], vec![unmet, also_unmet], None);
        assert!(requirement_gate(&q, &facts).unwrap().contains("or_group"));

        // `or_group` normalization: absent / null / false = no group.
        assert_eq!(or_group_key(None), None);
        assert_eq!(or_group_key(Some(&serde_json::Value::Null)), None);
        assert_eq!(or_group_key(Some(&serde_json::json!(false))), None);
        assert_eq!(
            or_group_key(Some(&serde_json::json!(true))),
            Some("true".into())
        );
        assert_eq!(or_group_key(Some(&serde_json::json!(3))), Some("3".into()));
    }

    #[test]
    fn error_reason_names_the_known_accept_errors() {
        assert_eq!(error_reason(621), "QuestDailyLimit");
        assert_eq!(error_reason(315), "BackpackOccupied");
        assert_eq!(error_reason(42), "SCErrorMsg 42");
    }

    #[test]
    fn outcome_names_match_the_json_lines() {
        for (outcome, name) in [
            (QuestOutcome::Completed, "completed"),
            (QuestOutcome::Failed, "failed"),
            (QuestOutcome::Stuck, "stuck"),
            (QuestOutcome::Rejected, "rejected"),
            (QuestOutcome::PrereqMissing, "prereq_missing"),
            (QuestOutcome::RequirementUnmet, "requirement_unmet"),
            (QuestOutcome::BlockedPlan, "blocked_plan"),
            (QuestOutcome::RunnerUnsupported, "runner_unsupported"),
            (QuestOutcome::NeedsTrigger, "needs_trigger"),
            (QuestOutcome::Error, "error"),
        ] {
            assert_eq!(outcome.name(), name);
            assert_eq!(
                serde_json::to_value(outcome).unwrap(),
                serde_json::json!(name)
            );
        }
        assert_eq!(
            serde_json::to_value(StepOutcomeKind::Rejected).unwrap(),
            serde_json::json!("rejected")
        );
    }

    #[test]
    fn summary_counts_results_and_top_failure_reasons() {
        let report = RunReport {
            results: vec![
                line(10, QuestOutcome::Completed, None),
                line(11, QuestOutcome::Rejected, Some("no answer from server")),
                line(12, QuestOutcome::Rejected, Some("no answer from server")),
                line(
                    13,
                    QuestOutcome::PrereqMissing,
                    Some("prerequisite quests not completed: 61"),
                ),
                line(
                    14,
                    QuestOutcome::PrereqMissing,
                    Some("prerequisite quests not completed: 62"),
                ),
            ],
            aborted: false,
            ordering_cyclic: 1,
        };
        let text = render_summary(&report);
        assert!(text.contains("completed=1"), "{text}");
        assert!(text.contains("rejected=2"), "{text}");
        assert!(text.contains("prereq_missing=2"), "{text}");
        assert!(
            text.contains("2x prerequisite quests not completed: #"),
            "{text}"
        );
        assert!(text.contains("2x no answer from server"), "{text}");
        assert!(
            text.contains("prerequisite cycles: kept plan order for 1 quest(s)"),
            "{text}"
        );
    }

    #[test]
    fn result_lines_serialize_to_the_documented_json() {
        let result = QuestResult {
            quest_id: 10,
            name: "Test quest".into(),
            character: "AiBot01".into(),
            race: 6,
            gender: 2,
            start_level: 5,
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
                "character": "AiBot01",
                "race": 6,
                "gender": 2,
                "start_level": 5,
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
        let character = RunCharacter {
            name: "Tester".into(),
            race: 4,
            gender: 2,
            start_level: 3,
        };
        let blocked = missing_quest_result(99, &character);
        let value = serde_json::to_value(&blocked).unwrap();
        assert_eq!(value["result"], "blocked_plan");
        assert_eq!(value["character"], "Tester");
        assert_eq!(value["race"], 4);
        assert_eq!(value["start_level"], 3);
        assert_eq!(value["failed_step"], serde_json::json!(null));
        assert_eq!(
            value["messages"],
            serde_json::json!(["quest 99 is not in the plan"])
        );
    }

    #[test]
    fn explicit_lists_order_prerequisites_first_over_plan_order() {
        let mut plan = vec![quest(10, "ok"), quest(11, "ok"), quest(12, "ok")];
        plan[2].prereq_quests = vec![11]; // 12 requires 11
                                          // Requested in reverse; ready quests are taken in plan order.
        let topo = topological_order(&plan, &[2, 1, 0]);
        assert_eq!(topo.order, vec![0, 1, 2]);
        assert_eq!(topo.cyclic, 0);
        // Requested subset keeps the same relative order.
        let topo = topological_order(&plan, &[2, 1]);
        assert_eq!(topo.order, vec![1, 2]);
    }

    #[test]
    fn with_prereqs_pulls_the_chain_recursively() {
        let mut plan = vec![
            quest(10, "ok"),
            quest(11, "ok"),
            quest(12, "ok"),
            quest(13, "ok"),
        ];
        plan[1].prereq_quests = vec![10]; // 11 -> 10
        plan[2].prereq_quests = vec![11]; // 12 -> 11 -> 10
        plan[3].prereq_quests = vec![999]; // outside the plan: cannot be pulled
                                           // Requesting 12 pulls 11 and 10 into the run, ordered before it.
        let expanded = expand_prerequisites(&plan, &[2]);
        assert_eq!(expanded, vec![0, 1, 2]);
        assert_eq!(topological_order(&plan, &expanded).order, vec![0, 1, 2]);
        // Requesting 11 and 12 does not duplicate 11.
        assert_eq!(expand_prerequisites(&plan, &[1, 2]), vec![0, 1, 2]);
        // A prerequisite outside the plan stays missing (later `prereq_missing`).
        assert_eq!(expand_prerequisites(&plan, &[3]), vec![3]);
    }

    #[test]
    fn generated_character_names_match_the_server_regex() {
        assert!(valid_character_name("Tester"));
        assert!(valid_character_name("Тестер")); // Cyrillic
        assert!(valid_character_name("a0Z"));
        assert!(!valid_character_name(""));
        let too_long = "a".repeat(19);
        assert!(!valid_character_name(&too_long));
        assert!(!valid_character_name("bad-name"));
        assert!(!valid_character_name("bad name"));

        let name = generated_character_name("audit", 0x1234_5678_9abc, 5).unwrap();
        assert!(valid_character_name(&name));
        assert!(name.starts_with("audit"));
        assert_eq!(name.chars().count(), 5 + 8);

        // A long prefix is truncated so the suffix still fits into 18 characters.
        let long = generated_character_name("abcdefghijklmnop", 0, 0).unwrap();
        assert!(valid_character_name(&long));
        assert_eq!(long.chars().count(), 18);
        assert_eq!(&long[..10], "abcdefghij");

        // Prefixes the regex rejects are an operator error, not silently cleaned.
        assert!(generated_character_name("bot-1", 0, 0).is_err());
        assert!(valid_character_name(&new_character_name("audit").unwrap()));
    }

    #[test]
    fn race_and_gender_flags_parse() {
        assert_eq!(parse_race("nuian").unwrap(), 1);
        assert_eq!(parse_race("Elf").unwrap(), 4);
        assert_eq!(parse_race("harani").unwrap(), 5);
        assert_eq!(parse_race("hariharan").unwrap(), 5);
        assert_eq!(parse_race("firran").unwrap(), 6);
        assert_eq!(parse_race("6").unwrap(), 6);
        assert_eq!(parse_race("1").unwrap(), 1);
        // Races with templates but not playable in 1.2 are rejected with the reason.
        for (value, name) in [
            ("3", "dwarf"),
            ("dwarf", "dwarf"),
            ("2", "fairy"),
            ("warborn", "warborn"),
            ("returned", "returned"),
        ] {
            let error = parse_race(value).unwrap_err().to_string();
            assert!(error.contains(name), "{error}");
            assert!(error.contains("not playable"), "{error}");
        }
        assert!(parse_race("kitten").is_err());

        assert_eq!(parse_gender("m").unwrap(), 1);
        assert_eq!(parse_gender("MALE").unwrap(), 1);
        assert_eq!(parse_gender("F").unwrap(), 2);
        assert_eq!(parse_gender("2").unwrap(), 2);
        assert!(parse_gender("x").is_err());
    }
}
