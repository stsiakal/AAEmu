//! The zone's movement verifier (`ZWReportMoveHack`, type 57): x2game `sub_3921E300` (entry,
//! from the WZUnitMovement handler 0x39362E80), samples `sub_39218810` (character) /
//! `sub_39219740` (vehicle), report `sub_3921BD10`. It runs only with
//! `movement_verify_enable` (CGame +1156, default 0) → `Physics.MovementVerifyEnable`
//! (default off).
//!
//! Ported checks (the ones that are arithmetic on the movement stream plus
//! `GetTerrainElevation`; `ZONE-PHYSICS-USAGE.md` §1.3, `zone_protocol_physics.md` §1):
//!
//! | reason | check | reliability |
//! |---|---|---|
//! | 0 | client time going backwards (0x3921C4C0) | 100 |
//! | 11 | under terrain: `terrainZ − z > 0.1`, `count·80 / samples` (0x3921D720) | computed |
//! | 12 | `z < movement_verify_min_z_pos` (−1) (0x3921D720) | `min(10·count, 100)` |
//! | 15 / 16 / 17 | teleport: 2-D speed > 150 / 100 / 50 m/s over > 50 / 25 / 10 m (0x3921C0A0), ≥ 5000 ms between reports | 90 / 80 / 50 |
//! | 19 | vehicle warp: driver move > 500 m (`d² > 250000`, 0x3924B550) | 50 |
//!
//! Not ported (need the zone actor's own simulation): 1 clock rate, 2 on-ground, 3 gravity,
//! 4 air-standing, 6 speed windows, 9 packet rate, 10 over-velocity, 14 pattern analysis,
//! 18 vehicle levitation. AAEmu has no consumer for the report: it is logged (warn) and
//! counted.

use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;

use crate::models::movement::{GameStanceType, MoveTypeActorFlags, UnitMove};
use crate::models::unit::UnitAttribute;
use crate::server::GameServer;
use crate::world::WorldObject;

use super::config::PhysicsConfig;

/// `movement_verify_min_z_pos`.
pub const MIN_Z_POS: f32 = -1.0;
/// Under-terrain tolerance (`terrainZ − z > 0.1`).
pub const UNDER_TERRAIN_TOLERANCE: f32 = 0.1;
/// `movement_verify_sample_max`.
pub const SAMPLE_MAX: u32 = 200;
/// Minimum time between teleport reports.
pub const TELEPORT_REPORT_INTERVAL_MS: i64 = 5000;
/// Vehicle warp (`dist² > 250000`).
pub const VEHICLE_WARP_DIST2: f32 = 250_000.0;

/// A ZWReportMoveHack.
#[derive(Debug, Clone, PartialEq)]
pub struct MoveHackReport {
    pub reason: i32,
    pub obj_id: u32,
    pub text: String,
    pub reliability: u8,
    pub extra: f32,
}

/// One movement sample (world position, client / server times).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MoveSample {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub client_ms: i64,
    pub server_ms: i64,
    /// Terrain elevation under the position (`GetTerrainElevation`, 0 = unknown).
    pub terrain_z: f32,
    /// The sample is a teleport / ship / climb / forced sync (skipped by the warp checks).
    pub exempt: bool,
}

#[derive(Debug, Clone, Default)]
struct UnitState {
    last: Option<MoveSample>,
    samples: u32,
    under_terrain: u32,
    below_min_z: u32,
    last_teleport_report_ms: i64,
}

/// The per-unit verifiers.
#[derive(Debug, Default)]
pub struct MoveVerifier {
    units: DashMap<u32, UnitState>,
    pub reports: AtomicU64,
}

/// The teleport class of a move (`sub_3921C0A0`): reason and reliability.
pub fn teleport_class(dist2d: f32, dt_s: f32) -> Option<(i32, u8)> {
    if dt_s <= 0.0 {
        return None;
    }
    let speed = dist2d / dt_s;
    if speed > 150.0 && dist2d > 50.0 {
        Some((15, 90))
    } else if speed > 100.0 && dist2d > 25.0 {
        Some((16, 80))
    } else if speed > 50.0 && dist2d > 10.0 {
        Some((17, 50))
    } else {
        None
    }
}

