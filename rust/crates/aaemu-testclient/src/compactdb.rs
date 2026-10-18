//! Read-only lookups from the same `compact.sqlite3` the game server loads. A real 1.2 client
//! has this data locally (item templates, interaction skills); the headless bot reads the same
//! database for the two decisions that are not on the wire:
//!
//! * `items.use_skill_id` + the skill's `target_type_id` for `CSStartSkillPacket` item use
//!   (`systems/skills/handlers.rs` requires `skill_id == use_skill_id`);
//! * the skill with an `InteractionEffect` of `WorldInteractionType.Use` (19) that casts on a
//!   doodad (`systems/npcs/bridges.rs::interaction_effect`, `doodads::use_doodad`).

use std::path::Path;

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
}

impl CompactDb {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn =
            rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .with_context(|| format!("opening compact database {}", path.display()))?;
        Ok(Self { conn })
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
}

use rusqlite::OptionalExtension as _;

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
