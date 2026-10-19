//! Skill sweep runner (`aaemu-testclient skill`): casts every class skill of the same
//! `compact.sqlite3` the game server loads (`skills`, `skill_effects` -> `effects` ->
//! `buff_effects` / `damage_effects` / `heal_effects`, `unit_reqs`) once through the packets the
//! 1.2 client sends, and appends one JSON result line per skill.
//!
//! Server flow being exercised (`systems/skills/handlers.rs`, `systems/skills/skill.rs`):
//! `CSStartSkillPacket` -> learned-check -> `Skill.Use` (target resolution, `validation::check`,
//! `unit_reqs::can_use_skill_on_target`, cooldown / GCD, mana) -> `Skill.Cast` (cast timer or
//! channel) -> `SCSkillStarted` / `SCSkillFired` / `SCSkillEnded` / `SCSkillStopped`,
//! `SCBuffCreated` / `SCBuffRemoved`, `SCUnitPoints`.
//!
//! Preparation per skill, with the 1.2 client packets and the GM commands of
//! `systems/services/commands`:
//!
//! * the skill's ability is made one of `ability1..3` with `CSSwapAbilityPacket` (the server
//!   resets the swapped-out ability's learned skills, which also frees skill points);
//! * `/level` raises the character and its active abilities to the level the data wants
//!   (`skills.ability_level`, capped by `ExperienceManager.PlayerLevelCap` = 55 on this data);
//! * `CSLearnSkillPacket` learns the skill (`SCSkillLearnedPacket` confirms; the server answers
//!   nothing when the ability is inactive or skill points are short — the cast still runs
//!   through the handler's template fallback);
//! * the four holdable slots the data's `unit_reqs` can demand are equipped once at run start
//!   (`/item add self` + the client's `CSSwapItemsPacket` into the equipment container):
//!   mainhand one-hand sword, offhand shield, ranged bow, musical instrument;
//! * hostile skills cast on a `/spawn npc` training dummy at a data-appropriate distance with
//!   the bot facing it; self/friendly on the bot; position skills at the target position;
//!   doodad skills on a `/spawn doodad` copy of `DOODAD_TEMPLATE`;
//! * `/heal` refills the bot and the dummy before every cast; `/resetcd` clears skill
//!   cooldowns between skills; a second immediate cast records whether the server rejects with
//!   `SkillResult` cooldown (8) as `skills.cooldown_time` demands.
//!
//! Every line records the data-vs-observed fields (cast times, mana, buffs, HP deltas, channel
//! ticks, cooldown rejection) and a `mismatches` array with a short reason per deviation.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use aaemu_game::opcodes::sc;
use aaemu_game::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};
use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};

use crate::compactdb::{DEFAULT_COMPACT_DB, DEFAULT_COMPACT_DB_FROM_RUST};
use crate::craftrun::{equipment_tasks, EquippedItem, RunCharacter};
use crate::game::{chat_text, GameClient, Packet};
use crate::login::LoginClient;
use crate::record::Recorder;
use crate::scenario::BotConfig;

/// Packet-pump chunk used by every bounded wait.
const PUMP: Duration = Duration::from_millis(250);
/// Quiet window after a GM command's first reply.
const COMMAND_QUIET: Duration = Duration::from_millis(300);
/// How long a GM command's effect may take to show (item give, spawn, level, ...).
const GM_EFFECT_WAIT: Duration = Duration::from_secs(4);
/// `/resetcd` has no single reply packet to latch on (its answers arrive compressed); a short
/// flat wait is enough because the next cast is seconds away anyway.
const RESETCD_WAIT: Duration = Duration::from_millis(700);
/// Quiet window after the first cast completion signal.
/// A cast that gets no SkillStarted/Fired/Ended/error packet within this window is a silent drop.
const SILENT_DROP_WAIT: Duration = Duration::from_secs(6);
const SETTLE_QUIET: Duration = Duration::from_millis(400);
/// Default per-skill timeout (`--step-timeout-secs`); the longest channel is 60 s, so the
/// default covers it plus setup and settle.
pub const DEFAULT_STEP_TIMEOUT: Duration = Duration::from_secs(90);
/// `/level` cap on the shipped data (`ExperienceManager.PlayerLevelCap`); level 55 also has the
/// 28 skill points one ability's skill tree needs (`levels.skill_points`).
const TARGET_LEVEL: u8 = 55;
/// `AbilityType.None`.
const ABILITY_NONE: u8 = 11;
/// `/spawn npc` training dummy: level 50, passive (`aggression = f`), faction 170.
const DUMMY_TEMPLATE: u32 = 13013;
/// Doodad spawned for `SkillTargetType.Doodad` casts (the Amber Boulder of the doodad
/// scenario; exists in the 1.2 data).
const DOODAD_TEMPLATE: u32 = 4252;
/// `EquipmentItemSlot` indices of the four holdable slots.
const SLOT_MAINHAND: u8 = 15;
const SLOT_OFFHAND: u8 = 16;
const SLOT_RANGED: u8 = 17;
const SLOT_MUSICAL: u8 = 18;
/// One-hand sword (first-steps long sword), shield, bow and string instrument; the four cover
/// every holdable `unit_reqs` can demand (`equip_slot` 8, `equip_shield` 21, `equip_ranged` 29).
const GEAR: [(u32, u8); 4] = [
    (5569, SLOT_MAINHAND),
    (6152, SLOT_OFFHAND),
    (6127, SLOT_RANGED),
    (6177, SLOT_MUSICAL),
];

// -------------------------------------------------------------------------------------------
// Compact database: skill plan data
// -------------------------------------------------------------------------------------------

/// One `damage_effects` row of a skill, as the server loads it for `DamageEffect.Apply`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DamageRow {
    pub effect_id: u32,
    pub damage_type: i32,
    pub fixed_min: i32,
    pub fixed_max: i32,
    pub multiplier: f64,
    pub use_mainhand_weapon: bool,
    pub use_level_damage: bool,
    pub level_md: f64,
    pub dps_multiplier: f64,
    pub health_steal_ratio: i32,
    pub mana_steal_ratio: i32,
}

/// One `heal_effects` row of a skill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealRow {
    pub effect_id: u32,
    pub fixed_min: i32,
    pub fixed_max: i32,
    pub use_level_heal: bool,
    pub level_md: f64,
}

/// One `unit_reqs` row owned by the skill (`kind_id`, `value1`, `value2`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReqRow {
    pub kind: i32,
    pub value1: i32,
    pub value2: i32,
}

/// What the skill's `skill_effects` -> `effects` resolve to.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EffectSummary {
    /// `buff_effects.buff_id` of every `BuffEffect` with `application_method` Target (1) /
    /// SourceToPos (4): the server applies these to the cast target.
    pub buff_target: Vec<u32>,
    /// `buff_effects.buff_id` of every `BuffEffect` with `application_method` Source (2) /
    /// SourceOnce (3): the server applies these to the caster.
    pub buff_caster: Vec<u32>,
    pub damage_rows: Vec<DamageRow>,
    pub heal_rows: Vec<HealRow>,
    /// Other effect types (`SpecialEffect`, `DispelEffect`, ...) by row count.
    pub other: BTreeMap<String, usize>,
}

impl EffectSummary {
    /// Every buff id the data expects a cast to apply (caster or target side).
    pub fn expected_buffs(&self) -> Vec<u32> {
        let mut out: Vec<u32> = self
            .buff_target
            .iter()
            .chain(self.buff_caster.iter())
            .copied()
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }
}

/// One `skills` row with the data the sweep checks against.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillPlan {
    pub id: u32,
    pub name: String,
    pub ability_id: u8,
    pub ability_level: i32,
    pub level_step: i32,
    pub skill_points: i32,
    pub auto_learn: bool,
    pub need_learn: bool,
    pub show: bool,
    pub target_type: u8,
    pub target_relation: u8,
    pub casting_time: i32,
    pub channeling_time: i32,
    pub channeling_tick: i32,
    pub cooldown_time: i32,
    pub mana_cost: i32,
    pub mana_level_md: f64,
    pub min_range: f64,
    pub max_range: f64,
    pub toggle_buff_id: u32,
    pub effect_delay: i32,
    pub effect_repeat_count: i32,
    pub effect_repeat_tick: i32,
    pub active_weapon_id: i32,
    pub mainhand_tool_id: u32,
    pub offhand_tool_id: u32,
    pub target_dead: bool,
    pub or_unit_reqs: bool,
    /// `skills.plot_id` (0 = none): plot skills answer with `SCPlotEvent` packets.
    pub plot_id: u32,
    pub effects: EffectSummary,
    pub reqs: Vec<ReqRow>,
}