impl MoveVerifier {
    /// Verifies a character movement sample; returns the reports it produces.
    pub fn verify(&self, obj_id: u32, s: MoveSample) -> Vec<MoveHackReport> {
        let mut out = Vec::new();
        let mut st = self.units.entry(obj_id).or_insert_with(|| UnitState {
            last_teleport_report_ms: i64::MIN / 2,
            ..UnitState::default()
        });
        let report = |reason: i32, text: String, reliability: u8, extra: f32| MoveHackReport {
            reason,
            obj_id,
            text,
            reliability,
            extra,
        };
        if let Some(prev) = st.last {
            if s.client_ms < prev.client_ms {
                out.push(report(
                    0,
                    format!("client time {} < {}", s.client_ms, prev.client_ms),
                    100,
                    (prev.client_ms - s.client_ms) as f32,
                ));
            }
            if !s.exempt && !prev.exempt {
                let d = ((s.x - prev.x).powi(2) + (s.y - prev.y).powi(2)).sqrt();
                let dt = (s.server_ms - prev.server_ms) as f32 / 1000.0;
                if let Some((reason, rel)) = teleport_class(d, dt) {
                    if s.server_ms - st.last_teleport_report_ms >= TELEPORT_REPORT_INTERVAL_MS {
                        st.last_teleport_report_ms = s.server_ms;
                        out.push(report(
                            reason,
                            format!("moved {d:.1} m in {dt:.3} s"),
                            rel,
                            d / dt,
                        ));
                    }
                }
            }
        }
        st.samples += 1;
        if s.terrain_z != 0.0 && s.terrain_z - s.z > UNDER_TERRAIN_TOLERANCE {
            st.under_terrain += 1;
        }
        if s.z < MIN_Z_POS {
            st.below_min_z += 1;
        }
        if st.samples >= SAMPLE_MAX {
            if st.under_terrain > 0 {
                let rel = (st.under_terrain * 80 / st.samples).min(100) as u8;
                out.push(report(
                    11,
                    format!("under terrain {}/{}", st.under_terrain, st.samples),
                    rel,
                    st.under_terrain as f32,
                ));
            }
            if st.below_min_z > 0 {
                out.push(report(
                    12,
                    format!("z < {MIN_Z_POS} {}/{}", st.below_min_z, st.samples),
                    (10 * st.below_min_z).min(100) as u8,
                    st.below_min_z as f32,
                ));
            }
            st.samples = 0;
            st.under_terrain = 0;
            st.below_min_z = 0;
        }
        st.last = Some(s);
        drop(st);
        for r in &out {
            self.reports.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                "ZWReportMoveHack unit {} reason {} reliability {}: {}",
                r.obj_id,
                r.reason,
                r.reliability,
                r.text
            );
        }
        out
    }

    /// Vehicle warp (`sub_3924B550`): the driver moved the vehicle more than 500 m.
    pub fn verify_vehicle(
        &self,
        obj_id: u32,
        from: (f32, f32, f32),
        to: (f32, f32, f32),
    ) -> Option<MoveHackReport> {
        let d2 = (to.0 - from.0).powi(2) + (to.1 - from.1).powi(2) + (to.2 - from.2).powi(2);
        (d2 > VEHICLE_WARP_DIST2).then(|| {
            self.reports.fetch_add(1, Ordering::Relaxed);
            let r = MoveHackReport {
                reason: 19,
                obj_id,
                text: format!("vehicle warp {:.1} m", d2.sqrt()),
                reliability: 50,
                extra: d2.sqrt(),
            };
            tracing::warn!("ZWReportMoveHack unit {obj_id} reason 19: {}", r.text);
            r
        })
    }

    /// Forgets a unit (logout / teleport).
    pub fn reset(&self, obj_id: u32) {
        self.units.remove(&obj_id);
    }
}

// ---------------------------------------------------------------------------------------------
// Player movement guard (Rust only)
// ---------------------------------------------------------------------------------------------

/// What the movement guard decided about one player-controlled move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveDecision {
    /// Apply and broadcast the move.
    Accept,
    /// Refuse the move and send the last valid position back to the player.
    Correct(String),
    /// Refuse the move without a correction packet (the client does not own this unit's
    /// position directly, e.g. a mount seat).
    Refuse(String),
}

