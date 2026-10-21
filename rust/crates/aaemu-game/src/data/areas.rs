//! Optional later-version area packages (`Areas:Enabled`).
//!
//! A package is a named set of world data that the 1.2 server does not ship: zone rows (plus
//! their groups / sub zones), NPC spawn placements, quest rows and loot. The compact patch under
//! `SQL/patches/compact/` adds the rows to `compact.sqlite3`; this module decides whether they are
//! visible. Every package descriptor lives in `<app_path>/Data/Areas/<name>.json`:
//!
//! ```json
//! {
//!   "ZoneIds": [227, 242],
//!   "ZoneKeys": [287, 286],
//!   "ZoneGroupIds": [],
//!   "SubZoneIds": [],
//!   "QuestZoneIds": [227],
//!   "ClientSources": ["Data/Areas/sunbite-wilds/sunbite-wilds.pak"],
//!   "Spawns": [
//!     { "World": "main_world", "NpcSpawns": "Data/Areas/sunbite-wilds/npc_spawns.json" }
//!   ]
//! }
//! ```
//!
//! A package is **off unless its name is listed in `Areas:Enabled`**. Its zone / group / sub zone
//! rows, quests and worlds are filtered from the loaders, and its spawn files are loaded only when
//! it is enabled. Disabled rows that a player somehow reaches are refused by
//! `world::session::on_zone_change`. `QuestZoneIds` defaults to `ZoneIds`; a descriptor may omit
//! every list (then enabling it only loads its spawn files).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::GameDataError;

/// One spawn file pair of a package (the loader reads only the files of enabled packages).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct AreaSpawns {
    /// World template name (`main_world`, ...) the file belongs to.
    pub world: String,
    /// `npc_spawns.json`-format file, relative to the application directory.
    pub npc_spawns: Option<String>,
    /// `doodad_spawns.json`-format file, relative to the application directory.
    pub doodad_spawns: Option<String>,
}

/// One `Data/Areas/<name>.json` descriptor (`Name` is the file stem).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct AreaPackage {
    /// Database zone ids (`zones.id`).
    pub zone_ids: Vec<u32>,
    /// Client zone keys (`zones.zone_key`, the ids of `world.xml` / `world.g`).
    pub zone_keys: Vec<u32>,
    /// `zone_groups.id` rows that belong only to the package (never an id present in 1.2).
    pub zone_group_ids: Vec<u32>,
    /// `sub_zones.id` rows that belong only to the package.
    pub sub_zone_ids: Vec<u32>,
    /// Database zone ids whose quests belong to the package (empty = `zone_ids`).
    pub quest_zone_ids: Vec<u32>,
    /// World template names that exist only when the package is enabled (type A packages).
    pub worlds: Vec<String>,
    /// Client sources prepended to `ClientData.Sources` while the package is enabled
    /// (a terrain overlay pak or an unpacked directory).
    pub client_sources: Vec<String>,
    /// Spawn files loaded only when the package is enabled.
    pub spawns: Vec<AreaSpawns>,
}

impl AreaPackage {
    /// The quest zone ids (`QuestZoneIds` when given, else `ZoneIds`).
    pub fn quest_zones(&self) -> Vec<u32> {
        if self.quest_zone_ids.is_empty() {
            self.zone_ids.clone()
        } else {
            self.quest_zone_ids.clone()
        }
    }
}

/// The loaded package set: every descriptor under `Data/Areas`, the enabled names and the
/// filtered ids (everything not enabled is disabled).
#[derive(Debug, Clone, Default)]
pub struct Areas {
    packages: HashMap<String, AreaPackage>,
    enabled: HashSet<String>,
    disabled_zone_ids: HashSet<u32>,
    disabled_zone_keys: HashSet<u32>,
    disabled_group_ids: HashSet<u32>,
    disabled_sub_zone_ids: HashSet<u32>,
    disabled_quest_zone_ids: HashSet<u32>,
    disabled_worlds: HashSet<String>,
    client_sources: Vec<String>,
    npc_spawn_files: HashMap<String, Vec<PathBuf>>,
    doodad_spawn_files: HashMap<String, Vec<PathBuf>>,
}

