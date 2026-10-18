//! Crafting sweep runner (`aaemu-testclient craft`): drives one GM bot through recipes of the
//! same `compact.sqlite3` the game server loads (`crafts`, `craft_materials`, `craft_products`,
//! `craft_pack_crafts` and the craft skills) and appends one JSON result line per recipe.
//!
//! Server flow being exercised (`systems/economy/craft.rs`, `systems/skills/skill.rs`):
//! `CSExecuteCraftPacket(craftId, doodadObjId, count)` -> `CharacterCraft.Craft` (materials,
//! actability limit, doodad permission) casts the craft skill on the doodad; the cast's
//! `CraftEffect` calls `EndCraft`, which hands out `craft_products` and consumes
//! `craft_materials`, and `Skill.EndSkill` charges the labor through
//! `SCCharacterLaborPowerChangedPacket` (`action = actabilityGroupId`, `point` = proficiency
//! gained).
//!
//! Preparation per recipe, with the GM commands of `systems/services/commands`:
//! `/item add self <id> <count>` fills the materials, `/labor <amount> [group]` tops up labor
//! and actability points, the real 1.2 `CSUpgradeExpertLimitPacket` raises the expert-limit
//! step when a recipe needs more points than the current cap allows, and
//! `/spawn doodad <templateId>` places `crafts.req_doodad_id` 3 m in front of the bot when the
//! recipe needs one. Recipes whose materials / products are missing from the 1.2 `items` table
//! record `unsupported` (the GM give cannot create them either).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use aaemu_commons::{PacketStream, Unmarshal};
use aaemu_game::opcodes::{cs, pp, sc};
use aaemu_game::systems::economy::error_message as econ_err;
use aaemu_game::systems::economy::inventory::actability_labor_cost_multiplier;
use aaemu_game::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};
use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};

use crate::compactdb::{DEFAULT_COMPACT_DB, DEFAULT_COMPACT_DB_FROM_RUST};
use crate::game::{chat_text, GameClient, GameState, Packet, LEVEL_PROXY};
use crate::inventory::{Bag, SLOT_TYPE_INVENTORY};
use crate::login::LoginClient;
use crate::record::Recorder;
use crate::scenario::BotConfig;

/// Packet-pump chunk used by every bounded wait.
const PUMP: Duration = Duration::from_millis(250);
/// Quiet window after a GM command's first reply.
const COMMAND_QUIET: Duration = Duration::from_millis(350);
/// Default per-recipe timeout (`--step-timeout-secs`); the longest 1.2 craft cast is 30 s and
/// the longest cooldown 1 s, so the default leaves plenty of room.
pub const DEFAULT_STEP_TIMEOUT: Duration = Duration::from_secs(90);
/// How long to wait for an item-give / labor / spawn to show its effect.
const GM_EFFECT_WAIT: Duration = Duration::from_secs(5);
/// Quiet window after the first craft completion signal (item task / cast end / error).
const CRAFT_SETTLE_QUIET: Duration = Duration::from_millis(350);
/// Bag stacks that trigger a full bag trim at the next recipe (`destroy_templates` missed a
/// destroy, or an earlier run was killed mid-recipe).
const BAG_TRIM_THRESHOLD: usize = 35;
/// How many expert-limit upgrades one recipe may trigger.
const MAX_EXPERT_UPGRADES: usize = 40;
/// `/item add` accepts at most this amount per command (`NumericSubCommandParameter` range).
const MAX_ITEM_ADD_AMOUNT: i32 = 1000;
/// `/labor` parses its amount as `i16` (`add_labor.rs`).
const MAX_LABOR_AMOUNT: i32 = 32_000;
/// Labor power kept in reserve so one `/labor` covers many recipes (still below the i16 cap).
const LABOR_HEADROOM: i32 = 20_000;
/// Below this reserve (plus the recipe's cost) the labor is topped up again.
const LABOR_REMAINING_MIN: i32 = 5_000;
/// Doodads within this radius of the bot are considered leftovers of the sweep's own
/// `/spawn doodad` calls and are removed at sweep start (they all land at the same 3 m spot
/// and would block each other's line of sight, `skills/validation.rs::check` obstacle ray).
const DOODAD_CLEANUP_RADIUS: f32 = 6.0;

// -------------------------------------------------------------------------------------------
// Compact database: craft plan data
// -------------------------------------------------------------------------------------------

/// One `craft_materials` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanMaterial {
    pub item_id: u32,
    pub amount: i32,
}

/// One `craft_products` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanProduct {
    pub item_id: u32,
    pub amount: i32,
    /// Raw `craft_products.rate` (100 = always, 50 in the 1.2 data). The 1.2 server hands out
    /// every product row regardless (`craft.rs::hand_out_products`).
    pub rate: i32,
}

/// The `SkillTemplate` facts `CharacterCraft` reads for a craft skill.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanSkill {
    pub id: u32,
    pub consume_lp: i32,
    pub actability_group: i32,
    pub casting_time: i32,
    pub cooldown_time: i32,
    /// `unit_reqs` `Level` minimum of the skill (`kind_id` 1, `owner_type` `Skill`); 0 = none.
    /// The runner reaches it with `/level` (skill 16766, the Commerce craft skill, needs 10).
    pub level_min: i32,
    /// `skills.target_type_id` (`SkillTargetType`): 8 = Doodad (normal crafts), 19 = Building
    /// and 0 = none are not drivable by the sweep.
    pub target_type: i32,
    /// Whether any `skill_effects` row of the skill resolves to a `CraftEffect` (the effect
    /// that calls `CharacterCraft.EndCraft`). Skills whose completion runs through another
    /// effect (e.g. an `InteractionEffect` + doodad func, craft 2815/2816) cannot be driven
    /// with `CSExecuteCraft`.
    pub craft_effect: bool,
}

/// One `crafts` row with its materials, products and skill facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CraftPlan {
    pub id: u32,
    pub title: String,
    pub cast_delay_ms: i32,
    pub req_doodad_id: u32,
    pub actability_limit: i32,
    pub skill_id: u32,
    /// `craft_pack_crafts` lists the craft (a trade pack result: excluded from samples).
    pub is_pack: bool,
    pub skill: PlanSkill,
    pub materials: Vec<PlanMaterial>,
    pub products: Vec<PlanProduct>,
}

/// Whether the sweep can drive a recipe end to end. Only data gaps block a recipe; anything
/// the runner itself can do (any number of material / product rows, trade packs that
/// auto-equip, actability limits that need expert-limit steps) is allowed:
///
/// * the craft has a skill (`crafts.skill_id != 0`);
/// * every `craft_materials.item_id` has an `items` row (the GM `/item add` refuses templates
///   that do not exist; a craft without material rows passes);
/// * every `craft_products.item_id` has an `items` row (`EndCraft` creates products the same
///   way; a craft without product rows passes);
/// * `crafts.req_doodad_id` has a `doodad_almighties` row (0 = portable, no station needed);
/// * the skill's `actability_group_id` has an `actability_groups` row (0 = no proficiency).
pub fn craft_is_usable(
    plan: &CraftPlan,
    item_ids: &HashSet<u32>,
    doodad_ids: &HashSet<u32>,
    actability_ids: &HashSet<u32>,
) -> bool {
    plan.skill_id != 0
        && plan.materials.iter().all(|m| item_ids.contains(&m.item_id))
        && plan.products.iter().all(|p| item_ids.contains(&p.item_id))
        && (plan.req_doodad_id == 0 || doodad_ids.contains(&plan.req_doodad_id))
        && (plan.skill.actability_group <= 0
            || actability_ids.contains(&(plan.skill.actability_group as u32)))
}

/// Read-only access to the craft tables of `compact.sqlite3`.
pub struct CraftDb {
    crafts: HashMap<u32, CraftPlan>,
    item_ids: HashSet<u32>,
    doodad_ids: HashSet<u32>,
    actability_ids: HashSet<u32>,
    /// `expert_limits.up_limit` in `ORDER BY up_limit ASC` (the server's step order).
    pub expert_caps: Vec<i32>,
    path: PathBuf,
}

impl CraftDb {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn =
            rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .with_context(|| format!("opening compact database {}", path.display()))?;

        let mut crafts: HashMap<u32, CraftPlan> = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT c.id, COALESCE(c.title,''), COALESCE(c.cast_delay,0), \
                 COALESCE(c.req_doodad_id,0), COALESCE(c.actability_limit,0), \
                 COALESCE(c.skill_id,0), \
                 CASE WHEN EXISTS (SELECT 1 FROM craft_pack_crafts p WHERE p.craft_id = c.id) \
                      THEN 1 ELSE 0 END, \
                 COALESCE(s.consume_lp,0), COALESCE(s.actability_group_id,0), \
                 COALESCE(s.casting_time,0), COALESCE(s.cooldown_time,0), \
                 COALESCE(s.target_type_id,0) \
                 FROM crafts c LEFT JOIN skills s ON s.id = c.skill_id",
            )?;
            let rows = stmt.query_map([], |r| {
                let id: u32 = r.get(0)?;
                Ok(CraftPlan {
                    id,
                    title: r.get(1)?,
                    cast_delay_ms: r.get(2)?,
                    req_doodad_id: r.get(3)?,
                    actability_limit: r.get(4)?,
                    skill_id: r.get(5)?,
                    is_pack: r.get::<_, i64>(6)? != 0,
                    skill: PlanSkill {
                        id: r.get(5)?,
                        consume_lp: r.get(7)?,
                        actability_group: r.get(8)?,
                        casting_time: r.get(9)?,
                        cooldown_time: r.get(10)?,
                        level_min: 0,
                        target_type: r.get(11)?,
                        craft_effect: false,
                    },
                    materials: Vec::new(),
                    products: Vec::new(),
                })
            })?;
            for row in rows {
                let plan = row?;
                crafts.insert(plan.id, plan);
            }
        }
        {
            let mut stmt =
                conn.prepare("SELECT craft_id, item_id, COALESCE(amount,1) FROM craft_materials")?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, u32>(0)?,
                    PlanMaterial {
                        item_id: r.get(1)?,
                        amount: r.get(2)?,
                    },
                ))
            })?;
            for row in rows {
                let (craft_id, material) = row?;
                if let Some(plan) = crafts.get_mut(&craft_id) {
                    plan.materials.push(material);
                }
            }
        }
        {
            let mut stmt = conn.prepare(
                "SELECT craft_id, item_id, COALESCE(amount,1), COALESCE(rate,100) \
                 FROM craft_products",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, u32>(0)?,
                    PlanProduct {
                        item_id: r.get(1)?,
                        amount: r.get(2)?,
                        rate: r.get(3)?,
                    },
                ))
            })?;
            for row in rows {
                let (craft_id, product) = row?;
                if let Some(plan) = crafts.get_mut(&craft_id) {
                    plan.products.push(product);
                }
            }
        }
        let mut item_ids = HashSet::new();
        {
            let mut stmt = conn.prepare("SELECT id FROM items")?;
            let rows = stmt.query_map([], |r| r.get::<_, u32>(0))?;
            for row in rows {
                item_ids.insert(row?);
            }
        }
        let mut doodad_ids = HashSet::new();
        {
            let mut stmt = conn.prepare("SELECT id FROM doodad_almighties")?;
            let rows = stmt.query_map([], |r| r.get::<_, u32>(0))?;
            for row in rows {
                doodad_ids.insert(row?);
            }
        }
        let mut actability_ids = HashSet::new();
        {
            let mut stmt = conn.prepare("SELECT id FROM actability_groups")?;
            let rows = stmt.query_map([], |r| r.get::<_, u32>(0))?;
            for row in rows {
                actability_ids.insert(row?);
            }
        }
        let mut expert_caps = Vec::new();
        {
            let mut stmt =
                conn.prepare("SELECT up_limit FROM expert_limits ORDER BY up_limit ASC")?;
            let rows = stmt.query_map([], |r| r.get::<_, i32>(0))?;
            for row in rows {
                expert_caps.push(row?);
            }
        }
        let mut skill_levels: HashMap<u32, i32> = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT owner_id, value1 FROM unit_reqs WHERE owner_type = 'Skill' AND kind_id = 1",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, i32>(1)?)))?;
            for row in rows {
                let (skill_id, level) = row?;
                let entry = skill_levels.entry(skill_id).or_insert(0);
                *entry = (*entry).max(level);
            }
        }
        let mut craft_effect_skills = HashSet::new();
        {
            let mut stmt = conn.prepare(
                "SELECT se.skill_id FROM skill_effects se JOIN effects ef ON ef.id = se.effect_id \
                 WHERE ef.actual_type = 'CraftEffect'",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, u32>(0))?;
            for row in rows {
                craft_effect_skills.insert(row?);
            }
        }
        for plan in crafts.values_mut() {
            if let Some(level) = skill_levels.get(&plan.skill_id) {
                plan.skill.level_min = *level;
            }
            plan.skill.craft_effect = craft_effect_skills.contains(&plan.skill_id);
        }
        Ok(Self {
            crafts,
            item_ids,
            doodad_ids,
            actability_ids,
            expert_caps,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every craft in id order.
    pub fn all_crafts(&self) -> Vec<CraftPlan> {
        let mut plans: Vec<CraftPlan> = self.crafts.values().cloned().collect();
        plans.sort_by_key(|p| p.id);
        plans
    }

    /// One craft by id.
    pub fn craft(&self, id: u32) -> Option<&CraftPlan> {
        self.crafts.get(&id)
    }

    pub fn item_exists(&self, item_id: u32) -> bool {
        self.item_ids.contains(&item_id)
    }

    pub fn doodad_exists(&self, doodad_id: u32) -> bool {
        self.doodad_ids.contains(&doodad_id)
    }

    pub fn actability_exists(&self, group: u32) -> bool {
        self.actability_ids.contains(&group)
    }

    /// [`craft_is_usable`] against this database's id sets.
    pub fn is_usable(&self, plan: &CraftPlan) -> bool {
        craft_is_usable(plan, &self.item_ids, &self.doodad_ids, &self.actability_ids)
    }

    /// Every craft the sweep can drive, in id order (the `--all` selection).
    pub fn usable(&self) -> Vec<CraftPlan> {
        let mut plans: Vec<CraftPlan> = self
            .crafts
            .values()
            .filter(|p| self.is_usable(p))
            .cloned()
            .collect();
        plans.sort_by_key(|p| p.id);
        plans
    }

    /// The `--all` selection gate counts per stage, in evaluation order (the stages of
    /// [`craft_is_usable`]). Printed at the start of a `--all` / `--sample` run so the data
    /// gaps behind a smaller-than-expected set are visible.
    pub fn filter_chain(&self) -> Vec<(&'static str, usize)> {
        let mut current: Vec<CraftPlan> = self.all_crafts();
        let mut out = Vec::new();
        let mut stage = |name: &'static str, current: &mut Vec<CraftPlan>| {
            out.push((name, current.len()));
        };
        stage("all crafts", &mut current);
        current.retain(|p| p.skill_id != 0);
        stage("crafts.skill_id != 0", &mut current);
        current.retain(|p| p.materials.iter().all(|m| self.item_exists(m.item_id)));
        stage("every craft_materials.item_id in items", &mut current);
        current.retain(|p| p.products.iter().all(|p2| self.item_exists(p2.item_id)));
        stage("every craft_products.item_id in items", &mut current);
        current.retain(|p| p.req_doodad_id == 0 || self.doodad_exists(p.req_doodad_id));
        stage(
            "crafts.req_doodad_id in doodad_almighties (0 = pass)",
            &mut current,
        );
        current.retain(|p| {
            p.skill.actability_group <= 0 || self.actability_exists(p.skill.actability_group as u32)
        });
        stage(
            "skills.actability_group_id in actability_groups (0 = pass)",
            &mut current,
        );
        out
    }
}

