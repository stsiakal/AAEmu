//! Value checks for a completed quest (player-mode design §4, plan task E).
//!
//! Given the quest that completed and the packets observed during it, the checks compare the
//! rewards and kills against the same `compact.sqlite3` the game server loaded:
//!
//! * XP: the `QuestActSupplyExp` acts of the quest plus the level supply (`quest_supplies`,
//!   added by `engine.rs::distribute_rewards` at the Reward step) against the quest window's
//!   `SCExpChangedPacket` deltas. The server scales positive gains by `World:ExpRate`
//!   (`skills/progress.rs::add_exp_locked`); the lane config ships `1.0`.
//! * gold: `QuestActSupplyCopper` acts plus the level supply's copper against the money tasks
//!   (`ItemTaskType.DepositMoney` = 83) of the window, minus the coins taken from loot bags.
//! * items: `QuestActSupplyItem` / `QuestActSupplySelectiveItem` acts against the
//!   `ItemTaskType.QuestSupplyItems` (= 38) create tasks. An act that ran before the Reward step
//!   only tops the character up to its count (`acts.rs::SupplyItem`), so items already owned at
//!   the window start explain a smaller grant. The server mails the reward pool when the bag has
//!   fewer free slots than the pool (`engine.rs::distribute_rewards`).
//! * kills: the level the server announced (`SCUnitStatePacket`) against `npcs.LEVEL`, the
//!   `SCExpChangedPacket` the server sends just before the death against the kill-XP formula
//!   (`npcs/npc/death.rs::kill_exp_for`, `pl_mod` 1 for a solo player), and every looted item
//!   (`SCLootItemTookPacket`) against the loot packs of the corpse's NPC
//!   (`loot_pack_dropping_npcs` -> `loots`).
//!
//! The kill-XP expectation is a **thin oracle**: it runs the server's own `npc_stats` code
//! (`npcs/npc/stats.rs`) to get `KillExp`, so a formula change moves both sides together. A
//! mismatch is a `suspect` (observed vs expected with the rows cited), never a confirmed bug.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use anyhow::Context;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};

use aaemu_game::data::formulas::FormulaManager;
use aaemu_game::models::unit::Bonuses;
use aaemu_game::systems::npcs::npc::data::NpcTemplate;
use aaemu_game::systems::npcs::npc::death::kill_exp_for;
use aaemu_game::systems::npcs::npc::stats::NpcStatCalc;

use crate::game::{ExpChange, GameEvent, GameState, KillInfo, LootTook};

/// `ItemTaskType.DepositMoney` (`models/item_task.rs`): quest / loot copper.
const TASK_DEPOSIT_MONEY: u8 = 83;
/// `ItemTaskType.QuestSupplyItems` (`models/item_task.rs`): quest reward / supply items.
const TASK_QUEST_SUPPLY_ITEMS: u8 = 38;
/// `data::items::ITEM_COINS`: a loot entry that is paid out as money.
const ITEM_COINS: u32 = 500;
/// The kill-XP `SCExpChangedPacket` and the `SCUnitDeathPacket` are sent by the same server
/// call (`npc/death.rs::before_death` then `base.DoDie`); a gain from an earlier step is older.
const KILL_XP_WINDOW_MS: u64 = 1_000;
/// `QuestComponentKind.Reward` (`systems/quests/model.rs`).
const KIND_REWARD: u8 = 8;
/// `QuestComponentKind.Fail` / `Drop`: those components never run on a completed quest.
const KIND_FAIL: u8 = 5;
const KIND_DROP: u8 = 7;

/// One check result per completed quest, appended to the [`crate::questrun::QuestResult`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ValueReport {
    /// Value mismatches: observed vs expected plus the rows cited. Not confirmed bugs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suspects: Vec<String>,
    /// Explained observations (pre-owned items, ratio not modelled, thin oracle, ...).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xp: Option<XpCheck>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gold: Option<GoldCheck>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<ItemCheck>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kills: Vec<KillCheck>,
}

impl ValueReport {
    pub fn note(message: impl Into<String>) -> Self {
        Self {
            notes: vec![message.into()],
            ..Self::default()
        }
    }
}

/// Quest experience: the acts + level supply against the window's own gains.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct XpCheck {
    pub expected: i64,
    /// `SCExpChangedPacket` gains of the window that were not paired with a kill.
    pub observed: i64,
    /// Gains paired with a kill (`death.rs::kill_exp_for` / the untagged raw branch).
    pub observed_kill: i64,
    /// `expected - observed` (positive = the window showed less than the data says).
    pub delta: i64,
}

/// Quest copper: the acts + level supply against the window's money tasks.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GoldCheck {
    pub expected: i64,
    pub observed: i64,
    pub delta: i64,
}

/// One expected quest item against what the supply item tasks created.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ItemCheck {
    pub template_id: u32,
    /// `supply` (`quest_act_supply_items`) or `selective` (`quest_act_supply_selective_items`).
    pub source: String,
    pub expected: i32,
    pub observed: i32,
    /// Copies already in the bag when the quest window started (an act only tops up to its
    /// count).
    pub owned_before: i32,
}

/// One kill of the quest window.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct KillCheck {
    pub obj_id: u32,
    pub npc_id: u32,
    /// Level the server announced with `SCUnitStatePacket`.
    pub level_observed: u8,
    /// `npcs.LEVEL` of the same template.
    pub level_data: u8,
    /// The player's own level when the kill happened.
    pub player_level: u8,
    /// `npc_stats` KillExp (the server formula, thin oracle).
    pub kill_exp: i64,
    /// `death.rs::kill_exp_for(kill_exp, 1.0, player level, npc level)`; `None` on a 10+ level gap.
    pub kill_exp_scaled: Option<i64>,
    /// The `SCExpChangedPacket` delta the server sent just before this death, when one did.
    pub exp_observed: Option<i64>,
    /// Items the client took from this corpse (`SCLootItemTookPacket`).
    pub looted: Vec<u32>,
}

