//! Read-only lookups from the same `compact.sqlite3` the game server loads. A real 1.2 client
//! has this data locally (item templates, interaction skills); the headless bot reads the same
//! database for the two decisions that are not on the wire:
//!
//! * `items.use_skill_id` + the skill's `target_type_id` for `CSStartSkillPacket` item use
//!   (`systems/skills/handlers.rs` requires `skill_id == use_skill_id`);
//! * the skill with an `InteractionEffect` of `WorldInteractionType.Use` (19) that casts on a
//!   doodad (`systems/npcs/bridges.rs::interaction_effect`, `doodads::use_doodad`).

use std::cell::OnceCell;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::Context;

/// One `loots` row (grouped by pack) that can drop `item_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactLootSource {
    pub loot_pack_id: u32,
    /// `loot_pack_dropping_npcs` of the pack (empty when no NPC drops it).
    pub npc_ids: Vec<u32>,
    /// Raw `loots.drop_rate` (10_000_000 = 100%).
    pub chance: i64,
    /// `items.loot_quest_id` when the drop is gated to a quest.
    pub loot_quest_id: Option<u32>,
}

/// One doodad func that can give `item_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactDoodadSource {
    /// `DoodadFuncLootItem` / `DoodadFuncLootPack` / `DoodadFuncPurchase`.
    pub func: String,
    pub doodad_ids: Vec<u32>,
}

/// One bundle item whose use skill grants `item_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactContainerSource {
    pub container_item_id: u32,
    pub skill_id: u32,
    /// `gain_item` / `skill_product`; absent for a `GainLootPackItemEffect` loot pack.
    pub grant: Option<String>,
    pub loot_pack_id: u32,
    pub chance: i64,
}

/// The merchant NPCs of one `merchant_packs` row that sells `item_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactStoreSource {
    pub npc_ids: Vec<u32>,
    pub pack_ids: Vec<u32>,
}

/// Application data path used by `aaemu-game` when it runs from the repository.
pub const DEFAULT_COMPACT_DB: &str = "rust/crates/aaemu-game/app/Data/compact.sqlite3";
/// Fallback when the runner is started from `rust/`.
pub const DEFAULT_COMPACT_DB_FROM_RUST: &str = "crates/aaemu-game/app/Data/compact.sqlite3";

/// `WorldInteractionType.Use` (`systems/npcs/doodad/interactions.rs`).
const WI_USE: i64 = 19;

/// What `items.use_skill_id` resolves to in the game data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemUseSkill {
    /// The use skill exists; `target_type_id` is its `SkillTargetType`.
    Known { skill_id: u32, target_type: u8 },
    /// `use_skill_id` points at a skill row that is missing from the data: the server's item
    /// cast is dropped (`systems/skills/handlers.rs::template_skill` returns `None`), so the
    /// use cannot advance a `QuestActObjItemUse` objective.
    MissingSkill { skill_id: u32 },
    /// The item has no `use_skill_id` (0 / NULL).
    None,
}

/// Open `path` read-only; the connection is used for single-row lookups only.
pub struct CompactDb {
    conn: rusqlite::Connection,
    path: PathBuf,
    /// `world_spawns.json` world name -> zone key, loaded once (`world_zone_key`).
    worlds: OnceCell<HashMap<String, u32>>,
}

