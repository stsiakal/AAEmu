//! The phase-0 bot walker: a straight line at the player run speed, broadcast as the same
//! `SCOneUnitMovementPacket` a client sends (through [`crate::world::session::apply_unit_move`]).

use std::sync::Arc;

use crate::models::movement::{
    GameStanceType, MoveKind, MoveType, MoveTypeActorFlags, MoveTypeAlertness, MoveTypeFlags,
    UnitMove,
};
use crate::server::GameServer;
use crate::systems::ai::npc::ms_of_day;
use crate::systems::skills::controllers::roll_pitch_yaw_sbytes_movement;
use crate::world::session;
use crate::world::{PlayerRef, WorldObject};

use super::Bot;

/// The 1.2 Nuian run speed (`game_stances.max_speed`), m/s.
pub const RUN_SPEED_MPS: f32 = 5.4;
/// The client's movement rate (~10 Hz).
pub const STEP_MS: u64 = 100;
/// Close enough to the target to stop.
pub const STOP_DISTANCE: f32 = 0.35;

/// Starts walking `bot` to `(x, y)` (replacing a running walk); the task stops on arrival,
/// death or removal.
pub fn start(server: &Arc<GameServer>, bot: &Arc<Bot>, x: f32, y: f32) {
    bot.cancel_walk();
    let server = server.clone();
    let player = bot.player.clone();
    let target = (x, y);
    let task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(STEP_MS));
        loop {
            tick.tick().await;
            let (hp, online) = {
                let c = player.data();
                (c.unit.hp, c.is_online)
            };
            if hp == 0 || !online {
                break;
            }
            if walk_step(&server, &player, target) {
                break;
            }
        }
    });
    bot.set_walk_task(task);
}

/// One walker step towards `target`; returns `true` when the bot has arrived (a final stopping
/// movement is broadcast in that case).
pub fn walk_step(server: &GameServer, player: &PlayerRef, target: (f32, f32)) -> bool {
    let (x, y, z, instance_id, parent, rot_z, disabled) = {
        let c = player.data();
        let t = c.unit.transform;
        (
            t.x,
            t.y,
            t.z,
            t.instance_id,
            t.parent_obj_id,
            t.rot_z,
            c.disabled_set_position,
        )
    };
    if disabled {
        // Leaving the world (see `session::leave_world_player`): stop before touching the grid.
        return true;
    }
    if parent != 0 {
        // Standing on a ship / mate: a ground walker has no local frame here (phase 0).
        return true;
    }
    let (dx, dy) = (target.0 - x, target.1 - y);
    let distance = (dx * dx + dy * dy).sqrt();
    if distance <= STOP_DISTANCE {
        stop(server, player, (x, y, z), rot_z);
        return true;
    }
    let step = (RUN_SPEED_MPS * STEP_MS as f32 / 1000.0).min(distance);
    let nx = x + dx / distance * step;
    let ny = y + dy / distance * step;
    // Follow the terrain when the collision service has it; otherwise keep the current height.
    let nz = crate::systems::ai::collision::ground_height_or(server, instance_id, nx, ny, z, || z);
    let angle = f64::from(dy).atan2(f64::from(dx));
    let (_, _, rotation_z) =
        roll_pitch_yaw_sbytes_movement(0.0, 0.0, (angle - std::f64::consts::FRAC_PI_2) as f32);
    let velocity = f64::from(RUN_SPEED_MPS) * 1000.0;
    let unit = UnitMove {
        x: nx,
        y: ny,
        z: nz,
        vel_x: (velocity * angle.cos()) as i16,
        vel_y: (velocity * angle.sin()) as i16,
        rotation_z,
        stance: GameStanceType::COMBAT,
        alertness: MoveTypeAlertness::IDLE,
        actor_flags: MoveTypeActorFlags::FLAG2,
        ..UnitMove::default()
    };
    let mut mt = MoveType::new(MoveKind::Unit(unit));
    mt.flags = MoveTypeFlags::MOVING;
    mt.time = ms_of_day();
    session::apply_unit_move(server, player, &WorldObject::from(player), &mt);
    false
}

/// The stopping movement at the current position (`MoveTypeFlags.Stopping`, no velocity).
fn stop(server: &GameServer, player: &PlayerRef, (x, y, z): (f32, f32, f32), rot_z: f32) {
    let (_, _, rotation_z) = roll_pitch_yaw_sbytes_movement(0.0, 0.0, rot_z);
    let unit = UnitMove {
        x,
        y,
        z,
        rotation_z,
        stance: GameStanceType::COMBAT,
        alertness: MoveTypeAlertness::IDLE,
        actor_flags: MoveTypeActorFlags::FLAG2,
        ..UnitMove::default()
    };
    let mut mt = MoveType::new(MoveKind::Unit(unit));
    mt.flags = MoveTypeFlags::STOPPING;
    mt.time = ms_of_day();
    session::apply_unit_move(server, player, &WorldObject::from(player), &mt);
}