/// Opens `--compact-db` (or the default game-data path when it exists).
pub fn open_craft_db(compact_db: Option<&Path>) -> anyhow::Result<CraftDb> {
    if let Some(path) = compact_db {
        if !path.exists() {
            bail!("compact database {} not found", path.display());
        }
        return CraftDb::open(path);
    }
    for candidate in [DEFAULT_COMPACT_DB, DEFAULT_COMPACT_DB_FROM_RUST] {
        let path = Path::new(candidate);
        if path.exists() {
            return CraftDb::open(path);
        }
    }
    bail!(
        "no compact database found; pass --compact-db (tried {DEFAULT_COMPACT_DB}, \
         {DEFAULT_COMPACT_DB_FROM_RUST})"
    )
}

/// A deterministic sample of `n` usable recipes spread over the actability groups: the plans
/// are grouped by `skills.actability_group_id` (0 = none), groups and plans keep their id
/// order, and groups are polled round-robin until `n` is reached.
pub fn sample_spread(db: &CraftDb, n: usize) -> Vec<CraftPlan> {
    let plans = db.all_crafts();
    let mut groups: BTreeMap<i32, Vec<&CraftPlan>> = BTreeMap::new();
    for plan in &plans {
        if db.is_usable(plan) {
            groups
                .entry(plan.skill.actability_group.max(0))
                .or_default()
                .push(plan);
        }
    }
    for group in groups.values_mut() {
        group.sort_by_key(|p| p.id);
    }
    let keys: Vec<i32> = groups.keys().copied().collect();
    let mut next: HashMap<i32, usize> = keys.iter().map(|k| (*k, 0)).collect();
    let mut out = Vec::new();
    loop {
        let mut progressed = false;
        for key in &keys {
            if out.len() >= n {
                return out;
            }
            let group = &groups[key];
            let index = next[key];
            if index < group.len() {
                out.push(group[index].clone());
                next.insert(*key, index + 1);
                progressed = true;
            }
        }
        if !progressed {
            return out;
        }
    }
}

// -------------------------------------------------------------------------------------------
// Results
// -------------------------------------------------------------------------------------------

/// How a recipe ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CraftOutcome {
    /// Every expected product row arrived in the bag.
    Crafted,
    /// The craft completed but the bag delta did not match the expected products.
    ProductMismatch,
    /// The server answered with an `SCErrorMsgPacket` or a non-zero `SkillResult`.
    ServerRejected,
    /// A required doodad could not be spawned / found.
    NoDoodad,
    /// A material could not be created in the bag (missing template / bag full).
    MaterialUnavailable,
    /// The actability limit could not be reached (expert-limit upgrade refused or capped).
    ActabilityBlocked,
    /// The recipe cannot be driven at all (no skill row, missing data).
    Unsupported,
    /// No craft response within the step timeout.
    Timeout,
    /// Connection or protocol error.
    Error,
}

impl CraftOutcome {
    pub const ALL: [CraftOutcome; 9] = [
        CraftOutcome::Crafted,
        CraftOutcome::ProductMismatch,
        CraftOutcome::ServerRejected,
        CraftOutcome::NoDoodad,
        CraftOutcome::MaterialUnavailable,
        CraftOutcome::ActabilityBlocked,
        CraftOutcome::Unsupported,
        CraftOutcome::Timeout,
        CraftOutcome::Error,
    ];

    pub fn name(self) -> &'static str {
        match self {
            CraftOutcome::Crafted => "crafted",
            CraftOutcome::ProductMismatch => "product_mismatch",
            CraftOutcome::ServerRejected => "server_rejected",
            CraftOutcome::NoDoodad => "no_doodad",
            CraftOutcome::MaterialUnavailable => "material_unavailable",
            CraftOutcome::ActabilityBlocked => "actability_blocked",
            CraftOutcome::Unsupported => "unsupported",
            CraftOutcome::Timeout => "timeout",
            CraftOutcome::Error => "error",
        }
    }
}

/// An expected item amount (materials / products).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemAmount {
    pub item_id: u32,
    pub amount: i32,
}

/// A template's bag count change over one recipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delta {
    pub item_id: u32,
    pub delta: i32,
}

/// One `--out` JSONL line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CraftResult {
    pub craft_id: u32,
    pub title: String,
    pub character: String,
    pub actability_group: i32,
    pub actability_limit: i32,
    pub result: CraftOutcome,
    /// Human-readable detail of `result` (missing product, server error name, ...).
    pub reason: String,
    pub materials_expected: Vec<ItemAmount>,
    pub materials_consumed: Vec<Delta>,
    pub products_expected: Vec<ItemAmount>,
    pub products_received: Vec<Delta>,
    /// Trade-pack products the server auto-equipped (they never enter the bag): ids and counts
    /// read off the equipment item tasks.
    pub products_equipped: Vec<Delta>,
    /// Labor the skill charges at the recipe's proficiency step (`ConsumeLaborPower` x step
    /// multiplier), and what the `SCCharacterLaborPowerChangedPacket` of the craft reported.
    pub labor_expected: i32,
    pub labor_spent: i32,
    pub proficiency_gained: i32,
    /// `CSExecuteCraft` -> first cast response (`SCSkillStarted` / `SCSkillFired` / item task).
    pub cast_time_ms: Option<u64>,
    /// `crafts.cast_delay` (the client progress animation).
    pub cast_delay_ms: i32,
    /// `skills.casting_time` of the craft skill (the server-side cast).
    pub skill_cast_time_ms: i32,
    /// Non-zero `SkillResult` of the server's cast validation.
    pub server_result: Option<u8>,
    /// `SCErrorMsg` codes seen while crafting.
    pub server_errors: Vec<String>,
    pub messages: Vec<String>,
    pub elapsed_ms: u64,
}

/// The whole run's result.
#[derive(Debug, Default)]
pub struct RunReport {
    pub results: Vec<CraftResult>,
    /// The connection was lost; the run stopped after the last written line.
    pub aborted: bool,
}

/// CLI options of the `craft` command.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// `--craft <id>...` (repeatable).
    pub craft_ids: Vec<u32>,
    /// `--all`: every craft of the database, in id order.
    pub all: bool,
    /// `--sample N`: N usable recipes spread over the actability groups.
    pub sample: Option<usize>,
    /// `--limit N`: cap the selected list.
    pub limit: Option<usize>,
    pub new_character: bool,
    pub race: u8,
    pub gender: u8,
    pub step_timeout: Duration,
    pub compact_db: Option<PathBuf>,
    pub out: Option<PathBuf>,
    /// `--keep-products`: do not destroy the bag deltas (materials / products) after a recipe.
    pub keep_products: bool,
}

/// `--summary`: counts by result and the top failure reasons (digit runs collapsed).
pub fn render_summary(report: &RunReport) -> String {
    let mut out = String::from("[craft] results:");
    for outcome in CraftOutcome::ALL {
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
        if r.result == CraftOutcome::Crafted {
            continue;
        }
        *reasons.entry(reason_key(&r.reason)).or_default() += 1;
    }
    let mut top: Vec<(&String, &usize)> = reasons.iter().collect();
    top.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    if !top.is_empty() {
        out.push_str("\n[craft] top failure reasons:");
        for (reason, n) in top.iter().take(8) {
            out.push_str(&format!("\n  {n}x {reason}"));
        }
    }
    out
}

/// Collapses digit runs so per-craft reasons (`craft 63`, `craft 61`) count together.
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

// -------------------------------------------------------------------------------------------
// Accounting
// -------------------------------------------------------------------------------------------

/// The per-template count change from `before` to `after` (sorted by template id).
pub fn bag_delta(before: &Bag, after: &Bag) -> Vec<Delta> {
    let mut counts: BTreeMap<u32, i32> = BTreeMap::new();
    for item in before.values() {
        *counts.entry(item.template_id).or_default() -= item.count;
    }
    for item in after.values() {
        *counts.entry(item.template_id).or_default() += item.count;
    }
    counts
        .into_iter()
        .filter(|(_, delta)| *delta != 0)
        .map(|(item_id, delta)| Delta { item_id, delta })
        .collect()
}

/// Whether every `expected` product row is covered by the positive bag deltas; returns the
/// missing amounts.
pub fn missing_products(expected: &[ItemAmount], received: &[Delta]) -> Vec<ItemAmount> {
    expected
        .iter()
        .filter_map(|want| {
            let got: i32 = received
                .iter()
                .filter(|d| d.item_id == want.item_id)
                .map(|d| d.delta)
                .sum();
            (got < want.amount).then_some(ItemAmount {
                item_id: want.item_id,
                amount: want.amount - got,
            })
        })
        .collect()
}

/// `Skill.EndSkill` labor cost of one craft at `step`: `ConsumeLaborPower` scaled by
/// `Actability.GetLaborCostMultiplier` (`skills/validation.rs::labor_cost`).
pub fn labor_cost(consume_lp: i32, step: u8) -> i32 {
    let labor = consume_lp.max(0);
    if labor == 0 {
        return 0;
    }
    let cost =
        (f64::from(labor) * f64::from(actability_labor_cost_multiplier(step))).round_ties_even();
    (cost as i32).max(1)
}

