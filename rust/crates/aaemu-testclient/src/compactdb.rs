//! Read-only lookups from the same `compact.sqlite3` the game server loads. A real 1.2 client
//! has this data locally (item templates, interaction skills); the headless bot reads the same
//! database for the two decisions that are not on the wire:
//!
//! * `items.use_skill_id` + the skill's `target_type_id` for `CSStartSkillPacket` item use
//!   (`systems/skills/handlers.rs` requires `skill_id == use_skill_id`);
//! * the skill with an `InteractionEffect` of `WorldInteractionType.Use` (19) that casts on a
//!   doodad (`systems/npcs/bridges.rs::interaction_effect`, `doodads::use_doodad`).

use std::cell::OnceCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context;

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
}
