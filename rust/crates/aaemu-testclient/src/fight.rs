//! Fight runner (`aaemu-testclient fight`): a player-mode combat loop.
//!
//! It picks the nearest live NPC of the configured template ids (`npcs.LEVEL` is respected: a
//! target more than `max_level_delta` levels above the character is never attacked), closes in
//! through the [`Approach`] callback (see [`DirectApproach`]; the parallel walker replaces it),
//! and casts the character's own learned class skills with a scheduler that tracks the global
//! cooldown / skill cooldowns from `SCSkillStarted` / `SCSkillFired` / `SCSkillEnded`, gates
//! casts on mana (`SCUnitPoints`), drinks a configured potion or retreats to rest at low HP, and
//! loots the corpse (`CSLootOpenBagPacket(lootAll)` / `CSLootCloseBagPacket`).
//!
//! Everything comes from real client packets: skills are learned with `CSLearnSkillPacket`
//! (skill points from `levels.skill_points` decide what the server accepts), casts are
//! `CSStartSkillPacket`, and a death is answered with `CSResurrectCharacterPacket` (temple or
//! in place). No god mode and no `/kill` are used; the only GM command is `/move`, and only
//! inside [`DirectApproach`] (the walker replaces it).
//!
//! `SCUnitDeathPacket` of a unit this bot engaged is a kill, `SCExpChangedPacket` deltas are the
//! XP gained / lost, `SCLevelChangedPacket` tracks the level, and the `SCItemTaskSuccessPacket`
//! create rows of the loot window are the loot. The run ends when the requested kill count is
//! reached and returns a JSON [`FightReport`].

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use aaemu_game::opcodes::{cs, sc};
use aaemu_game::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};
use anyhow::{bail, Context};
use futures::future::BoxFuture;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use crate::compactdb::{DEFAULT_COMPACT_DB, DEFAULT_COMPACT_DB_FROM_RUST};
use crate::craftrun::{parse_state_equipment, RunCharacter};
use crate::game::{parse_item_task, GameClient, Packet};
use crate::login::LoginClient;
use crate::record::Recorder;
use crate::scenario::BotConfig;

/// Packet-pump slice for every bounded wait in the combat loop.
const PUMP: Duration = Duration::from_millis(150);
/// Quiet window after a cast action (the response packets may arrive in a batch).
const CAST_SETTLE: Duration = Duration::from_millis(250);
/// How long a sent cast may stay unanswered before the scheduler retries it.
const CAST_RESPONSE_TIMEOUT_MS: u64 = 1500;
/// Retry delay after a cast the server answered with a rejection (`SkillResult`).
const REJECT_RETRY_MS: u64 = 1500;
/// Retry delay after a cast the server answered with `SkillResult` cooldown (8): the global
/// cooldown the server set may outlast a plain rejection back-off.
const COOLDOWN_RETRY_MS: u64 = 2000;
/// Retry delay after a sent cast that never got a response.
const SILENT_RETRY_MS: u64 = 500;
/// Server GCD for a player cast of a `default_gcd` skill (`systems/skills/skill.rs::cast`).
const GCD_DEFAULT_MS: u64 = 1000;
/// `SkillResult` cooldown (8, `skill_cooldown_time`).
const SKILL_RESULT_COOLDOWN: u8 = 8;
/// Below this share of the maximum HP seen a potion is drunk (when one is configured).
const POTION_HP_PCT: f32 = 0.45;
/// Below this HP share the bot leaves the fight and rests.
const RETREAT_HP_PCT: f32 = 0.25;
/// Resting ends when the HP share is back above this.
const REST_HP_PCT: f32 = 0.6;
/// How long a rest may take before the runner gives up on it and fights on.
const REST_TIMEOUT: Duration = Duration::from_secs(45);
/// Distance of the resting spot from the threat (approached away).
const REST_DISTANCE: f32 = 22.0;
/// Potion re-use delay (the real potion cooldowns are far longer; this stops spam).
const POTION_COOLDOWN: Duration = Duration::from_secs(10);
/// How long a corpse's loot window is left open before it is closed again.
const LOOT_WINDOW: Duration = Duration::from_millis(1500);
/// Quiet window after the last loot item task.
const LOOT_QUIET: Duration = Duration::from_millis(400);
/// How far from the target the fight stands (near melee range even for long-ranged skills).
const MELEE_STAND_RANGE: f32 = 3.2;
/// Slack over the skill range before a target is re-approached.
const RANGE_SLACK: f32 = 0.5;

// -------------------------------------------------------------------------------------------
// Options and report
// -------------------------------------------------------------------------------------------

/// A combat step: one target engagement's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Engage {
    /// The target died (its `SCUnitDeathPacket` arrived).
    Killed,
    /// The character died during the engagement.
    PlayerDead,
    /// The engagement budget ended the fight, or the target vanished.
    Timeout,
}

/// The fight-runner settings (`aaemu-testclient fight`).
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// NPC template ids to hunt (`--tpl`, repeatable).
    pub template_ids: Vec<u32>,
    /// Requested kill count (`--kills`).
    pub kills: u32,
    /// Where the bot waits for targets when none is in sight (`--at x,y,z`, repeatable).
    pub hunt_points: Vec<(f32, f32, f32)>,
    /// Highest accepted target level above the character's own (`--max-level-delta`).
    pub max_level_delta: u8,
    /// Bag item template used at low HP when present (`--potion`).
    pub potion: Option<u32>,
    /// Revive in place (`--resurrect-in-place`) instead of at the return portal.
    pub resurrect_in_place: bool,
    /// Abort the run after this many deaths (`--max-deaths`).
    pub max_deaths: u32,
    /// Abort the run after this many consecutive engagement timeouts without a kill
    /// (`--max-timeouts`); an unkillable target must not spin forever.
    pub max_timeouts: u32,
    /// Overall budget of the run (`--run-timeout-secs`).
    pub run_timeout: Duration,
    /// Create a fresh character instead of selecting the account's (`--new-character`).
    pub new_character: bool,
    pub race: u8,
    pub gender: u8,
    /// Acquisition budget: how long the runner looks for a target before giving up.
    pub acquire_timeout: Duration,
    /// Engagement budget per target.
    pub combat_timeout: Duration,
    pub compact_db: Option<PathBuf>,
    /// JSON report destination (`--out`).
    pub out: Option<PathBuf>,
    /// Packet capture destination (`--capture`).
    pub capture: Option<PathBuf>,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            template_ids: Vec::new(),
            kills: 10,
            hunt_points: Vec::new(),
            max_level_delta: 3,
            potion: None,
            resurrect_in_place: true,
            max_deaths: 3,
            max_timeouts: 5,
            run_timeout: Duration::from_secs(1800),
            new_character: false,
            race: 1,
            gender: 1,
            acquire_timeout: Duration::from_secs(60),
            combat_timeout: Duration::from_secs(90),
            compact_db: None,
            out: None,
            capture: None,
        }
    }
}

/// One looted item (template and amount), with its data name when the compact database has it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LootEntry {
    pub item_id: u32,
    pub name: String,
    pub count: i64,
}

/// What one fight run did.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FightReport {
    pub character: String,
    pub race: u8,
    pub gender: u8,
    pub start_level: u8,
    pub end_level: u8,
    pub requested_kills: u32,
    pub kills: u32,
    pub kills_by_template: BTreeMap<u32, u32>,
    pub deaths: u32,
    pub resurrects: u32,
    pub xp_gained: i64,
    pub xp_lost: i64,
    /// Skill id -> successful casts (`SCSkillEnded` after a start / fired).
    pub skills_used: BTreeMap<u32, u32>,
    /// Skill id -> name for the skills the scheduler knew.
    pub skill_names: BTreeMap<u32, String>,
    pub learned_skills: Vec<u32>,
    pub loot: Vec<LootEntry>,
    pub loot_money: i64,
    /// Notes in order (retreats, rests, loot windows, rejections, level-ups, ...).
    pub events: Vec<String>,
    /// Engagements that ran out of their budget or lost their target.
    pub timeouts: u32,
    pub aborted: bool,
}