/// `ErrorMessageType` name of a code for result reasons.
fn error_name(code: i16) -> &'static str {
    match code {
        econ_err::INVALID => "Invalid",
        econ_err::NOT_ENOUGH_LABOR_POWER => "NotEnoughLaborPower",
        econ_err::NOT_ENOUGH_SPACE => "NotEnoughSpace",
        econ_err::ACTABILITY_NOT_ENOUGH_POINT => "ActabilityNotEnoughPoint",
        econ_err::CRAFT_PERMISSION_DENY => "CraftPermissionDeny",
        econ_err::NOT_ENOUGH_REQUIRED_ITEM => "NotEnoughRequiredItem",
        econ_err::BACKPACK_OCCUPIED => "BackpackOccupied",
        econ_err::CRAFT_CANT_ACT_ANY_MORE => "CraftCantActAnyMore",
        econ_err::CRAFT_LOW_EXPERT => "CraftLowExpert",
        econ_err::NOT_ENOUGH_EXPAND_ITEM => "NotEnoughExpandItem",
        econ_err::BAG_FULL => "BagFull",
        _ => "",
    }
}

/// `SCErrorMsgPacket` carries the same error in `error1` / `error2` for the primary errors, and
/// a pair like `(148 CraftCantActAnyMore, 315 BackpackOccupied)` where the second names the
/// failed check.
fn error_text(e1: i16, e2: i16) -> String {
    if e2 != 0 && e2 != e1 {
        let n1 = error_name(e1);
        let n2 = error_name(e2);
        let mut text = format!("SCErrorMsg {e1}/{e2}");
        if !n1.is_empty() || !n2.is_empty() {
            text.push_str(&format!(" ({n1}/{n2})"));
        }
        text
    } else {
        let name = error_name(e1);
        if name.is_empty() {
            format!("SCErrorMsg {e1}")
        } else {
            format!("SCErrorMsg {e1} ({name})")
        }
    }
}

// -------------------------------------------------------------------------------------------
// Packet readers
// -------------------------------------------------------------------------------------------

/// `SCCharacterLaborPowerChangedPacket`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LaborChange {
    amount: i32,
    action: i32,
    point: i32,
    step: u8,
}

fn parse_labor(p: &Packet) -> Option<LaborChange> {
    if !p.is_sc(sc::SCCharacterLaborPowerChangedPacket) {
        return None;
    }
    let mut s = p.stream();
    Some(LaborChange {
        amount: s.read_i32().ok()?,
        action: s.read_i32().ok()?,
        point: s.read_i32().ok()?,
        step: s.read_u8().ok()?,
    })
}

/// `SCActabilityPacket` rows: `(id, point, step)`.
fn parse_actability(p: &Packet) -> Option<Vec<(u32, i32, u8)>> {
    if !p.is_sc(sc::SCActabilityPacket) {
        return None;
    }
    let mut s = p.stream();
    let _last = s.read_bool().ok()?;
    let n = s.read_u8().ok()?;
    let mut rows = Vec::with_capacity(usize::from(n));
    for _ in 0..n {
        rows.push((s.read_u32().ok()?, s.read_i32().ok()?, s.read_u8().ok()?));
    }
    Some(rows)
}

/// `SCExpertLimitModifiedPacket(isUpgrade, id, step)`.
fn parse_expert_limit(p: &Packet) -> Option<(bool, u32, u8)> {
    if !p.is_sc(sc::SCExpertLimitModifiedPacket) {
        return None;
    }
    let mut s = p.stream();
    Some((s.read_bool().ok()?, s.read_u32().ok()?, s.read_u8().ok()?))
}

/// `SCErrorMsgPacket.error1` / `error2`.
fn parse_error(p: &Packet) -> Option<(i16, i16)> {
    if !p.is_sc(sc::SCErrorMsgPacket) {
        return None;
    }
    let mut s = p.stream();
    Some((s.read_i16().ok()?, s.read_i16().ok()?))
}

/// The parts of `SCSkillStartedPacket` the sweep needs.
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
    let _real = s.read_u16().ok()?;
    let _base = s.read_u16().ok()?;
    let _synergy = s.read_u8().ok()?;
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

fn parse_skill_ended(p: &Packet) -> Option<u16> {
    if !p.is_sc(sc::SCSkillEndedPacket) {
        return None;
    }
    p.stream().read_u16().ok()
}

/// An item a task created / took into an equipment slot (`slot_type` 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EquippedItem {
    pub item_id: u64,
    pub template_id: u32,
    pub slot: u8,
}

/// Equipment-slot effects of one `SCItemTaskSuccessPacket`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EquipmentChanges {
    pub created: Vec<EquippedItem>,
    /// Slots whose item left the equipment container (`REMOVE` / `SEIZE` / a swap out).
    pub removed_slots: Vec<u8>,
}

/// The equipment rows of an `SCItemTaskSuccessPacket`. Trade-pack products are auto-equipped
/// (`craft.rs::hand_out_products` -> `inventory::try_equip_new_backpack`), so they never show
/// up in the bag: the sweep reads their item ids off the tasks to report and clean them up.
/// The walk mirrors `crate::inventory::apply_item_task_success` (kept separate because that
/// helper only surfaces bag rows).
pub fn equipment_tasks(body: &[u8]) -> anyhow::Result<EquipmentChanges> {
    use aaemu_game::models::item_task::item_action;
    const SLOT_TYPE_EQUIPMENT: u8 = 1;
    let mut s = PacketStream::from_bytes(body.to_vec());
    let _action = s.read_u8()?;
    let count = s.read_u8()?;
    let mut out = EquipmentChanges::default();
    for _ in 0..count {
        let action = s.read_u8()?;
        match action {
            item_action::CHANGE_MONEY_AMOUNT
            | item_action::CHANGE_BANK_MONEY_AMOUNT
            | item_action::CHANGE_AA_POINT
            | item_action::CHANGE_BANK_AA_POINT => {
                s.read_i32()?;
            }
            item_action::CHANGE_GAME_POINT => {
                s.read_u8()?;
                s.read_i32()?;
            }
            item_action::CHANGE_AUTO_USE_AA_POINT => {
                s.read_u8()?;
            }
            item_action::ADD_STACK => {
                let _slot_type = s.read_u8()?;
                let _slot = s.read_u8()?;
                s.read_u64()?;
                s.read_i32()?;
                s.read_u32()?;
            }
            item_action::CREATE | item_action::TAKE | item_action::CHANGE_OWNER => {
                let slot_type = s.read_u8()?;
                let slot = s.read_u8()?;
                let template_id = s.read_u32()?;
                let item_id = s.read_u64()?;
                s.read_u8()?; // grade
                s.read_u8()?; // flags
                s.read_i32()?; // count
                let len = s.read_i16()?;
                anyhow::ensure!(len >= 0, "negative task detail length {len}");
                s.read_bytes(len as usize)?;
                s.read_i64()?;
                s.read_i32()?;
                s.read_u32()?;
                s.read_u8()?;
                s.read_i64()?;
                s.read_i64()?;
                if slot_type == SLOT_TYPE_EQUIPMENT {
                    out.created.push(EquippedItem {
                        item_id,
                        template_id,
                        slot,
                    });
                }
            }
            item_action::REMOVE => {
                let slot_type = s.read_u8()?;
                let slot = s.read_u8()?;
                s.read_u64()?;
                s.read_u32()?;
                if slot_type == SLOT_TYPE_EQUIPMENT {
                    out.removed_slots.push(slot);
                }
            }
            item_action::SWAP_SLOT => {
                let from_type = s.read_u8()?;
                let from_slot = s.read_u8()?;
                let to_type = s.read_u8()?;
                let to_slot = s.read_u8()?;
                let _from_id = s.read_u64()?;
                let to_id = s.read_u64()?;
                s.read_i32()?;
                if from_type == SLOT_TYPE_EQUIPMENT {
                    out.removed_slots.push(from_slot);
                }
                if to_type == SLOT_TYPE_EQUIPMENT {
                    out.created.push(EquippedItem {
                        item_id: to_id,
                        template_id: 0,
                        slot: to_slot,
                    });
                }
            }
            item_action::UPDATE_DETAIL => {
                s.read_u8()?;
                s.read_u8()?;
                s.read_u64()?;
                let len = s.read_i16()?;
                anyhow::ensure!(len >= 0, "negative task detail length {len}");
                s.read_bytes(len as usize)?;
            }
            item_action::SET_FLAGS_BITS => {
                s.read_u8()?;
                s.read_u8()?;
                s.read_u64()?;
                s.read_u8()?;
                s.read_u64()?;
            }
            item_action::UPDATE_FLAGS => {
                s.read_u8()?;
                s.read_u8()?;
                s.read_u64()?;
                s.read_u8()?;
                s.read_bool()?;
                s.read_bool()?;
                s.read_bool()?;
                s.read_i64()?;
                s.read_i64()?;
            }
            item_action::REMOVE_CRAFTING => {
                s.read_u64()?;
            }
            item_action::SEIZE => {
                let slot_type = s.read_u8()?;
                let slot = s.read_u8()?;
                s.read_u64()?;
                if slot_type == SLOT_TYPE_EQUIPMENT {
                    out.removed_slots.push(slot);
                }
            }
            item_action::CHANGE_GRADE => {
                s.read_u8()?;
                s.read_u8()?;
                s.read_u64()?;
                s.read_u8()?;
            }
            other => anyhow::bail!("unknown item task action {other}"),
        }
    }
    Ok(out)
}

/// `SCCharacterStatePacket` -> `labor_power` (the fixed head plus the full `Character.Write`
/// body; the equipment rows are self-delimiting `Item.Write` records).
pub fn parse_state_labor(body: &[u8]) -> anyhow::Result<i32> {
    Ok(parse_state_equipment(body)?.0)
}

/// `SCCharacterStatePacket` -> `(labor_power, equipment rows, active abilities)`. The equipment
/// list is the container's `EquipmentItemSlot` order; `slot` is the row index
/// (`EquipmentItemSlot.Backpack` = 26); the abilities are `ability1..3` (`models/char_data.rs`).
pub fn parse_state_equipment(body: &[u8]) -> anyhow::Result<(i32, Vec<EquippedItem>, [u8; 3])> {
    use aaemu_game::models::unit_model::UnitCustomModelParams;
    let mut s = PacketStream::from_bytes(body.to_vec());
    s.read_u32()?; // instance id
    let guid_len = s.read_u16()?;
    s.read_bytes(usize::from(guid_len))?;
    s.read_i32()?; // rwd
    read_character_prefix(&mut s)?;
    let mut equipment = Vec::new();
    for slot in 0u8..28 {
        // `EquipmentContainer` size (`EQUIPMENT_CONTAINER_SIZE`); empty slots are `u32 0`.
        if matches!(slot, 19 | 20 | 24 | 25) {
            // Face / Hair / Body / Beard are `BodyPart` rows: `Item.Write`'s override writes
            // only the template id (no item id, details or timestamps).
            let template_id = s.read_u32()?;
            if template_id != 0 {
                equipment.push(EquippedItem {
                    item_id: 0,
                    template_id,
                    slot,
                });
            }
            continue;
        }
        if let Some(item) = crate::inventory::read_item(&mut s)? {
            equipment.push(EquippedItem {
                item_id: item.item_id,
                template_id: item.template_id,
                slot,
            });
        }
    }
    let abilities = [s.read_u8()?, s.read_u8()?, s.read_u8()?]; // ability1..3
    s.read_i64()?; // x
    s.read_i64()?; // y
    s.read_f32()?; // z
    let _model = UnitCustomModelParams::read(&mut s)?;
    Ok((s.read_i16()? as i32, equipment, abilities))
}

/// The fixed `Character.Write` head before the equipment rows.
fn read_character_prefix(s: &mut PacketStream) -> anyhow::Result<()> {
    s.read_u32()?; // id
    s.read_string()?; // name
    s.read_u8()?; // race
    s.read_u8()?; // gender
    s.read_u8()?; // level
    s.read_i32()?; // hp
    s.read_i32()?; // mp
    s.read_u32()?; // zone id
    s.read_u32()?; // faction
    s.read_string()?; // faction name
    s.read_u32()?; // expedition
    s.read_u32()?; // family
    Ok(())
}

// -------------------------------------------------------------------------------------------
// Session state (labor, actability)
// -------------------------------------------------------------------------------------------

/// What the runner tracks from the select snapshot and the live packets.
#[derive(Debug, Default)]
struct SessionState {
    /// Absolute labor power; `None` when the `SCCharacterStatePacket` could not be decoded.
    labor: Option<i32>,
    actability_points: HashMap<u32, i32>,
    actability_steps: HashMap<u32, u8>,
    /// Equipment items by `EquipmentItemSlot` index (26 = Backpack).
    equipment: HashMap<u8, EquippedItem>,
    /// Object ids of the doodads the sweep spawned itself (`/spawn doodad`); they are removed
    /// again so they never stack up at the spawn spot.
    owned_doodads: Vec<u32>,
    /// Item ids equipped when the session started (never the sweep's own auto-equipped
    /// gliders / trade packs).
    initial_equipment: HashSet<u64>,
}