impl SkillPlan {
    /// The data's description of what a passive/toggle skill should do, for the record.
    pub fn data_note(&self) -> String {
        let mut parts = Vec::new();
        if self.toggle_buff_id != 0 {
            parts.push(format!("toggle buff {}", self.toggle_buff_id));
        }
        if self.effects.damage_rows.is_empty()
            && self.effects.heal_rows.is_empty()
            && self.effects.expected_buffs().is_empty()
            && !self.effects.other.is_empty()
        {
            parts.push(format!(
                "no damage/heal/buff row, effects: {}",
                self.effects
                    .other
                    .iter()
                    .map(|(k, n)| format!("{k} x{n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !self.auto_learn {
            parts.push("auto_learn f".into());
        }
        if self.need_learn {
            parts.push("need_learn t".into());
        }
        if !self.show {
            parts.push("hidden (show f)".into());
        }
        parts.join("; ")
    }

    /// `SkillTargetType` name (`systems/skills/types.rs`).
    pub fn target_type_name(&self) -> &'static str {
        match self.target_type {
            0 => "self",
            1 => "friendly",
            2 => "party",
            3 => "raid",
            4 => "hostile",
            5 => "any_unit",
            6 => "position",
            7 => "line",
            8 => "doodad",
            9 => "item",
            10 => "pet",
            11 => "ballistic_pos",
            12 => "summon_pos",
            13 => "relative_pos",
            14 => "source_pos",
            15 => "artillery_pos",
            16 => "others",
            17 => "friendly_others",
            18 => "cursor_pos",
            19 => "building",
            _ => "unknown",
        }
    }

    /// Whether the sweep can drive the skill's `SkillTargetType` at all: self/friendly/any-unit
    /// on the bot, hostile on the dummy, position at the dummy / in front, doodad on a spawned
    /// copy. Everything else (party/raid/line/item/building/...) is reported `unsupported`.
    pub fn drivable(&self) -> bool {
        matches!(self.target_type, 0 | 1 | 4 | 5 | 6 | 8)
    }

    /// Whether the cast target is the dummy (`Target::Obj` of the spawned NPC).
    pub fn targets_dummy(&self) -> bool {
        self.target_type == 4
            || self.target_relation == 4
            || (self.target_type == 6 && self.target_relation != 0)
    }

    /// The holdable `EquipmentItemSlot` indices the skill's `unit_reqs` demand.
    pub fn required_gear_slots(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for req in &self.reqs {
            match req.kind {
                8 => out.push(req.value1 as u8),
                21 => out.push(SLOT_OFFHAND),
                29 => out.push(if req.value1 == 1 {
                    SLOT_MUSICAL
                } else {
                    SLOT_RANGED
                }),
                _ => {}
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }
}

/// Read-only access to the skill tables of `compact.sqlite3`.
pub struct SkillDb {
    skills: HashMap<u32, SkillPlan>,
    path: PathBuf,
}

impl SkillDb {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn =
            rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .with_context(|| format!("opening compact database {}", path.display()))?;

        let mut skills: HashMap<u32, SkillPlan> = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT id, COALESCE(name,''), ability_id, COALESCE(ability_level,0), \
                 COALESCE(level_step,0), COALESCE(skill_points,0), \
                 CASE WHEN auto_learn='t' THEN 1 ELSE 0 END, \
                 CASE WHEN need_learn='t' THEN 1 ELSE 0 END, \
                 CASE WHEN SHOW='t' THEN 1 ELSE 0 END, \
                 COALESCE(target_type_id,0), COALESCE(target_relation_id,0), \
                 COALESCE(casting_time,0), COALESCE(channeling_time,0), \
                 COALESCE(channeling_tick,0), COALESCE(cooldown_time,0), \
                 COALESCE(mana_cost,0), COALESCE(mana_level_md,0.0), \
                 COALESCE(min_range,0), COALESCE(max_range,0), \
                 COALESCE(toggle_buff_id,0), COALESCE(effect_delay,0), \
                 COALESCE(effect_repeat_count,1), COALESCE(effect_repeat_tick,0), \
                 COALESCE(active_weapon_id,0), COALESCE(mainhand_tool_id,0), \
                 COALESCE(offhand_tool_id,0), \
                 CASE WHEN target_dead='t' THEN 1 ELSE 0 END, \
                 CASE WHEN or_unit_reqs='t' THEN 1 ELSE 0 END, \
                 COALESCE(plot_id,0) \
                 FROM skills WHERE ability_id BETWEEN 1 AND 10",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(SkillPlan {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    ability_id: r.get(2)?,
                    ability_level: r.get(3)?,
                    level_step: r.get(4)?,
                    skill_points: r.get(5)?,
                    auto_learn: r.get::<_, i64>(6)? != 0,
                    need_learn: r.get::<_, i64>(7)? != 0,
                    show: r.get::<_, i64>(8)? != 0,
                    target_type: r.get(9)?,
                    target_relation: r.get(10)?,
                    casting_time: r.get(11)?,
                    channeling_time: r.get(12)?,
                    channeling_tick: r.get(13)?,
                    cooldown_time: r.get(14)?,
                    mana_cost: r.get(15)?,
                    mana_level_md: r.get(16)?,
                    min_range: f64::from(r.get::<_, i32>(17)?),
                    max_range: r.get(18)?,
                    toggle_buff_id: r.get(19)?,
                    effect_delay: r.get(20)?,
                    effect_repeat_count: r.get(21)?,
                    effect_repeat_tick: r.get(22)?,
                    active_weapon_id: r.get(23)?,
                    mainhand_tool_id: r.get(24)?,
                    offhand_tool_id: r.get(25)?,
                    target_dead: r.get::<_, i64>(26)? != 0,
                    or_unit_reqs: r.get::<_, i64>(27)? != 0,
                    plot_id: r.get(28)?,
                    effects: EffectSummary::default(),
                    reqs: Vec::new(),
                })
            })?;
            for row in rows {
                let plan = row?;
                skills.insert(plan.id, plan);
            }
        }
        {
            let mut stmt = conn.prepare("SELECT id, buff_id FROM buff_effects")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, u32>(1)?)))?;
            let mut buffs: HashMap<u32, u32> = HashMap::new();
            for row in rows {
                let (id, buff_id) = row?;
                buffs.insert(id, buff_id);
            }
            let mut stmt = conn.prepare(
                "SELECT id, damage_type_id, fixed_min, fixed_max, multiplier, \
                 CASE WHEN use_mainhand_weapon='t' THEN 1 ELSE 0 END, \
                 CASE WHEN use_level_damage='t' THEN 1 ELSE 0 END, \
                 COALESCE(level_md,0.0), COALESCE(dps_multiplier,0.0), \
                 COALESCE(health_steal_ratio,0), COALESCE(mana_steal_ratio,0) \
                 FROM damage_effects",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, u32>(0)?,
                    DamageRow {
                        effect_id: 0,
                        damage_type: r.get(1)?,
                        fixed_min: r.get(2)?,
                        fixed_max: r.get(3)?,
                        multiplier: r.get(4)?,
                        use_mainhand_weapon: r.get::<_, i64>(5)? != 0,
                        use_level_damage: r.get::<_, i64>(6)? != 0,
                        level_md: r.get(7)?,
                        dps_multiplier: r.get(8)?,
                        health_steal_ratio: r.get(9)?,
                        mana_steal_ratio: r.get(10)?,
                    },
                ))
            })?;
            let mut damage: HashMap<u32, DamageRow> = HashMap::new();
            for row in rows {
                let (id, dmg) = row?;
                damage.insert(id, dmg);
            }
            let mut stmt = conn.prepare(
                "SELECT id, fixed_min, fixed_max, \
                 CASE WHEN use_level_heal='t' THEN 1 ELSE 0 END, COALESCE(level_md,0.0) \
                 FROM heal_effects",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, u32>(0)?,
                    HealRow {
                        effect_id: 0,
                        fixed_min: r.get(1)?,
                        fixed_max: r.get(2)?,
                        use_level_heal: r.get::<_, i64>(3)? != 0,
                        level_md: r.get(4)?,
                    },
                ))
            })?;
            let mut heal: HashMap<u32, HealRow> = HashMap::new();
            for row in rows {
                let (id, h) = row?;
                heal.insert(id, h);
            }

            let mut stmt = conn.prepare(
                "SELECT se.skill_id, se.effect_id, ef.actual_type, ef.actual_id, \
                 se.application_method_id \
                 FROM skill_effects se JOIN effects ef ON ef.id = se.effect_id \
                 WHERE se.skill_id IN (SELECT id FROM skills WHERE ability_id BETWEEN 1 AND 10)",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, u32>(0)?,
                    r.get::<_, u32>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, u32>(3)?,
                    r.get::<_, i32>(4)?,
                ))
            })?;
            for row in rows {
                let (skill_id, effect_id, actual_type, actual_id, method) = row?;
                let Some(plan) = skills.get_mut(&skill_id) else {
                    continue;
                };
                match actual_type.as_str() {
                    "BuffEffect" => {
                        let Some(buff_id) = buffs.get(&actual_id).copied() else {
                            continue;
                        };
                        if matches!(method, 2 | 3) {
                            plan.effects.buff_caster.push(buff_id);
                        } else {
                            plan.effects.buff_target.push(buff_id);
                        }
                    }
                    "DamageEffect" => {
                        if let Some(mut dmg) = damage.get(&actual_id).cloned() {
                            dmg.effect_id = effect_id;
                            plan.effects.damage_rows.push(dmg);
                        }
                    }
                    "HealEffect" => {
                        if let Some(mut h) = heal.get(&actual_id).cloned() {
                            h.effect_id = effect_id;
                            plan.effects.heal_rows.push(h);
                        }
                    }
                    other => {
                        *plan.effects.other.entry(other.to_string()).or_default() += 1;
                    }
                }
            }
        }
        {
            let mut stmt = conn.prepare(
                "SELECT owner_id, kind_id, value1, value2 FROM unit_reqs \
                 WHERE owner_type = 'Skill' \
                 AND owner_id IN (SELECT id FROM skills WHERE ability_id BETWEEN 1 AND 10)",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, u32>(0)?,
                    ReqRow {
                        kind: r.get(1)?,
                        value1: r.get(2)?,
                        value2: r.get(3)?,
                    },
                ))
            })?;
            for row in rows {
                let (skill_id, req) = row?;
                if let Some(plan) = skills.get_mut(&skill_id) {
                    plan.reqs.push(req);
                }
            }
        }
        Ok(Self {
            skills,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn skill(&self, id: u32) -> Option<&SkillPlan> {
        self.skills.get(&id)
    }

    /// Every skill in id order.
    pub fn all(&self) -> Vec<SkillPlan> {
        let mut plans: Vec<SkillPlan> = self.skills.values().cloned().collect();
        plans.sort_by_key(|p| p.id);
        plans
    }

    /// Every skill of one ability in id order.
    pub fn by_ability(&self, ability: u8) -> Vec<SkillPlan> {
        let mut plans: Vec<SkillPlan> = self
            .skills
            .values()
            .filter(|p| p.ability_id == ability)
            .cloned()
            .collect();
        plans.sort_by_key(|p| p.id);
        plans
    }
}

/// Opens `--compact-db` (or the default game-data path when it exists).
pub fn open_skill_db(compact_db: Option<&Path>) -> anyhow::Result<SkillDb> {
    if let Some(path) = compact_db {
        if !path.exists() {
            bail!("compact database {} not found", path.display());
        }
        return SkillDb::open(path);
    }
    for candidate in [DEFAULT_COMPACT_DB, DEFAULT_COMPACT_DB_FROM_RUST] {
        let path = Path::new(candidate);
        if path.exists() {
            return SkillDb::open(path);
        }
    }
    bail!(
        "no compact database found; pass --compact-db (tried {DEFAULT_COMPACT_DB}, \
         {DEFAULT_COMPACT_DB_FROM_RUST})"
    )
}

// -------------------------------------------------------------------------------------------
// Results
// -------------------------------------------------------------------------------------------

/// How one skill ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillOutcome {
    /// A cast ran and completed (fired/ended observed, no rejection).
    Ok,
    /// The server rejected the cast with a non-zero `SkillResult` (or an `SCErrorMsg`).
    Rejected,
    /// No cast response within the step timeout.
    Timeout,
    /// A target kind / data combination the runner cannot drive.
    Unsupported,
    /// Preparation (weapon, dummy, level, ...) failed.
    SetupFailed,
    /// Connection or protocol error.
    Error,
}

impl SkillOutcome {
    pub const ALL: [SkillOutcome; 6] = [
        SkillOutcome::Ok,
        SkillOutcome::Rejected,
        SkillOutcome::Timeout,
        SkillOutcome::Unsupported,
        SkillOutcome::SetupFailed,
        SkillOutcome::Error,
    ];

    pub fn name(self) -> &'static str {
        match self {
            SkillOutcome::Ok => "ok",
            SkillOutcome::Rejected => "rejected",
            SkillOutcome::Timeout => "timeout",
            SkillOutcome::Unsupported => "unsupported",
            SkillOutcome::SetupFailed => "setup_failed",
            SkillOutcome::Error => "error",
        }
    }
}

/// One `--out` JSONL line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillResult {
    pub skill_id: u32,
    pub name: String,
    pub ability_id: u8,
    pub character: String,
    pub result: SkillOutcome,
    /// Human-readable detail of `result`.
    pub reason: String,
    /// Anything the data says about a passive/toggle skill (toggle buff, effect types, flags).
    pub data_note: String,
    // ----- setup
    pub level_raised_to: u32,
    pub ability_swapped: bool,
    pub learned: bool,
    pub gear_required: Vec<u8>,
    pub gear_missing: Vec<u8>,
    pub target: String,
    // ----- data expectations
    pub target_type: u8,
    pub target_type_name: String,
    pub casting_time_ms: i32,
    pub channeling_time_ms: i32,
    pub channeling_tick_ms: i32,
    pub effect_delay_ms: i32,
    pub cooldown_time_ms: i32,
    pub mana_cost_expected: i32,
    pub expected_buffs: Vec<u32>,
    pub damage_rows: Vec<DamageRow>,
    pub heal_rows: Vec<HealRow>,
    /// Channel ticks the data schedules (`(channeling_time - 1) / channeling_tick`).
    pub ticks_expected: Option<i64>,
    // ----- observed
    /// Send -> first cast response (`SCSkillStarted` / `SCSkillFired`), i.e. the cast latency.
    pub started_after_ms: Option<u64>,
    pub fired_after_ms: Option<u64>,
    pub ended_after_ms: Option<u64>,
    /// `SCSkillFired` `computed_delay` (ms) and `channeling_time` (ms) off the wire.
    pub fired_delay_ms: Option<i32>,
    pub fired_channel_ms: Option<i32>,
    pub real_cast_time_ms: Option<u32>,
    pub base_cast_time_ms: Option<u32>,
    /// Non-zero `SkillResult` of the first cast.
    pub result_byte: Option<u8>,
    pub result_uint: u32,
    pub stopped: bool,
    pub server_errors: Vec<String>,
    pub mana_spent: Option<i32>,
    /// Target HP after - before (negative = damage, positive = heal).
    pub target_hp_delta: Option<i32>,
    pub buffs_created_caster: Vec<u32>,
    pub buffs_created_target: Vec<u32>,
    pub buffs_removed_caster: Vec<u32>,
    pub buffs_removed_target: Vec<u32>,
    pub plot_events: u32,
    pub unit_points_updates_caster: u32,
    pub unit_points_updates_target: u32,
    /// The immediate second cast's `SkillResult` (8 = cooldown as expected); 0 = accepted,
    /// `None` = no answer.
    pub recast_result: Option<u8>,
    pub messages: Vec<String>,
    pub mismatches: Vec<String>,
    pub elapsed_ms: u64,
}

/// The whole run's result.
#[derive(Debug, Default)]
pub struct RunReport {
    pub results: Vec<SkillResult>,
    /// The connection was lost; the run stopped after the last written line.
    pub aborted: bool,
}

/// CLI options of the `skill` command.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// `--skill <id>...` (repeatable).
    pub skill_ids: Vec<u32>,
    /// `--ability <id>...` (repeatable); default: every class ability.
    pub abilities: Vec<u8>,
    /// `--all`: every skill of the class abilities, in id order.
    pub all: bool,
    /// `--limit N`: cap the selected list.
    pub limit: Option<usize>,
    pub new_character: bool,
    pub race: u8,
    pub gender: u8,
    pub step_timeout: Duration,
    pub compact_db: Option<PathBuf>,
    pub out: Option<PathBuf>,
}

/// `--summary`: counts by result, the top mismatch reasons and the top failure reasons.
pub fn render_summary(report: &RunReport) -> String {
    let mut out = String::from("[skill] results:");
    for outcome in SkillOutcome::ALL {
        let n = report
            .results
            .iter()
            .filter(|r| r.result == outcome)
            .count();
        if n > 0 {
            out.push_str(&format!(" {}={n}", outcome.name()));
        }
    }
    let mut mismatches: BTreeMap<String, usize> = BTreeMap::new();
    for r in &report.results {
        for m in &r.mismatches {
            *mismatches.entry(reason_key(m)).or_default() += 1;
        }
    }
    let mut top: Vec<(&String, &usize)> = mismatches.iter().collect();
    top.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    if !top.is_empty() {
        out.push_str("\n[skill] top mismatch reasons:");
        for (reason, n) in top.iter().take(10) {
            out.push_str(&format!("\n  {n}x {reason}"));
        }
    }
    let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
    for r in &report.results {
        if r.result == SkillOutcome::Ok {
            continue;
        }
        *reasons.entry(reason_key(&r.reason)).or_default() += 1;
    }
    let mut top: Vec<(&String, &usize)> = reasons.iter().collect();
    top.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    if !top.is_empty() {
        out.push_str("\n[skill] top failure reasons:");
        for (reason, n) in top.iter().take(8) {
            out.push_str(&format!("\n  {n}x {reason}"));
        }
    }
    out
}