impl FightReport {
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "[fight] {} ({} lvl {} -> {}) {}/{} kills, {} deaths, {} resurrects, {} timeouts\n",
            self.character,
            race_name(self.race),
            self.start_level,
            self.end_level,
            self.kills,
            self.requested_kills,
            self.deaths,
            self.resurrects,
            self.timeouts
        ));
        out.push_str(&format!(
            "[fight] xp +{} (-{})  loot money {}{}\n",
            self.xp_gained,
            self.xp_lost,
            self.loot_money,
            if self.aborted { "  ABORTED" } else { "" }
        ));
        if !self.kills_by_template.is_empty() {
            out.push_str(&format!(
                "[fight] kills by template: {}\n",
                self.kills_by_template
                    .iter()
                    .map(|(tpl, n)| format!("{tpl} x{n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !self.skills_used.is_empty() {
            out.push_str(&format!(
                "[fight] casts: {}\n",
                self.skills_used
                    .iter()
                    .map(|(id, n)| format!(
                        "{} ({}) x{n}",
                        id,
                        self.skill_names.get(id).map_or("?", String::as_str)
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !self.loot.is_empty() {
            out.push_str(&format!(
                "[fight] loot: {}\n",
                self.loot
                    .iter()
                    .map(|l| format!("{} ({}) x{}", l.item_id, l.name, l.count))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        for e in &self.events {
            out.push_str(&format!("[fight]   {e}\n"));
        }
        out
    }
}

fn race_name(race: u8) -> &'static str {
    match race {
        1 => "Nuian",
        2 => "Elf",
        3 => "Dwarf",
        4 => "Firran",
        5 => "Harani",
        _ => "?",
    }
}

/// Parses an `x,y,z` hunt point (`--at`).
pub fn parse_point(s: &str) -> anyhow::Result<(f32, f32, f32)> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 3 {
        bail!("'{s}' is not an x,y,z point");
    }
    let v: Vec<f32> = parts
        .iter()
        .map(|p| {
            p.trim()
                .parse::<f32>()
                .with_context(|| format!("'{s}' is not an x,y,z point"))
        })
        .collect::<anyhow::Result<_>>()?;
    Ok((v[0], v[1], v[2]))
}

// -------------------------------------------------------------------------------------------
// Compact database
// -------------------------------------------------------------------------------------------

/// One `skills` row as the fight runner needs it.
#[derive(Debug, Clone)]
pub struct FightSkillRow {
    pub id: u32,
    pub name: String,
    pub ability_id: u8,
    pub ability_level: i32,
    pub skill_points: i32,
    pub auto_learn: bool,
    pub need_learn: bool,
    pub show: bool,
    pub target_type: u8,
    pub target_relation: u8,
    pub min_range: f32,
    pub max_range: f32,
    pub cooldown_time: i32,
    pub mana_cost: i32,
    pub mana_level_md: f64,
    pub default_gcd: bool,
    pub custom_gcd: i32,
    pub ignore_global_cooldown: bool,
    pub target_dead: bool,
    pub plot_only: bool,
    /// `skills.plot_id` (0 = none): plot skills answer with `SCPlotEvent` / `SCPlotEnded`.
    pub plot_id: u32,
    pub toggle_buff_id: u32,
    /// Sum of `fixed_max + level_md * 10` over the skill's non-friendly damage rows (0 = no
    /// damage the runner can count on).
    pub damage_score: f64,
}

impl FightSkillRow {
    /// `Skill.ManaCost(caster)` scaled with the caster level (`systems/skills/skill.rs`).
    pub fn expected_mana(&self, level: u8) -> i32 {
        let base = (f64::from(level.saturating_sub(1)) * 1.6 + 8.0) * 3.0 / 3.65;
        (base * self.mana_level_md + f64::from(self.mana_cost)) as i32
    }

    /// The fight uses the row when its damage hits units: hostile-capable, not a dead-only or
    /// toggle-buff skill and with a non-friendly damage row (or the default auto-attack). A
    /// `plot_only` skill carries its damage in the plot tree (no `skill_effects` rows), so its
    /// plot id is enough.
    pub fn attacks_units(&self) -> bool {
        !self.target_dead
            && self.toggle_buff_id == 0
            && (self.target_type == 4 || self.target_relation == 4)
            && (self.damage_score > 0.0 || self.id == 2 || self.plot_id != 0)
    }

    /// `SkillData::finish`: `auto_learn && need_learn && ability_level <= 1 && show` skills of an
    /// ability are granted at character creation.
    pub fn start_ability_skill(&self) -> bool {
        self.auto_learn && self.need_learn && self.ability_level <= 1 && self.show
    }

    /// The scheduler view of the row.
    pub fn to_fight_skill(&self, level: u8) -> Option<FightSkill> {
        if !self.attacks_units() {
            return None;
        }
        let gcd_ms = if self.ignore_global_cooldown {
            0
        } else if self.default_gcd {
            GCD_DEFAULT_MS
        } else {
            self.custom_gcd.max(0) as u64
        };
        let auto_attack = self.id == 2;
        Some(FightSkill {
            id: self.id,
            name: self.name.clone(),
            mana_cost: self.expected_mana(level),
            cooldown_ms: self.cooldown_time.max(0) as u64,
            gcd_ms,
            min_range: self.min_range,
            max_range: if self.max_range > 0.0 {
                self.max_range
            } else {
                4.0
            },
            // Auto attacks are the fallback; a damage or plot skill always wins while usable.
            priority: if auto_attack {
                0
            } else if self.damage_score > 0.0 {
                100 + self.damage_score as i64
            } else {
                90
            },
            auto_attack,
            plot: self.plot_id != 0,
        })
    }
}

/// Read-only lookups from the same `compact.sqlite3` the game server loads.
pub struct FightDb {
    conn: rusqlite::Connection,
    path: PathBuf,
}

impl FightDb {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn =
            rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .with_context(|| format!("opening compact database {}", path.display()))?;
        Ok(Self {
            conn,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Opens `--compact-db` or the default game-data path when it exists.
    pub fn open_default(compact_db: Option<&Path>) -> anyhow::Result<Self> {
        if let Some(path) = compact_db {
            if !path.exists() {
                bail!("compact database {} not found", path.display());
            }
            return Self::open(path);
        }
        for candidate in [DEFAULT_COMPACT_DB, DEFAULT_COMPACT_DB_FROM_RUST] {
            let path = Path::new(candidate);
            if path.exists() {
                return Self::open(path);
            }
        }
        bail!(
            "no compact database found; pass --compact-db (tried {DEFAULT_COMPACT_DB}, \
             {DEFAULT_COMPACT_DB_FROM_RUST})"
        )
    }

    /// `npcs.LEVEL` and `npcs.faction_id`; `None` for a template the data does not have.
    pub fn npc(&self, template_id: u32) -> anyhow::Result<Option<(u8, u32)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT LEVEL, COALESCE(faction_id,0) FROM npcs WHERE id = ?1",
                [template_id],
                |r| Ok((r.get::<_, i64>(0)? as u8, r.get::<_, u32>(1)?)),
            )
            .optional()?)
    }

    /// Every class skill of ability 1..10 with the data the runner needs.
    pub fn ability_skills(&self) -> anyhow::Result<Vec<FightSkillRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, COALESCE(s.name,''), COALESCE(s.ability_id,0), \
             COALESCE(s.ability_level,0), COALESCE(s.skill_points,0), \
             CASE WHEN s.auto_learn='t' THEN 1 ELSE 0 END, \
             CASE WHEN s.need_learn='t' THEN 1 ELSE 0 END, \
             CASE WHEN s.show='t' THEN 1 ELSE 0 END, \
             COALESCE(s.target_type_id,0), COALESCE(s.target_relation_id,0), \
             COALESCE(s.min_range,0), COALESCE(s.max_range,0), \
             COALESCE(s.cooldown_time,0), COALESCE(s.mana_cost,0), COALESCE(s.mana_level_md,0.0), \
             CASE WHEN s.default_gcd='t' THEN 1 ELSE 0 END, COALESCE(s.custom_gcd,0), \
             CASE WHEN s.ignore_global_cooldown='t' THEN 1 ELSE 0 END, \
             CASE WHEN s.target_dead='t' THEN 1 ELSE 0 END, \
             CASE WHEN s.plot_only='t' THEN 1 ELSE 0 END, \
             COALESCE(s.plot_id,0), \
             COALESCE(s.toggle_buff_id,0), \
             COALESCE((SELECT SUM(de.fixed_max + de.level_md * 10) FROM skill_effects se \
                JOIN effects ef ON ef.id = se.effect_id \
                JOIN damage_effects de ON de.id = ef.actual_id \
                WHERE se.skill_id = s.id AND ef.actual_type = 'DamageEffect' \
                  AND se.non_friendly = 't'), 0.0) \
             FROM skills s WHERE s.ability_id BETWEEN 1 AND 10",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(FightSkillRow {
                id: r.get(0)?,
                name: r.get(1)?,
                ability_id: r.get(2)?,
                ability_level: r.get(3)?,
                skill_points: r.get(4)?,
                auto_learn: r.get::<_, i64>(5)? != 0,
                need_learn: r.get::<_, i64>(6)? != 0,
                show: r.get::<_, i64>(7)? != 0,
                target_type: r.get(8)?,
                target_relation: r.get(9)?,
                min_range: f64::from(r.get::<_, i32>(10)?) as f32,
                max_range: r.get::<_, i32>(11)? as f32,
                cooldown_time: r.get(12)?,
                mana_cost: r.get(13)?,
                mana_level_md: r.get(14)?,
                default_gcd: r.get::<_, i64>(15)? != 0,
                custom_gcd: r.get(16)?,
                ignore_global_cooldown: r.get::<_, i64>(17)? != 0,
                target_dead: r.get::<_, i64>(18)? != 0,
                plot_only: r.get::<_, i64>(19)? != 0,
                plot_id: r.get(20)?,
                toggle_buff_id: r.get(21)?,
                damage_score: r.get(22)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// `items.use_skill_id` of an item template (0 / NULL -> `None`).
    pub fn item_use_skill(&self, item_template: u32) -> anyhow::Result<Option<u32>> {
        let skill = self
            .conn
            .query_row(
                "SELECT COALESCE(use_skill_id,0) FROM items WHERE id = ?1",
                [item_template],
                |r| r.get::<_, u32>(0),
            )
            .optional()?;
        Ok(skill.filter(|id| *id != 0))
    }

    /// `items.name` of a template.
    pub fn item_name(&self, item_template: u32) -> anyhow::Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT COALESCE(name,'') FROM items WHERE id = ?1",
                [item_template],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
    }

    /// `levels.skill_points` of a level (`ExperienceManager.SkillPointsForLevel`).
    pub fn skill_points_for_level(&self, level: u8) -> anyhow::Result<i32> {
        Ok(self
            .conn
            .query_row(
                "SELECT COALESCE(skill_points,0) FROM levels WHERE id = ?1",
                [level],
                |r| r.get::<_, i32>(0),
            )
            .optional()?
            .unwrap_or(0))
    }
}

// -------------------------------------------------------------------------------------------
// Cast scheduler (pure; unit tested below)
// -------------------------------------------------------------------------------------------

/// A learned skill the scheduler may cast.
#[derive(Debug, Clone, PartialEq)]
pub struct FightSkill {
    pub id: u32,
    pub name: String,
    pub mana_cost: i32,
    pub cooldown_ms: u64,
    pub gcd_ms: u64,
    pub min_range: f32,
    pub max_range: f32,
    /// Higher runs first (damage skills over the auto-attack fallback).
    pub priority: i64,
    pub auto_attack: bool,
    /// A plot skill (`skills.plot_id`): it answers with `SCPlotEvent` / `SCPlotEnded`, never
    /// with `SCSkillStarted` / `SCSkillFired`.
    pub plot: bool,
}

#[derive(Debug, Clone, Copy)]
struct SentCast {
    skill_id: u32,
    at: u64,
}

#[derive(Debug, Clone, Copy)]
struct InFlight {
    tl: u16,
    skill_id: u32,
}

/// The cast timeline of one character: cooldowns, the global cooldown and the one cast that may
/// be in flight, fed with millisecond timestamps (monotonic since the run started).
#[derive(Debug, Default)]
pub struct SkillScheduler {
    skills: Vec<FightSkill>,
    gcd_until: u64,
    cooldown_until: HashMap<u32, u64>,
    retry_after: HashMap<u32, u64>,
    sent: Option<SentCast>,
    in_flight: Option<InFlight>,
    /// Successful casts (`SCSkillEnded` after a start / fired).
    pub casts: u32,
    /// Casts the server rejected (non-zero `SkillResult`).
    pub rejections: u32,
}

impl SkillScheduler {
    pub fn new(mut skills: Vec<FightSkill>) -> Self {
        skills.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.id.cmp(&b.id)));
        Self {
            skills,
            ..Self::default()
        }
    }

    pub fn skills(&self) -> &[FightSkill] {
        &self.skills
    }

    pub fn skill(&self, id: u32) -> Option<&FightSkill> {
        self.skills.iter().find(|s| s.id == id)
    }

    pub fn gcd_until(&self) -> u64 {
        self.gcd_until
    }

    pub fn in_flight_skill(&self) -> Option<u32> {
        self.in_flight.map(|f| f.skill_id)
    }

    /// True while a cast was sent or is running: nothing else may be started.
    pub fn busy(&self) -> bool {
        self.sent.is_some() || self.in_flight.is_some()
    }

    /// Records that a `CSStartSkillPacket` was sent (the response is awaited).
    pub fn cast_sent(&mut self, skill_id: u32, now_ms: u64) {
        self.sent = Some(SentCast {
            skill_id,
            at: now_ms,
        });
    }

    /// Expires a sent cast the server never answered.
    pub fn tick(&mut self, now_ms: u64) {
        if let Some(sent) = self.sent {
            if now_ms.saturating_sub(sent.at) >= CAST_RESPONSE_TIMEOUT_MS {
                self.sent = None;
                self.retry_after
                    .insert(sent.skill_id, now_ms + SILENT_RETRY_MS);
            }
        }
    }

    /// `SCSkillStartedPacket` of our own cast: `result != 0` is the server's rejection.
    pub fn on_started(&mut self, skill_id: u32, tl: u16, result: u8, now_ms: u64) {
        if result != 0 {
            self.reject(skill_id, result, now_ms);
            return;
        }
        if self.sent.is_some_and(|s| s.skill_id == skill_id) {
            self.sent = None;
        }
        self.in_flight = Some(InFlight { tl, skill_id });
    }

    /// `SCSkillFiredPacket` of our own cast (an instant cast has no `SCSkillStarted`).
    pub fn on_fired(&mut self, skill_id: u32, tl: u16) {
        if self.in_flight.is_none() {
            if self.sent.is_some_and(|s| s.skill_id == skill_id) {
                self.sent = None;
            }
            self.in_flight = Some(InFlight { tl, skill_id });
        }
    }

    /// `SCPlotEventPacket` of our own cast (a plot skill's first answer).
    pub fn on_plot_event(&mut self, skill_id: u32, tl: u16) {
        self.on_fired(skill_id, tl);
    }

    /// `SCPlotEndedPacket`: the plot finished (cooldown and GCD start now).
    pub fn on_plot_ended(&mut self, tl: u16, now_ms: u64) -> Option<u32> {
        self.on_ended(tl, now_ms)
    }

    /// `SCSkillEndedPacket`: the cast resolved (cooldown and GCD start now). Returns the skill
    /// that resolved, so the caller can count the cast.
    pub fn on_ended(&mut self, tl: u16, now_ms: u64) -> Option<u32> {
        if let Some(f) = self.in_flight {
            if f.tl == tl || f.tl == 0 {
                self.resolve(f.skill_id, now_ms);
                return Some(f.skill_id);
            }
        }
        // A rejected cast's `stop()` sends the ended too; resolve the sent cast if it matches.
        if let Some(sent) = self.sent {
            self.resolve(sent.skill_id, now_ms);
            return Some(sent.skill_id);
        }
        None
    }

    /// `SCSkillStoppedPacket` / `SCCastingStoppedPacket`: the cast was cancelled.
    pub fn on_cancelled(&mut self, skill_id: Option<u32>, tl: Option<u16>, now_ms: u64) {
        let in_flight_matches = match (self.in_flight, skill_id, tl) {
            (Some(f), _, Some(tl)) => f.tl == tl,
            (Some(f), Some(id), None) => f.skill_id == id,
            _ => false,
        };
        let sent_matches = skill_id.is_some_and(|id| self.sent.is_some_and(|s| s.skill_id == id));
        if in_flight_matches {
            let id = self.in_flight.map_or(0, |f| f.skill_id);
            self.in_flight = None;
            self.retry_after.insert(id, now_ms + SILENT_RETRY_MS);
        }
        if sent_matches {
            let id = self.sent.map_or(0, |s| s.skill_id);
            self.sent = None;
            self.retry_after.insert(id, now_ms + SILENT_RETRY_MS);
        }
    }

    fn resolve(&mut self, skill_id: u32, now_ms: u64) {
        self.in_flight = None;
        self.sent = None;
        self.casts += 1;
        let (cd, gcd) = self
            .skill(skill_id)
            .map_or((0, 0), |s| (s.cooldown_ms, s.gcd_ms));
        self.cooldown_until.insert(skill_id, now_ms + cd);
        if gcd > 0 {
            self.gcd_until = now_ms + gcd;
        }
    }

    fn reject(&mut self, skill_id: u32, result: u8, now_ms: u64) {
        self.rejections += 1;
        if self.sent.is_some_and(|s| s.skill_id == skill_id) {
            self.sent = None;
        }
        if self.in_flight.is_some_and(|f| f.skill_id == skill_id) {
            self.in_flight = None;
        }
        let delay = if result == SKILL_RESULT_COOLDOWN {
            COOLDOWN_RETRY_MS
        } else {
            REJECT_RETRY_MS
        };
        self.retry_after.insert(skill_id, now_ms + delay);
    }

    /// The skill to cast now (`None` while a cast is in flight, on the GCD or without mana).
    pub fn next_skill(&self, now_ms: u64, mp: Option<i32>) -> Option<FightSkill> {
        if self.busy() || now_ms < self.gcd_until {
            return None;
        }
        for s in &self.skills {
            if now_ms < self.cooldown_until.get(&s.id).copied().unwrap_or(0) {
                continue;
            }
            if now_ms < self.retry_after.get(&s.id).copied().unwrap_or(0) {
                continue;
            }
            if s.mana_cost > 0 && mp.is_some_and(|mp| mp < s.mana_cost) {
                continue;
            }
            return Some(s.clone());
        }
        None
    }

    /// Milliseconds until some skill could be cast again (caps the pump slice).
    pub fn wait_ms(&self, now_ms: u64, mp: Option<i32>) -> u64 {
        if self.busy() {
            return 200;
        }
        let mut best = self.gcd_until.saturating_sub(now_ms);
        for s in &self.skills {
            let cd = self
                .cooldown_until
                .get(&s.id)
                .copied()
                .unwrap_or(0)
                .max(self.retry_after.get(&s.id).copied().unwrap_or(0));
            let affordable = s.mana_cost <= 0 || mp.is_none_or(|mp| mp >= s.mana_cost);
            if affordable {
                best = best.min(cd.saturating_sub(now_ms));
            }
        }
        best.min(1000)
    }
}

// -------------------------------------------------------------------------------------------
// Target choice (pure; unit tested below)
// -------------------------------------------------------------------------------------------

/// One candidate target of the fight loop.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FightTarget {
    pub obj_id: u32,
    pub template_id: u32,
    /// `npcs.LEVEL`; 0 = unknown (always allowed).
    pub level: u8,
    pub pos: (f32, f32, f32),
    /// Announced dead / zero HP.
    pub dead: bool,
}

/// The nearest live, in-level candidate that is not avoided. Ties go to the lower level, then
/// the lower object id, so the choice is deterministic.
pub fn choose_target(
    candidates: &[FightTarget],
    me: (f32, f32, f32),
    self_level: u8,
    max_level_delta: u8,
    avoid: &HashSet<u32>,
) -> Option<FightTarget> {
    let cap = self_level.saturating_add(max_level_delta);
    candidates
        .iter()
        .copied()
        .filter(|c| {
            !c.dead
                && (c.level == 0 || c.level <= cap)
                && !avoid.contains(&c.obj_id)
                && c.obj_id != 0
        })
        .min_by(|a, b| {
            dist2(a.pos, me)
                .total_cmp(&dist2(b.pos, me))
                .then(a.level.cmp(&b.level))
                .then(a.obj_id.cmp(&b.obj_id))
        })
}

fn dist2(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
    let (dx, dy, dz) = (a.0 - b.0, a.1 - b.1, a.2 - b.2);
    dx * dx + dy * dy + dz * dz
}

fn dist3(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
    dist2(a, b).sqrt()
}

// -------------------------------------------------------------------------------------------
// Approach
// -------------------------------------------------------------------------------------------

/// How the fight loop closes in on a point. The parallel walker replaces the default with its
/// own implementation; the fight code never sends movement packets itself.
pub trait Approach: Send {
    fn approach<'a>(
        &'a mut self,
        game: &'a mut GameClient,
        pos: (f32, f32, f32),
        face: Option<(f32, f32, f32)>,
    ) -> BoxFuture<'a, anyhow::Result<()>>;
}

/// Default approach: a `/move` (GM teleport) for the long haul, then one real `CSMoveUnitPacket`
/// (with a facing yaw) for the last stretch. `face` aims the final move at the unit.
#[derive(Debug, Default)]
pub struct DirectApproach;

impl Approach for DirectApproach {
    fn approach<'a>(
        &'a mut self,
        game: &'a mut GameClient,
        pos: (f32, f32, f32),
        face: Option<(f32, f32, f32)>,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let me = game.state.self_pos;
            if dist3(me, pos) > 14.0 {
                let dir = unit_vector(pos, me);
                let staging = (
                    pos.0 - dir.0 * 7.0,
                    pos.1 - dir.1 * 7.0,
                    pos.2 - dir.2 * 7.0,
                );
                game.teleport(staging).await?;
            }
            let now = game.state.self_pos;
            if dist3(now, pos) > 0.3 {
                match face {
                    Some(target) => game.move_to_facing(pos, target, 1200).await?,
                    None => game.move_to(pos, 1200).await?,
                }
            } else if let Some(target) = face {
                game.move_to_facing(now, target, 600).await?;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
            Ok(())
        })
    }
}

