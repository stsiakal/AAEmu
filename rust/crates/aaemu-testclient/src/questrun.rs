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
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use aaemu_commons::PacketStream;
use aaemu_game::opcodes::{cs, pp, sc};
use aaemu_game::systems::services::commands::COMMAND_PREFIX;
use aaemu_game::systems::skills::types::{SkillCastTarget, SkillCaster};
use anyhow::{bail, Context};
use futures::future::LocalBoxFuture;
use serde::{Deserialize, Serialize};

use crate::compactdb::{CompactDb, ItemUseSkill};
use crate::craftrun::{self, CraftDb, CraftPlan};
use crate::game::{DoodadInfo, GameClient, GameState, Packet, UnitInfo, LEVEL_PROXY};
use crate::login::LoginClient;
use crate::quest::{self, QuestInfo};
use crate::record::Recorder;
use crate::scenario::BotConfig;
use crate::valuecheck::{QuestWindow, ValueChecker, ValueReport, ValueSnapshot};

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
/// One attempt at one item source (quest supply wait / loot run / doodad use).
const SOURCE_WAIT: Duration = Duration::from_secs(10);
const SOURCE_WINDOW: Duration = Duration::from_secs(20);
/// After looting a corpse, how long to wait for the bag update.
const LOOT_WAIT: Duration = Duration::from_secs(5);
/// How long one item / doodad use gets to move the objective before it is retried.
const USE_WAIT: Duration = Duration::from_secs(8);
/// `plan.py::ITEM_OPEN_DEPTH`: how deep the runner follows nested `item_open` containers.
const ITEM_OPEN_DEPTH: u32 = 3;
/// How long an entrance / exit doodad use gets to answer with `SCLoadInstancePacket` (and the
/// own unit state after the client's `CSInstanceLoadedPacket`).
const WORLD_LOAD_WAIT: Duration = Duration::from_secs(8);
/// Best-effort window for leaving an instance at the end of a quest.
const WORLD_LEAVE_WINDOW: Duration = Duration::from_secs(15);
/// Budget of the pre-quest recovery (normal exit retry + GM recall).
const RECOVERY_WINDOW: Duration = Duration::from_secs(60);
/// The main-world instance in `SCLoadInstancePacket` / `Transform.InstanceId`.
const MAIN_INSTANCE_ID: u32 = 0;
/// The 1.2 skills whose `Return` special (`special_effects.special_effect_type_id = 25`,
/// `value1 = 0`, no `unit_reqs` row) resolves the character's own return point: the GM
/// `/useskill` of one asks the server to recall the character to the main world
/// (`skills/effects/special/movement.rs::return_`: `SCLoadInstancePacket(instance 0)`),
/// which still works when the dungeon's own exit did not move the bot. Verified against the
/// shipped `compact.sqlite3` with the same query the game data loads:
/// `SELECT se.skill_id FROM skill_effects se JOIN effects ef ON ef.id = se.effect_id JOIN`
/// `special_effects sp ON sp.id = ef.actual_id WHERE ef.actual_type = 'SpecialEffect' AND`
/// `sp.special_effect_type_id = 25 AND sp.value1 = 0 AND NOT EXISTS (SELECT 1 FROM unit_reqs`
/// `ur WHERE ur.owner_type = 'Skill' AND ur.owner_id = se.skill_id) ORDER BY se.skill_id`
/// -> 11239, 11572, ... (both `casting_time` 0, no cost). The candidates are tried in order
/// because the recall is resolved from the character's return point.
const GM_RETURN_SKILLS: [u32; 2] = [11_239, 11_572];
/// The `/spawn` fallback window after a hunt / loot step exhausted its deadline.
const GM_SPAWN_WINDOW: Duration = Duration::from_secs(60);
/// The `/item add` fallback window (fresh: the step deadline is already used up).
const GM_ITEM_WINDOW: Duration = Duration::from_secs(15);
/// Quiet / max drain after a step or teleport, to process removals and replies.
const SETTLE_QUIET: Duration = Duration::from_millis(200);
const SETTLE_MAX: Duration = Duration::from_secs(1);
/// Quiet / max drain after the last step, before the result is recorded.
const FINAL_QUIET: Duration = Duration::from_millis(500);
const FINAL_MAX: Duration = Duration::from_secs(2);
/// The main world the bot spawns in; other plan worlds are entered per step (`switch_world`).
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
    /// Present on item steps: how each target item can be obtained (`plan.py::item_sources`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_sources: Option<Vec<PlanItemSources>>,
    /// Present on sphere steps with `--spheres`: the sign-sphere areas of the component.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spheres: Option<Vec<PlanSphere>>,
}

/// One `quest_sign_sphere.g` area of a sphere step (`plan.py --spheres`, world coordinates).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Default)]
pub struct PlanSphere {
    #[serde(default)]
    pub world: String,
    #[serde(default)]
    pub x: f32,
    #[serde(default)]
    pub y: f32,
    #[serde(default)]
    pub z: f32,
    #[serde(default)]
    pub radius: f32,
    #[serde(default)]
    pub zone_id: u32,
    #[serde(default)]
    pub component_id: u32,
}

/// The sources of one target item of an item step.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct PlanItemSources {
    #[serde(default)]
    pub item_id: u32,
    #[serde(default)]
    pub sources: Vec<PlanItemSource>,
}

/// One candidate source of an item (`npc_loot`, `doodad`, `quest_supply`, `craft`, `store`).
/// Only the fields of the source types the runner drives are named; everything is optional so
/// plan growth cannot break loading.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Default)]
pub struct PlanItemSource {
    /// `"npc_loot"` / `"doodad"` / `"quest_supply"` / `"craft"` / `"store"`.
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub npc_ids: Vec<u32>,
    #[serde(default)]
    pub doodad_ids: Vec<u32>,
    #[serde(default)]
    pub locations: Vec<PlanLocation>,
    /// `npc_loot`: the drop table (`loots.loot_pack_id`).
    #[serde(default)]
    pub loot_pack_id: u32,
    /// `npc_loot`: raw `loots.drop_rate` (10_000_000 = 100%).
    #[serde(default)]
    pub chance: i64,
    /// `npc_loot`: `loot_quest_id` when the drop is gated to a quest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loot_quest_id: Option<u32>,
    /// `doodad`: the func type (`DoodadFuncLootItem` / `DoodadFuncLootPack` / `DoodadFuncPurchase`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub func: Option<String>,
    /// `quest_supply`: the supplying act id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub act_id: Option<u32>,
    /// `craft` / `store` candidates (not driven by runner v4).
    #[serde(default)]
    pub craft_ids: Vec<u32>,
    #[serde(default)]
    pub pack_ids: Vec<u32>,
    /// `item_open`: the bundle item to use; its `use_skill_id` opens it and consumes it
    /// (`gain_loot_pack_item_effects.consume_source_item`).
    #[serde(default)]
    pub container_item_id: u32,
    /// `item_open`: the open skill (matches the container's `use_skill_id`; the server rejects
    /// a cast with a different skill, `systems/skills/handlers.rs`).
    #[serde(default)]
    pub skill_id: u32,
    /// `item_open`: `"gain_item"` / `"skill_product"` when the skill grants the item directly
    /// (absent when a `GainLootPackItemEffect` grants `loot_pack_id` instead).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<String>,
    /// `item_open`: the container's own sources (`plan.py::item_sources`, depth <= 3), used to
    /// obtain the next container after the open consumed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_sources: Option<Vec<PlanItemSource>>,
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
    /// The character already completed the (non-repeatable) quest: skipped, not a failure.
    AlreadyCompleted,
    BlockedPlan,
    /// A loaded quest record without a Start component: not acceptable in play.
    Auxiliary,
    /// The quest never started: its Start component has no accept act in the plan and the
    /// server ignored the empty accept (`engine.rs::starter_accepts`).
    NoStarter,
    RunnerUnsupported,
    NeedsTrigger,
    Error,
}

impl QuestOutcome {
    /// Every variant, for stable summary counts.
    pub const ALL: [QuestOutcome; 13] = [
        QuestOutcome::Completed,
        QuestOutcome::Failed,
        QuestOutcome::Stuck,
        QuestOutcome::Rejected,
        QuestOutcome::PrereqMissing,
        QuestOutcome::RequirementUnmet,
        QuestOutcome::AlreadyCompleted,
        QuestOutcome::BlockedPlan,
        QuestOutcome::Auxiliary,
        QuestOutcome::NoStarter,
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
            QuestOutcome::AlreadyCompleted => "already_completed",
            QuestOutcome::BlockedPlan => "blocked_plan",
            QuestOutcome::Auxiliary => "auxiliary",
            QuestOutcome::NoStarter => "no_starter",
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
    /// The step only succeeded after the `/spawn` GM fallback (never silent).
    OkGmSpawn,
    /// The step only succeeded after the `/item add` GM fallback (never silent).
    OkGmItem,
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

/// One plan step's execution record. `note` names the path that succeeded (e.g.
/// `quest_supply`, `npc_loot pack 6396`, `doodad DoodadFuncLootItem`, `gm_spawn 721`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StepOutcome {
    pub act_id: u32,
    pub act_type: String,
    pub outcome: StepOutcomeKind,
    #[serde(default)]
    pub note: String,
    pub ms: u64,
}

/// The step a quest stopped on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FailedStep {
    pub act_id: u32,
    pub act_type: String,
    pub reason: String,
}

/// One GM shortcut taken while a quest ran (`fallbacks` of a result line).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Fallback {
    /// `gm_item`, `gm_spawn`, `gm_teleport` or `forced_complete`.
    pub kind: String,
    /// The item / npc / quest the fallback was for.
    pub target: String,
    /// Why the real path did not work (the concrete source attempts).
    pub reason: String,
}

impl Fallback {
    pub fn new(kind: &str, target: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            target: target.into(),
            reason: reason.into(),
        }
    }
}

/// Whose fault a result is (`blocker` of a result line).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Blocker {
    /// `runner_gap` / `suspect` / `data_gap` / `requirement` / `none`.
    pub class: String,
    pub reason: String,
    /// The code path / data row that explains the class.
    pub code: String,
    /// A `suspect`'s evidence: what was observed (packet / error / result) and what the runner
    /// expected, with the source of the expectation (plan / compact field / quest text).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub evidence: String,
}

/// The blocker classes of a result line. `suspect` means the server behaved differently from
/// what the runner expected: it is **triaged separately** and is only a confirmed server issue
/// when it is reproduced and a retail source (client text, the C# original, period data) says
/// the server is wrong.
pub mod blocker_class {
    pub const NONE: &str = "none";
    pub const RUNNER_GAP: &str = "runner_gap";
    pub const SUSPECT: &str = "suspect";
    pub const DATA_GAP: &str = "data_gap";
    pub const REQUIREMENT: &str = "requirement";
    pub const ALL: [&str; 5] = [NONE, RUNNER_GAP, SUSPECT, DATA_GAP, REQUIREMENT];
}

impl Default for Blocker {
    fn default() -> Self {
        Self {
            class: blocker_class::NONE.into(),
            reason: String::new(),
            code: String::new(),
            evidence: String::new(),
        }
    }
}

impl Blocker {
    pub fn new(class: &str, reason: impl Into<String>, code: impl Into<String>) -> Self {
        Self {
            class: class.into(),
            reason: reason.into(),
            code: code.into(),
            evidence: String::new(),
        }
    }

    /// A `suspect` blocker: the observation, the code path and the evidence (observed vs
    /// expected + source). `evidence` is mandatory here.
    pub fn suspect(
        reason: impl Into<String>,
        code: impl Into<String>,
        evidence: impl Into<String>,
    ) -> Self {
        Self {
            class: blocker_class::SUSPECT.into(),
            reason: reason.into(),
            code: code.into(),
            evidence: evidence.into(),
        }
    }
}

fn is_false(value: &bool) -> bool {
    !*value
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
    /// The level this quest raised the character to (`/level`; never lowered, so the value
    /// only grows across the batch). `None` when the quest ran at the level it found.
    pub level_raised_to: Option<u32>,
    pub result: QuestOutcome,
    pub failed_step: Option<FailedStep>,
    pub steps: Vec<StepOutcome>,
    pub final_status: Option<u8>,
    pub final_objectives: Option<[i32; 5]>,
    pub messages: Vec<String>,
    pub elapsed_ms: u64,
    /// GM shortcuts used for this quest (empty = every path was the real one).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallbacks: Vec<Fallback>,
    /// Why the quest did not run cleanly (always present, `class = none` when it did).
    #[serde(default)]
    pub blocker: Blocker,
    /// The quest was forced to Reward after its real result (`--force-blocked`).
    #[serde(default, skip_serializing_if = "is_false")]
    pub forced_complete: bool,
    /// Prerequisites of this quest that were forced to completion earlier in the run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forced_prereqs: Vec<u32>,
    /// Reward / kill value checks of a completed quest (player-mode design §4): the packets
    /// observed in the quest window against `compact.sqlite3`. Suspects only, never bugs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_checks: Option<ValueReport>,
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
    /// `--compact-db`: the game data for item-use / interaction skill lookups.
    pub compact_db: Option<PathBuf>,
    pub out: Option<PathBuf>,
    /// `--force-blocked`: force a blocked quest to Reward (`/quest add` + `/quest step Reward`)
    /// after its real result so its dependents still run. On by default for `--all-ok`.
    pub force_blocked: bool,
    /// Combat skill used for `QuestActObjAggro` (the bot's own attack, `BotConfig.skill_id`).
    pub combat_skill: u32,
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

/// One failed attempt at one real item source (drives the `blocker` of the result).
#[derive(Debug, Clone)]
struct SourceAttempt {
    source: String,
    reason: String,
    class: &'static str,
    code: &'static str,
    evidence: String,
}

impl SourceAttempt {
    fn new(
        source: impl Into<String>,
        reason: impl Into<String>,
        class: &'static str,
        code: &'static str,
    ) -> Self {
        Self {
            source: source.into(),
            reason: reason.into(),
            class,
            code,
            evidence: String::new(),
        }
    }

    /// A `suspect` attempt: observed vs expected evidence is mandatory.
    fn suspect(
        source: impl Into<String>,
        reason: impl Into<String>,
        code: &'static str,
        evidence: impl Into<String>,
    ) -> Self {
        Self {
            source: source.into(),
            reason: reason.into(),
            class: blocker_class::SUSPECT,
            code,
            evidence: evidence.into(),
        }
    }

    /// `suspect` outranks `runner_gap`, which outranks `data_gap`.
    fn severity(&self) -> u8 {
        match self.class {
            blocker_class::SUSPECT => 3,
            blocker_class::RUNNER_GAP => 2,
            blocker_class::DATA_GAP => 1,
            _ => 0,
        }
    }
}

/// The fallbacks and the blocker hint of one quest run. Every arm that can take a GM shortcut
/// or hit a source gap records it here; the result line is built from it.
#[derive(Debug, Default)]
struct QuestJournal {
    fallbacks: Vec<Fallback>,
    blocker: Option<Blocker>,
}

impl QuestJournal {
    fn fallback(&mut self, kind: &str, target: impl Into<String>, reason: impl Into<String>) {
        self.fallbacks.push(Fallback::new(kind, target, reason));
    }

    fn note_blocker(&mut self, class: &str, reason: impl Into<String>, code: impl Into<String>) {
        if self.blocker.is_none() {
            self.blocker = Some(Blocker::new(class, reason, code));
        }
    }

    fn note_suspect(
        &mut self,
        reason: impl Into<String>,
        code: impl Into<String>,
        evidence: impl Into<String>,
    ) {
        if self.blocker.is_none() {
            self.blocker = Some(Blocker::suspect(reason, code, evidence));
        }
    }

    /// Sets a fully built blocker (the exit failure of [`exit_failure_suspect`]); the first
    /// blocker of the quest wins.
    fn note_suspect_blocker(&mut self, blocker: Blocker) {
        if self.blocker.is_none() {
            self.blocker = Some(blocker);
        }
    }

    /// Sets the blocker from the failed real-source attempts (the worst one wins; every attempt
    /// stays in the reason text; a suspect keeps its evidence).
    fn note_source_attempts(&mut self, attempts: &[SourceAttempt]) {
        let Some(best) = attempts.iter().max_by_key(|a| a.severity()) else {
            return;
        };
        let reason = attempts
            .iter()
            .map(|a| format!("{}: {}", a.source, a.reason))
            .collect::<Vec<_>>()
            .join("; ");
        if self.blocker.is_none() {
            self.blocker = Some(Blocker {
                class: best.class.into(),
                reason,
                code: best.code.into(),
                evidence: best.evidence.clone(),
            });
        }
    }
}

// ----- step dispatch -----

/// What the runner does for a plan step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepAction {
    /// Fail / Drop branch: the completed quest never executes them.
    Skip,
    Accept,
    /// Accept a quest from a doodad (`CSStartQuestContextPacket` doodad field).
    AcceptDoodad,
    Talk,
    Kill,
    /// `QuestActObjAggro`: kill the quest's NPC acceptor with the bot's own skill while it holds
    /// the aggro (the act only counts that NPC and needs an NPC acceptor).
    Aggro,
    Report,
    /// Report a quest at a doodad (`CSCompleteQuestContextPacket` doodad field).
    ReportDoodad,
    /// `QuestActConReportJournal`: report from the journal (object-free
    /// `CSCompleteQuestContextPacket`; the server's automatic report runs the act).
    ReportJournal,
    /// `QuestActObjInteraction`: use the doodad with an interaction skill.
    DoodadInteraction,
    /// `QuestActConAcceptItem`: start the quest by using the starter item.
    AcceptItem,
    /// `QuestActConAcceptComponent`: accept with no world object (the act is always satisfied).
    AcceptComponent,
    /// `QuestActObjItemGather` / `QuestActEtcItemObtain` / `QuestActConAcceptItemGain`.
    Gather,
    /// `QuestActObjItemUse`: use a bag item with its use skill.
    UseItem,
    /// `QuestActObjSphere` / `QuestActCheckSphere`: enter the sign sphere for the objective.
    Sphere,
    /// `QuestActObjLevel`: raise the character to the act's level (`/level`) and let the server
    /// evaluate the objective.
    ObjLevel,
    /// `QuestActConAcceptSphere`: the server starts the quest when the player enters the sphere.
    AcceptSphere,
    /// `QuestActConAutoComplete` / `QuestActSupply*` / `QuestActCheck*`: nothing to send.
    Passive,
    /// A dynamic NPC target (summoned by a `SpawnEffect` skill) for an act the runner cannot
    /// trigger itself; hunts attempt it instead (the item-use step spawns them).
    NeedsTrigger,
    /// Any other act type.
    Unsupported,
}

/// The dispatch decision. Dynamic sources block the NPC acts the runner cannot trigger, but
/// hunts run: their targets are expected to exist after the earlier item-use step, and the
/// respawn / `/spawn` fallback covers the rest.
pub fn step_action(step: &PlanStep) -> StepAction {
    if matches!(step.component_kind.as_str(), "Fail" | "Drop") {
        return StepAction::Skip;
    }
    let dynamic = step
        .sources
        .as_ref()
        .is_some_and(|sources| sources.iter().any(|s| !s.skill_ids.is_empty()));
    match step.act_type.as_str() {
        // `ConAcceptNpcKill` / `ConAcceptNpcEmotion` are start acts accepted at the NPC (the
        // kill act only matches the quest's NPC acceptor; accepting from it is what the server
        // checks in `starter_accepts`). Treating the kill act as a hunt left 61 quests
        // unaccepted in the v8 full run.
        "QuestActConAcceptNpc" | "QuestActConAcceptNpcKill" | "QuestActConAcceptNpcEmotion" => {
            if dynamic {
                StepAction::NeedsTrigger
            } else {
                StepAction::Accept
            }
        }
        "QuestActConAcceptDoodad" => StepAction::AcceptDoodad,
        "QuestActObjTalk" => {
            if dynamic {
                StepAction::NeedsTrigger
            } else {
                StepAction::Talk
            }
        }
        "QuestActObjMonsterHunt" | "QuestActObjMonsterGroupHunt" => StepAction::Kill,
        "QuestActObjAggro" => StepAction::Aggro,
        "QuestActConReportNpc" => {
            if dynamic {
                StepAction::NeedsTrigger
            } else {
                StepAction::Report
            }
        }
        "QuestActConReportDoodad" => StepAction::ReportDoodad,
        "QuestActConReportJournal" => StepAction::ReportJournal,
        "QuestActObjInteraction" => StepAction::DoodadInteraction,
        "QuestActConAcceptItem" => StepAction::AcceptItem,
        "QuestActConAcceptComponent" => StepAction::AcceptComponent,
        "QuestActObjItemGather" | "QuestActEtcItemObtain" | "QuestActConAcceptItemGain" => {
            StepAction::Gather
        }
        "QuestActObjItemUse" => StepAction::UseItem,
        "QuestActObjSphere" | "QuestActCheckSphere" => StepAction::Sphere,
        "QuestActObjLevel" => StepAction::ObjLevel,
        "QuestActConAcceptSphere" => StepAction::AcceptSphere,
        t if t == "QuestActConAutoComplete"
            || t.starts_with("QuestActSupply")
            || t.starts_with("QuestActCheck") =>
        {
            StepAction::Passive
        }
        _ => StepAction::Unsupported,
    }
}

