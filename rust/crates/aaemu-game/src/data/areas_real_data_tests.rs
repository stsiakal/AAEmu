//! Real-data test of the `sunbite-wilds` optional area package.
//!
//! Run with the 1.2 database:
//!
//! ```text
//! AAEMU_COMPACT_DB=<compact.sqlite3> cargo test -p aaemu-game sunbite_area_package -- --ignored
//! ```
//!
//! The test copies the database, applies every `SQL/patches/compact/*.sql` file twice (the
//! applier's `INSERT OR IGNORE` / `NOT EXISTS` rows must be rerun-safe), then loads the zone and
//! quest tables with the package enabled and disabled. It never writes to `AAEMU_COMPACT_DB`.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use super::{world_data::ZoneData, Areas};

fn temp_db(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "aaemu-sunbite-{tag}-{}-{}.sqlite3",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn patch_files() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../SQL/patches/compact");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("SQL/patches/compact")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "sql"))
        .collect();
    files.sort();
    files
}

/// Applies a patch like `cargo xtask compact-patches`: foreign keys stay off, and the file's
/// `PRAGMA` / explicit `BEGIN` / `COMMIT` lines are ignored (the applier strips transaction
/// control and the pragma is a no-op inside its transaction).
fn apply_patch(conn: &Connection, path: &Path) {
    let text = std::fs::read_to_string(path).expect("read patch");
    let body: String = text
        .lines()
        .filter(|line| {
            let l = line.trim_start().to_ascii_lowercase();
            !(l.starts_with("pragma foreign_keys")
                || l == "begin;"
                || l == "begin transaction;"
                || l == "commit;")
        })
        .collect::<Vec<_>>()
        .join("\n");
    conn.execute_batch(&body)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
}

fn load_zones(path: &Path, areas: &Areas) -> ZoneData {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    ZoneData::load_with_areas(&conn, 0, areas).expect("zones")
}

#[test]
#[ignore = "needs AAEMU_COMPACT_DB"]
fn sunbite_area_package_real_data() {
    let Ok(source) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let db = temp_db("patched");
    std::fs::copy(&source, &db).expect("copy compact.sqlite3");

    let patches = patch_files();
    let mut counts = Vec::new();
    for _ in 0..2 {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        for patch in &patches {
            apply_patch(&conn, patch);
        }
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        let zones: i64 = conn
            .query_row("SELECT COUNT(*) FROM zones", [], |r| r.get(0))
            .unwrap();
        let quests: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM quest_contexts WHERE zone_id = 227",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let items: i64 = conn
            .query_row("SELECT COUNT(*) FROM items", [], |r| r.get(0))
            .unwrap();
        counts.push((zones, quests, items));
    }
    assert_eq!(
        counts[0], counts[1],
        "the sunbite patches must be rerun-safe"
    );
    assert_eq!(
        counts[0].1, 78,
        "78 Sunbite quests are at or below level 55"
    );

    let app = Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
    let enabled = Areas::load(&app, &["sunbite-wilds".into()]).expect("areas");
    let disabled = Areas::load(&app, &[]).expect("areas");

    let on = load_zones(&db, &enabled);
    assert_eq!(on.zone_by_key(286).map(|z| z.id), Some(242));
    assert_eq!(on.zone_by_key(287).map(|z| z.id), Some(227));
    let off = load_zones(&db, &disabled);
    assert!(off.zone_by_key(286).is_none());
    assert!(off.zone_by_key(287).is_none());

    let conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let files = crate::data::client_files::ClientFileManager::from_sources(&[], &app);
    let on_quests =
        crate::systems::quests::data::QuestData::load_from_with_areas(&conn, &files, &enabled)
            .expect("quests enabled");
    let off_quests =
        crate::systems::quests::data::QuestData::load_from_with_areas(&conn, &files, &disabled)
            .expect("quests disabled");
    assert_eq!(on_quests.templates.len(), off_quests.templates.len() + 78);

    // The whole subsystem data load must accept the patched rows (NPCs, spawner templates,
    // doodads, quest components/acts) with the package enabled.
    let systems = crate::systems::SystemsData::load(&crate::systems::DataLoadCtx {
        conn: &conn,
        app_path: &app,
        files: &files,
        language: "en_us",
        areas: &enabled,
    })
    .expect("server data with the sunbite package");
    assert!(systems.npcs.npc.exist(16984));
    assert!(systems.quests.templates.contains_key(&8115));
    assert!(!enabled.client_sources().is_empty());
    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(app.join("Data/Areas/sunbite-wilds/overlay.manifest.json"))
            .expect("overlay manifest"),
    )
    .expect("manifest json");
    assert_eq!(manifest["Cells"].as_array().unwrap().len(), 6);
    assert_eq!(manifest["Files"].as_array().unwrap().len(), 73);
    assert_eq!(manifest["Pak"], "sunbite-wilds.pak");

    let systems_off = crate::systems::SystemsData::load(&crate::systems::DataLoadCtx {
        conn: &conn,
        app_path: &app,
        files: &files,
        language: "en_us",
        areas: &disabled,
    })
    .expect("server data without the sunbite package");
    assert!(!systems_off.quests.templates.contains_key(&8115));

    // Owner rules: quests <= 55, reward items <= 62 and no newly imported id >= 8,000,000.
    let over_level: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM quest_contexts WHERE zone_id = 227 AND level > 55",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(over_level, 0);
    let over_cap: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM quest_act_supply_items s \
             JOIN quest_acts a ON a.act_detail_id = s.id AND a.act_detail_type = 'QuestActSupplyItem' \
             JOIN quest_components c ON c.id = a.quest_component_id \
             JOIN quest_contexts q ON q.id = c.quest_context_id \
             JOIN items i ON i.id = s.item_id \
             WHERE q.zone_id = 227 AND i.level > 62",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(over_cap, 0);

    std::fs::remove_file(&db).ok();
}

#[test]
#[ignore = "needs AAEMU_OVERLAY_PAK and AAEMU_GAME_PAK"]
fn sunbite_overlay_pak_is_read_first() {
    let (Ok(overlay), Ok(base)) = (
        std::env::var("AAEMU_OVERLAY_PAK"),
        std::env::var("AAEMU_GAME_PAK"),
    ) else {
        return;
    };
    let app = Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
    let areas = Areas::load(&app, &["sunbite-wilds".into()]).unwrap();
    let files = crate::data::client_files::ClientFileManager::from_sources(&[overlay, base], &app);
    assert!(files.file_exists("game/worlds/main_world/cells/021_005/client/terrain/heightmap.dat"));
    let worlds = super::world_templates::WorldTemplates::load_with_areas(&files, &app, &areas)
        .expect("merged world.xml");
    let main = worlds.get_world_template_by_name("main_world").unwrap();
    assert_eq!(main.xml_world_zones()[&287].origin_x, 21);
    assert_eq!(worlds.get_zone_id(Some(main), 22_200.0, 4_700.0), 287);
}