impl Areas {
    /// A static empty set (loaders called without the server configuration, e.g. tests).
    pub fn none() -> &'static Areas {
        static NONE: std::sync::OnceLock<Areas> = std::sync::OnceLock::new();
        NONE.get_or_init(Areas::default)
    }

    /// Scans `<app_path>/Data/Areas/*.json` and enables the requested package names.
    ///
    /// A missing `Data/Areas` directory is fine when nothing is enabled. An enabled name
    /// without a descriptor, a duplicate descriptor or a missing spawn file is a startup error
    /// (a silent typo would otherwise disable a package that the operator expects to run).
    pub fn load(app_path: &Path, enabled: &[String]) -> Result<Self, GameDataError> {
        let dir = app_path.join("Data").join("Areas");
        let mut packages = HashMap::new();
        if dir.is_dir() {
            let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
                .map_err(|e| GameDataError::File(format!("Areas: {}: {e}", dir.display())))?
                .filter_map(Result::ok)
                .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                .map(|e| e.path())
                .filter(|p| {
                    p.extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("json"))
                })
                .collect();
            files.sort();
            for path in files {
                let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                let text = std::fs::read_to_string(&path)
                    .map_err(|e| GameDataError::File(format!("Areas: {}: {e}", path.display())))?;
                let package: AreaPackage =
                    serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| {
                        GameDataError::File(format!("Areas: {}: {e}", path.display()))
                    })?;
                if packages.insert(name.to_string(), package).is_some() {
                    return Err(GameDataError::File(format!(
                        "Areas: duplicate package name {name}"
                    )));
                }
            }
        }

        let mut areas = Areas {
            packages,
            ..Areas::default()
        };
        for name in enabled {
            let Some(package) = areas.packages.get(name).cloned() else {
                return Err(GameDataError::File(format!(
                    "Areas:Enabled lists unknown package {name} (no Data/Areas/{name}.json)"
                )));
            };
            areas.enabled.insert(name.clone());
            for source in &package.client_sources {
                areas
                    .client_sources
                    .push(app_path.join(source).to_string_lossy().into_owned());
            }
            for spawns in &package.spawns {
                for (field, target) in [
                    (&spawns.npc_spawns, &mut areas.npc_spawn_files),
                    (&spawns.doodad_spawns, &mut areas.doodad_spawn_files),
                ] {
                    let Some(relative) = field else {
                        continue;
                    };
                    let path = app_path.join(relative);
                    if !path.is_file() {
                        return Err(GameDataError::File(format!(
                            "Areas: package {name} spawn file {} does not exist",
                            path.display()
                        )));
                    }
                    target.entry(spawns.world.clone()).or_default().push(path);
                }
            }
        }

        // Everything that is not enabled is filtered. A package that is not listed stays fully
        // inert even when its compact patch has been applied.
        for (name, package) in &areas.packages {
            if areas.enabled.contains(name) {
                continue;
            }
            areas
                .disabled_zone_ids
                .extend(package.zone_ids.iter().copied());
            areas
                .disabled_zone_keys
                .extend(package.zone_keys.iter().copied());
            areas
                .disabled_group_ids
                .extend(package.zone_group_ids.iter().copied());
            areas
                .disabled_sub_zone_ids
                .extend(package.sub_zone_ids.iter().copied());
            areas.disabled_quest_zone_ids.extend(package.quest_zones());
            areas.disabled_worlds.extend(package.worlds.iter().cloned());
        }

        if !areas.enabled.is_empty() {
            let mut names: Vec<&String> = areas.enabled.iter().collect();
            names.sort();
            tracing::info!("Enabled optional areas: {:?}", names);
        }
        Ok(areas)
    }

    /// Whether a package is enabled.
    pub fn is_enabled(&self, name: &str) -> bool {
        self.enabled.contains(name)
    }

    /// The enabled package names (sorted).
    pub fn enabled_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.enabled.iter().cloned().collect();
        names.sort();
        names
    }

    /// Whether a zone row (by database id and client key) belongs to a disabled package.
    pub fn zone_disabled(&self, zone_id: u32, zone_key: u32) -> bool {
        self.disabled_zone_ids.contains(&zone_id) || self.disabled_zone_keys.contains(&zone_key)
    }

    /// Whether a client zone key belongs to a disabled package.
    pub fn zone_key_disabled(&self, zone_key: u32) -> bool {
        self.disabled_zone_keys.contains(&zone_key)
    }

    /// Whether a database zone id belongs to a disabled package.
    pub fn zone_id_disabled(&self, zone_id: u32) -> bool {
        self.disabled_zone_ids.contains(&zone_id)
    }

    /// Whether a zone group row belongs to a disabled package.
    pub fn zone_group_disabled(&self, group_id: u32) -> bool {
        self.disabled_group_ids.contains(&group_id)
    }

    /// Whether a `sub_zones` row belongs to a disabled package.
    pub fn sub_zone_disabled(&self, sub_zone_id: u32) -> bool {
        self.disabled_sub_zone_ids.contains(&sub_zone_id)
    }

    /// Whether quests of this database zone id belong to a disabled package.
    pub fn quest_zone_disabled(&self, zone_id: u32) -> bool {
        self.disabled_quest_zone_ids.contains(&zone_id)
    }

    /// Whether a world template belongs to a disabled package.
    pub fn world_disabled(&self, world: &str) -> bool {
        self.disabled_worlds.contains(world)
    }

    /// Client sources of every enabled package, in descriptor order.
    pub fn client_sources(&self) -> &[String] {
        &self.client_sources
    }

    /// Package NPC spawn files of a world template (enabled packages only).
    pub fn npc_spawn_files(&self, world: &str) -> &[PathBuf] {
        self.npc_spawn_files.get(world).map_or(&[], Vec::as_slice)
    }

    /// Package doodad spawn files of a world template (enabled packages only).
    pub fn doodad_spawn_files(&self, world: &str) -> &[PathBuf] {
        self.doodad_spawn_files
            .get(world)
            .map_or(&[], Vec::as_slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aaemu-areas-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("Data/Areas")).unwrap();
        dir
    }

    fn write_package(root: &Path, name: &str, json: &str) {
        std::fs::write(root.join(format!("Data/Areas/{name}.json")), json).unwrap();
    }

    #[test]
    fn disabled_by_default_and_ids_filtered() {
        let root = temp_dir("off");
        write_package(
            &root,
            "sunbite-wilds",
            r#"{
                "ZoneIds": [227, 242],
                "ZoneKeys": [287, 286],
                "QuestZoneIds": [227],
                "Worlds": ["instance_optional"],
                "Spawns": [{ "World": "main_world", "NpcSpawns": "Data/Areas/npc.json" }]
            }"#,
        );
        std::fs::write(root.join("Data/Areas/npc.json"), "[]").unwrap();
        let areas = Areas::load(&root, &[]).unwrap();
        assert!(!areas.is_enabled("sunbite-wilds"));
        assert!(areas.zone_disabled(227, 287));
        assert!(areas.zone_key_disabled(286));
        assert!(areas.zone_id_disabled(242));
        assert!(areas.quest_zone_disabled(227));
        assert!(!areas.quest_zone_disabled(242));
        assert!(areas.world_disabled("instance_optional"));
        assert!(areas.npc_spawn_files("main_world").is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn enabled_package_keeps_ids_and_loads_spawn_files() {
        let root = temp_dir("on");
        write_package(
            &root,
            "sunbite-wilds",
            r#"{
                "ZoneIds": [227, 242],
                "ZoneKeys": [287, 286],
                "ClientSources": ["Data/Areas/overlay.pak"],
                "Spawns": [{ "World": "main_world", "NpcSpawns": "Data/Areas/npc.json" }]
            }"#,
        );
        std::fs::write(root.join("Data/Areas/npc.json"), "[]").unwrap();
        let areas = Areas::load(&root, &["sunbite-wilds".into()]).unwrap();
        assert!(areas.is_enabled("sunbite-wilds"));
        assert!(!areas.zone_disabled(227, 287));
        assert!(!areas.world_disabled("instance_optional"));
        assert_eq!(areas.npc_spawn_files("main_world").len(), 1);
        assert_eq!(areas.client_sources().len(), 1);
        assert!(areas.client_sources()[0].ends_with("overlay.pak"));
        assert_eq!(areas.enabled_names(), vec!["sunbite-wilds".to_string()]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn quest_zone_ids_default_to_zone_ids() {
        let p: AreaPackage = serde_json::from_str(r#"{ "ZoneIds": [1, 2] }"#).unwrap();
        assert_eq!(p.quest_zones(), vec![1, 2]);
        let p: AreaPackage =
            serde_json::from_str(r#"{ "ZoneIds": [1, 2], "QuestZoneIds": [2] }"#).unwrap();
        assert_eq!(p.quest_zones(), vec![2]);
    }

    #[test]
    fn unknown_enabled_package_is_an_error() {
        let root = temp_dir("unknown");
        let err = Areas::load(&root, &["nope".into()]).unwrap_err();
        assert!(err.to_string().contains("unknown package nope"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn missing_enabled_spawn_file_is_an_error() {
        let root = temp_dir("missing");
        write_package(
            &root,
            "p",
            r#"{ "Spawns": [{ "World": "main_world", "NpcSpawns": "Data/Areas/missing.json" }] }"#,
        );
        let err = Areas::load(&root, &["p".into()]).unwrap_err();
        assert!(err.to_string().contains("spawn file"));
        std::fs::remove_dir_all(&root).ok();
    }
}