/// A blocked or auxiliary plan quest is recorded without running (`blocked_plan` /
/// `auxiliary`). `auxiliary` quests have no Start component, so they cannot be accepted in
/// play (`plan.py`, `engine.rs::starter_accepts`).
fn plan_gate(quest: &PlanQuest) -> Option<QuestOutcome> {
    match quest.status.as_str() {
        "blocked" => Some(QuestOutcome::BlockedPlan),
        "auxiliary" => Some(QuestOutcome::Auxiliary),
        _ => None,
    }
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
    /// Own faction id (`unit.faction_id` from the select snapshot head, e.g. 101 Nuian) when
    /// known.
    pub faction: Option<u32>,
    /// Own mother faction (`system_factions.mother_id` of `faction`; `None` = not resolved, so
    /// `MotherFaction` stays undecided).
    pub mother_faction: Option<u32>,
    /// Completed quests (select snapshot + live completions).
    pub completed: &'a BTreeSet<u32>,
    /// Item template ids the bag currently holds (`OwnItem`).
    pub owned_items: &'a BTreeSet<u32>,
    /// Active quest template ids (`PreCompleteQuestContext`).
    pub active_quests: &'a BTreeSet<u32>,
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
const REQ_OWN_ITEM: u32 = 0x0A;
const REQ_COMPLETE_QUEST_CONTEXT: u32 = 0x1F;
const REQ_PRE_COMPLETE_QUEST_CONTEXT: u32 = 0x25;
const REQ_EXCEPT_COMPLETE_QUEST_CONTEXT: u32 = 0x24;
const REQ_FACTION_MATCH: u32 = 0x28;
const REQ_MOTHER_FACTION: u32 = 0x2A;
const REQ_FACTION_MATCH_ONLY: u32 = 0x37;
const REQ_MOTHER_FACTION_ONLY: u32 = 0x38;
const REQ_FACTION_MATCH_ONLY_NOT: u32 = 0x3A;
const REQ_MOTHER_FACTION_ONLY_NOT: u32 = 0x3B;
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
/// cap as an upper bound); race / gender / faction only when the bot knows them (a faction the
/// DB cannot resolve stays undecided); quest contexts use the completed set.
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
    if req.kind_id == REQ_FACTION_MATCH
        || key == "factionmatch"
        || req.kind_id == REQ_FACTION_MATCH_ONLY
        || key == "factionmatchonly"
    {
        return match facts.faction {
            Some(faction) if i64::from(faction) == v1 => ReqState::Met,
            Some(_) => ReqState::Unmet,
            None => ReqState::Unknown,
        };
    }
    if req.kind_id == REQ_FACTION_MATCH_ONLY_NOT || key == "factionmatchonlynot" {
        return match facts.faction {
            Some(faction) if i64::from(faction) != v1 => ReqState::Met,
            Some(_) => ReqState::Unmet,
            None => ReqState::Unknown,
        };
    }
    if req.kind_id == REQ_MOTHER_FACTION
        || key == "motherfaction"
        || req.kind_id == REQ_MOTHER_FACTION_ONLY
        || key == "motherfactiononly"
    {
        return match facts.mother_faction {
            Some(mother) if i64::from(mother) == v1 => ReqState::Met,
            Some(_) => ReqState::Unmet,
            None => ReqState::Unknown,
        };
    }
    if req.kind_id == REQ_MOTHER_FACTION_ONLY_NOT || key == "motherfactiononlynot" {
        return match facts.mother_faction {
            Some(mother) if i64::from(mother) != v1 => ReqState::Met,
            Some(_) => ReqState::Unmet,
            None => ReqState::Unknown,
        };
    }
    if req.kind_id == REQ_OWN_ITEM || key == "ownitem" {
        let Some(id) = u32::try_from(v1).ok() else {
            return ReqState::Unknown;
        };
        return if facts.owned_items.contains(&id) {
            ReqState::Met
        } else {
            ReqState::Unmet
        };
    }
    if req.kind_id == REQ_PRE_COMPLETE_QUEST_CONTEXT || key == "precompletequestcontext" {
        let Some(id) = u32::try_from(v1).ok() else {
            return ReqState::Unknown;
        };
        // `unit_reqs.rs`: not completed and still active (in progress).
        return if facts.completed.contains(&id) || !facts.active_quests.contains(&id) {
            ReqState::Unmet
        } else {
            ReqState::Met
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

/// A stable topological order of the selected plan indices: prerequisites first, then by the
/// quest's level demand (`level_demand`, lowest first) and plan order (independent of the order
/// `selection` is given in). Low-level quests run before higher ones so the batch raises the
/// bot's level as late as possible; a prerequisite always runs before its dependents.
/// `cyclic` is the number of quests appended in plan order because prerequisite edges form a
/// cycle.
pub struct TopoOrder {
    pub order: Vec<usize>,
    pub cyclic: usize,
}

/// Stable topological order over the plan indices in `selection`. The input order does not
/// matter: ready quests are always taken by lowest level demand, then plan order, so explicit
/// `--quest` lists and `--all-ok` produce the same order for the same set.
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
    let key = |i: usize| (level_demand(&plan[i]), i);
    let mut ready = BinaryHeap::new();
    for &i in &selected {
        if in_degree[i] == 0 {
            ready.push(Reverse(key(i)));
        }
    }
    let mut order = Vec::with_capacity(selected.len());
    while let Some(Reverse((_, i))) = ready.pop() {
        order.push(i);
        for &next in &dependents[i] {
            in_degree[next] -= 1;
            if in_degree[next] == 0 {
                ready.push(Reverse(key(next)));
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

/// `--summary`: counts by result, blocker class and top reasons, fallback / forced counts.
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
    let mut classes: BTreeMap<&str, usize> = BTreeMap::new();
    for result in &report.results {
        *classes.entry(result.blocker.class.as_str()).or_default() += 1;
    }
    out.push_str("\n[quest] blockers:");
    for class in blocker_class::ALL {
        let n = classes.get(class).copied().unwrap_or(0);
        out.push_str(&format!(" {class}={n}"));
    }
    if classes.get(blocker_class::SUSPECT).copied().unwrap_or(0) > 0 {
        out.push_str(
            "\n[quest] suspects are triaged separately: a suspect is only a confirmed server \
             issue when it is reproduced AND a retail source (client text, the C# original, \
             period data) says the server is wrong; each suspect carries observed-vs-expected \
             evidence",
        );
    }
    out.push_str("\n[quest] top reasons by blocker class:");
    for class in blocker_class::ALL {
        let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
        for result in &report.results {
            if result.blocker.class == class && !result.blocker.reason.is_empty() {
                *reasons
                    .entry(reason_key(&result.blocker.reason))
                    .or_default() += 1;
            }
        }
        let mut top: Vec<(&String, &usize)> = reasons.iter().collect();
        top.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        for (reason, n) in top.iter().take(3) {
            out.push_str(&format!("\n  [{class}] {n}x {reason}"));
        }
    }
    let completed_without = report
        .results
        .iter()
        .filter(|r| r.result == QuestOutcome::Completed && r.fallbacks.is_empty())
        .count();
    let completed_with = report
        .results
        .iter()
        .filter(|r| r.result == QuestOutcome::Completed && !r.fallbacks.is_empty())
        .count();
    let forced = report.results.iter().filter(|r| r.forced_complete).count();
    // Requirements (race / faction / gender / already completed) are "not applicable to this
    // character", counted separately from the forced shortcuts.
    let not_applicable = report
        .results
        .iter()
        .filter(|r| r.blocker.class == blocker_class::REQUIREMENT)
        .count();
    let already_completed = report
        .results
        .iter()
        .filter(|r| r.result == QuestOutcome::AlreadyCompleted)
        .count();
    out.push_str(&format!(
        "\n[quest] completed without fallback={completed_without} with fallbacks={completed_with} \
         forced={forced} not_applicable={not_applicable} already_completed={already_completed}"
    ));
    let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
    for r in &report.results {
        if matches!(
            r.result,
            QuestOutcome::Completed | QuestOutcome::AlreadyCompleted
        ) {
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
    // Validate / open the game data before the login handshake: a bad `--compact-db` path is an
    // operator error, not a run failure.
    let compact_db = open_compact_db(opts)?;
    // The craft tables of the same database for the `craft` item sources (a missing database
    // only makes those sources record `runner_unsupported`).
    let craft_db = craftrun::open_craft_db(opts.compact_db.as_deref()).ok();
    // Value checks (task E) open the same compact database read-only. A checker that cannot
    // load only disables the reward / kill comparisons; the quest run itself is unchanged.
    let value_checker = match compact_db.as_ref() {
        Some(db) => match ValueChecker::open(db.path()) {
            Ok(checker) => Some(checker),
            Err(e) => {
                tracing::warn!("value checks disabled: {e:#}");
                None
            }
        },
        None => None,
    };
    let (mut game, character) = setup_world(cfg, &rec, opts).await?;
    // God mode keeps the bot alive while it stands in the spawns; `/level` is issued per quest.
    game.gm("godmode true", Duration::from_secs(1)).await.ok();

    let mut raised_level = u32::from(character.start_level).max(1);
    let mut world_tracker = WorldTracker::default();
    // Quests forced to completion earlier in the run (`--force-blocked`); later quests record
    // them in `forced_prereqs`.
    let mut forced: BTreeSet<u32> = BTreeSet::new();
    // Item templates the selected quests may still need; the bag keeper never destroys them and
    // destroys the rest when the bag fills up over a long sweep.
    let needed = needed_items(&selected, compact_db.as_ref());
    // Blocker class / reason of every quest already run, so a missing prerequisite can be
    // explained by the prerequisite's own result.
    let mut classes: BTreeMap<u32, (String, String)> = BTreeMap::new();
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
        // Keep the bag usable: destroy stacks no selected quest needs when it is nearly full
        // (a full bag makes loot, quest supplies and `/item add` fail alike).
        if bag_pressure(&game.state) {
            let _ = free_bag_space(
                &mut game,
                &needed,
                Instant::now() + Duration::from_secs(20),
                false,
            )
            .await;
        }
        // Every quest must start in the main world (each step enters the world it targets). A
        // quest whose exit was left behind traps the bot in an instance; recover it (retry the
        // normal exit, then the GM recall) instead of failing every remaining quest on the same
        // blocker, and stop with a clear error when even that cannot get it out.
        let recovery_fallback = match recover_main_world(
            &mut game,
            &mut world_tracker,
            compact_db.as_ref(),
            Instant::now() + RECOVERY_WINDOW,
        )
        .await
        {
            Recovery::MainWorld => None,
            Recovery::GmTeleport {
                skill,
                instance,
                exit_failure,
            } => Some(gm_teleport_fallback(instance, &exit_failure, skill)),
            Recovery::Failed { reason } => {
                let result = match &target {
                    Selected::Plan(quest) => recovery_failed_result(quest, &character, &reason),
                    Selected::Unknown(id) => {
                        let mut missing = missing_quest_result(*id, &character);
                        missing.messages.push(reason.clone());
                        missing
                    }
                };
                classes.insert(result.quest_id, (result.blocker.class.clone(), reason));
                emit(&mut writer, &result)?;
                report.results.push(result);
                report.aborted = true;
                break;
            }
        };
        let mut result = match target {
            Selected::Plan(quest) => {
                run_quest(
                    &mut game,
                    quest,
                    &character,
                    &mut raised_level,
                    &mut world_tracker,
                    compact_db.as_ref(),
                    craft_db.as_ref(),
                    value_checker.as_ref(),
                    opts.step_timeout,
                    opts,
                    &mut forced,
                    &needed,
                    &classes,
                )
                .await
            }
            Selected::Unknown(id) => missing_quest_result(id, &character),
        };
        if let Some(fallback) = recovery_fallback {
            result.fallbacks.insert(0, fallback);
        }
        // Record the result for later prerequisite explanations. A requirement result that is
        // itself only a prerequisite chain is reduced to its root requirement, so dependent
        // lines do not nest ("prerequisite 61 (requirement): prerequisite 62 (requirement): ...").
        let mut class_reason = result.blocker.reason.clone();
        if result.blocker.class == blocker_class::REQUIREMENT {
            if let Some(index) = class_reason.rfind("requirement ") {
                class_reason = class_reason[index..].to_string();
            }
        }
        classes.insert(
            result.quest_id,
            (result.blocker.class.clone(), class_reason),
        );
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

/// Every item template the selected quests may need: step targets, the items of their
/// `item_sources` (containers included), `OwnItem` requirements and the reagents of every use
/// skill those items carry (the bag keeper must never destroy a reagent mid-sweep).
fn needed_items(selected: &[Selected<'_>], compact_db: Option<&CompactDb>) -> BTreeSet<u32> {
    fn walk(source: &PlanItemSource, out: &mut BTreeSet<u32>) {
        if source.container_item_id != 0 {
            out.insert(source.container_item_id);
        }
        for child in source.container_sources.iter().flatten() {
            walk(child, out);
        }
    }
    let mut out = BTreeSet::new();
    let add_use_reagents = |item_id: u32, out: &mut BTreeSet<u32>| {
        let Some(db) = compact_db else {
            return;
        };
        let Ok(ItemUseSkill::Known { skill_id, .. }) = db.item_use_skill(item_id) else {
            return;
        };
        for (reagent, _) in db.skill_reagents(skill_id).unwrap_or_default() {
            out.insert(reagent);
        }
    };
    for target in selected {
        let Selected::Plan(quest) = target else {
            continue;
        };
        for step in &quest.steps {
            for item_id in &step.target.ids {
                out.insert(*item_id);
                add_use_reagents(*item_id, &mut out);
            }
            for entry in step.item_sources.iter().flatten() {
                out.insert(entry.item_id);
                for source in &entry.sources {
                    walk(source, &mut out);
                    if source.container_item_id != 0 {
                        add_use_reagents(source.container_item_id, &mut out);
                    }
                }
            }
        }
        for req in &quest.requires {
            if (req.kind_id == REQ_OWN_ITEM || kind_key(&req.kind) == "ownitem") && req.value1 > 0 {
                if let Ok(id) = u32::try_from(req.value1) {
                    out.insert(id);
                }
            }
        }
    }
    out
}

/// The bag slot count (`SCCharacterInvenInitPacket` / `SCInvenExpandedPacket`; 50 when the
/// snapshot never announced one).
fn bag_capacity(state: &GameState) -> u32 {
    if state.bag_slots == 0 {
        50
    } else {
        u32::from(state.bag_slots)
    }
}

/// True when the bag is close to its slot limit and should be cleaned before the next quest.
fn bag_pressure(state: &GameState) -> bool {
    (state.bag.len() as u32) + 8 >= bag_capacity(state)
}

/// Opens `--compact-db` (or the default game-data path when it exists). Steps that need
/// client-side data record `runner_unsupported` when it is missing.
fn open_compact_db(opts: &RunOptions) -> anyhow::Result<Option<CompactDb>> {
    if let Some(path) = &opts.compact_db {
        if !path.exists() {
            bail!("compact database {} not found", path.display());
        }
        return Ok(Some(CompactDb::open(path)?));
    }
    for candidate in [
        crate::compactdb::DEFAULT_COMPACT_DB,
        crate::compactdb::DEFAULT_COMPACT_DB_FROM_RUST,
    ] {
        let path = Path::new(candidate);
        if path.exists() {
            return Ok(Some(CompactDb::open(path)?));
        }
    }
    Ok(None)
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
        level_raised_to: None,
        result: QuestOutcome::BlockedPlan,
        failed_step: None,
        steps: Vec::new(),
        final_status: None,
        final_objectives: None,
        messages: vec![format!("quest {id} is not in the plan")],
        elapsed_ms: 0,
        fallbacks: Vec::new(),
        blocker: Blocker::new(
            blocker_class::DATA_GAP,
            format!("quest {id} is not in the plan"),
            "plan.py (the compact data has no loaded quest context)",
        ),
        forced_complete: false,
        forced_prereqs: Vec::new(),
        value_checks: None,
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
        level_raised_to: None,
        result: QuestOutcome::Error,
        failed_step: None,
        steps: Vec::new(),
        final_status: None,
        final_objectives: None,
        messages: vec!["connection lost".into()],
        elapsed_ms: 0,
        fallbacks: Vec::new(),
        blocker: Blocker::new(
            blocker_class::RUNNER_GAP,
            "connection lost before the quest result was written",
            "aaemu-testclient questrun.rs::run",
        ),
        forced_complete: false,
        forced_prereqs: Vec::new(),
        value_checks: None,
    }
}

/// The `error` line written when the pre-quest recovery could not return the bot to the main
/// world: the run stops there instead of failing every remaining quest on the same blocker.
fn recovery_failed_result(
    quest: &PlanQuest,
    character: &RunCharacter,
    reason: &str,
) -> QuestResult {
    QuestResult {
        quest_id: quest.quest_id,
        name: quest.name.clone(),
        character: character.name.clone(),
        race: character.race,
        gender: character.gender,
        start_level: character.start_level,
        level_raised_to: None,
        result: QuestOutcome::Error,
        failed_step: None,
        steps: Vec::new(),
        final_status: None,
        final_objectives: None,
        messages: vec![reason.to_string()],
        elapsed_ms: 0,
        fallbacks: Vec::new(),
        blocker: Blocker::suspect(
            reason,
            "aaemu-testclient/src/questrun.rs::recover_main_world (exit doodad + GM recall)",
            format!(
                "observed: {reason}; expected: the exit doodad (DoodadFuncExitIndun -> \
                 request_leave_instance -> return_to_main_world) or the GM recall \
                 (`{COMMAND_PREFIX}useskill <Return skill>`, \
                 aaemu-game/src/systems/skills/effects/special/movement.rs::return_) sends \
                 SCLoadInstancePacket with instance 0"
            ),
        ),
        forced_complete: false,
        forced_prereqs: Vec::new(),
        value_checks: None,
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

/// Server code paths cited by `blocker.code` (file:line at the time of this runner).
mod code {
    pub const ADD_QUEST: &str = "aaemu-game/src/systems/quests/engine.rs:1546 (add_quest)";
    pub const CAN_COMPONENT_RUN: &str =
        "aaemu-game/src/systems/quests/engine.rs:1588 (add_quest -> can_component_run)";
    pub const STARTER_ACCEPTS: &str =
        "aaemu-game/src/systems/quests/engine.rs:368 (starter_accepts)";
    pub const SCHEDULE: &str =
        "aaemu-game/src/systems/quests/engine.rs:1570 (quest_schedule_allows)";
    pub const UNIT_REQS: &str = "aaemu-game/src/systems/quests/unit_reqs.rs:156 (validate)";
    pub const ON_NPC_KILLED: &str = "aaemu-game/src/systems/quests/events.rs:785 (on_npc_killed)";
    pub const ON_DOODAD: &str =
        "aaemu-game/src/systems/quests/events.rs:701 (on_doodad_interaction)";
    pub const ON_REPORT: &str = "aaemu-game/src/systems/quests/events.rs:564 (on_report)";
    pub const ON_TALK: &str = "aaemu-game/src/systems/quests/acts.rs (QuestActObjTalk)";
    pub const SPHERES: &str = "aaemu-game/src/systems/quests/spheres.rs:307 (tick)";
    pub const ITEM_USE: &str = "aaemu-game/src/systems/skills/skill.rs (item_use)";
    pub const SKILL_TEMPLATE: &str = "aaemu-game/src/systems/skills/handlers.rs::template_skill";
    pub const LOOT_ROLL: &str = "aaemu-game/src/systems/economy/looting.rs:346 (generate_loot)";
    pub const DOODAD_LOOT: &str =
        "aaemu-game/src/systems/npcs/doodad/funcs/items.rs:40 (loot_item)";
    pub const GAIN_ITEM: &str = "aaemu-game/src/systems/economy/item_effects.rs:1024 (gain_item)";
    pub const LEAVE_INSTANCE: &str = "aaemu-game/src/systems/instances/indun.rs:620 \
         (request_leave_instance) -> instances/dungeon.rs:525 (leave_instance) -> \
         instances/worlds.rs:339 (return_to_main_world)";
    pub const RUNNER: &str = "aaemu-testclient/src/questrun.rs";
}

/// Classifies a result (`blocker.class` / `reason` / `code`). A completed quest is clean unless
/// a fallback recorded why the real path failed; a failed one is classified from its reason,
/// with the journal's concrete source attempts taking precedence for item goals.
fn classify_blocker(
    quest: &PlanQuest,
    facts: Option<&KnownFacts<'_>>,
    outcome: QuestOutcome,
    failed_step: Option<&FailedStep>,
    journal: &QuestJournal,
) -> Blocker {
    if outcome == QuestOutcome::Completed {
        return journal.blocker.clone().unwrap_or_default();
    }
    if outcome == QuestOutcome::AlreadyCompleted {
        return Blocker::default();
    }
    if let Some(hint) = &journal.blocker {
        // The gather arms know the concrete source failures; use them for item-goal failures
        // only. An accept that then failed for its own reason must not inherit the gather's
        // blocker (the gather even succeeded with a marked fallback).
        let item_goal = failed_step.is_none_or(|f| {
            (f.reason.starts_with("item(s) ")
                || f.reason.starts_with("item ")
                || f.reason.contains("not obtained")
                || f.reason.contains("not in the bag")
                || f.reason.contains("container"))
                && !f.reason.contains("accept")
                && !f.reason.contains("no answer from server")
        });
        if item_goal {
            return hint.clone();
        }
    }
    let reason = failed_step.map_or_else(|| outcome.name().to_string(), |f| f.reason.clone());
    match outcome {
        QuestOutcome::Completed | QuestOutcome::AlreadyCompleted => Blocker::default(),
        QuestOutcome::Auxiliary => Blocker::new(
            blocker_class::DATA_GAP,
            "quest has no Start component (plan status auxiliary); the server cannot accept it",
            "compact.sqlite3 quest_components / plan.py",
        ),
        QuestOutcome::BlockedPlan => {
            let detail = if quest.blockers.is_empty() {
                "plan status blocked (a required target has no spawn location)".to_string()
            } else {
                quest.blockers.join("; ")
            };
            Blocker::new(
                blocker_class::DATA_GAP,
                detail,
                "plan.py + Data/Worlds/world_spawns.json (1.2 spawn data)",
            )
        }
        QuestOutcome::PrereqMissing => Blocker::new(
            blocker_class::REQUIREMENT,
            reason,
            "plan prereq_quests (quest_contexts prerequisites)",
        ),
        QuestOutcome::RequirementUnmet => {
            Blocker::new(blocker_class::REQUIREMENT, reason, code::UNIT_REQS)
        }
        QuestOutcome::NeedsTrigger => Blocker::new(
            blocker_class::RUNNER_GAP,
            reason,
            "aaemu-testclient/src/questrun.rs::step_action (SpawnEffect skills)",
        ),
        _ => classify_reason(quest, facts, &reason),
    }
}

/// The reason-based half of [`classify_blocker`].
fn classify_reason(quest: &PlanQuest, facts: Option<&KnownFacts<'_>>, reason: &str) -> Blocker {
    let has = |needle: &str| reason.contains(needle);
    if has("accept silently ignored") {
        let undecided: Vec<String> = facts.map_or_else(Vec::new, |facts| {
            quest
                .requires
                .iter()
                .filter(|req| requirement_state(req, facts) == ReqState::Unknown)
                .map(requirement_reason)
                .collect()
        });
        if undecided.is_empty() {
            return Blocker::suspect(
                format!(
                    "server ignored the accept without an error and every plan requirement is \
                     met ({reason})"
                ),
                code::STARTER_ACCEPTS,
                format!(
                    "observed: no SCErrorMsg and no SCQuestContextStarted within {}s of the \
                     accept; expected: engine.rs::starter_accepts accepts the start component \
                     because every plan unit_reqs is met (plan requires: {})",
                    ACCEPT_WAIT.as_secs(),
                    if quest.requires.is_empty() {
                        "none".to_string()
                    } else {
                        quest
                            .requires
                            .iter()
                            .map(requirement_reason)
                            .collect::<Vec<_>>()
                            .join("; ")
                    }
                ),
            );
        }
        return Blocker::new(
            blocker_class::REQUIREMENT,
            format!(
                "server refused the accept silently; start-component unit_reqs the runner could \
                 not decide: {}",
                undecided.join("; ")
            ),
            code::CAN_COMPONENT_RUN,
        );
    }
    if has("scheduled_spawn") || has("spawn window") {
        return Blocker::new(
            blocker_class::REQUIREMENT,
            reason,
            "aaemu-game/src/systems/npcs/spawn_manager.rs::period_status_npc (spawn window)",
        );
    }
    if has("using the starter item did not start the quest") {
        return Blocker::suspect(
            reason,
            "aaemu-game/src/systems/quests/events.rs::accept_quest_effect / \
             engine.rs::add_quest",
            "observed: the starter item was used with its use_skill and neither \
             SCQuestContextStarted nor SCErrorMsg arrived; expected: AcceptQuestEffect starts \
             the quest for items whose items.impl_id is AcceptQuest (compact items / \
             item_accept_quests binding)",
        );
    }
    if has("QuestDailyLimit") {
        // The one place `engine.rs::add_quest` sends this code: "already completed and not
        // repeatable" (engine.rs ~1600); expected for a character that ran the quest before.
        return Blocker::new(
            blocker_class::REQUIREMENT,
            reason,
            "aaemu-game/src/systems/quests/engine.rs (add_quest: already completed, not \
             repeatable)",
        );
    }
    if has("its game schedule window is not in progress") {
        return Blocker::new(blocker_class::REQUIREMENT, reason, code::SCHEDULE);
    }
    if has("Item could not be created") || has("bag full") {
        return Blocker::new(
            blocker_class::RUNNER_GAP,
            reason,
            "aaemu-testclient/src/questrun.rs::free_bag_space (CSDestroyItemPacket / \
             CSExpandSlotsPacket)",
        );
    }
    if has("NEED_REAGENT") {
        return Blocker::new(
            blocker_class::RUNNER_GAP,
            reason,
            "aaemu-game/src/systems/skills/validation.rs::check_resources (NEED_REAGENT, skill \
             result 36) / compact.sqlite3 skill_reagents",
        );
    }
    if has("BackpackOccupied") {
        return Blocker::new(
            blocker_class::RUNNER_GAP,
            reason,
            "aaemu-testclient/src/questrun.rs::free_bag_space / \
             aaemu-game/src/systems/quests/engine.rs::can_accept_supply_items (BACKPACK_OCCUPIED)",
        );
    }
    if has("Item template id") && has("does not exist") {
        return Blocker::new(
            blocker_class::DATA_GAP,
            reason,
            "compact.sqlite3 items (the template is not in the 1.2 data)",
        );
    }
    if has("cannot spawn doodad") || has("don't exist") {
        return Blocker::new(
            blocker_class::DATA_GAP,
            reason,
            "compact.sqlite3 doodad_almighties (the template is not in the 1.2 data)",
        );
    }
    if has("no sign sphere") {
        return Blocker::new(
            blocker_class::DATA_GAP,
            reason,
            "client quest_sign_sphere.g (quests-spheres.json summary.missing_sphere_components); \
             aaemu-game/src/systems/quests/spheres.rs::load",
        );
    }
    if has("is blocked for this character") {
        return Blocker::new(
            blocker_class::REQUIREMENT,
            reason,
            "compact.sqlite3 unit_reqs (owner_type Skill) / \
             aaemu-game/src/systems/skills/unit_reqs.rs::validate",
        );
    }
    if has("no SCLoadInstancePacket after using") && has("for world") {
        return Blocker::suspect(
            reason,
            "aaemu-game/src/systems/instances/indun.rs::request_system_instance / \
             aaemu-game/src/systems/npcs/doodad/funcs/world.rs::enter_sys_instance",
            "observed: the entrance doodad was used with its own func skill and no \
             SCLoadInstancePacket arrived within the world-load window; expected: \
             DoodadFuncEnterSysInstance queues/creates the system instance and the load answers \
             with SCLoadInstancePacket (instances/indun.rs::request_system_instance -> \
             dungeon -> world load)",
        );
    }
    if has("no SCLoadInstancePacket after using") {
        return Blocker::suspect(
            reason,
            "aaemu-game/src/systems/instances/indun.rs::request_leave_instance",
            "observed: the exit doodad was used with its func skill and no SCLoadInstancePacket \
             arrived; expected: DoodadFuncExitIndun -> request_leave_instance -> \
             dungeon::leave_instance -> worlds::return_to_main_world sends SCLoadInstancePacket",
        );
    }
    if has("no world '") || has("no entrance doodad") || has("no DoodadFuncExitIndun") {
        return Blocker::new(
            blocker_class::DATA_GAP,
            reason,
            "Data/Worlds/world_spawns.json / compact.sqlite3 doodad funcs (1.2 data)",
        );
    }
    if has("has no skills row") || has("has no use_skill_id") {
        return Blocker::new(
            blocker_class::DATA_GAP,
            reason,
            "compact.sqlite3 items.use_skill_id -> skills (1.2 data)",
        );
    }
    if has("parsing world spawns") || has("world spawns") {
        return Blocker::new(
            blocker_class::DATA_GAP,
            reason,
            "Data/Worlds/world_spawns.json",
        );
    }
    if has("gm spawn fallback did not reach") {
        // A goal far beyond what the fallback window can kill (e.g. a 300-kill group hunt) is
        // the runner's window, not a server misbehaviour.
        let large = reason
            .split(" at ")
            .nth(1)
            .and_then(|rest| rest.split('/').nth(1))
            .and_then(|den| {
                den.split(|c: char| !c.is_ascii_digit())
                    .next()
                    .map(str::to_string)
            })
            .and_then(|digits| digits.parse::<i64>().ok())
            .is_some_and(|den| den > 60);
        if large {
            return Blocker::new(
                blocker_class::RUNNER_GAP,
                reason,
                "aaemu-testclient/src/questrun.rs::GM_SPAWN_WINDOW (large kill goal)",
            );
        }
        return Blocker::suspect(
            reason,
            code::ON_NPC_KILLED,
            "observed: the GM-spawned NPC was killed and the objective/quest did not advance; \
             expected: events.rs::on_npc_killed dispatches MonsterHunt/MonsterGroupHunt for the \
             killer (quest target list from the plan and compact quest_monster_npcs)",
        );
    }
    if has("doodad interaction did not update") {
        return Blocker::suspect(
            reason,
            code::ON_DOODAD,
            "observed: the doodad was used with the interaction skill and no quest update or \
             objective change followed; expected: the interaction effect fires \
             events.rs::on_doodad_interaction with the doodad template and the \
             QuestActObjInteraction target counts it (compact quest_act_obj_interactions)",
        );
    }
    if has("talk act") && has("did not update") {
        return Blocker::suspect(
            reason,
            code::ON_TALK,
            "observed: the talk packet was sent to the NPC and the quest objective did not move; \
             expected: the talk act counts the NPC's template (plan target ids / compact \
             quest_act_obj_talks)",
        );
    }
    if has("using the item did not update") {
        return Blocker::suspect(
            reason,
            code::ITEM_USE,
            "observed: the item was cast with its use_skill and the objective stayed unchanged; \
             expected: the use skill's effects advance the QuestActObjItemUse act (compact \
             items.use_skill_id / quest_act_obj_item_uses)",
        );
    }
    if has("sphere objective") {
        return Blocker::suspect(
            reason,
            code::SPHERES,
            "observed: the character entered the sign sphere and the objective did not change; \
             expected: spheres.rs::tick dispatches EnterSphere for the sphere component (plan \
             spheres geometry)",
        );
    }
    if has("no SCQuestContextCompleted") {
        return Blocker::suspect(
            reason,
            code::ON_REPORT,
            "observed: the report packet was sent to the target and no \
             SCQuestContextCompleted arrived; expected: events.rs::on_report completes the Ready \
             component at the reported NPC/doodad (plan report act)",
        );
    }
    if has("server rejected") || has("accept rejected") || has("no starter: accept rejected") {
        return Blocker::suspect(
            reason,
            code::ADD_QUEST,
            "observed: the server answered the accept with SCErrorMsg; expected: the accept \
             path accepts the start component (engine.rs::add_quest / starter_accepts; plan \
             accept act and unit_reqs)",
        );
    }
    if has("quest entered FAILED") {
        return Blocker::suspect(
            reason,
            "aaemu-game/src/systems/quests/engine.rs (fail_quest)",
            "observed: the live quest row moved to FAILED; expected: the plan steps satisfy \
             every non-optional act (plan components/acts)",
        );
    }
    if has("no SCQuestContextStarted after entering the sphere") {
        return Blocker::suspect(
            reason,
            code::SPHERES,
            "observed: the character entered the starter sphere and no SCQuestContextStarted \
             arrived; expected: spheres.rs::tick dispatches the starter event \
             (on_enter_quest_starter_sphere) for the sphere component",
        );
    }
    if has("is not handled by runner")
        || has("needs --compact-db")
        || has("is not driven")
        || has("depth limit")
        || has("only passive steps")
        || has("connection lost")
        || has("SpawnEffect")
    {
        return Blocker::new(blocker_class::RUNNER_GAP, reason, code::RUNNER);
    }
    if has("requirement") || has("prerequisite quests") {
        return Blocker::new(blocker_class::REQUIREMENT, reason, code::UNIT_REQS);
    }
    Blocker::new(
        blocker_class::RUNNER_GAP,
        reason,
        format!("{} (unclassified failure)", code::RUNNER),
    )
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
pub(crate) async fn setup_world(
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

#[allow(clippy::too_many_arguments)]
async fn run_quest(
    game: &mut GameClient,
    quest: &PlanQuest,
    character: &RunCharacter,
    raised_level: &mut u32,
    world_tracker: &mut WorldTracker,
    compact_db: Option<&CompactDb>,
    craft_db: Option<&CraftDb>,
    value_checker: Option<&ValueChecker>,
    step_timeout: Duration,
    opts: &RunOptions,
    forced: &mut BTreeSet<u32>,
    needed: &BTreeSet<u32>,
    classes: &BTreeMap<u32, (String, String)>,
) -> QuestResult {
    let start = Instant::now();
    let chat_start = game.state.chat_count;
    let mut steps = Vec::new();
    let mut last_info = None;
    let mut level_raised_to = None;
    let mut journal = QuestJournal::default();
    let forced_prereqs = forced_prereqs_of(quest, forced);

    // A quest the character already completed (the select snapshot or this session) that is not
    // repeatable can only answer `QuestDailyLimit`; skip it as `already_completed` (no failure,
    // no force).
    let repeatable = compact_db
        .and_then(|db| db.quest_repeatable(quest.quest_id).ok().flatten())
        .unwrap_or(false);
    if game.state.completed_quests.contains(&quest.quest_id) && !repeatable {
        return finish_quest(
            game,
            quest,
            character,
            QuestOutcome::AlreadyCompleted,
            None,
            steps,
            last_info,
            level_raised_to,
            chat_start,
            start,
            &journal,
            Blocker::default(),
            forced_prereqs,
            false,
            None,
        );
    }

    // Set just before the accept; `None` only on the pre-accept gate paths below, which never
    // reach the value checks (they need a completed quest).
    let mut value_start: Option<ValueSnapshot> = None;
    let (outcome, failed_step) = 'run: {
        if let Some(gate) = plan_gate(quest) {
            break 'run (gate, None);
        }

        // Prerequisites first: a quest whose prerequisite is neither in the select snapshot nor
        // completed live is skipped without touching the server. Forced prerequisites are in
        // `completed_quests` because forcing completed them on the server. A chain that only
        // fails on a requirement (race / faction / gender) is `requirement_unmet` naming the
        // prerequisite, not `prereq_missing`.
        let missing = missing_prerequisites(&quest.prereq_quests, &game.state.completed_quests);
        if !missing.is_empty() {
            let described: Vec<String> = missing
                .iter()
                .map(|id| match classes.get(id) {
                    Some((class, reason)) => format!("prerequisite {id} ({class}): {reason}"),
                    None => format!("prerequisite {id} was not run in this session"),
                })
                .collect();
            let only_requirement = missing.iter().all(|id| {
                classes
                    .get(id)
                    .is_some_and(|(class, _)| class == blocker_class::REQUIREMENT)
            });
            let outcome = if only_requirement {
                QuestOutcome::RequirementUnmet
            } else {
                QuestOutcome::PrereqMissing
            };
            break 'run (outcome, Some(gate_failed_step(described.join("; "))));
        }

        // Meet the quest's level demand (`min_level` when present, else `level`; `Level`
        // requirements and the level the quest's craft sources need included); `/level` only
        // adds the missing experience, never lowers, and the ensured level carries over to the
        // next quests. The raise (and the level it reached) is recorded in the result line.
        let demand = level_demand(quest).max(craft_level_demand(quest, craft_db));
        if demand >= 2 && demand > *raised_level {
            game.gm(&format!("level {demand}"), Duration::from_secs(2))
                .await
                .ok();
            *raised_level = demand;
            level_raised_to = Some(demand);
        }

        // `OwnItem` start requirements are items the accept path checks (`unit_reqs.rs`); fetch
        // them through the same real-source ladder before the accept (GM item only as the last
        // resort, marked in `fallbacks`). Quest 18 needs item 2142 and quest 2239 item 33381.
        let requirement_deadline = Instant::now() + step_timeout;
        if let Err(f) = ensure_requirement_items(
            game,
            quest,
            compact_db,
            craft_db,
            requirement_deadline,
            &mut journal,
            needed,
        )
        .await
        {
            break 'run (
                f.quest,
                Some(FailedStep {
                    act_id: 0,
                    act_type: String::new(),
                    reason: f.reason,
                }),
            );
        }

        // Value-check window (task E): every reward / kill observation from just before the
        // accept to the completion is compared against `compact.sqlite3` in `valuecheck`.
        value_start = Some(ValueSnapshot::capture(&game.state));

        // `unit_reqs` the runner can decide (race / gender / level from the selected character,
        // faction / mother faction, quest contexts, own items). Unknown kinds do not block: the
        // accept attempt lets the server decide and a silent rejection is classified in
        // `classify_blocker` with the undecided requirements.
        let owned_items: BTreeSet<u32> = game
            .state
            .bag
            .values()
            .map(|item| item.template_id)
            .collect();
        let active_quests: BTreeSet<u32> = game.state.quests.keys().copied().collect();
        let faction = game
            .state
            .self_info
            .as_ref()
            .map(|info| info.faction)
            .filter(|faction| *faction != 0);
        let mother_faction = match (faction, compact_db) {
            (Some(faction), Some(db)) => db.mother_faction(faction).ok().flatten(),
            _ => None,
        };
        let facts = KnownFacts {
            level: (*raised_level).max(u32::from(character.start_level)),
            race: Some(character.race),
            gender: Some(character.gender),
            faction,
            mother_faction,
            completed: &game.state.completed_quests,
            owned_items: &owned_items,
            active_quests: &active_quests,
        };
        if let Some(reason) = requirement_gate(quest, &facts) {
            break 'run (
                QuestOutcome::RequirementUnmet,
                Some(gate_failed_step(reason)),
            );
        }

        // Quests whose plan has no accept act (a Start component without acts: the server accepts
        // those from any source, `engine.rs::starter_accepts` `conditions.is_empty()`) get the
        // implicit empty `CSStartQuestContextPacket`; a refusal / silence is `no_starter` instead
        // of a reason-less `stuck`. An `ObjAggro` quest must be accepted from its engage-combat
        // NPC (`npcs.engage_combat_give_quest_id`) or the act never counts (its kill only matches
        // the quest's NPC acceptor).
        let engage_npc =
            compact_db.and_then(|db| db.engage_quest_npc(quest.quest_id).ok().flatten());
        let has_aggro = quest
            .steps
            .iter()
            .any(|step| step_action(step) == StepAction::Aggro);
        if needs_implicit_accept(quest)
            && !game.state.quests.contains_key(&quest.quest_id)
            && !quest_completed(game, quest.quest_id)
        {
            let accepted = if has_aggro {
                match engage_npc {
                    Some(npc) => accept_from_npc(
                        game,
                        quest.quest_id,
                        npc,
                        Instant::now() + step_timeout,
                        &mut journal,
                    )
                    .await
                    .map(|()| true),
                    None => {
                        journal.note_blocker(
                            blocker_class::DATA_GAP,
                            format!(
                                "QuestActObjAggro needs an NPC acceptor but no npcs row has \
                                 engage_combat_give_quest_id = {}",
                                quest.quest_id
                            ),
                            "compact.sqlite3 npcs.engage_combat_give_quest_id",
                        );
                        Ok(false)
                    }
                }
            } else {
                Ok(false)
            };
            match accepted {
                Ok(true) => {}
                Ok(false) => {
                    if let Err(f) =
                        implicit_accept(game, quest.quest_id, Instant::now() + step_timeout).await
                    {
                        if f.quest == QuestOutcome::AlreadyCompleted {
                            break 'run (QuestOutcome::AlreadyCompleted, None);
                        }
                        break 'run (f.quest, Some(gate_failed_step(f.reason)));
                    }
                }
                Err(f) => {
                    if f.quest == QuestOutcome::AlreadyCompleted {
                        break 'run (QuestOutcome::AlreadyCompleted, None);
                    }
                    break 'run (
                        f.quest,
                        Some(FailedStep {
                            act_id: 0,
                            act_type: String::new(),
                            reason: f.reason,
                        }),
                    );
                }
            }
        }

        let indices = objective_indices(&quest.steps);
        let mut outcome = None;
        let mut failed_step: Option<FailedStep> = None;
        for (i, step) in quest.steps.iter().enumerate() {
            let t0 = Instant::now();
            let deadline = t0 + step_timeout;
            let action = step_action(step);
            // Other worlds: enter the world this step targets (leaving one first when coming from
            // another); the world is left again after the quest. Steps that run nothing (skip /
            // passive) or fail before touching the world (unsupported / needs_trigger) stay where
            // the bot is.
            if !matches!(
                action,
                StepAction::Skip
                    | StepAction::Passive
                    | StepAction::Unsupported
                    | StepAction::NeedsTrigger
            ) {
                if let Err(f) = switch_world(
                    game,
                    world_tracker,
                    step_world(step).as_deref(),
                    compact_db,
                    deadline,
                )
                .await
                {
                    steps.push(StepOutcome {
                        act_id: step.act_id,
                        act_type: step.act_type.clone(),
                        outcome: f.step,
                        note: f.reason.clone(),
                        ms: t0.elapsed().as_millis() as u64,
                    });
                    break 'run (
                        f.quest,
                        Some(FailedStep {
                            act_id: step.act_id,
                            act_type: step.act_type.clone(),
                            reason: f.reason,
                        }),
                    );
                }
            }
            let res: Result<(StepOutcomeKind, String), StepFailure> = match action {
                StepAction::Skip => Ok((StepOutcomeKind::Skipped, String::new())),
                StepAction::Passive => Ok((StepOutcomeKind::Passive, String::new())),
                StepAction::Accept => {
                    step_accept(game, quest.quest_id, step, deadline, &mut journal).await
                }
                StepAction::AcceptDoodad => {
                    step_accept_doodad(game, quest.quest_id, step, deadline, &mut journal).await
                }
                StepAction::Talk => {
                    step_talk(
                        game,
                        quest.quest_id,
                        step,
                        indices[i],
                        deadline,
                        &mut journal,
                    )
                    .await
                }
                StepAction::Kill => {
                    step_kill(
                        game,
                        quest.quest_id,
                        step,
                        indices[i],
                        deadline,
                        &mut journal,
                    )
                    .await
                }
                StepAction::Aggro => {
                    step_aggro(
                        game,
                        quest.quest_id,
                        step,
                        indices[i],
                        engage_npc,
                        opts.combat_skill,
                        deadline,
                        &mut journal,
                    )
                    .await
                }
                StepAction::Report => {
                    step_report(game, quest.quest_id, step, deadline, &mut journal).await
                }
                StepAction::ReportDoodad => {
                    step_report_doodad(game, quest.quest_id, step, deadline, &mut journal).await
                }
                StepAction::ReportJournal => {
                    step_report_journal(game, quest.quest_id, deadline).await
                }
                StepAction::DoodadInteraction => {
                    step_doodad_interaction(
                        game,
                        quest.quest_id,
                        step,
                        indices[i],
                        compact_db,
                        deadline,
                        &mut journal,
                    )
                    .await
                }
                StepAction::Gather => {
                    step_gather(
                        game,
                        quest.quest_id,
                        step,
                        indices[i],
                        compact_db,
                        craft_db,
                        deadline,
                        &mut journal,
                        needed,
                    )
                    .await
                }
                StepAction::AcceptItem => {
                    step_accept_item(
                        game,
                        quest.quest_id,
                        step,
                        compact_db,
                        craft_db,
                        deadline,
                        &mut journal,
                        needed,
                    )
                    .await
                }
                StepAction::AcceptComponent => {
                    // An `ObjAggro` quest must be accepted from its engage-combat NPC, or the
                    // act's NPC-acceptor check never matches (`acts.rs` `ObjAggro`; the live
                    // game gives those quests by engaging the NPC).
                    if has_aggro {
                        if let Some(npc) = engage_npc {
                            accept_from_npc(game, quest.quest_id, npc, deadline, &mut journal)
                                .await
                                .map(|()| {
                                    (
                                        StepOutcomeKind::Ok,
                                        format!("accepted from engage-combat npc {npc}"),
                                    )
                                })
                        } else {
                            step_accept_component(game, quest.quest_id, deadline).await
                        }
                    } else {
                        step_accept_component(game, quest.quest_id, deadline).await
                    }
                }
                StepAction::UseItem => {
                    step_use_item(
                        game,
                        quest.quest_id,
                        step,
                        indices[i],
                        compact_db,
                        craft_db,
                        deadline,
                        &mut journal,
                        needed,
                    )
                    .await
                }
                StepAction::Sphere => {
                    step_sphere(game, quest.quest_id, step, indices[i], deadline).await
                }
                StepAction::ObjLevel => {
                    step_obj_level(game, quest.quest_id, step, indices[i], compact_db, deadline)
                        .await
                }
                StepAction::AcceptSphere => {
                    step_accept_sphere(game, quest.quest_id, step, deadline).await
                }
                StepAction::Unsupported => Err(StepFailure::unsupported(&step.act_type)),
                StepAction::NeedsTrigger => Err(StepFailure::plan(
                    QuestOutcome::NeedsTrigger,
                    StepOutcomeKind::NeedsTrigger,
                    format!(
                        "{} target is only spawned by a SpawnEffect skill the runner cannot \
                         trigger",
                        step.act_type
                    ),
                )),
            };
            let ms = t0.elapsed().as_millis() as u64;
            match res {
                Ok((kind, note)) => {
                    steps.push(StepOutcome {
                        act_id: step.act_id,
                        act_type: step.act_type.clone(),
                        outcome: kind,
                        note,
                        ms,
                    });
                    snapshot(game, quest.quest_id, &mut last_info);
                }
                Err(f) => {
                    steps.push(StepOutcome {
                        act_id: step.act_id,
                        act_type: step.act_type.clone(),
                        outcome: f.step,
                        note: f.reason.clone(),
                        ms,
                    });
                    // `QuestDailyLimit` proved the quest was already completed: the whole result
                    // becomes `already_completed` (no failed step, no forcing).
                    if f.quest == QuestOutcome::AlreadyCompleted {
                        outcome = Some(QuestOutcome::AlreadyCompleted);
                        failed_step = None;
                        break;
                    }
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

        // Still active at READY: the server waits for the client's turn-in. The object-free
        // `CSCompleteQuestContextPacket` is the one trigger that moves an automatic Ready
        // component (`ConAutoComplete` / `ConReportJournal`) on to the Reward step
        // (`events.rs::on_report`).
        if outcome.is_none()
            && !quest_completed(game, quest.quest_id)
            && !quest_failed(game, quest.quest_id)
            && quest_ready(game, quest.quest_id)
        {
            let _ = game.complete_quest(quest.quest_id, 0, 0).await;
            let _ = game.drain(FINAL_QUIET, FINAL_MAX).await;
            snapshot(game, quest.quest_id, &mut last_info);
        }

        let outcome = match outcome {
            Some(o) => o,
            None if quest_completed(game, quest.quest_id) => QuestOutcome::Completed,
            None if quest_failed(game, quest.quest_id) => QuestOutcome::Failed,
            None if drain_failed => QuestOutcome::Error,
            None if quest_ready(game, quest.quest_id) && only_passive_steps(quest) => {
                // No client trigger can finish it: the plan has no active step and the server
                // ignores the object-free report for an act-less Ready component (see
                // `unfinished_ready_reason`).
                failed_step = Some(gate_failed_step(unfinished_ready_reason(quest)));
                QuestOutcome::RunnerUnsupported
            }
            None => {
                // A quest whose run produced no step failure must still carry a clear reason.
                failed_step = Some(stuck_failed_step(quest));
                QuestOutcome::Stuck
            }
        };

        // Leave an instance the quest entered: the next quest starts in the main world, and the
        // run loop's pre-quest recovery covers a failure here (normal exit retry, then the GM
        // recall). The failed exit is reported once, on this quest (the one that entered), not
        // on every quest after it.
        if let Err(f) = switch_world(
            game,
            world_tracker,
            None,
            compact_db,
            Instant::now() + WORLD_LEAVE_WINDOW,
        )
        .await
        {
            journal
                .note_suspect_blocker(exit_failure_suspect(game.state.world_instance, &f.reason));
        }
        (outcome, failed_step)
    };

    // Classify the real result before any forcing: the class decides whether forcing is legal
    // (requirements are never forced). The facts borrow only the read-only state, so they are
    // scoped to this block; the blocker is owned.
    let blocker = {
        let owned_items: BTreeSet<u32> = game
            .state
            .bag
            .values()
            .map(|item| item.template_id)
            .collect();
        let active_quests: BTreeSet<u32> = game.state.quests.keys().copied().collect();
        let faction = game
            .state
            .self_info
            .as_ref()
            .map(|info| info.faction)
            .filter(|faction| *faction != 0);
        let mother_faction = match (faction, compact_db) {
            (Some(faction), Some(db)) => db.mother_faction(faction).ok().flatten(),
            _ => None,
        };
        let facts = KnownFacts {
            level: (*raised_level).max(u32::from(character.start_level)),
            race: Some(character.race),
            gender: Some(character.gender),
            faction,
            mother_faction,
            completed: &game.state.completed_quests,
            owned_items: &owned_items,
            active_quests: &active_quests,
        };
        classify_blocker(quest, Some(&facts), outcome, failed_step.as_ref(), &journal)
    };

    // Last resort (`--force-blocked`): force a quest that could not finish to Reward so its
    // dependents still run. The real result and failed step are kept; `forced_complete` marks
    // the shortcut and later quests record it in `forced_prereqs`. A requirement on race /
    // faction / gender (or another requirement) is never forced: that quest is simply not
    // applicable to this character and forcing it would complete another faction's content.
    let mut forced_complete = false;
    let force_class = matches!(
        blocker.class.as_str(),
        blocker_class::RUNNER_GAP | blocker_class::SUSPECT | blocker_class::DATA_GAP
    );
    if opts.force_blocked
        && force_class
        && outcome != QuestOutcome::Completed
        && outcome != QuestOutcome::Auxiliary
        && outcome != QuestOutcome::AlreadyCompleted
    {
        let original = failed_step
            .as_ref()
            .map(|f| f.reason.clone())
            .unwrap_or_else(|| outcome.name().to_string());
        match force_quest_complete(
            game,
            quest.quest_id,
            Instant::now() + Duration::from_secs(10),
        )
        .await
        {
            Ok(()) => {
                forced_complete = true;
                forced.insert(quest.quest_id);
                journal.fallback(
                    "forced_complete",
                    quest.quest_id.to_string(),
                    format!("real result {}: {original}", outcome.name()),
                );
            }
            Err(reason) => {
                journal.note_suspect(
                    format!("force attempt failed: {reason}"),
                    code::ADD_QUEST,
                    format!(
                        "observed: /quest add + /quest step Reward for {} did not complete the \
                         quest ({reason}); expected: the GM quest command marks the quest \
                         completed (commands/scripts/quest_cmd.rs -> engine.rs::gm_set_step)",
                        quest.quest_id
                    ),
                );
            }
        }
    }

    // Value checks (task E): compare the reward / kill packets observed since the window
    // start against the same compact data the server loaded. Only a completed quest has a
    // full reward picture; every other outcome leaves the field unset.
    let value_checks = match (outcome, value_checker, value_start.as_ref()) {
        (QuestOutcome::Completed, Some(checker), Some(value_start)) => {
            let state = &game.state;
            let window = QuestWindow {
                self_obj: value_start.self_obj(),
                selected_reward: 0,
                bag_free_slots: value_start.free_slots(),
                bag_before: value_start.bag(),
                exp_changes: state
                    .exp_changes
                    .get(value_start.exp()..)
                    .unwrap_or_default(),
                events: state.events.get(value_start.events()..).unwrap_or_default(),
                kills: state
                    .kill_log
                    .get(value_start.kills()..)
                    .unwrap_or_default(),
                loot: state.loot_log.get(value_start.loot()..).unwrap_or_default(),
            };
            Some(
                checker
                    .check(quest.quest_id, &window)
                    .unwrap_or_else(|e| ValueReport::note(format!("value checks failed: {e:#}"))),
            )
        }
        _ => None,
    };

    finish_quest(
        game,
        quest,
        character,
        outcome,
        failed_step,
        steps,
        last_info,
        level_raised_to,
        chat_start,
        start,
        &journal,
        blocker,
        forced_prereqs,
        forced_complete,
        value_checks,
    )
}

/// Prerequisites of `quest` (plan edges and completed-quest requirements) that were forced
/// earlier in this run.
fn forced_prereqs_of(quest: &PlanQuest, forced: &BTreeSet<u32>) -> Vec<u32> {
    let mut out: Vec<u32> = quest
        .prereq_quests
        .iter()
        .copied()
        .filter(|id| forced.contains(id))
        .collect();
    for req in &quest.requires {
        let key = kind_key(&req.kind);
        if (req.kind_id == REQ_COMPLETE_QUEST_CONTEXT
            || req.kind_id == REQ_PRE_COMPLETE_QUEST_CONTEXT
            || key == "completequestcontext"
            || key == "precompletequestcontext")
            && req.value1 > 0
        {
            if let Ok(id) = u32::try_from(req.value1) {
                if forced.contains(&id) {
                    out.push(id);
                }
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Builds the result line from the run journal (fallbacks + blocker).
#[allow(clippy::too_many_arguments)]
fn finish_quest(
    game: &GameClient,
    quest: &PlanQuest,
    character: &RunCharacter,
    outcome: QuestOutcome,
    failed_step: Option<FailedStep>,
    steps: Vec<StepOutcome>,
    last_info: Option<QuestInfo>,
    level_raised_to: Option<u32>,
    chat_start: u64,
    start: Instant,
    journal: &QuestJournal,
    blocker: Blocker,
    forced_prereqs: Vec<u32>,
    forced_complete: bool,
    value_checks: Option<ValueReport>,
) -> QuestResult {
    result_line(
        game,
        quest,
        character,
        outcome,
        failed_step,
        steps,
        last_info,
        level_raised_to,
        chat_start,
        start,
        journal.fallbacks.clone(),
        blocker,
        forced_complete,
        forced_prereqs,
        value_checks,
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
    level_raised_to: Option<u32>,
    chat_start: u64,
    start: Instant,
    fallbacks: Vec<Fallback>,
    blocker: Blocker,
    forced_complete: bool,
    forced_prereqs: Vec<u32>,
    value_checks: Option<ValueReport>,
) -> QuestResult {
    let live = game.state.quests.get(&quest.quest_id);
    let final_info = live.cloned().or(last_info);
    QuestResult {
        quest_id: quest.quest_id,
        name: quest.name.clone(),
        character: character.name.clone(),
        race: character.race,
        gender: character.gender,
        start_level: character.start_level,
        level_raised_to,
        result: outcome,
        failed_step,
        steps,
        final_status: final_status_of(
            outcome,
            live.map(|q| q.status),
            final_info.as_ref().map(|q| q.status),
        ),
        final_objectives: final_info.as_ref().map(|q| q.objectives),
        messages: window_messages(game, chat_start),
        elapsed_ms: start.elapsed().as_millis() as u64,
        fallbacks,
        blocker,
        forced_complete,
        forced_prereqs,
        value_checks,
    }
}

/// The honest final status of a run. The live row wins while the quest is still active. A
/// completed quest is dropped by the server and the `SCQuestContextCompletedPacket` carries no
/// status, so the completion itself is reported instead of the stale status of the last update
/// (quests 3880 / 6038 / 6217 auto-complete (`ConAutoComplete`) and were reported with the
/// `INVALID` of the started row); every other outcome keeps the last seen row.
pub fn final_status_of(outcome: QuestOutcome, live: Option<u8>, last: Option<u8>) -> Option<u8> {
    match live {
        Some(status) => Some(status),
        None if outcome == QuestOutcome::Completed => {
            Some(aaemu_game::systems::quests::model::status::COMPLETED)
        }
        None => last,
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

/// The quest is still active at `READY` (the server waits for the client's turn-in).
fn quest_ready(game: &GameClient, quest_id: u32) -> bool {
    quest_ready_state(&game.state, quest_id)
}

/// [`quest_ready`] on a plain state (unit tests).
fn quest_ready_state(state: &GameState, quest_id: u32) -> bool {
    state
        .quests
        .get(&quest_id)
        .is_some_and(|q| q.status == aaemu_game::systems::quests::model::status::READY)
}

/// The plan has no step that could finish the quest: no steps at all, or only passive/skipped
/// ones. Quests 1835 (`(test)`) and 5678 (`orders`) have act-less Ready components: the server
/// walks them to READY on accept and no client trigger finishes them.
fn only_passive_steps(quest: &PlanQuest) -> bool {
    quest
        .steps
        .iter()
        .all(|s| matches!(step_action(s), StepAction::Passive | StepAction::Skip))
}

/// Why a READY quest the runner cannot finish stays open. The server ignores the object-free
/// report when the Ready component has no `ConAutoComplete` / `ConReportJournal` act
/// (`events.rs::on_report` `automatic_report`), and these quests have no act to drive at all.
/// The C# `QuestManagerEvents.DoReportEvents` moved an object-free report straight to the
/// Reward step (`quest.Step = QuestComponentKind.Reward`), so the port's guard is what leaves
/// act-less quests unfinished — a server-side gap reported here with its code path.
fn unfinished_ready_reason(quest: &PlanQuest) -> String {
    format!(
        "quest {} stays in READY: no plan step finishes it and the server ignores the \
         object-free turn-in (events.rs::on_report needs a ConAutoComplete/ConReportJournal act \
         in the Ready component; the C# moved object-free reports to the Reward step)",
        quest.quest_id
    )
}

fn objective_count(game: &GameClient, quest_id: u32, index: u8) -> i32 {
    objective_count_of(&game.state, quest_id, index)
}

/// [`objective_count`] on a plain state (unit tests).
fn objective_count_of(state: &GameState, quest_id: u32, index: u8) -> i32 {
    state
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

#[derive(Debug)]
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

    /// `runner_unsupported` with a free-form reason (missing game data, unknown source, ...).
    fn unsupported_reason(reason: impl Into<String>) -> Self {
        Self::plan(
            QuestOutcome::RunnerUnsupported,
            StepOutcomeKind::Unsupported,
            reason,
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

/// The rejection of an accept. `ErrorCode 621 (QuestDailyLimit)` is the server's answer to a
/// non-repeatable already-completed quest (`engine.rs::add_quest`): record it as completed so
/// later selections skip it and report `already_completed` (blocker none) instead of a failure.
fn rejected_accept(game: &mut GameClient, quest_id: u32, what: &str, code: i16) -> StepFailure {
    if code == 621 {
        game.state.completed_quests.insert(quest_id);
        return StepFailure::plan(
            QuestOutcome::AlreadyCompleted,
            StepOutcomeKind::Rejected,
            format!("{what}: already completed and not repeatable (QuestDailyLimit)"),
        );
    }
    StepFailure::plan(
        QuestOutcome::Rejected,
        StepOutcomeKind::Rejected,
        format!("{what} rejected: {}", error_reason(code)),
    )
}

// ----- worlds -----

/// The world the bot is in, tracked across the whole run: the plan world [`switch_world`] last
/// entered (`None` = `main_world`) and the world instance the server last announced in
/// `SCLoadInstancePacket` / the character-select snapshot (`0` = the main world). The observed
/// instance is authoritative: a failed exit doodad leaves it pointing into the instance even
/// though `current` still names the plan world, and the next quest's recovery must notice.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct WorldTracker {
    /// Plan world the runner believes the bot is in (`None` = `main_world`).
    pub current: Option<String>,
    /// The world instance of the last `SCLoadInstancePacket` (`GameState::world_instance`);
    /// `0` = the main world.
    pub observed_instance: u32,
}

impl WorldTracker {
    /// Syncs the server-authoritative instance from the last world load
    /// ([`GameState::world_instance`]). A main-world load clears the plan world: the server
    /// moved the bot out, so the plan bookkeeping is stale, not the server state.
    pub fn observe(&mut self, state: &GameState) {
        self.observed_instance = state.world_instance;
        if self.observed_instance == MAIN_INSTANCE_ID {
            self.current = None;
        }
    }

    /// True when the server reports the main world (`SCLoadInstancePacket` instance 0).
    pub fn in_main_world(&self) -> bool {
        self.observed_instance == MAIN_INSTANCE_ID
    }
}

/// Whether the bot is in the world a switch targets: the observed instance decides the main
/// world (instance 0), the plan tracker names the desired world inside an instance. A main-world
/// observation wins over a stale plan world; an instance observation with no plan world (or a
/// different one) is not the target.
pub fn in_target_world(
    observed_instance: u32,
    planned_world: Option<&str>,
    target: Option<&str>,
) -> bool {
    match target {
        None => observed_instance == MAIN_INSTANCE_ID,
        Some(name) => {
            observed_instance != MAIN_INSTANCE_ID
                && planned_world.is_some_and(|planned| planned.eq_ignore_ascii_case(name))
        }
    }
}

/// What the pre-quest [`recover_main_world`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovery {
    /// The bot is in the main world: it already was, or the retried normal exit moved it.
    MainWorld,
    /// The normal exit did not move the bot; the GM recall (`/useskill <skill>`) did.
    GmTeleport {
        skill: u32,
        /// The instance the bot was trapped in.
        instance: u32,
        /// The concrete failure of the normal exit.
        exit_failure: String,
    },
    /// Neither the normal exit nor the GM recall returned the bot to the main world: stop the
    /// run instead of failing every remaining quest with the same blocker.
    Failed { reason: String },
}

/// The `gm_teleport` fallback recorded on the quest during which the recovery ran: the exit
/// doodad failed, the GM recall (`commands/scripts/use_skill.rs` -> `movement.rs::return_`)
/// moved the bot back to the main world.
fn gm_teleport_fallback(instance: u32, exit_failure: &str, skill: u32) -> Fallback {
    Fallback::new(
        "gm_teleport",
        "main_world",
        format!(
            "the exit doodad did not return the bot to the main world from instance {instance} \
             ({exit_failure}); `{COMMAND_PREFIX}useskill {skill}` ran the skill's Return special \
             (aaemu-game/src/systems/skills/effects/special/movement.rs::return_) and its \
             main-world SCLoadInstancePacket arrived"
        ),
    )
}

/// The one-time suspect of the quest whose end-of-quest exit left the bot in an instance. The
/// next quest's recovery explains what was done about it; the quests after it keep their own
/// results clean.
fn exit_failure_suspect(instance: u32, reason: &str) -> Blocker {
    Blocker::suspect(
        format!("the quest left the bot in world instance {instance}: {reason}"),
        code::LEAVE_INSTANCE,
        format!(
            "observed: the exit doodad was used with its func skill and the bot is still in \
             world instance {instance} ({reason}); expected: DoodadFuncExitIndun -> \
             indun::request_leave_instance -> dungeon::leave_instance -> \
             worlds::return_to_main_world sends SCLoadInstancePacket(instance 0) so the next \
             quest starts in the main world"
        ),
    )
}

/// The plan world a step targets: the world of its first location (a step without locations,
/// e.g. a sphere act, falls back to its first sphere). `None` = `main_world` or no data.
pub fn step_world(step: &PlanStep) -> Option<String> {
    let world = step
        .locations
        .first()
        .map(|l| l.world.as_str())
        .or_else(|| {
            step.spheres
                .as_deref()
                .and_then(|spheres| spheres.first())
                .map(|sphere| sphere.world.as_str())
        })?;
    (!world.eq_ignore_ascii_case(MAIN_WORLD)).then(|| world.to_string())
}

/// The first location of a step (plan order). [`run_quest`] switched the bot into the step's
/// world first ([`step_world`]), so the location is where the step's target lives.
fn step_location(step: &PlanStep) -> Result<(f32, f32, f32), StepFailure> {
    step.locations
        .first()
        .map(|loc| (loc.x, loc.y, loc.z))
        .ok_or_else(|| {
            StepFailure::plan(
                QuestOutcome::RunnerUnsupported,
                StepOutcomeKind::Unsupported,
                "step has no spawn location",
            )
        })
}

/// Spawns a GM doodad of `template` next to the bot and waits for the new object (a doodad of
/// the template that was visible before the spawn is not the new one). `/spawn doodad <id>`
/// (`commands/scripts/spawn.rs`).
async fn spawn_world_doodad(
    game: &mut GameClient,
    template: u32,
    deadline: Instant,
) -> Result<DoodadInfo, StepFailure> {
    let before: HashMap<u32, (f32, f32, f32)> = game
        .state
        .doodads
        .values()
        .filter(|d| d.template_id == template)
        .map(|d| (d.obj_id, d.pos))
        .collect();
    // `/spawn doodad` answers with a chat line even when the doodad never becomes visible (the
    // spawn tick can miss); retry the command a few times before failing.
    for _ in 0..3 {
        if Instant::now() >= deadline {
            break;
        }
        let messages = game
            .gm(
                &format!("spawn doodad {template}"),
                Duration::from_millis(1500),
            )
            .await
            .map_err(StepFailure::io)?;
        if messages
            .iter()
            .any(|m| m.contains("don't exist") || m.contains("parse error"))
        {
            return Err(StepFailure::unsupported_reason(format!(
                "cannot spawn doodad {template}: {}",
                messages.join("; ")
            )));
        }
        let appear = (Instant::now() + APPEAR_WAIT).min(deadline);
        loop {
            // The command spawns 3 m in front of the bot. A new object id is the clean signal;
            // when the server reuses the id of the just-despawned doodad, its position moved.
            if let Some(doodad) = game
                .state
                .doodads
                .values()
                .filter(|d| {
                    d.template_id == template
                        && (!before.contains_key(&d.obj_id)
                            || dist2(d.pos, before[&d.obj_id]) > 1.0)
                })
                .min_by(|a, b| {
                    dist2(a.pos, game.state.self_pos).total_cmp(&dist2(b.pos, game.state.self_pos))
                })
                .cloned()
            {
                return Ok(doodad);
            }
            let left = appear.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            game.recv_within(left.min(PUMP))
                .await
                .map_err(StepFailure::io)?;
        }
    }
    Err(StepFailure::timeout(format!(
        "doodad {template} did not appear after /spawn doodad (3 attempts)"
    )))
}

/// Waits for the world-load answer of an entrance / exit use and confirms it like the real
/// client: `SCLoadInstancePacket` -> `CSInstanceLoadedPacket` -> own `SCUnitStatePacket`
/// (`systems/instances/worlds.rs::instance_loaded`). `Ok(false)` when no load arrived.
async fn wait_world_load(game: &mut GameClient, deadline: Instant) -> Result<bool, StepFailure> {
    let load = wait_for_packet(
        game,
        (Instant::now() + WORLD_LOAD_WAIT).min(deadline),
        |p| p.is_sc(sc::SCLoadInstancePacket),
    )
    .await
    .map_err(StepFailure::io)?;
    if load.is_none() {
        return Ok(false);
    }
    game.confirm_instance_loaded()
        .await
        .map_err(StepFailure::io)?;
    let _ = wait_for_packet(
        game,
        (Instant::now() + WORLD_LOAD_WAIT).min(deadline),
        |p| p.is_sc(sc::SCUnitStatePacket),
    )
    .await
    .map_err(StepFailure::io)?;
    settle(game, deadline).await;
    Ok(true)
}

/// Enters the plan world `name`: the entrance doodad template of the world's default zone
/// (`Data/Worlds/world_spawns.json` -> `zones` -> `DoodadFuncEnterSysInstance` /
/// `DoodadFuncEnterInstance`) is spawned next to the bot and used with the interaction skill.
async fn enter_world(
    game: &mut GameClient,
    name: &str,
    compact_db: Option<&CompactDb>,
    deadline: Instant,
) -> Result<(), StepFailure> {
    let Some(db) = compact_db else {
        return Err(StepFailure::unsupported_reason(format!(
            "entering world '{name}' needs --compact-db (game data)"
        )));
    };
    let Some(zone_key) = db.world_zone_key(name).map_err(StepFailure::io)? else {
        return Err(StepFailure::unsupported_reason(format!(
            "no world '{name}' in the game data (Data/Worlds/world_spawns.json)"
        )));
    };
    let entrances = db
        .world_entrance_doodads(zone_key)
        .map_err(StepFailure::io)?;
    if entrances.is_empty() {
        return Err(StepFailure::unsupported_reason(format!(
            "no entrance doodad for world '{name}' (zone key {zone_key}) in the game data"
        )));
    }
    let Some(generic_skill) = db.interaction_use_skill().map_err(StepFailure::io)? else {
        return Err(StepFailure::unsupported_reason(
            "the game data has no InteractionEffect Use skill",
        ));
    };
    // Entrance doodads carry their own skill (17829 on the Mirage Isle portal) and some are
    // faction-gated (the instance_library portal's skill 24354 requires the Haranya factions):
    // pick the first entrance this character may use; when every one is blocked, that is a
    // requirement, not a server fault.
    let race = game.state.self_info.as_ref().map(|info| info.race);
    let faction = game
        .state
        .self_info
        .as_ref()
        .map(|info| info.faction)
        .filter(|faction| *faction != 0);
    let mother_faction = match (faction, Some(db)) {
        (Some(faction), Some(db)) => db.mother_faction(faction).ok().flatten(),
        _ => None,
    };
    let mut chosen: Option<(u32, Option<u32>)> = None;
    let mut blocked: Option<String> = None;
    for &template in entrances.iter().take(8) {
        let skill = db
            .doodad_start_skills(template)
            .unwrap_or_default()
            .into_iter()
            .next();
        let block = match skill {
            Some(skill) => db
                .skill_character_block(skill, race, faction, mother_faction)
                .unwrap_or(None)
                .map(|reason| format!("entrance doodad {template} skill {skill}: {reason}")),
            None => None,
        };
        if let Some(reason) = block {
            blocked.get_or_insert(reason);
            continue;
        }
        chosen = Some((template, skill));
        break;
    }
    let Some((template, chosen_skill)) = chosen else {
        return Err(StepFailure::plan(
            QuestOutcome::RequirementUnmet,
            StepOutcomeKind::Unsupported,
            format!(
                "every entrance doodad of world '{name}' is blocked for this character: {}",
                blocked.unwrap_or_else(|| "no usable entrance skill".into())
            ),
        ));
    };
    let doodad = spawn_world_doodad(game, template, deadline).await?;
    // The entrance func carries its own skill; the generic Use skill matches no func there and
    // the server drops the interaction.
    let skill = chosen_skill
        .or_else(|| doodad_phase_skill(db, &doodad))
        .unwrap_or(generic_skill);
    game.use_doodad(skill, doodad.obj_id)
        .await
        .map_err(StepFailure::io)?;
    if !wait_world_load(game, deadline).await? {
        return Err(StepFailure::timeout(format!(
            "no SCLoadInstancePacket after using entrance doodad {template} (skill {skill}) for \
             world '{name}'"
        )));
    }
    Ok(())
}

/// Leaves the current instance back to the saved main-world position
/// (`DoodadFuncExitIndun` -> `indun::request_leave_instance` -> `worlds::return_to_main_world`).
async fn exit_world(
    game: &mut GameClient,
    compact_db: Option<&CompactDb>,
    deadline: Instant,
) -> Result<(), StepFailure> {
    let Some(db) = compact_db else {
        return Err(StepFailure::unsupported_reason(
            "leaving an instance needs --compact-db (game data)",
        ));
    };
    let Some(template) = db.exit_doodad().map_err(StepFailure::io)? else {
        return Err(StepFailure::unsupported_reason(
            "no DoodadFuncExitIndun doodad in the game data",
        ));
    };
    let Some(generic_skill) = db.interaction_use_skill().map_err(StepFailure::io)? else {
        return Err(StepFailure::unsupported_reason(
            "the game data has no InteractionEffect Use skill",
        ));
    };
    let doodad = spawn_world_doodad(game, template, deadline).await?;
    let skill = doodad_phase_skill(db, &doodad).unwrap_or(generic_skill);
    game.use_doodad(skill, doodad.obj_id)
        .await
        .map_err(StepFailure::io)?;
    if !wait_world_load(game, deadline).await? {
        return Err(StepFailure::timeout(format!(
            "no SCLoadInstancePacket after using the exit doodad {template} (skill {skill})"
        )));
    }
    // Re-anchor the character in the main world: after the leave the server-side transform can
    // still resolve to the instance (`world_of`), and a doodad spawned there is invisible in the
    // main world. The GM `/move` round-trip refreshes the region.
    let here = game.state.self_pos;
    let _ = game.teleport(near(here)).await;
    Ok(())
}

/// Puts the bot into the plan world `world` (`None` = `main_world`) when it is not there yet:
/// leaves the current instance first, then enters the target world. The server's observed
/// instance decides where the bot actually is ([`in_target_world`]); the plan tracker is only
/// bookkeeping, so a failed exit (observed instance left behind) is left visible for the
/// pre-quest recovery instead of trusting a stale `current`. Used before every step that
/// targets another world and, best effort, at the end of a quest.
async fn switch_world(
    game: &mut GameClient,
    tracker: &mut WorldTracker,
    world: Option<&str>,
    compact_db: Option<&CompactDb>,
    deadline: Instant,
) -> Result<(), StepFailure> {
    tracker.observe(&game.state);
    let target = world.map(str::to_string);
    if in_target_world(
        tracker.observed_instance,
        tracker.current.as_deref(),
        target.as_deref(),
    ) {
        return Ok(());
    }
    if !tracker.in_main_world() {
        exit_world(game, compact_db, deadline).await?;
        tracker.observe(&game.state);
        if !tracker.in_main_world() {
            return Err(StepFailure::timeout(format!(
                "the exit doodad answered but the character is still in world instance {}",
                tracker.observed_instance
            )));
        }
    }
    if let Some(name) = &target {
        enter_world(game, name, compact_db, deadline).await?;
        tracker.observe(&game.state);
        if tracker.in_main_world() {
            return Err(StepFailure::timeout(format!(
                "the entrance doodad answered but the character is still in the main world \
                 (world '{name}')"
            )));
        }
        tracker.current = target;
    }
    Ok(())
}

/// Makes sure the next quest starts in the main world. A quest the previous one entered through
/// an exit doodad that did not answer leaves the bot trapped in the instance; retry the normal
/// exit once (the same path the quest used), then fall back to the GM recall
/// (`/useskill <Return skill>`). `Failed` when neither moved the bot: the caller stops the run
/// with a clear error instead of burning every remaining quest on the same blocker.
async fn recover_main_world(
    game: &mut GameClient,
    tracker: &mut WorldTracker,
    compact_db: Option<&CompactDb>,
    deadline: Instant,
) -> Recovery {
    tracker.observe(&game.state);
    if in_target_world(tracker.observed_instance, tracker.current.as_deref(), None) {
        return Recovery::MainWorld;
    }
    let instance = tracker.observed_instance;
    let exit_failure = match exit_world(game, compact_db, deadline).await {
        Ok(()) => {
            tracker.observe(&game.state);
            if tracker.in_main_world() {
                return Recovery::MainWorld;
            }
            format!(
                "the exit doodad answered but the character is still in world instance {}",
                tracker.observed_instance
            )
        }
        Err(f) => f.reason,
    };
    let gm_failure = match gm_teleport_main_world(game, deadline).await {
        Ok(skill) => {
            tracker.observe(&game.state);
            if tracker.in_main_world() {
                return Recovery::GmTeleport {
                    skill,
                    instance,
                    exit_failure,
                };
            }
            format!(
                "the recall answered but the character is still in world instance {}",
                tracker.observed_instance
            )
        }
        Err(f) => f.reason,
    };
    tracker.observe(&game.state);
    Recovery::Failed {
        reason: format!(
            "stuck in world instance {}: the exit doodad did not return the bot to the main \
             world ({exit_failure}); the GM recall failed too ({gm_failure}); stopping instead \
             of running the remaining quests with the same blocker",
            tracker.observed_instance
        ),
    }
}

/// The GM recall fallback: `/useskill <Return skill>` (`commands/scripts/use_skill.rs`)
/// schedules the skill's Return special (`skills/effects/special/movement.rs::return_`), which
/// sends the main-world `SCLoadInstancePacket` on its own. The candidates of
/// [`GM_RETURN_SKILLS`] are tried in order because the recall resolves the character's own
/// return point; the first load that arrives wins.
async fn gm_teleport_main_world(
    game: &mut GameClient,
    deadline: Instant,
) -> Result<u32, StepFailure> {
    let mut last = "no candidate skill answered".to_string();
    for skill in GM_RETURN_SKILLS {
        if Instant::now() >= deadline {
            last = "the recovery window closed".into();
            break;
        }
        let messages = game
            .gm(&format!("useskill {skill}"), Duration::from_millis(1500))
            .await
            .unwrap_or_default();
        match wait_recall_load(game, deadline).await {
            Ok(true) if game.state.world_instance == MAIN_INSTANCE_ID => return Ok(skill),
            Ok(true) => {
                last = format!(
                    "skill {skill} loaded world instance {} instead of the main world",
                    game.state.world_instance
                );
            }
            Ok(false) => {
                let said = messages
                    .first()
                    .map(|m| format!(" (server said: {m})"))
                    .unwrap_or_default();
                last = format!("skill {skill} sent no SCLoadInstancePacket{said}");
            }
            Err(f) => last = format!("skill {skill}: {}", f.reason),
        }
    }
    Err(StepFailure::timeout(format!(
        "the GM recall did not return the bot to the main world: {last}"
    )))
}

/// Waits for the world-load answer of the GM recall: [`GameClient::gm`]'s response window drops
/// packets that are not chat, so when it already consumed the `SCLoadInstancePacket` the
/// observer's `world_instance` says so (the main world is 0) and only the client handshake runs
/// (`CSInstanceLoadedPacket` -> own `SCUnitStatePacket`, `instances/worlds.rs::instance_loaded`).
/// `Ok(false)` when no load arrived at all.
async fn wait_recall_load(game: &mut GameClient, deadline: Instant) -> Result<bool, StepFailure> {
    let seen = if game.state.world_instance == MAIN_INSTANCE_ID {
        true
    } else {
        wait_for_packet(
            game,
            (Instant::now() + WORLD_LOAD_WAIT).min(deadline),
            |p| p.is_sc(sc::SCLoadInstancePacket),
        )
        .await
        .map_err(StepFailure::io)?
        .is_some()
    };
    if !seen {
        return Ok(false);
    }
    game.confirm_instance_loaded()
        .await
        .map_err(StepFailure::io)?;
    let _ = wait_for_packet(
        game,
        (Instant::now() + WORLD_LOAD_WAIT).min(deadline),
        |p| p.is_sc(sc::SCUnitStatePacket),
    )
    .await
    .map_err(StepFailure::io)?;
    settle(game, deadline).await;
    Ok(true)
}

// ----- shared waits -----

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
        if quest_completed(game, quest_id) || quest_ready(game, quest_id) {
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

/// `(kind, note)`: the note names the path that succeeded (source, `gm_spawn`, ...).
type StepRun = Result<(StepOutcomeKind, String), StepFailure>;

/// Waits (bounded by [`APPEAR_WAIT`]) for a doodad of `template_id` to become visible.
async fn wait_for_doodad(
    game: &mut GameClient,
    template_id: u32,
    deadline: Instant,
) -> Result<DoodadInfo, StepFailure> {
    let appear = (Instant::now() + APPEAR_WAIT).min(deadline);
    loop {
        if let Some(doodad) = game.state.find_doodad(template_id) {
            return Ok(doodad);
        }
        let left = appear.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(StepFailure::timeout(format!(
                "doodad {template_id} did not appear"
            )));
        }
        game.recv_within(left.min(PUMP))
            .await
            .map_err(StepFailure::io)?;
    }
}

/// The nearest visible NPC of `template_id` (waits [`APPEAR_WAIT`]); when the natural spawn is
/// missing, `/spawn npc` is the marked GM fallback (the real spawn is always tried first).
async fn find_or_spawn_npc(
    game: &mut GameClient,
    template_id: u32,
    deadline: Instant,
    journal: &mut QuestJournal,
) -> Result<UnitInfo, StepFailure> {
    let appear = (Instant::now() + APPEAR_WAIT).min(deadline);
    if let Some(npc) = wait_for_target(game, &[template_id], &HashSet::new(), appear).await? {
        return Ok(npc);
    }
    let note = spawn_npc(game, template_id).await?;
    let appear = (Instant::now() + APPEAR_WAIT).min(deadline);
    let Some(npc) = wait_for_target(game, &[template_id], &HashSet::new(), appear).await? else {
        return Err(StepFailure::timeout(format!(
            "{note} but npc {template_id} did not appear"
        )));
    };
    journal.fallback(
        "gm_spawn",
        template_id.to_string(),
        format!(
            "npc {template_id} was not naturally visible at its plan location after {}s",
            APPEAR_WAIT.as_secs()
        ),
    );
    Ok(npc)
}

/// Adds the plan's `scheduled_spawn` note to a spawn failure: the server's `period_status_npc`
/// gates those spawners on a time window the runner cannot move, so this is a requirement (the
/// bot is outside the window), not a runner bug.
fn annotate_scheduled(f: StepFailure, step: &PlanStep) -> StepFailure {
    if step.status == "scheduled_spawn" {
        StepFailure::plan(
            QuestOutcome::RequirementUnmet,
            f.step,
            format!(
                "{} (the plan marks the target scheduled_spawn: the server's spawn window gates \
                 it)",
                f.reason
            ),
        )
    } else {
        f
    }
}

/// The nearest visible doodad of `template_id` (waits [`APPEAR_WAIT`]); when the natural spawn is
/// missing, `/spawn doodad` is the marked GM fallback.
async fn find_or_spawn_doodad(
    game: &mut GameClient,
    template_id: u32,
    deadline: Instant,
    journal: &mut QuestJournal,
) -> Result<DoodadInfo, StepFailure> {
    match wait_for_doodad(game, template_id, deadline).await {
        Ok(doodad) => Ok(doodad),
        Err(failure) if failure.quest == QuestOutcome::Stuck => {
            let doodad = spawn_world_doodad(game, template_id, deadline).await?;
            journal.fallback(
                "gm_spawn",
                template_id.to_string(),
                format!(
                    "doodad {template_id} was not naturally visible at its plan location after \
                     {}s",
                    APPEAR_WAIT.as_secs()
                ),
            );
            Ok(doodad)
        }
        Err(failure) => Err(failure),
    }
}

/// Teleports next to the object (`near` clears the server's interaction range check) when the
/// bot is farther than 4 m from it.
async fn approach(game: &mut GameClient, pos: (f32, f32, f32), deadline: Instant) {
    if dist2(game.state.self_pos, pos) > 16.0 && game.teleport(near(pos)).await.is_ok() {
        settle(game, deadline).await;
    }
}

/// The skill that drives the doodad's current phase, from the compact data: the funcs' own
/// `func_skill_id`, `DoodadFuncFakeUse.fake_skill_id` or `DoodadFuncUse.skill_id`
/// (`func_for_skill` matches exactly these and falls back to the first skill-less func only
/// when none matches; the generic Use skill cannot drive a phase whose funcs carry a skill).
fn doodad_phase_skill(db: &CompactDb, doodad: &DoodadInfo) -> Option<u32> {
    db.doodad_func_skills(doodad.template_id, doodad.func_group_id)
        .ok()
        .and_then(|skills| skills.into_iter().next())
}

async fn step_accept(
    game: &mut GameClient,
    quest_id: u32,
    step: &PlanStep,
    deadline: Instant,
    journal: &mut QuestJournal,
) -> StepRun {
    let npc_template = *step
        .target
        .ids
        .first()
        .ok_or_else(|| StepFailure::unsupported("QuestActConAcceptNpc without a target"))?;
    let pos = step_location(step)?;
    game.teleport(near(pos)).await.map_err(StepFailure::io)?;
    settle(game, deadline).await;
    let npc = find_or_spawn_npc(game, npc_template, deadline, journal)
        .await
        .map_err(|f| annotate_scheduled(f, step))?;
    approach(game, npc.pos, deadline).await;
    game.start_quest(quest_id, npc.obj_id)
        .await
        .map_err(StepFailure::io)?;
    // The `CSStartQuestContextPacket` handler answers synchronously, with `SCErrorMsgPacket`
    // for an already-completed (non-repeatable) quest or a full backpack and *silence* for a
    // duplicate / invalid id / failed start-component requirement (`engine.rs::add_quest`).
    // `SCQuestUnitReqFailedPacket` exists in the 1.2 opcode table but no server sends it, so the
    // only answers are started / SCErrorMsg / nothing.
    let answer_deadline = Instant::now() + ACCEPT_WAIT;
    let found = wait_for_packet(game, answer_deadline, |p| {
        started_for(p, quest_id) || p.is_sc(sc::SCErrorMsgPacket)
    })
    .await
    .map_err(StepFailure::io)?;
    match found {
        Some(p) if started_for(&p, quest_id) => {
            Ok((StepOutcomeKind::Ok, format!("npc {npc_template}")))
        }
        Some(p) => {
            let code = p.stream().read_i16().unwrap_or(0);
            Err(rejected_accept(game, quest_id, "accept", code))
        }
        None if game.state.quests.contains_key(&quest_id) => {
            Ok((StepOutcomeKind::Ok, format!("npc {npc_template} (state)")))
        }
        None => Err(StepFailure::plan(
            QuestOutcome::Rejected,
            StepOutcomeKind::Rejected,
            "no answer from server (accept silently ignored; see engine.rs::add_quest)",
        )),
    }
}

/// `CSStartQuestContextPacket` with the doodad field (`events::add_quest_from_doodad`).
async fn step_accept_doodad(
    game: &mut GameClient,
    quest_id: u32,
    step: &PlanStep,
    deadline: Instant,
    journal: &mut QuestJournal,
) -> StepRun {
    let doodad_template = *step
        .target
        .ids
        .first()
        .ok_or_else(|| StepFailure::unsupported("QuestActConAcceptDoodad without a target"))?;
    let pos = step_location(step)?;
    game.teleport(near(pos)).await.map_err(StepFailure::io)?;
    settle(game, deadline).await;
    let doodad = find_or_spawn_doodad(game, doodad_template, deadline, journal).await?;
    approach(game, doodad.pos, deadline).await;
    game.start_quest_doodad(quest_id, doodad.obj_id)
        .await
        .map_err(StepFailure::io)?;
    let answer_deadline = Instant::now() + ACCEPT_WAIT;
    let found = wait_for_packet(game, answer_deadline, |p| {
        started_for(p, quest_id) || p.is_sc(sc::SCErrorMsgPacket)
    })
    .await
    .map_err(StepFailure::io)?;
    match found {
        Some(p) if started_for(&p, quest_id) => {
            Ok((StepOutcomeKind::Ok, format!("doodad {doodad_template}")))
        }
        Some(p) => {
            let code = p.stream().read_i16().unwrap_or(0);
            Err(rejected_accept(game, quest_id, "accept", code))
        }
        None if game.state.quests.contains_key(&quest_id) => Ok((
            StepOutcomeKind::Ok,
            format!("doodad {doodad_template} (state)"),
        )),
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
    journal: &mut QuestJournal,
) -> StepRun {
    let npc_template = *step
        .target
        .ids
        .first()
        .ok_or_else(|| StepFailure::unsupported("QuestActObjTalk without a target"))?;
    let pos = step_location(step)?;
    game.teleport(near(pos)).await.map_err(StepFailure::io)?;
    settle(game, deadline).await;
    let npc = find_or_spawn_npc(game, npc_template, deadline, journal).await?;
    approach(game, npc.pos, deadline).await;
    game.quest_talk(quest_id, npc.obj_id, step.component_id, step.act_id)
        .await
        .map_err(StepFailure::io)?;
    let want = step.target.count.max(1);
    let advanced = wait_progress(game, quest_id, objective, want, deadline)
        .await
        .map_err(StepFailure::io)?;
    if advanced || quest_completed(game, quest_id) {
        return Ok((StepOutcomeKind::Ok, format!("npc {npc_template}")));
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
    journal: &mut QuestJournal,
) -> StepRun {
    let npc_template = *step
        .target
        .ids
        .first()
        .ok_or_else(|| StepFailure::unsupported("QuestActConReportNpc without a target"))?;
    let pos = step_location(step)?;
    game.teleport(near(pos)).await.map_err(StepFailure::io)?;
    settle(game, deadline).await;
    let npc = find_or_spawn_npc(game, npc_template, deadline, journal)
        .await
        .map_err(|f| annotate_scheduled(f, step))?;
    approach(game, npc.pos, deadline).await;
    game.complete_quest(quest_id, npc.obj_id, 0)
        .await
        .map_err(StepFailure::io)?;
    let found = wait_for_packet(game, deadline, |p| {
        completed_for(p, quest_id) || p.is_sc(sc::SCErrorMsgPacket)
    })
    .await
    .map_err(StepFailure::io)?;
    match found {
        Some(p) if completed_for(&p, quest_id) => {
            Ok((StepOutcomeKind::Ok, format!("npc {npc_template}")))
        }
        Some(p) => Err(StepFailure::rejected(quest_id, "report", &p)),
        None if quest_completed(game, quest_id) => {
            Ok((StepOutcomeKind::Ok, format!("npc {npc_template}")))
        }
        None if quest_failed(game, quest_id) => Err(StepFailure::quest_failed(quest_id)),
        None => Err(StepFailure::timeout(format!(
            "no SCQuestContextCompleted for quest {quest_id}"
        ))),
    }
}

/// `CSCompleteQuestContextPacket` with the doodad field (`events::on_report` doodad branch).
async fn step_report_doodad(
    game: &mut GameClient,
    quest_id: u32,
    step: &PlanStep,
    deadline: Instant,
    journal: &mut QuestJournal,
) -> StepRun {
    let doodad_template = *step
        .target
        .ids
        .first()
        .ok_or_else(|| StepFailure::unsupported("QuestActConReportDoodad without a target"))?;
    let pos = step_location(step)?;
    game.teleport(near(pos)).await.map_err(StepFailure::io)?;
    settle(game, deadline).await;
    let doodad = find_or_spawn_doodad(game, doodad_template, deadline, journal).await?;
    approach(game, doodad.pos, deadline).await;
    game.complete_quest_doodad(quest_id, doodad.obj_id, 0)
        .await
        .map_err(StepFailure::io)?;
    let found = wait_for_packet(game, deadline, |p| {
        completed_for(p, quest_id) || p.is_sc(sc::SCErrorMsgPacket)
    })
    .await
    .map_err(StepFailure::io)?;
    match found {
        Some(p) if completed_for(&p, quest_id) => {
            Ok((StepOutcomeKind::Ok, format!("doodad {doodad_template}")))
        }
        Some(p) => Err(StepFailure::rejected(quest_id, "report", &p)),
        None if quest_completed(game, quest_id) => {
            Ok((StepOutcomeKind::Ok, format!("doodad {doodad_template}")))
        }
        None if quest_failed(game, quest_id) => Err(StepFailure::quest_failed(quest_id)),
        None => Err(StepFailure::timeout(format!(
            "no SCQuestContextCompleted for quest {quest_id}"
        ))),
    }
}

/// `QuestActConReportJournal`: report from the journal, no world object. The server's
/// object-free `CSCompleteQuestContextPacket` (`events.rs::on_report`) runs the automatic report
/// for a Ready component with a `ConReportJournal` act and completes the quest.
async fn step_report_journal(game: &mut GameClient, quest_id: u32, deadline: Instant) -> StepRun {
    game.complete_quest(quest_id, 0, 0)
        .await
        .map_err(StepFailure::io)?;
    let found = wait_for_packet(game, deadline, |p| {
        completed_for(p, quest_id) || p.is_sc(sc::SCErrorMsgPacket)
    })
    .await
    .map_err(StepFailure::io)?;
    match found {
        Some(p) if completed_for(&p, quest_id) => Ok((StepOutcomeKind::Ok, "journal".into())),
        Some(p) => Err(StepFailure::rejected(quest_id, "report", &p)),
        None if quest_completed(game, quest_id) => Ok((StepOutcomeKind::Ok, "journal".into())),
        None if quest_failed(game, quest_id) => Err(StepFailure::quest_failed(quest_id)),
        None => Err(StepFailure::timeout(format!(
            "no SCQuestContextCompleted for quest {quest_id} (journal report)"
        ))),
    }
}

/// `QuestActObjInteraction`: use the doodad with an interaction skill. `InteractionEffect`
/// (`systems/npcs/bridges.rs`) -> `doodad.Use` -> the phase funcs, plus the quest
/// `on_doodad_interaction` event.
async fn step_doodad_interaction(
    game: &mut GameClient,
    quest_id: u32,
    step: &PlanStep,
    objective: Option<u8>,
    compact_db: Option<&CompactDb>,
    deadline: Instant,
    journal: &mut QuestJournal,
) -> StepRun {
    let doodad_template = *step
        .target
        .ids
        .first()
        .ok_or_else(|| StepFailure::unsupported("QuestActObjInteraction without a target"))?;
    let pos = step_location(step)?;
    game.teleport(near(pos)).await.map_err(StepFailure::io)?;
    settle(game, deadline).await;
    let doodad = find_or_spawn_doodad(game, doodad_template, deadline, journal).await?;
    approach(game, doodad.pos, deadline).await;
    let Some(db) = compact_db else {
        return Err(StepFailure::unsupported(
            "interaction skill lookup needs --compact-db (game data)",
        ));
    };
    let Some(generic_skill) = db.interaction_use_skill().map_err(StepFailure::io)? else {
        return Err(StepFailure::unsupported(
            "the game data has no InteractionEffect Use skill",
        ));
    };
    let want = step.target.count.max(1);
    let mut used_skill = generic_skill;
    loop {
        if quest_completed(game, quest_id) || quest_ready(game, quest_id) {
            break;
        }
        if let Some(index) = objective {
            if objective_count(game, quest_id, index) >= want {
                break;
            }
        }
        if quest_failed(game, quest_id) {
            return Err(StepFailure::quest_failed(quest_id));
        }
        if Instant::now() >= deadline {
            return Err(StepFailure::timeout(format!(
                "doodad interaction did not update quest {quest_id}"
            )));
        }
        // The interaction skill's range is ~4 m; the plan location is a spawn point, not the
        // doodad's current position.
        let current = game
            .state
            .doodads
            .get(&doodad.obj_id)
            .cloned()
            .unwrap_or_else(|| doodad.clone());
        approach(game, current.pos, deadline).await;
        // A phase whose funcs carry their own skill (a fake-use step on the way to the quest
        // func) must be driven with it; the generic Use skill matches nothing there.
        used_skill = doodad_phase_skill(db, &current).unwrap_or(generic_skill);
        game.use_doodad(used_skill, doodad.obj_id)
            .await
            .map_err(StepFailure::io)?;
        let until = (Instant::now() + USE_WAIT).min(deadline);
        let _ = wait_progress(game, quest_id, objective, want, until)
            .await
            .map_err(StepFailure::io)?;
    }
    Ok((
        StepOutcomeKind::Ok,
        format!("doodad {doodad_template} skill {used_skill}"),
    ))
}

// ----- crafting (craft item sources) -----

/// How long a craft GM command's effect (item give / labor / doodad spawn) gets to show.
const CRAFT_GM_WAIT: Duration = Duration::from_secs(5);
/// Quiet window after a craft GM command's first reply.
const CRAFT_COMMAND_QUIET: Duration = Duration::from_millis(350);
/// `/item add` accepts at most this many per command (as in the craft runner).
const CRAFT_ITEM_ADD_CHUNK: i32 = 1000;
/// `/labor` takes an i16 amount (`scripts/add_labor.rs`).
const CRAFT_LABOR_MAX: i32 = 32_000;
/// Cap on expert-limit upgrades per craft attempt.
const CRAFT_MAX_EXPERT_UPGRADES: usize = 40;
/// A required craft station within this distance is reused instead of spawning a new one.
const CRAFT_STATION_REUSE_DISTANCE: f32 = 20.0;

/// The actability facts of one craft attempt (`SCCharacterLaborPowerChangedPacket`,
/// `SCActabilityPacket`, `SCExpertLimitModifiedPacket`): the subset of the craft runner's
/// `SessionState` this arm needs. [`craft_pump`] / [`craft_command`] decode the packets they
/// pump, which the runner's plain command helpers would otherwise swallow.
#[derive(Debug, Default)]
struct CraftState {
    points: HashMap<u32, i32>,
    steps: HashMap<u32, u8>,
    /// The last `SCErrorMsg` codes seen while casting (`e1`, `e2`).
    last_error: Option<(i16, i16)>,
}

impl CraftState {
    fn observe(&mut self, p: &Packet) {
        if p.is_sc(sc::SCErrorMsgPacket) {
            let mut s = p.stream();
            if let (Ok(e1), Ok(e2)) = (s.read_i16(), s.read_i16()) {
                self.last_error = Some((e1, e2));
            }
        }
        if p.is_sc(sc::SCActabilityPacket) {
            let mut s = p.stream();
            if s.read_bool().is_ok() {
                if let Ok(n) = s.read_u8() {
                    for _ in 0..n {
                        let (Ok(id), Ok(point), Ok(step)) =
                            (s.read_u32(), s.read_i32(), s.read_u8())
                        else {
                            break;
                        };
                        self.points.insert(id, point);
                        self.steps.insert(id, step);
                    }
                }
            }
        }
        if p.is_sc(sc::SCCharacterLaborPowerChangedPacket) {
            let mut s = p.stream();
            let (Ok(_amount), Ok(action), Ok(point), Ok(step)) =
                (s.read_i32(), s.read_i32(), s.read_i32(), s.read_u8())
            else {
                return;
            };
            if action > 0 && point != 0 {
                *self.points.entry(action as u32).or_insert(0) += point;
                if step > 0 {
                    self.steps.insert(action as u32, step);
                }
            }
        }
        if p.is_sc(sc::SCExpertLimitModifiedPacket) {
            let mut s = p.stream();
            let (Ok(upgrade), Ok(id), Ok(step)) = (s.read_bool(), s.read_u32(), s.read_u8()) else {
                return;
            };
            if upgrade {
                self.steps.insert(id, step);
            }
        }
    }

    fn point(&self, group: u32) -> i32 {
        self.points.get(&group).copied().unwrap_or(0)
    }

    fn step(&self, group: u32) -> u8 {
        self.steps.get(&group).copied().unwrap_or(0)
    }
}

/// Pumps packets (through [`CraftState`] and the game state) until `done` matches or the
/// deadline passes; `Ok(false)` on the deadline. `done` sees the updated facts explicitly, so
/// it never captures the state it inspects.
async fn craft_pump(
    game: &mut GameClient,
    state: &mut CraftState,
    deadline: Instant,
    mut done: impl FnMut(&Packet, &CraftState, &GameState) -> bool,
) -> Result<bool, StepFailure> {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(false);
        }
        let Some(p) = game
            .recv_within(left.min(PUMP))
            .await
            .map_err(StepFailure::io)?
        else {
            continue;
        };
        state.observe(&p);
        if done(&p, state, &game.state) {
            return Ok(true);
        }
    }
}

/// Sends a `/command` chat line, pumps until its reply goes quiet and returns the chat texts
/// (the craft runner's `run_command`; the packets also feed the [`CraftState`]).
async fn craft_command(
    game: &mut GameClient,
    state: &mut CraftState,
    command: &str,
    reply_wait: Duration,
) -> Result<Vec<String>, StepFailure> {
    let text = format!(
        "{}{}",
        aaemu_game::systems::services::commands::COMMAND_PREFIX,
        command
    );
    game.chat(aaemu_game::systems::social::chat::chat_type::WHITE, &text)
        .await
        .map_err(StepFailure::io)?;
    let deadline = Instant::now() + reply_wait;
    let mut messages = Vec::new();
    let mut last_packet: Option<Instant> = None;
    while Instant::now() < deadline {
        match game.recv_within(PUMP).await.map_err(StepFailure::io)? {
            Some(p) => {
                state.observe(&p);
                last_packet = Some(Instant::now());
                if let Some(text) = crate::game::chat_text(&p) {
                    messages.push(text);
                }
            }
            None => {
                if last_packet.is_some_and(|t| t.elapsed() >= CRAFT_COMMAND_QUIET) {
                    break;
                }
            }
        }
    }
    Ok(messages)
}

/// Every item source of the quest, including the nested `container_sources` of `item_open`.
fn quest_item_sources(quest: &PlanQuest) -> Vec<&PlanItemSource> {
    fn walk<'a>(source: &'a PlanItemSource, out: &mut Vec<&'a PlanItemSource>) {
        out.push(source);
        if let Some(nested) = &source.container_sources {
            for child in nested {
                walk(child, out);
            }
        }
    }
    let mut out = Vec::new();
    for step in &quest.steps {
        for entry in step.item_sources.iter().flatten() {
            for source in &entry.sources {
                walk(source, &mut out);
            }
        }
    }
    out
}

/// The level the quest's craft sources need (`PlanSkill.level_min`; the Commerce craft skill
/// 16766 needs 10). `run_quest` adds it to the quest's demand, so its one `/level` covers the
/// crafts and the level is never lowered afterwards.
pub fn craft_level_demand(quest: &PlanQuest, craft_db: Option<&CraftDb>) -> u32 {
    let Some(db) = craft_db else {
        return 0;
    };
    let mut level = 0u32;
    for source in quest_item_sources(quest) {
        if source.kind != "craft" {
            continue;
        }
        for &craft_id in &source.craft_ids {
            if let Some(plan) = db.craft(craft_id) {
                level = level.max(plan.skill.level_min.max(0) as u32);
            }
        }
    }
    level
}

/// The nearest visible craft station of `template_id` within [`CRAFT_STATION_REUSE_DISTANCE`].
fn near_craft_station(game: &GameClient, template_id: u32) -> Option<u32> {
    let me = game.state.self_pos;
    game.state
        .doodads
        .values()
        .filter(|d| d.template_id == template_id)
        .map(|d| (d.obj_id, dist2(d.pos, me)))
        .filter(|(_, d2)| *d2 <= CRAFT_STATION_REUSE_DISTANCE * CRAFT_STATION_REUSE_DISTANCE)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(obj_id, _)| obj_id)
}

/// Finds or spawns the recipe's required doodad (`crafts.req_doodad_id`; 0 = portable).
async fn ensure_craft_doodad(
    game: &mut GameClient,
    state: &mut CraftState,
    template_id: u32,
    deadline: Instant,
) -> Result<Result<u32, String>, StepFailure> {
    if template_id == 0 {
        return Ok(Ok(0));
    }
    if let Some(obj_id) = near_craft_station(game, template_id) {
        return Ok(Ok(obj_id));
    }
    let messages = craft_command(
        game,
        state,
        &format!("spawn doodad {template_id}"),
        CRAFT_GM_WAIT,
    )
    .await?;
    let until = (Instant::now() + CRAFT_GM_WAIT).min(deadline);
    let _ = craft_pump(game, state, until, |_, _, st| {
        near_craft_station_of(st, template_id).is_some()
    })
    .await?;
    Ok(match near_craft_station(game, template_id) {
        Some(obj_id) => Ok(obj_id),
        None => Err(format!(
            "no doodad {template_id} after /spawn doodad (chat: {})",
            messages.join(" | ")
        )),
    })
}

/// [`near_craft_station`] on a plain state (pump predicates).
fn near_craft_station_of(state: &GameState, template_id: u32) -> Option<u32> {
    state
        .doodads
        .values()
        .find(|d| d.template_id == template_id)
        .map(|d| d.obj_id)
}

/// Fills the recipe's materials with `/item add self` until the bag holds them.
async fn ensure_craft_materials(
    game: &mut GameClient,
    state: &mut CraftState,
    plan: &CraftPlan,
    deadline: Instant,
) -> Result<Result<(), String>, StepFailure> {
    for material in &plan.materials {
        if material.amount <= 0 {
            continue;
        }
        let mut have = game.state.bag_count(&[material.item_id]);
        while have < material.amount {
            let chunk = (material.amount - have).min(CRAFT_ITEM_ADD_CHUNK);
            let messages = craft_command(
                game,
                state,
                &format!("item add self {} {chunk}", material.item_id),
                CRAFT_GM_WAIT,
            )
            .await?;
            let until = (Instant::now() + CRAFT_GM_WAIT).min(deadline);
            let _ = craft_pump(game, state, until, |_, _, st| {
                st.bag_count(&[material.item_id]) > have
            })
            .await?;
            let after = game.state.bag_count(&[material.item_id]);
            if after <= have {
                return Ok(Err(format!(
                    "GM /item add created no {} (had {have}, wanted {}; chat: {})",
                    material.item_id,
                    material.amount,
                    messages.join(" | ")
                )));
            }
            have = after;
        }
    }
    Ok(Ok(()))
}

/// Raises the recipe's actability points to its limit, upgrading the expert-limit step with the
/// real client's `CSUpgradeExpertLimitPacket` when the step cap is reached (the craft runner's
/// `ensure_actability`).
async fn ensure_craft_actability(
    game: &mut GameClient,
    state: &mut CraftState,
    plan: &CraftPlan,
    expert_caps: &[i32],
    deadline: Instant,
) -> Result<Result<(), String>, StepFailure> {
    let group = plan.skill.actability_group.max(0) as u32;
    let limit = plan.actability_limit;
    if limit <= 0 || group == 0 {
        return Ok(Ok(()));
    }
    for _ in 0..CRAFT_MAX_EXPERT_UPGRADES {
        let point = state.point(group);
        if point >= limit {
            return Ok(Ok(()));
        }
        let step = state.step(group);
        let cap = expert_caps.get(usize::from(step)).copied();
        let room = cap.map(|c| c.saturating_sub(point));
        let add = match room {
            Some(r) if r > 0 => (limit - point).min(r).min(CRAFT_LABOR_MAX),
            _ => {
                // At the step cap: the client's upgrade packet is the only way further.
                let Some(cap) = cap else {
                    return Ok(Err(format!(
                        "actability {group} is at step {step} with no expert-limit cap (point \
                         {point}, need {limit})"
                    )));
                };
                if point < cap {
                    return Ok(Err(format!(
                        "actability {group} stuck at {point}/{cap} (step {step})"
                    )));
                }
                let mut body = PacketStream::new();
                body.write_u32(group).write_bool(false);
                game.send_cs(cs::CSUpgradeExpertLimitPacket, body.as_bytes())
                    .await
                    .map_err(StepFailure::io)?;
                let until = (Instant::now() + CRAFT_GM_WAIT).min(deadline);
                if !craft_pump(game, state, until, |p, _, _| {
                    p.is_sc(sc::SCExpertLimitModifiedPacket)
                })
                .await?
                {
                    return Ok(Err(format!(
                        "expert-limit upgrade of actability {group} was not answered (step {step})"
                    )));
                }
                continue;
            }
        };
        if add <= 0 {
            return Ok(Err(format!(
                "actability {group} cannot advance from point {point} to {limit}"
            )));
        }
        craft_command(game, state, &format!("labor {add} {group}"), CRAFT_GM_WAIT).await?;
        let until = (Instant::now() + CRAFT_GM_WAIT).min(deadline);
        let want = point + add;
        let _ = craft_pump(game, state, until, |_, craft_state, _| {
            craft_state.point(group) >= want
        })
        .await?;
    }
    Ok(Err(format!(
        "actability {group} still below {limit} after {CRAFT_MAX_EXPERT_UPGRADES} upgrades"
    )))
}

/// Tops up the labor power for one craft (`Skill.EndSkill` charges `consume_lp`).
async fn ensure_craft_labor(
    game: &mut GameClient,
    state: &mut CraftState,
    plan: &CraftPlan,
    deadline: Instant,
) -> Result<Result<(), String>, StepFailure> {
    let group = plan.skill.actability_group.max(0) as u32;
    let needed = craftrun::labor_cost(plan.skill.consume_lp, state.step(group)).max(1);
    let add = needed.min(CRAFT_LABOR_MAX);
    craft_command(game, state, &format!("labor {add}"), CRAFT_GM_WAIT).await?;
    let _ = deadline;
    Ok(Ok(()))
}

/// One attempt to make the recipe executable: materials, actability, labor. `Ok(Err(reason))`
/// when the recipe cannot be prepared (the caller tries the next one).
async fn ensure_craft_ready(
    game: &mut GameClient,
    state: &mut CraftState,
    plan: &CraftPlan,
    expert_caps: &[i32],
    deadline: Instant,
) -> Result<Result<(), String>, StepFailure> {
    if let Err(reason) = ensure_craft_materials(game, state, plan, deadline).await? {
        return Ok(Err(reason));
    }
    if let Err(reason) = ensure_craft_actability(game, state, plan, expert_caps, deadline).await? {
        return Ok(Err(reason));
    }
    ensure_craft_labor(game, state, plan, deadline).await
}

/// `craft` item sources: make the item with the recipe's own flow (the craft runner's):
/// `/item add self` for the materials, `/labor [group]` + `CSUpgradeExpertLimitPacket` for the
/// actability, `/labor` for the labor, `/spawn doodad` for `crafts.req_doodad_id`, then
/// `CSExecuteCraftPacket(craftId, doodadObjId, count)` (`systems/economy/craft.rs`; the cast's
/// `CraftEffect` -> `EndCraft` consumes the materials and creates the products).
#[allow(clippy::too_many_arguments)]
async fn craft_source(
    game: &mut GameClient,
    quest_id: u32,
    ids: &[u32],
    want: i32,
    objective: Option<u8>,
    craft_ids: &[u32],
    craft_db: Option<&CraftDb>,
    deadline: Instant,
) -> Result<Option<String>, StepFailure> {
    let Some(db) = craft_db else {
        return Ok(Some("craft (needs --compact-db game data)".into()));
    };
    let mut last_note = None;
    for &craft_id in craft_ids {
        let Some(plan) = db.craft(craft_id) else {
            last_note = Some(format!("craft {craft_id} is not in the game data"));
            continue;
        };
        if !db.is_usable(plan) {
            last_note = Some(format!(
                "craft {craft_id} ({}) has a data gap (materials / products / station missing)",
                plan.title
            ));
            continue;
        }
        if plan.is_pack {
            last_note = Some(format!(
                "craft {craft_id} ({}) is a trade-pack recipe",
                plan.title
            ));
            continue;
        }
        let mut state = CraftState::default();
        let mut crafted = 0usize;
        let mut failure: Option<String> = None;
        while !gather_done(&game.state, ids, want, quest_id, objective) && Instant::now() < deadline
        {
            if let Err(reason) =
                ensure_craft_ready(game, &mut state, plan, &db.expert_caps, deadline).await?
            {
                failure = Some(reason);
                break;
            }
            let doodad =
                match ensure_craft_doodad(game, &mut state, plan.req_doodad_id, deadline).await? {
                    Ok(obj_id) => obj_id,
                    Err(reason) => {
                        failure = Some(reason);
                        break;
                    }
                };
            let before_bag = game.state.bag_count(ids);
            let before_objective = objective.map(|i| objective_count(game, quest_id, i));
            state.last_error = None;
            let mut body = PacketStream::new();
            body.write_u32(craft_id).write_bc(doodad).write_i32(1);
            game.send_cs(cs::CSExecuteCraft, body.as_bytes())
                .await
                .map_err(StepFailure::io)?;
            crafted += 1;
            // The cast runs `cast_delay` and then `EndCraft` hands out the products; watch
            // until they land (bag / objective), an error arrives, or the cast window closes.
            let cast =
                Duration::from_millis(plan.cast_delay_ms.max(0) as u64) + Duration::from_secs(2);
            let until = (Instant::now() + cast).min(deadline);
            let _ = craft_pump(game, &mut state, until, |p, _, st| {
                p.is_sc(sc::SCErrorMsgPacket)
                    || st.bag_count(ids) > before_bag
                    || objective.is_some_and(|i| {
                        objective_count_of(st, quest_id, i) > before_objective.unwrap_or(0)
                    })
            })
            .await?;
            if state.last_error.is_some() {
                failure = Some(format!(
                    "craft {craft_id} answered SCErrorMsg {:?}",
                    state.last_error
                ));
                break;
            }
        }
        if gather_done(&game.state, ids, want, quest_id, objective) {
            return Ok(Some(format!("craft {craft_id} x{crafted}")));
        }
        last_note = Some(
            failure.unwrap_or_else(|| format!("craft {craft_id} x{crafted} made no progress")),
        );
    }
    Ok(last_note)
}

// ----- item steps -----

/// Every item source of the step's target ids, in plan order.
fn gather_sources(step: &PlanStep) -> Vec<&PlanItemSource> {
    let Some(entries) = &step.item_sources else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries {
        if !step.target.ids.contains(&entry.item_id) {
            continue;
        }
        out.extend(entry.sources.iter());
    }
    out
}

/// The locations of a source in the step's world (`world` = [`step_world`], `None` =
/// `main_world`): the bot was switched there before the step ran.
fn source_positions(source: &PlanItemSource, world: Option<&str>) -> Vec<(f32, f32, f32)> {
    source
        .locations
        .iter()
        .filter(|l| match world {
            Some(w) => l.world.eq_ignore_ascii_case(w),
            None => l.world.eq_ignore_ascii_case(MAIN_WORLD),
        })
        .map(|l| (l.x, l.y, l.z))
        .collect()
}

/// A gather step is done when the bag holds `want` of `ids` *or* the act's quest objective
/// counter reached `want` (whichever comes first), the quest completed, or the server marked it
/// READY (let-it-done / score quests can turn in before every objective is full).
fn gather_done(
    state: &GameState,
    ids: &[u32],
    want: i32,
    quest_id: u32,
    objective: Option<u8>,
) -> bool {
    state.bag_count(ids) >= want
        || objective.is_some_and(|index| objective_count_of(state, quest_id, index) >= want)
        || state.last_completed == Some(quest_id)
        || quest_ready_state(state, quest_id)
}

/// The success note of a gather step: the path plus both counters.
fn gather_note(
    state: &GameState,
    ids: &[u32],
    want: i32,
    quest_id: u32,
    objective: Option<u8>,
    path: &str,
) -> String {
    let bag = state.bag_count(ids);
    match objective {
        Some(index) => format!(
            "{path} (bag {bag}/{want}, objective {}/{want})",
            objective_count_of(state, quest_id, index)
        ),
        None => format!("{path} (bag {bag}/{want})"),
    }
}

/// Pump until the gather goal is reached or the deadline passes.
async fn wait_for_gather(
    game: &mut GameClient,
    quest_id: u32,
    ids: &[u32],
    want: i32,
    objective: Option<u8>,
    deadline: Instant,
) -> anyhow::Result<bool> {
    loop {
        if gather_done(&game.state, ids, want, quest_id, objective) {
            return Ok(true);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(false);
        }
        game.recv_within(left.min(PUMP)).await?;
    }
}

/// `QuestActObjItemGather` / `QuestActEtcItemObtain` / `QuestActConAcceptItemGain`: ensure the
/// item count in the bag (or the act's quest objective counter; both are reported in the
/// note). Sources are tried in plan order (`quest_supply` waits, `npc_loot` kills a dropper and
/// loots the corpse with the real loot packets, `doodad` uses the loot doodad); after the
/// deadline a dropper is `/spawn`ed (`ok_gm_spawn`, never silent).
#[allow(clippy::too_many_arguments)]
async fn step_gather(
    game: &mut GameClient,
    quest_id: u32,
    step: &PlanStep,
    objective: Option<u8>,
    compact_db: Option<&CompactDb>,
    craft_db: Option<&CraftDb>,
    deadline: Instant,
    journal: &mut QuestJournal,
    needed: &BTreeSet<u32>,
) -> StepRun {
    let ids = &step.target.ids;
    let want = step.target.count.max(1);
    if ids.is_empty() {
        return Err(StepFailure::unsupported("item step without item ids"));
    }
    if gather_done(&game.state, ids, want, quest_id, objective) {
        return Ok((
            StepOutcomeKind::Ok,
            gather_note(
                &game.state,
                ids,
                want,
                quest_id,
                objective,
                "already satisfied",
            ),
        ));
    }

    // The plan's sources first; targets the plan could not resolve fall back to the same
    // reverse lookups made against the compact database (the client's own game data).
    let mut sources: Vec<&PlanItemSource> = gather_sources(step);
    let compact_sources = if sources.is_empty() {
        compact_item_sources(compact_db, ids)
    } else {
        Vec::new()
    };
    sources.extend(compact_sources.iter());
    // Escalation ladder: quest supply -> doodad loot -> NPC drops -> merchants -> crafting ->
    // containers -> other quests' rewards (reported only).
    sources.sort_by_key(|source| source_order(source));
    let mut attempts: Vec<SourceAttempt> = Vec::new();
    if let Some(db) = compact_db {
        for item_id in ids {
            if let Ok(quests) = db.item_other_quest_rewards(*item_id) {
                if !quests.is_empty() {
                    attempts.push(SourceAttempt::new(
                        format!("other_quest_reward item {item_id}"),
                        format!("other quests reward it: {quests:?} (reported, not driven)"),
                        blocker_class::DATA_GAP,
                        "compact.sqlite3 quest_act_supply_items (report only)",
                    ));
                }
            }
        }
    }

    let last_note = if sources.is_empty() {
        None
    } else {
        gather_from_sources(
            game,
            quest_id,
            ids,
            want,
            objective,
            &sources,
            step,
            compact_db,
            craft_db,
            deadline,
            0,
            journal,
            &mut attempts,
            needed,
        )
        .await?
    }
    .or_else(|| Some(String::from("no real source produced the item")));
    if gather_done(&game.state, ids, want, quest_id, objective) {
        return Ok((
            StepOutcomeKind::Ok,
            gather_note(
                &game.state,
                ids,
                want,
                quest_id,
                objective,
                last_note.as_deref().unwrap_or("item source"),
            ),
        ));
    }

    // Wait out the rest of the step: respawns / delayed supplies may still arrive. When there
    // are no sources at all the wait is skipped (the GM fallback follows immediately).
    if !sources.is_empty() {
        let _ = wait_for_gather(game, quest_id, ids, want, objective, deadline)
            .await
            .map_err(StepFailure::io)?;
        if gather_done(&game.state, ids, want, quest_id, objective) {
            return Ok((
                StepOutcomeKind::Ok,
                gather_note(
                    &game.state,
                    ids,
                    want,
                    quest_id,
                    objective,
                    &format!("respawn: {}", last_note.as_deref().unwrap_or("item source")),
                ),
            ));
        }
        if quest_failed(game, quest_id) {
            return Err(StepFailure::quest_failed(quest_id));
        }
    }

    // Every real source failed: the GM item is the last resort (marked). The fallback gets a
    // fresh window: the step deadline is already used up by the source attempts.
    let attempts_note = summarize_attempts(&attempts);
    journal.note_source_attempts(&attempts);
    if attempts.is_empty() {
        journal.note_blocker(
            blocker_class::DATA_GAP,
            format!(
                "item(s) {ids:?} have no source in the plan or in the 1.2 compact data \
                 (loots / doodad_func_loot_items / merchant_goods / craft_products)"
            ),
            "compact.sqlite3 (1.2 item source tables)",
        );
    }
    let missing = missing_item_count(&game.state, ids, want, quest_id, objective);
    if missing > 0 {
        let item_id = ids[0];
        let fallback_deadline = Instant::now() + GM_ITEM_WINDOW;
        let added = gm_item_add(game, item_id, missing, fallback_deadline, needed, journal).await?;
        journal.fallback(
            "gm_item",
            item_id.to_string(),
            format!("every real source failed: {attempts_note}; /item add self {item_id} {added}"),
        );
        let _ = wait_for_gather(game, quest_id, ids, want, objective, fallback_deadline)
            .await
            .map_err(StepFailure::io)?;
        if gather_done(&game.state, ids, want, quest_id, objective) {
            return Ok((
                StepOutcomeKind::OkGmItem,
                gather_note(
                    &game.state,
                    ids,
                    want,
                    quest_id,
                    objective,
                    &format!("gm_item item {item_id} x{added}"),
                ),
            ));
        }
    }
    Err(StepFailure::timeout(format!(
        "item(s) {ids:?} not obtained: {}",
        last_note.as_deref().unwrap_or("no source")
    )))
}

/// The real-source ladder rank (quest supply first, containers last).
fn source_order(source: &PlanItemSource) -> (u8, u32) {
    let rank = match source.kind.as_str() {
        "quest_supply" => 0,
        "doodad" => 1,
        "npc_loot" => 2,
        "store" => 3,
        "craft" => 4,
        "item_open" => 5,
        "quest_reward" => 6,
        _ => 7,
    };
    (rank, source.loot_pack_id.max(source.container_item_id))
}

fn summarize_attempts(attempts: &[SourceAttempt]) -> String {
    if attempts.is_empty() {
        return "no real source".into();
    }
    attempts
        .iter()
        .map(|attempt| format!("{}: {}", attempt.source, attempt.reason))
        .collect::<Vec<_>>()
        .join("; ")
}

/// How many of the target items are still missing (bag and objective counter).
fn missing_item_count(
    state: &GameState,
    ids: &[u32],
    want: i32,
    quest_id: u32,
    objective: Option<u8>,
) -> i32 {
    if state.last_completed == Some(quest_id) {
        return 0;
    }
    let objective_have = objective
        .map(|index| objective_count_of(state, quest_id, index))
        .unwrap_or(0);
    (want - state.bag_count(ids).max(objective_have)).max(0)
}

/// The plan's `item_open` sources for these targets, resolved from `compact.sqlite3` (the same
/// reverse lookups `plan.py` makes), for items the plan flagged `unknown_item_source`.
fn compact_item_sources(compact_db: Option<&CompactDb>, ids: &[u32]) -> Vec<PlanItemSource> {
    let Some(db) = compact_db else {
        return Vec::new();
    };
    compact_item_sources_at(db, ids, 0)
}

fn compact_item_sources_at(db: &CompactDb, ids: &[u32], depth: u32) -> Vec<PlanItemSource> {
    let mut out = Vec::new();
    for &item_id in ids {
        for loot in db.item_loot_sources(item_id).unwrap_or_default() {
            out.push(PlanItemSource {
                kind: "npc_loot".into(),
                npc_ids: loot.npc_ids,
                loot_pack_id: loot.loot_pack_id,
                chance: loot.chance,
                loot_quest_id: loot.loot_quest_id,
                ..PlanItemSource::default()
            });
        }
        for doodad in db.item_doodad_sources(item_id).unwrap_or_default() {
            out.push(PlanItemSource {
                kind: "doodad".into(),
                doodad_ids: doodad.doodad_ids,
                func: Some(doodad.func),
                ..PlanItemSource::default()
            });
        }
        let crafts = db.item_craft_ids(item_id).unwrap_or_default();
        if !crafts.is_empty() {
            out.push(PlanItemSource {
                kind: "craft".into(),
                craft_ids: crafts,
                ..PlanItemSource::default()
            });
        }
        for store in db.item_store_sources(item_id).unwrap_or_default() {
            out.push(PlanItemSource {
                kind: "store".into(),
                npc_ids: store.npc_ids,
                pack_ids: store.pack_ids,
                ..PlanItemSource::default()
            });
        }
        if depth < ITEM_OPEN_DEPTH {
            for container in db.item_container_sources(item_id).unwrap_or_default() {
                if container.container_item_id == item_id {
                    continue;
                }
                let nested = compact_item_sources_at(db, &[container.container_item_id], depth + 1);
                out.push(PlanItemSource {
                    kind: "item_open".into(),
                    container_item_id: container.container_item_id,
                    skill_id: container.skill_id,
                    grant: container.grant,
                    loot_pack_id: container.loot_pack_id,
                    chance: container.chance,
                    container_sources: Some(nested),
                    ..PlanItemSource::default()
                });
            }
        }
    }
    out
}

/// Tries the sources in ladder order ([`gather_source`] each) until the gather goal is reached
/// or the deadline passes. Failed attempts are appended to `attempts` (the blocker evidence);
/// `Ok(Some(note))` is the note of the last source that may have changed the bag. `item_open`
/// recurses through here for the container's own sources, so the future is boxed.
#[allow(clippy::too_many_arguments)]
fn gather_from_sources<'a>(
    game: &'a mut GameClient,
    quest_id: u32,
    ids: &'a [u32],
    want: i32,
    objective: Option<u8>,
    sources: &'a [&'a PlanItemSource],
    step: &'a PlanStep,
    compact_db: Option<&'a CompactDb>,
    craft_db: Option<&'a CraftDb>,
    deadline: Instant,
    depth: u32,
    journal: &'a mut QuestJournal,
    attempts: &'a mut Vec<SourceAttempt>,
    needed: &'a BTreeSet<u32>,
) -> LocalBoxFuture<'a, Result<Option<String>, StepFailure>> {
    Box::pin(async move {
        let mut last_note = None;
        for &source in sources {
            if gather_done(&game.state, ids, want, quest_id, objective)
                || Instant::now() >= deadline
            {
                break;
            }
            match gather_source(
                game, quest_id, ids, want, objective, source, step, compact_db, craft_db, deadline,
                depth, journal, needed,
            )
            .await
            {
                Ok(SourceOutcome::Progress(note)) => last_note = note.or(last_note),
                Ok(SourceOutcome::Failed(attempt)) => {
                    last_note = Some(format!("{}: {}", attempt.source, attempt.reason));
                    attempts.push(attempt);
                }
                Err(f) => {
                    if f.quest != QuestOutcome::Stuck {
                        return Err(f);
                    }
                    last_note = Some(f.reason);
                }
            }
        }
        Ok(last_note)
    })
}

/// What one item-source attempt did.
enum SourceOutcome {
    /// The source ran and may have changed the bag (the note names the path).
    Progress(Option<String>),
    /// The source could not deliver; the attempt records whose fault it is.
    Failed(SourceAttempt),
}

/// One attempt at one item source, following the escalation ladder. Real sources are always
/// tried before the GM shortcut; every failed attempt returns a concrete reason and class.
#[allow(clippy::too_many_arguments)]
async fn gather_source(
    game: &mut GameClient,
    quest_id: u32,
    ids: &[u32],
    want: i32,
    objective: Option<u8>,
    source: &PlanItemSource,
    step: &PlanStep,
    compact_db: Option<&CompactDb>,
    craft_db: Option<&CraftDb>,
    deadline: Instant,
    depth: u32,
    journal: &mut QuestJournal,
    needed: &BTreeSet<u32>,
) -> Result<SourceOutcome, StepFailure> {
    match source.kind.as_str() {
        "quest_supply" => {
            let act = source.act_id.unwrap_or(0);
            let until = (Instant::now() + SOURCE_WAIT).min(deadline);
            let _ = wait_for_gather(game, quest_id, ids, want, objective, until)
                .await
                .map_err(StepFailure::io)?;
            if gather_done(&game.state, ids, want, quest_id, objective) {
                return Ok(SourceOutcome::Progress(Some(format!(
                    "quest_supply act {act}"
                ))));
            }
            Ok(SourceOutcome::Failed(SourceAttempt::suspect(
                format!("quest_supply act {act}"),
                format!(
                    "the supply act gave none of item(s) {ids:?} in {}s",
                    SOURCE_WAIT.as_secs()
                ),
                "aaemu-game/src/systems/quests/acts.rs (QuestActSupplyItem)",
                format!(
                    "observed: no SCItemTaskSuccess for item(s) {ids:?} within {}s of the act \
                     running; expected: the quest's own supply act hands the items over \
                     (compact quest_act_supply_items / acts.rs::QuestActSupplyItem)",
                    SOURCE_WAIT.as_secs()
                ),
            )))
        }
        "npc_loot" => {
            if source.npc_ids.is_empty() {
                return Ok(SourceOutcome::Failed(SourceAttempt::new(
                    "npc_loot",
                    format!(
                        "loot pack {} has no dropping NPC in the data",
                        source.loot_pack_id
                    ),
                    blocker_class::DATA_GAP,
                    "compact.sqlite3 loot_pack_dropping_npcs",
                )));
            }
            // A quest-bound drop (`items.loot_quest_id`) only rolls while that quest is active
            // (`loot.rs::try_take_loot_locked` HasQuest gate, as the C#): without the quest the
            // kills can never produce the item.
            if let Some(bound) = source.loot_quest_id {
                let active = bound == quest_id || game.state.quests.contains_key(&bound);
                if !active {
                    let repeatable = compact_db
                        .and_then(|db| db.quest_repeatable(bound).ok().flatten())
                        .unwrap_or(false);
                    let completed = game.state.completed_quests.contains(&bound);
                    let (class, code) = if completed && !repeatable {
                        (
                            blocker_class::REQUIREMENT,
                            "compact.sqlite3 items.loot_quest_id (completed, not repeatable)",
                        )
                    } else {
                        (
                            blocker_class::RUNNER_GAP,
                            "aaemu-testclient/src/questrun.rs::step_gather (quest not active) / \
                             aaemu-game/src/systems/economy/loot.rs:243 try_take_loot_locked \
                             (HasQuest(loot_quest_id))",
                        )
                    };
                    return Ok(SourceOutcome::Failed(SourceAttempt::new(
                        format!("npc_loot pack {}", source.loot_pack_id),
                        format!(
                            "item(s) {ids:?} have loot_quest_id {bound} and quest {bound} is {}; \
                             the drop only rolls while it is active",
                            if completed {
                                "already completed"
                            } else {
                                "not active in this run (run it first)"
                            }
                        ),
                        class,
                        code,
                    )));
                }
            }
            let positions = source_positions(source, step_world(step).as_deref());
            let window = (Instant::now() + SOURCE_WINDOW).min(deadline);
            let mut kills = 0usize;
            let mut spawned = false;
            while !gather_done(&game.state, ids, want, quest_id, objective)
                && Instant::now() < window
            {
                let goal = KillGoal::Kills { want: 1 };
                let Some((_, killed)) =
                    hunt_until(game, &source.npc_ids, &positions, goal, window).await?
                else {
                    break;
                };
                kills += killed.len();
                for corpse in killed {
                    game.loot_all(corpse).await.map_err(StepFailure::io)?;
                    game.close_loot_bag(corpse).await.map_err(StepFailure::io)?;
                }
                let until = (Instant::now() + LOOT_WAIT).min(window);
                let _ = wait_for_gather(game, quest_id, ids, want, objective, until)
                    .await
                    .map_err(StepFailure::io)?;
            }
            // No natural dropper reached: spawn one (marked) and kill/loot it like the natural
            // path, up to five corpses.
            if !gather_done(&game.state, ids, want, quest_id, objective)
                && Instant::now() < deadline
            {
                for &npc_id in source.npc_ids.iter().take(3) {
                    if gather_done(&game.state, ids, want, quest_id, objective) {
                        break;
                    }
                    let mut avoid: HashSet<u32> = HashSet::new();
                    for _ in 0..5 {
                        if gather_done(&game.state, ids, want, quest_id, objective)
                            || Instant::now() >= deadline
                        {
                            break;
                        }
                        let target = match nearest_target(game, &[npc_id], &avoid) {
                            Some(npc) => Some(npc),
                            None => {
                                spawn_npc(game, npc_id).await?;
                                let appear = (Instant::now() + APPEAR_WAIT).min(deadline);
                                let found =
                                    wait_for_target(game, &[npc_id], &avoid, appear).await?;
                                if !spawned && found.is_some() {
                                    spawned = true;
                                    journal.fallback(
                                        "gm_spawn",
                                        npc_id.to_string(),
                                        format!(
                                            "no natural spawn of npc {npc_id} at {} plan \
                                             location(s) in {}s",
                                            positions.len(),
                                            SOURCE_WINDOW.as_secs()
                                        ),
                                    );
                                }
                                found
                            }
                        };
                        let Some(npc) = target else {
                            break;
                        };
                        game.change_target(npc.obj_id)
                            .await
                            .map_err(StepFailure::io)?;
                        let messages = game.gm("kill", KILL_WAIT).await.map_err(StepFailure::io)?;
                        avoid.insert(npc.obj_id);
                        if messages
                            .iter()
                            .any(|m| m.contains("already dead") || m.contains("Cannot kill"))
                        {
                            continue;
                        }
                        kills += 1;
                        game.loot_all(npc.obj_id).await.map_err(StepFailure::io)?;
                        game.close_loot_bag(npc.obj_id)
                            .await
                            .map_err(StepFailure::io)?;
                        let until = (Instant::now() + LOOT_WAIT).min(deadline);
                        let _ = wait_for_gather(game, quest_id, ids, want, objective, until)
                            .await
                            .map_err(StepFailure::io)?;
                    }
                    if kills > 0 {
                        break;
                    }
                }
            }
            if gather_done(&game.state, ids, want, quest_id, objective) {
                return Ok(SourceOutcome::Progress(Some(format!(
                    "npc_loot pack {} ({kills} kill(s){})",
                    source.loot_pack_id,
                    if spawned { ", gm_spawn" } else { "" }
                ))));
            }
            if kills > 0 {
                // A full bag can swallow the loot silently (`give_loot_pack`'s room check);
                // that is the runner's state, not a suspect.
                let capacity = bag_capacity(&game.state);
                let free = capacity.saturating_sub(game.state.bag.len() as u32);
                if free < 3 {
                    return Ok(SourceOutcome::Failed(SourceAttempt::new(
                        format!("npc_loot pack {}", source.loot_pack_id),
                        format!(
                            "loot pack {} rolled nothing in {kills} kill(s); the bag had only \
                             {free}/{capacity} free slot(s)",
                            source.loot_pack_id
                        ),
                        blocker_class::RUNNER_GAP,
                        "aaemu-testclient/src/questrun.rs::bag_capacity (bag full)",
                    )));
                }
                return Ok(SourceOutcome::Failed(SourceAttempt::suspect(
                    format!("npc_loot pack {}", source.loot_pack_id),
                    format!(
                        "loot pack {} rolled nothing in {kills} kill(s) of npc(s) {:?}{}",
                        source.loot_pack_id,
                        source.npc_ids,
                        if spawned { " (GM-spawned)" } else { "" }
                    ),
                    code::LOOT_ROLL,
                    format!(
                        "observed: {kills} kill(s) credited to the bot and no \
                         SCItemTaskSuccess for item(s) {ids:?}; expected: loots.drop_rate {:?} \
                         on pack {} grants the item on the kill's loot roll (plan item_sources \
                         / compact loots; loot.rs::generate_loot/try_take_loot)",
                        source.chance, source.loot_pack_id
                    ),
                )));
            }
            if positions.is_empty() {
                return Ok(SourceOutcome::Failed(SourceAttempt::new(
                    format!("npc_loot pack {}", source.loot_pack_id),
                    format!(
                        "no npc of template(s) {:?} was visible and the plan has no location \
                         for it",
                        source.npc_ids
                    ),
                    blocker_class::DATA_GAP,
                    "plan.py / Data/Worlds/world_spawns.json",
                )));
            }
            Ok(SourceOutcome::Failed(SourceAttempt::suspect(
                format!("npc_loot pack {}", source.loot_pack_id),
                format!(
                    "no npc of template(s) {:?} was visible at {} plan location(s) and the GM \
                     spawn produced none",
                    source.npc_ids,
                    positions.len()
                ),
                "aaemu-game/src/systems/npcs/spawn_manager.rs",
                format!(
                    "observed: no unit of template(s) {:?} at {} plan location(s) and none \
                     after /spawn npc; expected: the world spawn data (or the GM spawn) creates \
                     the NPC (plan locations from world_spawns.json; spawn_manager)",
                    source.npc_ids,
                    positions.len()
                ),
            )))
        }
        "doodad" => {
            let func = source.func.as_deref().unwrap_or("DoodadFuncLootItem");
            if source.doodad_ids.is_empty() {
                return Ok(SourceOutcome::Failed(SourceAttempt::new(
                    format!("doodad {func}"),
                    "the data has no doodad template for this item".to_string(),
                    blocker_class::DATA_GAP,
                    "compact.sqlite3 doodad_func_loot_items / doodad_func_loot_packs",
                )));
            }
            let Some(db) = compact_db else {
                return Ok(SourceOutcome::Failed(SourceAttempt::new(
                    format!("doodad {func}"),
                    "needs --compact-db (the interaction skill)".to_string(),
                    blocker_class::RUNNER_GAP,
                    code::RUNNER,
                )));
            };
            let Some(generic_skill) = db.interaction_use_skill().map_err(StepFailure::io)? else {
                return Ok(SourceOutcome::Failed(SourceAttempt::new(
                    format!("doodad {func}"),
                    "the game data has no InteractionEffect Use skill".to_string(),
                    blocker_class::DATA_GAP,
                    "compact.sqlite3 interaction_effects (wi_id 19)",
                )));
            };
            let positions = source_positions(source, step_world(step).as_deref());
            let mut used: HashSet<u32> = HashSet::new();
            // Uses per doodad object while its phase stays unchanged (a failed loot roll).
            let mut retry_counts: HashMap<u32, u32> = HashMap::new();
            let mut uses = 0usize;
            let mut last_template = 0u32;
            // The last used doodad's phase carried its own skill (a fake-use / growth step): the
            // loot phase is behind it, which is the runner's limit, not a server answer.
            let mut last_had_skill = false;
            let mut spawned = false;
            let mut next_location = 0usize;
            while !gather_done(&game.state, ids, want, quest_id, objective)
                && Instant::now() < deadline
            {
                let visible = find_unused_doodad(game, &source.doodad_ids, &used);
                let doodad = match visible {
                    Some(doodad) => doodad,
                    None => {
                        if !positions.is_empty() {
                            let pos = positions[next_location % positions.len()];
                            next_location += 1;
                            game.teleport(near(pos)).await.map_err(StepFailure::io)?;
                            settle(game, deadline).await;
                        }
                        let appear = (Instant::now() + APPEAR_WAIT).min(deadline);
                        match wait_for_unused_doodad(game, &source.doodad_ids, &used, appear)
                            .await?
                        {
                            Some(doodad) => doodad,
                            None if spawned => break,
                            None => {
                                let template = source.doodad_ids.first().copied().unwrap_or(0);
                                if template == 0 {
                                    break;
                                }
                                let doodad = spawn_world_doodad(game, template, deadline).await?;
                                spawned = true;
                                journal.fallback(
                                    "gm_spawn",
                                    template.to_string(),
                                    format!(
                                        "no natural doodad {template} for {func} at {} plan \
                                         location(s) in {}s",
                                        positions.len(),
                                        SOURCE_WINDOW.as_secs()
                                    ),
                                );
                                doodad
                            }
                        }
                    }
                };
                last_template = doodad.template_id;
                // The plan position is the spawn point, not the doodad's current position; the
                // interaction skill's ~4 m range check needs the real position.
                approach(game, doodad.pos, deadline).await;
                if func == "DoodadFuncPurchase" {
                    let count =
                        missing_item_count(&game.state, ids, want, quest_id, objective).max(1);
                    return match buy_item_at(game, 0, doodad.obj_id, ids[0], count, deadline).await
                    {
                        Ok(note) => Ok(SourceOutcome::Progress(Some(note))),
                        Err(f) => Ok(SourceOutcome::Failed(SourceAttempt::suspect(
                            format!("doodad {func}"),
                            f.reason,
                            "aaemu-game/src/systems/economy/store.rs:242 (CSBuyItemsPacket)",
                            format!(
                                "observed: CSBuyItemsPacket(doodad {}, item {}) and no \
                                 SCItemTaskSuccess; expected: the purchase doodad sells item \
                                 {} (compact doodad_func_purchases.item_id)",
                                doodad.obj_id, ids[0], ids[0]
                            ),
                        ))),
                    };
                }
                let before_phase = doodad.func_group_id;
                // Phase funcs with their own skill (a fake-use step before the loot func) need
                // exactly that skill; loot funcs are skill-less and use the generic one.
                let phase_skill = doodad_phase_skill(db, &doodad);
                last_had_skill = phase_skill.is_some();
                let skill = phase_skill.unwrap_or(generic_skill);
                game.use_doodad(skill, doodad.obj_id)
                    .await
                    .map_err(StepFailure::io)?;
                uses += 1;
                let until = (Instant::now() + USE_WAIT).min(deadline);
                let _ = wait_for_gather(game, quest_id, ids, want, objective, until)
                    .await
                    .map_err(StepFailure::io)?;
                let after = game.state.doodads.get(&doodad.obj_id);
                // A phase change is progress toward the loot func: keep driving the same doodad
                // (the fake-use phases advance to the loot phase). Mark it used only when it
                // despawned or three uses left the phase unchanged (a failed loot roll,
                // `funcs/items.rs::loot_item` does not advance the phase).
                let unchanged = after.is_some_and(|d| d.func_group_id == before_phase);
                let retries = retry_counts.entry(doodad.obj_id).or_insert(0);
                if unchanged {
                    *retries += 1;
                } else {
                    *retries = 0;
                }
                if after.is_none() || *retries >= 3 || uses >= 30 {
                    used.insert(doodad.obj_id);
                }
            }
            if gather_done(&game.state, ids, want, quest_id, objective) {
                return Ok(SourceOutcome::Progress(Some(format!(
                    "doodad {func} x{uses} (last template {last_template})"
                ))));
            }
            // A `DoodadFuncLootPack` whose pack contains the item at an effectively zero drop
            // rate is a 1.2 data gap, not a server misbehaviour (the pumpkin pack 6459 drops
            // item 8018 at 1/10,000,000).
            let pack_chance = if func == "DoodadFuncLootPack" {
                compact_db.and_then(|db| {
                    db.doodad_pack_drop_chance(ids[0], last_template)
                        .ok()
                        .flatten()
                })
            } else {
                None
            };
            if let Some(chance) = pack_chance.filter(|chance| *chance < 1000) {
                return Ok(SourceOutcome::Failed(SourceAttempt::new(
                    format!("doodad {func}"),
                    format!(
                        "doodad(s) {:?} used {uses} time(s) (last template {last_template}{}) \
                         gave none of item(s) {ids:?}; the loot pack drop rate is \
                         {chance}/10,000,000 (effectively zero)",
                        source.doodad_ids,
                        if spawned { ", GM-spawned" } else { "" }
                    ),
                    blocker_class::DATA_GAP,
                    "compact.sqlite3 loots.drop_rate (1.2 data)",
                )));
            }
            // When the data has no phase edge leading to the loot group, no interaction can
            // ever harvest it (doodad 4594's harvest phase sits behind a growth that 1.2 does
            // not link): a data gap with the phase ids.
            if let Some(db) = compact_db {
                let loot_phases = db
                    .doodad_loot_phases(last_template, ids[0])
                    .unwrap_or_default();
                if !loot_phases.is_empty() {
                    let (starts, edges) = db.doodad_phase_graph(last_template).unwrap_or_default();
                    let mut reachable: HashSet<u32> = starts.iter().copied().collect();
                    let mut queue: Vec<u32> = starts.clone();
                    while let Some(group) = queue.pop() {
                        for (from, to) in &edges {
                            if *from == group && reachable.insert(*to) {
                                queue.push(*to);
                            }
                        }
                    }
                    if !loot_phases.iter().any(|phase| reachable.contains(phase)) {
                        return Ok(SourceOutcome::Failed(SourceAttempt::new(
                            format!("doodad {func}"),
                            format!(
                                "doodad {last_template}: loot phase(s) {loot_phases:?} of item \
                                 {} are not reachable from its Start phase(s) {starts:?} (phase \
                                 edges {edges:?})",
                                ids[0]
                            ),
                            blocker_class::DATA_GAP,
                            "compact.sqlite3 doodad_funcs.next_phase (no growth link in 1.2)",
                        )));
                    }
                }
            }
            // A nearly full bag silently swallows the loot (`give_loot_pack` room check):
            // the runner's state, not a suspect.
            let capacity = bag_capacity(&game.state);
            let free = capacity.saturating_sub(game.state.bag.len() as u32);
            if free < 3 {
                return Ok(SourceOutcome::Failed(SourceAttempt::new(
                    format!("doodad {func}"),
                    format!(
                        "doodad(s) {:?} used {uses} time(s) (last template {last_template}{}) \
                         gave none of item(s) {ids:?} while the bag had only {free}/{capacity} \
                         free slot(s)",
                        source.doodad_ids,
                        if spawned { ", GM-spawned" } else { "" }
                    ),
                    blocker_class::RUNNER_GAP,
                    "aaemu-testclient/src/questrun.rs::bag_capacity (bag full)",
                )));
            }
            // A doodad whose last phase carried its own skill was stuck in fake-use / growth
            // phases the runner could not drive toward the loot func (the natural spawn may
            // outgrow them over time); that is the runner's limit, not a server answer.
            if last_had_skill {
                return Ok(SourceOutcome::Failed(SourceAttempt::new(
                    format!("doodad {func}"),
                    format!(
                        "doodad(s) {:?} used {uses} time(s) (last template {last_template}{}) \
                         never reached the loot phase: its last phase is a fake-use / growth step",
                        source.doodad_ids,
                        if spawned { ", GM-spawned" } else { "" }
                    ),
                    blocker_class::RUNNER_GAP,
                    "aaemu-testclient/src/questrun.rs::doodad_phase_skill (no growth wait)",
                )));
            }
            Ok(SourceOutcome::Failed(SourceAttempt::suspect(
                format!("doodad {func}"),
                format!(
                    "doodad(s) {:?} used {uses} time(s) (last template {last_template}{}) gave \
                     none of item(s) {ids:?}",
                    source.doodad_ids,
                    if spawned { ", GM-spawned" } else { "" }
                ),
                code::DOODAD_LOOT,
                format!(
                    "observed: {uses} use(s) of doodad(s) {:?} with their phase skill and no \
                     SCItemTaskSuccess for item(s) {ids:?} (bag had {free} free slots); \
                     expected: the doodad's loot func grants the item (compact \
                     doodad_func_loot_items.percent/count for item {}) — \
                     npcs/doodad/funcs/items.rs::loot_item",
                    source.doodad_ids, ids[0]
                ),
            )))
        }
        "store" => {
            if source.npc_ids.is_empty() {
                return Ok(SourceOutcome::Failed(SourceAttempt::new(
                    "store",
                    format!(
                        "item {ids:?} is in merchant packs {:?} but no merchant NPC sells it",
                        source.pack_ids
                    ),
                    blocker_class::DATA_GAP,
                    "compact.sqlite3 merchants / merchant_packs",
                )));
            }
            // `shop_info` needs `npcs.merchant`; the `merchants` table alone is not enough
            // (npc 1196 for quest 18 is listed but has merchant = false).
            let vendors: Vec<u32> = match compact_db {
                Some(db) => source
                    .npc_ids
                    .iter()
                    .copied()
                    .filter(|npc| {
                        db.npc_merchant(*npc)
                            .ok()
                            .flatten()
                            .is_some_and(|(merchant, _)| merchant)
                    })
                    .collect(),
                None => source.npc_ids.clone(),
            };
            if vendors.is_empty() {
                return Ok(SourceOutcome::Failed(SourceAttempt::new(
                    "store",
                    format!(
                        "the merchants table lists npc(s) {:?} for packs {:?} but their \
                         npcs.merchant flag is false (the server's shop_info rejects them)",
                        source.npc_ids, source.pack_ids
                    ),
                    blocker_class::DATA_GAP,
                    "compact.sqlite3 npcs.merchant / aaemu-game/src/systems/npcs/bridges.rs::shop_info",
                )));
            }
            if game.state.bag_count(ids) >= want {
                return Ok(SourceOutcome::Progress(Some("store (already held)".into())));
            }
            let positions = source_positions(source, step_world(step).as_deref());
            let mut npc = nearest_target(game, &vendors, &HashSet::new());
            if npc.is_none() {
                for pos in &positions {
                    game.teleport(near(*pos)).await.map_err(StepFailure::io)?;
                    settle(game, deadline).await;
                    npc = nearest_target(game, &vendors, &HashSet::new());
                    if npc.is_some() {
                        break;
                    }
                }
            }
            let mut spawned = false;
            let npc = match npc {
                Some(npc) => npc,
                None => {
                    let template = vendors[0];
                    spawn_npc(game, template).await?;
                    let appear = (Instant::now() + APPEAR_WAIT).min(deadline);
                    let Some(npc) =
                        wait_for_target(game, &vendors, &HashSet::new(), appear).await?
                    else {
                        return Ok(SourceOutcome::Failed(SourceAttempt::suspect(
                            "store",
                            format!("no merchant npc of template(s) {vendors:?} appeared"),
                            "aaemu-game/src/systems/npcs/spawn_manager.rs",
                            format!(
                                "observed: /spawn npc {template} answered and no unit of \
                                 template(s) {vendors:?} appeared within the wait; expected: the \
                                 GM spawn creates a live NPC (commands/scripts/spawn.rs -> \
                                 spawner.spawn_all)"
                            ),
                        )));
                    };
                    spawned = true;
                    journal.fallback(
                        "gm_spawn",
                        npc.id.to_string(),
                        format!(
                            "merchant npc {} was not naturally visible at its plan location",
                            npc.id
                        ),
                    );
                    npc
                }
            };
            approach(game, npc.pos, deadline).await;
            ensure_gm_gold(game).await?;
            let count = missing_item_count(&game.state, ids, want, quest_id, objective).max(1);
            game.buy_item(npc.obj_id, 0, ids[0], count)
                .await
                .map_err(StepFailure::io)?;
            let until = (Instant::now() + SOURCE_WAIT).min(deadline);
            let _ = wait_for_gather(game, quest_id, ids, want, objective, until)
                .await
                .map_err(StepFailure::io)?;
            if gather_done(&game.state, ids, want, quest_id, objective) {
                return Ok(SourceOutcome::Progress(Some(format!(
                    "store npc {} x{count}{}",
                    npc.id,
                    if spawned { ", gm_spawn" } else { "" }
                ))));
            }
            Ok(SourceOutcome::Failed(SourceAttempt::suspect(
                "store",
                format!(
                    "merchant npc {} (packs {:?}) sold none of item {} x{}",
                    npc.id, source.pack_ids, ids[0], count
                ),
                "aaemu-game/src/systems/economy/store.rs:242 (CSBuyItemsPacket)",
                format!(
                    "observed: CSBuyItemsPacket(npc {}, item {}, x{count}) and no \
                     SCItemTaskSuccess for the item; expected: merchant npc {} sells item {} \
                     from pack(s) {:?} at the template price (compact merchant_goods / \
                     npcs.merchant and store.rs::run)",
                    npc.id, ids[0], npc.id, ids[0], source.pack_ids
                ),
            )))
        }
        "item_open" => {
            let container = source.container_item_id;
            if container == 0 {
                return Ok(SourceOutcome::Failed(SourceAttempt::new(
                    "item_open",
                    "item_open without container_item_id".to_string(),
                    blocker_class::RUNNER_GAP,
                    "plan.py item_sources",
                )));
            }
            if depth >= ITEM_OPEN_DEPTH {
                return Ok(SourceOutcome::Failed(SourceAttempt::new(
                    format!("item_open {container}"),
                    "container nesting exceeds the depth limit".to_string(),
                    blocker_class::RUNNER_GAP,
                    "plan.py::ITEM_OPEN_DEPTH",
                )));
            }
            let empty: Vec<PlanItemSource> = Vec::new();
            let container_sources: Vec<&PlanItemSource> = source
                .container_sources
                .as_deref()
                .unwrap_or(&empty)
                .iter()
                .collect();
            let mut opened = 0usize;
            let mut last_note: Option<String> = None;
            let mut container_attempts: Vec<SourceAttempt> = Vec::new();
            loop {
                if gather_done(&game.state, ids, want, quest_id, objective)
                    || Instant::now() >= deadline
                {
                    break;
                }
                // The open consumes the bundle (`consume_source_item`), so top the bag up from
                // the container's own sources first; when the plan could not resolve them, the
                // GM item is the marked last resort.
                if game.state.bag_count(&[container]) == 0 {
                    if !container_sources.is_empty() {
                        last_note = gather_from_sources(
                            game,
                            quest_id,
                            &[container],
                            1,
                            None,
                            &container_sources,
                            step,
                            compact_db,
                            craft_db,
                            deadline,
                            depth + 1,
                            journal,
                            &mut container_attempts,
                            needed,
                        )
                        .await?
                        .or(last_note);
                    }
                    if game.state.bag_count(&[container]) == 0 {
                        let added =
                            gm_item_add(game, container, 1, deadline, needed, journal).await?;
                        journal.fallback(
                            "gm_item",
                            container.to_string(),
                            format!(
                                "container {container} for item(s) {ids:?} was not obtainable \
                                 ({}); /item add self",
                                last_note.as_deref().unwrap_or("no source")
                            ),
                        );
                        last_note = Some(format!("gm_item container {container} x{added}"));
                    }
                    if game.state.bag_count(&[container]) == 0 {
                        break;
                    }
                }
                let Some(item) = game
                    .state
                    .bag
                    .values()
                    .find(|i| i.template_id == container)
                    .copied()
                else {
                    break;
                };
                let Some(db) = compact_db else {
                    return Ok(SourceOutcome::Failed(SourceAttempt::new(
                        format!("item_open {container}"),
                        "needs --compact-db (the open skill)".to_string(),
                        blocker_class::RUNNER_GAP,
                        code::RUNNER,
                    )));
                };
                let (skill, target_type) = match db.item_use_skill(container) {
                    Ok(ItemUseSkill::Known {
                        skill_id,
                        target_type,
                    }) => (skill_id, target_type),
                    Ok(ItemUseSkill::MissingSkill { skill_id }) => {
                        return Ok(SourceOutcome::Failed(SourceAttempt::new(
                            format!("item_open {container}"),
                            format!(
                                "container {container} use_skill_id {skill_id} has no skills row \
                                 (the server drops the cast)"
                            ),
                            blocker_class::DATA_GAP,
                            code::SKILL_TEMPLATE,
                        )));
                    }
                    Ok(ItemUseSkill::None) => {
                        return Ok(SourceOutcome::Failed(SourceAttempt::new(
                            format!("item_open {container}"),
                            format!("container {container} has no use_skill_id"),
                            blocker_class::DATA_GAP,
                            "compact.sqlite3 items.use_skill_id",
                        )));
                    }
                    Err(e) => return Err(StepFailure::io(e)),
                };
                // The open skill can need every reagent of a list at once (the doll pouches:
                // skill 22045/22046 want all nine dolls); a missing reagent is the runner's
                // gap and is reported instead of blaming the open.
                if let Err(f) = ensure_use_reagents(
                    game,
                    quest_id,
                    skill,
                    compact_db,
                    craft_db,
                    Instant::now() + Duration::from_secs(30),
                    journal,
                    needed,
                    depth,
                )
                .await
                {
                    return Ok(SourceOutcome::Failed(SourceAttempt::new(
                        format!("item_open {container}"),
                        f.reason,
                        blocker_class::RUNNER_GAP,
                        "aaemu-testclient/src/questrun.rs::ensure_use_reagents / \
                         compact.sqlite3 skill_reagents",
                    )));
                }
                let target = use_item_target(game, target_type, step)?;
                let before = game.state.bag_count(ids);
                game.use_item(item.item_id, container, skill, target)
                    .await
                    .map_err(StepFailure::io)?;
                opened += 1;
                last_note = Some(format!("opened container {container} x{opened}"));
                // The open's result (grant / skill product) gets a fresh window: the step
                // deadline may already be spent on the reagent gathering.
                let until = Instant::now() + USE_WAIT;
                let _ = wait_for_gather(game, quest_id, ids, want, objective, until)
                    .await
                    .map_err(StepFailure::io)?;
                if game.state.bag_count(ids) <= before
                    && game.state.bag_count(&[container]) > 0
                    && opened.is_multiple_of(3)
                {
                    break;
                }
            }
            if gather_done(&game.state, ids, want, quest_id, objective) {
                return Ok(SourceOutcome::Progress(Some(format!(
                    "opened container {container} x{opened}"
                ))));
            }
            // A full (or nearly full) bag makes `give_loot_pack` / the skill product fail to
            // hand the item over; that is the runner's bag state, not a suspect.
            let capacity = bag_capacity(&game.state);
            let free = capacity.saturating_sub(game.state.bag.len() as u32);
            if free < 3 {
                return Ok(SourceOutcome::Failed(SourceAttempt::new(
                    format!("item_open {container}"),
                    format!(
                        "opened container {container} x{opened} and item(s) {ids:?} never \
                         arrived while the bag had only {free}/{capacity} free slot(s)"
                    ),
                    blocker_class::RUNNER_GAP,
                    "aaemu-testclient/src/questrun.rs::bag_capacity (bag full)",
                )));
            }
            Ok(SourceOutcome::Failed(SourceAttempt::suspect(
                format!("item_open {container}"),
                format!(
                    "opened container {container} x{opened} but item(s) {ids:?} never arrived ({})",
                    last_note.unwrap_or_else(|| "container not obtained".into())
                ),
                if source.grant.as_deref() == Some("skill_product") {
                    "aaemu-game/src/systems/skills/skill.rs (skill_products)"
                } else {
                    code::GAIN_ITEM
                },
                format!(
                    "observed: container {container} was used {opened} time(s) with its open \
                     skill and no SCItemTaskSuccess for item(s) {ids:?}; expected: the open \
                     skill's grant (grant {:?}, loot_pack {}) hands the item over (compact \
                     gain_loot_pack_item_effects / skill_products; \
                     economy/loot.rs::give_loot_pack)",
                    source.grant, source.loot_pack_id
                ),
            )))
        }
        "craft" => {
            let note = craft_source(
                game,
                quest_id,
                ids,
                want,
                objective,
                &source.craft_ids,
                craft_db,
                deadline,
            )
            .await?;
            if gather_done(&game.state, ids, want, quest_id, objective) {
                return Ok(SourceOutcome::Progress(note));
            }
            let reason = note.unwrap_or_else(|| "craft made no progress".into());
            if reason.contains("needs --compact-db") || reason.contains("trade-pack recipe") {
                return Ok(SourceOutcome::Failed(SourceAttempt::new(
                    format!("craft {:?}", source.craft_ids),
                    reason,
                    blocker_class::RUNNER_GAP,
                    code::RUNNER,
                )));
            }
            if reason.contains("data gap")
                || reason.contains("not in the game data")
                || reason.contains("not in the data")
            {
                return Ok(SourceOutcome::Failed(SourceAttempt::new(
                    format!("craft {:?}", source.craft_ids),
                    reason,
                    blocker_class::DATA_GAP,
                    "compact.sqlite3 crafts / craft_products",
                )));
            }
            Ok(SourceOutcome::Failed(SourceAttempt::suspect(
                format!("craft {:?}", source.craft_ids),
                reason,
                "aaemu-game/src/systems/economy/craft.rs (EndCraft)",
                format!(
                    "observed: the craft flow for recipe(s) {:?} ran and no product landed in \
                     the bag; expected: craft_products of the recipe creates item(s) {ids:?} \
                     (compact craft_products; craft.rs::end_craft -> EndCraft)",
                    source.craft_ids
                ),
            )))
        }
        other => Ok(SourceOutcome::Failed(SourceAttempt::new(
            format!("source {other}"),
            format!("source kind '{other}' is not driven"),
            blocker_class::RUNNER_GAP,
            code::RUNNER,
        ))),
    }
}

/// `/item add self <id> <count>` (the last resort); returns how many the bag gained.
async fn gm_item_add(
    game: &mut GameClient,
    item_id: u32,
    count: i32,
    deadline: Instant,
    needed: &BTreeSet<u32>,
    journal: &mut QuestJournal,
) -> Result<i32, StepFailure> {
    let mut added = 0;
    let mut freed_once = false;
    while added < count && Instant::now() < deadline {
        let chunk = (count - added).min(CRAFT_ITEM_ADD_CHUNK);
        match gm_item_add_once(game, item_id, chunk, deadline).await {
            Ok(n) => added += n,
            Err(f) if !freed_once && f.reason.contains("could not be created") => {
                // The bag is full (`items.rs` acquire_default_item); free room and retry once.
                freed_once = true;
                let used_gm_item = free_bag_space(game, needed, deadline, true).await?;
                if used_gm_item {
                    journal.fallback(
                        "gm_item",
                        BAG_EXPANSION_ITEM.to_string(),
                        "bag was full; a bag-expansion item was GM-added and used".to_string(),
                    );
                }
            }
            Err(f) => return Err(f),
        }
    }
    Ok(added)
}

/// One `/item add self <id> <count>` command plus its bag wait; no bag management (so
/// [`free_bag_space`] can add the expansion item). Fails with the command's chat text when the
/// server refuses (missing template, full bag).
async fn gm_item_add_once(
    game: &mut GameClient,
    item_id: u32,
    chunk: i32,
    deadline: Instant,
) -> Result<i32, StepFailure> {
    let before = game.state.bag_count(&[item_id]);
    let messages = game
        .gm(
            &format!("item add self {item_id} {chunk}"),
            Duration::from_millis(1200),
        )
        .await
        .map_err(StepFailure::io)?;
    let chat = messages.join(" | ");
    if messages
        .iter()
        .any(|m| m.contains("does not exist") || m.contains("could not be created"))
    {
        return Err(StepFailure::plan(
            QuestOutcome::RunnerUnsupported,
            StepOutcomeKind::Unsupported,
            if chat.contains("does not exist") {
                format!("GM /item add self {item_id} {chunk} failed (chat: {chat})")
            } else {
                format!(
                    "GM /item add self {item_id} {chunk} created nothing (bag full; chat: {chat})"
                )
            },
        ));
    }
    let until = (Instant::now() + CRAFT_GM_WAIT).min(deadline);
    loop {
        let after = game.state.bag_count(&[item_id]);
        if after > before {
            return Ok(after - before);
        }
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(StepFailure::plan(
                QuestOutcome::RunnerUnsupported,
                StepOutcomeKind::Unsupported,
                format!("GM /item add self {item_id} {chunk} created nothing (chat: {chat})"),
            ));
        }
        game.recv_within(left.min(PUMP))
            .await
            .map_err(StepFailure::io)?;
    }
}

/// The bag-expansion item of `bag_expands` (`inventory.rs::expand_slot` needs one per step).
const BAG_EXPANSION_ITEM: u32 = 8_000_025;

/// Destroys stacks no selected quest needs until the bag has room, then (when `allow_expansion`)
/// buys and consumes a bag-expansion item and asks the server to expand. Returns `true` when a
/// GM expansion item was used. The bag is the long run's scarce resource: loot, quest supplies
/// and `/item add` all fail on a full bag.
async fn free_bag_space(
    game: &mut GameClient,
    needed: &BTreeSet<u32>,
    deadline: Instant,
    allow_expansion: bool,
) -> Result<bool, StepFailure> {
    let capacity = bag_capacity(&game.state);
    // Destroy stacks the selected quests never need, newest slots first (the oldest slots tend
    // to hold quest items of the current chain).
    let mut destroyed = 0usize;
    while (game.state.bag.len() as u32) + 2 >= capacity && destroyed < 60 {
        let Some((slot, item)) = game
            .state
            .bag
            .iter()
            .rev()
            .find(|(_, item)| !needed.contains(&item.template_id))
            .map(|(slot, item)| (*slot, *item))
        else {
            break;
        };
        if Instant::now() >= deadline {
            break;
        }
        game.destroy_item(item.item_id, slot, item.count)
            .await
            .map_err(StepFailure::io)?;
        destroyed += 1;
        let until = (Instant::now() + Duration::from_secs(2)).min(deadline);
        while Instant::now() < until {
            if !game.state.bag.contains_key(&slot) {
                break;
            }
            game.recv_within(PUMP).await.map_err(StepFailure::io)?;
        }
    }
    if (game.state.bag.len() as u32) + 2 < capacity {
        return Ok(false);
    }
    if !allow_expansion || Instant::now() >= deadline {
        return Err(StepFailure::plan(
            QuestOutcome::RunnerUnsupported,
            StepOutcomeKind::Unsupported,
            format!(
                "bag full ({} / {} slot(s)) and every item is needed; destroyed {destroyed} \
                 stack(s)",
                game.state.bag.len(),
                capacity
            ),
        ));
    }

    // Expansion: the server consumes one `bag_expands` item per step (`inventory.rs
    // ::expand_slot`); add it when missing, then send `CSExpandSlotsPacket`.
    let mut used_gm_item = false;
    if game.state.bag_count(&[BAG_EXPANSION_ITEM]) == 0 {
        let _ = gm_item_add_once(game, BAG_EXPANSION_ITEM, 1, deadline).await?;
        used_gm_item = true;
    }
    let before = game.state.bag_slots;
    game.expand_slots(2, true).await.map_err(StepFailure::io)?;
    let until = (Instant::now() + Duration::from_secs(2)).min(deadline);
    while Instant::now() < until {
        if game.state.bag_slots > before {
            break;
        }
        game.recv_within(PUMP).await.map_err(StepFailure::io)?;
    }
    if game.state.bag_slots <= before {
        return Err(StepFailure::plan(
            QuestOutcome::RunnerUnsupported,
            StepOutcomeKind::Unsupported,
            format!(
                "bag full and CSExpandSlotsPacket expanded nothing (slots {}); bag_expands data \
                 or money missing",
                game.state.bag_slots
            ),
        ));
    }
    Ok(used_gm_item)
}

/// GM gold for merchant purchases (`/gold add self 10000 0 0` = 10,000 gold).
async fn ensure_gm_gold(game: &mut GameClient) -> Result<(), StepFailure> {
    game.gm("gold add self 10000 0 0", Duration::from_millis(500))
        .await
        .map_err(StepFailure::io)?;
    Ok(())
}

/// One purchase at a merchant (`CSBuyItemsPacket`): the server checks the merchant's pack, the
/// 3 m distance and the price (`systems/economy/store.rs`).
async fn buy_item_at(
    game: &mut GameClient,
    npc_obj: u32,
    doodad_obj: u32,
    item_id: u32,
    count: i32,
    deadline: Instant,
) -> Result<String, StepFailure> {
    ensure_gm_gold(game).await?;
    game.buy_item(npc_obj, doodad_obj, item_id, count)
        .await
        .map_err(StepFailure::io)?;
    let until = (Instant::now() + SOURCE_WAIT).min(deadline);
    let _ = wait_for_gather(game, 0, &[item_id], count, None, until)
        .await
        .map_err(StepFailure::io)?;
    if game.state.bag_count(&[item_id]) >= count {
        Ok(format!("buy item {item_id} x{count}"))
    } else {
        Err(StepFailure::timeout(format!(
            "merchant purchase of item {item_id} x{count} gave nothing"
        )))
    }
}

/// The nearest visible doodad of `ids` that has not been used yet.
fn find_unused_doodad(game: &GameClient, ids: &[u32], used: &HashSet<u32>) -> Option<DoodadInfo> {
    game.state
        .doodads
        .values()
        .filter(|d| ids.contains(&d.template_id) && !used.contains(&d.obj_id))
        .min_by(|a, b| {
            dist2(a.pos, game.state.self_pos).total_cmp(&dist2(b.pos, game.state.self_pos))
        })
        .cloned()
}

/// Waits for an unused doodad of the ids to become visible; `Ok(None)` on the deadline.
async fn wait_for_unused_doodad(
    game: &mut GameClient,
    ids: &[u32],
    used: &HashSet<u32>,
    deadline: Instant,
) -> Result<Option<DoodadInfo>, StepFailure> {
    loop {
        if let Some(found) = find_unused_doodad(game, ids, used) {
            return Ok(Some(found));
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

/// The first sign sphere of a step, or `runner_unsupported` with the stable "no sign sphere
/// for component N" wording when the plan has no geometry (a real `quest_sign_sphere.g` data
/// gap; `plan.py --spheres` fills it).
fn sphere_of(step: &PlanStep) -> Result<&PlanSphere, StepFailure> {
    let sphere = step
        .spheres
        .as_ref()
        .and_then(|spheres| spheres.first())
        .ok_or_else(|| {
            StepFailure::unsupported_reason(format!(
                "no sign sphere for component {}",
                step.component_id
            ))
        })?;
    // The run loop switched the bot into `step_world`; a sphere of another world cannot fire
    // there.
    let step_world = step_world(step);
    if !sphere
        .world
        .eq_ignore_ascii_case(step_world.as_deref().unwrap_or(MAIN_WORLD))
    {
        return Err(StepFailure::unsupported_reason(format!(
            "sphere component {} is in world '{}' but the step targets {}",
            sphere.component_id,
            sphere.world,
            step_world.as_deref().unwrap_or(MAIN_WORLD)
        )));
    }
    Ok(sphere)
}

/// Enters a sign sphere: teleport to the centre, then a small move that stays inside. The
/// server's 500 ms sphere tick (`quests/spheres.rs::tick`) compares the player's last check
/// position with the current one and dispatches the `EnterSphere` / starter event on the
/// outside -> inside transition, so the server-side transform must move.
async fn enter_sphere(
    game: &mut GameClient,
    sphere: &PlanSphere,
    deadline: Instant,
) -> Result<(), StepFailure> {
    game.teleport((sphere.x, sphere.y, sphere.z))
        .await
        .map_err(StepFailure::io)?;
    settle(game, deadline).await;
    let step = (sphere.radius * 0.25).clamp(0.5, 2.0);
    game.move_to((sphere.x + step, sphere.y, sphere.z), 1000)
        .await
        .map_err(StepFailure::io)?;
    settle(game, deadline).await;
    Ok(())
}

fn sphere_note(sphere: &PlanSphere) -> String {
    format!(
        "sphere component {} ({:.0},{:.0},{:.0} r{:.0})",
        sphere.component_id, sphere.x, sphere.y, sphere.z, sphere.radius
    )
}

/// `QuestActObjSphere` / `QuestActCheckSphere`: enter the sign sphere and wait for the
/// objective (`ObjSphere` sets counter 1 on `EnterSphere`); `CheckSphere` only sets the
/// server-side presence flag, so it is done once inside.
async fn step_sphere(
    game: &mut GameClient,
    quest_id: u32,
    step: &PlanStep,
    objective: Option<u8>,
    deadline: Instant,
) -> StepRun {
    let sphere = sphere_of(step)?.clone();
    enter_sphere(game, &sphere, deadline).await?;
    let note = sphere_note(&sphere);
    let want = step.target.count.max(1);
    if objective.is_none() {
        // `CheckSphere`: presence flag only (no counter and no update packet); a short wait
        // still catches a quest update driven by the entry.
        let until = (Instant::now() + USE_WAIT).min(deadline);
        let _ = wait_progress(game, quest_id, objective, want, until)
            .await
            .map_err(StepFailure::io)?;
        return Ok((
            StepOutcomeKind::Ok,
            format!("{note} (presence set; no counter)"),
        ));
    }
    let advanced = wait_progress(game, quest_id, objective, want, deadline)
        .await
        .map_err(StepFailure::io)?;
    if advanced || quest_completed(game, quest_id) {
        return Ok((StepOutcomeKind::Ok, note));
    }
    if quest_failed(game, quest_id) {
        return Err(StepFailure::quest_failed(quest_id));
    }
    Err(StepFailure::timeout(format!(
        "sphere objective of quest {quest_id} not reached"
    )))
}

/// `QuestActObjLevel`: the server evaluates `level >= act.level` on every update
/// (`quests/acts.rs` `ObjLevel`); the runner raises the character with `/level` (never lowers)
/// and waits for the objective / READY / completion.
async fn step_obj_level(
    game: &mut GameClient,
    quest_id: u32,
    step: &PlanStep,
    objective: Option<u8>,
    compact_db: Option<&CompactDb>,
    deadline: Instant,
) -> StepRun {
    let Some(db) = compact_db else {
        return Err(StepFailure::unsupported(
            "QuestActObjLevel needs --compact-db (game data)",
        ));
    };
    let Some(level) = db
        .obj_level_act_level(step.act_id)
        .map_err(StepFailure::io)?
    else {
        return Err(StepFailure::unsupported_reason(format!(
            "quest_act_obj_levels has no row for act {}",
            step.act_id
        )));
    };
    if level >= 2 {
        // `/level` adds the missing experience; those SCExpChanged packets are not quest
        // rewards, so the value-check window does not count them.
        let exp_mark = game.state.exp_changes.len();
        game.gm(&format!("level {level}"), Duration::from_secs(2))
            .await
            .ok();
        game.state.exp_changes.truncate(exp_mark);
    }
    let advanced = wait_progress(game, quest_id, objective, 1, deadline)
        .await
        .map_err(StepFailure::io)?;
    if advanced
        || quest_completed(game, quest_id)
        || quest_ready(game, quest_id)
        || objective.is_some_and(|index| objective_count(game, quest_id, index) >= 1)
    {
        return Ok((StepOutcomeKind::Ok, format!("level {level}")));
    }
    if quest_failed(game, quest_id) {
        return Err(StepFailure::quest_failed(quest_id));
    }
    Err(StepFailure::timeout(format!(
        "QuestActObjLevel {level} did not update quest {quest_id}"
    )))
}

/// `QuestActConAcceptSphere`: the server starts the quest when the player enters the holder
/// sphere itself (`spheres.rs::tick` -> `events::on_enter_quest_starter_sphere` ->
/// `add_quest(.., acceptor::SPHERE, sphere_id)`); no client packet is involved.
async fn step_accept_sphere(
    game: &mut GameClient,
    quest_id: u32,
    step: &PlanStep,
    deadline: Instant,
) -> StepRun {
    let sphere = sphere_of(step)?.clone();
    enter_sphere(game, &sphere, deadline).await?;
    let answer_deadline = Instant::now() + ACCEPT_WAIT;
    let found = wait_for_packet(game, answer_deadline, |p| {
        started_for(p, quest_id) || p.is_sc(sc::SCErrorMsgPacket)
    })
    .await
    .map_err(StepFailure::io)?;
    let note = sphere_note(&sphere);
    match found {
        Some(p) if started_for(&p, quest_id) => Ok((StepOutcomeKind::Ok, note)),
        Some(p) => {
            let code = p.stream().read_i16().unwrap_or(0);
            Err(rejected_accept(game, quest_id, "sphere accept", code))
        }
        None if game.state.quests.contains_key(&quest_id) => Ok((StepOutcomeKind::Ok, note)),
        None if quest_completed(game, quest_id) => Ok((StepOutcomeKind::Ok, note)),
        None => Err(StepFailure::plan(
            QuestOutcome::Rejected,
            StepOutcomeKind::Rejected,
            "no SCQuestContextStarted after entering the sphere (starter tick saw no entry)",
        )),
    }
}

/// The plan has no accept act at all: the runner must send the implicit empty accept before
/// the steps (`engine.rs::starter_accepts` `conditions.is_empty()`).
fn needs_implicit_accept(quest: &PlanQuest) -> bool {
    !quest
        .steps
        .iter()
        .any(|s| s.act_type.starts_with("QuestActConAccept"))
}

/// The failure point of a `stuck` result: the last non-passive step, or a generic reason when
/// every step was passive (never a reason-less stuck).
fn stuck_failed_step(quest: &PlanQuest) -> FailedStep {
    quest
        .steps
        .iter()
        .rev()
        .find(|s| !matches!(step_action(s), StepAction::Skip | StepAction::Passive))
        .map(|step| FailedStep {
            act_id: step.act_id,
            act_type: step.act_type.clone(),
            reason: "quest not completed after all steps".into(),
        })
        .unwrap_or_else(|| {
            gate_failed_step(
                "quest not completed after all steps (only passive steps; the server did not \
                 finish it)"
                    .into(),
            )
        })
}

/// Quest starts whose plan has no accept act: the Start component has no acts, so the server
/// accepts the quest from any source (`engine.rs::starter_accepts`: `conditions.is_empty()`).
/// The empty `CSStartQuestContextPacket` is sent; a refusal / silence is `no_starter`.
async fn implicit_accept(
    game: &mut GameClient,
    quest_id: u32,
    _deadline: Instant,
) -> Result<(), StepFailure> {
    game.start_quest(quest_id, 0)
        .await
        .map_err(StepFailure::io)?;
    let answer_deadline = Instant::now() + ACCEPT_WAIT;
    let found = wait_for_packet(game, answer_deadline, |p| {
        started_for(p, quest_id) || p.is_sc(sc::SCErrorMsgPacket)
    })
    .await
    .map_err(StepFailure::io)?;
    match found {
        Some(p) if started_for(&p, quest_id) => Ok(()),
        Some(p) => {
            let code = p.stream().read_i16().unwrap_or(0);
            Err(rejected_accept(game, quest_id, "no starter: accept", code))
        }
        None if game.state.quests.contains_key(&quest_id) => Ok(()),
        None => Err(StepFailure::plan(
            QuestOutcome::NoStarter,
            StepOutcomeKind::Rejected,
            "no starter: the plan has no accept act and the server ignored the empty accept \
             (see engine.rs::starter_accepts)",
        )),
    }
}

/// Accepts a quest at its NPC (`CSStartQuestContextPacket` with the NPC object) after finding or
/// spawning the NPC. Used for `QuestActObjAggro`, whose act only counts kills of the quest's NPC
/// acceptor (`quests/acts.rs` `ObjAggro`; the live game accepts those quests by engaging the
/// NPC, `npcs.engage_combat_give_quest_id`).
async fn accept_from_npc(
    game: &mut GameClient,
    quest_id: u32,
    npc_template: u32,
    deadline: Instant,
    journal: &mut QuestJournal,
) -> Result<(), StepFailure> {
    let npc = find_or_spawn_npc(game, npc_template, deadline, journal).await?;
    approach(game, npc.pos, deadline).await;
    game.start_quest(quest_id, npc.obj_id)
        .await
        .map_err(StepFailure::io)?;
    let answer_deadline = Instant::now() + ACCEPT_WAIT;
    let found = wait_for_packet(game, answer_deadline, |p| {
        started_for(p, quest_id) || p.is_sc(sc::SCErrorMsgPacket)
    })
    .await
    .map_err(StepFailure::io)?;
    match found {
        Some(p) if started_for(&p, quest_id) => Ok(()),
        Some(p) => {
            let code = p.stream().read_i16().unwrap_or(0);
            Err(rejected_accept(game, quest_id, "accept at NPC", code))
        }
        None if game.state.quests.contains_key(&quest_id) => Ok(()),
        None => Err(StepFailure::plan(
            QuestOutcome::Rejected,
            StepOutcomeKind::Rejected,
            format!(
                "accept at NPC {npc_template} silently ignored (engine.rs::add_quest; \
                 start-component unit_reqs or acceptor mismatch)"
            ),
        )),
    }
}

/// Fetches the items `OwnItem` start requirements need before the accept: the same real-source
/// ladder as an item step, GM item as the marked last resort (`unit_reqs.rs` `OWN_ITEM` checks
/// `player.owns_item`; a missing item makes `add_quest` refuse silently).
async fn ensure_requirement_items(
    game: &mut GameClient,
    quest: &PlanQuest,
    compact_db: Option<&CompactDb>,
    craft_db: Option<&CraftDb>,
    deadline: Instant,
    journal: &mut QuestJournal,
    needed: &BTreeSet<u32>,
) -> Result<(), StepFailure> {
    for req in &quest.requires {
        let key = kind_key(&req.kind);
        if req.kind_id != REQ_OWN_ITEM && key != "ownitem" {
            continue;
        }
        let Ok(item_id) = u32::try_from(req.value1) else {
            continue;
        };
        if item_id == 0 || game.state.bag_count(&[item_id]) > 0 {
            continue;
        }
        // Prefer the quest step that already names the item (it carries the plan's sources);
        // otherwise use a synthetic item step and the compact reverse lookups.
        let owned_step;
        let step = if let Some(step) = quest.steps.iter().find(|step| {
            step.target.ids.contains(&item_id)
                || step
                    .item_sources
                    .as_ref()
                    .is_some_and(|entries| entries.iter().any(|entry| entry.item_id == item_id))
        }) {
            step
        } else {
            owned_step = synthetic_item_step(item_id, 1);
            &owned_step
        };
        let (_kind, note) = step_gather(
            game,
            quest.quest_id,
            step,
            None,
            compact_db,
            craft_db,
            deadline,
            journal,
            needed,
        )
        .await?;
        if game.state.bag_count(&[item_id]) == 0 {
            return Err(StepFailure::plan(
                QuestOutcome::RequirementUnmet,
                StepOutcomeKind::Timeout,
                format!("own_item {item_id} could not be obtained before the accept ({note})"),
            ));
        }
    }
    Ok(())
}

/// A synthetic item step for an item outside the quest's own steps (reagents, OwnItem
/// requirements): no locations, sources resolved from `compact.sqlite3` by [`step_gather`].
fn synthetic_item_step(item_id: u32, count: i32) -> PlanStep {
    PlanStep {
        component_id: 0,
        component_kind: "Start".into(),
        act_id: 0,
        act_type: "QuestActObjItemGather".into(),
        target: PlanTarget {
            kind: "item".into(),
            ids: vec![item_id],
            count: count.max(1),
        },
        locations: Vec::new(),
        status: "ok".into(),
        sources: None,
        item_sources: None,
        spheres: None,
    }
}

/// Ensures every reagent a use skill needs is in the bag before the cast. `first_reagent_only`
/// skills need one of the listed reagents, others need **all** of them (quest 5307/5313: skill
/// 22045/22046 wants the nine dolls at once, otherwise the server answers NEED_REAGENT / skill
/// result 36). Missing reagents go through the item ladder (open more packs, then a marked GM
/// item); a reagent the runner cannot obtain is a `runner_gap`, never a suspect.
#[allow(clippy::too_many_arguments)]
async fn ensure_use_reagents(
    game: &mut GameClient,
    quest_id: u32,
    skill_id: u32,
    compact_db: Option<&CompactDb>,
    craft_db: Option<&CraftDb>,
    deadline: Instant,
    journal: &mut QuestJournal,
    needed: &BTreeSet<u32>,
    depth: u32,
) -> Result<(), StepFailure> {
    let Some(db) = compact_db else {
        return Ok(());
    };
    let reagents = db.skill_reagents(skill_id).unwrap_or_default();
    if reagents.is_empty() {
        return Ok(());
    }
    if depth > ITEM_OPEN_DEPTH {
        return Err(StepFailure::plan(
            QuestOutcome::RunnerUnsupported,
            StepOutcomeKind::Unsupported,
            format!(
                "use skill {skill_id} needs reagents and the reagent sources nest too deep \
                 (item_open depth {depth})"
            ),
        ));
    }
    let first_only = db.skill_first_reagent_only(skill_id).unwrap_or(false);
    let mut missing = Vec::new();
    for (item, amount) in reagents {
        if amount <= 0 {
            continue;
        }
        if game.state.bag_count(&[item]) >= amount {
            if first_only {
                return Ok(());
            }
            continue;
        }
        // Each reagent gets a fresh window: the use is the terminal action of the step and the
        // step deadline is usually spent on the source attempts.
        let window = (Instant::now() + Duration::from_secs(30))
            .min(deadline.max(Instant::now() + Duration::from_secs(5)));
        let step = synthetic_item_step(item, amount);
        let mut reagent_journal = QuestJournal::default();
        let note = step_gather(
            game,
            quest_id,
            &step,
            None,
            compact_db,
            craft_db,
            window,
            &mut reagent_journal,
            needed,
        )
        .await
        .map(|(_, note)| note)
        .unwrap_or_else(|f| f.reason);
        journal.fallbacks.extend(reagent_journal.fallbacks);
        if game.state.bag_count(&[item]) < amount {
            missing.push(format!("reagent {item} x{amount} ({note})"));
            if first_only {
                break;
            }
        } else if first_only {
            return Ok(());
        }
    }
    if !missing.is_empty() {
        let reason = format!(
            "NEED_REAGENT (skill result 36): use skill {skill_id} requires all of {} and the \
             runner could not obtain {}",
            db.skill_reagents(skill_id)
                .unwrap_or_default()
                .iter()
                .map(|(item, amount)| format!("{item} x{amount}"))
                .collect::<Vec<_>>()
                .join(", "),
            missing.join("; ")
        );
        journal.note_blocker(
            blocker_class::RUNNER_GAP,
            reason.clone(),
            "compact.sqlite3 skill_reagents / \
             aaemu-game/src/systems/skills/validation.rs::check_resources (NEED_REAGENT)",
        );
        return Err(StepFailure::plan(
            QuestOutcome::RunnerUnsupported,
            StepOutcomeKind::Unsupported,
            reason,
        ));
    }
    Ok(())
}

/// Last resort (`--force-blocked`): `/quest add <id>` (GM forcibly) then `/quest step <id>
/// Reward` (`commands/scripts/quest_cmd.rs`). Returns `Ok` only when the server completed the
/// quest (a live `SCQuestContextCompletedPacket` / completed set), so a forced quest is never
/// silently marked.
async fn force_quest_complete(
    game: &mut GameClient,
    quest_id: u32,
    deadline: Instant,
) -> Result<(), String> {
    if quest_completed(game, quest_id) {
        return Ok(());
    }
    let add = game
        .gm(&format!("quest add {quest_id}"), Duration::from_millis(800))
        .await
        .map_err(|e| format!("{e:#}"))?;
    if add.iter().any(|m| {
        m.contains("Proper usage")
            || m.contains("not found")
            || m.contains("Failed to start")
            || m.contains("does not exist")
    }) {
        return Err(format!("quest add {quest_id} refused: {}", add.join(" | ")));
    }
    let until = (Instant::now() + Duration::from_secs(3)).min(deadline);
    while !game.state.quests.contains_key(&quest_id) && Instant::now() < until {
        if quest_completed(game, quest_id) {
            return Ok(());
        }
        let _ = game.recv_within(PUMP).await;
    }
    if !game.state.quests.contains_key(&quest_id) && !quest_completed(game, quest_id) {
        return Err(format!(
            "quest {quest_id} is not active after /quest add (chat: {})",
            add.join(" | ")
        ));
    }
    let step = game
        .gm(
            &format!("quest step {quest_id} Reward"),
            Duration::from_millis(800),
        )
        .await
        .map_err(|e| format!("{e:#}"))?;
    if step
        .iter()
        .any(|m| m.contains("You do not have the quest") || m.contains("Proper usage"))
    {
        return Err(format!(
            "quest step {quest_id} Reward refused: {}",
            step.join(" | ")
        ));
    }
    while Instant::now() < deadline {
        if quest_completed(game, quest_id) || game.state.completed_quests.contains(&quest_id) {
            return Ok(());
        }
        let _ = game.recv_within(PUMP).await;
    }
    Err(format!(
        "quest {quest_id} did not complete after /quest step Reward (chat: {})",
        step.join(" | ")
    ))
}

/// `QuestActConAcceptComponent`: the act is always satisfied when the Start component runs
/// (`engine.rs::starter_accepts`: `ConAutoComplete | ConAcceptComponent => true`), so a plain
/// `CSStartQuestContextPacket` without an NPC / doodad / sphere is accepted.
async fn step_accept_component(
    game: &mut GameClient,
    quest_id: u32,
    _deadline: Instant,
) -> StepRun {
    game.start_quest(quest_id, 0)
        .await
        .map_err(StepFailure::io)?;
    let answer_deadline = Instant::now() + ACCEPT_WAIT;
    let found = wait_for_packet(game, answer_deadline, |p| {
        started_for(p, quest_id) || p.is_sc(sc::SCErrorMsgPacket)
    })
    .await
    .map_err(StepFailure::io)?;
    match found {
        Some(p) if started_for(&p, quest_id) => {
            Ok((StepOutcomeKind::Ok, "component accept".into()))
        }
        Some(p) => {
            let code = p.stream().read_i16().unwrap_or(0);
            Err(rejected_accept(game, quest_id, "accept", code))
        }
        None if game.state.quests.contains_key(&quest_id) => {
            Ok((StepOutcomeKind::Ok, "component accept (state)".into()))
        }
        None => Err(StepFailure::plan(
            QuestOutcome::Rejected,
            StepOutcomeKind::Rejected,
            "no answer from server (accept silently ignored; see engine.rs::add_quest)",
        )),
    }
}

/// `QuestActConAcceptItem`: start the quest by using the starter item. The item's use skill
/// carries the `AcceptQuestEffect` (`skills` -> `quests::bridges` -> `events::
/// accept_quest_effect` -> `quest_id_from_starter_item`); the item is gathered through the
/// step's `item_sources` when it is not in the bag.
#[allow(clippy::too_many_arguments)]
async fn step_accept_item(
    game: &mut GameClient,
    quest_id: u32,
    step: &PlanStep,
    compact_db: Option<&CompactDb>,
    craft_db: Option<&CraftDb>,
    deadline: Instant,
    journal: &mut QuestJournal,
    needed: &BTreeSet<u32>,
) -> StepRun {
    let ids = &step.target.ids;
    let want = step.target.count.max(1);
    if ids.is_empty() {
        return Err(StepFailure::unsupported(
            "QuestActConAcceptItem without item ids",
        ));
    }
    let mut gathered = None;
    if game.state.bag_count(ids) < want {
        let (kind, note) = step_gather(
            game, quest_id, step, None, compact_db, craft_db, deadline, journal, needed,
        )
        .await?;
        gathered = Some(kind);
        if game.state.bag_count(ids) < want {
            return Err(StepFailure::timeout(format!(
                "starter item not in the bag after gathering ({note})"
            )));
        }
    }
    let Some(db) = compact_db else {
        return Err(StepFailure::unsupported(
            "starter item use skill lookup needs --compact-db (game data)",
        ));
    };
    let Some(item) = game
        .state
        .bag
        .values()
        .find(|i| ids.contains(&i.template_id))
        .copied()
    else {
        return Err(StepFailure::timeout(format!(
            "starter item {ids:?} is not in the bag"
        )));
    };
    let (skill, target_type) = match db.item_use_skill(item.template_id) {
        Ok(ItemUseSkill::Known {
            skill_id,
            target_type,
        }) => (skill_id, target_type),
        Ok(ItemUseSkill::MissingSkill { skill_id }) => {
            return Err(StepFailure::unsupported_reason(format!(
                "starter item {} use_skill_id {skill_id} has no skills row in the game data",
                item.template_id
            )));
        }
        Ok(ItemUseSkill::None) => {
            return Err(StepFailure::unsupported_reason(format!(
                "starter item {} has no use_skill_id",
                item.template_id
            )));
        }
        Err(e) => return Err(StepFailure::io(e)),
    };
    // The starter item's use skill may itself need reagents (all of them when
    // `first_reagent_only` is false); fetch them before the cast (NEED_REAGENT is runner_gap).
    ensure_use_reagents(
        game,
        quest_id,
        skill,
        compact_db,
        craft_db,
        Instant::now() + Duration::from_secs(30),
        journal,
        needed,
        0,
    )
    .await?;
    let target = use_item_target(game, target_type, step)?;
    game.use_item(item.item_id, item.template_id, skill, target)
        .await
        .map_err(StepFailure::io)?;
    let answer_deadline = Instant::now() + ACCEPT_WAIT;
    let found = wait_for_packet(game, answer_deadline, |p| {
        started_for(p, quest_id) || p.is_sc(sc::SCErrorMsgPacket)
    })
    .await
    .map_err(StepFailure::io)?;
    let note = format!("item {} skill {skill}", item.template_id);
    match found {
        Some(p) if started_for(&p, quest_id) => Ok((gathered.unwrap_or(StepOutcomeKind::Ok), note)),
        Some(p) => {
            let code = p.stream().read_i16().unwrap_or(0);
            Err(rejected_accept(game, quest_id, "item accept", code))
        }
        // The accept item's `AcceptQuestEffect` starts the quest even when the start packet is
        // lost (a long gather may have consumed the answer window): the state row is the proof.
        None if game.state.quests.contains_key(&quest_id) => Ok((
            gathered.unwrap_or(StepOutcomeKind::Ok),
            format!("{note} (state)"),
        )),
        None => Err(StepFailure::plan(
            QuestOutcome::Rejected,
            StepOutcomeKind::Rejected,
            "no answer from server (using the starter item did not start the quest)",
        )),
    }
}

/// `QuestActObjItemUse`: use the item with its `use_skill_id` (`CSStartSkillPacket` with a
/// `SkillItem` caster); the target follows the skill's `target_type_id`. Missing items are
/// gathered first from the step's item sources.
#[allow(clippy::too_many_arguments)]
async fn step_use_item(
    game: &mut GameClient,
    quest_id: u32,
    step: &PlanStep,
    objective: Option<u8>,
    compact_db: Option<&CompactDb>,
    craft_db: Option<&CraftDb>,
    deadline: Instant,
    journal: &mut QuestJournal,
    needed: &BTreeSet<u32>,
) -> StepRun {
    let ids = &step.target.ids;
    let want = step.target.count.max(1);
    if ids.is_empty() {
        return Err(StepFailure::unsupported(
            "QuestActObjItemUse without item ids",
        ));
    }
    let mut gathered: Option<StepOutcomeKind> = None;
    let mut gathered_note: Option<String> = None;
    if game.state.bag_count(ids) < want {
        let (kind, note) = step_gather(
            game, quest_id, step, objective, compact_db, craft_db, deadline, journal, needed,
        )
        .await?;
        gathered = Some(kind);
        if game.state.bag_count(ids) < want {
            return Err(StepFailure::timeout(format!(
                "item not in the bag after gathering ({note})"
            )));
        }
        gathered_note = Some(note);
    }
    let Some(db) = compact_db else {
        return Err(StepFailure::unsupported(
            "item use skill lookup needs --compact-db (game data)",
        ));
    };
    let mut used_note = String::new();
    loop {
        if quest_completed(game, quest_id) || quest_ready(game, quest_id) {
            break;
        }
        if let Some(index) = objective {
            if objective_count(game, quest_id, index) >= want {
                break;
            }
        }
        if quest_failed(game, quest_id) {
            return Err(StepFailure::quest_failed(quest_id));
        }
        if Instant::now() >= deadline {
            return Err(StepFailure::timeout(format!(
                "using the item did not update quest {quest_id}"
            )));
        }
        let Some(item) = game
            .state
            .bag
            .values()
            .find(|i| ids.contains(&i.template_id))
            .copied()
        else {
            return Err(StepFailure::timeout(format!(
                "item {ids:?} left the bag before the objective was reached"
            )));
        };
        let (skill, target_type) = match db.item_use_skill(item.template_id) {
            Ok(ItemUseSkill::Known {
                skill_id,
                target_type,
            }) => (skill_id, target_type),
            Ok(ItemUseSkill::MissingSkill { skill_id }) => {
                // The client would cast this id; the server's `template_skill` lookup returns
                // `None` and drops the use (`skills/handlers.rs`), so the objective cannot
                // advance. Quest 1394 / item 14177 (use_skill_id 12043) is this data gap.
                return Err(StepFailure::unsupported_reason(format!(
                    "item {} use_skill_id {skill_id} has no skills row in the game data \
                     (the server drops the use cast: systems/skills/handlers.rs::template_skill)",
                    item.template_id
                )));
            }
            Ok(ItemUseSkill::None) => {
                return Err(StepFailure::unsupported_reason(format!(
                    "item {} has no use_skill_id",
                    item.template_id
                )));
            }
            Err(e) => return Err(StepFailure::io(e)),
        };
        // A use skill can require several reagents at once (first_reagent_only = false): fetch
        // every one before the cast; NEED_REAGENT is the runner's gap, never a suspect.
        ensure_use_reagents(
            game,
            quest_id,
            skill,
            compact_db,
            craft_db,
            Instant::now() + Duration::from_secs(30),
            journal,
            needed,
            0,
        )
        .await?;
        let target = use_item_target(game, target_type, step)?;
        game.use_item(item.item_id, item.template_id, skill, target)
            .await
            .map_err(StepFailure::io)?;
        used_note = format!("item {} skill {skill}", item.template_id);
        // The cast's result (skill product / item task) gets a fresh window: the step deadline
        // may already be spent on the item gathering.
        let until = Instant::now() + USE_WAIT;
        if wait_progress(game, quest_id, objective, want, until)
            .await
            .map_err(StepFailure::io)?
        {
            break;
        }
    }
    let source_note = gathered_note.unwrap_or_default();
    let note = match gathered {
        Some(StepOutcomeKind::OkGmItem) => {
            format!("gather {source_note}; {used_note}")
        }
        Some(StepOutcomeKind::OkGmSpawn) => {
            format!("gather gm_spawn; {source_note}; {used_note}")
        }
        Some(_) => format!("gather via {source_note}; {used_note}"),
        None => used_note,
    };
    Ok((gathered.unwrap_or(StepOutcomeKind::Ok), note))
}

/// The wire target strategy of a `SkillTargetType` for item use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UseTargetKind {
    /// Self / friendly / party / raid / any-unit / pet: our own unit.
    SelfUnit,
    /// A hostile target: the nearest visible NPC.
    NearestNpc,
    /// A position skill: the step location or our current position.
    Position,
    /// A doodad skill: the nearest visible doodad.
    Doodad,
    /// Anything else (item targets, lines, ...): not driven.
    Unsupported,
}

/// `SkillTargetType` (`systems/skills/types.rs`) -> the wire target the runner sends.
pub fn use_target_kind(target_type: u8) -> UseTargetKind {
    match target_type {
        0 | 1 | 2 | 3 | 5 | 10 | 16 | 17 => UseTargetKind::SelfUnit,
        4 => UseTargetKind::NearestNpc,
        6 | 11 | 12 | 13 | 14 | 15 | 18 => UseTargetKind::Position,
        8 => UseTargetKind::Doodad,
        _ => UseTargetKind::Unsupported,
    }
}

fn use_item_target(
    game: &GameClient,
    target_type: u8,
    step: &PlanStep,
) -> Result<SkillCastTarget, StepFailure> {
    match use_target_kind(target_type) {
        UseTargetKind::SelfUnit => Ok(SkillCastTarget::Unit(game.state.self_obj)),
        UseTargetKind::NearestNpc => game
            .nearest_unit(1)
            .map(|npc| SkillCastTarget::Unit(npc.obj_id))
            .ok_or_else(|| StepFailure::timeout("no npc in sight for the item's skill")),
        UseTargetKind::Position => {
            let pos = step
                .locations
                .iter()
                .find(|l| l.world.eq_ignore_ascii_case(MAIN_WORLD))
                .map_or(game.state.self_pos, |l| (l.x, l.y, l.z));
            Ok(SkillCastTarget::Position {
                obj_id: 0,
                x: pos.0,
                y: pos.1,
                z: pos.2,
                rot: 0.0,
                obj_id1: 0,
                obj_id2: 0,
            })
        }
        UseTargetKind::Doodad => game
            .state
            .doodads
            .values()
            .min_by(|a, b| {
                dist2(a.pos, game.state.self_pos).total_cmp(&dist2(b.pos, game.state.self_pos))
            })
            .map(|d| SkillCastTarget::Doodad(d.obj_id))
            .ok_or_else(|| StepFailure::timeout("no doodad in sight for the item's skill")),
        UseTargetKind::Unsupported => Err(StepFailure::unsupported_reason(format!(
            "item skill target type {target_type} is not driven"
        ))),
    }
}

// ----- kills -----

/// What a kill run works towards: an objective counter (or "the kill advances the quest" for
/// `ConAcceptNpcKill`, which has no counter) or a fixed number of kills (item loot).
#[derive(Debug, Clone, Copy)]
enum KillGoal {
    Objective {
        quest_id: u32,
        index: Option<u8>,
        want: i32,
    },
    Kills {
        want: i32,
    },
}

fn goal_reached(game: &GameClient, goal: KillGoal, kills: i32) -> bool {
    match goal {
        KillGoal::Objective {
            quest_id,
            index,
            want,
        } => {
            quest_completed(game, quest_id)
                // Let-it-done / score quests reach READY before every counter is full; the
                // turn-in then finishes the quest (`events.rs::on_report`).
                || quest_ready(game, quest_id)
                || index.is_some_and(|i| objective_count(game, quest_id, i) >= want)
        }
        KillGoal::Kills { want } => kills >= want,
    }
}

/// After a kill, wait up to `until` for the goal (`index: None` also accepts a quest update).
async fn wait_after_kill(
    game: &mut GameClient,
    goal: KillGoal,
    kills: i32,
    until: Instant,
) -> anyhow::Result<bool> {
    while Instant::now() < until {
        if goal_reached(game, goal, kills) {
            return Ok(true);
        }
        if let KillGoal::Objective { quest_id, .. } = goal {
            if quest_failed(game, quest_id) {
                return Ok(false);
            }
        }
        let left = until.saturating_duration_since(Instant::now());
        if let Some(p) = game.recv_within(left.min(PUMP)).await? {
            if let KillGoal::Objective {
                quest_id,
                index: None,
                ..
            } = goal
            {
                if updated_for(&p, quest_id) {
                    return Ok(true);
                }
            }
        }
    }
    Ok(goal_reached(game, goal, kills))
}

/// `/kill` one visible target of `ids` (credits the bot; `kill.rs` ->
/// `combat::reduce_current_hp(.., Some(self), Gm)` -> the quest `on_kill`). Returns the killed
/// object id; `Ok(None)` on a rejected kill.
async fn kill_visible(
    game: &mut GameClient,
    ids: &[u32],
    avoid: &mut HashSet<u32>,
) -> Result<Option<u32>, StepFailure> {
    let Some(npc) = nearest_target(game, ids, avoid) else {
        return Ok(None);
    };
    game.change_target(npc.obj_id)
        .await
        .map_err(StepFailure::io)?;
    let messages = game.gm("kill", KILL_WAIT).await.map_err(StepFailure::io)?;
    let rejected = messages
        .iter()
        .any(|m| m.contains("Cannot kill") || m.contains("already dead"));
    avoid.insert(npc.obj_id);
    Ok((!rejected).then_some(npc.obj_id))
}

/// Hunts `ids` until `goal` is reached or `deadline`; teleports through `positions` (may be
/// empty for dynamic spawns) when no target is visible. `Ok(None)` when the deadline hit
/// first; the note says `kill`, the kills are the corpse object ids.
async fn hunt_until(
    game: &mut GameClient,
    ids: &[u32],
    positions: &[(f32, f32, f32)],
    goal: KillGoal,
    deadline: Instant,
) -> Result<Option<(String, Vec<u32>)>, StepFailure> {
    let mut avoid: HashSet<u32> = HashSet::new();
    let mut killed = Vec::new();
    let mut next_location = 0usize;
    loop {
        if goal_reached(game, goal, killed.len() as i32) {
            return Ok(Some(("kill".into(), killed)));
        }
        if let KillGoal::Objective { quest_id, .. } = goal {
            if quest_failed(game, quest_id) {
                return Err(StepFailure::quest_failed(quest_id));
            }
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        if nearest_target(game, ids, &avoid).is_none() {
            if !positions.is_empty() {
                let pos = positions[next_location % positions.len()];
                next_location += 1;
                game.teleport(near(pos)).await.map_err(StepFailure::io)?;
                settle(game, deadline).await;
            }
            let appear = (Instant::now() + APPEAR_WAIT).min(deadline);
            let _ = wait_for_target(game, ids, &avoid, appear).await?;
            continue;
        }
        match kill_visible(game, ids, &mut avoid).await? {
            Some(obj_id) => {
                killed.push(obj_id);
                let until = (Instant::now() + Duration::from_secs(8)).min(deadline);
                let _ = wait_after_kill(game, goal, killed.len() as i32, until)
                    .await
                    .map_err(StepFailure::io)?;
            }
            None => continue,
        }
    }
}

/// `/spawn npc <template>` in front of the bot (`commands/scripts/spawn.rs`); returns the note.
async fn spawn_npc(game: &mut GameClient, template_id: u32) -> Result<String, StepFailure> {
    let messages = game
        .gm(
            &format!("spawn npc {template_id}"),
            Duration::from_millis(1500),
        )
        .await
        .map_err(StepFailure::io)?;
    if messages.iter().any(|m| {
        m.contains("don't exist") || m.contains("parse error") || m.contains("can not be 0")
    }) {
        return Err(StepFailure::unsupported_reason(format!(
            "cannot spawn npc {template_id}: {}",
            messages.join("; ")
        )));
    }
    Ok(format!("gm_spawn {template_id}"))
}

/// The GM fallback of a hunt: spawn the template next to the bot, kill (and optionally loot)
/// until the goal is reached or the window closes. The fallback is recorded (never silent); a
/// failure names the kills and the objective state.
async fn spawn_and_kill(
    game: &mut GameClient,
    template_id: u32,
    goal: KillGoal,
    window: Duration,
    loot: bool,
    journal: &mut QuestJournal,
) -> Result<(String, Vec<u32>, bool), StepFailure> {
    let deadline = Instant::now() + window;
    let mut killed = Vec::new();
    let mut note = String::new();
    let mut avoid: HashSet<u32> = HashSet::new();
    let mut spawned = false;
    loop {
        if goal_reached(game, goal, killed.len() as i32) {
            return Ok((note, killed, spawned));
        }
        if let KillGoal::Objective { quest_id, .. } = goal {
            if quest_failed(game, quest_id) {
                return Err(StepFailure::quest_failed(quest_id));
            }
        }
        if Instant::now() >= deadline {
            let state = match goal {
                KillGoal::Objective {
                    quest_id,
                    index: Some(index),
                    want,
                } => format!(
                    "objective {} of quest {quest_id} at {}/{}",
                    index,
                    objective_count(game, quest_id, index),
                    want
                ),
                _ => format!("{} kill(s)", killed.len()),
            };
            return Err(StepFailure::plan(
                QuestOutcome::Stuck,
                StepOutcomeKind::Timeout,
                format!(
                    "gm spawn fallback did not reach the goal for npc {template_id} \
                     ({state}{})",
                    if spawned { ", GM-spawned" } else { "" }
                ),
            ));
        }
        match nearest_target(game, &[template_id], &avoid) {
            Some(npc) => {
                game.change_target(npc.obj_id)
                    .await
                    .map_err(StepFailure::io)?;
                let messages = game.gm("kill", KILL_WAIT).await.map_err(StepFailure::io)?;
                avoid.insert(npc.obj_id);
                // A corpse still in the unit list answers "already dead"; it is not a kill and
                // must not advance the loop (the v8 fallback counted them and stalled).
                if messages
                    .iter()
                    .any(|m| m.contains("already dead") || m.contains("Cannot kill"))
                {
                    continue;
                }
                killed.push(npc.obj_id);
                if loot {
                    game.loot_all(npc.obj_id).await.map_err(StepFailure::io)?;
                    game.close_loot_bag(npc.obj_id)
                        .await
                        .map_err(StepFailure::io)?;
                }
                let until = (Instant::now() + Duration::from_secs(3)).min(deadline);
                let _ = wait_after_kill(game, goal, killed.len() as i32, until)
                    .await
                    .map_err(StepFailure::io)?;
            }
            None => {
                note = spawn_npc(game, template_id).await?;
                if !spawned {
                    spawned = true;
                    journal.fallback(
                        "gm_spawn",
                        template_id.to_string(),
                        format!(
                            "no natural npc {template_id} at the plan locations within {}s; \
                             /spawn npc used",
                            GM_SPAWN_WINDOW.as_secs()
                        ),
                    );
                }
                let appear = (Instant::now() + APPEAR_WAIT).min(deadline);
                if wait_for_target(game, &[template_id], &avoid, appear)
                    .await?
                    .is_none()
                {
                    return Err(StepFailure::plan(
                        QuestOutcome::Stuck,
                        StepOutcomeKind::Timeout,
                        format!("{note} but the spawned npc did not appear"),
                    ));
                }
            }
        }
    }
}

/// `QuestActObjMonsterHunt` / `QuestActObjMonsterGroupHunt`: hunt normally, wait out respawns
/// until the step deadline, then the `/spawn` fallback (`ok_gm_spawn`, marked). Every target id
/// of a group gets a fallback attempt (the first spawneable one wins).
async fn step_kill(
    game: &mut GameClient,
    quest_id: u32,
    step: &PlanStep,
    objective: Option<u8>,
    deadline: Instant,
    journal: &mut QuestJournal,
) -> StepRun {
    if step.target.ids.is_empty() {
        return Err(StepFailure::unsupported("kill step without target ids"));
    }
    // The run loop switched the bot into the step's world; hunt at the locations of that world.
    let world = step_world(step);
    let positions: Vec<(f32, f32, f32)> = step
        .locations
        .iter()
        .filter(|l| match &world {
            Some(w) => l.world.eq_ignore_ascii_case(w),
            None => l.world.eq_ignore_ascii_case(MAIN_WORLD),
        })
        .map(|l| (l.x, l.y, l.z))
        .collect();
    if positions.is_empty() && step.status != "dynamic_spawn" && step.status != "scheduled_spawn" {
        // Reuse the "other world" / "no location" reason.
        return Err(step_location(step).err().unwrap_or_else(|| {
            StepFailure::unsupported("kill step without a location in its step world")
        }));
    }
    let want = step.target.count.max(1);
    let goal = KillGoal::Objective {
        quest_id,
        index: objective,
        want,
    };
    if let Some((_note, killed)) =
        hunt_until(game, &step.target.ids, &positions, goal, deadline).await?
    {
        return Ok((StepOutcomeKind::Ok, kill_note(&killed)));
    }
    if quest_failed(game, quest_id) {
        return Err(StepFailure::quest_failed(quest_id));
    }
    let mut last_reason = None;
    let fallback_end = Instant::now() + GM_SPAWN_WINDOW;
    for &template in step.target.ids.iter().take(5) {
        if goal_reached(game, goal, 0) {
            break;
        }
        let now = Instant::now();
        if now >= fallback_end {
            break;
        }
        match spawn_and_kill(
            game,
            template,
            goal,
            fallback_end.saturating_duration_since(now),
            false,
            journal,
        )
        .await
        {
            Ok((note, killed, spawned)) => {
                if spawned && !note.is_empty() {
                    return Ok((StepOutcomeKind::OkGmSpawn, note));
                }
                return Ok((StepOutcomeKind::Ok, kill_note(&killed)));
            }
            Err(f) => last_reason = Some(f),
        }
    }
    Err(last_reason.unwrap_or_else(|| {
        StepFailure::timeout(format!(
            "no spawn fallback could reach the kill goal for {:?}",
            step.target.ids
        ))
    }))
}

/// `QuestActObjAggro`: kill the quest's NPC acceptor with the bot's own skill while it holds the
/// aggro. The act only counts a `Kill` whose victim is the quest's NPC acceptor and ranks the
/// bot's aggro percentile (`quests/acts.rs` `ObjAggro`; `events.rs::kill_unit` reads
/// `npc_aggro_rating_percent`). A `/kill` alone leaves no aggro entry, so the step hits the NPC
/// with the configured combat skill first and only then finishes it.
#[allow(clippy::too_many_arguments)]
async fn step_aggro(
    game: &mut GameClient,
    quest_id: u32,
    step: &PlanStep,
    objective: Option<u8>,
    npc_template: Option<u32>,
    combat_skill: u32,
    deadline: Instant,
    journal: &mut QuestJournal,
) -> StepRun {
    let Some(template_id) = npc_template else {
        return Err(StepFailure::unsupported_reason(format!(
            "QuestActObjAggro needs an NPC acceptor but no npcs row has \
             engage_combat_give_quest_id = {quest_id}"
        )));
    };
    let _ = step;
    // The act stores the aggro rank (1..=3) in the objective; any rank satisfies it.
    let want = 1;
    let mut rounds = 0usize;
    let mut avoid: HashSet<u32> = HashSet::new();
    loop {
        if quest_completed(game, quest_id) || quest_ready(game, quest_id) {
            break;
        }
        if objective.is_some_and(|index| objective_count(game, quest_id, index) >= want) {
            break;
        }
        if quest_failed(game, quest_id) {
            return Err(StepFailure::quest_failed(quest_id));
        }
        if Instant::now() >= deadline {
            return Err(StepFailure::timeout(format!(
                "QuestActObjAggro did not set the aggro rank for quest {quest_id} \
                 ({rounds} attack round(s) on npc {template_id})"
            )));
        }
        let npc = match nearest_target(game, &[template_id], &avoid) {
            Some(npc) => npc,
            None => find_or_spawn_npc(game, template_id, deadline, journal).await?,
        };
        approach(game, npc.pos, deadline).await;
        game.change_target(npc.obj_id)
            .await
            .map_err(StepFailure::io)?;
        // A real hit creates the aggro entry (`bridges.rs::on_aggro` fires on damage).
        game.start_skill_as(
            combat_skill,
            SkillCaster::Unit(game.state.self_obj),
            SkillCastTarget::Unit(npc.obj_id),
        )
        .await
        .map_err(StepFailure::io)?;
        let until = (Instant::now() + Duration::from_secs(2)).min(deadline);
        let _ = wait_progress(game, quest_id, objective, want, until)
            .await
            .map_err(StepFailure::io)?;
        if objective.is_some_and(|index| objective_count(game, quest_id, index) >= want) {
            break;
        }
        // Still alive: the GM kill raises the kill event with the aggro table filled by the hit.
        let messages = game.gm("kill", KILL_WAIT).await.map_err(StepFailure::io)?;
        avoid.insert(npc.obj_id);
        if messages
            .iter()
            .any(|m| m.contains("already dead") || m.contains("Cannot kill"))
        {
            continue;
        }
        let until = (Instant::now() + Duration::from_secs(3)).min(deadline);
        let _ = wait_progress(game, quest_id, objective, want, until)
            .await
            .map_err(StepFailure::io)?;
        rounds += 1;
    }
    Ok((
        StepOutcomeKind::Ok,
        format!("aggro npc {template_id} ({rounds} round(s))"),
    ))
}

fn kill_note(killed: &[u32]) -> String {
    match killed.last() {
        Some(last) => format!("kill x{} (last obj {last:#x})", killed.len()),
        None => "kill".into(),
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
            item_sources: None,
            spheres: None,
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
            level_raised_to: None,
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
            fallbacks: Vec::new(),
            blocker: Blocker::default(),
            forced_complete: false,
            forced_prereqs: Vec::new(),
            value_checks: None,
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
    fn step_dispatch_covers_every_act_group() {
        let cases = [
            ("Start", "QuestActConAcceptNpc", StepAction::Accept),
            ("Start", "QuestActConAcceptDoodad", StepAction::AcceptDoodad),
            ("Progress", "QuestActObjTalk", StepAction::Talk),
            ("Progress", "QuestActObjMonsterHunt", StepAction::Kill),
            ("Progress", "QuestActObjMonsterGroupHunt", StepAction::Kill),
            ("Progress", "QuestActObjAggro", StepAction::Aggro),
            ("Start", "QuestActConAcceptNpcKill", StepAction::Accept),
            ("Start", "QuestActConAcceptNpcEmotion", StepAction::Accept),
            ("Ready", "QuestActConReportNpc", StepAction::Report),
            ("Ready", "QuestActConReportDoodad", StepAction::ReportDoodad),
            (
                "Progress",
                "QuestActObjInteraction",
                StepAction::DoodadInteraction,
            ),
            ("Start", "QuestActConAcceptItem", StepAction::AcceptItem),
            (
                "Start",
                "QuestActConAcceptComponent",
                StepAction::AcceptComponent,
            ),
            ("Progress", "QuestActObjItemGather", StepAction::Gather),
            ("Progress", "QuestActEtcItemObtain", StepAction::Gather),
            ("Start", "QuestActConAcceptItemGain", StepAction::Gather),
            ("Progress", "QuestActObjItemUse", StepAction::UseItem),
            ("Progress", "QuestActObjSphere", StepAction::Sphere),
            ("Progress", "QuestActCheckSphere", StepAction::Sphere),
            ("Progress", "QuestActObjLevel", StepAction::ObjLevel),
            ("Start", "QuestActConAcceptSphere", StepAction::AcceptSphere),
            ("Ready", "QuestActConAutoComplete", StepAction::Passive),
            ("Reward", "QuestActSupplyItem", StepAction::Passive),
            ("Reward", "QuestActSupplyCopper", StepAction::Passive),
            ("Progress", "QuestActCheckTimer", StepAction::Passive),
            (
                "Ready",
                "QuestActConReportJournal",
                StepAction::ReportJournal,
            ),
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
    fn dynamic_sources_only_block_the_npc_acts_the_runner_cannot_trigger() {
        let mut dynamic = step("Start", "QuestActConAcceptNpc");
        dynamic.target = target("npc", &[721], 1);
        dynamic.sources = Some(vec![PlanSource {
            effect_id: 72,
            skill_ids: vec![10732],
        }]);
        assert_eq!(step_action(&dynamic), StepAction::NeedsTrigger);

        // Hunts attempt dynamic targets: the earlier item use is expected to spawn them, and
        // the respawn / /spawn fallback covers the rest.
        let mut hunt = step("Progress", "QuestActObjMonsterHunt");
        hunt.target = target("npc", &[721], 1);
        hunt.sources = Some(vec![PlanSource {
            effect_id: 72,
            skill_ids: vec![10732],
        }]);
        assert_eq!(step_action(&hunt), StepAction::Kill);
        hunt.sources = Some(vec![PlanSource {
            effect_id: 569,
            skill_ids: Vec::new(),
        }]);
        assert_eq!(step_action(&hunt), StepAction::Kill);

        // A dynamic step in a Fail / Drop branch is still skipped.
        let mut skipped = step("Drop", "QuestActObjMonsterHunt");
        skipped.sources = Some(vec![PlanSource {
            effect_id: 72,
            skill_ids: vec![10732],
        }]);
        assert_eq!(step_action(&skipped), StepAction::Skip);
    }

    #[test]
    fn item_sources_deserialize_the_plan_shape_and_filter_to_the_targets() {
        let plan_step: PlanStep = serde_json::from_str(
            r#"{
              "component_id": 1, "component_kind": "Progress", "act_id": 2,
              "act_type": "QuestActObjItemGather",
              "target": {"kind": "item", "ids": [8017], "count": 2},
              "locations": [], "status": "ok",
              "item_sources": [{
                "item_id": 8017,
                "sources": [
                  {"type": "npc_loot", "npc_ids": [721], "loot_pack_id": 6396,
                   "chance": 1, "locations": [{"world": "main_world", "x": 1.0, "y": 2.0, "z": 3.0}]},
                  {"type": "doodad", "doodad_ids": [322], "func": "DoodadFuncLootItem",
                   "locations": []},
                  {"type": "quest_supply", "act_id": 42},
                  {"type": "store", "npc_ids": [0], "pack_ids": [218], "locations": []},
                  {"type": "item_open", "container_item_id": 27496, "skill_id": 21706,
                   "loot_pack_id": 7751, "chance": 10000000,
                   "container_sources": [
                     {"type": "npc_loot", "npc_ids": [], "loot_pack_id": 8300,
                      "chance": 500000, "locations": []},
                     {"type": "item_open", "container_item_id": 30542, "skill_id": 24250,
                      "loot_pack_id": 8300, "chance": 500000, "container_sources": []}
                   ]},
                  {"type": "item_open", "container_item_id": 31688, "skill_id": 24664,
                   "grant": "gain_item"}
                ]
              }, {
                "item_id": 9999,
                "sources": [{"type": "craft", "craft_ids": [7]}]
              }]
            }"#,
        )
        .unwrap();
        assert_eq!(plan_step.target, target("item", &[8017], 2));
        let sources = gather_sources(&plan_step);
        // The sources of another item id are ignored.
        assert_eq!(sources.len(), 6);
        assert_eq!(sources[0].kind, "npc_loot");
        assert_eq!(sources[0].npc_ids, [721]);
        assert_eq!(sources[0].loot_pack_id, 6396);
        assert_eq!(source_positions(sources[0], None).len(), 1);
        // The step world filters the locations: inside another world the main-world position is
        // not usable (the gather arms teleport within the current world).
        assert_eq!(
            source_positions(sources[0], Some("arche_mall_world")).len(),
            0
        );
        assert_eq!(sources[1].kind, "doodad");
        assert_eq!(sources[1].func.as_deref(), Some("DoodadFuncLootItem"));
        assert_eq!(sources[2].kind, "quest_supply");
        assert_eq!(sources[2].act_id, Some(42));
        assert_eq!(sources[3].kind, "store");
        // `item_open`: the container, its open skill and the container's own sources (plan.py).
        assert_eq!(sources[4].kind, "item_open");
        assert_eq!(sources[4].container_item_id, 27496);
        assert_eq!(sources[4].skill_id, 21706);
        assert_eq!(sources[4].loot_pack_id, 7751);
        let nested = sources[4].container_sources.as_ref().unwrap();
        assert_eq!(nested.len(), 2);
        assert_eq!(nested[0].kind, "npc_loot");
        assert_eq!(nested[0].loot_pack_id, 8300);
        assert_eq!(nested[1].container_item_id, 30542);
        assert!(nested[1].container_sources.as_ref().unwrap().is_empty());
        // The direct-grant variants carry `grant` and no loot pack.
        assert_eq!(sources[5].kind, "item_open");
        assert_eq!(sources[5].grant.as_deref(), Some("gain_item"));
        assert_eq!(sources[5].loot_pack_id, 0);
        assert!(sources[5].container_sources.is_none());

        // A step without `item_sources` has no sources (old plans).
        assert!(gather_sources(&step("Progress", "QuestActObjItemGather")).is_empty());
    }

    #[test]
    fn gather_goal_uses_the_objective_counter_and_reports_both() {
        use crate::inventory::ItemInfo;

        let mut state = GameState::default();
        state.bag.insert(
            4,
            ItemInfo {
                item_id: 0x0102,
                template_id: 14_635,
                count: 2,
                slot: 4,
            },
        );
        state.quests.insert(
            50,
            QuestInfo {
                template_id: 50,
                status: aaemu_game::systems::quests::model::status::PROGRESS,
                objectives: [0; 5],
            },
        );
        assert!(!gather_done(&state, &[14_635], 6, 50, Some(0)));
        assert_eq!(
            gather_note(&state, &[14_635], 6, 50, Some(0), "npc_loot pack 353"),
            "npc_loot pack 353 (bag 2/6, objective 0/6)"
        );

        // The objective counter alone satisfies the step (the server counted the loot even
        // while the bag decode lagged, quest 50's live case).
        state.quests.get_mut(&50).unwrap().objectives[0] = 6;
        assert!(gather_done(&state, &[14_635], 6, 50, Some(0)));
        assert_eq!(
            gather_note(&state, &[14_635], 6, 50, Some(0), "npc_loot pack 353"),
            "npc_loot pack 353 (bag 2/6, objective 6/6)"
        );

        // A live completion of the quest also ends a gather step.
        state.last_completed = Some(51);
        assert!(gather_done(&state, &[999], 1, 51, None));

        // Without an objective (acts that do not count) only the bag counts.
        assert!(!gather_done(&state, &[14_635], 6, 52, None));
        assert_eq!(
            gather_note(&state, &[14_635], 6, 52, None, "quest_supply act 42"),
            "quest_supply act 42 (bag 2/6)"
        );
    }

    #[test]
    fn implicit_accept_only_for_plans_without_accept_acts() {
        // Quests 3880 / 6038 / 6217: a Start component with no acts, only Reward steps.
        let mut no_acts = quest(3880, "ok");
        no_acts.steps = vec![
            step("Reward", "QuestActSupplyExp"),
            step("Reward", "QuestActConAutoComplete"),
        ];
        assert!(needs_implicit_accept(&no_acts));

        let mut accept = quest(10, "ok");
        accept.steps = vec![step("Start", "QuestActConAcceptNpc")];
        assert!(!needs_implicit_accept(&accept));

        // A dynamic (needs_trigger) accept act still counts as an accept act.
        let mut dynamic = quest(11, "ok");
        let mut step_ = step("Start", "QuestActConAcceptNpc");
        step_.sources = Some(vec![PlanSource {
            effect_id: 1,
            skill_ids: vec![2],
        }]);
        dynamic.steps = vec![step_];
        assert!(!needs_implicit_accept(&dynamic));

        // Every accept class counts, including item / sphere / component starts.
        for act_type in [
            "QuestActConAcceptDoodad",
            "QuestActConAcceptItem",
            "QuestActConAcceptItemGain",
            "QuestActConAcceptSphere",
            "QuestActConAcceptComponent",
        ] {
            let mut q = quest(12, "ok");
            q.steps = vec![step("Start", act_type)];
            assert!(!needs_implicit_accept(&q), "{act_type}");
        }
    }

    #[test]
    fn stuck_without_a_failed_step_still_carries_a_reason() {
        let mut all_passive = quest(3880, "ok");
        all_passive.steps = vec![
            step("Reward", "QuestActSupplyExp"),
            step("Reward", "QuestActConAutoComplete"),
        ];
        let failed = stuck_failed_step(&all_passive);
        assert_eq!(failed.act_id, 0);
        assert!(
            failed.reason.contains("only passive steps"),
            "{}",
            failed.reason
        );

        let mut with_kill = quest(10, "ok");
        with_kill.steps = vec![step("Progress", "QuestActObjMonsterHunt")];
        let failed = stuck_failed_step(&with_kill);
        assert_eq!(failed.act_id, 2);
        assert_eq!(failed.act_type, "QuestActObjMonsterHunt");
    }

    /// Quests 1835 / 5678: act-less Ready components. The runner sends the object-free turn-in
    /// but the server ignores it (`events.rs::on_report`), so the line must carry the cited
    /// server gap instead of the generic stuck reason.
    #[test]
    fn unfinished_ready_quests_report_the_server_gap() {
        use crate::quest::QuestInfo;

        // No steps at all (1835 / 5678): all-passive vacuously, READY after the turn-in.
        let no_steps = quest(1835, "ok");
        assert!(only_passive_steps(&no_steps));
        let reason = unfinished_ready_reason(&no_steps);
        assert!(reason.contains("quest 1835 stays in READY"), "{reason}");
        assert!(reason.contains("events.rs::on_report"), "{reason}");
        assert!(
            reason.contains("moved object-free reports to the Reward step"),
            "{reason}"
        );

        // A passive-only plan (prologue rewards) counts as well; an active step does not.
        let mut passive = quest(3880, "ok");
        passive.steps = vec![step("Reward", "QuestActSupplyExp")];
        assert!(only_passive_steps(&passive));
        let mut hunter = quest(10, "ok");
        hunter.steps = vec![step("Progress", "QuestActObjMonsterHunt")];
        assert!(!only_passive_steps(&hunter));

        // `quest_ready` only fires for a live READY row.
        let mut state = GameState::default();
        assert!(!quest_ready_state(&state, 1835));
        state.quests.insert(
            1835,
            QuestInfo {
                template_id: 1835,
                status: aaemu_game::systems::quests::model::status::READY,
                objectives: [0; 5],
            },
        );
        assert!(quest_ready_state(&state, 1835));
        assert!(!quest_ready_state(&state, 1836));
    }

    #[test]
    fn sphere_steps_without_geometry_report_the_stable_reason() {
        let without = step("Progress", "QuestActObjSphere");
        let failure = sphere_of(&without).unwrap_err();
        assert_eq!(failure.reason, "no sign sphere for component 1");
        assert_eq!(failure.quest, QuestOutcome::RunnerUnsupported);

        let mut with = step("Progress", "QuestActObjSphere");
        with.spheres = Some(vec![PlanSphere {
            world: "main_world".into(),
            x: 1.0,
            y: 2.0,
            z: 3.0,
            radius: 30.0,
            zone_id: 140,
            component_id: 64,
        }]);
        assert_eq!(sphere_of(&with).unwrap().radius, 30.0);

        // A sphere in another world is reachable: the run loop enters plan worlds, and the
        // sphere itself sets the step's world when there are no locations.
        let mut instance = step("Progress", "QuestActObjSphere");
        instance.spheres = Some(vec![PlanSphere {
            world: "instance_burntcastle_armory".into(),
            radius: 10.0,
            ..PlanSphere::default()
        }]);
        assert_eq!(sphere_of(&instance).unwrap().radius, 10.0);

        // A sphere world that disagrees with the step's locations stays unsupported: the run
        // loop switches to the locations' world, where the sphere cannot fire.
        let mut mismatch = step("Progress", "QuestActObjSphere");
        mismatch.locations = vec![PlanLocation {
            world: "main_world".into(),
            x: 0.0,
            y: 0.0,
            z: 0.0,
        }];
        mismatch.spheres = instance.spheres.clone();
        let failure = sphere_of(&mismatch).unwrap_err();
        assert!(
            failure.reason.contains("instance_burntcastle_armory"),
            "{}",
            failure.reason
        );
    }

    /// v8 worlds: a step's world comes from its first location (a sphere step falls back to the
    /// sphere); the location helper keeps plan order and the run loop switched the bot there.
    #[test]
    fn step_world_comes_from_the_first_location_or_sphere() {
        let mut s = step("Progress", "QuestActObjMonsterHunt");
        assert_eq!(step_world(&s), None); // no locations: the main world
        s.locations = vec![
            PlanLocation {
                world: "arche_mall_world".into(),
                x: 1.0,
                y: 2.0,
                z: 3.0,
            },
            PlanLocation {
                world: "racing_test_world".into(),
                x: 4.0,
                y: 5.0,
                z: 6.0,
            },
        ];
        assert_eq!(step_world(&s).as_deref(), Some("arche_mall_world"));
        assert_eq!(step_location(&s).unwrap(), (1.0, 2.0, 3.0));

        let mut main = step("Progress", "QuestActObjMonsterHunt");
        main.locations = vec![PlanLocation {
            world: "main_world".into(),
            x: 7.0,
            y: 8.0,
            z: 9.0,
        }];
        assert_eq!(step_world(&main), None);
        assert_eq!(step_location(&main).unwrap(), (7.0, 8.0, 9.0));

        let mut sphere = step("Progress", "QuestActObjSphere");
        sphere.spheres = Some(vec![PlanSphere {
            world: "instance_library".into(),
            ..PlanSphere::default()
        }]);
        assert_eq!(step_world(&sphere).as_deref(), Some("instance_library"));

        let empty = step("Progress", "QuestActConReportNpc");
        assert!(step_location(&empty)
            .unwrap_err()
            .reason
            .contains("no spawn location"));
    }

    /// The observed instance (last `SCLoadInstancePacket` / select snapshot) is authoritative
    /// for world tracking; a main-world load clears a stale plan world.
    #[test]
    fn world_tracker_follows_the_server_instance() {
        let mut tracker = WorldTracker {
            current: Some("instance_library".into()),
            observed_instance: 7,
        };
        let mut state = GameState {
            world_instance: 7,
            ..GameState::default()
        };

        tracker.observe(&state);
        assert_eq!(tracker.observed_instance, 7);
        assert_eq!(tracker.current.as_deref(), Some("instance_library"));
        assert!(!tracker.in_main_world());

        // A main-world load means the server moved the bot out: the plan world is stale.
        state.world_instance = MAIN_INSTANCE_ID;
        tracker.observe(&state);
        assert_eq!(tracker.observed_instance, MAIN_INSTANCE_ID);
        assert!(tracker.current.is_none());
        assert!(tracker.in_main_world());
    }

    /// The pre-quest recovery decision: a main-world target is the observed instance 0 (a
    /// stale plan world does not force an exit), a plan-world target needs both the instance
    /// and the matching plan bookkeeping, anything else must be recovered.
    #[test]
    fn in_target_world_prefers_the_observed_instance() {
        assert!(in_target_world(0, None, None));
        assert!(in_target_world(0, Some("instance_library"), None));
        assert!(!in_target_world(7, None, None));
        assert!(!in_target_world(7, Some("instance_library"), None));

        assert!(in_target_world(
            7,
            Some("instance_library"),
            Some("instance_library")
        ));
        assert!(!in_target_world(
            0,
            Some("instance_library"),
            Some("instance_library")
        ));
        assert!(!in_target_world(7, None, Some("instance_library")));
        assert!(!in_target_world(
            7,
            Some("arche_mall_world"),
            Some("instance_library")
        ));
    }

    /// The failed exit is reported once (suspect with observed/expected evidence on the quest
    /// that caused it); the GM recall recovery is a marked `gm_teleport` fallback naming the
    /// skill and code path; a failed recovery is an `error` line that stops the run.
    #[test]
    fn recovery_fallbacks_and_suspects_name_the_paths() {
        let fallback = gm_teleport_fallback(
            7,
            "no SCLoadInstancePacket after using the exit doodad 4535 (skill 16560)",
            11_239,
        );
        assert_eq!(fallback.kind, "gm_teleport");
        assert_eq!(fallback.target, "main_world");
        assert!(fallback.reason.contains("4535"), "{}", fallback.reason);
        assert!(fallback.reason.contains("11239"), "{}", fallback.reason);
        assert!(
            fallback.reason.contains("movement.rs::return_"),
            "{}",
            fallback.reason
        );

        let suspect = exit_failure_suspect(
            7,
            "no SCLoadInstancePacket after using the exit doodad 4535 (skill 16560)",
        );
        assert_eq!(suspect.class, blocker_class::SUSPECT);
        assert!(
            suspect.reason.contains("world instance 7"),
            "{}",
            suspect.reason
        );
        assert!(
            suspect.code.contains("request_leave_instance"),
            "{}",
            suspect.code
        );
        assert!(
            suspect.evidence.contains("observed:"),
            "{}",
            suspect.evidence
        );
        assert!(
            suspect.evidence.contains("expected:"),
            "{}",
            suspect.evidence
        );
        assert!(
            suspect.evidence.contains("SCLoadInstancePacket"),
            "{}",
            suspect.evidence
        );

        let character = RunCharacter {
            name: "Bot".into(),
            race: 1,
            gender: 1,
            start_level: 1,
        };
        let result =
            recovery_failed_result(&quest(10, "ok"), &character, "stuck in world instance 7");
        assert_eq!(result.result, QuestOutcome::Error);
        assert_eq!(result.messages, ["stuck in world instance 7"]);
        assert_eq!(result.blocker.class, blocker_class::SUSPECT);
        assert!(result.blocker.code.contains("recover_main_world"));
    }

    #[test]
    fn plan_spheres_deserialize_the_plan_shape() {
        let plan_step: PlanStep = serde_json::from_str(
            r#"{
              "component_id": 64, "component_kind": "Start", "act_id": 11435,
              "act_type": "QuestActConAcceptSphere",
              "target": {"kind": "sphere", "ids": [5], "count": 1},
              "locations": [{"world": "main_world", "x": 12595.5, "y": 14006.63, "z": 108.25}],
              "status": "ok",
              "spheres": [{"world": "main_world", "x": 12595.5, "y": 14006.63, "z": 108.25,
                           "radius": 30.0, "zone_id": 140, "component_id": 64}]
            }"#,
        )
        .unwrap();
        let sphere = sphere_of(&plan_step).unwrap().clone();
        assert_eq!(sphere.component_id, 64);
        assert_eq!(sphere.radius, 30.0);
        assert_eq!(sphere.zone_id, 140);
        assert!((sphere.x - 12595.5).abs() < 0.01);

        // Steps without `spheres` (old plans / the data gap) load with an empty option.
        assert!(step("Progress", "QuestActObjSphere").spheres.is_none());
    }

    #[test]
    fn item_use_target_kinds_follow_the_skill_target_type() {
        for target_type in [0, 1, 2, 3, 5, 10, 16, 17] {
            assert_eq!(use_target_kind(target_type), UseTargetKind::SelfUnit);
        }
        assert_eq!(use_target_kind(4), UseTargetKind::NearestNpc);
        for target_type in [6, 11, 12, 13, 14, 15, 18] {
            assert_eq!(use_target_kind(target_type), UseTargetKind::Position);
        }
        assert_eq!(use_target_kind(8), UseTargetKind::Doodad);
        for target_type in [7, 9, 19, 20] {
            assert_eq!(use_target_kind(target_type), UseTargetKind::Unsupported);
        }
    }

    #[test]
    fn blocked_and_auxiliary_plan_quests_are_gated_without_running() {
        assert_eq!(
            plan_gate(&quest(10, "blocked")),
            Some(QuestOutcome::BlockedPlan)
        );
        assert_eq!(
            plan_gate(&quest(10, "auxiliary")),
            Some(QuestOutcome::Auxiliary)
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

    /// v8 ordering: among ready quests the lowest `min_level` runs first (ties by plan order),
    /// but a prerequisite always runs before its dependents, whatever its level.
    #[test]
    fn run_order_prefers_lower_min_level_and_keeps_prerequisites_first() {
        let mut plan = vec![quest(10, "ok"), quest(11, "ok"), quest(12, "ok")];
        plan[0].min_level = Some(30); // first in the plan, highest level demand
        plan[1].min_level = Some(5);
        plan[2].min_level = Some(10);
        assert_eq!(topological_order(&plan, &[0, 1, 2]).order, vec![1, 2, 0]);
        // 11 (level 5) now requires 10 (level 30): the prerequisite still runs first, and the
        // ready level-10 quest (12) goes before the level-30 prerequisite.
        plan[1].prereq_quests = vec![10];
        assert_eq!(topological_order(&plan, &[0, 1, 2]).order, vec![2, 0, 1]);
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

    /// `craft` sources raise the quest's level demand by their recipe skill's minimum (the
    /// Commerce craft skill 16766 needs 10); the gated part runs only where the game data is
    /// present.
    #[test]
    fn craft_sources_raise_the_level_demand() {
        // No craft database: nothing to add.
        let mut q = quest(5308, "ok");
        q.steps = vec![step("Progress", "QuestActObjItemGather")];
        assert_eq!(craft_level_demand(&q, None), 0);

        let path = Path::new(crate::compactdb::DEFAULT_COMPACT_DB);
        if !path.exists() {
            return;
        }
        let db = crate::craftrun::open_craft_db(Some(path)).unwrap();
        // A recipe without a skill level requirement adds nothing (the doll bundle craft 5568).
        let mut step_ = step("Progress", "QuestActObjItemGather");
        step_.item_sources = Some(vec![PlanItemSources {
            item_id: 27_669,
            sources: vec![PlanItemSource {
                kind: "craft".into(),
                craft_ids: vec![5568],
                ..PlanItemSource::default()
            }],
        }]);
        let mut q = quest(5308, "ok");
        q.steps = vec![step_];
        assert_eq!(craft_level_demand(&q, Some(&db)), 0);
        // The deepest requirement of the quest's crafts becomes the demand.
        let Some(plan) = db
            .all_crafts()
            .into_iter()
            .find(|plan| plan.skill.level_min > 0)
        else {
            return;
        };
        let mut step_ = step("Progress", "QuestActObjItemGather");
        step_.item_sources = Some(vec![PlanItemSources {
            item_id: 1,
            sources: vec![PlanItemSource {
                kind: "craft".into(),
                craft_ids: vec![plan.id],
                ..PlanItemSource::default()
            }],
        }]);
        let mut q = quest(5309, "ok");
        q.steps = vec![step_];
        assert_eq!(
            craft_level_demand(&q, Some(&db)),
            plan.skill.level_min.max(0) as u32
        );
    }

    #[test]
    fn requirement_gate_checks_known_kinds() {
        let completed: BTreeSet<u32> = [61u32].into_iter().collect();
        let owned_items: BTreeSet<u32> = BTreeSet::new();
        let active_quests: BTreeSet<u32> = BTreeSet::new();
        let facts = KnownFacts {
            level: 14,
            race: Some(1),
            gender: Some(1),
            faction: Some(101),
            mother_faction: Some(148),
            completed: &completed,
            owned_items: &owned_items,
            active_quests: &active_quests,
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

        // Faction / mother faction (2302: the Nuian bot's mother 148 != Haranya 149 blocks;
        // an unresolved mother faction stays undecided).
        let q = quest_with(&[], vec![requirement(0x28, "FactionMatch", 101, 0)], None);
        assert!(requirement_gate(&q, &facts).is_none());
        let q = quest_with(&[], vec![requirement(0x28, "FactionMatch", 109, 0)], None);
        assert!(requirement_gate(&q, &facts)
            .unwrap()
            .contains("FactionMatch"));
        let q = quest_with(&[], vec![requirement(0x2A, "MotherFaction", 149, 0)], None);
        assert!(requirement_gate(&q, &facts)
            .unwrap()
            .contains("MotherFaction"));
        let q = quest_with(&[], vec![requirement(0x2A, "MotherFaction", 148, 0)], None);
        assert!(requirement_gate(&q, &facts).is_none());
        let q = quest_with(
            &[],
            vec![requirement(0x38, "MotherFactionOnly", 149, 0)],
            None,
        );
        assert!(requirement_gate(&q, &facts)
            .unwrap()
            .contains("MotherFactionOnly"));
        let q = quest_with(
            &[],
            vec![requirement(0x3A, "FactionMatchOnlyNot", 101, 0)],
            None,
        );
        assert!(requirement_gate(&q, &facts)
            .unwrap()
            .contains("FactionMatchOnlyNot"));
        let unknown = KnownFacts {
            mother_faction: None,
            ..facts
        };
        let q = quest_with(&[], vec![requirement(0x2A, "MotherFaction", 149, 0)], None);
        assert!(requirement_gate(&q, &unknown).is_none());
        let unknown = KnownFacts {
            faction: None,
            ..facts
        };
        let q = quest_with(&[], vec![requirement(0x28, "FactionMatch", 109, 0)], None);
        assert!(requirement_gate(&q, &unknown).is_none());

        // A level cap the ensured level is already above blocks; one above it stays unknown.
        let q = quest_with(&[], vec![requirement(1, "Level", 1, 10)], None);
        assert!(requirement_gate(&q, &facts).unwrap().contains("Level"));
        let q = quest_with(&[], vec![requirement(1, "Level", 1, 20)], None);
        assert!(requirement_gate(&q, &facts).is_none());
    }

    #[test]
    fn or_group_only_blocks_when_every_alternative_is_unmet() {
        let completed = BTreeSet::new();
        let owned_items = BTreeSet::new();
        let active_quests = BTreeSet::new();
        let facts = KnownFacts {
            level: 5,
            race: Some(1),
            gender: Some(1),
            faction: Some(101),
            mother_faction: Some(148),
            completed: &completed,
            owned_items: &owned_items,
            active_quests: &active_quests,
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
            (QuestOutcome::AlreadyCompleted, "already_completed"),
            (QuestOutcome::BlockedPlan, "blocked_plan"),
            (QuestOutcome::Auxiliary, "auxiliary"),
            (QuestOutcome::NoStarter, "no_starter"),
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
        assert_eq!(
            serde_json::to_value(StepOutcomeKind::OkGmSpawn).unwrap(),
            serde_json::json!("ok_gm_spawn")
        );
    }

    #[test]
    fn blocker_classification_names_the_owner() {
        let completed = BTreeSet::new();
        let owned = BTreeSet::new();
        let active = BTreeSet::new();
        let facts = KnownFacts {
            level: 5,
            race: Some(1),
            gender: Some(1),
            faction: Some(101),
            mother_faction: Some(148),
            completed: &completed,
            owned_items: &owned,
            active_quests: &active,
        };
        let silent = FailedStep {
            act_id: 0,
            act_type: String::new(),
            reason: "no answer from server (accept silently ignored; see engine.rs::add_quest)"
                .into(),
        };
        // An undecided start requirement explains the silence → requirement.
        let mut gated = quest(10, "ok");
        gated.requires = vec![requirement(2, "Ability", 1, 50)];
        let blocker = classify_blocker(
            &gated,
            Some(&facts),
            QuestOutcome::Rejected,
            Some(&silent),
            &QuestJournal::default(),
        );
        assert_eq!(blocker.class, blocker_class::REQUIREMENT, "{blocker:?}");
        // Nothing left undecided → the server refused without an error → a suspect, and every
        // suspect carries observed-vs-expected evidence.
        let plain = quest(10, "ok");
        let blocker = classify_blocker(
            &plain,
            Some(&facts),
            QuestOutcome::Rejected,
            Some(&silent),
            &QuestJournal::default(),
        );
        assert_eq!(blocker.class, blocker_class::SUSPECT, "{blocker:?}");
        assert!(
            blocker.evidence.contains("observed:") && blocker.evidence.contains("expected:"),
            "{blocker:?}"
        );
        // A 1.2 data gap (no sign sphere) is not the server's fault.
        let sphere = FailedStep {
            act_id: 1,
            act_type: "QuestActObjSphere".into(),
            reason: "no sign sphere for component 6499".into(),
        };
        let blocker = classify_blocker(
            &plain,
            Some(&facts),
            QuestOutcome::RunnerUnsupported,
            Some(&sphere),
            &QuestJournal::default(),
        );
        assert_eq!(blocker.class, blocker_class::DATA_GAP, "{blocker:?}");
        // A completed quest keeps the blocker of the fallback it needed.
        let mut journal = QuestJournal::default();
        journal.fallback("gm_item", "1466", "no source");
        journal.note_blocker(
            blocker_class::DATA_GAP,
            "no source in the 1.2 data",
            "compact.sqlite3",
        );
        let blocker = classify_blocker(
            &plain,
            Some(&facts),
            QuestOutcome::Completed,
            None,
            &journal,
        );
        assert_eq!(blocker.class, blocker_class::DATA_GAP, "{blocker:?}");
        // A clean completion has no blocker.
        let blocker = classify_blocker(
            &plain,
            Some(&facts),
            QuestOutcome::Completed,
            None,
            &QuestJournal::default(),
        );
        assert_eq!(blocker.class, blocker_class::NONE, "{blocker:?}");
        // `QuestDailyLimit` is the server's "already completed, not repeatable" answer
        // (`engine.rs::add_quest`): a requirement, never a server bug.
        let daily = FailedStep {
            act_id: 0,
            act_type: String::new(),
            reason: "accept rejected: QuestDailyLimit".into(),
        };
        let blocker = classify_blocker(
            &plain,
            Some(&facts),
            QuestOutcome::Rejected,
            Some(&daily),
            &QuestJournal::default(),
        );
        assert_eq!(blocker.class, blocker_class::REQUIREMENT, "{blocker:?}");
        // A full-bag GM add is the runner's own state, not the GM command's fault.
        let bag_full = FailedStep {
            act_id: 0,
            act_type: String::new(),
            reason: "GM /item add self 1466 1 created nothing (bag full; chat: x)".into(),
        };
        let blocker = classify_blocker(
            &plain,
            Some(&facts),
            QuestOutcome::RunnerUnsupported,
            Some(&bag_full),
            &QuestJournal::default(),
        );
        assert_eq!(blocker.class, blocker_class::RUNNER_GAP, "{blocker:?}");
        // A GM-spawned fallback that cannot reach a large kill goal is the runner's window; a
        // small goal it cannot reach stays a server bug.
        let large = FailedStep {
            act_id: 0,
            act_type: String::new(),
            reason: "gm spawn fallback did not reach the goal for npc 1 (objective 0 of quest 2 \
                     at 19/300, GM-spawned)"
                .into(),
        };
        let blocker = classify_blocker(
            &plain,
            Some(&facts),
            QuestOutcome::Stuck,
            Some(&large),
            &QuestJournal::default(),
        );
        assert_eq!(blocker.class, blocker_class::RUNNER_GAP, "{blocker:?}");
        let small = FailedStep {
            act_id: 0,
            act_type: String::new(),
            reason: "gm spawn fallback did not reach the goal for npc 1 (objective 0 of quest 2 \
                     at 2/8, GM-spawned)"
                .into(),
        };
        let blocker = classify_blocker(
            &plain,
            Some(&facts),
            QuestOutcome::Stuck,
            Some(&small),
            &QuestJournal::default(),
        );
        assert_eq!(blocker.class, blocker_class::SUSPECT, "{blocker:?}");
    }

    #[test]
    fn forced_prerequisites_are_recorded_for_dependents() {
        let mut forced = BTreeSet::new();
        forced.insert(61);
        let mut dependent = quest(10, "ok");
        dependent.prereq_quests = vec![61, 62];
        dependent.requires = vec![
            requirement(0x1F, "CompleteQuestContext", 61, 0),
            requirement(0x1F, "CompleteQuestContext", 63, 0),
        ];
        assert_eq!(forced_prereqs_of(&dependent, &forced), vec![61]);
        assert!(forced_prereqs_of(&dependent, &BTreeSet::new()).is_empty());
    }

    #[test]
    fn summary_counts_results_and_top_failure_reasons() {
        let mut with_fallback = line(15, QuestOutcome::Completed, None);
        with_fallback
            .fallbacks
            .push(Fallback::new("gm_item", "1466", "no source"));
        with_fallback.blocker =
            Blocker::new(blocker_class::DATA_GAP, "no source in the data", "compact");
        let mut forced = line(16, QuestOutcome::Stuck, Some("item(s) [1] not obtained"));
        forced.forced_complete = true;
        forced
            .fallbacks
            .push(Fallback::new("forced_complete", "16", "real result stuck"));
        forced.blocker = Blocker::new(blocker_class::RUNNER_GAP, "no source", "runner");
        let mut suspect = line(
            17,
            QuestOutcome::Stuck,
            Some("using the item did not update quest 17"),
        );
        suspect.blocker = Blocker::suspect(
            "using the item did not update quest 17",
            "skills item_use",
            "observed: no objective change; expected: the use skill advances the act",
        );
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
                with_fallback,
                forced,
                suspect,
            ],
            aborted: false,
            ordering_cyclic: 1,
        };
        let text = render_summary(&report);
        assert!(text.contains("completed=2"), "{text}");
        assert!(text.contains("rejected=2"), "{text}");
        assert!(text.contains("prereq_missing=2"), "{text}");
        assert!(text.contains("none=5"), "{text}");
        assert!(text.contains("suspect=1"), "{text}");
        assert!(text.contains("suspects are triaged separately"), "{text}");
        assert!(text.contains("data_gap=1"), "{text}");
        assert!(text.contains("runner_gap=1"), "{text}");
        assert!(
            text.contains(
                "completed without fallback=1 with fallbacks=1 forced=1 not_applicable=0 \
                 already_completed=0"
            ),
            "{text}"
        );
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

    /// Quests 3880 / 6038 / 6217: the server auto-completes (`ConAutoComplete`), drops the row
    /// and sends `SCQuestContextCompletedPacket`, which carries no status. The stale `INVALID`
    /// of the last update must not be reported as the final status.
    #[test]
    fn final_status_reports_the_completion_when_the_row_is_gone() {
        let completed = aaemu_game::systems::quests::model::status::COMPLETED;
        let progress = aaemu_game::systems::quests::model::status::PROGRESS;
        assert_eq!(
            final_status_of(QuestOutcome::Completed, None, Some(0)),
            Some(completed)
        );
        // A live row still wins (a daily quest that restarted).
        assert_eq!(
            final_status_of(QuestOutcome::Completed, Some(progress), Some(0)),
            Some(progress)
        );
        // Every other outcome keeps the last seen row.
        assert_eq!(final_status_of(QuestOutcome::Stuck, None, Some(0)), Some(0));
        assert_eq!(final_status_of(QuestOutcome::Stuck, None, None), None);
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
            level_raised_to: Some(14),
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
                    note: "npc 7708".into(),
                    ms: 12,
                },
                StepOutcome {
                    act_id: 13979,
                    act_type: "QuestActObjItemGather".into(),
                    outcome: StepOutcomeKind::Unsupported,
                    note: "no known item source".into(),
                    ms: 3,
                },
            ],
            final_status: Some(1),
            final_objectives: Some([0, 0, 0, 0, 0]),
            messages: vec!["[Quest] accepted".into()],
            elapsed_ms: 1234,
            fallbacks: vec![Fallback::new(
                "gm_item",
                "1466",
                "no real source produced the item; /item add self 1466 1",
            )],
            blocker: Blocker::new(
                blocker_class::DATA_GAP,
                "no source in the 1.2 data",
                "compact.sqlite3",
            ),
            forced_complete: true,
            forced_prereqs: vec![61],
            value_checks: None,
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
                "level_raised_to": 14,
                "result": "runner_unsupported",
                "failed_step": {
                    "act_id": 13979,
                    "act_type": "QuestActObjItemGather",
                    "reason": "QuestActObjItemGather is not handled by runner v1"
                },
                "steps": [
                    {"act_id": 68, "act_type": "QuestActConAcceptNpc", "outcome": "ok",
                     "note": "npc 7708", "ms": 12},
                    {"act_id": 13979, "act_type": "QuestActObjItemGather",
                     "outcome": "unsupported", "note": "no known item source", "ms": 3}
                ],
                "final_status": 1,
                "final_objectives": [0, 0, 0, 0, 0],
                "messages": ["[Quest] accepted"],
                "elapsed_ms": 1234,
                "fallbacks": [{"kind": "gm_item", "target": "1466",
                               "reason": "no real source produced the item; /item add self 1466 1"}],
                "blocker": {"class": "data_gap", "reason": "no source in the 1.2 data",
                            "code": "compact.sqlite3"},
                "forced_complete": true,
                "forced_prereqs": [61]
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
        assert_eq!(value["blocker"]["class"], "data_gap");
        // Clean runs omit `fallbacks` / `forced_complete` / `forced_prereqs` but always carry
        // the blocker.
        let clean = line(10, QuestOutcome::Completed, None);
        let value = serde_json::to_value(&clean).unwrap();
        assert!(value.get("fallbacks").is_none());
        assert!(value.get("forced_complete").is_none());
        assert_eq!(value["blocker"]["class"], "none");
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
