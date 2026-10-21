//! Regression test for `SQL/patches/compact/2026-10-21-gear-3.0-batch-1a.sql`: the patch
//! imports the retail 3.0 gear batch 1a (288 costumes/armor/cloaks below the Eanad power
//! line, plus the material items of their crafts), re-keys the 3.0 armor looks above the
//! 1.2 range, remaps their `item_assets` paths, adds the item category / tags / mod sets /
//! icons / texts the rows need, and leaves the custom 8M band and the Dawnsdrop sets out.
//!
//! The test executes the real patch file (`include_str!`) twice on a copy of the database
//! (the second run must change nothing), then loads the patched copy with the server's own
//! item loader. `armor_assets` / `item_armor_assets` / `item_assets` are not read by the
//! server (the C# and Rust item loaders read `item_armors`), so the look chain is checked
//! directly for the client / Bevy reader.
//!
//! `AAEMU_COMPACT_DB=rust/crates/aaemu-game/app/Data/compact.sqlite3 \
//!  cargo test -p aaemu-game gear_1a_patch -- --ignored`

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

use super::ItemData;
use crate::data::items::TemplateKind;

/// The actual compact-data patch.
const PATCH_SQL: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../SQL/patches/compact/2026-10-21-gear-3.0-batch-1a.sql"
));

/// Selected gear items (the three samples: a cloak, an underwear costume and a leather
/// armor piece) plus the 295 rows the patch inserts (288 gear + 7 craft materials).
const SAMPLE_CLOAK: u32 = 3006;
const SAMPLE_COSTUME: u32 = 34303;
const SAMPLE_ARMOR: u32 = 35272;
const INSERTED_ITEMS: i64 = 295;
/// 288 gear + 7 materials, minus the 8 items whose orphan `item_armors` row already made
/// a template in the 1.2 loader (the patch updates those rows instead of inserting).
const INSERTED_NEW_TEMPLATES: usize = 287;
const INSERTED_ARMOR_ROWS: i64 = 280;
const INSERTED_CATEGORY_173_ITEMS: i64 = 55;
const INSERTED_LOOKS: i64 = 118;
const INSERTED_LOOK_PATHS: i64 = 1419;
const INSERTED_ITEM_ASSETS: i64 = 465;
const INSERTED_ICONS: i64 = 31;
const INSERTED_CRAFTS: i64 = 19;
const INSERTED_CRAFT_PRODUCTS: i64 = 19;
const INSERTED_CRAFT_MATERIALS: i64 = 55;
const INSERTED_TEXTS: i64 = 865;
const INSERTED_TAGS: i64 = 2;

/// A private copy of the real database, removed when the test ends (also on panic).
struct TempCopy(PathBuf);

impl TempCopy {
    fn new(source: &Path) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = std::env::temp_dir().join(format!(
            "aaemu-gear-1a-patch-{}-{nanos}.sqlite3",
            std::process::id()
        ));
        std::fs::copy(source, &path).expect("copy compact.sqlite3");
        Self(path)
    }

    fn open(&self) -> Connection {
        Connection::open(&self.0).expect("open the patched copy")
    }
}

impl Drop for TempCopy {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Counts {
    items: i64,
    item_armors: i64,
    armor_assets: i64,
    item_armor_assets: i64,
    item_assets: i64,
    icons: i64,
    item_categories: i64,
    tags: i64,
    buffs: i64,
    crafts: i64,
    craft_products: i64,
    craft_materials: i64,
    localized_texts: i64,
    dyeable_items: i64,
    equip_item_attr_modifiers: i64,
    equip_item_sets: i64,
    equip_item_set_bonuses: i64,
    item_proc_bindings: i64,
    skills: i64,
    skill_effects: i64,
    effects: i64,
    craft_effects: i64,
    buff_effects: i64,
    tagged_buffs: i64,
    skill_modifiers: i64,
    unit_modifiers: i64,
}

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM \"{table}\""), [], |r| {
        r.get(0)
    })
    .unwrap_or_else(|e| panic!("count {table}: {e}"))
}

