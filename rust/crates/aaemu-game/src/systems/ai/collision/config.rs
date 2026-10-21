//! `Configurations/Physics.json` (`"Physics": { … }`): the collision world and the zone-side
//! checks. Every field is optional; the defaults reproduce the zone server's behaviour where
//! it has one (`movement_verify_enable = 0`).

use serde::Deserialize;

use crate::config::{lenient, lenient_enum};

/// What the collision worlds load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GeometryLevel {
    /// Terrain heightfield + ocean only.
    Terrain,
    /// + static brushes (`brush.dat`).
    Brushes,
    /// + vegetation (`vegetation.dat`), the zone's full static world.
    #[default]
    Full,
}

impl crate::config::ConfigEnum for GeometryLevel {
    fn parse_config(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "terrain" | "0" => Some(Self::Terrain),
            "brushes" | "1" => Some(Self::Brushes),
            "full" | "2" => Some(Self::Full),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PhysicsConfig {
    /// The collision query service and the physics worlds (off: every query answers from the
    /// heightmap only, like before).
    #[serde(rename = "enable", deserialize_with = "lenient")]
    pub enable: bool,
    #[serde(rename = "geometry", deserialize_with = "lenient_enum")]
    pub geometry: GeometryLevel,
    /// Cells (1024 m) kept loaded around every player / ship / active NPC: 0 = only the cell
    /// itself, 1 = 3×3 …
    #[serde(rename = "loadradiuscells", deserialize_with = "lenient")]
    pub load_radius_cells: i32,
    /// A query in a cell that is not loaded loads it synchronously (else: terrain only there).
    #[serde(rename = "loadonquery", deserialize_with = "lenient")]
    pub load_on_query: bool,
    /// Loaded cells nobody was near for this long are unloaded (seconds; 0 = never).
    #[serde(rename = "unloadafterseconds", deserialize_with = "lenient")]
    pub unload_after_seconds: u64,
    /// Period of the residency / doodad-collider sync task (milliseconds).
    #[serde(rename = "syncperiodms", deserialize_with = "lenient")]
    pub sync_period_ms: u64,
    /// Doodad / house colliders from their models (`ZdModel*` + `_UpdateCollisionParams`).
    #[serde(rename = "doodadcolliders", deserialize_with = "lenient")]
    pub doodad_colliders: bool,
    /// Load every cell of every world at startup (measurements / tools; slow).
    #[serde(rename = "preloadallcells", deserialize_with = "lenient")]
    pub preload_all_cells: bool,
    /// `movement_verify_enable` (zone CVar, default 0): the ZWReportMoveHack checks.
    #[serde(rename = "movementverifyenable", deserialize_with = "lenient")]
    pub movement_verify_enable: bool,
    /// Server-side movement guard for player-controlled moves (speed over a time window,
    /// teleport-distance jumps, correction). On by default; see the `move_verify_*` fields.
    #[serde(rename = "moveverifyenable", deserialize_with = "lenient")]
    pub move_verify_enable: bool,
    /// Multiplier on the character's real max speed to absorb latency, packet bunching and
    /// client-side animation overshoot (1.35 = 35 % slack).
    #[serde(rename = "moveverifytolerance", deserialize_with = "lenient")]
    pub move_verify_tolerance: f32,
    /// Length of the travel budget window (seconds): a burst of up to this many seconds of
    /// movement is allowed, but the sustained speed is still capped.
    #[serde(rename = "moveverifywindowseconds", deserialize_with = "lenient")]
    pub move_verify_window_seconds: f32,
    /// A single move longer than this (metres) is treated as a teleport and refused
    /// unless the server initiated it.
    #[serde(rename = "moveverifyteleportdistance", deserialize_with = "lenient")]
    pub move_verify_teleport_distance: f32,
    /// How close (metres) a move must land to a server teleport / blink destination to be
    /// accepted as the client's catch-up move.
    #[serde(rename = "moveverifyexpectedradius", deserialize_with = "lenient")]
    pub move_verify_expected_radius: f32,
    /// How long (milliseconds) a server teleport destination stays valid for the catch-up move.
    #[serde(rename = "moveverifyexpectedtimeoutms", deserialize_with = "lenient")]
    pub move_verify_expected_timeout_ms: i64,
    /// Grace after the character used a skill (milliseconds) for knockback / dash / blink
    /// displacements that the client plays.
    #[serde(rename = "moveverifyskillgracems", deserialize_with = "lenient")]
    pub move_verify_skill_grace_ms: i64,
    /// Speed cap (m/s) during the skill grace window.
    #[serde(rename = "moveverifyskillspeed", deserialize_with = "lenient")]
    pub move_verify_skill_speed: f32,
    /// Speed cap (m/s) for falling moves (`ActorFlags.Flag8` / `fallVel`).
    #[serde(rename = "moveverifyfallspeed", deserialize_with = "lenient")]
    pub move_verify_fall_speed: f32,
    /// Speed used when the actor model / stance is missing (custom servers, sparse data).
    #[serde(rename = "moveverifydefaultspeed", deserialize_with = "lenient")]
    pub move_verify_default_speed: f32,
    /// Exempt characters with an access level of at least `move_verify_gm_access_level` (GMs).
    #[serde(rename = "moveverifygmexempt", deserialize_with = "lenient")]
    pub move_verify_gm_exempt: bool,
    #[serde(rename = "moveverifygmaccesslevel", deserialize_with = "lenient")]
    pub move_verify_gm_access_level: i32,
    /// Exempt server-side bots (`/bot`), which are driven by the server, not a client.
    #[serde(rename = "moveverifyexemptbots", deserialize_with = "lenient")]
    pub move_verify_exempt_bots: bool,
    /// Zone fall damage cap `min(fallVel, √(40·Δh))` (ZWUnitFell 0x3935C850).
    #[serde(rename = "falldamagezonecap", deserialize_with = "lenient")]
    pub fall_damage_zone_cap: bool,
    /// Ships on the CryPhysics port (`aaemu-physics` rigid bodies) instead of the Jitter2 subset.
    #[serde(rename = "cryphysicsships", deserialize_with = "lenient")]
    pub cryphysics_ships: bool,
}

impl Default for PhysicsConfig {
    fn default() -> Self {
        Self {
            enable: true,
            geometry: GeometryLevel::Full,
            load_radius_cells: 1,
            load_on_query: true,
            unload_after_seconds: 600,
            sync_period_ms: 1000,
            doodad_colliders: true,
            preload_all_cells: false,
            movement_verify_enable: false,
            move_verify_enable: true,
            move_verify_tolerance: 1.35,
            move_verify_window_seconds: 2.0,
            move_verify_teleport_distance: 50.0,
            move_verify_expected_radius: 20.0,
            move_verify_expected_timeout_ms: 30_000,
            move_verify_skill_grace_ms: 2_000,
            move_verify_skill_speed: 50.0,
            move_verify_fall_speed: 60.0,
            move_verify_default_speed: 12.0,
            move_verify_gm_exempt: true,
            move_verify_gm_access_level: 100,
            move_verify_exempt_bots: true,
            fall_damage_zone_cap: true,
            cryphysics_ships: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn movement_verify_defaults_are_on_with_generous_thresholds() {
        let c = PhysicsConfig::default();
        assert!(c.move_verify_enable);
        assert_eq!(c.move_verify_tolerance, 1.35);
        assert_eq!(c.move_verify_window_seconds, 2.0);
        assert_eq!(c.move_verify_teleport_distance, 50.0);
        assert_eq!(c.move_verify_expected_timeout_ms, 30_000);
        assert!(c.move_verify_gm_exempt);
        assert_eq!(c.move_verify_gm_access_level, 100);
        // The collision verifier keeps its own (off) zone switch.
        assert!(!c.movement_verify_enable);
    }

    #[test]
    fn movement_verify_keys_bind_leniently() {
        // `AppConfig::from_value` lowercases the keys before binding (as the loader does).
        let c: PhysicsConfig = serde_json::from_str(
            r#"{"moveverifyenable":"false","moveverifytolerance":"2.0","moveverifyteleportdistance":"80"}"#,
        )
        .unwrap();
        assert!(!c.move_verify_enable);
        assert_eq!(c.move_verify_tolerance, 2.0);
        assert_eq!(c.move_verify_teleport_distance, 80.0);
    }
}