/// The accepted-movement anchor and travel budget of one unit.
#[derive(Debug, Clone, Copy)]
struct SpeedState {
    x: f32,
    y: f32,
    z: f32,
    last_ms: i64,
    /// Distance the unit may still travel (metres); refilled at the real max speed.
    budget: f32,
    instance_id: u32,
    /// `Character.teleport_seq` at the anchor (players; 0 for other units).
    teleport_seq: u32,
}

/// The action snapshot of a unit for one move.
struct UnitSnapshot {
    model_id: u32,
    speed_mul: f32,
    x: f32,
    y: f32,
    z: f32,
    instance_id: u32,
    parent_obj_id: u32,
    buff_ids: Vec<u32>,
    skill_last_used_ms: i64,
    displaced_until_ms: i64,
}

/// Server-side movement validation for player-controlled units (`CSMoveUnit` /
/// `VehicleMoveType`). Rust only: the 1.2 C# trusted every client position (`SusManager` only
/// logged speeds above a flat 25 m/s).
///
/// The check is a travel budget over a time window, not a per-packet speed: every move refills
/// the unit's budget with `max_speed * tolerance * dt` (capped at the window's worth), so packet
/// bunching / latency bursts are accepted while the sustained speed stays bounded. A single move
/// longer than `move_verify_teleport_distance` is always refused unless the server itself sent
/// the client there (portals, GM `/move`, resurrection, instance moves, blink) and the move
/// lands within `move_verify_expected_radius` of that destination.
#[derive(Debug, Default)]
pub struct MoveSpeedGuard {
    units: DashMap<u32, SpeedState>,
}

impl MoveSpeedGuard {
    /// Forgets a unit (logout / teleport / despawn).
    pub fn reset(&self, obj_id: u32) {
        self.units.remove(&obj_id);
    }

    /// Verifies a unit move of `target` (a player or the mate it controls) at `now_ms`.
    pub fn check_unit_at(
        &self,
        server: &GameServer,
        target: &WorldObject,
        u: &UnitMove,
        now_ms: i64,
    ) -> MoveDecision {
        let cfg = &server.config.physics;
        if !cfg.move_verify_enable {
            self.reset(target.obj_id());
            return MoveDecision::Accept;
        }
        let Some(snap) = snapshot(target, now_ms) else {
            return MoveDecision::Accept;
        };
        let obj_id = target.obj_id();
        // Deck / climb moves carry coordinates in the supporting object's frame: the vehicle
        // itself is validated separately, so the rider's local offsets are not world motion.
        if snap.parent_obj_id != 0
            || u.actor_flags
                .contains(MoveTypeActorFlags::STANDING_ON_OBJECT)
            || u.actor_flags
                .contains(MoveTypeActorFlags::HANGING_FROM_OBJECT)
        {
            self.reset(obj_id);
            return MoveDecision::Accept;
        }
        // Sparse / custom data: without the actor model there is no real max speed to judge
        // against, so the guard stays out of the way instead of guessing.
        if server
            .game_data
            .systems
            .ai
            .ai
            .actor_model(snap.model_id)
            .is_none()
        {
            self.reset(obj_id);
            return MoveDecision::Accept;
        }
        let pos = [u.x, u.y, u.z];
        let falling = u.actor_flags.contains(MoveTypeActorFlags::FLAG8) || u.fall_vel > 0;

        let mut teleport_seq = 0;
        let mut is_player = false;
        if let Some(p) = target.as_player() {
            is_player = true;
            let (access_level, seq, character_id, expected_ok) = {
                let mut c = p.data();
                teleport_seq = c.teleport_seq;
                let access = c.access_level;
                let character_id = c.id;
                let expected = c.expected_position;
                let fresh = expected.is_some_and(|e| {
                    e.instance_id == snap.instance_id
                        && now_ms >= e.sent_ms
                        && now_ms - e.sent_ms <= cfg.move_verify_expected_timeout_ms
                });
                let ok = fresh
                    && c.take_expected_position(
                        pos[0],
                        pos[1],
                        pos[2],
                        cfg.move_verify_expected_radius,
                    );
                (access, c.teleport_seq, character_id, ok)
            };
            if cfg.move_verify_gm_exempt && access_level >= cfg.move_verify_gm_access_level {
                self.reset(obj_id);
                return MoveDecision::Accept;
            }
            if cfg.move_verify_exempt_bots && server.systems.bots.is_bot(character_id) {
                self.reset(obj_id);
                return MoveDecision::Accept;
            }
            if expected_ok {
                // The client's catch-up move after a server teleport: accept it as the anchor.
                self.seed(obj_id, pos, now_ms, snap.instance_id, seq);
                return MoveDecision::Accept;
            }
        }
        if self.units.get(&obj_id).is_none() {
            // Nothing accepted yet: judge the first move from the server's own position (the
            // last accepted transform), with 200 ms of budget for the login / landing burst.
            // Seeding at the packet position would accept one arbitrary teleport.
            self.units.insert(
                obj_id,
                SpeedState {
                    x: snap.x,
                    y: snap.y,
                    z: snap.z,
                    last_ms: now_ms - 200,
                    budget: 0.0,
                    instance_id: snap.instance_id,
                    teleport_seq,
                },
            );
        }
        let max_speed = self.max_speed(server, &snap, u, is_player, falling, now_ms, cfg);
        self.check(
            obj_id,
            pos,
            now_ms,
            snap.instance_id,
            teleport_seq,
            max_speed,
            cfg,
            is_player,
        )
    }