impl CompactDb {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn =
            rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .with_context(|| format!("opening compact database {}", path.display()))?;
        Ok(Self {
            conn,
            path: path.to_path_buf(),
            worlds: OnceCell::new(),
        })
    }

    /// `items.use_skill_id` and, when the skill exists, its `target_type_id`
    /// (`SkillTargetType`).
    pub fn item_use_skill(&self, item_id: u32) -> anyhow::Result<ItemUseSkill> {
        let skill_id = self
            .conn
            .query_row(
                "SELECT use_skill_id FROM items WHERE id = ?1",
                [item_id],
                |r| r.get::<_, Option<u32>>(0),
            )
            .optional()?
            .flatten()
            .unwrap_or(0);
        if skill_id == 0 {
            return Ok(ItemUseSkill::None);
        }
        let target_type = self
            .conn
            .query_row(
                "SELECT target_type_id FROM skills WHERE id = ?1",
                [skill_id],
                |r| r.get::<_, Option<u8>>(0),
            )
            .optional()?
            .flatten();
        Ok(match target_type {
            Some(target_type) => ItemUseSkill::Known {
                skill_id,
                target_type,
            },
            None => ItemUseSkill::MissingSkill { skill_id },
        })
    }

    /// The lowest skill id whose effect is an `InteractionEffect` with `wi_id = 19`
    /// (`InteractionEffect.Apply` -> `doodads::use_doodad` for any doodad target).
    pub fn interaction_use_skill(&self) -> anyhow::Result<Option<u32>> {
        let row = self
            .conn
            .query_row(
                "SELECT se.skill_id \
                 FROM skill_effects se \
                 JOIN effects ef ON ef.id = se.effect_id \
                 JOIN interaction_effects ie ON ie.id = ef.actual_id \
                 WHERE ef.actual_type = 'InteractionEffect' AND ie.wi_id = ?1 \
                 ORDER BY se.skill_id LIMIT 1",
                [WI_USE],
                |r| r.get::<_, u32>(0),
            )
            .optional()?;
        Ok(row)
    }

    /// `system_factions.mother_id` of a faction, as the server resolves it
    /// (`quests/engine.rs` `mother_faction_id`; `data/world_data.rs::FactionData::load`).
    /// `None` when the faction has no row.
    pub fn mother_faction(&self, faction_id: u32) -> anyhow::Result<Option<u32>> {
        let row = self
            .conn
            .query_row(
                "SELECT mother_id FROM system_factions WHERE id = ?1",
                [faction_id],
                |r| r.get::<_, u32>(0),
            )
            .optional()?;
        Ok(row)
    }

    /// `loots` -> `loot_pack_dropping_npcs`: the NPC drops of `item_id`, one entry per pack
    /// (`economy/loot_data.rs`; the same reverse lookup `plan.py::item_sources` makes).
    pub fn item_loot_sources(&self, item_id: u32) -> anyhow::Result<Vec<CompactLootSource>> {
        let loot_quest_id: Option<u32> = self
            .conn
            .query_row(
                "SELECT loot_quest_id FROM items WHERE id = ?1",
                [item_id],
                |r| r.get::<_, Option<u32>>(0),
            )
            .optional()?
            .flatten();
        let mut stmt = self.conn.prepare(
            "SELECT l.loot_pack_id, l.drop_rate, n.npc_id \
             FROM loots l \
             LEFT JOIN loot_pack_dropping_npcs n ON n.loot_pack_id = l.loot_pack_id \
             WHERE l.item_id = ?1 \
             ORDER BY l.loot_pack_id, n.npc_id",
        )?;
        let rows = stmt.query_map([item_id], |r| {
            Ok((
                r.get::<_, u32>(0)?,
                r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                r.get::<_, Option<u32>>(2)?,
            ))
        })?;
        let mut packs: BTreeMap<u32, CompactLootSource> = BTreeMap::new();
        for row in rows {
            let (pack, chance, npc) = row?;
            let entry = packs.entry(pack).or_insert_with(|| CompactLootSource {
                loot_pack_id: pack,
                npc_ids: Vec::new(),
                chance,
                loot_quest_id,
            });
            if let Some(npc) = npc {
                entry.npc_ids.push(npc);
            }
        }
        Ok(packs.into_values().collect())
    }

    /// The doodad funcs (`DoodadFuncLootItem` / `LootPack` / `Purchase`) that can give
    /// `item_id`, resolved through `doodad_funcs` -> `doodad_func_groups` -> doodad templates
    /// (`npcs/doodad/funcs/items.rs`).
    pub fn item_doodad_sources(&self, item_id: u32) -> anyhow::Result<Vec<CompactDoodadSource>> {
        let queries = [
            (
                "DoodadFuncLootItem",
                "SELECT DISTINCT dfa.id \
                 FROM doodad_func_loot_items li \
                 JOIN doodad_funcs df ON df.actual_func_type = 'DoodadFuncLootItem' \
                     AND df.actual_func_id = li.id \
                 JOIN doodad_func_groups g ON g.id = df.doodad_func_group_id \
                 JOIN doodad_almighties dfa ON dfa.id = g.doodad_almighty_id \
                 WHERE li.item_id = ?1 ORDER BY dfa.id",
            ),
            (
                "DoodadFuncLootPack",
                "SELECT DISTINCT dfa.id \
                 FROM loots l \
                 JOIN doodad_func_loot_packs lp ON lp.loot_pack_id = l.loot_pack_id \
                 JOIN doodad_funcs df ON df.actual_func_type = 'DoodadFuncLootPack' \
                     AND df.actual_func_id = lp.id \
                 JOIN doodad_func_groups g ON g.id = df.doodad_func_group_id \
                 JOIN doodad_almighties dfa ON dfa.id = g.doodad_almighty_id \
                 WHERE l.item_id = ?1 ORDER BY dfa.id",
            ),
            (
                "DoodadFuncPurchase",
                "SELECT DISTINCT dfa.id \
                 FROM doodad_func_purchases p \
                 JOIN doodad_funcs df ON df.actual_func_type = 'DoodadFuncPurchase' \
                     AND df.actual_func_id = p.id \
                 JOIN doodad_func_groups g ON g.id = df.doodad_func_group_id \
                 JOIN doodad_almighties dfa ON dfa.id = g.doodad_almighty_id \
                 WHERE p.item_id = ?1 ORDER BY dfa.id",
            ),
        ];
        let mut out = Vec::new();
        for (func, sql) in queries {
            let mut stmt = self.conn.prepare(sql)?;
            let ids = stmt
                .query_map([item_id], |r| r.get::<_, u32>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            if !ids.is_empty() {
                out.push(CompactDoodadSource {
                    func: func.into(),
                    doodad_ids: ids,
                });
            }
        }
        Ok(out)
    }

    /// `craft_products`: the recipes that make `item_id`.
    pub fn item_craft_ids(&self, item_id: u32) -> anyhow::Result<Vec<u32>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT craft_id FROM craft_products WHERE item_id = ?1 ORDER BY craft_id",
        )?;
        let out = stmt
            .query_map([item_id], |r| r.get::<_, u32>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(out)
    }

    /// `merchant_goods` -> `merchants` / `merchant_packs`: the vendors that sell `item_id`
    /// (`systems/economy/store.rs`).
    pub fn item_store_sources(&self, item_id: u32) -> anyhow::Result<Vec<CompactStoreSource>> {
        let mut stmt = self.conn.prepare(
            "SELECT npc_id, pack FROM ( \
                 SELECT m.npc_id AS npc_id, mg.merchant_pack_id AS pack \
                 FROM merchant_goods mg \
                 JOIN merchants m ON m.merchant_pack_id = mg.merchant_pack_id \
                 WHERE mg.item_id = ?1 \
                 UNION \
                 SELECT p.owner_npc_id AS npc_id, mg.merchant_pack_id AS pack \
                 FROM merchant_goods mg \
                 JOIN merchant_packs p ON p.id = mg.merchant_pack_id \
                 WHERE mg.item_id = ?1 AND p.owner_npc_id IS NOT NULL AND p.owner_npc_id > 0 \
             ) WHERE npc_id IS NOT NULL ORDER BY pack, npc_id",
        )?;
        let rows = stmt.query_map([item_id], |r| {
            Ok((r.get::<_, u32>(0)?, r.get::<_, u32>(1)?))
        })?;
        let mut packs: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for row in rows {
            let (npc, pack) = row?;
            packs.entry(pack).or_default().push(npc);
        }
        Ok(packs
            .into_iter()
            .map(|(pack, npc_ids)| CompactStoreSource {
                npc_ids,
                pack_ids: vec![pack],
            })
            .collect())
    }

    /// The bundle items whose use skill grants `item_id` (`GainLootPackItemEffect` with the
    /// target's loot pack, `GainItem` special 27, or `skill_products`).
    pub fn item_container_sources(
        &self,
        item_id: u32,
    ) -> anyhow::Result<Vec<CompactContainerSource>> {
        let mut out: BTreeMap<(u32, u32), CompactContainerSource> = BTreeMap::new();
        let mut stmt = self.conn.prepare(
            "SELECT i.id, se.skill_id, gl.loot_pack_id \
             FROM items i \
             JOIN skill_effects se ON se.skill_id = i.use_skill_id \
             JOIN effects ef ON ef.id = se.effect_id \
             JOIN gain_loot_pack_item_effects gl ON gl.id = ef.actual_id \
             WHERE ef.actual_type = 'GainLootPackItemEffect' \
               AND gl.loot_pack_id IN (SELECT loot_pack_id FROM loots WHERE item_id = ?1) \
               AND i.id != ?1 \
             ORDER BY i.id, se.skill_id",
        )?;
        for row in stmt.query_map([item_id], |r| {
            Ok((
                r.get::<_, u32>(0)?,
                r.get::<_, u32>(1)?,
                r.get::<_, u32>(2)?,
            ))
        })? {
            let (container, skill, pack) = row?;
            out.entry((container, skill))
                .or_insert(CompactContainerSource {
                    container_item_id: container,
                    skill_id: skill,
                    grant: None,
                    loot_pack_id: pack,
                    chance: 10_000_000,
                });
        }
        let mut stmt = self.conn.prepare(
            "SELECT i.id, se.skill_id \
             FROM items i \
             JOIN skill_effects se ON se.skill_id = i.use_skill_id \
             JOIN effects ef ON ef.id = se.effect_id \
             JOIN special_effects sp ON sp.id = ef.actual_id \
             WHERE ef.actual_type = 'SpecialEffect' \
               AND sp.special_effect_type_id = 27 AND sp.value1 = ?1 \
               AND i.id != ?1 \
             ORDER BY i.id, se.skill_id",
        )?;
        for row in stmt.query_map([item_id], |r| {
            Ok((r.get::<_, u32>(0)?, r.get::<_, u32>(1)?))
        })? {
            let (container, skill) = row?;
            out.entry((container, skill))
                .or_insert(CompactContainerSource {
                    container_item_id: container,
                    skill_id: skill,
                    grant: Some("gain_item".into()),
                    loot_pack_id: 0,
                    chance: 10_000_000,
                });
        }
        let mut stmt = self.conn.prepare(
            "SELECT i.id, sp.skill_id \
             FROM skill_products sp \
             JOIN skills s ON s.id = sp.skill_id \
             JOIN items i ON i.use_skill_id = sp.skill_id \
             WHERE sp.item_id = ?1 AND i.id != ?1 \
             ORDER BY i.id, sp.skill_id",
        )?;
        for row in stmt.query_map([item_id], |r| {
            Ok((r.get::<_, u32>(0)?, r.get::<_, u32>(1)?))
        })? {
            let (container, skill) = row?;
            out.entry((container, skill))
                .or_insert(CompactContainerSource {
                    container_item_id: container,
                    skill_id: skill,
                    grant: Some("skill_product".into()),
                    loot_pack_id: 0,
                    chance: 10_000_000,
                });
        }
        Ok(out.into_values().collect())
    }

    /// Other quests whose reward acts supply `item_id` (report-only source; the runner does not
    /// run another quest for it).
    pub fn item_other_quest_rewards(&self, item_id: u32) -> anyhow::Result<Vec<u32>> {
        let mut stmt = self.conn.prepare(
            "SELECT quest_id FROM ( \
                 SELECT DISTINCT qc.quest_context_id AS quest_id \
                 FROM quest_act_supply_items s \
                 JOIN quest_acts qa ON qa.act_detail_id = s.id \
                     AND qa.act_detail_type = 'QuestActSupplyItem' \
                 JOIN quest_components qc ON qc.id = qa.quest_component_id \
                 WHERE s.item_id = ?1 \
                 UNION \
                 SELECT DISTINCT qc.quest_context_id AS quest_id \
                 FROM quest_act_supply_selective_items s \
                 JOIN quest_acts qa ON qa.act_detail_id = s.id \
                     AND qa.act_detail_type = 'QuestActSupplySelectiveItem' \
                 JOIN quest_components qc ON qc.id = qa.quest_component_id \
                 WHERE s.item_id = ?1 \
             ) ORDER BY quest_id",
        )?;
        let out = stmt
            .query_map([item_id], |r| r.get::<_, u32>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(out)
    }

    /// The skills that drive the doodad's `Start`-kind phase groups (the phase a fresh spawn
    /// begins in); empty when the template has no Start group. Used to filter entrance doodads
    /// whose skill the character cannot cast (faction / race requirements).
    pub fn doodad_start_skills(&self, doodad_template: u32) -> anyhow::Result<Vec<u32>> {
        let mut stmt = self.conn.prepare(
            "SELECT x.skill FROM ( \
                 SELECT df.func_skill_id AS skill, df.id AS fid FROM doodad_funcs df \
                 JOIN doodad_func_groups g ON g.id = df.doodad_func_group_id \
                 WHERE g.doodad_almighty_id = ?1 AND g.doodad_func_group_kind_id = 1 \
                     AND df.func_skill_id > 0 \
                 UNION \
                 SELECT fu.fake_skill_id AS skill, df.id AS fid FROM doodad_funcs df \
                 JOIN doodad_func_groups g ON g.id = df.doodad_func_group_id \
                 JOIN doodad_func_fake_uses fu ON df.actual_func_type = 'DoodadFuncFakeUse' \
                     AND fu.id = df.actual_func_id \
                 WHERE g.doodad_almighty_id = ?1 AND g.doodad_func_group_kind_id = 1 \
                     AND fu.fake_skill_id > 0 \
                 UNION \
                 SELECT du.skill_id AS skill, df.id AS fid FROM doodad_funcs df \
                 JOIN doodad_func_groups g ON g.id = df.doodad_func_group_id \
                 JOIN doodad_func_uses du ON df.actual_func_type = 'DoodadFuncUse' \
                     AND du.id = df.actual_func_id \
                 WHERE g.doodad_almighty_id = ?1 AND g.doodad_func_group_kind_id = 1 \
                     AND du.skill_id > 0 \
             ) x JOIN skills s ON s.id = x.skill \
             ORDER BY x.fid",
        )?;
        let rows = stmt.query_map([doodad_template], |r| r.get::<_, u32>(0))?;
        let mut out = Vec::new();
        for row in rows {
            let skill = row?;
            if !out.contains(&skill) {
                out.push(skill);
            }
        }
        Ok(out)
    }

    /// A `unit_reqs` row of `skill_id` (`owner_type = 'Skill'`) the character cannot satisfy:
    /// `Race` / `FactionMatch(-Only)(-Not)` / `MotherFaction(-Only)(-Not)`
    /// (`skills/unit_reqs.rs::validate`). `None` when the skill has no such requirement or the
    /// character facts are unknown (the server then decides).
    pub fn skill_character_block(
        &self,
        skill_id: u32,
        race: Option<u8>,
        faction: Option<u32>,
        mother_faction: Option<u32>,
    ) -> anyhow::Result<Option<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT kind_id, value1 FROM unit_reqs \
             WHERE owner_type = 'Skill' AND owner_id = ?1",
        )?;
        let rows = stmt.query_map([skill_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (kind_id, value1) = row?;
            let blocked = match kind_id {
                0x03 => race.is_some_and(|race| i64::from(race) != value1),
                0x28 | 0x37 => faction.is_some_and(|faction| i64::from(faction) != value1),
                0x3A => faction.is_some_and(|faction| i64::from(faction) == value1),
                0x2A | 0x38 => mother_faction.is_some_and(|mother| i64::from(mother) != value1),
                0x3B => mother_faction.is_some_and(|mother| i64::from(mother) == value1),
                _ => false,
            };
            if blocked {
                let name = match kind_id {
                    0x03 => "race",
                    0x28 | 0x37 => "faction",
                    0x3A => "not-faction",
                    0x2A | 0x38 => "mother_faction",
                    0x3B => "not-mother_faction",
                    _ => "kind",
                };
                return Ok(Some(format!(
                    "the skill's unit_reqs require {name} {value1} (kind_id {kind_id})"
                )));
            }
        }
        Ok(None)
    }

    /// The NPC whose `EngageCombatGiveQuestId` gives `quest_id` (the NPC acceptor of
    /// `QuestActObjAggro`; `quests/lifecycle_sim_tests.rs` documents the live path).
    pub fn engage_quest_npc(&self, quest_id: u32) -> anyhow::Result<Option<u32>> {
        let row = self
            .conn
            .query_row(
                "SELECT id FROM npcs WHERE engage_combat_give_quest_id = ?1 ORDER BY id LIMIT 1",
                [quest_id],
                |r| r.get::<_, u32>(0),
            )
            .optional()?;
        Ok(row)
    }

    /// The required character level of a `QuestActObjLevel` act (`quest_act_obj_levels.level`).
    pub fn obj_level_act_level(&self, act_id: u32) -> anyhow::Result<Option<u32>> {
        let row = self
            .conn
            .query_row(
                "SELECT l.level FROM quest_acts qa \
                 JOIN quest_act_obj_levels l ON l.id = qa.act_detail_id \
                 WHERE qa.id = ?1 AND qa.act_detail_type = 'QuestActObjLevel'",
                [act_id],
                |r| r.get::<_, Option<u32>>(0),
            )
            .optional()?
            .flatten();
        Ok(row)
    }

    /// `skill_reagents` of a skill: `(item_id, amount)` in data order. `first_reagent_only`
    /// decides whether one or every reagent is required (`skills/handlers.rs` validation).
    pub fn skill_reagents(&self, skill_id: u32) -> anyhow::Result<Vec<(u32, i32)>> {
        let mut stmt = self.conn.prepare(
            "SELECT item_id, amount FROM skill_reagents WHERE skill_id = ?1 ORDER BY id",
        )?;
        let mut out = Vec::new();
        for row in stmt.query_map([skill_id], |r| {
            Ok((r.get::<_, u32>(0)?, r.get::<_, i32>(1)?))
        })? {
            out.push(row?);
        }
        Ok(out)
    }

    /// `skills.first_reagent_only` (`true`: any one reagent satisfies the cast).
    pub fn skill_first_reagent_only(&self, skill_id: u32) -> anyhow::Result<bool> {
        let row = self
            .conn
            .query_row(
                "SELECT first_reagent_only FROM skills WHERE id = ?1",
                [skill_id],
                |r| {
                    Ok(r.get::<_, Option<String>>(0)?
                        .is_some_and(|v| v.eq_ignore_ascii_case("t") || v == "1"))
                },
            )
            .optional()?;
        Ok(row.unwrap_or(false))
    }

    /// The doodad's phase groups whose funcs grant `item_id` (`DoodadFuncLootItem` directly or a
    /// `DoodadFuncLootPack` whose pack contains it).
    pub fn doodad_loot_phases(
        &self,
        doodad_template: u32,
        item_id: u32,
    ) -> anyhow::Result<Vec<u32>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT g.id FROM doodad_func_groups g \
             JOIN doodad_funcs df ON df.doodad_func_group_id = g.id \
             WHERE g.doodad_almighty_id = ?1 AND ( \
                 (df.actual_func_type = 'DoodadFuncLootItem' AND df.actual_func_id IN ( \
                     SELECT id FROM doodad_func_loot_items WHERE item_id = ?2)) \
                 OR (df.actual_func_type = 'DoodadFuncLootPack' AND df.actual_func_id IN ( \
                     SELECT lp.id FROM doodad_func_loot_packs lp \
                     JOIN loots l ON l.loot_pack_id = lp.loot_pack_id \
                     WHERE l.item_id = ?2))) \
             ORDER BY g.id",
        )?;
        let mut out = Vec::new();
        for row in stmt.query_map(rusqlite::params![doodad_template, item_id], |r| {
            r.get::<_, u32>(0)
        })? {
            out.push(row?);
        }
        Ok(out)
    }

    /// The phase graph of a doodad template: `(Start-kind group ids, (from, next_phase) edges)`.
    /// A spawned doodad starts in a Start-kind group (`npcs/doodad/phase.rs::init_doodad`); the
    /// loot phase is reachable only when an edge chain leads to it.
    #[allow(clippy::type_complexity)]
    pub fn doodad_phase_graph(
        &self,
        doodad_template: u32,
    ) -> anyhow::Result<(Vec<u32>, Vec<(u32, u32)>)> {
        let start_groups: Vec<u32> = {
            let mut stmt = self.conn.prepare(
                "SELECT id FROM doodad_func_groups \
                 WHERE doodad_almighty_id = ?1 AND doodad_func_group_kind_id = 1 ORDER BY id",
            )?;
            let rows = stmt
                .query_map([doodad_template], |r| r.get::<_, u32>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        let mut edges = Vec::new();
        {
            let mut stmt = self.conn.prepare(
                "SELECT df.doodad_func_group_id, df.next_phase \
                 FROM doodad_funcs df \
                 JOIN doodad_func_groups g ON g.id = df.doodad_func_group_id \
                 WHERE g.doodad_almighty_id = ?1 AND df.next_phase > 0 \
                 ORDER BY df.id",
            )?;
            for row in stmt.query_map([doodad_template], |r| {
                Ok((r.get::<_, u32>(0)?, r.get::<_, i32>(1)?))
            })? {
                let (from, to) = row?;
                edges.push((from, to as u32));
            }
        }
        Ok((start_groups, edges))
    }

    /// The highest drop rate of `item_id` in the loot packs the doodad template's
    /// `DoodadFuncLootPack` funcs grant (for the "effectively zero drop rate" data gap).
    pub fn doodad_pack_drop_chance(
        &self,
        item_id: u32,
        doodad_template: u32,
    ) -> anyhow::Result<Option<i64>> {
        let row = self
            .conn
            .query_row(
                "SELECT MAX(l.drop_rate) FROM doodad_funcs df \
                 JOIN doodad_func_groups g ON g.id = df.doodad_func_group_id \
                 JOIN doodad_func_loot_packs lp ON df.actual_func_type = 'DoodadFuncLootPack' \
                     AND df.actual_func_id = lp.id \
                 JOIN loots l ON l.loot_pack_id = lp.loot_pack_id AND l.item_id = ?1 \
                 WHERE g.doodad_almighty_id = ?2",
                rusqlite::params![item_id, doodad_template],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten();
        Ok(row)
    }

    /// `quest_contexts.repeatable`; `None` when the quest has no row. A completed non-repeatable
    /// quest can only answer `QuestDailyLimit` (`engine.rs::add_quest`).
    pub fn quest_repeatable(&self, quest_id: u32) -> anyhow::Result<Option<bool>> {
        let row = self
            .conn
            .query_row(
                "SELECT repeatable FROM quest_contexts WHERE id = ?1",
                [quest_id],
                |r| {
                    Ok(r.get::<_, Option<String>>(0)?
                        .is_some_and(|v| v.eq_ignore_ascii_case("t") || v == "1"))
                },
            )
            .optional()?;
        Ok(row)
    }

    /// The skills that drive the current phase of a doodad: the funcs' own `func_skill_id`, the
    /// `DoodadFuncFakeUse.fake_skill_id` and `DoodadFuncUse.skill_id` of the func group, in func
    /// order (`npcs/doodad/data.rs::func_for_skill` matches the client's skill against exactly
    /// these). A func with no skill (a loot func) is driven once the phase reaches it.
    pub fn doodad_func_skills(
        &self,
        doodad_template: u32,
        func_group_id: u32,
    ) -> anyhow::Result<Vec<u32>> {
        let mut stmt = self.conn.prepare(
            "SELECT x.skill FROM ( \
                 SELECT df.func_skill_id AS skill, df.id AS fid, df.next_phase AS nxt \
                 FROM doodad_funcs df \
                 WHERE df.doodad_func_group_id = ?2 AND df.func_skill_id > 0 \
                 UNION \
                 SELECT fu.fake_skill_id AS skill, df.id AS fid, df.next_phase AS nxt \
                 FROM doodad_funcs df \
                 JOIN doodad_func_fake_uses fu ON df.actual_func_type = 'DoodadFuncFakeUse' \
                     AND fu.id = df.actual_func_id \
                 WHERE df.doodad_func_group_id = ?2 AND fu.fake_skill_id > 0 \
                 UNION \
                 SELECT du.skill_id AS skill, df.id AS fid, df.next_phase AS nxt \
                 FROM doodad_funcs df \
                 JOIN doodad_func_uses du ON df.actual_func_type = 'DoodadFuncUse' \
                     AND du.id = df.actual_func_id \
                 WHERE df.doodad_func_group_id = ?2 AND du.skill_id > 0 \
             ) x \
             JOIN doodad_func_groups g ON g.id = ?2 AND g.doodad_almighty_id = ?1 \
             JOIN skills s ON s.id = x.skill \
             ORDER BY EXISTS(SELECT 1 FROM doodad_funcs nf \
                             WHERE nf.doodad_func_group_id = x.nxt) DESC, x.fid",
        )?;
        let rows = stmt.query_map(rusqlite::params![doodad_template, func_group_id], |r| {
            r.get::<_, u32>(0)
        })?;
        let mut out = Vec::new();
        for row in rows {
            let skill = row?;
            if !out.contains(&skill) {
                out.push(skill);
            }
        }
        Ok(out)
    }

    /// `npcs.merchant` and the last `merchants.merchant_pack_id` of the NPC (the server's
    /// `shop_info` requires the template flag; `npcs/bridges.rs::shop_info`). `None` when the
    /// NPC has no row.
    pub fn npc_merchant(&self, npc_id: u32) -> anyhow::Result<Option<(bool, u32)>> {
        let row = self
            .conn
            .query_row(
                "SELECT merchant, (SELECT merchant_pack_id FROM merchants m \
                     WHERE m.npc_id = n.id ORDER BY rowid DESC LIMIT 1) \
                 FROM npcs n WHERE id = ?1",
                [npc_id],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?
                            .is_some_and(|v| v.eq_ignore_ascii_case("t") || v == "1"),
                        r.get::<_, Option<u32>>(1)?.unwrap_or(0),
                    ))
                },
            )
            .optional()?;
        Ok(row)
    }

    /// The default zone key of a plan world name, from the `world_spawns.json` next to the
    /// database (`Data/Worlds/world_spawns.json`; the game's world spawn list). `None` when the
    /// file or the world is missing.
    pub fn world_zone_key(&self, world: &str) -> anyhow::Result<Option<u32>> {
        if self.worlds.get().is_none() {
            let loaded = load_world_spawns(&self.path)?;
            let _ = self.worlds.set(loaded);
        }
        Ok(self
            .worlds
            .get()
            .and_then(|worlds| worlds.get(&world.to_lowercase()))
            .copied())
    }

    /// The doodad templates whose funcs enter the zone of `zone_key`
    /// (`DoodadFuncEnterSysInstance` / `DoodadFuncEnterInstance`, resolved through
    /// `doodad_funcs` -> `doodad_func_groups` -> `doodad_almighties`). Deterministic order
    /// (func row, then template), deduplicated.
    pub fn world_entrance_doodads(&self, zone_key: u32) -> anyhow::Result<Vec<u32>> {
        let zone_id: Option<u32> = self
            .conn
            .query_row(
                "SELECT id FROM zones WHERE zone_key = ?1 LIMIT 1",
                [zone_key],
                |r| r.get(0),
            )
            .optional()?;
        let Some(zone_id) = zone_id else {
            return Ok(Vec::new());
        };
        let mut out: Vec<u32> = Vec::new();
        for func_type in ["DoodadFuncEnterSysInstance", "DoodadFuncEnterInstance"] {
            let table = if func_type == "DoodadFuncEnterSysInstance" {
                "doodad_func_enter_sys_instances"
            } else {
                "doodad_func_enter_instances"
            };
            let sql = format!(
                "SELECT dfa.id \
                 FROM {table} e \
                 JOIN doodad_funcs df ON df.actual_func_id = e.id \
                     AND df.actual_func_type = '{func_type}' \
                 JOIN doodad_func_groups g ON g.id = df.doodad_func_group_id \
                 JOIN doodad_almighties dfa ON dfa.id = g.doodad_almighty_id \
                 WHERE e.zone_id = ?1 \
                 ORDER BY df.id, dfa.id"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map([zone_id], |r| r.get::<_, u32>(0))?;
            for row in rows {
                let template = row?;
                if !out.contains(&template) {
                    out.push(template);
                }
            }
        }
        Ok(out)
    }

    /// A doodad template with an `DoodadFuncExitIndun` func that returns to the saved main
    /// world position (`return_point_id` 0 / null; `funcs/world.rs::exit_indun`). The runner
    /// spawns it inside an instance to leave. Templates whose exit func lives in a `Start`-kind
    /// group (and carries its own skill) come first: the spawn starts in that group, so a single
    /// use leaves; the progress-group variants hide the exit func behind `DoodadFuncSkillHit`
    /// phases the runner cannot drive (template 4139).
    pub fn exit_doodad(&self) -> anyhow::Result<Option<u32>> {
        let query = |start_kind_only: bool| {
            let kind_filter = if start_kind_only {
                "AND g.doodad_func_group_kind_id = 1 AND df.func_skill_id > 0"
            } else {
                ""
            };
            format!(
                "SELECT dfa.id \
                 FROM doodad_func_exit_induns e \
                 JOIN doodad_funcs df ON df.actual_func_type = 'DoodadFuncExitIndun' \
                     AND df.actual_func_id = e.id \
                 JOIN doodad_func_groups g ON g.id = df.doodad_func_group_id \
                 JOIN doodad_almighties dfa ON dfa.id = g.doodad_almighty_id \
                 WHERE (e.return_point_id IS NULL OR e.return_point_id = 0) {kind_filter} \
                 ORDER BY df.id, dfa.id LIMIT 1"
            )
        };
        for start_kind_only in [true, false] {
            let row = self
                .conn
                .query_row(&query(start_kind_only), [], |r| r.get::<_, u32>(0))
                .optional()?;
            if row.is_some() {
                return Ok(row);
            }
        }
        Ok(None)
    }
}