/// The observed window of one quest, sliced from the bot's [`GameState`].
pub struct QuestWindow<'a> {
    /// The bot's own object id (only its `SCExpChangedPacket` gains count).
    pub self_obj: u32,
    /// Reward index the client sent with `CSCompleteQuestContextPacket` (0 in the runner).
    pub selected_reward: i32,
    /// Free bag slots when the window started (`distribute_rewards` mails a full bag's pool).
    pub bag_free_slots: u32,
    /// Template id -> count in the bag when the window started.
    pub bag_before: &'a BTreeMap<u32, i32>,
    pub exp_changes: &'a [ExpChange],
    /// `SCErrorMsgPacket` / `SCItemTaskSuccessPacket` events of the window, in arrival order.
    pub events: &'a [GameEvent],
    pub kills: &'a [KillInfo],
    pub loot: &'a [LootTook],
}

/// The cursor of [`GameState`] when a quest window starts.
#[derive(Debug, Clone)]
pub struct ValueSnapshot {
    exp: usize,
    events: usize,
    kills: usize,
    loot: usize,
    bag: BTreeMap<u32, i32>,
    free_slots: u32,
    self_obj: u32,
}

impl ValueSnapshot {
    pub fn capture(state: &GameState) -> Self {
        let mut bag: BTreeMap<u32, i32> = BTreeMap::new();
        for item in state.bag.values() {
            *bag.entry(item.template_id).or_default() += item.count;
        }
        Self {
            exp: state.exp_changes.len(),
            events: state.events.len(),
            kills: state.kill_log.len(),
            loot: state.loot_log.len(),
            bag,
            free_slots: u32::from(state.bag_slots).saturating_sub(state.bag.len() as u32),
            self_obj: state.self_obj,
        }
    }

    pub fn exp(&self) -> usize {
        self.exp
    }

    pub fn events(&self) -> usize {
        self.events
    }

    pub fn kills(&self) -> usize {
        self.kills
    }

    pub fn loot(&self) -> usize {
        self.loot
    }

    pub fn bag(&self) -> &BTreeMap<u32, i32> {
        &self.bag
    }

    pub fn free_slots(&self) -> u32 {
        self.free_slots
    }

    pub fn self_obj(&self) -> u32 {
        self.self_obj
    }
}

/// One `quest_acts` row of a quest, in the loader's component / act order.
#[derive(Debug, Clone)]
struct ActRow {
    kind: String,
    detail_id: i64,
    component_kind: u8,
}

/// A quest reward act that creates `template_id`.
#[derive(Debug, Clone, Default)]
struct ItemExpectation {
    expected: i32,
    sources: Vec<String>,
    source: String,
}

/// The read-only value checker (`compact.sqlite3` + the server's unit formulas).
pub struct ValueChecker {
    conn: Connection,
    formulas: FormulaManager,
}