fn counts(conn: &Connection) -> Counts {
    Counts {
        items: count(conn, "items"),
        item_armors: count(conn, "item_armors"),
        armor_assets: count(conn, "armor_assets"),
        item_armor_assets: count(conn, "item_armor_assets"),
        item_assets: count(conn, "item_assets"),
        icons: count(conn, "icons"),
        item_categories: count(conn, "item_categories"),
        tags: count(conn, "tags"),
        buffs: count(conn, "buffs"),
        crafts: count(conn, "crafts"),
        craft_products: count(conn, "craft_products"),
        craft_materials: count(conn, "craft_materials"),
        localized_texts: count(conn, "localized_texts"),
        dyeable_items: count(conn, "dyeable_items"),
        equip_item_attr_modifiers: count(conn, "equip_item_attr_modifiers"),
        equip_item_sets: count(conn, "equip_item_sets"),
        equip_item_set_bonuses: count(conn, "equip_item_set_bonuses"),
        item_proc_bindings: count(conn, "item_proc_bindings"),
        skills: count(conn, "skills"),
        skill_effects: count(conn, "skill_effects"),
        effects: count(conn, "effects"),
        craft_effects: count(conn, "craft_effects"),
        buff_effects: count(conn, "buff_effects"),
        tagged_buffs: count(conn, "tagged_buffs"),
        skill_modifiers: count(conn, "skill_modifiers"),
        unit_modifiers: count(conn, "unit_modifiers"),
    }
}