/// `Data/Worlds/world_spawns.json` next to the database: `[{"Name", "SpawnPosition":
/// {"ZoneId", ...}}]` -> lower-case name to zone key.
fn load_world_spawns(db_path: &Path) -> anyhow::Result<HashMap<String, u32>> {
    let Some(data_dir) = db_path.parent() else {
        return Ok(HashMap::new());
    };
    let path = data_dir.join("Worlds").join("world_spawns.json");
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading world spawns {}", path.display()))?;
    // The shipped file has a BOM and `//` comments; strip them like the server's loader does.
    let text = aaemu_game::data::world_templates::strip_json_extensions(
        text.trim_start_matches('\u{feff}'),
    );
    let value: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parsing world spawns {}", path.display()))?;
    let mut out = HashMap::new();
    if let Some(list) = value.as_array() {
        for entry in list {
            let (Some(name), Some(zone)) = (
                entry.get("Name").and_then(|v| v.as_str()),
                entry
                    .get("SpawnPosition")
                    .and_then(|s| s.get("ZoneId"))
                    .and_then(|v| v.as_u64()),
            ) else {
                continue;
            };
            out.insert(name.to_lowercase(), zone as u32);
        }
    }
    Ok(out)
}

use rusqlite::OptionalExtension as _;

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped `world_spawns.json` starts with a BOM and carries `//` comments.
    #[test]
    fn world_spawns_accept_bom_and_comments() {
        let dir = std::env::temp_dir().join(format!("aaemu-ws-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Worlds")).unwrap();
        std::fs::write(
            dir.join("Worlds").join("world_spawns.json"),
            "\u{feff}[\n  {\n    \"Name\": \"instance_library_3\", // Screaming Archives\n    \"SpawnPosition\": { \"ZoneId\": 298 }\n  }\n]\n",
        )
        .unwrap();
        let spawns = load_world_spawns(&dir.join("compact.sqlite3")).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(spawns.get("instance_library_3"), Some(&298));
    }

    /// Runs only on a machine with the game data; CI does not ship `compact.sqlite3`.
    #[test]
    fn lookups_work_when_the_game_data_is_present() {
        let path = Path::new(DEFAULT_COMPACT_DB);
        if !path.exists() {
            return;
        }
        let db = CompactDb::open(path).unwrap();
        // The interaction skill table is non-empty in 1.2 data.
        assert!(db.interaction_use_skill().unwrap().is_some());
        // An unknown item has no use skill.
        assert_eq!(db.item_use_skill(0).unwrap(), ItemUseSkill::None);
        // The first `quest_act_obj_item_uses` row has an item; its skill may still be 0.
        let item_uses = db
            .conn
            .prepare("SELECT item_id FROM quest_act_obj_item_uses LIMIT 20")
            .unwrap()
            .query_map([], |r| r.get::<_, u32>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(!item_uses.is_empty());
        // Quest 1394's item 14177 references `use_skill_id` 12043, which has no `skills` row in
        // the 1.2 data: the server drops the cast (`skills/handlers.rs` `template_skill`).
        assert_eq!(
            db.item_use_skill(14_177).unwrap(),
            ItemUseSkill::MissingSkill { skill_id: 12_043 }
        );
        // A known item/skill pair resolves with its target type.
        assert!(matches!(
            db.item_use_skill(5145).unwrap(),
            ItemUseSkill::Known {
                skill_id: 13416,
                ..
            }
        ));
        // Race faction mothers: Nuian 101 -> 148 (Nuia), Harani 109 -> 149 (Haranya). Quest
        // 2302 requires `mother_faction` 149, so the runner can decide it before the sphere.
        assert_eq!(db.mother_faction(101).unwrap(), Some(148));
        assert_eq!(db.mother_faction(109).unwrap(), Some(149));
        assert_eq!(db.mother_faction(0).unwrap(), None);
        // The worlds next to the database map a plan world name to its default zone key; the
        // entrance doodads of that zone are the ones the runner can spawn to enter
        // (arche_mall_world -> zone 260 -> template 4894, the Mirage Isle portal).
        assert_eq!(db.world_zone_key("arche_mall_world").unwrap(), Some(260));
        assert_eq!(db.world_zone_key("instance_library").unwrap(), Some(296));
        assert_eq!(db.world_zone_key("no_such_world").unwrap(), None);
        assert!(db.world_entrance_doodads(260).unwrap().contains(&4894));
        assert!(!db.world_entrance_doodads(296).unwrap().is_empty());
        assert!(db.exit_doodad().unwrap().is_some());
        // The picked exit doodad is directly usable (Start-kind group with its own skill), not
        // template 4139 whose exit func hides behind DoodadFuncSkillHit phases.
        assert_ne!(db.exit_doodad().unwrap(), Some(4139));
    }

    /// The v9 item-source reverse lookups (plan.py's tables, read here for
    /// `unknown_item_source` targets). Gated like the other game-data test.
    #[test]
    fn item_source_lookups_work_when_the_game_data_is_present() {
        let path = Path::new(DEFAULT_COMPACT_DB);
        if !path.exists() {
            return;
        }
        let db = CompactDb::open(path).unwrap();
        // Item 1466 drops from the DoodadFuncLootItem doodad template 1590.
        let doodads = db.item_doodad_sources(1466).unwrap();
        assert!(doodads.iter().any(|source| {
            source.func == "DoodadFuncLootItem" && source.doodad_ids.contains(&1590)
        }));
        // Item 8018 is in the loot pack 6459 (loot pack dropping NPCs may be empty).
        let loot = db.item_loot_sources(8018).unwrap();
        assert!(loot.iter().any(|source| source.loot_pack_id == 6459));
        // Item 27809 is a `skill_products` product of the container 27808's skill 22045.
        let containers = db.item_container_sources(27809).unwrap();
        assert!(containers.iter().any(|source| {
            source.container_item_id == 27808
                && source.skill_id == 22045
                && source.grant.as_deref() == Some("skill_product")
        }));
        // Item 27669 is a craft product (the doll bundle recipes).
        assert!(!db.item_craft_ids(27669).unwrap().is_empty());
        // Quest 4863's `QuestActObjAggro` is given by the engage-combat NPC 4240.
        assert_eq!(db.engage_quest_npc(4863).unwrap(), Some(4240));
        // Merchant pack 119 sells item 21851 through npc 1176.
        assert!(db
            .item_store_sources(21851)
            .unwrap()
            .iter()
            .any(|source| { source.pack_ids.contains(&119) && source.npc_ids.contains(&1176) }));
        // The pumpkin loot pack of doodad 2266 drops item 8018 at 1/10,000,000 (the effectively
        // zero rate that makes the GM item fallback the only practical source).
        assert_eq!(db.doodad_pack_drop_chance(8018, 2266).unwrap(), Some(1));
        // npc 1196 is listed in `merchants` but is not a merchant in its template.
        assert_eq!(db.npc_merchant(1196).unwrap(), Some((false, 266)));
        assert!(db.npc_merchant(1061).unwrap().is_some_and(|(m, _)| m));
        // Repeatable flags (quest 18 is not repeatable, 2107 is).
        assert_eq!(db.quest_repeatable(18).unwrap(), Some(false));
        assert_eq!(db.quest_repeatable(2107).unwrap(), Some(true));
        assert_eq!(db.quest_repeatable(0).unwrap(), None);
        // The Mirage Isle entrance doodad 4894 is driven by skill 17829 (its func has a skill;
        // the generic Use skill matches nothing there); the doll pouch's fake-use phase has its
        // own two skills before the loot func.
        assert_eq!(db.doodad_func_skills(4894, 12204).unwrap(), vec![17829]);
        // The fake-use whose next phase has funcs (13589 -> 5432, the loot) comes before the
        // one leading to 5433, a phase with no funcs at all.
        assert_eq!(
            db.doodad_func_skills(2552, 5431).unwrap(),
            vec![13589, 13593]
        );
        assert_eq!(
            db.doodad_func_skills(2552, 5432).unwrap(),
            Vec::<u32>::new()
        );
        // The Mirage Isle portal's Start group drives with skill 17829; doodad 4139's Start
        // group (SkillHit funcs) carries none, so its exit func hidden in the Progress group is
        // not reachable from a fresh spawn.
        assert_eq!(db.doodad_start_skills(4894).unwrap(), vec![17829]);
        assert!(db.doodad_start_skills(4139).unwrap().is_empty());
        // The instance_library entrance skill 24354 requires faction 109/112/113: a 109 caster
        // is blocked, 113 is allowed.
        assert!(db
            .skill_character_block(24354, Some(4), Some(109), None)
            .unwrap()
            .is_some());
        assert!(db
            .skill_character_block(24354, Some(4), Some(113), None)
            .unwrap()
            .is_none());
        // Quest 5307/5313: skill 22045 needs all nine dolls (first_reagent_only = false).
        assert_eq!(db.skill_reagents(22045).unwrap().len(), 9);
        assert!(!db.skill_first_reagent_only(22045).unwrap());
        // Doodad 4594's harvest phase 11308 grants item 21850 but no Start-kind phase leads to
        // it (the 1.2 growth link is missing): a data gap, not a suspect.
        assert_eq!(db.doodad_loot_phases(4594, 21850).unwrap(), vec![11308]);
        let (starts, edges) = db.doodad_phase_graph(4594).unwrap();
        assert!(starts.is_empty());
        assert!(edges.contains(&(11307, 11308)));
    }

    /// The GM recall fallback of the quest runner (`questrun.rs::GM_RETURN_SKILLS`) uses 1.2
    /// skills whose `Return` special resolves the character's own return point
    /// (`special_effect_type_id` 25, `value1` 0) and that carry no `unit_reqs`; gated like the
    /// other game-data tests.
    #[test]
    fn main_world_return_skills_are_recall_specials() {
        let path = Path::new(DEFAULT_COMPACT_DB);
        if !path.exists() {
            return;
        }
        let db = CompactDb::open(path).unwrap();
        for skill_id in [11_239u32, 11_572] {
            let returns: i64 = db
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM skill_effects se \
                     JOIN effects ef ON ef.id = se.effect_id AND ef.actual_type = 'SpecialEffect' \
                     JOIN special_effects sp ON sp.id = ef.actual_id \
                     WHERE se.skill_id = ?1 AND sp.special_effect_type_id = 25 AND sp.value1 = 0",
                    [skill_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                returns, 1,
                "skill {skill_id} is not a value1=0 Return special"
            );
            let reqs: i64 = db
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM unit_reqs WHERE owner_type = 'Skill' AND owner_id = ?1",
                    [skill_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(reqs, 0, "skill {skill_id} has unit_reqs");
        }
    }
}
