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
    /// spawns it inside an instance to leave.
    pub fn exit_doodad(&self) -> anyhow::Result<Option<u32>> {
        let row = self
            .conn
            .query_row(
                "SELECT dfa.id \
                 FROM doodad_func_exit_induns e \
                 JOIN doodad_funcs df ON df.actual_func_id = e.id \
                     AND df.actual_func_type = 'DoodadFuncExitIndun' \
                 JOIN doodad_func_groups g ON g.id = df.doodad_func_group_id \
                 JOIN doodad_almighties dfa ON dfa.id = g.doodad_almighty_id \
                 WHERE e.return_point_id IS NULL OR e.return_point_id = 0 \
                 ORDER BY df.id, dfa.id LIMIT 1",
                [],
                |r| r.get::<_, u32>(0),
            )
            .optional()?;
        Ok(row)
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
    }
}