impl ValueChecker {
    /// Opens `path` read-only and loads the unit formulas for the kill-XP oracle.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("opening compact database {}", path.display()))?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> anyhow::Result<Self> {
        let formulas =
            FormulaManager::load(&conn).context("loading unit formulas for the kill-XP oracle")?;
        Ok(Self { conn, formulas })
    }

    /// Checks one completed quest against the packets observed in its window.
    pub fn check(&self, quest_id: u32, w: &QuestWindow<'_>) -> anyhow::Result<ValueReport> {
        let mut report = ValueReport::default();
        let Some((level, let_it_done)) = self.quest_row(quest_id)? else {
            report
                .notes
                .push(format!("quest {quest_id} has no quest_contexts row"));
            return Ok(report);
        };
        let acts = self.quest_acts(quest_id)?;

        let mut expected_exp: i64 = 0;
        let mut exp_sources: Vec<String> = Vec::new();
        let mut expected_copper: i64 = 0;
        let mut copper_sources: Vec<String> = Vec::new();
        let mut item_expected: BTreeMap<u32, ItemExpectation> = BTreeMap::new();
        let mut selective: Vec<(i64, u8, u32, i32)> = Vec::new();
        let mut aggro = false;

        for act in &acts {
            // Fail / Drop components never run on a completed quest.
            if act.component_kind == KIND_FAIL || act.component_kind == KIND_DROP {
                continue;
            }
            match act.kind.as_str() {
                "QuestActSupplyExp" => {
                    if let Some(exp) = self.supply_exp(act.detail_id)? {
                        expected_exp += i64::from(exp);
                        exp_sources.push(format!(
                            "quest_act_supply_exps id {} exp {exp}",
                            act.detail_id
                        ));
                    }
                }
                "QuestActSupplyCopper" => {
                    if let Some(amount) = self.supply_copper(act.detail_id)? {
                        expected_copper += i64::from(amount);
                        copper_sources.push(format!(
                            "quest_act_supply_coppers id {} amount {amount}",
                            act.detail_id
                        ));
                    }
                }
                "QuestActSupplyItem" => {
                    if let Some((item_id, count)) = self.supply_item(act.detail_id)? {
                        let entry =
                            item_expected
                                .entry(item_id)
                                .or_insert_with(|| ItemExpectation {
                                    source: "supply".into(),
                                    ..ItemExpectation::default()
                                });
                        entry.expected += count;
                        entry.sources.push(format!(
                            "quest_act_supply_items id {} item {item_id} x{count}",
                            act.detail_id
                        ));
                    }
                }
                "QuestActSupplySelectiveItem" => {
                    if let Some((item_id, count)) = self.selective_item(act.detail_id)? {
                        // The server numbers the selective acts 1..n across the quest and only
                        // grants the chosen index; entering Reward with 0 defaults to 1
                        // (`engine.rs::default_selective_reward`).
                        selective.push((act.detail_id, act.component_kind, item_id, count));
                    }
                }
                "QuestActObjAggro" => aggro = true,
                _ => {}
            }
        }

        // The selection is 1-based; the runner sends 0, which the server turns into 1.
        let first_selective = {
            let n = selective.len() as i32;
            if (1..=n).contains(&w.selected_reward) {
                w.selected_reward
            } else {
                1
            }
        };
        for (index, (detail_id, component_kind, item_id, count)) in selective.iter().enumerate() {
            if *component_kind != KIND_REWARD {
                continue;
            }
            if index as i32 + 1 != first_selective {
                continue;
            }
            let entry = item_expected
                .entry(*item_id)
                .or_insert_with(|| ItemExpectation {
                    source: "selective".into(),
                    ..ItemExpectation::default()
                });
            entry.expected += count;
            entry.sources.push(format!(
                "quest_act_supply_selective_items id {} item {item_id} x{count} (index {})",
                detail_id,
                index + 1
            ));
        }

        if level > 0 {
            if let Some((exp, copper)) = self.level_supply(level)? {
                expected_exp += i64::from(exp);
                expected_copper += i64::from(copper);
                exp_sources.push(format!(
                    "quest_supplies[LEVEL={level}] exp {exp} (engine.rs::distribute_rewards)"
                ));
                copper_sources.push(format!(
                    "quest_supplies[LEVEL={level}] copper {copper} \
                     (engine.rs::distribute_rewards)"
                ));
            }
        }

        // ----- observed rewards -----
        let mut observed_items: BTreeMap<u32, i32> = BTreeMap::new();
        let mut observed_money: i64 = 0;
        for event in w.events {
            let GameEvent::ItemTask(info) = event else {
                continue;
            };
            if info.action == TASK_QUEST_SUPPLY_ITEMS {
                for item in &info.items {
                    if matches!(item.task, 5 | 6 | 15) {
                        *observed_items.entry(item.template).or_default() += item.count;
                    }
                }
            }
            if info.action == TASK_DEPOSIT_MONEY {
                observed_money += info.money;
            }
        }
        let looted_copper: i64 = w
            .loot
            .iter()
            .filter(|l| l.item_template_id == ITEM_COINS)
            .map(|l| i64::from(l.count))
            .sum();
        let quest_money = observed_money - looted_copper;

        // ----- observed kill XP and loot -----
        let self_kills: Vec<&KillInfo> = w
            .kills
            .iter()
            .filter(|kill| kill.killer_obj_id == w.self_obj)
            .collect();
        let mut kill_checks: Vec<KillCheck> = Vec::with_capacity(self_kills.len());
        let mut candidates: Vec<Vec<i64>> = Vec::with_capacity(self_kills.len());
        let mut loot_by_owner: HashMap<u32, Vec<u32>> = HashMap::new();
        for loot in w.loot {
            if loot.item_template_id != ITEM_COINS {
                loot_by_owner
                    .entry(loot.owner_obj_id)
                    .or_default()
                    .push(loot.item_template_id);
            }
        }

        for kill in &self_kills {
            let data_level = self.npc_level(kill.template_id)?.unwrap_or(0);
            let kill_exp = self.npc_kill_exp(kill.template_id, kill.level)?;
            let scaled =
                kill_exp_for(kill_exp as i32, 1.0, kill.player_level, kill.level).map(i64::from);
            let mut cand = vec![kill_exp];
            if let Some(scaled) = scaled {
                if scaled != kill_exp {
                    cand.push(scaled);
                }
            }
            candidates.push(cand);
            let looted = loot_by_owner.get(&kill.obj_id).cloned().unwrap_or_default();
            kill_checks.push(KillCheck {
                obj_id: kill.obj_id,
                npc_id: kill.template_id,
                level_observed: kill.level,
                level_data: data_level,
                player_level: kill.player_level,
                kill_exp,
                kill_exp_scaled: scaled,
                exp_observed: None,
                looted,
            });
            if data_level != 0 && kill.level != 0 && data_level != kill.level {
                report.suspects.push(format!(
                    "kill of npc {} (obj {}): SCUnitStatePacket level {} but npcs.LEVEL = {} \
                     (compact.sqlite3 npcs; npc_stats uses the announced level)",
                    kill.template_id, kill.obj_id, kill.level, data_level
                ));
            }
        }

        // Pair each kill with the experience gain the server sends immediately **before** the
        // death packet: kill XP is granted in `npc/death.rs::before_death`, before `base.DoDie`
        // broadcasts `SCUnitDeathPacket`. A gain that matches neither candidate is kept as that
        // kill's observation and reported; every gain not consumed by a kill is quest XP.
        let mut events: Vec<(u64, Option<usize>, i64, u64)> = Vec::new();
        for change in w.exp_changes {
            if change.obj_id == w.self_obj {
                events.push((change.seq, None, i64::from(change.exp), change.t_ms));
            }
        }
        for (index, kill) in self_kills.iter().enumerate() {
            events.push((kill.seq, Some(index), 0, kill.t_ms));
        }
        events.sort_by_key(|e| e.0);
        let mut assigned: Vec<Option<i64>> = vec![None; kill_checks.len()];
        // Gains that no following death has consumed yet; only the newest can be kill XP.
        let mut unmatched: Vec<(u64, i64)> = Vec::new();
        for (_, index, delta, t_ms) in events {
            let Some(kill_index) = index else {
                unmatched.push((t_ms, delta));
                continue;
            };
            // The kill-XP packet and the death packet share one server call, so they arrive
            // back to back; a gain from an earlier step (accept reward, ...) is older.
            let Some(&(exp_t, exp_delta)) = unmatched.last() else {
                continue;
            };
            if t_ms.saturating_sub(exp_t) > KILL_XP_WINDOW_MS {
                continue;
            }
            unmatched.pop();
            assigned[kill_index] = Some(exp_delta);
            if !candidates[kill_index].contains(&exp_delta) {
                let check = &kill_checks[kill_index];
                report.suspects.push(format!(
                    "kill of npc {} (obj {}): observed SCExpChanged delta {exp_delta} just before \
                     the death matches neither the untagged raw kill_exp {} nor \
                     death.rs:46 kill_exp_for(..., 1.0, player level {}, npc level {}) = {} \
                     (thin oracle: npcs/npc/stats.rs KillExp formula)",
                    check.npc_id,
                    check.obj_id,
                    check.kill_exp,
                    check.player_level,
                    check.level_observed,
                    check
                        .kill_exp_scaled
                        .map_or("none".to_string(), |v| v.to_string())
                ));
            }
        }
        let quest_xp: i64 = unmatched.iter().map(|(_, delta)| *delta).sum();
        let kill_xp: i64 = assigned.iter().flatten().sum();
        for (i, check) in kill_checks.iter_mut().enumerate() {
            check.exp_observed = assigned[i];
            if check.exp_observed.is_none()
                && check.kill_exp_scaled.is_some()
                && check.kill_exp != 0
            {
                report.notes.push(format!(
                    "kill of npc {} (obj {}) produced no SCExpChanged: a tagged solo player \
                     gets no kill XP on a 10+ level gap (death.rs:46) or the kill was unattributed",
                    check.npc_id, check.obj_id
                ));
            }
        }

        // ----- loot membership -----
        let mut checked_objects: BTreeSet<u32> = BTreeSet::new();
        for kill in &self_kills {
            if !checked_objects.insert(kill.obj_id) {
                continue;
            }
            let Some(looted) = loot_by_owner.get(&kill.obj_id) else {
                continue;
            };
            let packs = self.loot_packs(kill.template_id)?;
            if packs.is_empty() {
                report.notes.push(format!(
                    "npc {} has no loot_pack_dropping_npcs row; its {} looted item(s) cannot be \
                     checked",
                    kill.template_id,
                    looted.len()
                ));
                continue;
            }
            let allowed = self.loot_items(kill.template_id)?;
            for item in looted {
                if !allowed.contains(item) {
                    report.suspects.push(format!(
                        "loot from npc {} (obj {}): item {item} was taken but is not in its loot \
                         packs {packs:?} (loots.loot_pack_id via \
                         loot_pack_dropping_npcs.npc_id = {})",
                        kill.template_id, kill.obj_id, kill.template_id
                    ));
                }
            }
        }
        // Loot taken from an owner that was not a recorded kill is only noted.
        for owner in loot_by_owner.keys() {
            if !w.kills.iter().any(|k| k.obj_id == *owner) {
                report.notes.push(format!(
                    "SCLootItemTookPacket owner {owner} is not a recorded kill; its loot is not \
                     checked"
                ));
            }
        }

        // ----- compare rewards -----
        let reward_ratio_explained = let_it_done || aggro;
        if reward_ratio_explained {
            report.notes.push(format!(
                "XP/gold ratio not modelled: {} (the server applies the let_it_done objective \
                 ratio / the aggro rank ratio in engine.rs::distribute_rewards)",
                if aggro {
                    "QuestActObjAggro"
                } else {
                    "let_it_done"
                }
            ));
        }

        let mut xp = XpCheck {
            expected: expected_exp,
            observed: quest_xp,
            observed_kill: kill_xp,
            delta: expected_exp - quest_xp,
        };
        if !reward_ratio_explained && xp.delta != 0 {
            report.suspects.push(format!(
                "quest {quest_id}: observed {quest_xp} quest XP (+{kill_xp} paired kill XP) but \
                 expected {expected_exp} = {} (World:ExpRate scales gains, \
                 skills/progress.rs::add_exp_locked; acts.rs::QuestActSupplyExp adds act XP \
                 immediately, engine.rs::distribute_rewards adds the level supply)",
                if exp_sources.is_empty() {
                    "no quest XP act / level supply".to_string()
                } else {
                    exp_sources.join(" + ")
                }
            ));
        }
        if xp.expected == 0 && xp.observed == 0 && xp.observed_kill == 0 {
            xp = XpCheck::default();
        }
        report.xp = Some(xp);

        let gold = GoldCheck {
            expected: expected_copper,
            observed: quest_money,
            delta: expected_copper - quest_money,
        };
        if !reward_ratio_explained && gold.delta != 0 {
            report.suspects.push(format!(
                "quest {quest_id}: observed {quest_money} copper in DepositMoney tasks (minus \
                 {looted_copper} looted coin copper) but expected {expected_copper} = {} \
                 (engine.rs::distribute_rewards -> change_money)",
                if copper_sources.is_empty() {
                    "no quest copper act / level supply".to_string()
                } else {
                    copper_sources.join(" + ")
                }
            ));
        }
        report.gold = Some(gold);

        let pool_len = item_expected.len() as u32;
        for (template_id, expectation) in item_expected {
            let observed = observed_items.get(&template_id).copied().unwrap_or(0);
            let owned = w.bag_before.get(&template_id).copied().unwrap_or(0);
            let check = ItemCheck {
                template_id,
                source: expectation.source.clone(),
                expected: expectation.expected,
                observed,
                owned_before: owned,
            };
            let sources = expectation.sources.join(" + ");
            if observed < expectation.expected.saturating_sub(owned) {
                if aggro {
                    // `QuestActObjAggro` can switch the whole item pool off by rank
                    // (`allow_item_rewards`), which the data does not predict.
                    report.notes.push(format!(
                        "item {template_id} from {sources}: observed {observed} of {}; an \
                         aggro-rank act may set allow_item_rewards = false \
                         (acts.rs::QuestActObjAggro)",
                        expectation.expected
                    ));
                } else if pool_len > w.bag_free_slots {
                    report.notes.push(format!(
                        "item {template_id} from {sources}: observed {observed} of {} but the bag \
                         had {} free slot(s) for {pool_len} reward item type(s); the server mails \
                         the reward pool then (engine.rs::distribute_rewards)",
                        expectation.expected, w.bag_free_slots
                    ));
                } else {
                    report.suspects.push(format!(
                        "quest {quest_id}: item {template_id} from {sources}: observed {observed} \
                         created by QuestSupplyItems tasks (owned before: {owned}) but expected \
                         {} (acts.rs::QuestActSupplyItem tops up to the act count; \
                         engine.rs::distribute_rewards creates the pool)",
                        expectation.expected
                    ));
                }
            } else if observed > expectation.expected {
                report.suspects.push(format!(
                    "quest {quest_id}: item {template_id} from {sources}: observed {observed} \
                     created by QuestSupplyItems tasks but the data grants {} \
                     (quest_act_supply_items / quest_act_supply_selective_items)",
                    expectation.expected
                ));
            }
            report.items.push(check);
        }
        report.kills = kill_checks;
        Ok(report)
    }

    // ----- compact.sqlite3 lookups -----

    fn quest_row(&self, quest_id: u32) -> anyhow::Result<Option<(u8, bool)>> {
        let row = self
            .conn
            .query_row(
                "SELECT LEVEL, let_it_done FROM quest_contexts WHERE id = ?1",
                [quest_id],
                |r| Ok((r.get::<_, i64>(0)?, truthy(r.get_ref(1)?))),
            )
            .optional()?;
        Ok(row.map(|(level, let_it_done)| (clamp_u8(level), let_it_done)))
    }

    fn quest_acts(&self, quest_id: u32) -> anyhow::Result<Vec<ActRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT a.act_detail_type, a.act_detail_id, c.component_kind_id \
             FROM quest_acts a \
             JOIN quest_components c ON c.id = a.quest_component_id \
             WHERE c.quest_context_id = ?1 \
             ORDER BY c.component_kind_id, c.id, a.id",
        )?;
        let rows = stmt.query_map([quest_id], |r| {
            Ok(ActRow {
                kind: r.get::<_, String>(0)?,
                detail_id: r.get::<_, i64>(1)?,
                component_kind: clamp_u8(r.get::<_, i64>(2)?),
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    fn supply_exp(&self, detail_id: i64) -> anyhow::Result<Option<i32>> {
        Ok(self
            .conn
            .query_row(
                "SELECT exp FROM quest_act_supply_exps WHERE id = ?1",
                [detail_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .map(clamp_i32))
    }

    fn supply_copper(&self, detail_id: i64) -> anyhow::Result<Option<i32>> {
        Ok(self
            .conn
            .query_row(
                "SELECT amount FROM quest_act_supply_coppers WHERE id = ?1",
                [detail_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .map(clamp_i32))
    }

    fn supply_item(&self, detail_id: i64) -> anyhow::Result<Option<(u32, i32)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT item_id, count FROM quest_act_supply_items WHERE id = ?1",
                [detail_id],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()?
            .map(|(item, count)| (u32::try_from(item).unwrap_or(0), clamp_i32(count))))
    }

    fn selective_item(&self, detail_id: i64) -> anyhow::Result<Option<(u32, i32)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT item_id, count FROM quest_act_supply_selective_items WHERE id = ?1",
                [detail_id],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()?
            .map(|(item, count)| (u32::try_from(item).unwrap_or(0), clamp_i32(count))))
    }

    fn level_supply(&self, level: u8) -> anyhow::Result<Option<(i32, i32)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT exp, copper FROM quest_supplies WHERE LEVEL = ?1",
                [level],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()?
            .map(|(exp, copper)| (clamp_i32(exp), clamp_i32(copper))))
    }

    fn npc_row(&self, npc_id: u32) -> anyhow::Result<Option<NpcRow>> {
        self.conn
            .query_row(
                "SELECT LEVEL, exp_multiplier, exp_adder, no_exp, npc_template_id, \
                        npc_kind_id, npc_grade_id \
                 FROM npcs WHERE id = ?1",
                [npc_id],
                |r| {
                    Ok(NpcRow {
                        level: clamp_u8(r.get::<_, i64>(0)?),
                        exp_multiplier: r.get::<_, f64>(1)? as f32,
                        exp_adder: clamp_i32(r.get::<_, i64>(2)?),
                        no_exp: truthy(r.get_ref(3)?),
                        npc_template_id: clamp_u8(r.get::<_, i64>(4)?),
                        npc_kind_id: clamp_u8(r.get::<_, i64>(5)?),
                        npc_grade_id: clamp_u8(r.get::<_, i64>(6)?),
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    fn npc_level(&self, npc_id: u32) -> anyhow::Result<Option<u8>> {
        Ok(self.npc_row(npc_id)?.map(|row| row.level))
    }

    /// `npc_stats(...).kill_exp` for the NPC template at `level` (`npcs/npc/stats.rs`): the
    /// server's own formula chain. A thin oracle by construction.
    fn npc_kill_exp(&self, npc_id: u32, level: u8) -> anyhow::Result<i64> {
        let Some(row) = self.npc_row(npc_id)? else {
            return Ok(0);
        };
        if row.no_exp {
            return Ok(0);
        }
        let template = NpcTemplate {
            id: npc_id,
            name: String::new(),
            npc_grade_id: row.npc_grade_id,
            npc_kind_id: row.npc_kind_id,
            level,
            npc_template_id: row.npc_template_id,
            no_exp: row.no_exp,
            exp_multiplier: row.exp_multiplier,
            exp_adder: row.exp_adder,
            ..NpcTemplate::default()
        };
        let bonuses = Bonuses::default();
        let calc = NpcStatCalc {
            formulas: &self.formulas,
            template: &template,
            level,
            bonuses: &bonuses,
            mainhand: None,
            offhand: None,
            ranged: None,
        };
        Ok(i64::from(calc.compute().kill_exp))
    }

    fn loot_packs(&self, npc_id: u32) -> anyhow::Result<Vec<u32>> {
        let mut stmt = self.conn.prepare(
            "SELECT loot_pack_id FROM loot_pack_dropping_npcs WHERE npc_id = ?1 \
             ORDER BY loot_pack_id",
        )?;
        let rows = stmt.query_map([npc_id], |r| r.get::<_, i64>(0))?;
        Ok(rows
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|id| u32::try_from(id).unwrap_or(0))
            .collect())
    }

    fn loot_items(&self, npc_id: u32) -> anyhow::Result<BTreeSet<u32>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT l.item_id \
             FROM loots l \
             JOIN loot_pack_dropping_npcs p ON p.loot_pack_id = l.loot_pack_id \
             WHERE p.npc_id = ?1",
        )?;
        let rows = stmt.query_map([npc_id], |r| r.get::<_, i64>(0))?;
        Ok(rows
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|id| u32::try_from(id).unwrap_or(0))
            .collect())
    }
}

#[derive(Debug, Clone, Copy)]
struct NpcRow {
    level: u8,
    exp_multiplier: f32,
    exp_adder: i32,
    no_exp: bool,
    npc_template_id: u8,
    npc_kind_id: u8,
    npc_grade_id: u8,
}

fn clamp_u8(value: i64) -> u8 {
    u8::try_from(value.clamp(0, i64::from(u8::MAX))).unwrap_or(0)
}

fn clamp_i32(value: i64) -> i32 {
    i32::try_from(value.clamp(i64::from(i32::MIN), i64::from(i32::MAX))).unwrap_or(0)
}

/// The compact database stores booleans as `'t'` / `'f'` text (or 0/1 in fixtures).
fn truthy(value: ValueRef<'_>) -> bool {
    match value {
        ValueRef::Text(text) => matches!(text, b"t" | b"true" | b"1"),
        ValueRef::Integer(value) => value != 0,
        _ => false,
    }
}

/// Test-only helpers for the fixture tables.
#[cfg(test)]
fn task_item(template: u32, count: i32) -> crate::game::TaskItem {
    crate::game::TaskItem {
        task: 5,
        template,
        id: u64::from(template) * 10,
        count,
        mate: None,
    }
}

/// A fixture `GameEvent::ItemTask` with the given task type / items / money.
#[cfg(test)]
fn item_task(action: u8, items: Vec<crate::game::TaskItem>, money: i64) -> GameEvent {
    GameEvent::ItemTask(crate::game::ItemTaskInfo {
        action,
        items,
        money,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"
        CREATE TABLE quest_contexts (id INTEGER, LEVEL INTEGER, let_it_done TEXT);
        CREATE TABLE quest_components (id INTEGER, quest_context_id INTEGER, component_kind_id INTEGER);
        CREATE TABLE quest_acts (id INTEGER, quest_component_id INTEGER, act_detail_id INTEGER, act_detail_type TEXT);
        CREATE TABLE quest_act_supply_exps (id INTEGER, exp INTEGER);
        CREATE TABLE quest_act_supply_coppers (id INTEGER, amount INTEGER);
        CREATE TABLE quest_act_supply_items (
            id INTEGER, item_id INTEGER, count INTEGER, grade_id INTEGER,
            show_action_bar TEXT, cleanup TEXT, drop_when_destroy TEXT, destroy_when_drop TEXT
        );
        CREATE TABLE quest_act_supply_selective_items (id INTEGER, item_id INTEGER, count INTEGER, grade_id INTEGER);
        CREATE TABLE quest_supplies (id INTEGER, LEVEL INTEGER, exp INTEGER, copper INTEGER);
        CREATE TABLE npcs (
            id INTEGER, name TEXT, LEVEL INTEGER, exp_multiplier REAL, exp_adder INTEGER,
            no_exp TEXT, npc_template_id INTEGER, npc_kind_id INTEGER, npc_grade_id INTEGER
        );
        CREATE TABLE loots (
            id INTEGER, "group" INTEGER, item_id INTEGER, drop_rate INTEGER,
            min_amount INTEGER, max_amount INTEGER, loot_pack_id INTEGER, grade_id INTEGER,
            always_drop TEXT
        );
        CREATE TABLE loot_pack_dropping_npcs (
            id INTEGER, npc_id INTEGER, loot_pack_id INTEGER, default_pack TEXT
        );
        CREATE TABLE unit_formulas (id INTEGER, formula TEXT, kind_id INTEGER, owner_type_id INTEGER);
        CREATE TABLE unit_formula_variables (
            id INTEGER, unit_formula_id INTEGER, variable_kind_id INTEGER, key INTEGER, value REAL
        );
        CREATE TABLE wearable_formulas (id INTEGER, kind_id INTEGER, formula TEXT);
        CREATE TABLE formulas (id INTEGER, formula TEXT);

        -- Quest 1: level 5, supply acts in Start and Reward, a selective item and no kills.
        INSERT INTO quest_contexts VALUES (1, 5, 'f');
        INSERT INTO quest_components VALUES (10, 1, 2), (11, 1, 4), (12, 1, 8);
        INSERT INTO quest_acts VALUES
            (100, 10, 1000, 'QuestActSupplyExp'),
            (101, 10, 1001, 'QuestActSupplyItem'),
            (102, 12, 1002, 'QuestActSupplyExp'),
            (103, 12, 1003, 'QuestActSupplyCopper'),
            (104, 12, 1004, 'QuestActSupplyItem'),
            (105, 12, 1005, 'QuestActSupplySelectiveItem'),
            (106, 11, 1006, 'QuestActObjMonsterHunt');
        INSERT INTO quest_act_supply_exps VALUES (1000, 50), (1002, 70);
        INSERT INTO quest_act_supply_coppers VALUES (1003, 10);
        INSERT INTO quest_act_supply_items VALUES
            (1001, 100, 2, 0, 't', 't', 't', 't'),
            (1004, 200, 1, 0, 't', 't', 't', 't');
        INSERT INTO quest_act_supply_selective_items VALUES (1005, 300, 1, 0);
        INSERT INTO quest_supplies VALUES (1, 5, 30, 7);

        -- Quest 2: one kill of npc 777 at level 5, loot pack 9 with item 400.
        INSERT INTO quest_contexts VALUES (2, 1, 'f');
        INSERT INTO quest_components VALUES (20, 2, 4), (21, 2, 8);
        INSERT INTO quest_acts VALUES (200, 20, 2000, 'QuestActObjMonsterHunt');
        INSERT INTO quest_supplies VALUES (2, 1, 0, 0);
        INSERT INTO npcs VALUES (777, 'Mob', 5, 1.0, 0, 'f', 1, 1, 1);
        INSERT INTO loots VALUES (1, 1, 400, 10000000, 1, 1, 9, 0, 't');
        INSERT INTO loot_pack_dropping_npcs VALUES (1, 777, 9, 't');

        -- The npc KillExp formula kind (34), owner Npc (1): level * 10.
        INSERT INTO unit_formulas VALUES (1, 'level * 10', 34, 1);
    "#;

    fn checker() -> ValueChecker {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(FIXTURE).unwrap();
        ValueChecker::from_connection(conn).unwrap()
    }

    fn kill(obj: u32, npc: u32, level: u8, player_level: u8, seq: u64) -> KillInfo {
        KillInfo {
            obj_id: obj,
            template_id: npc,
            level,
            killer_obj_id: 0x100,
            player_level,
            seq,
            t_ms: seq * 10,
        }
    }

    fn change(exp: i32, seq: u64) -> ExpChange {
        ExpChange {
            obj_id: 0x100,
            exp,
            seq,
            t_ms: seq * 10,
        }
    }

    fn window<'a>(
        bag_before: &'a BTreeMap<u32, i32>,
        exp_changes: &'a [ExpChange],
        events: &'a [GameEvent],
        kills: &'a [KillInfo],
        loot: &'a [LootTook],
    ) -> QuestWindow<'a> {
        QuestWindow {
            self_obj: 0x100,
            selected_reward: 0,
            bag_free_slots: 45,
            bag_before,
            exp_changes,
            events,
            kills,
            loot,
        }
    }

    #[test]
    fn clean_quest_matches_acts_level_supply_items_and_money() {
        let events = vec![
            item_task(TASK_QUEST_SUPPLY_ITEMS, vec![task_item(100, 2)], 0),
            item_task(TASK_DEPOSIT_MONEY, Vec::new(), 17),
            item_task(TASK_QUEST_SUPPLY_ITEMS, vec![task_item(200, 1)], 0),
            item_task(TASK_QUEST_SUPPLY_ITEMS, vec![task_item(300, 1)], 0),
        ];
        let exp = vec![change(50, 1), change(70, 2), change(30, 3)];
        let bag = BTreeMap::new();
        let report = checker()
            .check(1, &window(&bag, &exp, &events, &[], &[]))
            .unwrap();
        assert!(report.suspects.is_empty(), "{report:?}");
        assert_eq!(
            report.xp,
            Some(XpCheck {
                expected: 150,
                observed: 150,
                observed_kill: 0,
                delta: 0
            })
        );
        assert_eq!(
            report.gold,
            Some(GoldCheck {
                expected: 17,
                observed: 17,
                delta: 0
            })
        );
        assert_eq!(report.items.len(), 3);
        assert!(report.items.iter().all(|i| i.observed == i.expected));
        assert!(report.kills.is_empty());
    }

    #[test]
    fn missing_xp_and_gold_are_suspects_with_the_rows() {
        let events = vec![
            item_task(TASK_QUEST_SUPPLY_ITEMS, vec![task_item(100, 2)], 0),
            item_task(TASK_DEPOSIT_MONEY, Vec::new(), 5),
            item_task(TASK_QUEST_SUPPLY_ITEMS, vec![task_item(200, 1)], 0),
            item_task(TASK_QUEST_SUPPLY_ITEMS, vec![task_item(300, 1)], 0),
        ];
        let exp = vec![change(50, 1), change(70, 2)];
        let bag = BTreeMap::new();
        let report = checker()
            .check(1, &window(&bag, &exp, &events, &[], &[]))
            .unwrap();
        assert_eq!(report.xp.as_ref().unwrap().delta, 30);
        assert_eq!(report.gold.as_ref().unwrap().delta, 12);
        assert!(report
            .suspects
            .iter()
            .any(|s| s.contains("quest_supplies[LEVEL=5] exp 30")));
        assert!(report
            .suspects
            .iter()
            .any(|s| s.contains("observd 5 copper") || s.contains("observed 5 copper")));
    }

    #[test]
    fn pre_owned_items_and_full_bag_are_explained_not_suspects() {
        let events = vec![item_task(
            TASK_QUEST_SUPPLY_ITEMS,
            vec![task_item(100, 1)],
            0,
        )];
        let exp = vec![change(50, 1), change(70, 2), change(30, 3)];
        let mut bag = BTreeMap::new();
        bag.insert(100, 1);
        let mut w = window(&bag, &exp, &events, &[], &[]);
        w.bag_free_slots = 0;
        let report = checker().check(1, &w).unwrap();
        assert!(
            report.suspects.iter().all(|s| !s.contains("item 100")
                && !s.contains("item 200")
                && !s.contains("item 300")),
            "{report:?}"
        );
        assert!(report.notes.iter().any(|n| n.contains("free slot")));
    }

    #[test]
    fn kill_xp_level_and_loot_are_checked_against_the_data() {
        // Kill XP 50 (level * 10) is sent just before the death packet; loot 400 and a bogus
        // 401 are then taken from the corpse.
        let exp = vec![change(50, 1)];
        let kills = vec![kill(0x200, 777, 5, 5, 2)];
        let loot = vec![
            LootTook {
                item_template_id: 400,
                owner_obj_id: 0x200,
                count: 1,
            },
            LootTook {
                item_template_id: 401,
                owner_obj_id: 0x200,
                count: 1,
            },
        ];
        let bag = BTreeMap::new();
        let report = checker()
            .check(2, &window(&bag, &exp, &[], &kills, &loot))
            .unwrap();
        assert_eq!(report.kills.len(), 1);
        let check = &report.kills[0];
        assert_eq!((check.level_observed, check.level_data), (5, 5));
        assert_eq!(check.kill_exp, 50);
        assert_eq!(check.kill_exp_scaled, Some(50));
        assert_eq!(check.exp_observed, Some(50));
        assert_eq!(check.looted, vec![400, 401]);
        assert!(report
            .suspects
            .iter()
            .any(|s| s.contains("item 401") && s.contains("[9]")));
        assert_eq!(report.suspects.len(), 1, "{report:?}");
    }

    #[test]
    fn level_mismatch_and_unexplained_kill_xp_are_suspects() {
        let exp = vec![change(47, 1)];
        let kills = vec![kill(0x200, 777, 6, 6, 2)];
        let bag = BTreeMap::new();
        let report = checker()
            .check(2, &window(&bag, &exp, &[], &kills, &[]))
            .unwrap();
        assert!(report.suspects.iter().any(|s| s.contains("npcs.LEVEL = 5")));
        assert!(report
            .suspects
            .iter()
            .any(|s| s.contains("observed SCExpChanged delta 47")));
        // The oracle computed the kill XP at the announced level (6), not npcs.LEVEL.
        assert_eq!(report.kills[0].kill_exp, 60);
    }

    #[test]
    fn reward_gain_after_a_death_is_not_paired_as_kill_xp() {
        // Exact live shape (quests 2263 / 2485 / 2491 / 2494): kill XP 50 arrives just before
        // the death (seq 1, t=10 ms); the quest reward 150 arrives later (t=5 s) and must stay
        // quest XP. Before the time window existed the reward was mispaired and both checks
        // reported suspects.
        let exp = vec![
            change(50, 1),
            ExpChange {
                obj_id: 0x100,
                exp: 150,
                seq: 3,
                t_ms: 5_000,
            },
        ];
        let kills = vec![kill(0x200, 777, 5, 5, 2)];
        let events = vec![
            item_task(TASK_QUEST_SUPPLY_ITEMS, vec![task_item(100, 2)], 0),
            item_task(TASK_DEPOSIT_MONEY, Vec::new(), 17),
            item_task(TASK_QUEST_SUPPLY_ITEMS, vec![task_item(200, 1)], 0),
            item_task(TASK_QUEST_SUPPLY_ITEMS, vec![task_item(300, 1)], 0),
        ];
        let bag = BTreeMap::new();
        let report = checker()
            .check(1, &window(&bag, &exp, &events, &kills, &[]))
            .unwrap();
        assert!(report.suspects.is_empty(), "{report:?}");
        assert_eq!(
            report.xp,
            Some(XpCheck {
                expected: 150,
                observed: 150,
                observed_kill: 50,
                delta: 0
            })
        );
        assert_eq!(report.kills[0].exp_observed, Some(50));
    }

    #[test]
    fn let_it_done_ratio_is_noted_not_suspected() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(FIXTURE).unwrap();
        conn.execute(
            "UPDATE quest_contexts SET let_it_done = 't' WHERE id = 1",
            [],
        )
        .unwrap();
        let checker = ValueChecker::from_connection(conn).unwrap();
        // The server applies the objective ratio to the reward pool only (acts.rs; SupplyItem
        // items are not scaled): 0.3 x 150 XP = 45, 0.3 x 17 copper = 5.
        let events = vec![
            item_task(TASK_QUEST_SUPPLY_ITEMS, vec![task_item(100, 2)], 0),
            item_task(TASK_DEPOSIT_MONEY, Vec::new(), 5),
            item_task(TASK_QUEST_SUPPLY_ITEMS, vec![task_item(200, 1)], 0),
            item_task(TASK_QUEST_SUPPLY_ITEMS, vec![task_item(300, 1)], 0),
        ];
        let exp = vec![change(45, 1)];
        let bag = BTreeMap::new();
        let report = checker
            .check(1, &window(&bag, &exp, &events, &[], &[]))
            .unwrap();
        assert!(report.suspects.is_empty(), "{report:?}");
        assert!(report.notes.iter().any(|n| n.contains("let_it_done")));
    }
}