    /// Verifies a client-driven land vehicle move (`VehicleMoveType`) at `now_ms`.
    pub fn check_vehicle_at(
        &self,
        server: &GameServer,
        slave: &crate::world::SlaveRef,
        pos: [f32; 3],
        now_ms: i64,
    ) -> MoveDecision {
        let cfg = &server.config.physics;
        if !cfg.move_verify_enable {
            self.reset(slave.obj_id);
            return MoveDecision::Accept;
        }
        let (model_id, speed_mul, instance_id, tx, ty, tz) = {
            let d = slave.data();
            let t = d.unit.transform;
            (
                d.unit.model_id,
                (d.unit
                    .calculate_with_bonuses(1000.0, UnitAttribute::MoveSpeedMul, now_ms)
                    as f32)
                    / 1000.0,
                t.instance_id,
                t.x,
                t.y,
                t.z,
            )
        };
        let Some(base) = server
            .game_data
            .systems
            .vehicles
            .vehicle_model(model_id)
            .map(|m| m.velocity)
            .filter(|v| *v > 0.0)
        else {
            self.reset(slave.obj_id);
            return MoveDecision::Accept;
        };
        let max_speed = (base * speed_mul).max(0.1);
        if self.units.get(&slave.obj_id).is_none() {
            self.units.insert(
                slave.obj_id,
                SpeedState {
                    x: tx,
                    y: ty,
                    z: tz,
                    last_ms: now_ms - 200,
                    budget: 0.0,
                    instance_id,
                    teleport_seq: 0,
                },
            );
        }
        self.check(
            slave.obj_id,
            pos,
            now_ms,
            instance_id,
            0,
            max_speed,
            cfg,
            false,
        )
    }

    /// [`Self::check_unit_at`] with the server clock.
    pub fn check_unit(
        &self,
        server: &GameServer,
        target: &WorldObject,
        u: &UnitMove,
    ) -> MoveDecision {
        self.check_unit_at(server, target, u, crate::world::now_ms())
    }

    /// [`Self::check_vehicle_at`] with the server clock.
    pub fn check_vehicle(
        &self,
        server: &GameServer,
        slave: &crate::world::SlaveRef,
        pos: [f32; 3],
    ) -> MoveDecision {
        self.check_vehicle_at(server, slave, pos, crate::world::now_ms())
    }

