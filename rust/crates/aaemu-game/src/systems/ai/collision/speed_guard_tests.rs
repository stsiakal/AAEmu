//! Movement-guard flow tests (`Physics.MoveVerify*`): the real actor-model speeds (run 5.4 m/s,
//! mount 9 m/s, glider 16 m/s) pass, twice the speed is refused, server teleports (GM / portal)
//! are whitelisted and a client-side teleport is refused and corrected.

use std::sync::Arc;

use crate::data::skills::BuffKind;
use crate::data::GameData;
use crate::models::buff::{Buff, SkillCaster};
use crate::models::mate::Mate;
use crate::models::movement::{GameStanceType, MoveKind, MoveType, UnitMove};
use crate::models::npc::{Npc, PortalInfo};
use crate::models::slave::Slave;
use crate::models::transform::Transform;
use crate::models::unit::{Bonus, BonusTemplate, UnitAttribute, UnitModifierType};
use crate::net::registry::ClientPacket;
use crate::systems::ai::collision::move_verify::MoveDecision;
use crate::systems::ai::data::{ActorModel, GameStance};
use crate::systems::npcs::npc::data::NpcTemplate;
use crate::systems::npcs::portal::{
    packets::CSTeleportEndedPacket, use_portal, PORTAL_EXIT_TEMPLATE_ID,
};
use crate::systems::vehicles::data::VehicleModel;
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient, TEST_ZONE};
use crate::world::{Entity, PlayerRef, WorldObject};

/// The Nuian actor model 10: `game_stances.max_speed` 5.4 (combat / relaxed), 3.0 swim, 5.4 fly.
const PLAYER_MODEL: u32 = 10;
/// A mount actor model (`game_stances.max_speed` 9.0).
const MOUNT_MODEL: u32 = 77;
/// A land vehicle (`vehicle_models.velocity` 10.0).
const VEHICLE_MODEL: u32 = 300;
/// The glider buff (the 1.2 rows carry 6 / 8 / 16 m/s).
const GLIDER_BUFF: u32 = 5000;

fn stance(stance_id: GameStanceType, max_speed: f32) -> (GameStanceType, GameStance) {
    (
        stance_id,
        GameStance {
            id: u32::from(stance_id.0) + 1,
            stance_id,
            actor_model_id: PLAYER_MODEL,
            ai_move_speed_run: max_speed,
            ai_move_speed_slow: max_speed,
            ai_move_speed_sprint: max_speed,
            ai_move_speed_walk: max_speed,
            height_collider: 0.0,
            height_pivot: 0.0,
            normal_speed: max_speed,
            max_speed,
            size: [0.0; 3],
            model_offset: [0.0; 3],
            view_offset: [0.0; 3],
            use_capsule: true,
        },
    )
}

fn model(id: u32, max_speed: f32, fly_speed: f32) -> Arc<ActorModel> {
    Arc::new(ActorModel {
        id,
        radius: 0.5,
        height: 2.0,
        movement_id: 0,
        underwater_creature: false,
        stances: vec![
            stance(GameStanceType::COMBAT, max_speed),
            stance(GameStanceType::RELAXED, max_speed),
            stance(GameStanceType::SWIM, 3.0),
            stance(GameStanceType::FLY, fly_speed),
        ],
    })
}

fn data() -> GameData {
    let mut d = GameData::empty();
    d.systems
        .ai
        .ai
        .actor_models
        .insert(PLAYER_MODEL, model(PLAYER_MODEL, 5.4, 5.4));
    d.systems
        .ai
        .ai
        .actor_models
        .insert(MOUNT_MODEL, model(MOUNT_MODEL, 9.0, 9.0));
    d.systems.npcs.npc.templates.insert(
        PORTAL_EXIT_TEMPLATE_ID,
        NpcTemplate {
            id: PORTAL_EXIT_TEMPLATE_ID,
            level: 1,
            model_id: 7000,
            ..NpcTemplate::default()
        },
    );
    d.systems
        .vehicles
        .model_types
        .insert(VEHICLE_MODEL, (1, "VehicleModel".into()));
    d.systems.vehicles.vehicle_models.insert(
        1,
        VehicleModel {
            id: 1,
            velocity: 10.0,
            ..VehicleModel::default()
        },
    );
    d.systems
        .skills
        .add_buff(crate::systems::skills::data::BuffTemplate {
            id: GLIDER_BUFF,
            gliding: true,
            gliding_move_speed_slow: 6.0,
            gliding_move_speed_normal: 8.0,
            gliding_move_speed_fast: 16.0,
            ..Default::default()
        });
    d
}

