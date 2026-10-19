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
use crate::craftrun::{equipment_tasks, EquipmentChanges, EquippedItem, RunCharacter};
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
/// The caster level the sweep runs at; `skill_effects.start_level` / `end_level` are compared
/// against it (`Skill.Cast` skips rows outside the caster's level range).
const SWEEP_CASTER_LEVEL: u8 = TARGET_LEVEL;
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
/// `SlotType.Equipment` of `SCItemTaskSuccessPacket` rows.
const SLOT_TYPE_EQUIPMENT: u8 = 1;
/// One-hand sword (first-steps long sword), shield, bow and string instrument; the four cover
/// every holdable `unit_reqs` can demand (`equip_slot` 8, `equip_shield` 21, `equip_ranged` 29).
const GEAR: [(u32, u8); 4] = [
    (5569, SLOT_MAINHAND),
    (6152, SLOT_OFFHAND),
    (6127, SLOT_RANGED),
    (6177, SLOT_MUSICAL),
];
/// Mainhand weapon of the default setup (the one-hand sword).
const MELEE_WEAPON_TEMPLATE: u32 = GEAR[0].0;
/// Level-55 two-handed staff ("Delphinad ... staff", magic DPS) worn for spell-scaled damage so
/// the weapon term of `DamageEffect.Apply` is not zero on the bare bot.
const SPELL_WEAPON_TEMPLATE: u32 = 20446;
/// A cooldown at or below this is not probeable: the player GCD is ~1 s (`Skill.Cast`
/// `default_gcd`) and the recast answers `SkillResult 8` for the GCD before the skill's own
/// cooldown can be told apart from it.
const COOLDOWN_PROBE_MIN_MS: i32 = 3000;

// -------------------------------------------------------------------------------------------
// Compact database: skill plan data
// -------------------------------------------------------------------------------------------

/// One `damage_effects` row of a skill, as the server loads it for `DamageEffect.Apply`, plus the
/// `skill_effects` gates the server checks before the row can run.
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
    /// `skill_effects.friendly` / `non_friendly`.
    #[serde(default)]
    pub friendly: bool,
    #[serde(default)]
    pub non_friendly: bool,
    /// `skill_effects.chance` / `weight`; a run picks one weighted row of a group.
    #[serde(default)]
    pub chance: i32,
    #[serde(default)]
    pub weight: i32,
    /// A source / target buff-tag gate the runner cannot evaluate (a missing hit is not a
    /// mismatch then, it is recorded as a note).
    #[serde(default)]
    pub conditional: bool,
}

impl DamageRow {
    /// A row the data leaves no choice about: it runs on every cast with a compatible target.
    pub fn unconditional(&self, target_self: bool) -> bool {
        self.chance >= 100 && self.weight == 0 && !self.conditional && self.applies(target_self)
    }

    /// The `friendly` / `non_friendly` gate of the server (`Skill.Cast`): a hostile-only row does
    /// not run on the caster, a friendly-only row does not run on a hostile target.
    pub fn applies(&self, target_self: bool) -> bool {
        relation_allows(self.friendly, self.non_friendly, target_self)
    }
}

/// One `heal_effects` row of a skill (with the same `skill_effects` gates as [`DamageRow`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealRow {
    pub effect_id: u32,
    pub fixed_min: i32,
    pub fixed_max: i32,
    pub use_level_heal: bool,
    pub level_md: f64,
    #[serde(default)]
    pub friendly: bool,
    #[serde(default)]
    pub non_friendly: bool,
    #[serde(default)]
    pub chance: i32,
    #[serde(default)]
    pub weight: i32,
    #[serde(default)]
    pub conditional: bool,
}

impl HealRow {
    pub fn unconditional(&self, target_self: bool) -> bool {
        self.chance >= 100 && self.weight == 0 && !self.conditional && self.applies(target_self)
    }

    pub fn applies(&self, target_self: bool) -> bool {
        relation_allows(self.friendly, self.non_friendly, target_self)
    }
}

/// One `skill_effects` row that applies a buff, with the gates the server checks before the row
/// can run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuffRow {
    pub buff_id: u32,
    /// `application_method_id`: 1 Target / 4 SourceToPos apply to the cast target, 2 Source /
    /// 3 SourceOnce to the caster.
    pub application_method: i32,
    pub friendly: bool,
    pub non_friendly: bool,
    pub source_buff_tag_id: u32,
    pub target_buff_tag_id: u32,
    pub source_no_buff_tag_id: u32,
    pub target_no_buff_tag_id: u32,
    pub chance: i32,
    pub weight: i32,
    /// Plot event ids carrying this buff (plot skills): the buff only applies when the plot
    /// reaches one of them.
    #[serde(default)]
    pub plot_events: Vec<u32>,
}

impl BuffRow {
    /// Buffs the cast target receives (`application_method_id` 1 / 4).
    pub fn is_target_side(&self) -> bool {
        matches!(self.application_method, 1 | 4)
    }

    /// The `friendly` / `non_friendly` gate against the actual target (the dummy is hostile, the
    /// caster friendly; non-unit targets are left to the data).
    pub fn applies_to(&self, target_self: bool) -> bool {
        if self.is_target_side() {
            relation_allows(self.friendly, self.non_friendly, target_self)
        } else {
            // Source side: the caster is its own (friendly) target.
            relation_allows(self.friendly, self.non_friendly, true)
        }
    }

    /// A buff the runner can demand unconditionally: no tag / chance / weight / plot gate.
    pub fn unconditional(&self, target_self: bool) -> bool {
        self.chance >= 100
            && self.weight == 0
            && self.source_buff_tag_id == 0
            && self.target_buff_tag_id == 0
            && self.source_no_buff_tag_id == 0
            && self.target_no_buff_tag_id == 0
            && self.plot_events.is_empty()
            && self.applies_to(target_self)
    }

    /// Why a gated buff may legitimately not be applied (for the record).
    pub fn gate_note(&self) -> String {
        let mut parts = Vec::new();
        if self.source_buff_tag_id != 0 {
            parts.push(format!("source buff tag {}", self.source_buff_tag_id));
        }
        if self.target_buff_tag_id != 0 {
            parts.push(format!("target buff tag {}", self.target_buff_tag_id));
        }
        if self.source_no_buff_tag_id != 0 {
            parts.push(format!("source no-buff tag {}", self.source_no_buff_tag_id));
        }
        if self.target_no_buff_tag_id != 0 {
            parts.push(format!("target no-buff tag {}", self.target_no_buff_tag_id));
        }
        if self.weight > 0 {
            parts.push(format!("weight {}", self.weight));
        }
        if self.chance < 100 {
            parts.push(format!("chance {}", self.chance));
        }
        if !self.plot_events.is_empty() {
            parts.push(format!("plot event(s) {:?}", self.plot_events));
        }
        if !self.applies_to(true) || !self.applies_to(false) {
            parts.push("relation gate".to_string());
        }
        if parts.is_empty() {
            parts.push("condition not clear".to_string());
        }
        parts.join(", ")
    }
}

/// The `friendly` / `non_friendly` gate of one `skill_effects` row for a target that is the
/// caster itself (friendly) or another unit (the sweep's dummy, hostile). Mirrors `Skill.Cast`:
/// a hostile-only row is skipped on the caster itself, a friendly-only row on a hostile target;
/// a row with both flags applies anywhere.
fn relation_allows(friendly: bool, non_friendly: bool, target_self: bool) -> bool {
    if !friendly && non_friendly && target_self {
        return false;
    }
    if friendly && !non_friendly && !target_self {
        return false;
    }
    true
}

/// One `unit_reqs` row owned by the skill (`kind_id`, `value1`, `value2`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReqRow {
    pub kind: i32,
    pub value1: i32,
    pub value2: i32,
}

/// What the skill's `skill_effects` -> `effects` resolve to (level-filtered to the caster level
/// the sweep raises to).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EffectSummary {
    /// `BuffEffect` rows with the gates of their `skill_effects` row.
    pub buff_rows: Vec<BuffRow>,
    pub damage_rows: Vec<DamageRow>,
    pub heal_rows: Vec<HealRow>,
    /// Other effect types (`SpecialEffect`, `DispelEffect`, ...) by row count.
    pub other: BTreeMap<String, usize>,
}