    fn seed(&self, obj_id: u32, pos: [f32; 3], now_ms: i64, instance_id: u32, teleport_seq: u32) {
        self.units.insert(
            obj_id,
            SpeedState {
                x: pos[0],
                y: pos[1],
                z: pos[2],
                last_ms: now_ms,
                budget: 0.0,
                instance_id,
                teleport_seq,
            },
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn check(
        &self,
        obj_id: u32,
        pos: [f32; 3],
        now_ms: i64,
        instance_id: u32,
        teleport_seq: u32,
        max_speed: f32,
        cfg: &PhysicsConfig,
        player: bool,
    ) -> MoveDecision {
        let reason = |text: String| {
            if player {
                MoveDecision::Correct(text)
            } else {
                MoveDecision::Refuse(text)
            }
        };
        let capacity =
            (max_speed * cfg.move_verify_tolerance * cfg.move_verify_window_seconds).max(1.0);
        let Some(prev) = self.units.get(&obj_id).map(|r| *r) else {
            self.units.insert(
                obj_id,
                SpeedState {
                    x: pos[0],
                    y: pos[1],
                    z: pos[2],
                    last_ms: now_ms,
                    budget: 0.0,
                    instance_id,
                    teleport_seq,
                },
            );
            return MoveDecision::Accept;
        };
        let dist = distance([prev.x, prev.y, prev.z], pos);
        if prev.instance_id != instance_id || prev.teleport_seq != teleport_seq {
            // A server-initiated teleport without a destination marker (or an instance move):
            // re-anchor, but still bound the jump so it cannot be used as a free teleport.
            if dist > cfg.move_verify_teleport_distance {
                return reason(format!(
                    "teleport {dist:.1} m (max single move {:.1} m)",
                    cfg.move_verify_teleport_distance
                ));
            }
            self.units.insert(
                obj_id,
                SpeedState {
                    x: pos[0],
                    y: pos[1],
                    z: pos[2],
                    last_ms: now_ms,
                    budget: 0.0,
                    instance_id,
                    teleport_seq,
                },
            );
            return MoveDecision::Accept;
        }
        let dt = (now_ms - prev.last_ms) as f32 / 1000.0;
        if !(0.0..=10.0).contains(&dt) {
            if dist > cfg.move_verify_teleport_distance {
                return reason(format!(
                    "teleport {dist:.1} m (max single move {:.1} m)",
                    cfg.move_verify_teleport_distance
                ));
            }
            self.units.insert(
                obj_id,
                SpeedState {
                    x: pos[0],
                    y: pos[1],
                    z: pos[2],
                    last_ms: now_ms,
                    budget: 0.0,
                    instance_id,
                    teleport_seq,
                },
            );
            return MoveDecision::Accept;
        }
        if dist > cfg.move_verify_teleport_distance {
            return reason(format!(
                "teleport {dist:.1} m (max single move {:.1} m)",
                cfg.move_verify_teleport_distance
            ));
        }
        let rate = max_speed * cfg.move_verify_tolerance;
        let budget = (prev.budget + rate * dt).min(capacity);
        if dist > budget {
            let speed = if dt > 0.0 { dist / dt } else { f32::INFINITY };
            return reason(format!(
                "moved {dist:.2} m in {dt:.3} s ({speed:.1} m/s, max {max_speed:.1} m/s)"
            ));
        }
        self.units.insert(
            obj_id,
            SpeedState {
                x: pos[0],
                y: pos[1],
                z: pos[2],
                last_ms: now_ms,
                budget: budget - dist,
                instance_id,
                teleport_seq,
            },
        );
        MoveDecision::Accept
    }

    #[allow(clippy::too_many_arguments)]
    fn max_speed(
        &self,
        server: &GameServer,
        snap: &UnitSnapshot,
        u: &UnitMove,
        player: bool,
        falling: bool,
        now_ms: i64,
        cfg: &PhysicsConfig,
    ) -> f32 {
        let glider = player
            .then(|| glider_speed(server, &snap.buff_ids))
            .flatten();
        let base = glider
            .or_else(|| stance_speed(server, snap.model_id, u.stance, player))
            .unwrap_or(cfg.move_verify_default_speed);
        let mut max_speed = (base * snap.speed_mul).max(0.1);
        if falling {
            max_speed = max_speed.max(cfg.move_verify_fall_speed);
        }
        let in_skill_grace = snap.displaced_until_ms >= now_ms
            || (snap.skill_last_used_ms > 0
                && now_ms - snap.skill_last_used_ms >= 0
                && now_ms - snap.skill_last_used_ms <= cfg.move_verify_skill_grace_ms);
        if in_skill_grace {
            max_speed = max_speed.max(cfg.move_verify_skill_speed);
        }
        max_speed
    }
}

/// The stance speed of an actor model. A player may not claim the fly stance without a glider
/// buff; mates keep the model's own fly speed (flying mounts).
fn stance_speed(
    server: &GameServer,
    model_id: u32,
    stance: GameStanceType,
    player: bool,
) -> Option<f32> {
    let model = server.game_data.systems.ai.ai.actor_model(model_id)?;
    if player && stance == GameStanceType::FLY {
        return model
            .stance(GameStanceType::RELAXED)
            .or_else(|| model.stance(GameStanceType::COMBAT))
            .map(|s| s.max_speed)
            .filter(|v| *v > 0.0);
    }
    model
        .stance(stance)
        .map(|s| s.max_speed)
        .filter(|v| *v > 0.0)
}

/// The fastest glider speed of the active gliding buffs (m/s).
fn glider_speed(server: &GameServer, buff_ids: &[u32]) -> Option<f32> {
    buff_ids
        .iter()
        .filter_map(|id| server.game_data.systems.skills.buff(*id))
        .filter(|t| t.gliding)
        .map(|t| {
            t.gliding_move_speed_fast
                .max(t.gliding_move_speed_normal)
                .max(t.gliding_move_speed_slow)
        })
        .filter(|v| *v > 0.0)
        .reduce(f32::max)
}

fn snapshot(target: &WorldObject, now_ms: i64) -> Option<UnitSnapshot> {
    target.with_unit(|d| {
        let u = d.unit();
        UnitSnapshot {
            model_id: u.model_id,
            speed_mul: (u.calculate_with_bonuses(1000.0, UnitAttribute::MoveSpeedMul, now_ms)
                as f32)
                / 1000.0,
            x: u.transform.x,
            y: u.transform.y,
            z: u.transform.z,
            instance_id: u.transform.instance_id,
            parent_obj_id: u.transform.parent_obj_id,
            buff_ids: d.buffs().effects.iter().map(|e| e.template.id).collect(),
            skill_last_used_ms: u.skill_last_used_ms,
            displaced_until_ms: u
                .displaced_until_ms
                .unwrap_or(0)
                .max(u.skill_state.displaced_until_ms),
        }
    })
}

fn distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(x: f32, z: f32, ms: i64, terrain: f32) -> MoveSample {
        MoveSample {
            x,
            y: 0.0,
            z,
            client_ms: ms,
            server_ms: ms,
            terrain_z: terrain,
            exempt: false,
        }
    }

    #[test]
    fn teleport_classes() {
        assert_eq!(teleport_class(60.0, 0.2), Some((15, 90)));
        assert_eq!(teleport_class(30.0, 0.2), Some((16, 80)));
        assert_eq!(teleport_class(12.0, 0.2), Some((17, 50)));
        assert_eq!(teleport_class(8.0, 0.1), None);
        assert_eq!(teleport_class(5.0, 1.0), None);
    }

    #[test]
    fn verifier_reports() {
        let v = MoveVerifier::default();
        assert!(v.verify(1, s(0.0, 100.0, 1000, 99.0)).is_empty());
        let r = v.verify(1, s(100.0, 100.0, 1200, 99.0));
        assert_eq!(r[0].reason, 15);
        // within 5 s: no second teleport report
        assert!(v.verify(1, s(200.0, 100.0, 1400, 99.0)).is_empty());
        let r = v.verify(1, s(200.0, 100.0, 1300, 99.0));
        assert_eq!(r[0].reason, 0);
        // under terrain + below min z over a sample window
        let v = MoveVerifier::default();
        let mut all = Vec::new();
        for i in 0..SAMPLE_MAX {
            all.extend(v.verify(2, s(0.0, -2.0, 1000 + i64::from(i) * 100, 5.0)));
        }
        assert!(all.iter().any(|r| r.reason == 11 && r.reliability == 80));
        assert!(all.iter().any(|r| r.reason == 12 && r.reliability == 100));
        assert!(v
            .verify_vehicle(3, (0.0, 0.0, 0.0), (600.0, 0.0, 0.0))
            .is_some());
        assert!(v
            .verify_vehicle(3, (0.0, 0.0, 0.0), (400.0, 0.0, 0.0))
            .is_none());
    }
}
