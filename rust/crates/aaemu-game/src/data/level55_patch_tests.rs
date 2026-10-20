//! Regression test for `SQL/patches/compact/2026-10-20-level-55-retail-3.0.sql`: the patch
//! unhides the ten ability_level 55 capstones, sets the 3.0.4.2 experience curve for levels
//! 51-55 and the 3.0 quest supplies exp for levels 51-55, while keeping the 1.2 skill points,
//! quest copper, mate experience and every other row. The test executes the real patch file
//! (`include_str!`) on a copy of the database, twice (idempotence), then loads the result with
//! the server's own loaders.
//!
//! `AAEMU_COMPACT_DB=rust/crates/aaemu-game/app/Data/compact.sqlite3 \
//!  cargo test -p aaemu-game level55_patch -- --ignored`

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

use super::{load_levels, ExperienceTable, SkillData};

/// The actual compact-data patch.
const PATCH_SQL: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../SQL/patches/compact/2026-10-20-level-55-retail-3.0.sql"
));

const CAPSTONES: [u32; 10] = [
    23587, 23588, 23589, 23591, 23592, 23593, 23594, 23595, 23596, 23934,
];

/// `levels.total_exp` 51..55 from the 3.0.4.2 compact.
const EXP_51_55: [i32; 5] = [5_174_400, 6_550_320, 8_751_792, 12_164_073, 17_453_110];

/// `levels.skill_points` 51..55, unchanged from 1.2.
const SKILL_POINTS_51_55: [i32; 5] = [24, 25, 26, 27, 28];

/// `levels.total_mate_exp` 51..55, deliberately unchanged from 1.2.
const MATE_EXP_51_55: [i32; 5] = [2_146_250, 2_276_300, 2_411_500, 2_551_950, 2_697_750];

/// `quest_supplies.exp` 51..55 from the 3.0.4.2 compact.
const QUEST_EXP_51_55: [i32; 5] = [9_023, 12_604, 16_986, 21_989, 27_512];

/// `quest_supplies.copper` 51..55, unchanged from 1.2.
const QUEST_COPPER_51_55: [i32; 5] = [3_300, 3_404, 3_510, 3_618, 3_728];

/// A private copy of the real database, removed when the test ends (also on panic).
struct TempCopy(PathBuf);

impl TempCopy {
    fn new(source: &Path) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = std::env::temp_dir().join(format!(
            "aaemu-level55-patch-{}-{nanos}.sqlite3",
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

fn row_i32(conn: &Connection, sql: &str, params: &[i64]) -> i32 {
    conn.query_row(sql, rusqlite::params_from_iter(params), |r| {
        r.get::<_, i64>(0).map(|v| v as i32)
    })
    .expect("row")
}

fn level_exp(conn: &Connection, level: i32) -> i32 {
    row_i32(
        conn,
        "SELECT total_exp FROM levels WHERE id = ?1",
        &[level.into()],
    )
}

fn mate_exp(conn: &Connection, level: i32) -> i32 {
    row_i32(
        conn,
        "SELECT total_mate_exp FROM levels WHERE id = ?1",
        &[level.into()],
    )
}

fn skill_points(conn: &Connection, level: i32) -> i32 {
    row_i32(
        conn,
        "SELECT skill_points FROM levels WHERE id = ?1",
        &[level.into()],
    )
}

fn quest_exp(conn: &Connection, level: i32) -> i32 {
    row_i32(
        conn,
        "SELECT exp FROM quest_supplies WHERE level = ?1",
        &[level.into()],
    )
}

fn quest_copper(conn: &Connection, level: i32) -> i32 {
    row_i32(
        conn,
        "SELECT copper FROM quest_supplies WHERE level = ?1",
        &[level.into()],
    )
}

#[test]
#[ignore = "needs AAEMU_COMPACT_DB"]
fn level55_patch_applies_to_a_copy_and_loads() {
    let Ok(source) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let copy = TempCopy::new(Path::new(&source));
    let conn = copy.open();

    // A pristine 1.2 row the patch never touches, and the counts of the three patched tables.
    assert_eq!(level_exp(&conn, 50), 4_410_000);
    assert_eq!(quest_exp(&conn, 50), 4_711);
    assert_eq!(mate_exp(&conn, 50), 2_021_250);
    let levels: i64 = conn
        .query_row("SELECT COUNT(*) FROM levels", [], |r| r.get(0))
        .unwrap();
    let supplies: i64 = conn
        .query_row("SELECT COUNT(*) FROM quest_supplies", [], |r| r.get(0))
        .unwrap();

    conn.execute_batch(PATCH_SQL).expect("patch");
    // The exact-original guards make a second run change nothing.
    conn.execute_batch(PATCH_SQL).expect("patch rerun");

    for (index, level) in (51..=55).enumerate() {
        assert_eq!(
            level_exp(&conn, level),
            EXP_51_55[index],
            "levels.total_exp {level}"
        );
        assert_eq!(
            skill_points(&conn, level),
            SKILL_POINTS_51_55[index],
            "levels.skill_points {level}"
        );
        assert_eq!(
            mate_exp(&conn, level),
            MATE_EXP_51_55[index],
            "levels.total_mate_exp {level}"
        );
        assert_eq!(
            quest_exp(&conn, level),
            QUEST_EXP_51_55[index],
            "quest_supplies.exp {level}"
        );
        assert_eq!(
            quest_copper(&conn, level),
            QUEST_COPPER_51_55[index],
            "quest_supplies.copper {level}"
        );
    }
    // The 1.2 level 56 row is untouched, so the 1.2 mate curve stays strictly increasing.
    assert_eq!(level_exp(&conn, 56), 999_999_056);
    assert_eq!(mate_exp(&conn, 56), 2_849_000);
    assert_eq!(levels, 101);
    assert_eq!(supplies, 55);
    assert_eq!(mate_exp(&conn, 55), 2_697_750);

    // The server's loaders accept the patched copy.
    let exp = ExperienceTable::from_templates(load_levels(&conn).unwrap()).unwrap();
    assert_eq!(exp.max_player_level(), 55);
    for (index, level) in (51..=55).enumerate() {
        assert_eq!(exp.exp_for_level(level, false), EXP_51_55[index]);
        assert_eq!(
            exp.exp_for_level(level, true),
            MATE_EXP_51_55[index],
            "mate exp {level}"
        );
        assert_eq!(exp.skill_points_for_level(level), SKILL_POINTS_51_55[index]);
    }

    let skills = SkillData::load(&conn).unwrap();
    for id in CAPSTONES {
        let skill = skills.skill(id).expect("capstone template");
        assert!(skill.show, "capstone {id} is visible");
        assert_eq!(skill.ability_level, 55, "capstone {id} ability level");
    }
    // The retired 1.2 ability_level 99 rows stay hidden.
    assert!(!skills.skill(10_385).expect("retired row").show);
    let visible: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM skills WHERE show IN ('t', '1')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(visible, 5_645);
}