fn fixture() -> (crate::GameServer, TestClient, PlayerRef) {
    let server = test_server_with(data());
    let mut client = TestClient::new(&server, 1, 1);
    let mut c = test_character(1, "Runner", 100.0, 100.0);
    c.unit.model_id = PLAYER_MODEL;
    // Outside the range `server.ids.object` hands out (the portal fixture below allocates one).
    let p = spawn_player(&server, &client, c, 0x00F0_0001);
    client.conn.active_char = Some(p.clone());
    client.drain();
    (server, client, p)
}

fn walker(x: f32, stance: GameStanceType) -> UnitMove {
    UnitMove {
        x,
        y: 100.0,
        z: 100.0,
        stance,
        ..Default::default()
    }
}

fn guard(
    server: &crate::GameServer,
) -> &crate::systems::ai::collision::move_verify::MoveSpeedGuard {
    &server.systems.ai.collision.move_speed
}

/// Feeds `steps` moves of `speed` m/s (100 ms apart) starting at `x`; returns the first refused
/// decision, if any.
fn feed(
    server: &crate::GameServer,
    target: &WorldObject,
    stance: GameStanceType,
    start_x: f32,
    speed: f32,
    start_ms: i64,
    steps: u32,
) -> Option<MoveDecision> {
    let g = guard(server);
    let mut x = start_x;
    let mut now = start_ms;
    let mut refused = None;
    // The first sample anchors at the target's current transform.
    let d = g.check_unit_at(server, target, &walker(x, stance), now);
    assert_eq!(d, MoveDecision::Accept, "first sample");
    for _ in 0..steps {
        now += 100;
        x += speed * 0.1;
        let d = g.check_unit_at(server, target, &walker(x, stance), now);
        if !matches!(d, MoveDecision::Accept) {
            refused = Some(d);
            break;
        }
    }
    refused
}

#[tokio::test]
async fn walking_running_and_sprinting_at_max_speed_pass() {
    let (server, _client, p) = fixture();
    let obj = WorldObject::from(&p);
    assert_eq!(
        feed(
            &server,
            &obj,
            GameStanceType::RELAXED,
            100.0,
            5.4,
            1_000,
            60
        ),
        None,
        "run speed 5.4 m/s (game_stances.max_speed)"
    );

    let (server, _client, p) = fixture();
    let obj = WorldObject::from(&p);
    // Sprint: MoveSpeedMul +1000 (the 1.2 movement buffs, e.g. buff 110).
    p.data().unit.bonuses.add_bonus(
        900,
        Bonus {
            template: BonusTemplate {
                attribute: UnitAttribute::MoveSpeedMul,
                modifier_type: UnitModifierType::Value,
                value: 1000,
                linear_level_bonus: 0,
            },
            value: 1000,
        },
    );
    assert_eq!(
        feed(
            &server,
            &obj,
            GameStanceType::COMBAT,
            100.0,
            10.8,
            1_000,
            60
        ),
        None,
        "sprint speed 5.4 * 2"
    );
}

#[tokio::test]
async fn mounted_and_gliding_at_max_speed_pass() {
    let (server, _client, p) = fixture();
    let world = server.world.world(0).unwrap();
    let mut mate = Mate::new(1, 1);
    mate.owner_obj_id = p.obj_id;
    mate.unit.model_id = MOUNT_MODEL;
    mate.unit.transform.set_xyz(100.0, 100.0, 100.0);
    mate.unit.transform.zone_id = TEST_ZONE;
    let mate = Entity::new(0x300, mate);
    world.spawn(&mate, &server.view_ctx());
    let obj = WorldObject::from(&mate);
    assert_eq!(
        feed(
            &server,
            &obj,
            GameStanceType::RELAXED,
            100.0,
            9.0,
            1_000,
            60
        ),
        None,
        "mount speed 9.0 m/s (game_stances of the mate model)"
    );

    let (server, _client, p) = fixture();
    let obj = WorldObject::from(&p);
    let t = crate::data::skills::BuffTemplate::simple(GLIDER_BUFF, BuffKind::Good);
    p.data()
        .unit
        .buffs
        .add_buff(Buff::new(t, SkillCaster::default(), None, 0), 0, 0, 0);
    assert_eq!(
        feed(&server, &obj, GameStanceType::FLY, 100.0, 16.0, 1_000, 60),
        None,
        "glider fast speed 16 m/s (buffs.gliding_move_speed_fast)"
    );
}