/// Normalized direction from `from` to `to` ((1, 0, 0) when they coincide).
fn unit_vector(to: (f32, f32, f32), from: (f32, f32, f32)) -> (f32, f32, f32) {
    let (dx, dy, dz) = (to.0 - from.0, to.1 - from.1, to.2 - from.2);
    let len = (dx * dx + dy * dy + dz * dz).sqrt();
    if len <= f32::EPSILON {
        (1.0, 0.0, 0.0)
    } else {
        (dx / len, dy / len, dz / len)
    }
}

// -------------------------------------------------------------------------------------------
// Packet readers
// -------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SkillStarted {
    skill_id: u32,
    tl: u16,
    caster_obj: u32,
    result: u8,
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
    s.read_u16().ok()?; // real cast time / 10
    s.read_u16().ok()?; // base cast time / 10
    s.read_u8().ok()?; // synergy
    let flags = s.read_u8().ok()?;
    let mut result = 0u8;
    if flags & 1 != 0 {
        result = s.read_u8().ok()?;
    }
    Some(SkillStarted {
        skill_id,
        tl,
        caster_obj: caster.obj_id(),
        result,
    })
}

/// `SCSkillFiredPacket` -> `(skill_id, tl, caster_obj)`.
fn parse_skill_fired(p: &Packet) -> Option<(u32, u16, u32)> {
    if !p.is_sc(sc::SCSkillFiredPacket) {
        return None;
    }
    let mut s = p.stream();
    let skill_id = s.read_u32().ok()?;
    let tl = s.read_u16().ok()?;
    let caster_kind = s.read_u8().ok()?;
    let caster = SkillCaster::read(caster_kind, &mut s).ok()?;
    Some((skill_id, tl, caster.obj_id()))
}