impl EffectSummary {
    /// Every buff id the data can apply (caster or target side).
    pub fn expected_buffs(&self) -> Vec<u32> {
        let mut out: Vec<u32> = self.buff_rows.iter().map(|r| r.buff_id).collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Damage rows that can run at all for the actual target (`friendly` / `non_friendly`).
    pub fn damage_rows_for(&self, target_self: bool) -> Vec<&DamageRow> {
        self.damage_rows
            .iter()
            .filter(|r| r.applies(target_self))
            .collect()
    }

    /// Heal rows that can run at all for the actual target; a `DamageType.Heal` damage row
    /// counts as a heal (the 1.2 data uses both).
    pub fn heal_rows_for(&self, target_self: bool) -> Vec<&HealRow> {
        self.heal_rows
            .iter()
            .filter(|r| r.applies(target_self))
            .collect()
    }

    /// Buff rows that can run at all for the actual target.
    pub fn buff_rows_for(&self, target_self: bool) -> Vec<&BuffRow> {
        self.buff_rows
            .iter()
            .filter(|r| r.applies_to(target_self))
            .collect()
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
    pub target_selection: i32,
    pub target_area_radius: i32,
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
    /// `skills.plot_only`: the skill returns from `Skill.Use` after `run_plot`, so its
    /// `skill_effects` never run and the plot tree (mana, buffs, damage) is the whole effect.
    pub plot_only: bool,
    /// The plot's `SpecialEffect` 39 `ManaCost` total (plot skills only): every reachable event's
    /// `value1 + value2 / 6.35`, each truncated as the server truncates on the wire. Only rows
    /// whose source is the caster count (`ManaCost` needs a player caster).
    pub plot_mana_cost: Option<i32>,
    /// Reachable plot events carrying a caster-side `ManaCost`: the cost only applies when one
    /// of them ran (`plot_mana_cost` is the sum of all of them).
    #[serde(default)]
    pub plot_mana_events: Vec<u32>,
    /// The skill has a `RestoreManaEffect`: the net MP delta is not the cost, so the mana check
    /// is skipped.
    #[serde(default)]
    pub mana_restore: bool,
    /// `unit_reqs` kind 12 (`skill_urk_combat`): the caster must not be in battle. The runner
    /// waits out the server's 15 s combat timeout before the cast.
    #[serde(default)]
    pub requires_no_combat: bool,
    /// The skill's effects include a `SpecialEffect` 43 (`ResetCooldown`): the cast removes its
    /// own cooldown, so the recast probe cannot test it.
    #[serde(default)]
    pub resets_cooldown: bool,
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

    /// A damage row that can only hit a hostile target: the sweep must never cast such a skill
    /// on itself (the server skips the row on a friendly target and godmode swallows the rest).
    pub fn has_hostile_effects(&self) -> bool {
        self.effects
            .damage_rows
            .iter()
            .any(|r| !r.friendly && r.non_friendly)
    }

    /// Spell-scaled damage: a row that scales with the caster level (`level_md`) and has no
    /// fixed part, so the weapon's DPS term should not be zero (the staff is worn then).
    pub fn uses_spell_scaling(&self) -> bool {
        self.effects.damage_rows.iter().any(|r| {
            r.fixed_min == 0 && r.fixed_max == 0 && (r.level_md > 0.0 || r.damage_type == 2)
        })
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

/// What a plot tree contributes to a `plot_only` skill: the total `ManaCost` of its reachable
/// events and the buff each reachable event's `BuffEffect` row applies.
struct PlotPlan {
    mana_cost: i32,
    /// Reachable events carrying a caster-side `ManaCost` row (the cost only applies when one
    /// of them ran).
    mana_events: Vec<u32>,
    /// buff id -> (event id, `PlotEffectTarget`) of reachable events carrying it.
    buff_events: HashMap<u32, Vec<(u32, i32)>>,
}

/// Walks the plots of `plot_ids` (`plots` -> root event -> `plot_next_events` children,
/// transitively) and collects their `SpecialEffect` 39 `ManaCost` (the server's
/// `special::combat::mana_cost`) and `BuffEffect` rows from `plot_effects`.
fn load_plots(
    conn: &rusqlite::Connection,
    plot_ids: &HashSet<u32>,
) -> rusqlite::Result<HashMap<u32, PlotPlan>> {
    let mut roots: HashMap<u32, u32> = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT plot_id, id FROM plot_events WHERE position = 1")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, u32>(1)?)))?;
        for row in rows {
            let (plot, event) = row?;
            if plot_ids.contains(&plot) {
                roots.entry(plot).or_insert(event);
            }
        }
    }
    let mut next: HashMap<u32, Vec<u32>> = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT event_id, next_event_id FROM plot_next_events")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, u32>(1)?)))?;
        for row in rows {
            let (from, to) = row?;
            next.entry(from).or_default().push(to);
        }
    }
    let mut mana_by_event: HashMap<u32, i32> = HashMap::new();
    let mut buff_by_event: HashMap<u32, Vec<(u32, i32)>> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT event_id, actual_id, actual_type, target_id, source_id FROM plot_effects \
             WHERE actual_type IN ('SpecialEffect','BuffEffect')",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, u32>(0)?,
                r.get::<_, u32>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i32>(3)?,
                r.get::<_, i32>(4)?,
            ))
        })?;
        for row in rows {
            let (event, actual_id, actual_type, target_id, source_id) = row?;
            match actual_type.as_str() {
                "SpecialEffect" => {
                    // `ManaCost` takes mana from the caster: only a source of 1 (the caster) or
                    // 3 (the previous source -- the caster when the plot starts) can pay it
                    // (`special::combat::mana_cost` needs a player caster).
                    if !matches!(source_id, 1 | 3) {
                        continue;
                    }
                    let se = conn.query_row(
                        "SELECT special_effect_type_id, value1, value2 FROM special_effects \
                         WHERE id = ?1",
                        [actual_id],
                        |r| {
                            Ok((
                                r.get::<_, i32>(0)?,
                                r.get::<_, i32>(1)?,
                                r.get::<_, i32>(2)?,
                            ))
                        },
                    );
                    if let Ok((39, value1, value2)) = se {
                        *mana_by_event.entry(event).or_insert(0) +=
                            (f64::from(value1) + f64::from(value2) / 6.35) as i32;
                    }
                }
                "BuffEffect" => {
                    if let Ok(buff_id) = conn.query_row(
                        "SELECT buff_id FROM buff_effects WHERE id = ?1",
                        [actual_id],
                        |r| r.get::<_, u32>(0),
                    ) {
                        buff_by_event
                            .entry(event)
                            .or_default()
                            .push((buff_id, target_id));
                    }
                }
                _ => {}
            }
        }
    }
    let mut out = HashMap::new();
    for (plot, root) in roots {
        let mut reachable: HashSet<u32> = HashSet::new();
        let mut stack = vec![root];
        while let Some(event) = stack.pop() {
            if !reachable.insert(event) {
                continue;
            }
            if let Some(children) = next.get(&event) {
                stack.extend(children.iter().copied());
            }
        }
        let mana_events: Vec<u32> = reachable
            .iter()
            .filter(|event| mana_by_event.contains_key(event))
            .copied()
            .collect();
        let mana_cost = mana_events
            .iter()
            .filter_map(|event| mana_by_event.get(event))
            .sum();
        let mut buff_events: HashMap<u32, Vec<(u32, i32)>> = HashMap::new();
        for event in &reachable {
            if let Some(buffs) = buff_by_event.get(event) {
                for (buff, target_id) in buffs {
                    buff_events
                        .entry(*buff)
                        .or_default()
                        .push((*event, *target_id));
                }
            }
        }
        out.insert(
            plot,
            PlotPlan {
                mana_cost,
                mana_events,
                buff_events,
            },
        );
    }
    Ok(out)
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
                 COALESCE(target_selection_id,0), COALESCE(target_area_radius,0), \
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
                 COALESCE(plot_id,0), \
                 CASE WHEN plot_only='t' THEN 1 ELSE 0 END \
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
                    target_selection: r.get(11)?,
                    target_area_radius: r.get(12)?,
                    casting_time: r.get(13)?,
                    channeling_time: r.get(14)?,
                    channeling_tick: r.get(15)?,
                    cooldown_time: r.get(16)?,
                    mana_cost: r.get(17)?,
                    mana_level_md: r.get(18)?,
                    min_range: f64::from(r.get::<_, i32>(19)?),
                    max_range: r.get(20)?,
                    toggle_buff_id: r.get(21)?,
                    effect_delay: r.get(22)?,
                    effect_repeat_count: r.get(23)?,
                    effect_repeat_tick: r.get(24)?,
                    active_weapon_id: r.get(25)?,
                    mainhand_tool_id: r.get(26)?,
                    offhand_tool_id: r.get(27)?,
                    target_dead: r.get::<_, i64>(28)? != 0,
                    or_unit_reqs: r.get::<_, i64>(29)? != 0,
                    plot_id: r.get(30)?,
                    plot_only: r.get::<_, i64>(31)? != 0,
                    plot_mana_cost: None,
                    plot_mana_events: Vec::new(),
                    mana_restore: false,
                    requires_no_combat: false,
                    resets_cooldown: false,
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
                        friendly: true,
                        non_friendly: true,
                        chance: 100,
                        weight: 0,
                        conditional: false,
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
                        friendly: true,
                        non_friendly: true,
                        chance: 100,
                        weight: 0,
                        conditional: false,
                    },
                ))
            })?;
            let mut heal: HashMap<u32, HealRow> = HashMap::new();
            for row in rows {
                let (id, h) = row?;
                heal.insert(id, h);
            }
            // `SpecialEffect` type per id (the `ResetCooldown` action resets the cast's own
            // cooldown, so the recast probe cannot test it).
            let mut special_types: HashMap<u32, i32> = HashMap::new();
            {
                let mut stmt =
                    conn.prepare("SELECT id, special_effect_type_id FROM special_effects")?;
                let rows =
                    stmt.query_map([], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, i32>(1)?)))?;
                for row in rows {
                    let (id, kind) = row?;
                    special_types.insert(id, kind);
                }
            }

            // One row per `skill_effects` entry; the gate columns drive the runner's
            // expectation (level, relation, buff tags, chance, weight).
            struct Gate {
                start_level: u8,
                end_level: u8,
                friendly: bool,
                non_friendly: bool,
                source_buff_tag_id: u32,
                target_buff_tag_id: u32,
                source_no_buff_tag_id: u32,
                target_no_buff_tag_id: u32,
                chance: i32,
                weight: i32,
            }
            impl Gate {
                fn conditional(&self) -> bool {
                    self.source_buff_tag_id != 0
                        || self.target_buff_tag_id != 0
                        || self.source_no_buff_tag_id != 0
                        || self.target_no_buff_tag_id != 0
                }
            }
            let mut stmt = conn.prepare(
                "SELECT se.skill_id, se.effect_id, ef.actual_type, ef.actual_id, \
                 se.application_method_id, se.start_level, se.end_level, \
                 CASE WHEN se.friendly='t' THEN 1 ELSE 0 END, \
                 CASE WHEN se.non_friendly='t' THEN 1 ELSE 0 END, \
                 COALESCE(se.source_buff_tag_id,0), COALESCE(se.target_buff_tag_id,0), \
                 COALESCE(se.source_nobuff_tag_id,0), COALESCE(se.target_nobuff_tag_id,0), \
                 COALESCE(se.chance,0), COALESCE(se.weight,0) \
                 FROM skill_effects se JOIN effects ef ON ef.id = se.effect_id \
                 WHERE se.skill_id IN (SELECT id FROM skills WHERE ability_id BETWEEN 1 AND 10)",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, u32>(0)?,    // skill id
                    r.get::<_, u32>(1)?,    // effect id
                    r.get::<_, String>(2)?, // actual type
                    r.get::<_, u32>(3)?,    // actual id
                    r.get::<_, i32>(4)?,    // application method
                    Gate {
                        start_level: r.get(5)?,
                        end_level: r.get(6)?,
                        friendly: r.get::<_, i64>(7)? != 0,
                        non_friendly: r.get::<_, i64>(8)? != 0,
                        source_buff_tag_id: r.get(9)?,
                        target_buff_tag_id: r.get(10)?,
                        source_no_buff_tag_id: r.get(11)?,
                        target_no_buff_tag_id: r.get(12)?,
                        chance: r.get(13)?,
                        weight: r.get(14)?,
                    },
                ))
            })?;
            for row in rows {
                let (skill_id, effect_id, actual_type, actual_id, method, gate) = row?;
                let Some(plan) = skills.get_mut(&skill_id) else {
                    continue;
                };
                // The server skips a row outside the caster's level range.
                if gate.start_level > SWEEP_CASTER_LEVEL || gate.end_level < SWEEP_CASTER_LEVEL {
                    continue;
                }
                match actual_type.as_str() {
                    "BuffEffect" => {
                        let Some(buff_id) = buffs.get(&actual_id).copied() else {
                            continue;
                        };
                        plan.effects.buff_rows.push(BuffRow {
                            buff_id,
                            application_method: method,
                            friendly: gate.friendly,
                            non_friendly: gate.non_friendly,
                            source_buff_tag_id: gate.source_buff_tag_id,
                            target_buff_tag_id: gate.target_buff_tag_id,
                            source_no_buff_tag_id: gate.source_no_buff_tag_id,
                            target_no_buff_tag_id: gate.target_no_buff_tag_id,
                            chance: gate.chance,
                            weight: gate.weight,
                            plot_events: Vec::new(),
                        });
                    }
                    "DamageEffect" => {
                        if let Some(mut dmg) = damage.get(&actual_id).cloned() {
                            dmg.effect_id = effect_id;
                            dmg.friendly = gate.friendly;
                            dmg.non_friendly = gate.non_friendly;
                            dmg.chance = gate.chance;
                            dmg.weight = gate.weight;
                            dmg.conditional = gate.conditional();
                            plan.effects.damage_rows.push(dmg);
                        }
                    }
                    "HealEffect" => {
                        if let Some(mut h) = heal.get(&actual_id).cloned() {
                            h.effect_id = effect_id;
                            h.friendly = gate.friendly;
                            h.non_friendly = gate.non_friendly;
                            h.chance = gate.chance;
                            h.weight = gate.weight;
                            h.conditional = gate.conditional();
                            plan.effects.heal_rows.push(h);
                        }
                    }
                    "RestoreManaEffect" => {
                        plan.mana_restore = true;
                        *plan.effects.other.entry(actual_type).or_default() += 1;
                    }
                    "SpecialEffect" => {
                        if special_types.get(&actual_id) == Some(&43) {
                            plan.resets_cooldown = true;
                        }
                        *plan.effects.other.entry(actual_type).or_default() += 1;
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
                    if req.kind == 12 {
                        plan.requires_no_combat = true;
                    }
                    plan.reqs.push(req);
                }
            }
        }
        // Plot trees of the plot skills: mana is taken by `SpecialEffect` 39 `ManaCost` rows of
        // reachable events (a `plot_only` skill returns before `Skill.Cast`'s `ConsumeMana`),
        // buffs only apply when their event is reached.
        {
            let plot_ids: HashSet<u32> = skills
                .values()
                .map(|p| p.plot_id)
                .filter(|p| *p != 0)
                .collect();
            if !plot_ids.is_empty() {
                let plots = load_plots(&conn, &plot_ids)?;
                for plan in skills.values_mut() {
                    if plan.plot_id == 0 {
                        continue;
                    }
                    let Some(plot) = plots.get(&plan.plot_id) else {
                        continue;
                    };
                    if plan.plot_only {
                        plan.plot_mana_cost = Some(plot.mana_cost);
                        plan.plot_mana_events = plot.mana_events.clone();
                    }
                    for row in &mut plan.effects.buff_rows {
                        if let Some(events) = plot.buff_events.get(&row.buff_id) {
                            row.plot_events = events.iter().map(|(event, _)| *event).collect();
                        }
                    }
                    // A plot buff the `skill_effects` data does not list is still applied by the
                    // plot: record it so a reached-but-missing buff shows up. `PlotEffectTarget`
                    // 1 (caster) / 3 (previous source) apply to the caster side, 2 / 4 / 5 to the
                    // target side.
                    for (buff_id, events) in &plot.buff_events {
                        if plan.effects.buff_rows.iter().any(|r| r.buff_id == *buff_id) {
                            continue;
                        }
                        let caster_side = events.iter().all(|(_, target)| matches!(target, 1 | 3));
                        plan.effects.buff_rows.push(BuffRow {
                            buff_id: *buff_id,
                            application_method: if caster_side { 3 } else { 1 },
                            friendly: true,
                            non_friendly: true,
                            source_buff_tag_id: 0,
                            target_buff_tag_id: 0,
                            source_no_buff_tag_id: 0,
                            target_no_buff_tag_id: 0,
                            chance: 100,
                            weight: 0,
                            plot_events: events.iter().map(|(event, _)| *event).collect(),
                        });
                    }
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
    /// `items.id` of the weapon equipped for the cast (spell-scaled damage gets the staff).
    pub weapon_used: u32,
    /// Distance to the training dummy right before the cast (metres).
    pub dummy_distance_m: Option<f64>,
    pub target: String,
    /// `self` / `unit` / `doodad` / `position` (what the observed HP delta belongs to).
    pub target_kind: String,
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
    pub plot_id: u32,
    pub plot_only: bool,
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
    /// Where `mana_spent` was read: `first update after the cast response`, `lowest update
    /// during the cast` (the cost update precedes the response on the wire) or `end of cast`.
    pub mana_source: String,
    /// Target HP after - before (negative = damage, positive = heal).
    pub target_hp_delta: Option<i32>,
    /// `SCUnitDamagedPacket`s of this skill on any unit other than the caster (misses
    /// included; an area skill hits every unit in its area).
    pub damage_packets: u32,
    /// Of those, the ones with a miss / immune / dodge / parry / block hit type.
    pub miss_packets: u32,
    /// The largest damage value of those packets (0 = every hit was absorbed).
    pub dealt_damage: i32,
    /// `SCUnitDamagedPacket`s of this skill on the caster itself.
    pub self_damage_packets: u32,
    pub heal_packets: u32,
    pub buffs_created_caster: Vec<u32>,
    pub buffs_created_target: Vec<u32>,
    pub buffs_removed_caster: Vec<u32>,
    pub buffs_removed_target: Vec<u32>,
    pub plot_events: u32,
    /// `SCPlotEventPacket.event_id`s seen for this skill (bounded to 64).
    pub plot_event_ids: Vec<u32>,
    pub unit_points_updates_caster: u32,
    pub unit_points_updates_target: u32,
    /// The immediate second cast's `SkillResult` (8 = cooldown as expected); 0 = accepted,
    /// `None` = no answer.
    pub recast_result: Option<u8>,
    /// `probed`, or why the cooldown probe was skipped (`cooldown <= 3000 ms`, a `skill_req`
    /// blocking buff, the recast answered `skill_req_fail`).
    pub cooldown_probe: String,
    pub messages: Vec<String>,
    pub mismatches: Vec<String>,
    /// Facts that explain an observation without being a data deviation (buffs changed the HP,
    /// godmode suppressed self damage, a plot-dependent buff was not reached, ...).
    pub notes: Vec<String>,
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
    let mut notes: BTreeMap<String, usize> = BTreeMap::new();
    for r in &report.results {
        for n in &r.notes {
            *notes.entry(reason_key(n)).or_default() += 1;
        }
    }
    let mut top: Vec<(&String, &usize)> = notes.iter().collect();
    top.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    if !top.is_empty() {
        out.push_str("\n[skill] notes (not mismatches):");
        for (note, n) in top.iter().take(10) {
            out.push_str(&format!("\n  {n}x {note}"));
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
        50 => "skill_req_fail",
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

/// The mana the cast is expected to take. A `plot_only` skill *with a plot* returns from
/// `Skill.Use` before `Skill.Cast`'s `ConsumeMana`, so its cost is the plot's `SpecialEffect` 39
/// `ManaCost` (`special/combat.rs::mana_cost`); a `plot_only` row without a `plot_id` runs the
/// normal cast and pays the formula. A plot the data gives no cost takes nothing. Skill
/// modifiers (which the server applies on top) are not part of the expectation: the sweep's bot
/// carries none.
pub fn expected_mana_cost(plan: &SkillPlan, ab_level: i32) -> i32 {
    if plan.plot_only && plan.plot_id != 0 {
        plan.plot_mana_cost.unwrap_or(0)
    } else {
        mana_cost_expected(plan, ab_level)
    }
}

/// Reads the cast's mana from the observed `SCUnitPointsPacket` updates of the caster. The cost
/// is paid once, at the cast (`Skill.Cast` for a normal skill, during the plot for a
/// `plot_only` one); the update may precede the cast's first response packet (an instant cast
/// pays before `SCSkillFired`, a plot applies its effects before the event batch is sent), so a
/// larger drop than the first-after-response update is still the cost. An end-of-cast delta is
/// not used: a channel's regen (or a mana restore effect) hides the cost behind it.
fn measure_mana(
    before: Option<i32>,
    updates: &[i32],
    after_response: Option<i32>,
) -> (Option<i32>, String) {
    let Some(before) = before else {
        return (None, "no MP reading before the cast".to_string());
    };
    let drop_of = |mp: i32| (before - mp).max(0);
    let max_drop = updates.iter().map(|mp| drop_of(*mp)).max();
    if let Some(after) = after_response {
        let d = drop_of(after);
        if let Some(max_drop) = max_drop {
            if max_drop > d {
                return (
                    Some(max_drop),
                    "largest MP drop during the cast".to_string(),
                );
            }
        }
        return (Some(d), "first update after the cast response".to_string());
    }
    if let Some(min) = updates.iter().min() {
        return (
            Some(drop_of(*min)),
            "lowest update during the cast".to_string(),
        );
    }
    (None, "no MP update during the cast".to_string())
}

// -------------------------------------------------------------------------------------------
// Packet readers
// -------------------------------------------------------------------------------------------

/// `SCPlotEventPacket` -> `(tl, event, skill)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PlotEvent {
    tl: u16,
    event_id: u32,
    skill_id: u32,
}

/// `SCUnitDamagedPacket` -> the hit facts (the sweep sees misses too, which `SCUnitPoints` alone
/// cannot tell from "no damage at all").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DamageHit {
    skill_id: u32,
    caster_obj: u32,
    caster_id: u32,
    target_id: u32,
    damage: i32,
    /// `SkillHitType` (the wire packs it into the low byte of `288 | hitType`).
    hit_type: u8,
}

impl DamageHit {
    /// A miss / immune / dodge / parry / block hit type ("a miss" for the runner).
    fn is_miss(&self) -> bool {
        matches!(
            self.hit_type,
            0x04 | 0x05 | 0x06 | 0x07 | 0x0A | 0x0E | 0x10 | 0x11 | 0x12 | 0x13 | 0x14
        )
    }
}

/// `SCUnitHealedPacket`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HealHit {
    skill_id: u32,
    caster_obj: u32,
    target_id: u32,
    heal_type: u8,
    hit_type: u8,
    value: i32,
}

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

/// `SCPlotEventPacket`: plot skills (`skills.plot_id`) announce their steps with it instead of
/// `SCSkillStarted` / `SCSkillFired`. The batch it arrives in is split into level-1 packets by
/// [`crate::game::split_compressed_batch`], so each event is read on its own.
fn parse_plot_event(p: &Packet) -> Option<PlotEvent> {
    if !p.is_sc(sc::SCPlotEventPacket) {
        return None;
    }
    let mut s = p.stream();
    let tl = s.read_u16().ok()?;
    let event_id = s.read_u32().ok()?;
    let skill_id = s.read_u32().ok()?;
    Some(PlotEvent {
        tl,
        event_id,
        skill_id,
    })
}

/// `SCUnitDamagedPacket`: the cast action identifies the skill (or plot) that ran the damage.
fn parse_unit_damaged(p: &Packet) -> Option<DamageHit> {
    if !p.is_sc(sc::SCUnitDamagedPacket) {
        return None;
    }
    let mut s = p.stream();
    let skill_id = parse_cast_action(&mut s)?;
    let caster_kind = s.read_u8().ok()?;
    let caster = SkillCaster::read(caster_kind, &mut s).ok()?;
    let caster_id = s.read_bc().ok()?;
    let target_id = s.read_bc().ok()?;
    s.read_u8().ok()?; // crime state
    let values = s.read_pisc_w(3).ok()?; // damage, absorbed, 0
    s.read_pisc_w(3).ok()?; // 0, 0, mana burn
    s.read_u8().ok()?; // holdable id
    let packed = s.read_u16().ok()?; // 288 | hit type
    s.read_u8().ok()?; // flag
    s.read_u8().ok()?; // result
    Some(DamageHit {
        skill_id,
        caster_obj: caster.obj_id(),
        caster_id,
        target_id,
        damage: values[0] as i32,
        // `288 + hit_type` on the wire (C# `Write((ushort)(288 + (byte)HitType))`): subtract the
        // constant back out.
        hit_type: (packed as i32 - 288).clamp(0, 0xFF) as u8,
    })
}

/// `SCUnitHealedPacket`.
fn parse_unit_healed(p: &Packet) -> Option<HealHit> {
    if !p.is_sc(sc::SCUnitHealedPacket) {
        return None;
    }
    let mut s = p.stream();
    let skill_id = parse_cast_action(&mut s)?;
    let caster_kind = s.read_u8().ok()?;
    let caster = SkillCaster::read(caster_kind, &mut s).ok()?;
    let target_id = s.read_bc().ok()?;
    let heal_type = s.read_u8().ok()?;
    let hit_type = s.read_u8().ok()?;
    let value = s.read_i32().ok()?;
    Some(HealHit {
        skill_id,
        caster_obj: caster.obj_id(),
        target_id,
        heal_type,
        hit_type,
        value,
    })
}

/// `CastAction` -> the id of the skill whose effect applied the packet (0 for a buff / unknown
/// cast action). `CastAction::Plot` carries the skill id too.
fn parse_cast_action(s: &mut aaemu_commons::PacketStream) -> Option<u32> {
    match s.read_u8().ok()? {
        0 => {
            let skill_id = s.read_u32().ok()?;
            s.read_u16().ok()?; // tl
            Some(skill_id)
        }
        1 => {
            s.read_u32().ok()?; // plot id
            s.read_u16().ok()?; // tl
            s.read_u32().ok()?; // event id
            Some(s.read_u32().ok()?)
        }
        2 => {
            s.read_u32().ok()?;
            s.read_bc().ok()?;
            s.read_u32().ok()?;
            s.read_bool().ok()?;
            s.read_bool().ok()?;
            Some(0)
        }
        3 => {
            s.read_i32().ok()?;
            s.read_i32().ok()?;
            s.read_bc().ok()?;
            Some(0)
        }
        4 => {
            s.read_i32().ok()?;
            s.read_bc().ok()?;
            Some(0)
        }
        _ => None,
    }
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
    /// Item id -> template id of every item the session has seen (equipment / bag snapshots
    /// and item tasks): an `ItemMove` task carries only item ids.
    item_templates: HashMap<u64, u32>,
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
            match equipment_tasks(&p.body) {
                Ok(changes) => self.apply_equipment_changes(changes),
                Err(e) => tracing::debug!("equipment task of an SCItemTaskSuccess not parsed: {e}"),
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

    /// Applies an `SCItemTaskSuccessPacket`'s equipment rows. `ItemMove` tasks carry no
    /// templates: the moved item's template comes from [`Self::item_templates`] (fed by the
    /// create / snapshot rows) or from the equipment row it came from.
    fn apply_equipment_changes(&mut self, changes: EquipmentChanges) {
        for (item_id, template_id) in &changes.templates {
            self.item_templates.insert(*item_id, *template_id);
        }
        for row in &changes.created {
            tracing::debug!(
                "equipment: slot {} <- template {} (item {})",
                row.slot,
                row.template_id,
                row.item_id
            );
            self.equipment.insert(
                row.slot,
                EquippedItem {
                    item_id: row.item_id,
                    template_id: row.template_id,
                    slot: row.slot,
                },
            );
        }
        for slot in changes.removed_slots {
            self.equipment.remove(&slot);
        }
        for mv in changes.moves {
            let from_template = self
                .item_templates
                .get(&mv.from_item_id)
                .copied()
                .or_else(|| {
                    self.equipment
                        .values()
                        .find(|i| i.item_id == mv.from_item_id)
                        .map(|i| i.template_id)
                });
            let to_template = self
                .item_templates
                .get(&mv.to_item_id)
                .copied()
                .or_else(|| {
                    self.equipment
                        .get(&mv.to_slot)
                        .filter(|i| i.item_id == mv.to_item_id)
                        .map(|i| i.template_id)
                });
            if mv.to_item_id != 0 {
                // The displaced item moves to the source slot (an equipment <-> equipment
                // swap; a bag source just gets it back).
                if mv.from_slot_type == SLOT_TYPE_EQUIPMENT {
                    if let Some(template_id) = to_template {
                        self.equipment.insert(
                            mv.from_slot,
                            EquippedItem {
                                item_id: mv.to_item_id,
                                template_id,
                                slot: mv.from_slot,
                            },
                        );
                    } else {
                        self.equipment.remove(&mv.from_slot);
                    }
                }
            }
            if mv.from_slot_type == SLOT_TYPE_EQUIPMENT {
                self.equipment.remove(&mv.from_slot);
            }
            if mv.to_slot_type == SLOT_TYPE_EQUIPMENT {
                match from_template {
                    Some(template_id) => {
                        tracing::debug!(
                            "equipment: slot {} <- template {} (item {}, moved)",
                            mv.to_slot,
                            template_id,
                            mv.from_item_id
                        );
                        self.equipment.insert(
                            mv.to_slot,
                            EquippedItem {
                                item_id: mv.from_item_id,
                                template_id,
                                slot: mv.to_slot,
                            },
                        );
                    }
                    None => {
                        tracing::debug!(
                            "equipment: slot {} <- unknown item {} (no template seen)",
                            mv.to_slot,
                            mv.from_item_id
                        );
                    }
                }
            }
        }
    }

    /// Seeds the item id -> template map from the selection snapshot's bag and equipment.
    fn seed_known_items(&mut self, game: &GameClient) {
        for item in game.state.bag.values() {
            self.item_templates.insert(item.item_id, item.template_id);
        }
        for row in self.equipment.values() {
            self.item_templates.insert(row.item_id, row.template_id);
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
    // The item id -> template map the template-less `ItemMove` tasks need: the selection
    // snapshot's equipment and the bag rows the inventory packets carried.
    session.seed_known_items(&game);
    for row in session.equipment.values() {
        game.state
            .item_templates
            .insert(row.item_id, row.template_id);
    }
    let bag_rows: Vec<(u64, u32)> = game
        .state
        .bag
        .values()
        .map(|item| (item.item_id, item.template_id))
        .collect();
    for (item_id, template_id) in bag_rows {
        game.state.item_templates.insert(item_id, template_id);
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

    /// The distance from the bot to `obj` in metres.
    fn dummy_distance(&self, obj: u32) -> Option<f64> {
        let me = self.game.state.self_pos;
        self.game.state.units.get(&obj).map(|u| {
            let (dx, dy) = (f64::from(u.pos.0 - me.0), f64::from(u.pos.1 - me.1));
            (dx * dx + dy * dy).sqrt()
        })
    }

    /// Respawns the training dummy in front of the bot and returns its fresh object id. The old
    /// copy is despawned first: a dummy that wandered or was left at half HP from an earlier
    /// skill made the damage/HP measurements unreliable (and a dead target stays dead).
    async fn place_dummy(&mut self) -> anyhow::Result<Result<u32, String>> {
        let old = self.state.dummy_obj;
        if old != 0 {
            let _ = self
                .cmd(&format!("despawn npc {old}"), GM_EFFECT_WAIT, Done::Never)
                .await;
            self.state.dummy_obj = 0;
        }
        self.spawn_dummy_excluding(old).await
    }

    /// Spawns one more training dummy in front of the bot (the area skills' second target) and
    /// returns its object id.
    async fn spawn_dummy(&mut self) -> anyhow::Result<Result<u32, String>> {
        let old = self.state.dummy_obj;
        self.spawn_dummy_excluding(old).await
    }

    /// Spawns a dummy in front of the bot, ignoring `old` (the copy being replaced).
    async fn spawn_dummy_excluding(&mut self, old: u32) -> anyhow::Result<Result<u32, String>> {
        let _ = self
            .cmd(
                &format!("spawn npc {DUMMY_TEMPLATE}"),
                GM_EFFECT_WAIT,
                Done::LiveUnitAppears(DUMMY_TEMPLATE),
            )
            .await?;
        // The despawned copy can still be in view; the fresh one is the unit with a new object
        // id. Give its announcement a moment when the first check only saw the old one.
        let deadline = Instant::now() + Duration::from_millis(1500);
        loop {
            let fresh = self
                .game
                .state
                .units
                .values()
                .filter(|u| u.unit_type == 1 && u.id == DUMMY_TEMPLATE && u.obj_id != old)
                .filter(|u| !self.game.state.dead_units.contains(&u.obj_id))
                .max_by(|a, b| {
                    let me = self.game.state.self_pos;
                    let da = (a.pos.0 - me.0).powi(2) + (a.pos.1 - me.1).powi(2);
                    let db = (b.pos.0 - me.0).powi(2) + (b.pos.1 - me.1).powi(2);
                    db.total_cmp(&da)
                })
                .cloned();
            if let Some(unit) = fresh {
                self.state.dummy_obj = unit.obj_id;
                // The dummy spawns 3 m in front of the bot and fights nothing back.
                return Ok(Ok(unit.obj_id));
            }
            if Instant::now() >= deadline {
                return Ok(Err(format!(
                    "/spawn npc {DUMMY_TEMPLATE} did not announce a fresh unit"
                )));
            }
            if let Some(p) = self.game.recv_within(PUMP).await? {
                self.session.observe(&p);
            }
        }
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
    /// `CSSwapItemsPacket` into the equipment container). A two-handed staff swap moves the
    /// shield to the bag; this puts it back.
    async fn ensure_gear(&mut self) -> anyhow::Result<()> {
        for (template, slot) in GEAR {
            self.ensure_slot(template, slot).await?;
        }
        Ok(())
    }

    /// The weapon the skill's damage scales with: the one-hand sword by default, the level-55
    /// two-handed staff for spell-scaled damage (`skills` damage rows the sweep marks).
    async fn ensure_weapon(&mut self, template: u32) -> anyhow::Result<()> {
        self.ensure_slot(template, SLOT_MAINHAND).await
    }

    /// Makes `template` the item of `slot` (giving it to the bot when the bag has none).
    async fn ensure_slot(&mut self, template: u32, slot: u8) -> anyhow::Result<()> {
        if self
            .session
            .equipment
            .get(&slot)
            .is_some_and(|item| item.template_id == template)
        {
            return Ok(());
        }
        if self.game.state.bag_count(&[template]) == 0 {
            tracing::debug!(
                "ensure_slot {template} slot {slot}: bag has no {template}; bag rows: {:?}",
                self.game
                    .state
                    .bag
                    .iter()
                    .map(|(slot, item)| (*slot, item.item_id % 0x1_000_000, item.template_id))
                    .collect::<Vec<_>>()
            );
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
            bail!("the GM give did not create item {template} (equipment slot {slot} stays empty)");
        };
        // Pass the target slot's current item id: the server only swaps the old item back when
        // it is told what it is (a two-handed staff into an occupied mainhand would otherwise
        // leave the old weapon behind).
        let target_item = self
            .session
            .equipment
            .get(&slot)
            .map(|item| item.item_id)
            .unwrap_or(0);
        tracing::debug!(
            "equip: template {template} item {item_id} bag slot {bag_slot} -> equipment slot {slot} \
             (target item {target_item}, mainhand {:?})",
            self.session
                .equipment
                .get(&SLOT_MAINHAND)
                .map(|i| i.template_id)
        );
        self.game
            .move_item(item_id, 2, bag_slot, target_item, SLOT_TYPE_EQUIPMENT, slot)
            .await?;
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
                    attempt.responded = true;
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
            if let Some(event) = parse_plot_event(&p) {
                if event.skill_id == plan.id {
                    our_tl = Some(event.tl);
                    attempt.plot_events += 1;
                    attempt.responded = true;
                    if attempt.plot_event_ids.len() < 64 {
                        attempt.plot_event_ids.push(event.event_id);
                    }
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
            if let Some((obj, _, mp)) = parse_unit_points(&p) {
                if obj == self_obj {
                    attempt.points_self += 1;
                    attempt.mp_self.push(mp);
                    if attempt.responded && attempt.mp_after_response.is_none() {
                        attempt.mp_after_response = Some(mp);
                    }
                }
                if obj == compare_obj {
                    attempt.points_compare += 1;
                }
            }
            if let Some(hit) = parse_unit_damaged(&p) {
                tracing::debug!(
                    "SCUnitDamaged skill {} caster {}->{} target {} damage {} hit {}",
                    hit.skill_id,
                    hit.caster_obj,
                    hit.caster_id,
                    hit.target_id,
                    hit.damage,
                    hit.hit_type
                );
                attempt.damage_hits.push(hit);
            }
            if let Some(hit) = parse_unit_healed(&p) {
                tracing::debug!(
                    "SCUnitHealed skill {} caster {} target {} value {}",
                    hit.skill_id,
                    hit.caster_obj,
                    hit.target_id,
                    hit.value
                );
                attempt.heal_hits.push(hit);
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

        // `unit_reqs` kind 12 wants the caster out of battle; the server clears that flag 15 s
        // (world `DefaultCombatTimeout`) after the last combat activity, so wait it out (the
        // command that clears it client-side does not touch the server flag).
        if plan.requires_no_combat {
            let deadline = Instant::now() + Duration::from_secs(17);
            while let Some(left) = deadline.checked_duration_since(Instant::now()) {
                if left.is_zero() {
                    break;
                }
                match self.game.recv_within(PUMP.min(left)).await {
                    Ok(Some(p)) => self.session.observe(&p),
                    Ok(None) => {}
                    Err(e) => {
                        out.result = SkillOutcome::Error;
                        out.reason = format!("connection error while waiting out combat: {e:#}");
                        self.finish(chat_start, start, &mut out);
                        return out;
                    }
                }
            }
        }

        // Gear: the four holdable slots the data's unit_reqs can demand (also restores what a
        // two-handed staff swap moved to the bag), then the weapon the skill's damage scales
        // with. Spell-scaled damage (`level_md` with no fixed part) gets the staff so the
        // weapon DPS term is not zero; a skill that needs the offhand slot keeps the sword.
        if let Err(e) = self.ensure_gear().await {
            out.result = SkillOutcome::SetupFailed;
            out.reason = format!("could not equip the sweep's holdables: {e:#}");
            self.finish(chat_start, start, &mut out);
            return out;
        }
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
        let spell_scaled = plan.uses_spell_scaling() && !out.gear_required.contains(&SLOT_OFFHAND);
        let weapon = if spell_scaled {
            SPELL_WEAPON_TEMPLATE
        } else {
            MELEE_WEAPON_TEMPLATE
        };
        if let Err(e) = self.ensure_weapon(weapon).await {
            out.result = SkillOutcome::SetupFailed;
            out.reason = format!("could not equip weapon {weapon}: {e:#}");
            self.finish(chat_start, start, &mut out);
            return out;
        }
        out.weapon_used = weapon;

        // Target setup: doodad copy, training dummy (also for target_dead corpses) or self.
        // Hostile skills are never cast on the bot itself (godmode would swallow the self
        // damage); they go on a freshly placed dummy in front of the bot.
        let self_obj = self.game.state.self_obj;
        let mut compare_obj = self_obj;
        let mut target = SkillCastTarget::Unit(self_obj);
        let needs_dummy = plan.targets_dummy() || plan.target_dead;
        if plan.target_type == 8 {
            match self.ensure_doodad().await {
                Ok(Ok(obj)) => {
                    target = SkillCastTarget::Doodad(obj);
                    out.target = format!("doodad {DOODAD_TEMPLATE}#{obj}");
                    out.target_kind = "doodad".into();
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
            match self.place_dummy().await {
                Ok(Ok(obj)) => {
                    compare_obj = obj;
                    let pos_a = self
                        .game
                        .state
                        .units
                        .get(&obj)
                        .map(|u| u.pos)
                        .unwrap_or(self.game.state.self_pos);
                    let desired = Self::desired_distance(plan);
                    if plan.target_type == 6 {
                        if let Err(e) = self.face_at_distance(pos_a, desired).await {
                            out.result = SkillOutcome::Error;
                            out.reason = format!("connection error while positioning: {e:#}");
                            self.finish(chat_start, start, &mut out);
                            return out;
                        }
                        target = SkillCastTarget::Position3 {
                            obj_id: 0,
                            x: pos_a.0,
                            y: pos_a.1,
                            z: pos_a.2 + 1.0,
                            pitch: 0.0,
                        };
                        out.target = format!("position at npc {DUMMY_TEMPLATE}#{obj}");
                        out.target_kind = "position".into();
                    } else if plan.target_dead {
                        target = SkillCastTarget::Unit(obj);
                        out.target = format!("dead npc {DUMMY_TEMPLATE}#{obj}");
                        out.target_kind = "unit".into();
                    } else {
                        if let Err(e) = self.face_at_distance(pos_a, desired).await {
                            out.result = SkillOutcome::Error;
                            out.reason = format!("connection error while positioning: {e:#}");
                            self.finish(chat_start, start, &mut out);
                            return out;
                        }
                        // Hostile relation or hostile-only effect rows: cast on the dummy,
                        // never on the bot (godmode would swallow self damage). A self-target
                        // skill with a hostile relation is a plot skill whose events expect a
                        // hostile `previous target`, so it goes on the dummy too.
                        let hostile = plan.target_relation == 4 || plan.has_hostile_effects();
                        if plan.target_type == 4
                            || (plan.target_type == 5 && hostile)
                            || (plan.target_type == 0 && plan.target_relation == 4)
                        {
                            target = SkillCastTarget::Unit(obj);
                            out.target = format!("npc {DUMMY_TEMPLATE}#{obj}");
                        } else {
                            out.target = format!("self ({self_obj})");
                        }
                        out.target_kind = "unit".into();
                    }
                    // Area skills exclude the cast target from their own area
                    // (`geometry::around_radius` skips the origin): a second dummy inside the
                    // area is the unit the damage and buffs land on.
                    if plan.target_area_radius > 0 && plan.target_selection == 2 {
                        let lo = (plan.min_range.max(0.0) + 0.5).max(0.75);
                        let hi = if plan.max_range > 0.0 {
                            (plan.max_range - 0.5).max(lo)
                        } else {
                            desired + 1.0
                        };
                        let d = (desired + 1.0).clamp(lo, hi);
                        if let Err(e) = self.face_at_distance(pos_a, d).await {
                            out.notes
                                .push(format!("could not position for the area dummy: {e:#}"));
                        } else {
                            match self.spawn_dummy().await {
                                Ok(Ok(b)) => {
                                    compare_obj = b;
                                    out.dummy_distance_m = self.dummy_distance(b);
                                    out.target.push_str(&format!(" + area dummy {b}"));
                                }
                                Ok(Err(reason)) => {
                                    out.notes.push(format!("no area dummy: {reason}"));
                                }
                                Err(e) => {
                                    out.notes
                                        .push(format!("could not spawn the area dummy: {e:#}"));
                                }
                            }
                        }
                    }
                    if out.dummy_distance_m.is_none() {
                        out.dummy_distance_m = self.dummy_distance(obj);
                    }
                    if compare_obj == self_obj {
                        out.target_kind = "self".into();
                    }
                    self.game.change_target(compare_obj).await.ok();
                    if plan.target_dead && !self.game.state.dead_units.contains(&obj) {
                        self.game.change_target(obj).await.ok();
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
            // A position skill without a hostile relation: cast a few metres in front of the
            // bot. A cast at the bot's own position has no direction and answers
            // `SkillResult 16` (`skill_outof_angle`).
            let pos = self.game.state.self_pos;
            let ahead = (pos.0, pos.1 + 3.0, pos.2);
            if let Err(e) = self.game.move_to_facing(pos, ahead, 300).await {
                out.result = SkillOutcome::Error;
                out.reason = format!("connection error while facing ahead: {e:#}");
                self.finish(chat_start, start, &mut out);
                return out;
            }
            target = SkillCastTarget::Position3 {
                obj_id: 0,
                x: ahead.0,
                y: ahead.1,
                z: ahead.2,
                pitch: 0.0,
            };
            out.target = format!("position 3 m ahead of self ({self_obj})");
            out.target_kind = "position".into();
        } else {
            out.target = format!("self ({self_obj})");
            out.target_kind = "self".into();
        }
        if compare_obj == self_obj {
            out.target_kind = "self".into();
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

        let hp_after = self
            .game
            .state
            .unit_points
            .get(&compare_obj)
            .map(|(hp, _)| *hp);

        // Buffs created / removed on the caster and on any other unit (an area skill applies
        // them to every unit in its area, not only to the cast target).
        for (owner, buff) in &attempt.buffs_created {
            if *owner == self_obj {
                out.buffs_created_caster.push(*buff);
            } else {
                out.buffs_created_target.push(*buff);
            }
        }
        for (owner, buff) in &attempt.buffs_removed {
            if *owner == self_obj {
                out.buffs_removed_caster.push(*buff);
            } else {
                out.buffs_removed_target.push(*buff);
            }
        }
        out.plot_events = attempt.plot_events;
        out.plot_event_ids = attempt.plot_event_ids.clone();
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
        // Mana: the cost is paid once, when the cast runs (`Skill.Cast` for a normal skill,
        // during the plot for a `plot_only` one). The update can precede the cast's response
        // packet, so the first update after the response is preferred, the lowest update
        // during the cast is the fallback (an end-of-channel delta would hide the cost behind
        // regen).
        let (mana_spent, mana_source) =
            measure_mana(mp_before, &attempt.mp_self, attempt.mp_after_response);
        out.mana_spent = mana_spent;
        out.mana_source = mana_source;
        out.target_hp_delta = match (hp_before, hp_after) {
            (Some(before), Some(after)) => Some(after - before),
            _ => None,
        };
        // Damage / heal packets of this cast on any unit other than the caster (an area skill
        // hits every unit in its area; the cast target itself is excluded by the server).
        out.damage_packets = attempt
            .damage_hits
            .iter()
            .filter(|h| h.skill_id == plan.id && h.target_id != self_obj)
            .count() as u32;
        out.miss_packets = attempt
            .damage_hits
            .iter()
            .filter(|h| h.skill_id == plan.id && h.target_id != self_obj && h.is_miss())
            .count() as u32;
        out.self_damage_packets = attempt
            .damage_hits
            .iter()
            .filter(|h| h.skill_id == plan.id && h.target_id == self_obj)
            .count() as u32;
        out.dealt_damage = attempt
            .damage_hits
            .iter()
            .filter(|h| h.skill_id == plan.id && h.target_id != self_obj)
            .map(|h| h.damage)
            .max()
            .unwrap_or(0);
        out.heal_packets = attempt
            .heal_hits
            .iter()
            .filter(|h| h.skill_id == plan.id)
            .count() as u32;
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

        // The cooldown probe: the recast goes out right after the cast ends. A skill cooldown
        // at or below [`COOLDOWN_PROBE_MIN_MS`] is not probeable (the ~1 s GCD window leaves no
        // room before the cooldown itself expires), and a `skill_req` buff the cast applied
        // makes the recast answer `skill_req_fail` before the cooldown check — neither is a
        // cooldown deviation.
        if out.result == SkillOutcome::Ok && plan.cooldown_time > 0 {
            if plan.resets_cooldown {
                out.cooldown_probe =
                    "cooldown not probeable: the skill resets its own cooldown (SpecialEffect 43)"
                        .to_string();
            } else if plan.cooldown_time <= COOLDOWN_PROBE_MIN_MS {
                out.cooldown_probe = format!(
                    "cooldown not probeable: cooldown {} ms <= {} ms (the GCD window is ~1 s)",
                    plan.cooldown_time, COOLDOWN_PROBE_MIN_MS
                );
            } else {
                match self
                    .probe_recast(plan, target, Duration::from_millis(2500))
                    .await
                {
                    Ok(Some(50))
                        if !out.buffs_created_caster.is_empty()
                            || !out.buffs_created_target.is_empty() =>
                    {
                        out.cooldown_probe = format!(
                            "cooldown not probeable: recast answered SkillResult 50 \
                             (skill_req_fail) with buffs from the first cast (caster {:?}, \
                             target {:?})",
                            out.buffs_created_caster, out.buffs_created_target
                        );
                    }
                    // The recast resolved the same target from a stale position (a wandering
                    // dummy, or a cast that turned the caster): a range / angle rejection
                    // comes before the cooldown check and says nothing about it.
                    Ok(Some(code)) if (14..=16).contains(&code) => {
                        out.cooldown_probe = format!(
                            "cooldown not probeable: the recast answered SkillResult {code} ({})",
                            skill_result_name(code)
                        );
                    }
                    Ok(result) => {
                        out.recast_result = result;
                        out.cooldown_probe = match result {
                            Some(code) => format!(
                                "probed: recast answered SkillResult {code} ({})",
                                skill_result_name(code)
                            ),
                            None => "probed: recast got no answer".to_string(),
                        };
                    }
                    Err(e) => {
                        out.cooldown_probe = format!("probe failed: {e:#}");
                    }
                }
                let _ = self.reset_cooldowns().await;
            }
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
        let target_self = out.target_kind == "self";
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
        // `SCSkillFired.computed_delay` is the wire's `delay / 10 + 10`, so it quantises to
        // 10 ms; only a delay more than one step below `effect_delay` is a deviation.
        if plan.effect_delay > 0 {
            if let Some(delay) = attempt.fired_delay_ms {
                if delay < plan.effect_delay - 10 {
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
        }
        // End timing: the server ends a non-plot cast `computed_delay` (effect delay + the
        // `combat_sync_time` of the fire animation, `Skill.ScheduleEffects`) after
        // `SCSkillFired`, plus the channel for a channeled skill (`end_skill` after the
        // channel). The gap is compared, not the absolute end. A plot skill ends on the plot
        // timeline (`SCPlotEnded`), which the data does not describe in `skills`.
        if plan.plot_id == 0 {
            if let (Some(fired), Some(ended)) = (attempt.fired_ms, attempt.ended_ms) {
                let gap = ended.saturating_sub(fired) as i64;
                let effect = i64::from(attempt.fired_delay_ms.unwrap_or(0).max(0));
                let channel = i64::from(plan.channeling_time.max(0));
                let expected = effect + channel;
                let tolerance = 150 + expected / 10;
                if (gap - expected).abs() > tolerance {
                    out.mismatches.push(format!(
                        "ended {} ms after SCSkillFired vs computed_delay {} ms + channel {} ms",
                        gap, effect, channel
                    ));
                }
            }
        }
        // Mana. A `RestoreManaEffect` makes the net delta a restore, not the cost, and a plot
        // `ManaCost` is only paid when its event ran -- both are notes.
        if plan.mana_restore {
            if let Some(spent) = out.mana_spent {
                out.notes.push(format!(
                    "skill restores mana (RestoreManaEffect); MP change {spent} not compared"
                ));
            }
        } else if plan.plot_id != 0 {
            let reached = plan
                .plot_mana_events
                .iter()
                .any(|event| out.plot_event_ids.contains(event));
            if !plan.plot_mana_events.is_empty() && !reached {
                out.notes.push(format!(
                    "plot ManaCost event(s) {:?} not reached; MP change {:?} not compared",
                    plan.plot_mana_events, out.mana_spent
                ));
            } else if let Some(spent) = out.mana_spent {
                let expected = expected_mana_cost(plan, self.state.raised_level as i32);
                let tolerance = 5.max(expected / 5);
                if (spent - expected).abs() > tolerance {
                    out.mismatches.push(format!(
                        "mana cost {} vs observed MP loss {} ({})",
                        expected, spent, out.mana_source
                    ));
                }
            }
        } else if let Some(spent) = out.mana_spent {
            let expected = expected_mana_cost(plan, self.state.raised_level as i32);
            let tolerance = 5.max(expected / 5);
            if (spent - expected).abs() > tolerance {
                out.mismatches.push(format!(
                    "mana cost {} vs observed MP loss {} ({})",
                    expected, spent, out.mana_source
                ));
            }
        }
        // Buffs: only rows the server would run are demanded. The level filter ran at load; a
        // tag / chance / weight gated row, a row whose relation excludes the actual target and
        // a plot-dependent row whose event the plot never reached are notes, not mismatches.
        // A plot skill (`plot_id`) never runs `skill_effects`: only buffs its plot carries can
        // be demanded, and only when the carrying event ran.
        let applied: HashSet<u32> = out
            .buffs_created_caster
            .iter()
            .chain(out.buffs_created_target.iter())
            .copied()
            .collect();
        for row in plan.effects.buff_rows_for(target_self) {
            let side = if row.is_target_side() {
                "target"
            } else {
                "caster"
            };
            let applied_here = applied.contains(&row.buff_id);
            let plot_reached = row
                .plot_events
                .iter()
                .any(|event| out.plot_event_ids.contains(event));
            if plan.plot_id != 0 {
                if applied_here {
                    continue;
                }
                if !row.plot_events.is_empty() && plot_reached {
                    out.mismatches.push(format!(
                        "the plot reached buff {}'s event(s) {:?} but the {side} buff was not applied",
                        row.buff_id, row.plot_events
                    ));
                } else {
                    out.notes.push(format!(
                        "{side} buff {} not applied ({})",
                        row.buff_id,
                        row.gate_note()
                    ));
                }
                continue;
            }
            if row.unconditional(target_self) {
                if !applied_here {
                    out.mismatches.push(format!(
                        "expected {side} buff {} was not applied during the cast",
                        row.buff_id
                    ));
                }
                continue;
            }
            if applied_here {
                continue;
            }
            out.notes.push(format!(
                "{side} buff {} not applied ({})",
                row.buff_id,
                row.gate_note()
            ));
        }
        // Damage / heal rows vs the packets the cast produced on other units.
        let all_damage: Vec<&DamageRow> = plan.effects.damage_rows_for(target_self);
        let damage_rows: Vec<&DamageRow> = all_damage
            .iter()
            .copied()
            .filter(|r| r.damage_type != 5)
            .collect();
        let heal_rows: Vec<&HealRow> = plan.effects.heal_rows_for(target_self);
        let heal_damage: Vec<&DamageRow> = all_damage
            .iter()
            .copied()
            .filter(|r| r.damage_type == 5)
            .collect();
        let damage_count = damage_rows.len();
        let heal_count = heal_rows.len() + heal_damage.len();
        let unconditional_damage = damage_rows
            .iter()
            .filter(|r| r.unconditional(target_self))
            .count();
        let unconditional_heal = heal_rows
            .iter()
            .filter(|r| r.unconditional(target_self))
            .count();
        if plan.plot_id != 0 {
            // A plot skill never runs `skill_effects`; the plot tree decides what happens, so
            // its rows are not an expectation.
            if (damage_count > 0 || heal_count > 0) && out.damage_packets == 0 {
                out.notes.push(format!(
                    "plot skill: {} damage / {} heal skill_effects row(s) never run (the plot tree carries the effects)",
                    damage_count, heal_count
                ));
            }
        } else if let Some(delta) = out.target_hp_delta {
            if damage_count > 0 && delta >= 0 {
                if out.damage_packets == 0 {
                    if target_self {
                        out.notes.push(format!(
                            "{damage_count} damage row(s) on the caster itself: `CanAttack` refuses self (delta {delta})"
                        ));
                    } else if unconditional_damage > 0 {
                        out.mismatches.push(format!(
                            "{unconditional_damage} unconditional damage row(s) exist but no SCUnitDamagedPacket and no target HP loss (delta {delta})"
                        ));
                    } else {
                        out.notes.push(format!(
                            "{damage_count} damage row(s) exist, no damage packet and no HP loss (delta {delta}): the row's gates were not met"
                        ));
                    }
                } else if out.dealt_damage == 0 {
                    if out.miss_packets > 0 {
                        out.notes.push(format!(
                            "{} of {} damage packet(s) missed (hit type); target HP delta {}",
                            out.miss_packets, out.damage_packets, delta
                        ));
                    } else {
                        out.notes.push(format!(
                            "{} damage packet(s) all absorbed (0 damage); target HP delta {}",
                            out.damage_packets, delta
                        ));
                    }
                }
            }
            if damage_count == 0 && heal_count == 0 && delta != 0 {
                if !applied.is_empty() || !plan.effects.buff_rows_for(target_self).is_empty() {
                    out.notes.push(format!(
                        "target HP delta {delta} attributed to the buff effects of the cast"
                    ));
                } else if !plan.effects.other.is_empty() {
                    out.notes.push(format!(
                        "target HP delta {delta} attributed to the skill's other effects: {}",
                        plan.effects
                            .other
                            .iter()
                            .map(|(kind, n)| format!("{kind} x{n}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                } else {
                    out.mismatches.push(format!(
                        "target HP delta {} without a damage_effects / heal_effects row",
                        delta
                    ));
                }
            }
            if heal_count > 0 && delta < 0 && unconditional_heal > 0 {
                out.mismatches.push(format!(
                    "{unconditional_heal} unconditional heal row(s) exist but target HP delta is {delta}"
                ));
            }
            if target_self && damage_count > 0 && delta == 0 && out.self_damage_packets > 0 {
                out.notes.push(format!(
                    "self-target damage suppressed by godmode ({} SCUnitDamagedPacket(s) on the caster)",
                    out.self_damage_packets
                ));
            }
        }
        // Channel ticks.
        if plan.channeling_time > 0 && plan.channeling_tick > 0 && damage_count > 0 {
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
        // Cooldown (only when the probe ran; see `run_one` for the skip rules).
        if plan.cooldown_time > 0 && out.cooldown_probe.starts_with("probed") {
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
            out.notes.push(format!(
                "effect_repeat_count {} x {} ms (tick packets are not separable on the wire)",
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
            weapon_used: 0,
            dummy_distance_m: None,
            target: String::new(),
            target_kind: String::new(),
            target_type: plan.target_type,
            target_type_name: plan.target_type_name().to_string(),
            casting_time_ms: plan.casting_time,
            channeling_time_ms: plan.channeling_time,
            channeling_tick_ms: plan.channeling_tick,
            effect_delay_ms: plan.effect_delay,
            cooldown_time_ms: plan.cooldown_time,
            mana_cost_expected: expected_mana_cost(plan, self.state.raised_level as i32),
            expected_buffs: plan.effects.expected_buffs(),
            damage_rows: plan.effects.damage_rows.clone(),
            heal_rows: plan.effects.heal_rows.clone(),
            plot_id: plan.plot_id,
            plot_only: plan.plot_only,
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
            mana_source: String::new(),
            target_hp_delta: None,
            damage_packets: 0,
            miss_packets: 0,
            dealt_damage: 0,
            self_damage_packets: 0,
            heal_packets: 0,
            buffs_created_caster: Vec::new(),
            buffs_created_target: Vec::new(),
            buffs_removed_caster: Vec::new(),
            buffs_removed_target: Vec::new(),
            plot_events: 0,
            plot_event_ids: Vec::new(),
            unit_points_updates_caster: 0,
            unit_points_updates_target: 0,
            recast_result: None,
            cooldown_probe: String::new(),
            messages: Vec::new(),
            mismatches: Vec::new(),
            notes: Vec::new(),
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
    /// The caster's `SCUnitPointsPacket.mp` values, in arrival order, during the cast.
    mp_self: Vec<i32>,
    /// The first `mp_self` value that arrived after the cast's first response packet.
    mp_after_response: Option<i32>,
    /// `SCPlotEventPacket.event_id`s of the cast.
    plot_event_ids: Vec<u32>,
    damage_hits: Vec<DamageHit>,
    heal_hits: Vec<HealHit>,
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
                    target_selection: 0,
                    target_area_radius: 0,
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
                    plot_only: false,
                    plot_mana_cost: None,
                    plot_mana_events: Vec::new(),
                    mana_restore: false,
                    requires_no_combat: false,
                    resets_cooldown: false,
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
    use crate::craftrun::EquipmentMove;
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
        assert_eq!(
            parse_plot_event(&ev),
            Some(PlotEvent {
                tl: 7,
                event_id: 1,
                skill_id: 11166
            })
        );
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
            target_selection: 0,
            target_area_radius: 0,
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
            plot_only: false,
            plot_mana_cost: None,
            plot_mana_events: Vec::new(),
            mana_restore: false,
            requires_no_combat: false,
            resets_cooldown: false,
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
            target_selection: 0,
            target_area_radius: 0,
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
            plot_only: false,
            plot_mana_cost: None,
            plot_mana_events: Vec::new(),
            mana_restore: false,
            requires_no_combat: false,
            resets_cooldown: false,
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

    /// Opens the game's `compact.sqlite3` for the data-driven tests: `AAEMU_COMPACT_DB` first,
    /// then the two default locations. A placeholder file (a fresh lane worktree has a 0-byte
    /// `compact.sqlite3`) is skipped.
    fn test_compact_db() -> Option<SkillDb> {
        let candidates = [
            std::env::var("AAEMU_COMPACT_DB").ok(),
            Some(DEFAULT_COMPACT_DB.to_string()),
            Some(DEFAULT_COMPACT_DB_FROM_RUST.to_string()),
        ];
        for candidate in candidates.into_iter().flatten() {
            let path = PathBuf::from(&candidate);
            let real = std::fs::metadata(&path)
                .map(|m| m.len() > 1_000_000)
                .unwrap_or(false);
            if real {
                return SkillDb::open(&path).ok();
            }
        }
        None
    }

    #[test]
    fn skill_db_loads_the_running_game_data_when_present() {
        let Some(db) = test_compact_db() else {
            return;
        };
        let all = db.all();
        assert_eq!(all.len(), 170);
        // Ability 1 (Fight) has 21 skills; 10644 (Ground Smash) wants a mainhand weapon.
        assert_eq!(db.by_ability(1).len(), 21);
        let smash = db.skill(10644).unwrap();
        assert_eq!(smash.target_type, 0);
        assert_eq!(smash.target_relation, 4);
        assert!(smash.reqs.iter().any(|r| r.kind == 8 && r.value1 == 15));
        assert!(smash.effects.buff_rows.iter().any(|r| r.buff_id == 240));
        // 19050 (a hostile strike) carries a damage row.
        assert!(db
            .skill(19050)
            .unwrap()
            .effects
            .damage_rows
            .iter()
            .any(|r| r.damage_type != 0));
        // 10375 (Impenetrable Defense, Defense) applies the level-50..99 BuffEffect rows to the
        // caster; the level-30..39 rows (buff 328) are filtered out at caster level 55.
        let defense = db.skill(10375).unwrap();
        let expected = defense.effects.expected_buffs();
        assert!(expected.contains(&330) && expected.contains(&2208));
        assert!(!expected.contains(&328));
        // 10488 (Play Dead) is a 60 s channel.
        assert_eq!(db.skill(10488).unwrap().channeling_time, 60_000);
        // 10546 (Resurrect) needs a dead target.
        assert!(db.skill(10546).unwrap().target_dead);
        // 10694 (Glide) is the one toggle skill of the ten abilities.
        assert_eq!(db.skill(10694).unwrap().toggle_buff_id, 181);
    }

    #[test]
    fn plot_only_skills_take_the_plots_mana_cost() {
        let Some(db) = test_compact_db() else {
            return;
        };
        // 10644 (Ground Smash): plot_only, plot 649's ManaCost is 530 / 6.35 = 83.46 -> 83 (the
        // server truncates per `ManaCost` apply). The `skills` formula would be 411.
        let smash = db.skill(10644).unwrap();
        assert!(smash.plot_only);
        assert_eq!(smash.plot_mana_cost, Some(83));
        assert_eq!(expected_mana_cost(smash, 55), 83);
        assert_ne!(mana_cost_expected(smash, 55), 83);
        // 14810: plot 1078, ManaCost = 24 + 0.
        assert_eq!(expected_mana_cost(db.skill(14810).unwrap(), 55), 24);
        // 12759 (plot 623): ManaCost = 300 / 6.35 -> 47 from the caster (source 1).
        assert_eq!(expected_mana_cost(db.skill(12759).unwrap(), 55), 47);
        // 12345 is flagged `plot_only` but has no `plot_id`: the server runs the normal cast,
        // so the `Skill.ManaCost` formula applies.
        let no_plot = db.skill(12345).unwrap();
        assert!(no_plot.plot_only && no_plot.plot_id == 0);
        assert_eq!(
            expected_mana_cost(no_plot, 55),
            mana_cost_expected(no_plot, 55)
        );
        // A non-plot skill keeps the `Skill.ManaCost` formula.
        let basic = db.skill(10372).unwrap();
        assert!(!basic.plot_only);
        assert_eq!(expected_mana_cost(basic, 55), mana_cost_expected(basic, 55));
    }

    #[test]
    fn buff_rows_carry_their_gates() {
        let Some(db) = test_compact_db() else {
            return;
        };
        // 18131's buff 3842 row is `end_level 1`: the level filter drops it for a level-55
        // caster (`Skill.Cast` skips rows outside the caster's level range).
        let skill = db.skill(18131).unwrap();
        assert!(skill.effects.buff_rows.iter().all(|r| r.buff_id != 3842));
        // 23587's buff 7543 row needs the source buff tag 1476.
        let skill = db.skill(23587).unwrap();
        let row = skill
            .effects
            .buff_rows
            .iter()
            .find(|r| r.buff_id == 7543)
            .unwrap();
        assert_eq!(row.source_buff_tag_id, 1476);
        assert!(!row.unconditional(true));
        // 11973's buff 821 row needs the target buff tag 155.
        let skill = db.skill(11973).unwrap();
        let row = skill
            .effects
            .buff_rows
            .iter()
            .find(|r| r.buff_id == 821)
            .unwrap();
        assert_eq!(row.target_buff_tag_id, 155);
        assert!(!row.unconditional(true));
        // 18955's buff rows are hostile-only: they cannot apply to the caster itself.
        let skill = db.skill(18955).unwrap();
        let row = skill
            .effects
            .buff_rows
            .iter()
            .find(|r| r.buff_id == 4210)
            .unwrap();
        assert!(!row.applies_to(true));
        // 10710 (plot_only, plot 580): the plot carries buff 5932 (as a new plot-dependent row);
        // the `skill_effects` buff 797 row (source tag 108) is not in the plot tree.
        let skill = db.skill(10710).unwrap();
        assert!(skill.plot_only);
        let plot_buff = skill
            .effects
            .buff_rows
            .iter()
            .find(|r| r.buff_id == 5932)
            .unwrap();
        assert_eq!(plot_buff.plot_events, vec![7533]);
        assert!(!plot_buff.unconditional(true));
        let tag_buff = skill
            .effects
            .buff_rows
            .iter()
            .find(|r| r.buff_id == 797)
            .unwrap();
        assert_eq!(tag_buff.source_buff_tag_id, 108);
        assert!(tag_buff.plot_events.is_empty());
    }

    #[test]
    fn equipment_moves_resolve_templates_from_seen_items() {
        let mut session = SkillSession::default();
        // `/item add self` created the sword in the bag (a create row in another container).
        session.apply_equipment_changes(EquipmentChanges {
            templates: vec![(0x11, 5569)],
            ..Default::default()
        });
        // Bag slot 0 -> equipment slot 15 with an empty target (`to_item_id` 0): the moved item
        // is `from_item_id`, not the target's previous id.
        session.apply_equipment_changes(EquipmentChanges {
            moves: vec![EquipmentMove {
                from_slot_type: 2,
                from_slot: 0,
                from_item_id: 0x11,
                to_slot_type: SLOT_TYPE_EQUIPMENT,
                to_slot: SLOT_MAINHAND,
                to_item_id: 0,
            }],
            ..Default::default()
        });
        assert_eq!(
            session.equipment.get(&SLOT_MAINHAND).map(|i| i.template_id),
            Some(5569)
        );
        // Swap the sword out for the staff (both slots occupied).
        session.apply_equipment_changes(EquipmentChanges {
            templates: vec![(0x22, SPELL_WEAPON_TEMPLATE)],
            moves: vec![EquipmentMove {
                from_slot_type: 2,
                from_slot: 1,
                from_item_id: 0x22,
                to_slot_type: SLOT_TYPE_EQUIPMENT,
                to_slot: SLOT_MAINHAND,
                to_item_id: 0x11,
            }],
            ..Default::default()
        });
        assert_eq!(
            session.equipment.get(&SLOT_MAINHAND).map(|i| i.template_id),
            Some(SPELL_WEAPON_TEMPLATE)
        );
        // The staff leaves the mainhand for the bag.
        session.apply_equipment_changes(EquipmentChanges {
            removed_slots: vec![SLOT_MAINHAND],
            moves: vec![EquipmentMove {
                from_slot_type: SLOT_TYPE_EQUIPMENT,
                from_slot: SLOT_MAINHAND,
                from_item_id: 0x22,
                to_slot_type: 2,
                to_slot: 1,
                to_item_id: 0,
            }],
            ..Default::default()
        });
        assert!(!session.equipment.contains_key(&SLOT_MAINHAND));
    }

    #[test]
    fn mana_is_measured_at_the_cost_update() {
        // Instant cast: the cost update arrives before the response, so the lowest update of
        // the cast is the measurement.
        assert_eq!(
            measure_mana(Some(100), &[80, 95], None),
            (Some(20), "lowest update during the cast".to_string())
        );
        // Plot / channel: the first update after the response.
        assert_eq!(
            measure_mana(Some(100), &[100, 90, 95], Some(90)),
            (Some(10), "first update after the cast response".to_string())
        );
        // A plot pays before its event batch: the larger earlier drop is the cost.
        assert_eq!(
            measure_mana(Some(100), &[53], Some(96)),
            (Some(47), "largest MP drop during the cast".to_string())
        );
        // No update at all: nothing to compare.
        assert_eq!(measure_mana(Some(100), &[], None).0, None);
    }

    #[test]
    fn buff_row_gates_follow_the_target_relation() {
        let mut row = BuffRow {
            buff_id: 1,
            application_method: 1,
            friendly: true,
            non_friendly: false,
            source_buff_tag_id: 0,
            target_buff_tag_id: 0,
            source_no_buff_tag_id: 0,
            target_no_buff_tag_id: 0,
            chance: 100,
            weight: 0,
            plot_events: Vec::new(),
        };
        // A friendly-only target row applies on the bot, not on the hostile dummy.
        assert!(row.applies_to(true));
        assert!(!row.applies_to(false));
        assert!(row.unconditional(true));
        assert!(!row.unconditional(false));
        // A both-ways row applies to either.
        row.friendly = true;
        row.non_friendly = true;
        assert!(row.unconditional(true));
        assert!(row.unconditional(false));
        // A hostile-only row cannot apply to the caster (source side is always the caster).
        row.application_method = 3;
        row.friendly = false;
        assert!(!row.applies_to(true));
        // A tag-gated row is never demanded unconditionally.
        row.application_method = 1;
        row.friendly = true;
        row.non_friendly = true;
        row.target_buff_tag_id = 155;
        assert!(!row.unconditional(true));
        assert!(row.gate_note().contains("155"));
    }
}