/// Collapses digit runs so per-skill reasons (`skill 63`, `skill 61`) count together.
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

/// `SkillResult` name (`SkillResultHelper.SkillResultErrorKeyToIdFor_r208022` inverted).
pub fn skill_result_name(code: u8) -> &'static str {
    match code {
        0 => "success",
        1 => "skill_failure",
        2 => "skill_source_died",
        3 => "skill_source_alive",
        4 => "skill_target_died",
        5 => "skill_target_destroyed",
        6 => "skill_target_alive",
        7 => "skill_on_casting",
        8 => "skill_cooldown_time",
        9 => "skill_no_target",
        10 => "skill_lack_health",
        11 => "skill_lack_mana",
        12 => "skill_obstacle",
        13 => "skill_outof_height",
        14 => "skill_too_close_range",
        15 => "skill_too_far_range",
        16 => "skill_outof_angle",
        17 => "skill_cannot_cast_in_combat",
        18 => "skill_cannot_cast_while_moving",
        19 => "skill_cannot_cast_in_stun",
        20 => "skill_cannot_cast_while_walking",
        21 => "skill_cannot_cast_in_swimming",
        22 => "skill_blank_minded",
        23 => "skill_silence",
        24 => "skill_crippled",
        25 => "skill_cannot_cast_in_channeling",
        26 => "skill_cannot_cast_in_prison",
        27 => "skill_need_stealth",
        28 => "skill_need_nocombat_target",
        29 => "skill_target_immune",
        30 => "skill_invalid_skill",
        31 => "skill_inactive_ability",
        32 => "skill_not_enough_ability_level",
        33 => "skill_invalid_source",
        34 => "skill_invalid_target",
        35 => "skill_invalid_location",
        36 => "skill_need_reagent",
        37 => "skill_item_locked",
        38 => "skill_need_money",
        39 => "skill_need_labor_power",
        40 => "skill_source_is_hanging",
        41 => "skill_source_is_riding",
        42 => "skill_higher_buff",
        43 => "skill_not_pvp_area",
        44 => "skill_not_now",
        45 => "skill_no_perm",
        46 => "skill_bag_full",
        47 => "skill_protected_faction",
        48 => "skill_protected_level",
        49 => "skill_unit_reqs_or_fail",
        51 => "backpack_occupied",
        52 => "skill_obstacle_for_spawn_doodad",
        53 => "skill_cannot_spawn_doodad_in_house",
        54 => "skill_cannot_use_for_self",
        55 => "skill_not_preoccupied",
        56 => "skill_not_my_npc",
        57 => "skill_not_checked_second_pass",
        58 => "SKILL_CANNOT_USE_HERE",
        59 => "skill_invalid_grade_enchant_support_item",
        60 => "skill_check_character_p_stat_min",
        61 => "skill_check_character_p_stat_max",
        62 => "skill_item_secured",
        63 => "skill_invalid_account_attribute",
        64 => "skill_urk_level",
        65 => "skill_urk_ability",
        66 => "skill_urk_race",
        67 => "skill_urk_gender",
        68 => "skill_urk_equip_slot",
        69 => "skill_urk_equip_item",
        70 => "skill_urk_own_item",
        71 => "skill_urk_trained_skill",
        72 => "skill_urk_combat",
        73 => "skill_urk_stealth",
        74 => "skill_urk_health",
        75 => "skill_urk_buff",
        76 => "skill_urk_target_buff",
        77 => "skill_urk_target_combat",
        78 => "skill_urk_can_learn_craft",
        79 => "skill_urk_doodad_range",
        80 => "skill_urk_equip_shield",
        81 => "skill_urk_nobuff",
        82 => "skill_urk_target_buff_tag",
        83 => "skill_urk_corpse_range",
        84 => "skill_urk_equip_weapon_type",
        85 => "skill_urk_target_health_less_than",
        86 => "skill_urk_target_npc",
        87 => "skill_urk_target_doodad",
        88 => "skill_urk_equip_ranged",
        89 => "skill_urk_no_buff_tag",
        90 => "skill_urk_complete_quest_context",
        91 => "skill_urk_progress_quest_context",
        92 => "skill_urk_ready_quest_context",
        93 => "skill_urk_target_npc_group",
        94 => "skill_urk_area_sphere",
        95 => "skill_urk_except_complete_quest_context",
        96 => "skill_urk_precomplete_quest_context",
        97 => "skill_urk_target_owner_type",
        98 => "skill_urk_not_under_water",
        99 => "skill_urk_faction_match",
        100 => "skill_urk_tod",
        101 => "skill_urk_mother_faction",
        102 => "skill_urk_actability_point",
        103 => "skill_urk_crime_point",
        104 => "skill_urk_honor_point",
        105 => "skill_urk_living_point",
        106 => "skill_urk_crime_record",
        107 => "skill_urk_jury_point",
        108 => "skill_urk_source_owner_type",
        109 => "skill_urk_appelation",
        110 => "skill_urk_in_zone",
        111 => "skill_urk_out_zone",
        112 => "skill_urk_dominion_owner",
        113 => "skill_urk_verdict_only",
        114 => "skill_urk_faction_match_only",
        115 => "skill_urk_mother_faction_only",
        116 => "skill_urk_nation_owner",
        117 => "skill_urk_faction_match_only_not",
        118 => "skill_urk_mother_faction_only_not",
        119 => "skill_urk_nation_member",
        120 => "skill_urk_nation_member_not",
        121 => "skill_urk_nation_owner_at_pos",
        122 => "skill_urk_dominion_owner_at_pos",
        123 => "skill_urk_housing",
        124 => "skill_urk_health_margin",
        125 => "skill_urk_mana_margin",
        126 => "skill_urk_labor_power_margin",
        127 => "skill_urk_unknown",
        128 => "skill_urk_max_level",
        129 => "skill_urk_expedition_owner",
        130 => "skill_urk_expedition_member",
        _ => "unknown",
    }
}

/// `Skill.ManaCost(caster)` (`systems/skills/skill.rs`): base scales with the ability level,
/// `mana_level_md` and the fixed `mana_cost`; modifiers are not applied to the expectation.
pub fn mana_cost_expected(plan: &SkillPlan, ab_level: i32) -> i32 {
    let base = (f64::from(ab_level - 1) * 1.6 + 8.0) * 3.0 / 3.65;
    (base * plan.mana_level_md + f64::from(plan.mana_cost)) as i32
}

// -------------------------------------------------------------------------------------------
// Packet readers
// -------------------------------------------------------------------------------------------

/// The parts of `SCSkillStartedPacket` the sweep needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SkillStarted {
    skill_id: u32,
    tl: u16,
    caster_obj: u32,
    result: u8,
    result_uint: u32,
    real_cast_ms: u32,
    base_cast_ms: u32,
}

fn parse_skill_started(p: &Packet) -> Option<SkillStarted> {
    if !p.is_sc(sc::SCSkillStartedPacket) {
        return None;
    }
    let mut s = p.stream();
    let skill_id = s.read_u32().ok()?;
    let tl = s.read_u16().ok()?;
    let caster_kind = s.read_u8().ok()?;
    let caster = SkillCaster::read(caster_kind, &mut s).ok()?;
    let target_kind = s.read_u8().ok()?;
    let _target = SkillCastTarget::read(target_kind, &mut s).ok()?;
    let object_kind = s.read_u8().ok()?;
    let _object = SkillObject::read(object_kind, &mut s).ok()?;
    let real_cast_ms = u32::from(s.read_u16().ok()?) * 10;
    let base_cast_ms = u32::from(s.read_u16().ok()?) * 10;
    let _synergy = s.read_u8().ok()?;
    let flags = s.read_u8().ok()?;
    let mut result = 0u8;
    let mut result_uint = 0u32;
    if flags & 1 != 0 {
        result = s.read_u8().ok()?;
    }
    if flags & 2 != 0 {
        let _ = s.read_u16().ok()?;
    }
    if flags & 4 != 0 {
        result_uint = s.read_u32().ok()?;
    }
    Some(SkillStarted {
        skill_id,
        tl,
        caster_obj: caster.obj_id(),
        result,
        result_uint,
        real_cast_ms,
        base_cast_ms,
    })
}

/// The parts of `SCSkillFiredPacket` the sweep needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SkillFired {
    skill_id: u32,
    tl: u16,
    caster_obj: u32,
    /// `i16 (computed_delay / 10 + 10)` off the wire, decoded back to ms.
    delay_ms: i32,
    /// `i16 (channeling_time / 10 + 10)` off the wire, decoded back to ms.
    channel_ms: i32,
}

fn parse_skill_fired(p: &Packet) -> Option<SkillFired> {
    if !p.is_sc(sc::SCSkillFiredPacket) {
        return None;
    }
    let mut s = p.stream();
    let skill_id = s.read_u32().ok()?;
    let tl = s.read_u16().ok()?;
    let caster_kind = s.read_u8().ok()?;
    let caster = SkillCaster::read(caster_kind, &mut s).ok()?;
    let target_kind = s.read_u8().ok()?;
    let _target = SkillCastTarget::read(target_kind, &mut s).ok()?;
    let object_kind = s.read_u8().ok()?;
    let _object = SkillObject::read(object_kind, &mut s).ok()?;
    let delay_ms = (i32::from(s.read_i16().ok()?) - 10) * 10;
    let channel_ms = (i32::from(s.read_i16().ok()?) - 10) * 10;
    s.read_u8().ok()?; // f
    let _anim = s.read_u32().ok()?;
    s.read_u8().ok()?; // flag
    Some(SkillFired {
        skill_id,
        tl,
        caster_obj: caster.obj_id(),
        delay_ms,
        channel_ms,
    })
}

fn parse_skill_ended(p: &Packet) -> Option<u16> {
    if !p.is_sc(sc::SCSkillEndedPacket) {
        return None;
    }
    p.stream().read_u16().ok()
}

/// `SCSkillStoppedPacket` -> `(obj_id, skill_id)`.
fn parse_skill_stopped(p: &Packet) -> Option<(u32, u32)> {
    if !p.is_sc(sc::SCSkillStoppedPacket) {
        return None;
    }
    let mut s = p.stream();
    Some((s.read_bc().ok()?, s.read_u32().ok()?))
}

/// `SCPlotEventPacket` -> `(tl, skill_id)`: plot skills (`skills.plot_id`) announce their steps
/// with it instead of `SCSkillStarted` / `SCSkillFired`.
fn parse_plot_event(p: &Packet) -> Option<(u16, u32)> {
    let mut s;
    if p.level == crate::game::LEVEL_COMPRESSED {
        // The server batches plot events into `CompressedGamePackets`; the inflated payload is
        // `[u16 0][u16 opcode][body]` per packet without lengths, so only the first inner
        // packet can be read without knowing every body size.
        if p.body.len() < 4 || p.body[..2] != [0, 0] {
            return None;
        }
        if u16::from_le_bytes([p.body[2], p.body[3]]) != sc::SCPlotEventPacket {
            return None;
        }
        s = aaemu_commons::PacketStream::from_bytes(p.body[4..].to_vec());
    } else {
        if !p.is_sc(sc::SCPlotEventPacket) {
            return None;
        }
        s = p.stream();
    }
    let tl = s.read_u16().ok()?;
    let _event = s.read_u32().ok()?;
    Some((tl, s.read_u32().ok()?))
}

/// `SCPlotEndedPacket` -> `tl`.
fn parse_plot_ended(p: &Packet) -> Option<u16> {
    if !p.is_sc(sc::SCPlotEndedPacket) {
        return None;
    }
    p.stream().read_u16().ok()
}

/// `SCPlotCastingStoppedPacket` / `SCPlotChannelingStoppedPacket` -> `tl`.
fn parse_plot_stopped(p: &Packet) -> Option<u16> {
    if !p.is_sc(sc::SCPlotCastingStoppedPacket) && !p.is_sc(sc::SCPlotChannelingStoppedPacket) {
        return None;
    }
    p.stream().read_u16().ok()
}

/// `SCCastingStoppedPacket` -> `tl`.
fn parse_casting_stopped(p: &Packet) -> Option<u16> {
    if !p.is_sc(sc::SCCastingStoppedPacket) {
        return None;
    }
    p.stream().read_u16().ok()
}

/// `SCBuffCreatedPacket` -> `(owner obj id, index, buff id)`.
fn parse_buff_created(p: &Packet) -> Option<(u32, u32, u32)> {
    if !p.is_sc(sc::SCBuffCreatedPacket) {
        return None;
    }
    let mut s = p.stream();
    let kind = s.read_u8().ok()?;
    SkillCaster::read(kind, &mut s).ok()?;
    s.read_u32().ok()?; // caster id
    let owner = s.read_bc().ok()?;
    let index = s.read_u32().ok()?;
    let buff_id = s.read_u32().ok()?;
    Some((owner, index, buff_id))
}

