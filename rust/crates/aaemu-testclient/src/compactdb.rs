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

    /// `items.use_skill_id` and that skill's `target_type_id` (`SkillTargetType`), if the item
    /// has a use skill. `None` when the item is unknown or not usable.
    pub fn item_use_skill(&self, item_id: u32) -> anyhow::Result<Option<(u32, u8)>> {
        let row = self
            .conn
            .query_row(
                "SELECT s.id, s.target_type_id \
                 FROM items i JOIN skills s ON s.id = i.use_skill_id \
                 WHERE i.id = ?1 AND i.use_skill_id > 0",
                [item_id],
                |r| Ok((r.get::<_, u32>(0)?, r.get::<_, Option<u8>>(1)?.unwrap_or(0))),
            )
            .optional()?;
        Ok(row)
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
        assert!(db.item_use_skill(0).unwrap().is_none());
    }
}