impl SessionState {
    /// Applies one received packet to the session facts.
    fn observe(&mut self, p: &Packet) {
        if let Some(rows) = parse_actability(p) {
            for (id, point, step) in rows {
                self.actability_points.insert(id, point);
                self.actability_steps.insert(id, step);
            }
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
        if let Some(change) = parse_labor(p) {
            if let Some(labor) = &mut self.labor {
                *labor = i32::from((*labor as i16).wrapping_add(change.amount as i16));
            }
            if change.action > 0 && change.point != 0 {
                let entry = self
                    .actability_points
                    .entry(change.action as u32)
                    .or_insert(0);
                *entry += change.point;
                let step = self
                    .actability_steps
                    .entry(change.action as u32)
                    .or_insert(0);
                if change.step > 0 {
                    *step = change.step;
                }
            }
        }
        if let Some((upgrade, id, step)) = parse_expert_limit(p) {
            if upgrade {
                self.actability_steps.insert(id, step);
            }
        }
    }

    fn point(&self, group: u32) -> i32 {
        self.actability_points.get(&group).copied().unwrap_or(0)
    }

    fn step(&self, group: u32) -> u8 {
        self.actability_steps.get(&group).copied().unwrap_or(0)
    }

    /// Equipment items whose template is `template_id` (the server's material check counts
    /// the whole inventory, `Inv::items_count` -> `all_items_by_template(None, ..)`).
    fn equipped_count(&self, template_id: u32) -> i32 {
        self.equipment
            .values()
            .filter(|item| item.template_id == template_id)
            .count() as i32
    }

    /// Equipment items the sweep itself put into a slot (auto-equipped gliders / trade packs).
    fn given_equipped(&self) -> Vec<EquippedItem> {
        self.equipment
            .values()
            .filter(|item| !self.initial_equipment.contains(&item.item_id))
            .copied()
            .collect()
    }
}

// -------------------------------------------------------------------------------------------
// Session setup
// -------------------------------------------------------------------------------------------

/// The character a run drives (recorded in every result line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunCharacter {
    pub name: String,
    pub race: u8,
    pub gender: u8,
    pub start_level: u8,
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
        bail!("character creation failed (reason {reason}, name {name:?})");
    }
    Ok(())
}