/// `SCPlotEventPacket` -> `(skill_id, tl)` (the plot skill's answer; the plots of NPC casts
/// carry their own skill ids, so the caller matches the skill it cast).
fn parse_plot_event(p: &Packet) -> Option<(u32, u16)> {
    if !p.is_sc(sc::SCPlotEventPacket) {
        return None;
    }
    let mut s = p.stream();
    let tl = s.read_u16().ok()?;
    s.read_u32().ok()?; // event id
    let skill_id = s.read_u32().ok()?;
    Some((skill_id, tl))
}

fn parse_plot_ended(p: &Packet) -> Option<u16> {
    if !p.is_sc(sc::SCPlotEndedPacket) {
        return None;
    }
    p.stream().read_u16().ok()
}

fn parse_plot_stopped(p: &Packet) -> Option<u16> {
    if !p.is_sc(sc::SCPlotCastingStoppedPacket) && !p.is_sc(sc::SCPlotChannelingStoppedPacket) {
        return None;
    }
    p.stream().read_u16().ok()
}

fn parse_skill_ended(p: &Packet) -> Option<u16> {
    if !p.is_sc(sc::SCSkillEndedPacket) {
        return None;
    }
    p.stream().read_u16().ok()
}

fn parse_skill_stopped(p: &Packet) -> Option<u32> {
    if !p.is_sc(sc::SCSkillStoppedPacket) {
        return None;
    }
    let mut s = p.stream();
    s.read_bc().ok()?;
    s.read_u32().ok()
}

fn parse_casting_stopped(p: &Packet) -> Option<u16> {
    if !p.is_sc(sc::SCCastingStoppedPacket) {
        return None;
    }
    p.stream().read_u16().ok()
}

/// `SCUnitPointsPacket` -> `(obj_id, hp, mp)`.
fn parse_unit_points(p: &Packet) -> Option<(u32, i32, i32)> {
    if !p.is_sc(sc::SCUnitPointsPacket) {
        return None;
    }
    let mut s = p.stream();
    let obj = s.read_bc().ok()?;
    Some((obj, s.read_i32().ok()? / 100, s.read_i32().ok()? / 100))
}

/// `SCUnitDeathPacket` -> the dead unit's object id.
fn parse_unit_death(p: &Packet) -> Option<u32> {
    if !p.is_sc(sc::SCUnitDeathPacket) {
        return None;
    }
    p.stream().read_bc().ok()
}

/// `SCExpChangedPacket` -> `(obj_id, exp)`.
fn parse_exp_changed(p: &Packet) -> Option<(u32, i32)> {
    if !p.is_sc(sc::SCExpChangedPacket) {
        return None;
    }
    let mut s = p.stream();
    let obj = s.read_bc().ok()?;
    Some((obj, s.read_i32().ok()?))
}

/// `SCLevelChangedPacket` -> `(obj_id, level)`.
fn parse_level_changed(p: &Packet) -> Option<(u32, u8)> {
    if !p.is_sc(sc::SCLevelChangedPacket) {
        return None;
    }
    let mut s = p.stream();
    let obj = s.read_bc().ok()?;
    Some((obj, s.read_u8().ok()?))
}

// -------------------------------------------------------------------------------------------
// The runner
// -------------------------------------------------------------------------------------------

/// Counters and notes of one run.
#[derive(Debug, Default)]
struct Facts {
    kills: u32,
    kills_by_template: BTreeMap<u32, u32>,
    deaths: u32,
    resurrects: u32,
    xp_gained: i64,
    xp_lost: i64,
    loot: BTreeMap<u32, i64>,
    loot_money: i64,
    events: Vec<String>,
    timeouts: u32,
    /// Engagements that timed out since the last kill (the run aborts when this explodes).
    consecutive_timeouts: u32,
    skills_used: BTreeMap<u32, u32>,
}

/// `npcs` row of a wanted template.
#[derive(Debug, Clone, Copy)]
struct NpcInfo {
    level: u8,
    faction_id: u32,
}

struct Fighter<'a> {
    game: &'a mut GameClient,
    db: FightDb,
    opts: &'a RunOptions,
    character: RunCharacter,
    /// Active ability ids (`ability1..3`) of the selected character.
    abilities: [u8; 3],
    /// The character's own faction (friendly-target guard).
    self_faction: u32,
    level: u8,
    started: Instant,
    /// All class skill rows of the data, by id.
    rows: HashMap<u32, FightSkillRow>,
    /// `npcs` facts of the wanted templates (0 level = template unknown).
    npcs: HashMap<u32, NpcInfo>,
    /// Skills the server confirmed with `SCSkillLearnedPacket`.
    learned: BTreeSet<u32>,
    scheduler: SkillScheduler,
    hp: Option<i32>,
    mp: Option<i32>,
    max_hp_seen: i32,
    potion_cooldown_until: u64,
    potion_skill: Option<u32>,
    /// `obj id -> template` of the units this run engaged.
    engaged: HashMap<u32, u32>,
    /// Engaged units whose death was seen (the kill is already counted).
    killed: HashSet<u32>,
    /// Forgotten targets (timeouts / corpses), until the given run ms.
    avoid: HashMap<u32, u64>,
    /// The last `CSChangeTargetPacket` target.
    last_target: u32,
    /// The corpse whose loot window is open.
    looting: Option<u32>,
    /// When the last loot item task arrived.
    loot_seen: Option<Instant>,
    /// A death was seen and no resurrection yet.
    dead: bool,
    /// `SCCharacterResurrectedPacket` / positive own `SCUnitPoints` since the last death.
    resurrected: bool,
    learn_pending: bool,
    facts: Facts,
}