/// `SCBuffRemovedPacket` -> `(owner obj id, index)`.
fn parse_buff_removed(p: &Packet) -> Option<(u32, u32)> {
    if !p.is_sc(sc::SCBuffRemovedPacket) {
        return None;
    }
    let mut s = p.stream();
    Some((s.read_bc().ok()?, s.read_u32().ok()?))
}

/// `SCUnitPointsPacket` -> `(obj id, hp, mp)`.
fn parse_unit_points(p: &Packet) -> Option<(u32, i32, i32)> {
    if !p.is_sc(sc::SCUnitPointsPacket) {
        return None;
    }
    let mut s = p.stream();
    let obj = s.read_bc().ok()?;
    Some((obj, s.read_i32().ok()? / 100, s.read_i32().ok()? / 100))
}

/// `SCErrorMsgPacket.error1` / `error2`.
fn parse_error(p: &Packet) -> Option<(i16, i16)> {
    if !p.is_sc(sc::SCErrorMsgPacket) {
        return None;
    }
    let mut s = p.stream();
    Some((s.read_i16().ok()?, s.read_i16().ok()?))
}

// -------------------------------------------------------------------------------------------
// Session state and setup
// -------------------------------------------------------------------------------------------

/// What the runner tracks from the select snapshot and the live packets.
#[derive(Debug, Default)]
struct SkillSession {
    /// Equipment items by `EquipmentItemSlot` index.
    equipment: HashMap<u8, EquippedItem>,
    /// `SCUnitPointsPacket` count per unit (monotonic; lets a command wait for a refill).
    points_count: HashMap<u32, u32>,
    /// Last `SCSkillLearnedPacket`.
    learned: Option<(u32, u8)>,
    /// An `SCAbilitySwappedPacket` arrived.
    ability_swapped: bool,
    /// Last `SCUnitDeathPacket` object.
    death: u32,
}

impl SkillSession {
    fn observe(&mut self, p: &Packet) {
        if let Some((obj, _, _)) = parse_unit_points(p) {
            *self.points_count.entry(obj).or_insert(0) += 1;
        }
        if p.is_sc(sc::SCItemTaskSuccessPacket) {
            if let Ok(changes) = equipment_tasks(&p.body) {
                for slot in changes.removed_slots {
                    self.equipment.remove(&slot);
                }
                for row in changes.created {
                    self.equipment.insert(row.slot, row);
                }
            }
        }
        if p.is_sc(sc::SCSkillLearnedPacket) {
            let mut s = p.stream();
            if let (Ok(skill_id), Ok(level)) = (s.read_u32(), s.read_u8()) {
                self.learned = Some((skill_id, level));
            }
        }
        if p.is_sc(sc::SCAbilitySwappedPacket) {
            self.ability_swapped = true;
        }
        if p.is_sc(sc::SCUnitDeathPacket) {
            if let Ok(obj) = p.stream().read_bc() {
                self.death = obj;
            }
        }
    }

    fn points_seen(&self, obj: u32) -> u32 {
        self.points_count.get(&obj).copied().unwrap_or(0)
    }
}

/// The live facts of the sweep.
#[derive(Debug)]
struct SweepState {
    /// Object id of the training dummy (0 = none spawned yet).
    dummy_obj: u32,
    /// Object id of the doodad spawned for a doodad-target skill (0 = none).
    doodad_obj: u32,
    /// ability1..3 as the client knows them (`SCCharacterStatePacket` + our swaps).
    abilities: [u8; 3],
    /// Learned skills per ability; a swap resets the swapped-out ability's list.
    learned: HashMap<u8, HashSet<u32>>,
    raised_level: u32,
}

impl Default for SweepState {
    fn default() -> Self {
        Self {
            dummy_obj: 0,
            doodad_obj: 0,
            abilities: [ABILITY_NONE, ABILITY_NONE, ABILITY_NONE],
            learned: HashMap::new(),
            raised_level: 1,
        }
    }
}

/// Sends `CSCreateCharacterPacket` and waits for the answer.
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
        bail!("character creation failed (reason {reason}, name {name:?})");
    }
    Ok(())
}