#[tokio::test]
async fn double_speed_is_refused() {
    let (server, _client, p) = fixture();
    let obj = WorldObject::from(&p);
    let d = feed(
        &server,
        &obj,
        GameStanceType::RELAXED,
        100.0,
        10.8,
        1_000,
        60,
    );
    match d {
        Some(MoveDecision::Correct(reason)) => {
            assert!(reason.contains("m/s"), "{reason}");
            assert!(reason.contains("max 5.4 m/s"), "{reason}");
        }
        other => panic!("2x speed not refused: {other:?}"),
    }
}

#[tokio::test]
async fn client_teleport_is_refused_and_corrected() {
    let (server, mut client, p) = fixture();
    let me = WorldObject::from(&p);
    let mt = MoveType::new(MoveKind::Unit(walker(400.0, GameStanceType::RELAXED)));
    crate::world::session::apply_unit_move(&server, &p, &me, &mt);

    let t = p.data().unit.transform;
    assert_eq!((t.x, t.y, t.z), (100.0, 100.0, 100.0), "move refused");
    assert!(p.data().disabled_set_position, "sent back");
    assert_eq!(server.systems.services.sus.move_violations(), 1);
    let frames = client.drain();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].0, crate::opcodes::sc::SCTeleportUnitPacket);
    let mut body = aaemu_commons::PacketStream::new();
    body.write_u8(crate::systems::skills::packets_special::TELEPORT_REASON_MOVE_TO_LOCATION)
        .write_i16(0)
        .write_position(100.0, 100.0, 100.0)
        .write_f32(0.0);
    assert_eq!(frames[0].1, body.into_bytes());

    // The client acknowledges at the last valid position; normal movement resumes there.
    CSTeleportEndedPacket {
        x: 100.0,
        y: 100.0,
        z: 100.0,
        ori: [0; 16],
    }
    .handle(&server, &mut client.conn)
    .await
    .unwrap();
    assert!(!p.data().disabled_set_position);
    let mt = MoveType::new(MoveKind::Unit(walker(100.54, GameStanceType::RELAXED)));
    crate::world::session::apply_unit_move(&server, &p, &me, &mt);
    assert_eq!(p.data().unit.transform.x, 100.54);
    assert_eq!(server.systems.services.sus.move_violations(), 1);
}