/// Login, lobby burst, character create / select, spawn and notify (the same flow the other
/// runners use), plus the active abilities of the selected character.
pub(crate) async fn setup_world(
    cfg: &BotConfig,
    opts: &RunOptions,
    rec: &Recorder,
) -> anyhow::Result<(GameClient, RunCharacter, [u8; 3])> {
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
    if opts.new_character || listed == 0 {
        if listed >= crate::questrun::MAX_CHARACTERS_PER_ACCOUNT {
            bail!(
                "the account already has {listed} characters; the login server allows \
                 {} per account (MaxCharactersPerAccount)",
                crate::questrun::MAX_CHARACTERS_PER_ACCOUNT
            );
        }
        if !crate::questrun::valid_character_name(&cfg.character) {
            bail!(
                "config character '{}' does not match the server's CharacterNameRegex",
                cfg.character
            );
        }
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let name = if opts.new_character {
            crate::questrun::generated_character_name(&cfg.character, millis, 0)?
        } else {
            cfg.character.clone()
        };
        game.create_character_as(&name, opts.race, opts.gender)
            .await?;
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
        created = Some(RunCharacter {
            name,
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
    game.drain(Duration::from_millis(1500), Duration::from_secs(15))
        .await?;
    anyhow::ensure!(game.state.self_obj != 0, "own unit state not received");

    let mut abilities = [11u8; 3];
    for p in &select_packets {
        if p.is_sc(sc::SCCharacterStatePacket) {
            if let Ok((_, _, found)) = parse_state_equipment(&p.body) {
                abilities = found;
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
    Ok((game, character, abilities))
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

impl<'a> Fighter<'a> {
    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// One packet, or a timeout; the connection's keep-alive runs inside.
    async fn tick(&mut self, wait: Duration) -> anyhow::Result<()> {
        if let Some(p) = self.game.recv_within(wait).await? {
            self.observe(&p);
        }
        Ok(())
    }

    /// Applies the facts the fight loop tracks on top of the game state.
    fn observe(&mut self, p: &Packet) {
        let self_obj = self.game.state.self_obj;
        if let Some(s) = parse_skill_started(p) {
            if s.caster_obj == self_obj {
                let now = self.now_ms();
                self.scheduler.on_started(s.skill_id, s.tl, s.result, now);
                if s.result != 0 {
                    self.facts.events.push(format!(
                        "skill {} rejected (SkillResult {})",
                        s.skill_id, s.result
                    ));
                }
            }
        } else if let Some((skill_id, tl, caster)) = parse_skill_fired(p) {
            if caster == self_obj {
                self.scheduler.on_fired(skill_id, tl);
            }
        } else if let Some(tl) = parse_skill_ended(p) {
            if let Some(skill_id) = self.scheduler.on_ended(tl, self.now_ms()) {
                if skill_id != 0 {
                    *self.facts.skills_used.entry(skill_id).or_insert(0) += 1;
                }
            }
        } else if let Some((skill_id, tl)) = parse_plot_event(p) {
            // Plot skills (the level-1 fighter's only attack is one) answer with events only.
            if skill_id != 0 {
                self.scheduler.on_plot_event(skill_id, tl);
            }
        } else if let Some(tl) = parse_plot_ended(p) {
            if let Some(skill_id) = self.scheduler.on_plot_ended(tl, self.now_ms()) {
                if skill_id != 0 {
                    *self.facts.skills_used.entry(skill_id).or_insert(0) += 1;
                }
            }
        } else if let Some(tl) = parse_plot_stopped(p) {
            self.scheduler.on_cancelled(None, Some(tl), self.now_ms());
        } else if let Some(tl) = parse_casting_stopped(p) {
            self.scheduler.on_cancelled(None, Some(tl), self.now_ms());
        } else if let Some(skill_id) = parse_skill_stopped(p) {
            self.scheduler
                .on_cancelled(Some(skill_id), None, self.now_ms());
        }

        if let Some((obj, hp, mp)) = parse_unit_points(p) {
            if obj == self_obj {
                self.hp = Some(hp);
                self.mp = Some(mp);
                self.max_hp_seen = self.max_hp_seen.max(hp);
                if self.dead && hp > 0 {
                    self.dead = false;
                    self.resurrected = true;
                }
            }
        }
        if let Some(obj) = parse_unit_death(p) {
            if obj == self_obj {
                self.dead = true;
                self.facts.deaths += 1;
                self.facts.events.push(format!(
                    "died at {} ms (death #{})",
                    self.now_ms(),
                    self.facts.deaths
                ));
            } else if let Some(template) = self.engaged.get(&obj).copied() {
                self.killed.insert(obj);
                self.engaged.remove(&obj);
                self.facts.kills += 1;
                *self.facts.kills_by_template.entry(template).or_insert(0) += 1;
                self.avoid.insert(obj, self.now_ms().saturating_add(60_000));
            }
        }
        if p.is_sc(sc::SCCharacterResurrectedPacket) {
            let mut s = p.stream();
            if s.read_bc().ok() == Some(self_obj) {
                self.dead = false;
                self.resurrected = true;
            }
        }
        if p.is_sc(sc::SCNotifyResurrectionPacket) {
            self.facts.events.push("resurrection offered".into());
        }
        if p.is_sc(sc::SCSkillLearnedPacket) {
            let mut s = p.stream();
            if let (Ok(skill_id), Ok(_level)) = (s.read_u32(), s.read_u8()) {
                self.learned.insert(skill_id);
                self.facts.events.push(format!("learned skill {skill_id}"));
            }
        }
        if let Some((obj, level)) = parse_level_changed(p) {
            if obj == self_obj {
                self.level = level;
                self.learn_pending = true;
                self.facts.events.push(format!("level up -> {level}"));
            }
        }
        if let Some((obj, exp)) = parse_exp_changed(p) {
            if obj == self_obj {
                if exp >= 0 {
                    self.facts.xp_gained += i64::from(exp);
                } else {
                    self.facts.xp_lost += i64::from(-exp);
                }
            }
        }
        if p.is_sc(sc::SCItemTaskSuccessPacket) && self.looting.is_some() {
            let info = parse_item_task(&p.body);
            for item in &info.items {
                if item.template != 0 && item.count != 0 {
                    *self.facts.loot.entry(item.template).or_insert(0) +=
                        i64::from(item.count.max(1));
                }
            }
            self.facts.loot_money += info.money;
            self.loot_seen = Some(Instant::now());
        }
    }

    /// The class skills of the data the character may learn now.
    fn learn_candidates(&self) -> Vec<FightSkillRow> {
        let mut rows: Vec<FightSkillRow> = self
            .rows
            .values()
            .filter(|r| {
                self.abilities.contains(&r.ability_id)
                    && r.ability_level <= i32::from(self.level)
                    && r.attacks_units()
                    && r.skill_points <= 3
            })
            .cloned()
            .collect();
        rows.sort_by(|a, b| {
            a.ability_level
                .cmp(&b.ability_level)
                .then(a.skill_points.cmp(&b.skill_points))
                .then(a.id.cmp(&b.id))
        });
        rows
    }

    /// The learned set the server would have: the ability's start skills are granted at
    /// creation (`SkillData::finish`), further candidates were learned while skill points were
    /// left (`systems/skills/progress.rs::learn_skill`). The fight runner drives the character
    /// the same way, so the model matches what the server knows (a skill the server already has
    /// answers nothing to another `CSLearnSkillPacket`).
    fn simulate_learned(&self) -> BTreeSet<u32> {
        let mut learned: BTreeSet<u32> = BTreeSet::new();
        let mut points = self.db.skill_points_for_level(self.level).unwrap_or(0);
        for r in self.rows.values() {
            if self.abilities.contains(&r.ability_id) && r.start_ability_skill() {
                learned.insert(r.id);
                points -= r.skill_points;
            }
        }
        for r in self.learn_candidates() {
            if learned.contains(&r.id) {
                continue;
            }
            if r.skill_points <= points {
                learned.insert(r.id);
                points -= r.skill_points;
            }
        }
        learned
    }

    /// Sends `CSLearnSkillPacket` for the simulated learned set and records what the server
    /// answers: skills the server does not know yet are taught (points permitting), known ones
    /// stay silent.
    async fn learn_sweep(&mut self) -> anyhow::Result<()> {
        if !self.learn_pending {
            return Ok(());
        }
        self.learn_pending = false;
        let simulated = self.simulate_learned();
        let new: Vec<u32> = simulated
            .iter()
            .copied()
            .filter(|id| !self.learned.contains(id))
            .collect();
        self.learned.extend(simulated);
        if new.is_empty() {
            self.rebuild_scheduler();
            return Ok(());
        }
        self.facts
            .events
            .push(format!("learning {} skill(s)", new.len()));
        for id in &new {
            self.game.learn_skill(*id).await?;
        }
        let deadline = Instant::now() + Duration::from_secs(4);
        let sent_at = Instant::now();
        let mut learned_seen = self.learned.len();
        let mut last_learned: Option<Instant> = None;
        loop {
            self.tick(PUMP).await?;
            if self.learned.len() > learned_seen {
                learned_seen = self.learned.len();
                last_learned = Some(Instant::now());
            }
            if last_learned.is_some_and(|t| t.elapsed() >= Duration::from_millis(600)) {
                break;
            }
            // A known skill stays silent: do not burn the whole budget on it.
            if last_learned.is_none() && sent_at.elapsed() >= Duration::from_millis(800) {
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
        }
        self.rebuild_scheduler();
        Ok(())
    }

    /// Rebuilds the scheduler from the learned set (plus the default auto-attack, skill 2).
    fn rebuild_scheduler(&mut self) {
        let mut skills: Vec<FightSkill> = Vec::new();
        for id in self.learned.clone() {
            if let Some(row) = self.rows.get(&id) {
                if let Some(skill) = row.to_fight_skill(self.level) {
                    skills.push(skill);
                }
            }
        }
        if !self.learned.contains(&2) {
            if let Some(row) = self.rows.get(&2) {
                if let Some(skill) = row.to_fight_skill(self.level) {
                    skills.push(skill);
                }
            }
        }
        self.scheduler = SkillScheduler::new(skills);
    }

    fn hp_pct(&self) -> f32 {
        match (self.hp, self.max_hp_seen) {
            (Some(hp), max) if max > 0 => (hp as f32 / max as f32).clamp(0.0, 1.0),
            _ => 1.0,
        }
    }

    /// Candidate targets from the visible units of the wanted templates.
    fn candidates(&self) -> Vec<FightTarget> {
        let mut out = Vec::new();
        for u in self.game.state.units.values() {
            if u.unit_type != 1 || u.obj_id == self.game.state.self_obj {
                continue;
            }
            if !self.opts.template_ids.contains(&u.id) {
                continue;
            }
            let info = self.npcs.get(&u.id).copied();
            if info.is_some_and(|i| i.faction_id != 0 && i.faction_id == self.self_faction) {
                continue;
            }
            let dead = self.game.state.dead_units.contains(&u.obj_id)
                || self
                    .game
                    .state
                    .unit_points
                    .get(&u.obj_id)
                    .is_some_and(|(hp, _)| *hp <= 0);
            out.push(FightTarget {
                obj_id: u.obj_id,
                template_id: u.id,
                level: info.map_or(0, |i| i.level),
                pos: u.pos,
                dead,
            });
        }
        out
    }

    /// Walks the hunt points (when nothing is visible) and returns the best target, or `None`
    /// when the acquisition budget elapsed.
    async fn acquire(
        &mut self,
        approach: &mut dyn Approach,
    ) -> anyhow::Result<Option<FightTarget>> {
        let deadline = Instant::now() + self.opts.acquire_timeout;
        let mut hunt = 0usize;
        let mut announced = false;
        loop {
            self.scheduler.tick(self.now_ms());
            let avoid: HashSet<u32> = self
                .avoid
                .iter()
                .filter(|(_, until)| **until > self.now_ms())
                .map(|(id, _)| *id)
                .collect();
            if let Some(t) = choose_target(
                &self.candidates(),
                self.game.state.self_pos,
                self.level,
                self.opts.max_level_delta,
                &avoid,
            ) {
                return Ok(Some(t));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            if !self.opts.hunt_points.is_empty() {
                let point = self.opts.hunt_points[hunt % self.opts.hunt_points.len()];
                hunt += 1;
                if !announced {
                    self.facts.events.push(format!(
                        "no target in sight; searching around {} point(s)",
                        self.opts.hunt_points.len()
                    ));
                    announced = true;
                }
                approach.approach(self.game, point, None).await?;
                // Give the server a moment to announce the units around the new spot.
                let wait_until = Instant::now() + Duration::from_secs(2);
                while Instant::now() < wait_until {
                    self.tick(PUMP).await?;
                }
            } else {
                self.tick(PUMP).await?;
            }
        }
    }

    fn is_target_dead(&self, obj: u32) -> bool {
        self.killed.contains(&obj)
            || self.game.state.dead_units.contains(&obj)
            || self
                .game
                .state
                .unit_points
                .get(&obj)
                .is_some_and(|(hp, _)| *hp <= 0)
    }

    /// One engagement with a target until it dies, the player dies, or the budget runs out.
    async fn engage(
        &mut self,
        approach: &mut dyn Approach,
        target: FightTarget,
    ) -> anyhow::Result<Engage> {
        self.engaged.insert(target.obj_id, target.template_id);
        let deaths_before = self.facts.deaths;
        let started = Instant::now();
        loop {
            self.scheduler.tick(self.now_ms());
            // A death whose packet was consumed while walking (the approach pumps the
            // connection itself) is still in the game state.
            let me = self.game.state.self_obj;
            if !self.dead && self.game.state.dead_units.contains(&me) {
                self.dead = true;
                self.facts.deaths += 1;
                self.facts
                    .events
                    .push(format!("died at {} ms (seen in the state)", self.now_ms()));
            }
            if self.facts.deaths > deaths_before {
                self.engaged.remove(&target.obj_id);
                return Ok(Engage::PlayerDead);
            }
            let Some(unit) = self.game.state.units.get(&target.obj_id).cloned() else {
                self.engaged.remove(&target.obj_id);
                return Ok(if self.killed.contains(&target.obj_id) {
                    Engage::Killed
                } else {
                    Engage::Timeout
                });
            };
            if self.is_target_dead(target.obj_id) {
                return Ok(Engage::Killed);
            }
            if started.elapsed() >= self.opts.combat_timeout {
                self.engaged.remove(&target.obj_id);
                self.facts
                    .events
                    .push(format!("target {} timed out", target.obj_id));
                self.facts.timeouts += 1;
                return Ok(Engage::Timeout);
            }

            if self.hp_pct() < RETREAT_HP_PCT {
                if self.try_potion().await? {
                    continue;
                }
                self.rest(approach, unit.pos).await?;
                continue;
            }
            if self.hp_pct() < POTION_HP_PCT && self.try_potion().await? {
                continue;
            }

            let now = self.now_ms();
            let Some(skill) = self.scheduler.next_skill(now, self.mp) else {
                let wait = Duration::from_millis(self.scheduler.wait_ms(now, self.mp).min(150));
                self.tick(wait).await?;
                continue;
            };
            let me = self.game.state.self_pos;
            let range = skill.max_range.min(4.0);
            let dist = dist3(me, unit.pos);
            if dist > range - RANGE_SLACK || dist < skill.min_range + 0.2 {
                let stand = self.stand_point(unit.pos, MELEE_STAND_RANGE);
                approach.approach(self.game, stand, Some(unit.pos)).await?;
                self.tick(CAST_SETTLE).await?;
                continue;
            }
            if self.last_target != target.obj_id {
                self.game.change_target(target.obj_id).await?;
                self.last_target = target.obj_id;
            }
            // Face the target: angle checks and auto attacks use the movement yaw.
            self.game.move_to_facing(me, unit.pos, 600).await?;
            self.game.start_skill(skill.id, target.obj_id).await?;
            self.scheduler.cast_sent(skill.id, now);
            self.tick(CAST_SETTLE).await?;
        }
    }

    /// A stand point `dist` away from `target`, on the line towards the character.
    fn stand_point(&self, target: (f32, f32, f32), dist: f32) -> (f32, f32, f32) {
        let me = self.game.state.self_pos;
        let dir = unit_vector(me, target);
        (
            target.0 + dir.0 * dist,
            target.1 + dir.1 * dist,
            target.2 + dir.2 * dist,
        )
    }

    /// Drinks the configured potion when it is in the bag and off cooldown.
    async fn try_potion(&mut self) -> anyhow::Result<bool> {
        let Some(template) = self.opts.potion else {
            return Ok(false);
        };
        if self.now_ms() < self.potion_cooldown_until {
            return Ok(false);
        }
        let Some(skill) = self.potion_skill else {
            return Ok(false);
        };
        let Some(item) = self
            .game
            .state
            .bag
            .values()
            .find(|i| i.template_id == template)
            .copied()
        else {
            return Ok(false);
        };
        self.game
            .use_item(
                item.item_id,
                item.template_id,
                skill,
                SkillCastTarget::Unit(self.game.state.self_obj),
            )
            .await?;
        self.potion_cooldown_until = self.now_ms() + POTION_COOLDOWN.as_millis() as u64;
        self.facts.events.push(format!(
            "potion {} used at {:.0}% hp",
            template,
            self.hp_pct() * 100.0
        ));
        Ok(true)
    }

    /// Leaves the fight (approached away from the threat) and waits for HP to come back.
    async fn rest(
        &mut self,
        approach: &mut dyn Approach,
        threat: (f32, f32, f32),
    ) -> anyhow::Result<()> {
        let me = self.game.state.self_pos;
        let dir = unit_vector(threat, me);
        let rest_point = (
            me.0 + dir.0 * REST_DISTANCE,
            me.1 + dir.1 * REST_DISTANCE,
            me.2 + dir.2 * REST_DISTANCE,
        );
        self.facts.events.push(format!(
            "retreating to rest at {:.0}% hp",
            self.hp_pct() * 100.0
        ));
        approach.approach(self.game, rest_point, None).await?;
        self.engaged.clear();
        self.last_target = 0;
        let deadline = Instant::now() + REST_TIMEOUT;
        loop {
            self.tick(PUMP).await?;
            if self.dead {
                return Ok(());
            }
            if self.hp_pct() >= REST_HP_PCT {
                return Ok(());
            }
            if Instant::now() >= deadline {
                self.facts.events.push(format!(
                    "rest timed out at {:.0}% hp",
                    self.hp_pct() * 100.0
                ));
                return Ok(());
            }
        }
    }

    /// Answers a death with `CSResurrectCharacterPacket` and waits for the revive.
    async fn revive(&mut self) -> anyhow::Result<()> {
        self.facts.resurrects += 1;
        self.facts.events.push(format!(
            "resurrecting ({})",
            if self.opts.resurrect_in_place {
                "in place"
            } else {
                "temple"
            }
        ));
        resurrect(self.game, self.opts.resurrect_in_place).await?;
        self.resurrected = false;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.tick(PUMP).await?;
            if self.resurrected {
                break;
            }
            if Instant::now() >= deadline {
                self.facts.events.push("resurrection not confirmed".into());
                break;
            }
        }
        self.dead = false;
        self.engaged.clear();
        self.last_target = 0;
        Ok(())
    }

    /// Opens the corpse's loot window, waits for the item tasks and closes it again.
    async fn loot_corpse(&mut self, obj: u32) -> anyhow::Result<()> {
        if !self.game.state.units.contains_key(&obj) {
            return Ok(());
        }
        self.looting = Some(obj);
        self.loot_seen = None;
        let items_before: i64 = self.facts.loot.values().sum();
        self.game.loot_all(obj).await?;
        let deadline = Instant::now() + LOOT_WINDOW;
        loop {
            self.tick(PUMP).await?;
            match self.loot_seen {
                Some(t) if t.elapsed() >= LOOT_QUIET => break,
                _ if Instant::now() >= deadline => break,
                _ => {}
            }
        }
        let _ = self.game.close_loot_bag(obj).await;
        self.looting = None;
        let gained: i64 = self.facts.loot.values().sum::<i64>() - items_before;
        if gained > 0 {
            self.facts
                .events
                .push(format!("loot window on {obj} gave {gained} item(s)"));
        }
        Ok(())
    }
}

/// The client half of `systems/skills/resurrect.rs`: a `CSResurrectCharacterPacket`.
pub async fn resurrect(game: &mut GameClient, in_place: bool) -> anyhow::Result<()> {
    game.send_cs(cs::CSResurrectCharacterPacket, &[u8::from(in_place)])
        .await
}

// -------------------------------------------------------------------------------------------
// Player-mode session (the quest runner's combat)
// -------------------------------------------------------------------------------------------

/// How one [`Session::kill`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillOutcome {
    /// The target died (its corpse was looted).
    Killed,
    /// The character died and was revived.
    Died,
    /// The target vanished or the engagement budget ran out.
    Timeout,
}

/// Combat knowledge of one character across a whole quest run: the class skill rows, the
/// skills the server accepted and the totals the run report needs. Each [`Session::kill`]
/// builds a short-lived fighter around the quest runner's own `GameClient`, so the fight uses
/// the same connection and `GameState` as the quest steps.
pub struct Session {
    db: FightDb,
    rows: HashMap<u32, FightSkillRow>,
    abilities: [u8; 3],
    learned: BTreeSet<u32>,
    /// The level the learn sweep last ran at (0 = never).
    learned_level: u8,
    opts: RunOptions,
    pub kills: u32,
    pub deaths: u32,
    pub resurrects: u32,
    pub timeouts: u32,
    pub loot_items: BTreeMap<u32, i64>,
    pub events: Vec<String>,
}

impl Session {
    pub fn new(
        compact_db: Option<&Path>,
        abilities: [u8; 3],
        opts: RunOptions,
    ) -> anyhow::Result<Self> {
        let db = FightDb::open_default(compact_db)?;
        let mut rows = HashMap::new();
        for row in db.ability_skills()? {
            rows.insert(row.id, row);
        }
        Ok(Self {
            db,
            rows,
            abilities,
            learned: BTreeSet::new(),
            learned_level: 0,
            opts,
            kills: 0,
            deaths: 0,
            resurrects: 0,
            timeouts: 0,
            loot_items: BTreeMap::new(),
            events: Vec::new(),
        })
    }

    /// `npcs.LEVEL` / `npcs.faction_id` of a template.
    pub fn npc(&self, template_id: u32) -> Option<(u8, u32)> {
        self.db.npc(template_id).ok().flatten()
    }

    /// Fights `obj` (a unit in the game state) until it dies, the character dies (revived in
    /// place) or the engagement budget ends, and loots the corpse of a kill.
    pub async fn kill(
        &mut self,
        game: &mut GameClient,
        obj: u32,
        approach: &mut dyn Approach,
    ) -> anyhow::Result<KillOutcome> {
        let Some(unit) = game.state.units.get(&obj).cloned() else {
            return Ok(KillOutcome::Timeout);
        };
        let Some(info) = game.state.self_info.clone() else {
            bail!("no own character info for the fight");
        };
        let level = info.level.max(1);
        let self_faction = info.faction;
        let target = FightTarget {
            obj_id: obj,
            template_id: unit.id,
            level: unit.level,
            pos: unit.pos,
            dead: false,
        };
        let opts = self.opts.clone();
        let mut fighter = Fighter {
            game,
            db: FightDb::open(self.db.path())?,
            opts: &opts,
            character: RunCharacter {
                name: info.name.clone(),
                race: info.race,
                gender: info.gender,
                start_level: level,
            },
            abilities: self.abilities,
            self_faction,
            level,
            started: Instant::now(),
            rows: self.rows.clone(),
            npcs: HashMap::new(),
            learned: self.learned.clone(),
            scheduler: SkillScheduler::new(Vec::new()),
            hp: None,
            mp: None,
            max_hp_seen: 0,
            potion_cooldown_until: 0,
            potion_skill: None,
            engaged: HashMap::new(),
            killed: HashSet::new(),
            avoid: HashMap::new(),
            last_target: 0,
            looting: None,
            loot_seen: None,
            dead: false,
            resurrected: false,
            learn_pending: self.learned_level != level,
            facts: Facts::default(),
        };
        // The current HP seeds the retreat logic (no packet may arrive before the first tick).
        if let Some((hp, mp)) = fighter
            .game
            .state
            .unit_points
            .get(&fighter.game.state.self_obj)
        {
            fighter.hp = Some(*hp);
            fighter.mp = Some(*mp);
            fighter.max_hp_seen = *hp;
        }
        if fighter.learn_pending {
            fighter.learn_sweep().await?;
            self.learned_level = level;
        } else {
            fighter.rebuild_scheduler();
        }
        if fighter.scheduler.skills().is_empty() {
            bail!(
                "no castable skill: abilities {:?}, level {level}",
                fighter.abilities
            );
        }
        let outcome = match fighter.engage(approach, target).await? {
            Engage::Killed => {
                fighter.loot_corpse(obj).await?;
                KillOutcome::Killed
            }
            Engage::PlayerDead => {
                fighter.revive().await?;
                KillOutcome::Died
            }
            Engage::Timeout => KillOutcome::Timeout,
        };
        self.learned = fighter.learned.clone();
        // A level-up during the fight must be learned before the next one.
        if fighter.level != level {
            self.learned_level = 0;
        }
        self.kills += fighter.facts.kills;
        self.deaths += fighter.facts.deaths;
        self.resurrects += fighter.facts.resurrects;
        self.timeouts += fighter.facts.timeouts;
        for (item, count) in &fighter.facts.loot {
            *self.loot_items.entry(*item).or_insert(0) += *count;
        }
        for e in &fighter.facts.events {
            if self.events.len() < 500 {
                self.events.push(e.clone());
            }
        }
        Ok(outcome)
    }

    /// Answers a death seen outside a fight (walking, a quest step) with the revive.
    pub async fn revive(&mut self, game: &mut GameClient) -> anyhow::Result<()> {
        self.deaths += 1;
        self.resurrects += 1;
        resurrect(game, self.opts.resurrect_in_place).await?;
        let deadline = Instant::now() + Duration::from_secs(10);
        let me = game.state.self_obj;
        while Instant::now() < deadline {
            game.recv_within(PUMP).await?;
            if !game.state.dead_units.contains(&me) {
                break;
            }
        }
        Ok(())
    }
}

// -------------------------------------------------------------------------------------------
// Run
// -------------------------------------------------------------------------------------------

/// Runs one fight session and returns the report.
pub async fn run(
    cfg: &BotConfig,
    opts: &RunOptions,
    approach: &mut dyn Approach,
) -> anyhow::Result<FightReport> {
    anyhow::ensure!(!opts.template_ids.is_empty(), "no --tpl target");
    let rec = Recorder::new(true);
    rec.step("setup");
    let (mut game, character, abilities) = setup_world(cfg, opts, &rec).await?;
    let db = FightDb::open_default(opts.compact_db.as_deref())?;
    let mut rows: HashMap<u32, FightSkillRow> = HashMap::new();
    for row in db.ability_skills()? {
        rows.insert(row.id, row);
    }
    let start_level = character.start_level;
    let self_faction = game.state.self_info.as_ref().map_or(0, |i| i.faction);
    let mut npcs: HashMap<u32, NpcInfo> = HashMap::new();
    for tpl in &opts.template_ids {
        if let Some((level, faction_id)) = db.npc(*tpl)? {
            npcs.insert(*tpl, NpcInfo { level, faction_id });
        } else {
            npcs.insert(
                *tpl,
                NpcInfo {
                    level: 0,
                    faction_id: 0,
                },
            );
        }
    }
    let potion_skill =
        match opts.potion {
            Some(tpl) => Some(db.item_use_skill(tpl)?.with_context(|| {
                format!("potion item {tpl} has no use skill (items.use_skill_id)")
            })?),
            None => None,
        };
    let mut fighter = Fighter {
        game: &mut game,
        db,
        opts,
        character: character.clone(),
        abilities,
        self_faction,
        level: character.start_level,
        started: Instant::now(),
        rows,
        npcs,
        learned: BTreeSet::new(),
        scheduler: SkillScheduler::new(Vec::new()),
        hp: None,
        mp: None,
        max_hp_seen: 0,
        potion_cooldown_until: 0,
        potion_skill,
        engaged: HashMap::new(),
        killed: HashSet::new(),
        avoid: HashMap::new(),
        last_target: 0,
        looting: None,
        loot_seen: None,
        dead: false,
        resurrected: false,
        learn_pending: true,
        facts: Facts::default(),
    };

    fighter.learn_sweep().await?;
    if fighter.scheduler.skills().is_empty() {
        bail!(
            "no castable skill: the character learned none of the data's hostile damage skills \
             (abilities {:?}, level {})",
            fighter.abilities,
            fighter.level
        );
    }
    fighter.facts.events.push(format!(
        "starting level {}, {} skill(s): {}",
        fighter.level,
        fighter.scheduler.skills().len(),
        fighter
            .scheduler
            .skills()
            .iter()
            .map(|s| format!("{} ({})", s.id, s.name))
            .collect::<Vec<_>>()
            .join(", ")
    ));

    rec.step("fight");
    let mut aborted = false;
    let run_deadline = Instant::now() + opts.run_timeout;
    while fighter.facts.kills < opts.kills {
        if fighter.facts.deaths > opts.max_deaths {
            fighter
                .facts
                .events
                .push(format!("aborting after {} deaths", fighter.facts.deaths));
            aborted = true;
            break;
        }
        if fighter.facts.consecutive_timeouts >= opts.max_timeouts {
            fighter.facts.events.push(format!(
                "aborting after {} engagement timeouts without a kill",
                fighter.facts.consecutive_timeouts
            ));
            aborted = true;
            break;
        }
        if Instant::now() >= run_deadline {
            fighter
                .facts
                .events
                .push(format!("aborting after {:?}", opts.run_timeout));
            aborted = true;
            break;
        }
        if fighter.learn_pending {
            fighter.learn_sweep().await?;
        }
        let Some(target) = fighter.acquire(approach).await? else {
            fighter.facts.events.push(format!(
                "no target of {:?} in sight within {:?}",
                opts.template_ids, opts.acquire_timeout
            ));
            break;
        };
        let obj = target.obj_id;
        match fighter.engage(approach, target).await? {
            Engage::Killed => {
                fighter.facts.consecutive_timeouts = 0;
                fighter.loot_corpse(obj).await?;
            }
            Engage::PlayerDead => {
                fighter.revive().await?;
                fighter.facts.consecutive_timeouts = 0;
            }
            Engage::Timeout => {
                fighter.facts.consecutive_timeouts += 1;
            }
        }
    }
    rec.step("leave");
    leave(&mut *fighter.game).await;
    if let Some(path) = &opts.capture {
        rec.save(path)?;
    }

    let mut loot: Vec<LootEntry> = Vec::new();
    for (item_id, count) in &fighter.facts.loot {
        let name = fighter
            .db
            .item_name(*item_id)?
            .unwrap_or_else(|| "?".to_string());
        loot.push(LootEntry {
            item_id: *item_id,
            name,
            count: *count,
        });
    }
    let skill_names: BTreeMap<u32, String> = fighter
        .scheduler
        .skills()
        .iter()
        .map(|s| (s.id, s.name.clone()))
        .collect();
    let report = FightReport {
        character: fighter.character.name.clone(),
        race: fighter.character.race,
        gender: fighter.character.gender,
        start_level,
        end_level: fighter.level,
        requested_kills: opts.kills,
        kills: fighter.facts.kills,
        kills_by_template: fighter.facts.kills_by_template.clone(),
        deaths: fighter.facts.deaths,
        resurrects: fighter.facts.resurrects,
        xp_gained: fighter.facts.xp_gained,
        xp_lost: fighter.facts.xp_lost,
        skills_used: fighter.facts.skills_used.clone(),
        skill_names,
        learned_skills: fighter.learned.iter().copied().collect(),
        loot,
        loot_money: fighter.facts.loot_money,
        events: fighter.facts.events.clone(),
        timeouts: fighter.facts.timeouts,
        aborted,
    };
    if let Some(path) = &opts.out {
        std::fs::write(path, serde_json::to_string_pretty(&report)?)
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(report)
}

// -------------------------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------------------------

#[cfg(test)]
mod scheduler_tests {
    use super::*;

    fn skill(id: u32, priority: i64, mana: i32, cooldown_ms: u64, gcd_ms: u64) -> FightSkill {
        FightSkill {
            id,
            name: format!("skill {id}"),
            mana_cost: mana,
            cooldown_ms,
            gcd_ms,
            min_range: 0.0,
            max_range: 4.0,
            priority,
            auto_attack: id == 2,
            plot: false,
        }
    }

    #[test]
    fn picks_the_highest_priority_ready_skill_and_gates_mana() {
        let mut s = SkillScheduler::new(vec![
            skill(2, 0, 0, 0, 1000),
            skill(18132, 100, 30, 5000, 500),
        ]);
        assert_eq!(s.next_skill(0, None).map(|s| s.id), Some(18132));
        assert_eq!(s.next_skill(0, Some(10)).map(|s| s.id), Some(2));
        // Without an MP reading the mana gate is left to the server.
        assert_eq!(s.next_skill(0, None).map(|s| s.id), Some(18132));

        s.cast_sent(18132, 0);
        s.on_fired(18132, 7);
        let resolved = s.on_ended(7, 0).expect("the cast resolved");
        assert_eq!(resolved, 18132);
        assert_eq!(s.casts, 1);
        // The GCD from the 500 ms skill gates both skills at first.
        assert!(s.next_skill(100, None).is_none());
        // After the GCD the auto attack takes over: the damage skill is on its own cooldown.
        assert_eq!(s.next_skill(500, None).map(|s| s.id), Some(2));
        assert_eq!(s.next_skill(5000, None).map(|s| s.id), Some(18132));
    }

    #[test]
    fn an_in_flight_cast_blocks_the_next_one_until_it_ends() {
        let mut s = SkillScheduler::new(vec![skill(18132, 100, 0, 0, 500)]);
        s.cast_sent(18132, 0);
        s.on_started(18132, 3, 0, 0);
        assert!(s.busy());
        assert!(s.next_skill(10_000, None).is_none());
        assert_eq!(s.in_flight_skill(), Some(18132));
        assert_eq!(s.on_ended(3, 10_000), Some(18132));
        assert!(!s.busy());
        // The cast's GCD still gates the next one.
        assert!(s.next_skill(10_000, None).is_none());
        assert!(s.next_skill(10_500, None).is_some());
    }

    #[test]
    fn a_cast_time_skill_resolves_cooldown_and_gcd_at_the_ended() {
        let mut s = SkillScheduler::new(vec![skill(18132, 100, 0, 12_000, 500)]);
        s.cast_sent(18132, 0);
        s.on_started(18132, 9, 0, 0); // 2 s cast
        s.on_fired(18132, 9);
        assert_eq!(s.on_ended(9, 2000), Some(18132));
        assert_eq!(s.gcd_until(), 2500);
        // The own cooldown outlasts the GCD.
        assert!(s.next_skill(2500, None).is_none());
        assert!(s.next_skill(14_000, None).is_some());
    }

    #[test]
    fn a_rejected_cast_backs_off_and_a_gcd_rejection_backs_off_longer() {
        let mut s = SkillScheduler::new(vec![skill(18132, 100, 0, 0, 500)]);
        s.cast_sent(18132, 0);
        s.on_started(18132, 3, 32, 0); // SkillResult 32: requirement failed
        assert_eq!(s.rejections, 1);
        assert!(s.next_skill(100, None).is_none());
        assert!(s.next_skill(REJECT_RETRY_MS, None).is_some());

        let mut s = SkillScheduler::new(vec![skill(18132, 100, 0, 0, 500)]);
        s.cast_sent(18132, 0);
        s.on_started(18132, 3, SKILL_RESULT_COOLDOWN, 0);
        assert!(s.next_skill(REJECT_RETRY_MS + 1, None).is_none());
        assert!(s.next_skill(COOLDOWN_RETRY_MS + 1, None).is_some());
    }

    #[test]
    fn a_sent_cast_without_a_response_is_retried_after_the_timeout() {
        let mut s = SkillScheduler::new(vec![skill(18132, 100, 0, 0, 500)]);
        s.cast_sent(18132, 0);
        s.tick(CAST_RESPONSE_TIMEOUT_MS - 1);
        assert!(s.busy());
        s.tick(CAST_RESPONSE_TIMEOUT_MS);
        assert!(!s.busy());
        // A short back-off keeps the retry from being sent in the same pump slice.
        assert!(s.next_skill(CAST_RESPONSE_TIMEOUT_MS, None).is_none());
        assert!(s
            .next_skill(CAST_RESPONSE_TIMEOUT_MS + SILENT_RETRY_MS, None)
            .is_some());
    }

    #[test]
    fn a_cancelled_cast_frees_the_scheduler() {
        let mut s = SkillScheduler::new(vec![skill(18132, 100, 0, 0, 500)]);
        s.cast_sent(18132, 0);
        s.on_started(18132, 5, 0, 0);
        s.on_cancelled(None, Some(5), 100);
        assert!(!s.busy());
        assert!(s.next_skill(100, None).is_none());
        assert!(s.next_skill(100 + SILENT_RETRY_MS, None).is_some());
    }

    #[test]
    fn a_plot_skill_resolves_on_the_plot_packets() {
        let mut s = SkillScheduler::new(vec![FightSkill {
            plot: true,
            ..skill(18132, 90, 30, 0, 500)
        }]);
        s.cast_sent(18132, 0);
        // A known plot skill answers silently; the timeout frees it again.
        s.tick(CAST_RESPONSE_TIMEOUT_MS);
        assert!(!s.busy());
        assert!(s.next_skill(CAST_RESPONSE_TIMEOUT_MS, None).is_none());

        s.cast_sent(18132, 2000);
        s.on_plot_event(18132, 11);
        assert_eq!(s.in_flight_skill(), Some(18132));
        assert!(s.next_skill(2500, None).is_none());
        assert_eq!(s.on_plot_ended(11, 2500), Some(18132));
        assert_eq!(s.casts, 1);
        assert_eq!(s.gcd_until(), 3000);
        assert!(s.next_skill(3000, None).is_some());
    }
}

#[cfg(test)]
mod target_tests {
    use super::*;

    fn target(obj_id: u32, level: u8, pos: (f32, f32, f32)) -> FightTarget {
        FightTarget {
            obj_id,
            template_id: 1000 + obj_id,
            level,
            pos,
            dead: false,
        }
    }

    #[test]
    fn the_nearest_live_in_level_candidate_wins() {
        let me = (0.0, 0.0, 0.0);
        let candidates = [
            target(1, 1, (50.0, 0.0, 0.0)),
            target(2, 2, (10.0, 0.0, 0.0)),
            target(3, 3, (30.0, 0.0, 0.0)),
        ];
        let picked = choose_target(&candidates, me, 1, 3, &HashSet::new()).unwrap();
        assert_eq!(picked.obj_id, 2);
    }

    #[test]
    fn over_level_dead_and_avoided_candidates_are_skipped() {
        let me = (0.0, 0.0, 0.0);
        let mut candidates = [
            target(1, 9, (5.0, 0.0, 0.0)),
            target(2, 2, (10.0, 0.0, 0.0)),
            target(3, 1, (20.0, 0.0, 0.0)),
        ];
        candidates[1].dead = true;
        let avoid: HashSet<u32> = [3u32].into_iter().collect();
        assert!(choose_target(&candidates, me, 1, 3, &avoid).is_none());
        // An unknown level (0) is always allowed.
        let unknown = [target(4, 0, (7.0, 0.0, 0.0))];
        assert_eq!(
            choose_target(&unknown, me, 1, 3, &HashSet::new())
                .unwrap()
                .obj_id,
            4
        );
    }

    #[test]
    fn ties_go_to_the_lower_level_then_the_lower_object_id() {
        let me = (0.0, 0.0, 0.0);
        let candidates = [
            target(9, 3, (10.0, 0.0, 0.0)),
            target(7, 1, (10.0, 0.0, 0.0)),
            target(8, 1, (10.0, 0.0, 0.0)),
        ];
        let picked = choose_target(&candidates, me, 1, 3, &HashSet::new()).unwrap();
        assert_eq!(picked.obj_id, 7);
    }
}

#[cfg(test)]
mod packet_tests {
    use super::*;
    use crate::game::LEVEL_GAME;
    use aaemu_commons::{Marshal, PacketStream};
    use aaemu_game::packets::world::SCUnitPointsPacket;
    use aaemu_game::systems::skills::packets::{SCExpChangedPacket, SCUnitDeathPacket};
    use aaemu_game::systems::skills::types::KillReason;

    fn encode<M: Marshal>(m: &M) -> Vec<u8> {
        let mut s = PacketStream::new();
        m.write(&mut s);
        s.into_bytes()
    }

    fn packet(opcode: u16, body: Vec<u8>) -> Packet {
        Packet {
            level: LEVEL_GAME,
            opcode,
            body,
        }
    }

    #[test]
    fn death_points_and_exp_packets_are_read() {
        let death = packet(
            sc::SCUnitDeathPacket,
            encode(&SCUnitDeathPacket {
                obj_id: 0x1234,
                kill_reason: KillReason::Damage,
                resurrect_wait_time: 0,
                lost_exp: 5,
                durability_loss_rate: 0,
                killer: None,
            }),
        );
        assert_eq!(parse_unit_death(&death), Some(0x1234));

        let points = packet(
            sc::SCUnitPointsPacket,
            encode(&SCUnitPointsPacket {
                obj_id: 0x22,
                hp: 12345,
                mp: 6789,
            }),
        );
        // The packet's `Marshal` scales hp / mp by 100 on the wire; the reader decodes back.
        assert_eq!(parse_unit_points(&points), Some((0x22, 12345, 6789)));

        let exp = packet(
            sc::SCExpChangedPacket,
            encode(&SCExpChangedPacket {
                obj_id: 0x22,
                exp: 518,
                should_add_ability_exp: true,
            }),
        );
        assert_eq!(parse_exp_changed(&exp), Some((0x22, 518)));
    }

    #[test]
    fn the_resurrect_body_decodes_with_the_server_reader() {
        use aaemu_game::net::registry::ClientPacket;
        use aaemu_game::systems::skills::resurrect::CSResurrectCharacterPacket;
        let p = CSResurrectCharacterPacket::read(&mut PacketStream::from_bytes(vec![1])).unwrap();
        assert!(p.in_place);
        let p = CSResurrectCharacterPacket::read(&mut PacketStream::from_bytes(vec![0])).unwrap();
        assert!(!p.in_place);
    }
}