fn row_i64(conn: &Connection, sql: &str, params: &[i64]) -> i64 {
    conn.query_row(sql, rusqlite::params_from_iter(params), |r| r.get(0))
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn leaf_paths(conn: &Connection, item_id: u32) -> Vec<String> {
    let mut stmt = conn
        .prepare(concat!(
            "SELECT ia.path FROM item_armors a",
            " JOIN item_armor_assets iaa ON iaa.armor_asset_id = a.asset_id",
            " JOIN item_assets ia ON ia.id = iaa.asset_id",
            " WHERE a.item_id = ?1 ORDER BY iaa.id",
        ))
        .expect("look query");
    stmt.query_map([item_id], |r| r.get::<_, String>(0))
        .expect("look rows")
        .map(|r| r.expect("look path"))
        .collect()
}

#[test]
#[ignore = "needs AAEMU_COMPACT_DB"]
fn gear_1a_patch_applies_to_a_copy_and_loads() {
    let Ok(source) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let copy = TempCopy::new(Path::new(&source));
    let conn = copy.open();
    // `cargo xtask compact-patches` turns foreign keys off for the file transaction.
    conn.execute_batch("PRAGMA foreign_keys = OFF;")
        .expect("foreign keys off as in the applier");

    let before = counts(&conn);
    let before_retail_armor_max = row_i64(
        &conn,
        "SELECT COALESCE(MAX(id), 0) FROM armor_assets WHERE id < 8000000",
        &[],
    );
    let before_custom_items = row_i64(&conn, "SELECT COUNT(*) FROM items WHERE id >= 8000000", &[]);
    let before_custom_set_rows = row_i64(
        &conn,
        "SELECT COUNT(*) FROM item_armors WHERE eiset_id >= 9000001",
        &[],
    );
    let before_data = ItemData::load(&conn).expect("load pristine items");
    let before_templates = before_data.templates.len();
    let before_sets = before_data.equip_item_sets.len();

    conn.execute_batch(PATCH_SQL).expect("patch");
    let after_first = counts(&conn);
    // The guarded inserts and the value-for-value UPDATE make a second run a no-op.
    conn.execute_batch(PATCH_SQL).expect("patch rerun");
    let after = counts(&conn);
    assert_eq!(after_first, after, "second patch run changed the data");

    assert_eq!(after.items - before.items, INSERTED_ITEMS);
    assert_eq!(
        after.item_armors - before.item_armors,
        INSERTED_ARMOR_ROWS,
        "288 gear items minus 8 replaced orphan rows; the 7 materials have no class row"
    );
    assert_eq!(after.armor_assets - before.armor_assets, INSERTED_LOOKS);
    assert_eq!(
        after.item_armor_assets - before.item_armor_assets,
        INSERTED_LOOK_PATHS
    );
    assert_eq!(after.item_assets - before.item_assets, INSERTED_ITEM_ASSETS);
    assert_eq!(after.icons - before.icons, INSERTED_ICONS);
    assert_eq!(after.item_categories - before.item_categories, 1);
    assert_eq!(after.tags - before.tags, INSERTED_TAGS);
    assert_eq!(after.buffs - before.buffs, 11);
    assert_eq!(after.crafts - before.crafts, INSERTED_CRAFTS);
    assert_eq!(
        after.craft_products - before.craft_products,
        INSERTED_CRAFT_PRODUCTS
    );
    assert_eq!(
        after.craft_materials - before.craft_materials,
        INSERTED_CRAFT_MATERIALS
    );
    assert_eq!(
        after.localized_texts - before.localized_texts,
        INSERTED_TEXTS
    );
    assert_eq!(after.dyeable_items - before.dyeable_items, 25);
    assert_eq!(
        after.equip_item_attr_modifiers - before.equip_item_attr_modifiers,
        3
    );
    assert_eq!(after.equip_item_sets, before.equip_item_sets);
    assert_eq!(after.equip_item_set_bonuses, before.equip_item_set_bonuses);
    assert_eq!(after.item_proc_bindings, before.item_proc_bindings);
    assert_eq!(after.skills - before.skills, 3);
    assert_eq!(after.skill_effects - before.skill_effects, 5);
    assert_eq!(after.effects - before.effects, 5);
    assert_eq!(after.craft_effects - before.craft_effects, 3);
    assert_eq!(after.buff_effects - before.buff_effects, 2);
    assert_eq!(after.tagged_buffs - before.tagged_buffs, 20);
    assert_eq!(after.skill_modifiers - before.skill_modifiers, 11);
    assert_eq!(after.unit_modifiers - before.unit_modifiers, 7);

    // The custom bands are untouched: no 8M items, no Dawnsdrop set row, no dangling
    // custom tag from the 3.0-only emulator buffs.
    assert_eq!(
        row_i64(&conn, "SELECT COUNT(*) FROM items WHERE id >= 8000000", &[]),
        before_custom_items
    );
    assert_eq!(
        row_i64(
            &conn,
            "SELECT COUNT(*) FROM item_armors WHERE eiset_id >= 9000001",
            &[]
        ),
        before_custom_set_rows
    );
    assert_eq!(
        row_i64(
            &conn,
            "SELECT COUNT(*) FROM equip_item_sets WHERE id >= 9000001",
            &[]
        ),
        0
    );
    assert_eq!(
        row_i64(
            &conn,
            "SELECT COUNT(*) FROM tagged_buffs WHERE tag_id = 9000007",
            &[]
        ),
        0
    );
    assert_eq!(
        row_i64(
            &conn,
            "SELECT COUNT(*) FROM tags WHERE id IN (1626, 2480)",
            &[]
        ),
        2
    );
    assert_eq!(
        row_i64(
            &conn,
            "SELECT COUNT(*) FROM items WHERE category_id = 173",
            &[]
        ),
        INSERTED_CATEGORY_173_ITEMS
    );

    // Every imported look id is above the 1.2 range (but below the custom band), and the
    // chain resolves: the client/Bevy reader needs both rows. Sample pieces: the cloak and
    // underwear orphan rows are replacements; the leather piece is a plain insert.
    for item_id in [SAMPLE_CLOAK, SAMPLE_COSTUME, SAMPLE_ARMOR] {
        let (asset_id, slot): (i64, i64) = conn
            .query_row(
                "SELECT asset_id, slot_type_id FROM item_armors WHERE item_id = ?1",
                [item_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("sample armor row");
        assert!(
            asset_id > before_retail_armor_max && asset_id < 8_000_000,
            "item {item_id} look {asset_id} is re-keyed above the 1.2 range"
        );
        let chain: i64 = row_i64(
            &conn,
            concat!(
                "SELECT COUNT(*) FROM item_armor_assets iaa",
                " JOIN item_assets ia ON ia.id = iaa.asset_id",
                " WHERE iaa.armor_asset_id = ?1",
            ),
            &[asset_id],
        );
        assert!(chain > 0, "item {item_id} look chain is not empty");
        let paths = leaf_paths(&conn, item_id);
        for race in ["nuian", "elf", "hariharan", "ferre"] {
            assert!(
                paths
                    .iter()
                    .any(|path| path.contains(&format!("/characters/{race}/"))),
                "item {item_id} has a {race} look path"
            );
        }
        assert_eq!(
            row_i64(
                &conn,
                concat!(
                    "SELECT COUNT(*) FROM item_armor_assets iaa",
                    " LEFT JOIN item_assets ia ON ia.id = iaa.asset_id",
                    " WHERE iaa.armor_asset_id = ?1 AND ia.id IS NULL",
                ),
                &[asset_id],
            ),
            0,
            "item {item_id} look chain has no dangling item_assets row"
        );
        let expected_slot = match item_id {
            SAMPLE_CLOAK => 9,
            SAMPLE_COSTUME => 13,
            _ => 1,
        };
        assert_eq!(slot, expected_slot);
    }
    for item_id in [SAMPLE_CLOAK, SAMPLE_COSTUME, SAMPLE_ARMOR] {
        assert_eq!(
            row_i64(
                &conn,
                "SELECT COUNT(*) FROM item_armors WHERE item_id = ?1",
                &[item_id.into()]
            ),
            1,
            "item {item_id} has exactly one class row after the replacements"
        );
    }

    // The server's item loader accepts the patched copy: 295 new templates, the 1.2 equip
    // sets unchanged, and the samples have the expected class.
    let data = ItemData::load(&conn).expect("ItemData::load accepts the patched copy");
    assert_eq!(
        data.templates.len(),
        before_templates + INSERTED_NEW_TEMPLATES
    );
    assert_eq!(data.equip_item_sets.len(), before_sets);
    assert!(
        data.equip_item_set(206).is_some(),
        "1.2 Eanad set still loads"
    );
    let costume = data.template(SAMPLE_COSTUME).expect("costume template");
    assert_eq!(costume.category_id, 10);
    assert_eq!(costume.level, 1);
    assert!(matches!(
        costume.kind,
        TemplateKind::Armor(ref e) if e.slot_type_id == 13 && e.equip_item_set_id == 0
    ));
    let armor = data.template(SAMPLE_ARMOR).expect("armor template");
    assert_eq!(armor.category_id, 84);
    assert_eq!(armor.level, 49);
    assert!(matches!(
        armor.kind,
        TemplateKind::Armor(ref e) if e.slot_type_id == 1 && e.armor_type_id == 2
    ));
    let cloak = data.template(SAMPLE_CLOAK).expect("cloak template");
    assert!(matches!(
        cloak.kind,
        TemplateKind::Armor(ref e) if e.slot_type_id == 9
    ));
    for item_id in [SAMPLE_CLOAK, SAMPLE_COSTUME, SAMPLE_ARMOR] {
        let template = data.template(item_id).expect("imported template");
        if let TemplateKind::Armor(ref e) = template.kind {
            assert_eq!(e.equip_item_set_id, 0, "item {item_id} has no equip set");
        }
    }
    assert_eq!(
        conn.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .expect("integrity check"),
        "ok"
    );
}