#[tokio::test]
async fn gm_move_and_portal_teleports_pass() {
    let (server, mut client, p) = fixture();
    // GM /move: teleport_player is the shared helper of /move, /teleport, /nudge, ...
    crate::systems::services::commands::scripts::util::teleport_player(
        &p, 12, 500.0, 600.0, 70.0, 0.0,
    );
    assert!(p.data().disabled_set_position);
    assert!(p.data().expected_position.is_some());
    CSTeleportEndedPacket {
        x: 500.0,
        y: 600.0,
        z: 70.0,
        ori: [0; 16],
    }
    .handle(&server, &mut client.conn)
    .await
    .unwrap();
    let t = p.data().unit.transform;
    assert!(!p.data().disabled_set_position);
    assert_eq!((t.x, t.y, t.z), (500.0, 600.0, 70.0));
    let me = WorldObject::from(&p);
    let mt = MoveType::new(MoveKind::Unit(UnitMove {
        x: 500.54,
        y: 600.0,
        z: 70.0,
        stance: GameStanceType::RELAXED,
        ..Default::default()
    }));
    crate::world::session::apply_unit_move(&server, &p, &me, &mt);
    assert_eq!(p.data().unit.transform.x, 500.54);
    client.drain();
    // Portal (same instance): the gate sends the destination and the client confirms it.
    let mut npc = Npc::new(PORTAL_EXIT_TEMPLATE_ID);
    npc.unit.transform.set_xyz(101.0, 100.0, 100.0);
    npc.unit.transform.zone_id = TEST_ZONE;
    npc.unit.hp = 1;
    let mut tp = Transform {
        zone_id: TEST_ZONE,
        ..Transform::default()
    };
    tp.set_position(900.0, 800.0, 60.0, 0.0, 0.0, 90.0);
    npc.portal = Some(PortalInfo {
        teleport_position: tp,
        linked_portal_obj_id: 0,
    });
    let npc = Entity::new(server.ids.object.get_next_id().unwrap(), npc);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&npc, &server.view_ctx());
    let world = server.world.world(0).unwrap();
    let ids: Vec<u32> = world.all_npcs().iter().map(|n| n.obj_id).collect();
    assert!(
        world.npc(npc.obj_id).is_some(),
        "npc {} in world (world npcs {:?})",
        npc.obj_id,
        ids
    );
    assert!(
        world.npc(npc.obj_id).unwrap().data().portal.is_some(),
        "portal info"
    );
    use_portal(&server, &p, npc.obj_id);
    assert!(p.data().disabled_set_position, "portal teleport");
    CSTeleportEndedPacket {
        x: 900.0,
        y: 800.0,
        z: 60.0,
        ori: [0; 16],
    }
    .handle(&server, &mut client.conn)
    .await
    .unwrap();
    assert!(!p.data().disabled_set_position);
    let mt = MoveType::new(MoveKind::Unit(UnitMove {
        x: 900.54,
        y: 800.0,
        z: 60.0,
        stance: GameStanceType::RELAXED,
        ..Default::default()
    }));
    crate::world::session::apply_unit_move(&server, &p, &me, &mt);
    assert_eq!(p.data().unit.transform.x, 900.54);
    assert_eq!(server.systems.services.sus.move_violations(), 0);
}

#[tokio::test]
async fn vehicle_uses_the_slave_model_speed() {
    let (server, _client, p) = fixture();
    let world = server.world.world(0).unwrap();
    let mut car = Slave::new(1, 1);
    car.unit.model_id = VEHICLE_MODEL;
    car.unit.transform.set_xyz(100.0, 100.0, 100.0);
    car.summoner_obj_id = p.obj_id;
    let car = Entity::new(0x300, car);
    world.spawn(&car, &server.view_ctx());
    let g = guard(&server);
    let mut now = 1_000;
    assert_eq!(
        g.check_vehicle_at(&server, &car, [100.0, 100.0, 100.0], now),
        MoveDecision::Accept
    );
    let mut x = 100.0;
    for _ in 0..60 {
        now += 100;
        x += 1.0; // 10 m/s
        assert_eq!(
            g.check_vehicle_at(&server, &car, [x, 100.0, 100.0], now),
            MoveDecision::Accept
        );
    }
    // Twice the model speed no longer fits the travel budget (re-seed at the current position).
    car.data().unit.transform.set_xyz(x, 100.0, 100.0);
    g.reset(car.obj_id);
    assert_eq!(
        g.check_vehicle_at(&server, &car, [x, 100.0, 100.0], now),
        MoveDecision::Accept
    );
    let mut refused = None;
    for _ in 0..60 {
        now += 100;
        x += 2.0; // 20 m/s
        let d = g.check_vehicle_at(&server, &car, [x, 100.0, 100.0], now);
        if !matches!(d, MoveDecision::Accept) {
            refused = Some(d);
            break;
        }
    }
    assert!(
        matches!(refused, Some(MoveDecision::Refuse(_))),
        "{refused:?}"
    );
}

#[tokio::test]
async fn gm_exemption_switch_and_generous_defaults() {
    let (server, _client, p) = fixture();
    p.data().access_level = 100;
    let obj = WorldObject::from(&p);
    // The GM exemption is on by default.
    let g = guard(&server);
    let d = g.check_unit_at(
        &server,
        &obj,
        &walker(400.0, GameStanceType::RELAXED),
        1_000,
    );
    assert_eq!(d, MoveDecision::Accept);
    // Switched off: the same move is a client-side teleport.
    server
        .config
        .update(|c| c.physics.move_verify_gm_exempt = false);
    g.reset(p.obj_id);
    let d = g.check_unit_at(
        &server,
        &obj,
        &walker(400.0, GameStanceType::RELAXED),
        2_000,
    );
    assert!(matches!(d, MoveDecision::Correct(_)), "{d:?}");
}