/// Login, lobby burst, character create / select, spawn and notify; decodes the select
/// snapshot's equipment and active abilities (mirrors the craft runner's setup).
async fn setup_world(
    cfg: &BotConfig,
    rec: &Recorder,
    opts: &RunOptions,
) -> anyhow::Result<(GameClient, RunCharacter, SkillSession, [u8; 3])> {
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
        p.is(
            crate::game::LEVEL_PROXY,
            aaemu_game::opcodes::pp::ChangeStatePacket,
        )
    })
    .await?;
    for state in 0..=7 {
        game.finish_state(state).await?;
        if state < 7 {
            game.wait_for("ChangeStatePacket", |p| {
                p.is(
                    crate::game::LEVEL_PROXY,
                    aaemu_game::opcodes::pp::ChangeStatePacket,
                )
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
    let mut created: Option<RunCharacter> = None;
    if opts.new_character {
        if listed >= crate::questrun::MAX_CHARACTERS_PER_ACCOUNT {
            bail!(
                "the account already has {listed} characters; the login server allows \
                 {} per account (MaxCharactersPerAccount)",
                crate::questrun::MAX_CHARACTERS_PER_ACCOUNT
            );
        }
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let name = crate::questrun::generated_character_name(&cfg.character, millis, 0)?;
        create_and_wait(&mut game, &name, opts.race, opts.gender).await?;
        created = Some(RunCharacter {
            name,
            race: opts.race,
            gender: opts.gender,
            start_level: 1,
        });
    } else if listed == 0 {
        if !crate::questrun::valid_character_name(&cfg.character) {
            bail!(
                "config character '{}' does not match the server's CharacterNameRegex",
                cfg.character
            );
        }
        create_and_wait(&mut game, &cfg.character, opts.race, opts.gender).await?;
        created = Some(RunCharacter {
            name: cfg.character.clone(),
            race: opts.race,
            gender: opts.gender,
            start_level: 1,
        });
    }

    let target = match &created {
        Some(c) => game
            .state
            .characters
            .iter()
            .find(|x| x.name == c.name)
            .cloned()
            .with_context(|| format!("created character {:?} is not in the list", c.name))?,
        None => game
            .state
            .characters
            .first()
            .cloned()
            .context("the account has no character to select")?,
    };
    let mut character = created.unwrap_or_else(|| RunCharacter {
        name: target.name.clone(),
        race: target.race,
        gender: target.gender,
        start_level: target.level,
    });
    game.select_character(target.id).await?;
    let state_packet = game.wait_sc(sc::SCCharacterStatePacket).await?;
    let mut select_packets = vec![state_packet];
    select_packets.extend(
        game.drain(Duration::from_millis(1500), Duration::from_secs(10))
            .await?,
    );
    game.spawn_character().await?;
    game.wait_for("own SCUnitStatePacket", |p| p.is_sc(sc::SCUnitStatePacket))
        .await?;
    game.notify_in_game().await?;
    game.notify_in_game_completed().await?;
    let spawn_packets = game
        .drain(Duration::from_millis(1500), Duration::from_secs(15))
        .await?;
    anyhow::ensure!(game.state.self_obj != 0, "own unit state not received");

    let mut session = SkillSession::default();
    let mut abilities = [ABILITY_NONE; 3];
    for p in select_packets.iter().chain(spawn_packets.iter()) {
        session.observe(p);
        if p.is_sc(sc::SCCharacterStatePacket) {
            if let Ok((_, equipment, found)) = crate::craftrun::parse_state_equipment(&p.body) {
                abilities = found;
                for item in equipment {
                    session.equipment.insert(item.slot, item);
                }
            }
        }
    }
    if let Some(info) = game.state.self_info.clone() {
        character = RunCharacter {
            name: info.name,
            race: info.race,
            gender: info.gender,
            start_level: info.level,
        };
    }
    Ok((game, character, session, abilities))
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

/// A condition a GM command waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Done {
    /// The command's reply duration is the whole wait.
    Never,
    /// The bag holds at least `count` of `template` (`/item add`).
    BagCount(u32, i32),
    /// A unit of `template` is in view and not dead (`/spawn npc`).
    LiveUnitAppears(u32),
    /// The unit is dead (`/kill`).
    UnitDead(u32),
    /// A doodad of `template` is in view (`/spawn doodad`).
    DoodadAppears(u32),
    /// The doodad object is gone (`/spawn remove`).
    DoodadGone(u32),
    /// `n` more `SCUnitPointsPacket`s arrived for the unit than at command start (`/heal`).
    PointsAdvanced(u32, u32),
}

// -------------------------------------------------------------------------------------------
// The sweep
// -------------------------------------------------------------------------------------------

/// The runner with its live connection and state.
struct Sweep<'a> {
    game: GameClient,
    session: SkillSession,
    state: SweepState,
    character: RunCharacter,
    opts: &'a RunOptions,
}

impl<'a> Sweep<'a> {
    /// Whether `done` holds over the current state.
    fn done(&self, done: Done) -> bool {
        match done {
            Done::Never => false,
            Done::BagCount(template, count) => self.game.state.bag_count(&[template]) >= count,
            Done::LiveUnitAppears(template) => self.game.state.units.values().any(|u| {
                u.unit_type == 1
                    && u.id == template
                    && !self.game.state.dead_units.contains(&u.obj_id)
                    && self
                        .game
                        .state
                        .unit_points
                        .get(&u.obj_id)
                        .is_none_or(|(hp, _)| *hp > 0)
            }),
            Done::UnitDead(obj) => {
                self.game.state.dead_units.contains(&obj)
                    || self
                        .game
                        .state
                        .unit_points
                        .get(&obj)
                        .is_some_and(|(hp, _)| *hp <= 0)
            }
            Done::DoodadAppears(template) => self.game.state.find_doodad(template).is_some(),
            Done::DoodadGone(obj) => !self.game.state.doodads.contains_key(&obj),
            Done::PointsAdvanced(obj, before) => self.session.points_seen(obj) > before,
        }
    }

    /// Sends a `/command` chat line, pumps packets through the session facts until `done` has
    /// held for [`COMMAND_QUIET`] or `reply_wait` elapsed; returns the chat texts.
    async fn cmd(
        &mut self,
        command: &str,
        reply_wait: Duration,
        done: Done,
    ) -> anyhow::Result<Vec<String>> {
        let text = format!(
            "{}{}",
            aaemu_game::systems::services::commands::COMMAND_PREFIX,
            command
        );
        self.game
            .chat(aaemu_game::systems::social::chat::chat_type::WHITE, &text)
            .await?;
        let started = Instant::now();
        let deadline = started + reply_wait;
        let mut messages = Vec::new();
        let mut done_since: Option<Instant> = None;
        loop {
            if Instant::now() >= deadline {
                break;
            }
            if done_since.is_some_and(|t| t.elapsed() >= COMMAND_QUIET) {
                break;
            }
            if let Some(p) = self.game.recv_within(PUMP).await? {
                self.session.observe(&p);
                if let Some(t) = chat_text(&p) {
                    messages.push(t);
                }
                if done_since.is_none() && self.done(done) {
                    done_since = Some(Instant::now());
                }
            }
        }
        tracing::debug!(
            "/{command}: {} ms, {} chat",
            started.elapsed().as_millis(),
            messages.len()
        );
        Ok(messages)
    }

    /// Moves the bot to `distance` metres from `target`, facing it (`CSMoveUnitPacket`).
    async fn face_at_distance(
        &mut self,
        target: (f32, f32, f32),
        distance: f64,
    ) -> anyhow::Result<()> {
        let me = self.game.state.self_pos;
        let (dx, dy) = (f64::from(target.0 - me.0), f64::from(target.1 - me.1));
        let len = (dx * dx + dy * dy).sqrt();
        if len < 0.01 {
            return Ok(());
        }
        let pos = (
            (f64::from(target.0) - dx / len * distance) as f32,
            (f64::from(target.1) - dy / len * distance) as f32,
            me.2,
        );
        self.game.move_to_facing(pos, target, 600).await
    }

    /// Distance for damage distance checks. No target (self/AoE around the source) keeps the
    /// spawn distance; otherwise a value inside `skills.min_range..max_range`.
    fn desired_distance(plan: &SkillPlan) -> f64 {
        if plan.max_range <= 0.0 {
            return 3.0;
        }
        let min = plan.min_range.max(0.0);
        (plan.max_range - 0.5)
            .min((min + 1.0).max(3.0))
            .max(min + 0.5)
    }

    /// Spawns (or re-uses) the training dummy in front of the bot and returns its object id.
    async fn ensure_dummy(&mut self) -> anyhow::Result<Result<u32, String>> {
        let alive = |obj: u32, game: &GameClient| {
            obj != 0
                && !game.state.dead_units.contains(&obj)
                && game
                    .state
                    .unit_points
                    .get(&obj)
                    .is_none_or(|(hp, _)| *hp > 0)
        };
        if alive(self.state.dummy_obj, &self.game) {
            return Ok(Ok(self.state.dummy_obj));
        }
        if self.state.dummy_obj != 0 {
            let old = self.state.dummy_obj;
            let _ = self
                .cmd(&format!("despawn npc {old}"), GM_EFFECT_WAIT, Done::Never)
                .await;
            self.state.dummy_obj = 0;
        }
        let _ = self
            .cmd(
                &format!("spawn npc {DUMMY_TEMPLATE}"),
                GM_EFFECT_WAIT,
                Done::LiveUnitAppears(DUMMY_TEMPLATE),
            )
            .await?;
        let Some(unit) = self
            .game
            .state
            .units
            .values()
            .filter(|u| u.unit_type == 1 && u.id == DUMMY_TEMPLATE)
            .max_by(|a, b| {
                let me = self.game.state.self_pos;
                let da = (a.pos.0 - me.0).powi(2) + (a.pos.1 - me.1).powi(2);
                let db = (b.pos.0 - me.0).powi(2) + (b.pos.1 - me.1).powi(2);
                db.total_cmp(&da)
            })
            .cloned()
        else {
            return Ok(Err(format!(
                "/spawn npc {DUMMY_TEMPLATE} did not announce a unit"
            )));
        };
        self.state.dummy_obj = unit.obj_id;
        // The dummy spawns 3 m in front of the bot and fights nothing back.
        Ok(Ok(unit.obj_id))
    }

    /// Damages the dummy to death with `/kill` (the current target), for skills that require a
    /// dead target.
    async fn kill_dummy(&mut self, obj: u32) -> anyhow::Result<bool> {
        self.game.change_target(obj).await?;
        let _ = self.cmd("kill", GM_EFFECT_WAIT, Done::UnitDead(obj)).await;
        Ok(self.game.state.dead_units.contains(&obj))
    }

    /// Spawns a doodad for doodad-target skills; returns its object id.
    async fn ensure_doodad(&mut self) -> anyhow::Result<Result<u32, String>> {
        if self.state.doodad_obj != 0
            && self.game.state.doodads.contains_key(&self.state.doodad_obj)
        {
            return Ok(Ok(self.state.doodad_obj));
        }
        if let Some(found) = self.game.state.find_doodad(DOODAD_TEMPLATE) {
            self.state.doodad_obj = found.obj_id;
            return Ok(Ok(found.obj_id));
        }
        let _ = self
            .cmd(
                &format!("spawn doodad {DOODAD_TEMPLATE}"),
                GM_EFFECT_WAIT,
                Done::DoodadAppears(DOODAD_TEMPLATE),
            )
            .await?;
        let Some(doodad) = self.game.state.find_doodad(DOODAD_TEMPLATE) else {
            return Ok(Err(format!(
                "/spawn doodad {DOODAD_TEMPLATE} did not announce a doodad"
            )));
        };
        self.state.doodad_obj = doodad.obj_id;
        Ok(Ok(doodad.obj_id))
    }

    /// Removes the sweep's own doodad.
    async fn remove_doodad(&mut self) {
        let obj = self.state.doodad_obj;
        if obj == 0 {
            return;
        }
        let _ = self
            .cmd(
                &format!("spawn remove {obj}"),
                GM_EFFECT_WAIT,
                Done::DoodadGone(obj),
            )
            .await;
        self.state.doodad_obj = 0;
    }

    /// Raises the level (and the active abilities' exp) to `TARGET_LEVEL`. `force` re-applies it
    /// after an ability swap: the swapped-in ability's exp came from the DB, not from the level
    /// command, so it needs the top-up again.
    async fn raise_level(&mut self, force: bool) -> anyhow::Result<()> {
        if self.state.raised_level >= u32::from(TARGET_LEVEL) && !force {
            return Ok(());
        }
        let _ = self
            .cmd(
                &format!("level {TARGET_LEVEL}"),
                Duration::from_secs(3),
                Done::Never,
            )
            .await?;
        self.state.raised_level = u32::from(TARGET_LEVEL);
        Ok(())
    }

    /// Makes `ability` one of `ability1..3` with the client's `CSSwapAbilityPacket`; a swap
    /// resets the swapped-out ability's learned skills (which the state mirrors).
    async fn ensure_ability(&mut self, ability: u8) -> anyhow::Result<bool> {
        if self.state.abilities.contains(&ability) {
            return Ok(false);
        }
        let old = self.state.abilities[1];
        self.session.ability_swapped = false;
        self.game.swap_ability(old, ability).await?;
        for _ in 0..12 {
            let Some(p) = self.game.recv_within(PUMP).await? else {
                continue;
            };
            self.session.observe(&p);
            if self.session.ability_swapped {
                break;
            }
        }
        anyhow::ensure!(
            self.session.ability_swapped,
            "the server did not answer CSSwapAbilityPacket(old {old}, new {ability})"
        );
        // The server replaces the first slot holding `old` (ability1 first, then ability2).
        let slot = if self.state.abilities[0] == old { 0 } else { 1 };
        self.state.abilities[slot] = ability;
        self.state.learned.remove(&old);
        self.raise_level(true).await?;
        Ok(true)
    }

    /// Learns the skill with `CSLearnSkillPacket`; `false` when the server stays silent
    /// (inactive ability / not enough skill points — the cast runs anyway through the
    /// handler's template fallback).
    async fn ensure_learned(&mut self, skill_id: u32, ability: u8) -> anyhow::Result<bool> {
        if self
            .state
            .learned
            .get(&ability)
            .is_some_and(|set| set.contains(&skill_id))
        {
            return Ok(true);
        }
        self.session.learned = None;
        self.game.learn_skill(skill_id).await?;
        let deadline = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < deadline {
            let Some(p) = self.game.recv_within(PUMP).await? else {
                continue;
            };
            self.session.observe(&p);
            if matches!(self.session.learned, Some((id, _)) if id == skill_id) {
                self.state
                    .learned
                    .entry(ability)
                    .or_default()
                    .insert(skill_id);
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Equips the four holdable slots the data can demand (`/item add self` + the client's
    /// `CSSwapItemsPacket` into the equipment container).
    async fn ensure_gear(&mut self) -> anyhow::Result<()> {
        for (template, slot) in GEAR {
            if self
                .session
                .equipment
                .get(&slot)
                .is_some_and(|item| item.template_id == template)
            {
                continue;
            }
            if self.game.state.bag_count(&[template]) == 0 {
                let _ = self
                    .cmd(
                        &format!("item add self {template} 1"),
                        GM_EFFECT_WAIT,
                        Done::BagCount(template, 1),
                    )
                    .await?;
            }
            let Some((bag_slot, item_id)) = self
                .game
                .state
                .bag
                .iter()
                .find(|(_, item)| item.template_id == template)
                .map(|(slot, item)| (*slot, item.item_id))
            else {
                bail!(
                    "the GM give did not create item {template} (equipment slot {slot} stays empty)"
                );
            };
            self.game.equip_item(item_id, bag_slot, slot).await?;
            let deadline = Instant::now() + GM_EFFECT_WAIT;
            while Instant::now() < deadline {
                let Some(p) = self.game.recv_within(PUMP).await? else {
                    continue;
                };
                self.session.observe(&p);
                if self
                    .session
                    .equipment
                    .get(&slot)
                    .is_some_and(|item| item.template_id == template)
                {
                    break;
                }
            }
            anyhow::ensure!(
                self.session
                    .equipment
                    .get(&slot)
                    .is_some_and(|item| item.template_id == template),
                "item {template} was not equipped into slot {slot}"
            );
        }
        Ok(())
    }

    /// `/heal` on the bot: the current target is set to the bot first (the command refills the
    /// current target unit).
    async fn heal_self(&mut self) -> anyhow::Result<bool> {
        let me = self.game.state.self_obj;
        self.game.change_target(me).await?;
        let before = self.session.points_seen(me);
        let _ = self
            .cmd("heal", GM_EFFECT_WAIT, Done::PointsAdvanced(me, before))
            .await?;
        Ok(self.session.points_seen(me) > before)
    }

    /// `/heal` on the dummy (the current target is set to it first).
    async fn heal_dummy(&mut self, obj: u32) -> anyhow::Result<bool> {
        self.game.change_target(obj).await?;
        let before = self.session.points_seen(obj);
        let _ = self
            .cmd("heal", GM_EFFECT_WAIT, Done::PointsAdvanced(obj, before))
            .await?;
        Ok(self.session.points_seen(obj) > before)
    }

    /// `/resetcd`: clears the per-skill cooldowns (the next cast must not inherit them).
    /// Removes every buff `obj` currently carries with the `/buff -<id>` GM command (the
    /// command acts on the selected target).
    async fn clear_buffs(&mut self, obj: u32) -> anyhow::Result<()> {
        let ids: Vec<u32> = self
            .game
            .state
            .buffs
            .get(&obj)
            .map(|b| b.values().map(|i| i.buff_id).collect())
            .unwrap_or_default();
        if ids.is_empty() {
            return Ok(());
        }
        self.game.change_target(obj).await?;
        for id in ids {
            let _ = self
                .cmd(&format!("buff -{id}"), Duration::from_secs(1), Done::Never)
                .await?;
        }
        Ok(())
    }

    async fn reset_cooldowns(&mut self) -> anyhow::Result<()> {
        let _ = self.cmd("resetcd", RESETCD_WAIT, Done::Never).await?;
        Ok(())
    }

    /// Best-effort clean leave at the end of the run.
    async fn leave(&mut self) {
        leave(&mut self.game).await;
    }

    /// Sends `CSStartSkillPacket` and watches the cast until it settles.
    async fn execute_cast(
        &mut self,
        plan: &SkillPlan,
        target: SkillCastTarget,
        compare_obj: u32,
        timeout: Duration,
    ) -> anyhow::Result<CastAttempt> {
        let self_obj = self.game.state.self_obj;
        let mut attempt = CastAttempt::default();
        // The buff index -> id map as of now; `SCBuffRemovedPacket` only carries the index.
        let mut known: HashMap<(u32, u32), u32> = HashMap::new();
        for (owner, buffs) in &self.game.state.buffs {
            for (index, info) in buffs {
                known.insert((*owner, *index), info.buff_id);
            }
        }
        let sent_at = Instant::now();
        self.game
            .start_skill_as(plan.id, SkillCaster::Unit(self_obj), target)
            .await?;

        let deadline = sent_at + timeout;
        let mut our_tl: Option<u16> = None;
        let mut done_at: Option<Instant> = None;
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            // A cast the server drops silently never answers; do not wait out the step timeout.
            if our_tl.is_none()
                && !attempt.responded
                && now.duration_since(sent_at) >= SILENT_DROP_WAIT
            {
                break;
            }
            if done_at.is_some_and(|done| now.duration_since(done) >= SETTLE_QUIET) {
                break;
            }
            let wait = PUMP.min(deadline - now);
            let Some(p) = self.game.recv_within(wait).await? else {
                continue;
            };
            self.session.observe(&p);
            let mut signal = false;
            if let Some(started) = parse_skill_started(&p) {
                if started.skill_id == plan.id && started.caster_obj == self_obj {
                    our_tl = Some(started.tl);
                    attempt.started_ms = Some(sent_at.elapsed().as_millis() as u64);
                    attempt.real_cast_ms = Some(started.real_cast_ms);
                    attempt.base_cast_ms = Some(started.base_cast_ms);
                    if started.result != 0 {
                        attempt.result = Some(started.result);
                        attempt.result_uint = started.result_uint;
                        signal = true;
                    }
                }
            }
            if let Some(fired) = parse_skill_fired(&p) {
                if fired.skill_id == plan.id && fired.caster_obj == self_obj {
                    our_tl = Some(fired.tl);
                    attempt.fired_ms = Some(sent_at.elapsed().as_millis() as u64);
                    attempt.fired_delay_ms = Some(fired.delay_ms);
                    attempt.fired_channel_ms = Some(fired.channel_ms);
                    attempt.responded = true;
                }
            }
            if let Some((tl, skill_id)) = parse_plot_event(&p) {
                if skill_id == plan.id {
                    our_tl = Some(tl);
                    attempt.plot_events += 1;
                    attempt.responded = true;
                    if attempt.started_ms.is_none() {
                        attempt.started_ms = Some(sent_at.elapsed().as_millis() as u64);
                    }
                }
            }
            if let Some(tl) = parse_plot_ended(&p) {
                if Some(tl) == our_tl {
                    attempt.ended_ms = Some(sent_at.elapsed().as_millis() as u64);
                    signal = true;
                }
            }
            if let Some(tl) = parse_plot_stopped(&p) {
                if Some(tl) == our_tl {
                    attempt.casting_stopped = true;
                    signal = true;
                }
            }
            if let Some(tl) = parse_skill_ended(&p) {
                if Some(tl) == our_tl {
                    attempt.ended_ms = Some(sent_at.elapsed().as_millis() as u64);
                    signal = true;
                }
            }
            if let Some((obj, skill_id)) = parse_skill_stopped(&p) {
                if obj == self_obj && skill_id == plan.id {
                    attempt.stopped = true;
                    signal = true;
                }
            }
            if let Some(tl) = parse_casting_stopped(&p) {
                if Some(tl) == our_tl {
                    attempt.casting_stopped = true;
                    signal = true;
                }
            }
            if let Some((owner, index, buff_id)) = parse_buff_created(&p) {
                known.insert((owner, index), buff_id);
                attempt.buffs_created.push((owner, buff_id));
            }
            if let Some((owner, index)) = parse_buff_removed(&p) {
                if let Some(buff_id) = known.remove(&(owner, index)) {
                    attempt.buffs_removed.push((owner, buff_id));
                }
            }
            if let Some((obj, _, _)) = parse_unit_points(&p) {
                if obj == self_obj {
                    attempt.points_self += 1;
                }
                if obj == compare_obj {
                    attempt.points_compare += 1;
                }
            }
            if let Some((e1, e2)) = parse_error(&p) {
                attempt.errors.push((e1, e2));
                signal = true;
            }
            if signal {
                attempt.responded = true;
                if done_at.is_none() {
                    done_at = Some(Instant::now());
                }
            }
        }
        Ok(attempt)
    }

    /// Sends a second `CSStartSkillPacket` and returns the first `SkillResult` the server
    /// answers with (`Some(0)` = accepted). The caller waits out the GCD first.
    async fn probe_recast(
        &mut self,
        plan: &SkillPlan,
        target: SkillCastTarget,
        wait: Duration,
    ) -> anyhow::Result<Option<u8>> {
        let self_obj = self.game.state.self_obj;
        self.game
            .start_skill_as(plan.id, SkillCaster::Unit(self_obj), target)
            .await?;
        let deadline = Instant::now() + wait;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            let Some(p) = self.game.recv_within(PUMP.min(deadline - now)).await? else {
                continue;
            };
            self.session.observe(&p);
            if let Some(started) = parse_skill_started(&p) {
                if started.skill_id == plan.id && started.caster_obj == self_obj {
                    return Ok(Some(started.result));
                }
            }
            if let Some(fired) = parse_skill_fired(&p) {
                if fired.skill_id == plan.id && fired.caster_obj == self_obj {
                    return Ok(Some(0));
                }
            }
        }
    }

    /// Pauses while pumping packets (the GCD window before the cooldown probe).
    async fn pump_for(&mut self, duration: Duration) -> anyhow::Result<()> {
        let deadline = Instant::now() + duration;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            if left.is_zero() {
                break;
            }
            if let Some(p) = self.game.recv_within(PUMP.min(left)).await? {
                self.session.observe(&p);
            }
        }
        Ok(())
    }

    /// One skill: setup, cast, second-cast cooldown probe, data-vs-observed comparison.
    async fn run_one(&mut self, plan: &SkillPlan, unknown: bool) -> SkillResult {
        let start = Instant::now();
        let chat_start = self.game.state.chat_count;
        let mut out = self.new_result(plan, unknown);
        if unknown {
            out.result = SkillOutcome::Unsupported;
            out.reason = format!(
                "skill {} is not in the compact database (ability 1..10)",
                plan.id
            );
            out.elapsed_ms = start.elapsed().as_millis() as u64;
            return out;
        }
        if !plan.drivable() {
            out.result = SkillOutcome::Unsupported;
            out.reason = format!(
                "SkillTargetType {} ({}) is not drivable by the sweep",
                plan.target_type,
                plan.target_type_name()
            );
            out.elapsed_ms = start.elapsed().as_millis() as u64;
            return out;
        }

        if plan.target_dead && plan.target_relation != 4 && plan.target_type != 4 {
            // A friendly resurrect needs a dead friendly player; the sweep drives one bot and
            // its training dummy is hostile and despawns once dead.
            out.result = SkillOutcome::Unsupported;
            out.reason = "target_dead skill with a friendly target: needs a dead friendly player (the sweep has one character)".to_string();
            out.elapsed_ms = start.elapsed().as_millis() as u64;
            return out;
        }

        // Ability, level, learning.
        match self.ensure_ability(plan.ability_id).await {
            Ok(swapped) => out.ability_swapped = swapped,
            Err(e) => {
                out.result = SkillOutcome::SetupFailed;
                out.reason = format!("could not activate ability {}: {e:#}", plan.ability_id);
                self.finish(chat_start, start, &mut out);
                return out;
            }
        }
        let _ = self.ensure_learned(plan.id, plan.ability_id).await;
        out.learned = self
            .state
            .learned
            .get(&plan.ability_id)
            .is_some_and(|set| set.contains(&plan.id));

        // Gear.
        out.gear_required = plan.required_gear_slots();
        out.gear_missing = out
            .gear_required
            .iter()
            .filter(|slot| !self.session.equipment.contains_key(slot))
            .copied()
            .collect();
        if !out.gear_missing.is_empty() {
            out.result = SkillOutcome::SetupFailed;
            out.reason = format!(
                "required holdable slot(s) {:?} are empty (unit_reqs)",
                out.gear_missing
            );
            self.finish(chat_start, start, &mut out);
            return out;
        }

        // Target setup: doodad copy, training dummy (also for target_dead corpses) or self.
        let self_obj = self.game.state.self_obj;
        let mut compare_obj = self_obj;
        let mut target = SkillCastTarget::Unit(self_obj);
        let needs_dummy = plan.targets_dummy() || plan.target_dead;
        if plan.target_type == 8 {
            match self.ensure_doodad().await {
                Ok(Ok(obj)) => {
                    target = SkillCastTarget::Doodad(obj);
                    out.target = format!("doodad {DOODAD_TEMPLATE}#{obj}");
                    compare_obj = obj;
                }
                Ok(Err(reason)) => {
                    out.result = SkillOutcome::SetupFailed;
                    out.reason = reason;
                    self.finish(chat_start, start, &mut out);
                    return out;
                }
                Err(e) => {
                    out.result = SkillOutcome::Error;
                    out.reason = format!("connection error while spawning the doodad: {e:#}");
                    self.finish(chat_start, start, &mut out);
                    return out;
                }
            }
        } else if needs_dummy {
            match self.ensure_dummy().await {
                Ok(Ok(obj)) => {
                    compare_obj = obj;
                    if plan.target_type == 6 {
                        let pos = self
                            .game
                            .state
                            .units
                            .get(&obj)
                            .map(|u| u.pos)
                            .unwrap_or(self.game.state.self_pos);
                        let desired = Self::desired_distance(plan);
                        if let Err(e) = self.face_at_distance(pos, desired).await {
                            out.result = SkillOutcome::Error;
                            out.reason = format!("connection error while positioning: {e:#}");
                            self.finish(chat_start, start, &mut out);
                            return out;
                        }
                        target = SkillCastTarget::Position3 {
                            obj_id: 0,
                            x: pos.0,
                            y: pos.1,
                            z: pos.2 + 1.0,
                            pitch: 0.0,
                        };
                        out.target = format!("position at npc {DUMMY_TEMPLATE}#{obj}");
                    } else if plan.target_dead {
                        target = SkillCastTarget::Unit(obj);
                        out.target = format!("dead npc {DUMMY_TEMPLATE}#{obj}");
                    } else {
                        if let Some(pos) = self.game.state.units.get(&obj).map(|u| u.pos) {
                            let desired = Self::desired_distance(plan);
                            if let Err(e) = self.face_at_distance(pos, desired).await {
                                out.result = SkillOutcome::Error;
                                out.reason = format!("connection error while positioning: {e:#}");
                                self.finish(chat_start, start, &mut out);
                                return out;
                            }
                        }
                        if plan.target_type == 4 {
                            target = SkillCastTarget::Unit(obj);
                            out.target = format!("npc {DUMMY_TEMPLATE}#{obj}");
                        } else {
                            // Self-centred area skill with a hostile relation: cast on the
                            // bot, the dummy stands inside the area.
                            out.target =
                                format!("self ({self_obj}), npc {DUMMY_TEMPLATE}#{obj} nearby");
                        }
                    }
                    self.game.change_target(obj).await.ok();
                    if plan.target_dead && !self.game.state.dead_units.contains(&obj) {
                        match self.kill_dummy(obj).await {
                            Ok(true) => {}
                            Ok(false) => {
                                out.result = SkillOutcome::SetupFailed;
                                out.reason = format!(
                                    "could not kill the dummy {obj} for a target_dead skill"
                                );
                                self.finish(chat_start, start, &mut out);
                                return out;
                            }
                            Err(e) => {
                                out.result = SkillOutcome::Error;
                                out.reason =
                                    format!("connection error while killing the dummy: {e:#}");
                                self.finish(chat_start, start, &mut out);
                                return out;
                            }
                        }
                    }
                }
                Ok(Err(reason)) => {
                    out.result = SkillOutcome::SetupFailed;
                    out.reason = reason;
                    self.finish(chat_start, start, &mut out);
                    return out;
                }
                Err(e) => {
                    out.result = SkillOutcome::Error;
                    out.reason = format!("connection error while spawning the dummy: {e:#}");
                    self.finish(chat_start, start, &mut out);
                    return out;
                }
            }
        } else if plan.target_type == 6 {
            // A position skill without a hostile relation resolves at the caster's position.
            let pos = self.game.state.self_pos;
            target = SkillCastTarget::Position3 {
                obj_id: 0,
                x: pos.0,
                y: pos.1,
                z: pos.2,
                pitch: 0.0,
            };
            out.target = format!("position at self ({self_obj})");
        } else {
            out.target = format!("self ({self_obj})");
        }

        // Refill before the cast (the dummy too, so the HP delta is honest).
        if let Err(e) = self.heal_self().await {
            out.result = SkillOutcome::Error;
            out.reason = format!("connection error while healing: {e:#}");
            self.finish(chat_start, start, &mut out);
            return out;
        }
        if !plan.target_dead && needs_dummy && compare_obj != self_obj {
            if let Err(e) = self.heal_dummy(compare_obj).await {
                out.result = SkillOutcome::Error;
                out.reason = format!("connection error while healing the dummy: {e:#}");
                self.finish(chat_start, start, &mut out);
                return out;
            }
        }
        if let Err(e) = self.reset_cooldowns().await {
            out.result = SkillOutcome::Error;
            out.reason = format!("connection error during /resetcd: {e:#}");
            self.finish(chat_start, start, &mut out);
            return out;
        }

        // Buffs left by earlier skills (stuns, roots, refreshed buffs) would reject the cast or
        // hide the buff creation this skill is checked for.
        if let Err(e) = self.clear_buffs(self_obj).await {
            out.result = SkillOutcome::Error;
            out.reason = format!("connection error while clearing caster buffs: {e:#}");
            self.finish(chat_start, start, &mut out);
            return out;
        }
        if compare_obj != self_obj && !self.game.state.dead_units.contains(&compare_obj) {
            if let Err(e) = self.clear_buffs(compare_obj).await {
                out.result = SkillOutcome::Error;
                out.reason = format!("connection error while clearing target buffs: {e:#}");
                self.finish(chat_start, start, &mut out);
                return out;
            }
        }
        if compare_obj != self_obj {
            self.game.change_target(compare_obj).await.ok();
        }

        let mp_before = self
            .game
            .state
            .unit_points
            .get(&self_obj)
            .map(|(_, mp)| *mp);
        let hp_before = self
            .game
            .state
            .unit_points
            .get(&compare_obj)
            .map(|(hp, _)| *hp);

        let attempt = match self
            .execute_cast(plan, target, compare_obj, self.opts.step_timeout)
            .await
        {
            Ok(a) => a,
            Err(e) => {
                out.result = SkillOutcome::Error;
                out.reason = format!("connection error during the cast: {e:#}");
                self.finish(chat_start, start, &mut out);
                return out;
            }
        };

        let mp_after = self
            .game
            .state
            .unit_points
            .get(&self_obj)
            .map(|(_, mp)| *mp);
        let hp_after = self
            .game
            .state
            .unit_points
            .get(&compare_obj)
            .map(|(hp, _)| *hp);

        // Buffs created / removed on the caster and the compare unit, during the cast.
        for (owner, buff) in &attempt.buffs_created {
            if *owner == self_obj {
                out.buffs_created_caster.push(*buff);
            }
            if *owner == compare_obj {
                out.buffs_created_target.push(*buff);
            }
        }
        for (owner, buff) in &attempt.buffs_removed {
            if *owner == self_obj {
                out.buffs_removed_caster.push(*buff);
            }
            if *owner == compare_obj {
                out.buffs_removed_target.push(*buff);
            }
        }
        out.plot_events = attempt.plot_events;
        out.unit_points_updates_caster = attempt.points_self;
        out.unit_points_updates_target = attempt.points_compare;
        out.started_after_ms = attempt.started_ms;
        out.fired_after_ms = attempt.fired_ms;
        out.ended_after_ms = attempt.ended_ms;
        out.fired_delay_ms = attempt.fired_delay_ms;
        out.fired_channel_ms = attempt.fired_channel_ms;
        out.real_cast_time_ms = attempt.real_cast_ms;
        out.base_cast_time_ms = attempt.base_cast_ms;
        out.result_byte = attempt.result;
        out.result_uint = attempt.result_uint;
        out.stopped = attempt.stopped || attempt.casting_stopped;
        out.server_errors = attempt
            .errors
            .iter()
            .map(|(e1, e2)| format!("SCErrorMsg {e1}/{e2}"))
            .collect();
        out.mana_spent = match (mp_before, mp_after) {
            (Some(before), Some(after)) => Some((before - after).max(0)),
            _ => None,
        };
        out.target_hp_delta = match (hp_before, hp_after) {
            (Some(before), Some(after)) => Some(after - before),
            _ => None,
        };
        out.messages = window_messages(&self.game, chat_start);

        // Classification.
        if let Some(code) = out.result_byte {
            out.result = SkillOutcome::Rejected;
            out.reason = format!("SkillResult {code} ({})", skill_result_name(code));
            if out.result_uint != 0 {
                out.reason.push_str(&format!(", uint {}", out.result_uint));
            }
        } else if !attempt.responded && out.server_errors.is_empty() {
            if plan.plot_id != 0 {
                out.result = SkillOutcome::Rejected;
                out.reason = format!(
                    "plot {} emitted no SCPlotEvent within {} s (the server sends nothing when the first plot event's conditions fail)",
                    plan.plot_id,
                    SILENT_DROP_WAIT.as_secs()
                );
            } else {
                out.result = SkillOutcome::Timeout;
                out.reason = format!(
                    "no cast response within {} s (silent drop: GCD / state / requirement)",
                    SILENT_DROP_WAIT.as_secs()
                );
            }
        } else if attempt.fired_ms.is_none() && attempt.ended_ms.is_none() {
            out.result = SkillOutcome::Rejected;
            out.reason = if out.server_errors.is_empty() {
                "the server answered without a cast packet".to_string()
            } else {
                out.server_errors.join(", ")
            };
        } else {
            out.result = SkillOutcome::Ok;
            out.reason = match (attempt.fired_ms, attempt.ended_ms) {
                (Some(fired), Some(ended)) => {
                    format!("fired after {fired} ms, ended after {ended} ms")
                }
                (Some(fired), None) => format!("fired after {fired} ms, no end observed"),
                (None, Some(ended)) => format!("ended after {ended} ms"),
                (None, None) => "cast completed".to_string(),
            };
        }

        // The immediate-recast cooldown probe. The player GCD is 1000 ms (`Skill.Cast`
        // `default_gcd`), so a wait past it makes a `SkillResult 8` the skill's own cooldown.
        if out.result == SkillOutcome::Ok && plan.cooldown_time > 0 {
            let _ = self.pump_for(Duration::from_millis(1250)).await;
            match self
                .probe_recast(plan, target, Duration::from_millis(2500))
                .await
            {
                Ok(result) => out.recast_result = result,
                Err(e) => tracing::debug!("recast probe of skill {} errored: {e:#}", plan.id),
            }
            let _ = self.reset_cooldowns().await;
        }

        self.compare(plan, &attempt, &mut out);
        if plan.target_type == 8 {
            self.remove_doodad().await;
        }
        // A target_dead skill left the dummy dead; the next dummy-needing skill respawns it.
        if plan.target_dead {
            let dead = self.state.dummy_obj;
            if dead != 0 {
                let _ = self
                    .cmd(&format!("despawn npc {dead}"), GM_EFFECT_WAIT, Done::Never)
                    .await;
                self.state.dummy_obj = 0;
            }
        }
        self.finish(chat_start, start, &mut out);
        out
    }

    /// Data-vs-observed mismatches for an accepted cast.
    fn compare(&self, plan: &SkillPlan, attempt: &CastAttempt, out: &mut SkillResult) {
        if out.result != SkillOutcome::Ok {
            return;
        }
        // Cast timing.
        if plan.casting_time > 0 {
            if let Some(fired) = attempt.fired_ms {
                let expected = u64::from(plan.casting_time as u32);
                let tolerance = 250 + expected / 4;
                if fired.abs_diff(expected) > tolerance {
                    out.mismatches.push(format!(
                        "casting_time {} ms vs fired after {} ms",
                        plan.casting_time, fired
                    ));
                }
            }
        }
        if plan.effect_delay > 0 {
            if let Some(delay) = attempt.fired_delay_ms {
                if delay < plan.effect_delay {
                    out.mismatches.push(format!(
                        "SCSkillFired computed_delay {} ms < effect_delay {} ms",
                        delay, plan.effect_delay
                    ));
                }
            }
        }
        if plan.channeling_time > 0 {
            if let Some(channel) = attempt.fired_channel_ms {
                if (channel - plan.channeling_time).abs() > 10 {
                    out.mismatches.push(format!(
                        "SCSkillFired channeling {} ms != skills.channeling_time {} ms",
                        channel, plan.channeling_time
                    ));
                }
            }
        } else if plan.casting_time > 0 || plan.effect_delay > 0 {
            if let Some(ended) = attempt.ended_ms {
                let expected = u64::from((plan.casting_time + plan.effect_delay).max(0) as u32);
                let tolerance = 300 + expected / 5;
                if ended.abs_diff(expected) > tolerance {
                    out.mismatches.push(format!(
                        "cast+effect_delay {} ms vs ended after {} ms",
                        expected, ended
                    ));
                }
            }
        }
        // Mana.
        if let Some(spent) = out.mana_spent {
            let expected = mana_cost_expected(plan, self.state.raised_level as i32);
            let tolerance = 5.max(expected / 5);
            if (spent - expected).abs() > tolerance {
                out.mismatches.push(format!(
                    "mana cost {} vs observed MP loss {}",
                    expected, spent
                ));
            }
        }
        // Buffs.
        let expected_buffs = plan.effects.expected_buffs();
        let applied: HashSet<u32> = out
            .buffs_created_caster
            .iter()
            .chain(out.buffs_created_target.iter())
            .copied()
            .collect();
        // `skill_effects` lists every buff row of the skill: level-tier variants (e.g. 402 / 403 /
        // 404) and chance or condition-gated rows apply one at a time, so a side is only a
        // mismatch when none of its expected buffs showed up.
        for (side, buffs) in [
            ("caster", &plan.effects.buff_caster),
            ("target", &plan.effects.buff_target),
        ] {
            let wanted: Vec<u32> = expected_buffs
                .iter()
                .copied()
                .filter(|b| buffs.contains(b))
                .collect();
            if !wanted.is_empty() && wanted.iter().all(|b| !applied.contains(b)) {
                out.mismatches.push(format!(
                    "none of the expected {side} buffs {wanted:?} was applied during the cast"
                ));
            }
        }
        // Damage / heal rows vs the compare unit's HP delta.
        let damage_rows = plan
            .effects
            .damage_rows
            .iter()
            .filter(|r| r.damage_type != 5)
            .count();
        let heal_rows = plan.effects.heal_rows.len()
            + plan
                .effects
                .damage_rows
                .iter()
                .filter(|r| r.damage_type == 5)
                .count();
        if let Some(delta) = out.target_hp_delta {
            if damage_rows > 0 && delta >= 0 {
                out.mismatches.push(format!(
                    "{} damage row(s) exist but target HP delta is {}",
                    damage_rows, delta
                ));
            }
            // Plot skills run their effects from the plot tree (`plot_effects`), not `skill_effects`.
            if damage_rows == 0 && heal_rows == 0 && delta != 0 && plan.plot_id == 0 {
                out.mismatches.push(format!(
                    "target HP delta {} without a damage_effects / heal_effects row",
                    delta
                ));
            }
            if heal_rows > 0 && delta < 0 {
                out.mismatches.push(format!(
                    "{} heal row(s) exist but target HP delta is {}",
                    heal_rows, delta
                ));
            }
        }
        // Channel ticks.
        if plan.channeling_time > 0 && plan.channeling_tick > 0 && damage_rows > 0 {
            let ticks = i64::from((plan.channeling_time - 1) / plan.channeling_tick);
            out.ticks_expected = Some(ticks);
            if ticks >= 2 && attempt.points_compare == 0 {
                out.mismatches.push(format!(
                    "{} channel ticks expected (every {} ms) but no target HP update observed",
                    ticks, plan.channeling_tick
                ));
            }
        } else if plan.channeling_time > 0 && plan.channeling_tick > 0 {
            out.ticks_expected = Some(i64::from((plan.channeling_time - 1) / plan.channeling_tick));
        }
        // Cooldown.
        if plan.cooldown_time > 0 {
            match out.recast_result {
                Some(8) => {}
                Some(other) => out.mismatches.push(format!(
                    "cooldown {} ms but immediate recast got SkillResult {} ({}) instead of 8",
                    plan.cooldown_time,
                    other,
                    skill_result_name(other)
                )),
                None => out.mismatches.push(format!(
                    "cooldown {} ms but the immediate recast was not rejected",
                    plan.cooldown_time
                )),
            }
        }
        // Repeat effects (`skills.effect_repeat_count` / `effect_repeat_tick`).
        if plan.effect_repeat_count > 1 && plan.effect_repeat_tick > 0 {
            out.mismatches.push(format!(
                "note: effect_repeat_count {} x {} ms (tick packets are not separable on the wire)",
                plan.effect_repeat_count, plan.effect_repeat_tick
            ));
        }
    }

    /// Builds the result skeleton with the data expectations.
    fn new_result(&self, plan: &SkillPlan, _unknown: bool) -> SkillResult {
        SkillResult {
            skill_id: plan.id,
            name: plan.name.clone(),
            ability_id: plan.ability_id,
            character: self.character.name.clone(),
            result: SkillOutcome::Error,
            reason: String::new(),
            data_note: plan.data_note(),
            level_raised_to: self.state.raised_level,
            ability_swapped: false,
            learned: false,
            gear_required: Vec::new(),
            gear_missing: Vec::new(),
            target: String::new(),
            target_type: plan.target_type,
            target_type_name: plan.target_type_name().to_string(),
            casting_time_ms: plan.casting_time,
            channeling_time_ms: plan.channeling_time,
            channeling_tick_ms: plan.channeling_tick,
            effect_delay_ms: plan.effect_delay,
            cooldown_time_ms: plan.cooldown_time,
            mana_cost_expected: mana_cost_expected(plan, self.state.raised_level as i32),
            expected_buffs: plan.effects.expected_buffs(),
            damage_rows: plan.effects.damage_rows.clone(),
            heal_rows: plan.effects.heal_rows.clone(),
            plot_events: 0,
            ticks_expected: None,
            started_after_ms: None,
            fired_after_ms: None,
            ended_after_ms: None,
            fired_delay_ms: None,
            fired_channel_ms: None,
            real_cast_time_ms: None,
            base_cast_time_ms: None,
            result_byte: None,
            result_uint: 0,
            stopped: false,
            server_errors: Vec::new(),
            mana_spent: None,
            target_hp_delta: None,
            buffs_created_caster: Vec::new(),
            buffs_created_target: Vec::new(),
            buffs_removed_caster: Vec::new(),
            buffs_removed_target: Vec::new(),
            unit_points_updates_caster: 0,
            unit_points_updates_target: 0,
            recast_result: None,
            messages: Vec::new(),
            mismatches: Vec::new(),
            elapsed_ms: 0,
        }
    }

    /// Records the chat window and elapsed time.
    fn finish(&self, chat_start: u64, start: Instant, out: &mut SkillResult) {
        out.messages = window_messages(&self.game, chat_start);
        out.elapsed_ms = start.elapsed().as_millis() as u64;
    }
}

/// One cast's observable result.
#[derive(Debug, Default)]
struct CastAttempt {
    responded: bool,
    result: Option<u8>,
    result_uint: u32,
    started_ms: Option<u64>,
    fired_ms: Option<u64>,
    ended_ms: Option<u64>,
    stopped: bool,
    casting_stopped: bool,
    plot_events: u32,
    fired_delay_ms: Option<i32>,
    fired_channel_ms: Option<i32>,
    real_cast_ms: Option<u32>,
    base_cast_ms: Option<u32>,
    errors: Vec<(i16, i16)>,
    points_self: u32,
    points_compare: u32,
    buffs_created: Vec<(u32, u32)>,
    buffs_removed: Vec<(u32, u32)>,
}

// -------------------------------------------------------------------------------------------
// Selection and run
// -------------------------------------------------------------------------------------------

enum Selected {
    Plan(Box<SkillPlan>),
    Unknown(u32),
}

fn select_plans(db: &SkillDb, opts: &RunOptions) -> anyhow::Result<Vec<Selected>> {
    let mut selected: Vec<Selected> = if !opts.skill_ids.is_empty() {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for id in &opts.skill_ids {
            if !seen.insert(*id) {
                continue;
            }
            match db.skill(*id) {
                Some(plan) => out.push(Selected::Plan(Box::new(plan.clone()))),
                None => out.push(Selected::Unknown(*id)),
            }
        }
        out
    } else {
        let abilities: Vec<u8> = if opts.abilities.is_empty() {
            (1..=10).collect()
        } else {
            let mut a = opts.abilities.clone();
            a.sort_unstable();
            a.dedup();
            a
        };
        if !opts.all && opts.abilities.is_empty() {
            bail!("the skill command needs --skill <id> (repeatable), --ability <id> and/or --all");
        }
        let mut out = Vec::new();
        for ability in abilities {
            for plan in db.by_ability(ability) {
                out.push(Selected::Plan(Box::new(plan)));
            }
        }
        out
    };
    if let Some(limit) = opts.limit {
        selected.truncate(limit);
    }
    Ok(selected)
}

/// Skill ids already recorded in an `--out` JSONL file (malformed lines are ignored).
pub fn parse_done_ids(text: &str) -> HashSet<u32> {
    let mut done = HashSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(id) = value.get("skill_id").and_then(serde_json::Value::as_u64) {
            if let Ok(id) = u32::try_from(id) {
                done.insert(id);
            }
        }
    }
    done
}

/// [`parse_done_ids`] for an `--out` path (`None` when the file does not exist yet).
pub fn read_done_ids(path: &Path) -> HashSet<u32> {
    std::fs::read_to_string(path).map_or_else(|_| HashSet::new(), |text| parse_done_ids(&text))
}

fn emit(out: &mut Option<BufWriter<File>>, result: &SkillResult) -> anyhow::Result<()> {
    let line = serde_json::to_string(result).context("serializing the skill result")?;
    println!("{line}");
    if let Some(w) = out {
        writeln!(w, "{line}")?;
        w.flush()?;
    }
    Ok(())
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

/// Runs the selected skills; one JSON line per skill goes to stdout and, when given, is
/// appended to `opts.out`. An existing `--out` file makes the run resume: every skill id
/// already present in it is skipped.
pub async fn run(cfg: &BotConfig, opts: &RunOptions) -> anyhow::Result<RunReport> {
    let db = open_skill_db(opts.compact_db.as_deref())?;
    let mut selected = select_plans(&db, opts)?;
    if let Some(path) = &opts.out {
        let done = read_done_ids(path);
        if !done.is_empty() {
            let before = selected.len();
            selected.retain(|target| match target {
                Selected::Plan(plan) => !done.contains(&plan.id),
                Selected::Unknown(id) => !done.contains(id),
            });
            println!(
                "[skill] resuming from {}: {} already recorded, {} to run",
                path.display(),
                before - selected.len(),
                selected.len()
            );
        }
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
    let (game, character, session, abilities) = setup_world(cfg, &rec, opts).await?;
    let mut sweep = Sweep {
        game,
        session,
        state: SweepState {
            abilities,
            raised_level: u32::from(character.start_level).max(1),
            ..SweepState::default()
        },
        character,
        opts,
    };
    // Safety net: hostile skills hit the dummy, never the bot.
    sweep
        .cmd("godmode true", Duration::from_secs(1), Done::Never)
        .await
        .ok();
    sweep.raise_level(false).await?;
    if let Err(e) = sweep.ensure_gear().await {
        sweep.leave().await;
        return Err(e.context("equipping the sweep's holdables"));
    }

    let mut report = RunReport::default();
    for target in selected {
        if sweep.game.state.disconnected {
            report.aborted = true;
            break;
        }
        let (plan, unknown) = match target {
            Selected::Plan(plan) => ((*plan).clone(), false),
            Selected::Unknown(id) => (
                SkillPlan {
                    id,
                    name: String::new(),
                    ability_id: 0,
                    ability_level: 0,
                    level_step: 0,
                    skill_points: 0,
                    auto_learn: false,
                    need_learn: false,
                    show: false,
                    target_type: 0,
                    target_relation: 0,
                    casting_time: 0,
                    channeling_time: 0,
                    channeling_tick: 0,
                    cooldown_time: 0,
                    mana_cost: 0,
                    mana_level_md: 0.0,
                    min_range: 0.0,
                    max_range: 0.0,
                    toggle_buff_id: 0,
                    effect_delay: 0,
                    effect_repeat_count: 1,
                    effect_repeat_tick: 0,
                    active_weapon_id: 0,
                    mainhand_tool_id: 0,
                    offhand_tool_id: 0,
                    target_dead: false,
                    or_unit_reqs: false,
                    plot_id: 0,
                    effects: EffectSummary::default(),
                    reqs: Vec::new(),
                },
                true,
            ),
        };
        let result = sweep.run_one(&plan, unknown).await;
        emit(&mut writer, &result)?;
        report.results.push(result);
        if sweep.game.state.disconnected {
            report.aborted = true;
            break;
        }
    }
    sweep.leave().await;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaemu_commons::{Marshal, PacketStream};

    fn started_packet(result: u8, real_ms: u16, base_ms: u16) -> Packet {
        use aaemu_game::systems::skills::packets::SCSkillStartedPacket;
        let mut p = SCSkillStartedPacket::new(
            10375,
            7,
            SkillCaster::Unit(0x100),
            SkillCastTarget::Unit(0x200),
            SkillObject::default(),
        );
        p.real_cast_time_div10 = real_ms;
        p.base_cast_time_div10 = base_ms;
        p.set_skill_result(result);
        let mut body = PacketStream::new();
        p.write(&mut body);
        Packet {
            level: crate::game::LEVEL_GAME,
            opcode: sc::SCSkillStartedPacket,
            body: body.into_bytes(),
        }
    }

    #[test]
    fn plot_packets_parse_tl_and_skill() {
        let ev = Packet {
            level: crate::game::LEVEL_GAME,
            opcode: sc::SCPlotEventPacket,
            body: vec![7, 0, 1, 0, 0, 0, 0x9e, 0x2b, 0, 0],
        };
        assert_eq!(parse_plot_event(&ev), Some((7, 11166)));
        for (op, f) in [
            (
                sc::SCPlotEndedPacket,
                parse_plot_ended as fn(&Packet) -> Option<u16>,
            ),
            (sc::SCPlotCastingStoppedPacket, parse_plot_stopped),
            (sc::SCPlotChannelingStoppedPacket, parse_plot_stopped),
        ] {
            let p = Packet {
                level: crate::game::LEVEL_GAME,
                opcode: op,
                body: vec![9, 0, 0, 0, 0, 0, 0],
            };
            assert_eq!(f(&p), Some(9));
        }
        assert_eq!(parse_plot_ended(&ev), None);
        let batch = Packet {
            level: crate::game::LEVEL_COMPRESSED,
            opcode: 2,
            body: vec![0, 0, 0xae, 0, 7, 0, 1, 0, 0, 0, 0x9e, 0x2b, 0, 0],
        };
        assert_eq!(parse_plot_event(&batch), Some((7, 11166)));
    }

    #[test]
    fn skill_started_parses_result_and_cast_times() {
        let p = started_packet(8, 1500, 1500);
        let parsed = parse_skill_started(&p).unwrap();
        assert_eq!(parsed.skill_id, 10375);
        assert_eq!(parsed.tl, 7);
        assert_eq!(parsed.caster_obj, 0x100);
        assert_eq!(parsed.result, 8);
        assert_eq!(parsed.real_cast_ms, 15_000);
        assert_eq!(parsed.base_cast_ms, 15_000);

        let p = started_packet(0, 0, 0);
        let parsed = parse_skill_started(&p).unwrap();
        assert_eq!(parsed.result, 0);
        assert_eq!(parsed.result_uint, 0);
    }

    #[test]
    fn skill_fired_parses_delay_and_channel() {
        use aaemu_game::systems::skills::packets::SCSkillFiredPacket;
        let fired = SCSkillFiredPacket {
            id: 10488,
            tl: 9,
            caster: SkillCaster::Unit(0x100),
            target: SkillCastTarget::Unit(0x100),
            skill_object: SkillObject::default(),
            computed_delay: 1234,
            channeling_time: 60_000,
            fire_anim_id: 0,
        };
        let mut body = PacketStream::new();
        fired.write(&mut body);
        let p = Packet {
            level: crate::game::LEVEL_GAME,
            opcode: sc::SCSkillFiredPacket,
            body: body.into_bytes(),
        };
        let parsed = parse_skill_fired(&p).unwrap();
        assert_eq!(parsed.skill_id, 10488);
        assert_eq!(parsed.tl, 9);
        assert_eq!(parsed.caster_obj, 0x100);
        assert_eq!(parsed.delay_ms, 1230); // wire rounds to 10 ms
        assert_eq!(parsed.channel_ms, 60_000);
    }

    #[test]
    fn mana_cost_scales_with_ability_level() {
        let plan = SkillPlan {
            id: 1,
            name: String::new(),
            ability_id: 1,
            ability_level: 1,
            level_step: 1,
            skill_points: 0,
            auto_learn: false,
            need_learn: false,
            show: true,
            target_type: 0,
            target_relation: 0,
            casting_time: 0,
            channeling_time: 0,
            channeling_tick: 0,
            cooldown_time: 0,
            mana_cost: 20,
            mana_level_md: 2.0,
            min_range: 0.0,
            max_range: 0.0,
            toggle_buff_id: 0,
            effect_delay: 0,
            effect_repeat_count: 1,
            effect_repeat_tick: 0,
            active_weapon_id: 0,
            mainhand_tool_id: 0,
            offhand_tool_id: 0,
            target_dead: false,
            or_unit_reqs: false,
            plot_id: 0,
            effects: EffectSummary::default(),
            reqs: Vec::new(),
        };
        // Skill.ManaCost: ((ab - 1) * 1.6 + 8) * 3 / 3.65 * mana_level_md + mana_cost.
        // Level 1: base 8 * 3 / 3.65 = 6.57, x2.0 = 13.15, + 20 -> 33 (f64 -> i32 truncates).
        assert_eq!(mana_cost_expected(&plan, 1), 33);
        // Level 55: base (54 * 1.6 + 8) * 3 / 3.65 = 77.59, x2.0 = 155.18, + 20 -> 175.
        assert_eq!(mana_cost_expected(&plan, 55), 175);
    }

    #[test]
    fn desired_distance_lands_inside_the_range() {
        let mut plan = SkillPlan {
            id: 1,
            name: String::new(),
            ability_id: 1,
            ability_level: 1,
            level_step: 1,
            skill_points: 0,
            auto_learn: false,
            need_learn: false,
            show: true,
            target_type: 4,
            target_relation: 0,
            casting_time: 0,
            channeling_time: 0,
            channeling_tick: 0,
            cooldown_time: 0,
            mana_cost: 0,
            mana_level_md: 0.0,
            min_range: 0.0,
            max_range: 4.0,
            toggle_buff_id: 0,
            effect_delay: 0,
            effect_repeat_count: 1,
            effect_repeat_tick: 0,
            active_weapon_id: 0,
            mainhand_tool_id: 0,
            offhand_tool_id: 0,
            target_dead: false,
            or_unit_reqs: false,
            plot_id: 0,
            effects: EffectSummary::default(),
            reqs: Vec::new(),
        };
        assert!((0.0..=4.0).contains(&Sweep::desired_distance(&plan)));
        plan.min_range = 6.0;
        plan.max_range = 20.0;
        assert!((6.0..=20.0).contains(&Sweep::desired_distance(&plan)));
        plan.min_range = 10.0;
        plan.max_range = 50.0;
        assert!((10.0..=50.0).contains(&Sweep::desired_distance(&plan)));
    }

    #[test]
    fn done_ids_parse_skill_lines() {
        let text = "{\"skill_id\":10375,\"result\":\"ok\"}\nnot json\n{\"skill_id\":4}\n";
        let done = parse_done_ids(text);
        assert!(done.contains(&10375));
        assert!(done.contains(&4));
        assert_eq!(done.len(), 2);
    }

    #[test]
    fn skill_db_loads_the_running_game_data_when_present() {
        let path = Path::new(DEFAULT_COMPACT_DB);
        if !path.exists() {
            return;
        }
        let db = SkillDb::open(path).unwrap();
        let all = db.all();
        assert_eq!(all.len(), 170);
        // Ability 1 (Fight) has 21 skills; 10644 (Ground Smash) wants a mainhand weapon.
        assert_eq!(db.by_ability(1).len(), 21);
        let smash = db.skill(10644).unwrap();
        assert_eq!(smash.target_type, 4);
        assert!(smash.reqs.iter().any(|r| r.kind == 8 && r.value1 == 15));
        assert!(smash.effects.damage_rows.iter().any(|r| r.damage_type != 0));
        // 10375 (Impenetrable Defense, Defense) applies the six BuffEffect rows to the caster.
        let defense = db.skill(10375).unwrap();
        assert!(defense.effects.expected_buffs().contains(&328));
        // 10488 (Play Dead) is a 60 s channel.
        assert_eq!(db.skill(10488).unwrap().channeling_time, 60_000);
        // 10546 (Resurrect) needs a dead target.
        assert!(db.skill(10546).unwrap().target_dead);
        // 10694 (Glide) is the one toggle skill of the ten abilities.
        assert_eq!(db.skill(10694).unwrap().toggle_buff_id, 181);
    }
}