/// Login, lobby burst, character create / select, spawn and notify, mirroring the quest
/// runner's setup. Also decodes the select snapshot's labor power and actability rows.
async fn setup_world(
    cfg: &BotConfig,
    rec: &Recorder,
    opts: &RunOptions,
) -> anyhow::Result<(GameClient, RunCharacter, SessionState)> {
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

    let mut session = SessionState::default();
    for p in select_packets.iter().chain(spawn_packets.iter()) {
        session.observe(p);
        if p.is_sc(sc::SCCharacterStatePacket) {
            if let Ok((labor, equipment, _abilities)) = parse_state_equipment(&p.body) {
                session.labor = Some(labor);
                for item in equipment {
                    session.equipment.insert(item.slot, item);
                }
            }
        }
    }
    session.initial_equipment = session
        .equipment
        .values()
        .map(|item| item.item_id)
        .collect();
    if let Some(info) = game.state.self_info.clone() {
        character = RunCharacter {
            name: info.name,
            race: info.race,
            gender: info.gender,
            start_level: info.level,
        };
    }
    Ok((game, character, session))
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

// -------------------------------------------------------------------------------------------
// GM command helpers
// -------------------------------------------------------------------------------------------

/// Sends a `/command` chat line and pumps packets through the session facts until `done` has
/// held for [`COMMAND_QUIET`] or `reply_wait` elapsed; returns the chat texts.
///
/// `done` matters because the area around the bot broadcasts world packets continuously, so a
/// "packet-quiet" heuristic never fires and every command would burn the full `reply_wait`.
/// A command without an observable effect waits `reply_wait` (used for `/level`).
async fn run_command(
    game: &mut GameClient,
    session: &mut SessionState,
    command: &str,
    reply_wait: Duration,
    mut done: impl FnMut(&GameState, &SessionState) -> bool,
) -> anyhow::Result<Vec<String>> {
    let text = format!(
        "{}{}",
        aaemu_game::systems::services::commands::COMMAND_PREFIX,
        command
    );
    game.chat(aaemu_game::systems::social::chat::chat_type::WHITE, &text)
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
        if let Some(p) = game.recv_within(PUMP).await? {
            session.observe(&p);
            if let Some(t) = chat_text(&p) {
                messages.push(t);
            }
            if done_since.is_none() && done(&game.state, session) {
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

/// `reply_wait` for a command whose effect is observed directly (item give, labor, spawn):
/// the success path breaks after [`COMMAND_QUIET`]; only a refusal waits the full window.
const COMMAND_WAIT: Duration = Duration::from_secs(2);

/// Waits (bounded) until `pred` holds over the game state and session facts, pumping packets
/// through `session`.
async fn wait_until(
    game: &mut GameClient,
    session: &mut SessionState,
    deadline: Instant,
    mut pred: impl FnMut(&GameState, &SessionState) -> bool,
) -> anyhow::Result<bool> {
    loop {
        if pred(&game.state, session) {
            return Ok(true);
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(pred(&game.state, session));
        }
        let wait = PUMP.min(deadline - now);
        if let Some(p) = game.recv_within(wait).await? {
            session.observe(&p);
        }
    }
}

// -------------------------------------------------------------------------------------------
// Per-recipe preparation
// -------------------------------------------------------------------------------------------

/// Gives `item_id` until the bag holds `amount`; returns `Err(reason)` when the GM give could
/// not create it (missing template / bag full).
async fn ensure_material(
    game: &mut GameClient,
    session: &mut SessionState,
    item_id: u32,
    amount: i32,
) -> anyhow::Result<Result<(), String>> {
    // The server's material check counts the whole inventory (`Inv::items_count` ->
    // `all_items_by_template(None, ..)`), so an auto-equipped glider / trade pack (the GM give
    // equips Backpack templates) satisfies the recipe as well.
    let count = game.state.bag_count(&[item_id]) + session.equipped_count(item_id);
    if count >= amount {
        return Ok(Ok(()));
    }
    let mut missing = amount - count;
    while missing > 0 {
        let chunk = missing.min(MAX_ITEM_ADD_AMOUNT);
        let messages = run_command(
            game,
            session,
            &format!("item add self {item_id} {chunk}"),
            COMMAND_WAIT,
            |st, s| st.bag_count(&[item_id]) + s.equipped_count(item_id) >= amount,
        )
        .await?;
        let mut after = game.state.bag_count(&[item_id]) + session.equipped_count(item_id);
        if after >= amount {
            return Ok(Ok(()));
        }
        if after <= count && messages.is_empty() {
            // A silent give can still be in flight (server lag / save tick): give it one more
            // command window before declaring it failed.
            let deadline = Instant::now() + COMMAND_WAIT;
            let _ = wait_until(game, session, deadline, |st, s| {
                st.bag_count(&[item_id]) + s.equipped_count(item_id) > count
            })
            .await;
            after = game.state.bag_count(&[item_id]) + session.equipped_count(item_id);
        }
        if after >= amount {
            return Ok(Ok(()));
        }
        if after <= count {
            return Ok(Err(format!(
                "GM /item add created no {item_id} (had {count}, wanted {amount}; chat: {})",
                messages.join(" | ")
            )));
        }
        missing = amount - after;
    }
    Ok(Ok(()))
}

/// Removes every doodad the sweep spawned so far, spawns a fresh station of `template_id`
/// next to the bot and returns its object id.
///
/// Spawned stations all land at the same 3 m spot in front of the bot; leaving them stacked
/// makes the server's obstacle ray for later doodad casts hit a sibling station instead of the
/// target (`skills/validation.rs::check`), which drops the cast silently. Only doodads the
/// sweep spawned itself are reused or removed.
async fn ensure_doodad(
    game: &mut GameClient,
    session: &mut SessionState,
    template_id: u32,
) -> anyhow::Result<Result<u32, String>> {
    remove_owned_doodads(game, session).await;
    let before: HashSet<u32> = game
        .state
        .doodads
        .values()
        .filter(|d| d.template_id == template_id)
        .map(|d| d.obj_id)
        .collect();
    let messages = run_command(
        game,
        session,
        &format!("spawn doodad {template_id}"),
        COMMAND_WAIT,
        |st, _| {
            st.doodads
                .values()
                .any(|d| d.template_id == template_id && !before.contains(&d.obj_id))
        },
    )
    .await?;
    let me = game.state.self_pos;
    let spawned = game
        .state
        .doodads
        .values()
        .filter(|d| d.template_id == template_id && !before.contains(&d.obj_id))
        .map(|d| (d.obj_id, dist2(d.pos, me)))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(obj_id, _)| obj_id);
    match spawned {
        Some(obj_id) => {
            session.owned_doodads.push(obj_id);
            Ok(Ok(obj_id))
        }
        None => Ok(Err(format!(
            "no doodad {template_id} after /spawn doodad (chat: {})",
            messages.join(" | ")
        ))),
    }
}

/// Removes the station the sweep spawned for a finished recipe.
async fn remove_doodad(
    game: &mut GameClient,
    session: &mut SessionState,
    obj_id: u32,
) -> anyhow::Result<()> {
    if !game.state.doodads.contains_key(&obj_id) {
        session.owned_doodads.retain(|id| *id != obj_id);
        return Ok(());
    }
    let _ = run_command(
        game,
        session,
        &format!("spawn remove {obj_id}"),
        COMMAND_WAIT,
        move |st, _| !st.doodads.contains_key(&obj_id),
    )
    .await?;
    session.owned_doodads.retain(|id| *id != obj_id);
    Ok(())
}

async fn remove_owned_doodads(game: &mut GameClient, session: &mut SessionState) {
    let owned = std::mem::take(&mut session.owned_doodads);
    for obj_id in owned {
        let _ = remove_doodad(game, session, obj_id).await;
    }
}

/// Removes every doodad within [`DOODAD_CLEANUP_RADIUS`] of the bot (leftovers from an earlier
/// sweep whose stations stack at the spawn spot). Run once at sweep start.
async fn clear_nearby_doodads(
    game: &mut GameClient,
    session: &mut SessionState,
) -> anyhow::Result<usize> {
    let me = game.state.self_pos;
    let radius2 = DOODAD_CLEANUP_RADIUS * DOODAD_CLEANUP_RADIUS;
    let targets: Vec<u32> = game
        .state
        .doodads
        .values()
        .filter(|d| dist2(d.pos, me) <= radius2)
        .map(|d| d.obj_id)
        .collect();
    for obj_id in &targets {
        let _ = remove_doodad(game, session, *obj_id).await;
    }
    if !targets.is_empty() {
        tracing::info!(
            "craft sweep: removed {} leftover doodad(s) within {DOODAD_CLEANUP_RADIUS} m",
            targets.len()
        );
    }
    Ok(targets.len())
}

fn nearest_own_doodad(game: &GameClient, template_id: u32) -> Option<u32> {
    let me = game.state.self_pos;
    game.state
        .doodads
        .values()
        .filter(|d| d.template_id == template_id)
        .map(|d| (d.obj_id, dist2(d.pos, me)))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(obj_id, _)| obj_id)
}

fn dist2(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
    let (dx, dy, dz) = (a.0 - b.0, a.1 - b.1, a.2 - b.2);
    dx * dx + dy * dy + dz * dz
}

/// Raises the actability points of `group` to at least `limit` with `/labor` and the real
/// client's `CSUpgradeExpertLimitPacket` when the current step's cap is reached.
async fn ensure_actability(
    game: &mut GameClient,
    session: &mut SessionState,
    caps: &[i32],
    group: u32,
    limit: i32,
) -> anyhow::Result<Result<(), String>> {
    if limit <= 0 || group == 0 {
        return Ok(Ok(()));
    }
    for _ in 0..MAX_EXPERT_UPGRADES {
        let point = session.point(group);
        if point >= limit {
            return Ok(Ok(()));
        }
        let step = session.step(group);
        let cap = caps.get(usize::from(step)).copied();
        let room = cap.map(|c| c.saturating_sub(point));
        let add = match room {
            Some(r) if r > 0 => (limit - point).min(r).min(MAX_LABOR_AMOUNT),
            _ => {
                // At the step cap: the client's upgrade packet is the only way further.
                let Some(cap) = cap else {
                    return Ok(Err(format!(
                        "actability {group} is at step {step} with no expert-limit cap (point {point}, need {limit})"
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
                    .await?;
                let deadline = Instant::now() + COMMAND_WAIT;
                let mut upgraded = false;
                while Instant::now() < deadline && !upgraded {
                    let now = Instant::now();
                    let wait = PUMP.min(deadline.saturating_duration_since(now));
                    match game.recv_within(wait).await? {
                        Some(p) => {
                            session.observe(&p);
                            if let Some((true, id, _)) = parse_expert_limit(&p) {
                                if id == group {
                                    upgraded = true;
                                }
                            }
                        }
                        None => break,
                    }
                }
                if !upgraded {
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
        let before = session.labor;
        run_command(
            game,
            session,
            &format!("labor {add} {group}"),
            COMMAND_WAIT,
            |_, s| s.labor != before,
        )
        .await?;
    }
    Ok(Err(format!(
        "actability {group} still below {limit} after {MAX_EXPERT_UPGRADES} upgrades"
    )))
}

/// Tops up labor power with a group-less `/labor` (no proficiency gain). The sweep keeps a
/// large headroom so one command lasts many recipes instead of one per recipe.
async fn ensure_labor(
    game: &mut GameClient,
    session: &mut SessionState,
    needed: i32,
) -> anyhow::Result<()> {
    let Some(labor) = session.labor else {
        // The select snapshot did not decode: add the recipe's cost with a margin.
        let add = (needed + LABOR_HEADROOM).clamp(1, MAX_LABOR_AMOUNT);
        run_command(
            game,
            session,
            &format!("labor {add}"),
            COMMAND_WAIT,
            |_, s| s.labor.is_some(),
        )
        .await?;
        return Ok(());
    };
    if labor >= needed + LABOR_REMAINING_MIN {
        return Ok(());
    }
    let add = (needed + LABOR_HEADROOM - labor).clamp(1, MAX_LABOR_AMOUNT);
    run_command(
        game,
        session,
        &format!("labor {add}"),
        COMMAND_WAIT,
        |_, s| s.labor != Some(labor),
    )
    .await?;
    Ok(())
}

// -------------------------------------------------------------------------------------------
// One craft
// -------------------------------------------------------------------------------------------

/// What the craft execution observed.
#[derive(Debug, Default)]
struct CraftAttempt {
    /// `CSExecuteCraft` -> first cast response.
    cast_time_ms: Option<u64>,
    labor_spent: i32,
    proficiency_gained: i32,
    server_result: Option<u8>,
    errors: Vec<(i16, i16)>,
    /// Items the craft's item tasks put into an equipment slot (trade-pack products).
    equipped: Vec<EquippedItem>,
    /// A response arrived (any of: cast packet, labor packet, item task, error).
    responded: bool,
}

/// Sends `CSExecuteCraftPacket` and watches the craft until the server's answer settles.
///
/// Completion signals are the packets that can only follow `EndCraft` / `EndSkill`: the item
/// tasks that consume the materials and hand out the products, the negative labor change of
/// `Skill.EndSkill`, our `SCSkillEndedPacket` timeline, an `SCErrorMsg`, or a non-zero
/// `SkillResult`. `SCSkillStarted` alone is only the start of the cast (`casting_time` can be
/// seconds), so the settle window opens on the first completion signal and drains the rest.
async fn execute_craft(
    game: &mut GameClient,
    session: &mut SessionState,
    plan: &CraftPlan,
    doodad_obj_id: u32,
    timeout: Duration,
) -> anyhow::Result<CraftAttempt> {
    let self_obj = game.state.self_obj;
    let mut body = PacketStream::new();
    body.write_u32(plan.id).write_bc(doodad_obj_id).write_i32(1);
    let sent_at = Instant::now();
    game.send_cs(cs::CSExecuteCraft, body.as_bytes()).await?;

    let deadline = sent_at + timeout;
    let mut attempt = CraftAttempt::default();
    let mut our_tl: Option<u16> = None;
    let mut done_at: Option<Instant> = None;
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        if done_at.is_some_and(|done| now.duration_since(done) >= CRAFT_SETTLE_QUIET) {
            break;
        }
        let wait = PUMP.min(deadline - now);
        let Some(p) = game.recv_within(wait).await? else {
            continue;
        };
        session.observe(&p);
        let mut signal = false;
        if let Some(started) = parse_skill_started(&p) {
            if started.skill_id == plan.skill_id && started.caster_obj == self_obj {
                our_tl = Some(started.tl);
                if started.result != 0 {
                    attempt.server_result = Some(started.result);
                    signal = true;
                }
            }
        }
        if let Some((skill_id, tl, caster)) = parse_skill_fired(&p) {
            if skill_id == plan.skill_id && caster == self_obj {
                our_tl = Some(tl);
            }
        }
        if let Some(tl) = parse_skill_ended(&p) {
            if Some(tl) == our_tl {
                signal = true;
            }
        }
        if let Some(change) = parse_labor(&p) {
            if change.amount < 0 {
                attempt.labor_spent += -change.amount;
                attempt.proficiency_gained += change.point.max(0);
                signal = true;
            }
        }
        if let Some(err) = parse_error(&p) {
            attempt.errors.push(err);
            signal = true;
        }
        if p.is_sc(sc::SCItemTaskSuccessPacket) {
            if let Ok(changes) = equipment_tasks(&p.body) {
                attempt.equipped.extend(changes.created);
            }
            signal = true;
        }
        if signal {
            attempt.responded = true;
            attempt
                .cast_time_ms
                .get_or_insert_with(|| sent_at.elapsed().as_millis() as u64);
            if done_at.is_none() {
                done_at = Some(Instant::now());
            }
        }
    }
    Ok(attempt)
}

// -------------------------------------------------------------------------------------------
// Run
// -------------------------------------------------------------------------------------------

enum Selected {
    Plan(Box<CraftPlan>),
    Unknown(u32),
}

fn select_plans(db: &CraftDb, opts: &RunOptions) -> anyhow::Result<Vec<Selected>> {
    let mut selected: Vec<Selected> = if !opts.craft_ids.is_empty() {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for id in &opts.craft_ids {
            if !seen.insert(*id) {
                continue;
            }
            match db.craft(*id) {
                Some(plan) => out.push(Selected::Plan(Box::new(plan.clone()))),
                None => out.push(Selected::Unknown(*id)),
            }
        }
        out
    } else if let Some(n) = opts.sample {
        sample_spread(db, n)
            .into_iter()
            .map(|p| Selected::Plan(Box::new(p)))
            .collect()
    } else if opts.all || opts.limit.is_some() {
        db.usable()
            .into_iter()
            .map(|p| Selected::Plan(Box::new(p)))
            .collect()
    } else {
        bail!("the craft command needs --craft <id> (repeatable), --sample <n> and/or --all");
    };
    if let Some(limit) = opts.limit {
        selected.truncate(limit);
    }
    Ok(selected)
}

/// Craft ids already recorded in an `--out` JSONL file (malformed lines are ignored), for
/// resuming an interrupted sweep.
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
        if let Some(id) = value.get("craft_id").and_then(serde_json::Value::as_u64) {
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

fn emit(out: &mut Option<BufWriter<File>>, result: &CraftResult) -> anyhow::Result<()> {
    let line = serde_json::to_string(result).context("serializing the craft result")?;
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

/// Runs the selected recipes; one JSON line per recipe goes to stdout and, when given, is
/// appended to `opts.out`. An existing `--out` file makes the run resume: every craft id
/// already present in it is skipped.
pub async fn run(cfg: &BotConfig, opts: &RunOptions) -> anyhow::Result<RunReport> {
    let db = open_craft_db(opts.compact_db.as_deref())?;
    if opts.all || opts.sample.is_some() {
        println!("[craft] --all gate (counts per stage):");
        for (stage, count) in db.filter_chain() {
            println!("[craft]   {count:5}  {stage}");
        }
    }
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
                "[craft] resuming from {}: {} already recorded, {} to run",
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
    let (mut game, character, mut session) = setup_world(cfg, &rec, opts).await?;
    // Safety net: the sweep stands next to spawned doodads, not in combat.
    game.gm("godmode true", Duration::from_secs(1)).await.ok();
    anyhow::ensure!(
        session.labor.is_some(),
        "the select snapshot did not decode labor power"
    );

    // Spawned stations all land at the same spot; leftovers from an earlier sweep block each
    // other's line of sight.
    if let Err(e) = clear_nearby_doodads(&mut game, &mut session).await {
        tracing::warn!("could not clear leftover doodads: {e:#}");
    }

    let mut raised_level = u32::from(character.start_level).max(1);
    let mut report = RunReport::default();
    for target in selected {
        if game.state.disconnected {
            report.aborted = true;
            break;
        }
        let result = match target {
            Selected::Plan(plan) => {
                run_one(
                    &mut game,
                    &mut session,
                    &db,
                    &plan,
                    &character,
                    &mut raised_level,
                    opts,
                )
                .await
            }
            Selected::Unknown(id) => unknown_craft_result(id, &character),
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

fn unknown_craft_result(id: u32, character: &RunCharacter) -> CraftResult {
    CraftResult {
        craft_id: id,
        title: String::new(),
        character: character.name.clone(),
        actability_group: 0,
        actability_limit: 0,
        result: CraftOutcome::Unsupported,
        reason: format!("craft {id} is not in the compact database"),
        materials_expected: Vec::new(),
        materials_consumed: Vec::new(),
        products_expected: Vec::new(),
        products_received: Vec::new(),
        products_equipped: Vec::new(),
        labor_expected: 0,
        labor_spent: 0,
        proficiency_gained: 0,
        cast_time_ms: None,
        cast_delay_ms: 0,
        skill_cast_time_ms: 0,
        server_result: None,
        server_errors: Vec::new(),
        messages: Vec::new(),
        elapsed_ms: 0,
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_one(
    game: &mut GameClient,
    session: &mut SessionState,
    db: &CraftDb,
    plan: &CraftPlan,
    character: &RunCharacter,
    raised_level: &mut u32,
    opts: &RunOptions,
) -> CraftResult {
    let start = Instant::now();
    let chat_start = game.state.chat_count;
    let material_expect: Vec<ItemAmount> = plan
        .materials
        .iter()
        .map(|m| ItemAmount {
            item_id: m.item_id,
            amount: m.amount,
        })
        .collect();
    let product_expect: Vec<ItemAmount> = plan
        .products
        .iter()
        .map(|p| ItemAmount {
            item_id: p.item_id,
            amount: p.amount,
        })
        .collect();
    let step = session.step(plan.skill.actability_group.max(0) as u32);
    let labor_expected = labor_cost(plan.skill.consume_lp, step);

    let mut result = CraftResult {
        craft_id: plan.id,
        title: plan.title.clone(),
        character: character.name.clone(),
        actability_group: plan.skill.actability_group,
        actability_limit: plan.actability_limit,
        result: CraftOutcome::Error,
        reason: String::new(),
        materials_expected: material_expect.clone(),
        materials_consumed: Vec::new(),
        products_expected: product_expect.clone(),
        products_received: Vec::new(),
        products_equipped: Vec::new(),
        labor_expected,
        labor_spent: 0,
        proficiency_gained: 0,
        cast_time_ms: None,
        cast_delay_ms: plan.cast_delay_ms,
        skill_cast_time_ms: plan.skill.casting_time,
        server_result: None,
        server_errors: Vec::new(),
        messages: Vec::new(),
        elapsed_ms: 0,
    };

    if plan.skill_id == 0 {
        result.result = CraftOutcome::Unsupported;
        result.reason = "the craft has no skill (crafts.skill_id 0)".into();
        finish(game, chat_start, start, &mut result);
        return result;
    }
    if matches!(plan.skill.target_type, 19 | 0) {
        // `systems/skills/skill.rs::initial_target`: `Building` resolves a house / shipyard
        // (which the sweep cannot place), `None` logs NotSupportedException; both drop the
        // cast silently. Crafts 276 / 2810..2813 (skill 11823) and 6277 (skill 25051).
        result.result = CraftOutcome::Unsupported;
        result.reason = format!(
            "skill {} targets type {} (building / none): needs a house or shipyard target the \
             sweep cannot place",
            plan.skill_id, plan.skill.target_type
        );
        finish(game, chat_start, start, &mut result);
        return result;
    }
    if !plan.skill.craft_effect {
        // `CharacterCraft.EndCraft` only runs from a `CraftEffect` (`economy/bridges.rs`
        // `craft_effect`); these skills complete through another effect (craft 2815/2816:
        // `InteractionEffect` + a doodad func), so `CSExecuteCraft` only burns the cast and
        // `Skill.EndSkill` charges the labor.
        result.result = CraftOutcome::Unsupported;
        result.reason = format!(
            "skill {} has no CraftEffect (its completion runs through a doodad interaction the \
             sweep does not drive)",
            plan.skill_id
        );
        finish(game, chat_start, start, &mut result);
        return result;
    }
    let group = plan.skill.actability_group.max(0) as u32;
    let missing_item = plan
        .materials
        .iter()
        .map(|m| m.item_id)
        .chain(plan.products.iter().map(|p| p.item_id))
        .find(|id| !db.item_exists(*id));
    if let Some(item_id) = missing_item {
        result.result = CraftOutcome::Unsupported;
        result.reason = format!(
            "item {item_id} of the recipe has no row in the 1.2 items table (the GM give / \
             EndCraft cannot create it)"
        );
        finish(game, chat_start, start, &mut result);
        return result;
    }
    // The skill's `Level` requirement (`unit_reqs` kind 1; skill 16766 wants 10).
    if plan.skill.level_min > 0 && u32::try_from(plan.skill.level_min).unwrap_or(1) > *raised_level
    {
        let _ = run_command(
            game,
            session,
            &format!("level {}", plan.skill.level_min),
            COMMAND_WAIT,
            |_, _| false,
        )
        .await;
        *raised_level = plan.skill.level_min as u32;
    }

    // The sweep destroys what it creates; a missed destroy (or a killed earlier run) must not
    // fill the 50-slot bag halfway through a long sweep.
    if let Err(e) = trim_bag(game).await {
        result.result = CraftOutcome::Error;
        result.reason = format!("connection error while trimming the bag: {e:#}");
        finish(game, chat_start, start, &mut result);
        return result;
    }

    // Actability limit first (the /labor top-ups also raise labor power).
    match ensure_actability(game, session, &db.expert_caps, group, plan.actability_limit).await {
        Ok(Ok(())) => {}
        Ok(Err(reason)) => {
            result.result = CraftOutcome::ActabilityBlocked;
            result.reason = reason;
            finish(game, chat_start, start, &mut result);
            return result;
        }
        Err(e) => {
            result.result = CraftOutcome::Error;
            result.reason = format!("connection error while setting actability: {e:#}");
            finish(game, chat_start, start, &mut result);
            return result;
        }
    }

    tracing::debug!(
        "craft {}: level/actability/trim took {} ms",
        plan.id,
        start.elapsed().as_millis()
    );

    // Materials.
    for material in &plan.materials {
        match ensure_material(game, session, material.item_id, material.amount).await {
            Ok(Ok(())) => {}
            Ok(Err(reason)) => {
                result.result = CraftOutcome::MaterialUnavailable;
                result.reason = reason;
                finish(game, chat_start, start, &mut result);
                return result;
            }
            Err(e) => {
                result.result = CraftOutcome::Error;
                result.reason = format!("connection error while giving materials: {e:#}");
                finish(game, chat_start, start, &mut result);
                return result;
            }
        }
    }

    // Equipment items the material gives auto-equipped (gliders / trade packs).
    let given_equipped = session.given_equipped();

    // Doodad.
    let mut doodad_obj = 0u32;
    if plan.req_doodad_id > 0 {
        match ensure_doodad(game, session, plan.req_doodad_id).await {
            Ok(Ok(obj_id)) => doodad_obj = obj_id,
            Ok(Err(reason)) => {
                result.result = CraftOutcome::NoDoodad;
                result.reason = reason;
                finish(game, chat_start, start, &mut result);
                return result;
            }
            Err(e) => {
                result.result = CraftOutcome::Error;
                result.reason = format!("connection error while spawning the doodad: {e:#}");
                finish(game, chat_start, start, &mut result);
                return result;
            }
        }
    }

    tracing::debug!(
        "craft {}: materials/doodad took {} ms (doodad {})",
        plan.id,
        start.elapsed().as_millis(),
        doodad_obj
    );
    // Labor for the skill charge.
    result.labor_expected = labor_cost(plan.skill.consume_lp, session.step(group));
    if let Err(e) = ensure_labor(game, session, result.labor_expected).await {
        result.result = CraftOutcome::Error;
        result.reason = format!("connection error while topping up labor: {e:#}");
        finish(game, chat_start, start, &mut result);
        return result;
    }

    let before = game.state.bag.clone();
    let mut notes = String::new();
    let mut attempt = match execute_craft(game, session, plan, doodad_obj, opts.step_timeout).await
    {
        Ok(a) => a,
        Err(e) => {
            result.result = CraftOutcome::Error;
            result.reason = format!("connection error during the craft: {e:#}");
            finish(game, chat_start, start, &mut result);
            return result;
        }
    };
    // A trade pack left by an earlier session occupies the backpack slot and rejects every
    // craft (`craft.rs::can_replace_glider_in_backpack_slot` -> 148/315). Destroy it once and
    // retry; the failed attempt consumed nothing.
    if !opts.keep_products
        && attempt
            .errors
            .first()
            .is_some_and(|(_, e2)| *e2 == econ_err::BACKPACK_OCCUPIED)
    {
        match clear_backpack(game, session).await {
            Ok(true) => {
                attempt = match execute_craft(game, session, plan, doodad_obj, opts.step_timeout)
                    .await
                {
                    Ok(a) => a,
                    Err(e) => {
                        result.result = CraftOutcome::Error;
                        result.reason = format!("connection error during the craft retry: {e:#}");
                        finish(game, chat_start, start, &mut result);
                        return result;
                    }
                };
                if result_is_clean(&attempt) {
                    notes.push_str(" (backpack slot cleared first)");
                }
            }
            Ok(false) => {}
            Err(e) => {
                result.result = CraftOutcome::Error;
                result.reason = format!("connection error while clearing the backpack: {e:#}");
                finish(game, chat_start, start, &mut result);
                return result;
            }
        }
    }
    // A fully silent attempt (not a single packet) is retried: nothing was consumed, and a
    // few crafts flake this way (a station's obstacle ray racing the previous removal, big
    // station meshes crossing the ray). The second retry also relocates the bot so the fresh
    // station has a different line of sight.
    let retry_timeout = opts.step_timeout.min(Duration::from_secs(35));
    if !attempt.responded {
        let _ = game
            .drain(Duration::from_millis(200), Duration::from_secs(1))
            .await;
        if let Ok(second) = execute_craft(game, session, plan, doodad_obj, retry_timeout).await {
            attempt = second;
            if result_is_clean(&attempt) {
                notes.push_str(" (crafted on retry)");
            }
        }
    }
    if !attempt.responded && doodad_obj != 0 {
        let (x, y, z) = game.state.self_pos;
        let _ = run_command(
            game,
            session,
            &format!("move {:.1} {:.1} {:.1}", x + 2.5, y + 1.5, z),
            COMMAND_WAIT,
            |_, _| false,
        )
        .await;
        if let Ok(Ok(id)) = ensure_doodad(game, session, plan.req_doodad_id).await {
            doodad_obj = id;
        }
        if let Ok(third) = execute_craft(game, session, plan, doodad_obj, retry_timeout).await {
            attempt = third;
            if result_is_clean(&attempt) {
                notes.push_str(" (crafted after relocating)");
            }
        }
    }
    tracing::debug!(
        "craft {}: execute took {} ms (cast {:?})",
        plan.id,
        start.elapsed().as_millis(),
        attempt.cast_time_ms
    );
    let after = game.state.bag.clone();
    let delta = bag_delta(&before, &after);
    result.materials_consumed = delta
        .iter()
        .filter(|d| plan.materials.iter().any(|m| m.item_id == d.item_id))
        .copied()
        .collect();
    result.products_received = delta
        .iter()
        .filter(|d| plan.products.iter().any(|p| p.item_id == d.item_id))
        .copied()
        .collect();
    let mut equipped: BTreeMap<u32, i32> = BTreeMap::new();
    for item in &attempt.equipped {
        *equipped.entry(item.template_id).or_default() += 1;
    }
    result.products_equipped = equipped
        .into_iter()
        .map(|(item_id, delta)| Delta { item_id, delta })
        .collect();
    let received: Vec<Delta> = result
        .products_received
        .iter()
        .chain(result.products_equipped.iter())
        .copied()
        .collect();
    result.cast_time_ms = attempt.cast_time_ms;
    result.labor_spent = attempt.labor_spent;
    result.proficiency_gained = attempt.proficiency_gained;
    result.server_result = attempt.server_result;
    result.server_errors = attempt
        .errors
        .iter()
        .map(|(e1, e2)| error_text(*e1, *e2))
        .collect();

    if let Some(result_code) = attempt.server_result {
        result.result = CraftOutcome::ServerRejected;
        result.reason = format!("cast rejected with SkillResult {result_code}");
    } else if let Some((e1, e2)) = attempt.errors.first() {
        result.result = CraftOutcome::ServerRejected;
        result.reason = error_text(*e1, *e2);
    } else if !attempt.responded {
        if plan.req_doodad_id > 0 && nearest_own_doodad(game, plan.req_doodad_id).is_none() {
            result.result = CraftOutcome::NoDoodad;
            result.reason = format!(
                "the required doodad {} was no longer in view at the timeout (the spawned \
                 doodad disappeared before the cast could resolve)",
                plan.req_doodad_id
            );
        } else {
            result.result = CraftOutcome::Timeout;
            result.reason = format!(
                "no craft response within {} s (silent cast rejection: GCD / cooldown / doodad \
                 target / requirement)",
                opts.step_timeout.as_secs()
            );
        }
    } else {
        let missing = missing_products(&product_expect, &received);
        if missing.is_empty() {
            result.result = CraftOutcome::Crafted;
            result.reason = format!(
                "{} product row(s), {} material row(s) consumed",
                plan.products.len(),
                plan.materials.len()
            );
        } else {
            result.result = CraftOutcome::ProductMismatch;
            result.reason = format!(
                "missing products: {}",
                missing
                    .iter()
                    .map(|m| format!("{} x{}", m.item_id, m.amount))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }

    // The station the sweep spawned for this recipe is not reused (stacking blocks later
    // casts); remove it again.
    if doodad_obj != 0 {
        let _ = remove_doodad(game, session, doodad_obj).await;
    }

    // Keep the bag clean for the next recipe: destroy what this recipe added (products and
    // any material the bot itself created that the craft did not consume). Materials that were
    // already in the bag (before count > 0) are left alone.
    // A material the GM give auto-equipped is counted by `Craft()` but not consumed by
    // `EndCraft` (it consumes from the bag only) -- worth naming in the record.
    for item in &given_equipped {
        if session
            .equipment
            .get(&item.slot)
            .is_some_and(|current| current.item_id == item.item_id)
        {
            notes.push_str(&format!(
                " (equipped material {} was not consumed: EndCraft consumes from the bag only)",
                item.template_id
            ));
            break;
        }
    }

    if !opts.keep_products {
        // Free the backpack slot first: an equipped trade pack rejects every later craft
        // (`craft.rs` `can_replace_glider_in_backpack_slot`).
        for items in [&attempt.equipped, &given_equipped] {
            if destroy_equipped(game, session, items).await.is_ok() {
                for item in items {
                    session.equipment.remove(&item.slot);
                }
            }
        }
        let mut destroy: HashSet<u32> = delta
            .iter()
            .filter(|d| d.delta > 0)
            .map(|d| d.item_id)
            .collect();
        for material in &plan.materials {
            if bag_template_count(&before, material.item_id) == 0 {
                destroy.insert(material.item_id);
            }
        }
        destroy_templates(game, &destroy).await.ok();
    }
    tracing::debug!(
        "craft {}: destroy took {} ms",
        plan.id,
        start.elapsed().as_millis()
    );
    result.reason.push_str(&notes);
    finish(game, chat_start, start, &mut result);
    result
}

fn result_is_clean(attempt: &CraftAttempt) -> bool {
    attempt.responded && attempt.errors.is_empty() && attempt.server_result.is_none()
}

/// Destroys the item in the equipment backpack slot (`EquipmentItemSlot.Backpack` = 26).
/// Returns whether something was cleared.
async fn clear_backpack(game: &mut GameClient, session: &mut SessionState) -> anyhow::Result<bool> {
    const SLOT_TYPE_EQUIPMENT: u8 = 1;
    const EQUIP_SLOT_BACKPACK: u8 = 26;
    let Some(item) = session.equipment.get(&EQUIP_SLOT_BACKPACK).copied() else {
        tracing::debug!(
            "clear_backpack: no item tracked in slot 26 (tracked slots: {:?})",
            session.equipment.keys().collect::<Vec<_>>()
        );
        return Ok(false);
    };
    tracing::info!(
        "clear_backpack: destroying item {} (template {}) in the backpack slot",
        item.item_id,
        item.template_id
    );
    let mut body = PacketStream::new();
    body.write_u64(item.item_id)
        .write_u8(SLOT_TYPE_EQUIPMENT)
        .write_u8(EQUIP_SLOT_BACKPACK)
        .write_i32(1);
    game.send_cs(cs::CSDestroyItemPacket, body.as_bytes())
        .await?;
    let deadline = Instant::now() + COMMAND_WAIT;
    let _ = wait_until(game, session, deadline, |_, s| {
        s.equipment
            .get(&EQUIP_SLOT_BACKPACK)
            .is_none_or(|current| current.item_id != item.item_id)
    })
    .await;
    session.equipment.remove(&EQUIP_SLOT_BACKPACK);
    Ok(true)
}

/// Destroys the equipment items a recipe created (`CSDestroyItemPacket` with the equipment
/// slot type; trade packs the server auto-equipped) and waits for the removal tasks.
async fn destroy_equipped(
    game: &mut GameClient,
    session: &mut SessionState,
    items: &[EquippedItem],
) -> anyhow::Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    const SLOT_TYPE_EQUIPMENT: u8 = 1;
    for item in items {
        let mut body = PacketStream::new();
        body.write_u64(item.item_id)
            .write_u8(SLOT_TYPE_EQUIPMENT)
            .write_u8(item.slot)
            .write_i32(1);
        game.send_cs(cs::CSDestroyItemPacket, body.as_bytes())
            .await?;
    }
    let deadline = Instant::now() + COMMAND_WAIT;
    let _ = wait_until(game, session, deadline, |_, s| {
        items.iter().all(|item| {
            s.equipment
                .get(&item.slot)
                .is_none_or(|current| current.item_id != item.item_id)
        })
    })
    .await;
    Ok(())
}

fn bag_template_count(bag: &Bag, template_id: u32) -> i32 {
    bag.values()
        .filter(|i| i.template_id == template_id)
        .map(|i| i.count)
        .sum()
}

/// Destroys every bag stack when the bag grew past [`BAG_TRIM_THRESHOLD`] (the sweep owns the
/// character's bag; this only catches destroys that failed or a killed earlier run).
async fn trim_bag(game: &mut GameClient) -> anyhow::Result<()> {
    if game.state.bag.len() <= BAG_TRIM_THRESHOLD {
        return Ok(());
    }
    let templates: HashSet<u32> = game.state.bag.values().map(|i| i.template_id).collect();
    tracing::info!(
        "craft sweep: trimming {} bag stacks ({} templates)",
        game.state.bag.len(),
        templates.len()
    );
    destroy_templates(game, &templates).await
}

/// Destroys every bag stack of the given templates (`CSDestroyItemPacket`).
async fn destroy_templates(game: &mut GameClient, templates: &HashSet<u32>) -> anyhow::Result<()> {
    if templates.is_empty() {
        return Ok(());
    }
    let stacks: Vec<(u64, u8, i32)> = game
        .state
        .bag
        .values()
        .filter(|i| templates.contains(&i.template_id))
        .map(|i| (i.item_id, i.slot, i.count))
        .collect();
    for (item_id, slot, count) in stacks {
        let mut body = PacketStream::new();
        body.write_u64(item_id)
            .write_u8(SLOT_TYPE_INVENTORY)
            .write_u8(slot)
            .write_i32(count);
        game.send_cs(cs::CSDestroyItemPacket, body.as_bytes())
            .await?;
    }
    let deadline = Instant::now() + GM_EFFECT_WAIT;
    let mut pump_state = SessionState::default();
    let _ = wait_until(game, &mut pump_state, deadline, |st, _| {
        templates.iter().all(|id| st.bag_count(&[*id]) == 0)
    })
    .await;
    Ok(())
}

fn finish(game: &GameClient, chat_start: u64, start: Instant, result: &mut CraftResult) {
    result.messages = window_messages(game, chat_start);
    result.elapsed_ms = start.elapsed().as_millis() as u64;
}

// -------------------------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use aaemu_commons::Marshal;
    use aaemu_game::models::character::Character;
    use aaemu_game::models::unit_model::UnitCustomModelParams;
    use aaemu_game::packets::world::{SCActabilityPacket, SCErrorMsgPacket};
    use aaemu_game::systems::skills::packets::{
        SCSkillEndedPacket, SCSkillFiredPacket, SCSkillStartedPacket,
    };
    use aaemu_game::systems::skills::progress::SCCharacterLaborPowerChangedPacket;
    use aaemu_game::systems::skills::types::SkillObject;

    use crate::game::LEVEL_GAME;
    use crate::inventory::ItemInfo;

    fn item(template_id: u32, item_id: u64, slot: u8, count: i32) -> ItemInfo {
        ItemInfo {
            item_id,
            template_id,
            count,
            slot,
        }
    }

    fn packet(opcode: u16, body: Vec<u8>) -> Packet {
        Packet {
            level: LEVEL_GAME,
            opcode,
            body,
        }
    }

    fn encode<T: Marshal>(v: &T) -> Vec<u8> {
        let mut s = PacketStream::new();
        v.write(&mut s);
        s.into_bytes()
    }

    /// A minimal `compact.sqlite3` shape for the craft loader (only the read tables).
    fn write_test_db(path: &Path) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE crafts (id INTEGER PRIMARY KEY, title TEXT, cast_delay INTEGER, \
             req_doodad_id INTEGER, actability_limit INTEGER, skill_id INTEGER);
             CREATE TABLE skills (id INTEGER PRIMARY KEY, consume_lp INTEGER, \
             actability_group_id INTEGER, casting_time INTEGER, cooldown_time INTEGER, \
             target_type_id INTEGER);
             CREATE TABLE craft_materials (craft_id INTEGER, item_id INTEGER, amount INTEGER);
             CREATE TABLE craft_products (craft_id INTEGER, item_id INTEGER, amount INTEGER, \
             rate INTEGER);
             CREATE TABLE craft_pack_crafts (craft_id INTEGER);
             CREATE TABLE items (id INTEGER PRIMARY KEY);
             CREATE TABLE doodad_almighties (id INTEGER PRIMARY KEY);
             CREATE TABLE actability_groups (id INTEGER PRIMARY KEY);
             CREATE TABLE expert_limits (up_limit INTEGER);
             CREATE TABLE unit_reqs (owner_type TEXT, owner_id INTEGER, kind_id INTEGER, \
             value1 INTEGER);
             CREATE TABLE skill_effects (id INTEGER PRIMARY KEY, skill_id INTEGER, \
             effect_id INTEGER);
             CREATE TABLE effects (id INTEGER PRIMARY KEY, actual_type TEXT, actual_id INTEGER);",
        )
        .unwrap();
        for id in 1..=100u32 {
            conn.execute("INSERT INTO items VALUES (?1)", [id]).unwrap();
        }
        conn.execute("INSERT INTO doodad_almighties VALUES (568)", [])
            .unwrap();
        conn.execute("INSERT INTO actability_groups VALUES (4)", [])
            .unwrap();
        conn.execute("INSERT INTO actability_groups VALUES (11)", [])
            .unwrap();
        conn.execute("INSERT INTO expert_limits VALUES (10000)", [])
            .unwrap();
        fn add_craft(
            conn: &rusqlite::Connection,
            id: u32,
            group: Option<i32>,
            mats: &[(u32, i32)],
            prods: &[(u32, i32)],
            doodad: u32,
            pack: bool,
        ) {
            conn.execute(
                "INSERT INTO crafts VALUES (?1, ?2, 5000, ?3, 0, ?1)",
                rusqlite::params![id, format!("craft {id}"), doodad],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO skills VALUES (?1, 10, ?2, 5000, 0, 8)",
                rusqlite::params![id, group],
            )
            .unwrap();
            // Every test skill completes through a CraftEffect.
            conn.execute("INSERT INTO effects VALUES (?1, 'CraftEffect', 0)", [id])
                .unwrap();
            conn.execute("INSERT INTO skill_effects VALUES (?1, ?1, ?1)", [id])
                .unwrap();
            for (item, amount) in mats {
                conn.execute(
                    "INSERT INTO craft_materials VALUES (?1, ?2, ?3)",
                    rusqlite::params![id, item, amount],
                )
                .unwrap();
            }
            for (item, amount) in prods {
                conn.execute(
                    "INSERT INTO craft_products VALUES (?1, ?2, ?3, 100)",
                    rusqlite::params![id, item, amount],
                )
                .unwrap();
            }
            if pack {
                conn.execute("INSERT INTO craft_pack_crafts VALUES (?1)", [id])
                    .unwrap();
            }
        }
        let add_craft = |id, group, mats: &[(u32, i32)], prods: &[(u32, i32)], doodad, pack| {
            add_craft(&conn, id, group, mats, prods, doodad, pack);
        };
        add_craft(1, Some(4), &[(1, 1)], &[(2, 1)], 0, false);
        add_craft(2, Some(4), &[(1, 1)], &[(2, 1)], 0, false);
        add_craft(3, Some(4), &[(1, 1)], &[(2, 1)], 0, false);
        add_craft(10, Some(11), &[(1, 1)], &[(2, 1)], 0, false);
        add_craft(11, Some(11), &[(1, 1)], &[(2, 1)], 0, false);
        add_craft(20, None, &[(1, 1)], &[(2, 1)], 0, false);
        // A trade pack: usable (the product auto-equips and the sweep clears it).
        add_craft(31, Some(4), &[(1, 1)], &[(2, 1)], 0, true);
        // Data gaps: a missing item, a missing doodad template, a missing actability group.
        add_craft(30, Some(4), &[(404, 1)], &[(2, 1)], 0, false);
        add_craft(33, Some(4), &[(1, 1)], &[(2, 1)], 999, false);
        add_craft(34, Some(99), &[(1, 1)], &[(2, 1)], 0, false);
    }

    fn test_db(name: &str) -> CraftDb {
        let path =
            std::env::temp_dir().join(format!("craftrun-{name}-{}.sqlite3", std::process::id()));
        let _ = std::fs::remove_file(&path);
        write_test_db(&path);
        let db = CraftDb::open(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        db
    }

    #[test]
    fn sample_spread_round_robins_over_groups_in_id_order() {
        let db = test_db("sample");
        let sample = sample_spread(&db, 4);
        let order: Vec<u32> = sample.iter().map(|p| p.id).collect();
        // Groups 0, 4, 11 in key order; the two smallest ids of group 4, then group 11.
        assert_eq!(order, vec![20, 1, 10, 2]);
        // More than the pool: everything usable, once (the trade pack 31 included).
        let all = sample_spread(&db, 100);
        let mut ids_all: Vec<u32> = all.iter().map(|p| p.id).collect();
        ids_all.sort_unstable();
        assert_eq!(ids_all, vec![1, 2, 3, 10, 11, 20, 31]);
        assert!(sample_spread(&db, 0).is_empty());
    }

    #[test]
    fn filter_chain_counts_each_stage_and_the_all_selection_is_the_usable_set() {
        let db = test_db("chain");
        let chain = db.filter_chain();
        assert_eq!(
            chain,
            vec![
                ("all crafts", 10),
                ("crafts.skill_id != 0", 10),
                ("every craft_materials.item_id in items", 9),
                ("every craft_products.item_id in items", 9),
                ("crafts.req_doodad_id in doodad_almighties (0 = pass)", 8),
                (
                    "skills.actability_group_id in actability_groups (0 = pass)",
                    7
                ),
            ]
        );
        let usable: Vec<u32> = db.usable().iter().map(|p| p.id).collect();
        assert_eq!(usable, vec![1, 2, 3, 10, 11, 20, 31]);
    }

    #[test]
    fn resume_parser_reads_recorded_craft_ids() {
        let text = "{\"craft_id\":5,\"result\":\"crafted\"}\n\
                    \n\
                    not json\n\
                    {\"craft_id\":9}\n\
                    {\"craft_id\":\"x\"}\n";
        let done = parse_done_ids(text);
        assert!(done.contains(&5) && done.contains(&9));
        assert_eq!(done.len(), 2);
        assert!(parse_done_ids("").is_empty());
    }

    #[test]
    fn bag_delta_counts_templates_and_stacking() {
        let mut before = Bag::new();
        before.insert(0, item(1000, 0x11, 0, 3));
        before.insert(1, item(2000, 0x22, 1, 1));
        let mut after = Bag::new();
        // Template 1000 consumed entirely, 2000 gained +4, 3000 created.
        after.insert(1, item(2000, 0x22, 1, 5));
        after.insert(2, item(3000, 0x33, 2, 2));
        let delta = bag_delta(&before, &after);
        assert_eq!(
            delta,
            vec![
                Delta {
                    item_id: 1000,
                    delta: -3
                },
                Delta {
                    item_id: 2000,
                    delta: 4
                },
                Delta {
                    item_id: 3000,
                    delta: 2
                },
            ]
        );
        assert_eq!(bag_delta(&before, &before), Vec::new());
    }

    #[test]
    fn missing_products_reports_the_shortfall() {
        let expected = vec![
            ItemAmount {
                item_id: 100,
                amount: 2,
            },
            ItemAmount {
                item_id: 200,
                amount: 1,
            },
        ];
        let received = vec![
            Delta {
                item_id: 100,
                delta: 5,
            },
            Delta {
                item_id: 200,
                delta: 0,
            },
        ];
        assert_eq!(missing_products(&expected, &received).len(), 1);
        let received = vec![
            Delta {
                item_id: 100,
                delta: 1,
            },
            Delta {
                item_id: 200,
                delta: 3,
            },
        ];
        assert_eq!(
            missing_products(&expected, &received),
            vec![ItemAmount {
                item_id: 100,
                amount: 1
            }]
        );
    }

    #[test]
    fn labor_cost_scales_with_the_expert_step() {
        assert_eq!(labor_cost(0, 0), 0);
        assert_eq!(labor_cost(10, 0), 10);
        // 0.95f32 as f64 is 0.94999..., so 10 x 0.95 truncates to 9 (the server's formula).
        assert_eq!(labor_cost(10, 2), 9);
        assert_eq!(labor_cost(100, 6), 77);
        assert_eq!(labor_cost(1, 6), 1);
        assert_eq!(labor_cost(-5, 0), 0);
    }

    #[test]
    fn outcome_names_are_the_summary_keys() {
        assert_eq!(CraftOutcome::Crafted.name(), "crafted");
        assert_eq!(CraftOutcome::ServerRejected.name(), "server_rejected");
        assert_eq!(reason_key("craft 123 failed"), "craft # failed");
    }

    #[test]
    fn labor_packet_parses_amount_and_proficiency() {
        let body = encode(&SCCharacterLaborPowerChangedPacket {
            amount: -77,
            action: 11,
            point: 77,
            step: 2,
        });
        let parsed = parse_labor(&packet(sc::SCCharacterLaborPowerChangedPacket, body)).unwrap();
        assert_eq!(
            parsed,
            LaborChange {
                amount: -77,
                action: 11,
                point: 77,
                step: 2
            }
        );
    }

    #[test]
    fn session_tracks_labor_and_actability() {
        let mut session = SessionState {
            labor: Some(50),
            ..SessionState::default()
        };
        session.observe(&packet(
            sc::SCCharacterLaborPowerChangedPacket,
            encode(&SCCharacterLaborPowerChangedPacket {
                amount: -30,
                action: 11,
                point: 30,
                step: 0,
            }),
        ));
        assert_eq!(session.labor, Some(20));
        assert_eq!(session.point(11), 30);
        // A group-less GM top-up changes labor only.
        session.observe(&packet(
            sc::SCCharacterLaborPowerChangedPacket,
            encode(&SCCharacterLaborPowerChangedPacket {
                amount: 500,
                action: 0,
                point: 0,
                step: 0,
            }),
        ));
        assert_eq!(session.labor, Some(520));
        assert_eq!(session.point(11), 30);
        // The upgrade packet moves the step.
        let body = encode(
            &aaemu_game::systems::services::expands::SCExpertLimitModifiedPacket {
                is_upgrade: true,
                id: 11,
                step: 1,
            },
        );
        session.observe(&packet(sc::SCExpertLimitModifiedPacket, body));
        assert_eq!(session.step(11), 1);
    }

    #[test]
    fn actability_snapshot_parses() {
        let body = encode(&SCActabilityPacket {
            last: true,
            actabilities: vec![
                aaemu_game::models::char_data::Actability {
                    id: 1,
                    point: 500,
                    step: 0,
                },
                aaemu_game::models::char_data::Actability {
                    id: 11,
                    point: 12345,
                    step: 2,
                },
            ],
        });
        let rows = parse_actability(&packet(sc::SCActabilityPacket, body)).unwrap();
        assert_eq!(rows, vec![(1, 500, 0), (11, 12345, 2)]);
    }

    #[test]
    fn error_msg_parses_the_error_code() {
        let body = encode(&SCErrorMsgPacket {
            error1: 722,
            error2: 0,
            kind: 3,
            is_notify: false,
        });
        assert_eq!(
            parse_error(&packet(sc::SCErrorMsgPacket, body)),
            Some((722, 0))
        );
        assert_eq!(error_text(722, 0), "SCErrorMsg 722 (NotEnoughRequiredItem)");
        assert_eq!(error_text(9, 9), "SCErrorMsg 9");
        // The crafting failures use the (primary, check) pair.
        assert_eq!(
            error_text(148, 315),
            "SCErrorMsg 148/315 (CraftCantActAnyMore/BackpackOccupied)"
        );
    }

    #[test]
    fn equipment_tasks_surface_the_auto_equipped_product() {
        use aaemu_game::models::item::{Item, ItemClass};
        use aaemu_game::models::item_task::ItemTask;
        use aaemu_game::systems::economy::item_packets::SCItemTaskSuccessPacket;

        let mut it = Item::new(1);
        it.class = ItemClass::Item;
        it.template_id = 24920;
        it.id = 0x1234;
        it.count = 1;
        it.slot_type = 1; // Equipment
        it.slot = 26; // EquipmentItemSlot.Backpack
        let mut body = PacketStream::new();
        SCItemTaskSuccessPacket {
            action: 9,
            tasks: vec![ItemTask::add(&it)],
            force_remove: vec![],
        }
        .write(&mut body);
        assert_eq!(
            equipment_tasks(body.as_bytes()).unwrap(),
            EquipmentChanges {
                created: vec![EquippedItem {
                    item_id: 0x1234,
                    template_id: 24920,
                    slot: 26,
                }],
                removed_slots: Vec::new(),
            }
        );

        // A bag row is not an equipment row.
        it.slot_type = SLOT_TYPE_INVENTORY;
        let mut body = PacketStream::new();
        SCItemTaskSuccessPacket {
            action: 9,
            tasks: vec![ItemTask::add(&it)],
            force_remove: vec![],
        }
        .write(&mut body);
        let bag_row = equipment_tasks(body.as_bytes()).unwrap();
        assert!(bag_row.created.is_empty() && bag_row.removed_slots.is_empty());

        // A destroy of the equipped pack surfaces as a removal (the sweep waits for it).
        it.slot_type = 1;
        let mut body = PacketStream::new();
        SCItemTaskSuccessPacket {
            action: 9,
            tasks: vec![ItemTask::remove(&it)],
            force_remove: vec![],
        }
        .write(&mut body);
        assert_eq!(
            equipment_tasks(body.as_bytes()).unwrap().removed_slots,
            vec![26]
        );
    }

    #[test]
    fn skill_packets_identify_the_caster_and_result() {
        let started = SCSkillStartedPacket {
            id: 110_078,
            tl: 7,
            caster: SkillCaster::Unit(0x500),
            target: SkillCastTarget::Doodad(0x600),
            skill_object: SkillObject::default(),
            real_cast_time_div10: 0,
            base_cast_time_div10: 500,
            cast_synergy: 0,
            extra_flags: 1,
            extra_byte: 15,
            extra_ushort: 0,
            extra_uint: 0,
        };
        let parsed =
            parse_skill_started(&packet(sc::SCSkillStartedPacket, encode(&started))).unwrap();
        assert_eq!(parsed.skill_id, 110_078);
        assert_eq!(parsed.tl, 7);
        assert_eq!(parsed.caster_obj, 0x500);
        assert_eq!(parsed.result, 15);

        let fired = SCSkillFiredPacket {
            id: 110_078,
            tl: 7,
            caster: SkillCaster::Unit(0x500),
            target: SkillCastTarget::Doodad(0x600),
            skill_object: SkillObject::default(),
            computed_delay: 0,
            channeling_time: 0,
            fire_anim_id: 0,
        };
        assert_eq!(
            parse_skill_fired(&packet(sc::SCSkillFiredPacket, encode(&fired))),
            Some((110_078, 7, 0x500))
        );
        assert_eq!(
            parse_skill_ended(&packet(
                sc::SCSkillEndedPacket,
                encode(&SCSkillEndedPacket { tl: 7 })
            )),
            Some(7)
        );
    }

    #[test]
    fn state_packet_decodes_labor_after_the_full_character_body() {
        let mut c = Character::new(42, 7, UnitCustomModelParams::default());
        c.unit.name = "Bot".into();
        c.labor_power = 1234;
        let mut body = PacketStream::new();
        body.write_u32(3) // instance
            .write_u16(0) // guid length
            .write_i32(0); // rwd
        c.write(&mut body);
        assert_eq!(parse_state_labor(body.as_bytes()).unwrap(), 1234);

        // A truncated body is an error, never a wrong number.
        assert!(parse_state_labor(&[0u8; 8]).is_err());
    }

    #[test]
    fn state_equipment_handles_body_part_rows_and_finds_the_backpack() {
        use aaemu_game::models::item::{Item, ItemClass};
        use aaemu_game::models::unit_model::UnitCustomModelParams;

        // Face (19) is a BodyPart row: only the template id; the Backpack (26) is a full item;
        // every other slot is empty (`u32 0`).
        let mut face = Item::new(1);
        face.class = ItemClass::BodyPart;
        face.template_id = 19838;
        face.slot_type = 1;
        face.slot = 19;
        let mut pack = Item::new(1);
        pack.class = ItemClass::Item;
        pack.template_id = 24920;
        pack.id = 0x7766;
        pack.count = 1;
        pack.slot_type = 1;
        pack.slot = 26;

        let mut s = PacketStream::new();
        s.write_u32(3).write_u16(0).write_i32(0); // instance, guid, rwd
        s.write_u32(42).write_str("Bot");
        s.write_u8(1).write_u8(1).write_u8(10); // race, gender, level
        s.write_i32(100).write_i32(50); // hp, mp
        s.write_u32(1).write_u32(101); // zone, faction
        s.write_str("Nuian").write_u32(0).write_u32(0); // faction name, expedition, family
        for slot in 0..28 {
            match slot {
                19 => face.write(&mut s),
                26 => pack.write(&mut s),
                _ => {
                    s.write_u32(0);
                }
            }
        }
        s.write_raw(&[0; 3]); // abilities
        s.write_i64(0).write_i64(0).write_f32(0.0); // x, y, z
        UnitCustomModelParams::default().write(&mut s);
        s.write_i16(77); // labor

        let (labor, equipment, abilities) = parse_state_equipment(s.as_bytes()).unwrap();
        assert_eq!(labor, 77);
        assert_eq!(abilities, [0, 0, 0]);
        assert_eq!(
            equipment,
            vec![
                EquippedItem {
                    item_id: 0,
                    template_id: 19838,
                    slot: 19
                },
                EquippedItem {
                    item_id: 0x7766,
                    template_id: 24920,
                    slot: 26
                },
            ]
        );
    }

    #[test]
    fn craft_db_loads_the_running_game_data_when_present() {
        let path = Path::new(DEFAULT_COMPACT_DB);
        if !path.exists() {
            return;
        }
        let db = CraftDb::open(path).unwrap();
        assert!(db.all_crafts().len() > 6_000);
        assert!(!db.expert_caps.is_empty());
        // Craft 153 is a 1.2 recipe with a required doodad (`req_doodad_id` 568).
        let c = db.craft(153).unwrap();
        assert_eq!(c.req_doodad_id, 568);
        assert!(!c.materials.is_empty() && !c.products.is_empty());
        // The Commerce craft skill 16766 demands level 10 (`unit_reqs` kind 1).
        assert_eq!(db.craft(2).unwrap().skill.level_min, 10);
        assert_eq!(db.craft(46).unwrap().skill.level_min, 0);
        // Craft 2816's skill 12299 completes through an InteractionEffect, not a CraftEffect.
        assert!(db.craft(46).unwrap().skill.craft_effect);
        assert!(!db.craft(2816).unwrap().skill.craft_effect);
        assert_eq!(db.craft(276).unwrap().skill.target_type, 19);
        // The gate chain matches the data: thousands of usable recipes, no stage grows.
        let chain = db.filter_chain();
        assert_eq!(chain.first().unwrap().1, db.all_crafts().len());
        assert!(chain.last().unwrap().1 > 2_500, "chain: {chain:?}");
        for pair in chain.windows(2) {
            assert!(pair[1].1 <= pair[0].1, "chain not monotonic: {chain:?}");
        }
        assert_eq!(
            db.usable().len(),
            db.all_crafts().iter().filter(|p| db.is_usable(p)).count()
        );
        // The sample spread is deterministic and reaches many actability groups (the fixed
        // gate admits the groups the old <=4-materials / no-pack filters hid).
        let sample = sample_spread(&db, 40);
        assert_eq!(sample.len(), 40);
        let mut groups: Vec<i32> = sample.iter().map(|p| p.skill.actability_group).collect();
        groups.sort_unstable();
        groups.dedup();
        assert!(groups.len() >= 12, "groups: {groups:?}");
    }
}
