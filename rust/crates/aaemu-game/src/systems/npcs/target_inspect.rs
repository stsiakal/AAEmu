//! `[target-inspect]` GM target logging, plus the NPC/AI state gathering shared with the
//! `testai info` command.
//!
//! The log block is deliberately built from the same [`ai_info`] / [`spawner_info`] functions
//! the command uses, so the two views cannot drift. Nothing here holds an object data lock
//! while calling into the world: all object state is copied out first (CONTRIBUTING-PORT.md
//! § "Locking rules").

use crate::data::world_templates::WorldSpawnPosition;
use crate::models::movement::{GameStanceType, MoveTypeAlertness};
use crate::server::GameServer;
use crate::systems::ai::framework::{AiKind, BehaviorKind};
use crate::systems::ai::manager::run_now;
use crate::systems::ai::params::{AiCommandCategory, AiCommands};
use crate::systems::skills::controllers::{calculate_distance, Vec3};
use crate::world::{NpcRef, WorldObject};

/// Prefix of every target-inspection log line.
pub const LOG_PREFIX: &str = "[target-inspect]";

/// `GameStanceType` member names (value = index).
const GAME_STANCE_NAMES: [&str; 9] = [
    "Combat", "Relaxed", "Swim", "CoSwim", "Combat2", "Stealth", "Crouch", "Prone", "Fly",
];

/// The full type name of the behavior class of a kind (`behavior.ToString()`).
pub fn behavior_type_name(kind: BehaviorKind) -> String {
    let folder = match kind {
        BehaviorKind::ArcherAttack => "Archer",
        BehaviorKind::BigMonsterAttack => "BigMonster",
        BehaviorKind::FlytrapAlert | BehaviorKind::FlytrapAttack => "Flytrap",
        BehaviorKind::WildBoarAttack => "WildBoar",
        _ => "Common",
    };
    format!("AAEmu.Game.Models.Game.AI.v2.Behaviors.{folder}.{kind:?}Behavior")
}

/// The AI state `testai info` prints and the target log adds.
#[derive(Debug, Clone)]
pub struct AiInfo {
    pub kind: AiKind,
    pub current_behavior: Option<BehaviorKind>,
    pub path_points: usize,
    pub path_remaining: usize,
    pub path_looping: bool,
    pub path_speed: f32,
    pub path_actor_flags: u8,
    pub path_target: Vec3,
    pub commands_in_queue: usize,
    pub current_command: Option<AiCommands>,
    pub commands: Vec<AiCommands>,
    pub follow_unit: Option<WorldObject>,
    pub ai_file_name: String,
    pub ai_file_name2: String,
    pub idle_position: Vec3,
    pub home_position: Vec3,
    pub already_targeted: bool,
    pub alert_end_time: i64,
}

/// Reads an AI's state (`None` = no AI attached).
pub fn ai_info(server: &GameServer, obj_id: u32) -> Option<AiInfo> {
    run_now(server, obj_id, |c| AiInfo {
        kind: c.ai.kind,
        current_behavior: c.ai.current_behavior(),
        path_points: c.ai.path_handler.ai_path_points.len(),
        path_remaining: c.ai.path_handler.ai_path_points_remaining.len(),
        path_looping: c.ai.path_handler.ai_path_looping,
        path_speed: c.ai.path_handler.ai_path_speed,
        path_actor_flags: c.ai.path_handler.ai_path_actor_flags,
        path_target: c.ai.path_handler.target_position,
        commands_in_queue: c.ai.commands_queue.len(),
        current_command: c.ai.current_command.clone(),
        commands: c.ai.commands_queue.iter().cloned().collect(),
        follow_unit: c.ai.follow_unit.clone(),
        ai_file_name: c.ai.ai_file_name.clone(),
        ai_file_name2: c.ai.ai_file_name2.clone(),
        idle_position: c.ai.idle_position,
        home_position: c.ai.home_position,
        already_targeted: c.ai.already_targeted,
        alert_end_time: c.ai.alert_end_time,
    })
}

/// The spawner data `testai info` prints.
#[derive(Debug, Clone)]
pub struct SpawnerInfo {
    pub id: u32,
    /// `spawner.template().Id` (`None` = the spawner has no template).
    pub template_id: Option<u32>,
    pub position: WorldSpawnPosition,
    pub follow_path: String,
    pub follow_npc: u32,
    pub parent_world: Option<u32>,
    /// The NPC template the spawner entry spawns.
    pub unit_id: u32,
}

/// Reads an NPC's spawner (`None` = the NPC was not spawned by a spawner).
pub fn spawner_info(server: &GameServer, obj_id: u32) -> Option<SpawnerInfo> {
    let spawner = server.systems.npcs.spawns.npc_spawner_of(obj_id)?;
    Some(SpawnerInfo {
        id: spawner.id,
        template_id: spawner.template().map(|t| t.id),
        position: spawner.position(),
        follow_path: spawner.follow_path.clone(),
        follow_npc: spawner.follow_npc,
        parent_world: spawner.parent_world(),
        unit_id: spawner.unit_id,
    })
}

/// The `[target-inspect]` block for `target`, or `None` when the switch is off or the caller is
/// not a GM. The caller resolves the GM check (`systems::services::character::is_gm`).
pub fn target_inspect_log(
    server: &GameServer,
    is_gm: bool,
    gm_name: &str,
    target: Option<&WorldObject>,
) -> Option<String> {
    if !is_gm || !server.config.world.log_gm_targets {
        return None;
    }
    let target = target?;
    Some(describe_target(server, gm_name, target))
}

/// The complete block for one target (no gating): NPCs get every field, other object kinds a
/// short one-line summary.
pub fn describe_target(server: &GameServer, gm_name: &str, target: &WorldObject) -> String {
    match target.as_npc() {
        Some(npc) => describe_npc(server, gm_name, npc),
        None => describe_other(server, gm_name, target),
    }
}

fn describe_npc(server: &GameServer, gm_name: &str, npc: &NpcRef) -> String {
    let obj_id = npc.obj_id;
    let (
        name,
        template_id,
        model_id,
        faction_id,
        level,
        hp,
        max_hp,
        mp,
        max_mp,
        stance,
        alertness,
        aggro_target,
        target_id,
        buffs,
    ) = {
        let n = npc.data();
        (
            n.unit.name.clone(),
            n.unit.template_id,
            n.unit.model_id,
            n.unit.faction_id,
            n.unit.level,
            n.unit.hp,
            n.unit.max_hp,
            n.unit.mp,
            n.unit.max_mp,
            n.current_game_stance,
            n.current_alertness,
            n.current_aggro_target,
            n.unit.current_target,
            n.unit
                .buffs
                .effects
                .iter()
                .map(|e| e.template.id)
                .collect::<Vec<_>>(),
        )
    };

    let template_name = server
        .game_data
        .systems
        .npcs
        .npc
        .template(template_id)
        .map(|t| t.name.clone())
        .unwrap_or_default();
    let name = if name.is_empty() { template_name } else { name };
    let pos = world_position(server, &WorldObject::from(npc));
    let ai = ai_info(server, obj_id);
    let spawner = spawner_info(server, obj_id);
    let spawn_pos = spawner.as_ref().map(|s| s.position);

    let mut out = String::new();
    let mut line = |text: String| {
        out.push_str(LOG_PREFIX);
        out.push(' ');
        out.push_str(&text);
        out.push('\n');
    };

    line(format!(
        "gm={gm_name:?} target=Npc obj={obj_id} name={name:?} template={template_id} \
         model={model_id} faction={faction_id} level={level} hp={hp}/{max_hp} mp={mp}/{max_mp}"
    ));
    let instance = npc.data().unit.transform.instance_id;
    line(match spawn_pos {
        Some(s) => format!(
            "position={} instance={instance} spawn={} distance_from_spawn={:.2}",
            fmt_pos(pos),
            fmt_pos((s.x, s.y, s.z)),
            calculate_distance(
                Vec3::new(pos.0, pos.1, pos.2),
                Vec3::new(s.x, s.y, s.z),
                true
            )
        ),
        None => format!(
            "position={} instance={instance} spawn=unknown",
            fmt_pos(pos)
        ),
    });
    match &spawner {
        Some(s) => line(format!(
            "spawner=id {} template {} world {:?} spawn_pos={} follow_path={:?} \
             follow_npc={} entry_npc={}",
            s.id,
            s.template_id
                .map_or_else(|| "unknown".to_string(), |id| id.to_string()),
            s.parent_world,
            fmt_pos((s.position.x, s.position.y, s.position.z)),
            s.follow_path,
            s.follow_npc,
            s.unit_id
        )),
        None => line("spawner=none".into()),
    }
    match &ai {
        Some(ai) => {
            let current = ai
                .current_behavior
                .map(behavior_type_name)
                .unwrap_or_else(|| "none".into());
            line(format!(
                "ai={} class={} current={current} stance={}({}) alertness={}({}) \
                 ai_file={:?} ai_file2={:?} already_targeted={} alert_end={}",
                ai.kind.class_name().replace("AiCharacter", ""),
                ai.kind.class_name(),
                stance_name(stance),
                stance.0,
                alertness_name(alertness),
                alertness.0,
                ai.ai_file_name,
                ai.ai_file_name2,
                ai.already_targeted,
                ai.alert_end_time
            ));
            let path_name = spawner
                .as_ref()
                .map(|s| s.follow_path.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("none");
            line(format!(
                "path name={path_name:?} points={} remaining={} looping={} speed={} \
                 flags={} target={}",
                ai.path_points,
                ai.path_remaining,
                ai.path_looping,
                ai.path_speed,
                ai.path_actor_flags,
                fmt_pos((ai.path_target.x, ai.path_target.y, ai.path_target.z))
            ));
            let current = ai
                .current_command
                .as_ref()
                .map(command_summary)
                .unwrap_or_else(|| "none".into());
            let mut queued: Vec<String> = ai.commands.iter().take(8).map(command_summary).collect();
            if ai.commands.len() > queued.len() {
                queued.push(format!("+{} more", ai.commands.len() - queued.len()));
            }
            line(format!(
                "commands current={current:?} queue=[{}] ({} queued)",
                queued.join(", "),
                ai.commands_in_queue
            ));
            line(format!(
                "follow_obj={:?} idle={} home={} moved_from_home={}",
                ai.follow_unit.as_ref().map(WorldObject::obj_id),
                fmt_pos((ai.idle_position.x, ai.idle_position.y, ai.idle_position.z)),
                fmt_pos((ai.home_position.x, ai.home_position.y, ai.home_position.z)),
                calculate_distance(ai.home_position, Vec3::new(pos.0, pos.1, pos.2), true) > 0.5
            ));
        }
        None => line("ai=none".into()),
    }
    line(format!(
        "aggro={aggro_target} target={target_id} buffs=[{}]",
        format_ids(&buffs)
    ));
    line(
        "movement=no last-move timestamp is tracked; position changes show in distance_from_spawn"
            .into(),
    );
    out
}

fn describe_other(server: &GameServer, gm_name: &str, target: &WorldObject) -> String {
    let (name, template_id, level, hp, max_hp) = target
        .with_unit(|u| {
            let b = u.unit();
            (b.name.clone(), b.template_id, b.level, b.hp, b.max_hp)
        })
        .unwrap_or_else(|| {
            target.with_base_unit(|b| (b.name().to_string(), b.template_id(), 0, 0, 0))
        });
    format!(
        "{LOG_PREFIX} gm={gm_name:?} target={:?} obj={} name={name:?} template={template_id} \
         level={level} hp={hp}/{max_hp} position={}",
        target.kind(),
        target.obj_id(),
        fmt_pos(world_position(server, target))
    )
}

fn command_summary(c: &AiCommands) -> String {
    match c.cmd_id {
        AiCommandCategory::FollowUnit => format!("FollowUnit {}", c.param1),
        AiCommandCategory::FollowPath => {
            if c.param1 == 1 {
                format!("FollowPath {} (return)", c.param2)
            } else {
                format!("FollowPath {}", c.param2)
            }
        }
        AiCommandCategory::UseSkill => format!("UseSkill {}", c.param1),
        AiCommandCategory::Timeout => format!("Timeout {}", c.param1),
        AiCommandCategory::Other(v) => format!("Other({v}) p1={} p2={}", c.param1, c.param2),
    }
}

fn stance_name(stance: GameStanceType) -> &'static str {
    GAME_STANCE_NAMES
        .get(usize::from(stance.0))
        .copied()
        .unwrap_or("Unknown")
}

fn alertness_name(alertness: MoveTypeAlertness) -> &'static str {
    match alertness {
        MoveTypeAlertness::IDLE => "Idle",
        MoveTypeAlertness::ALERT => "Alert",
        MoveTypeAlertness::COMBAT => "Combat",
        _ => "Unknown",
    }
}

fn format_ids(ids: &[u32]) -> String {
    let shown: Vec<String> = ids.iter().take(32).map(u32::to_string).collect();
    if ids.len() > shown.len() {
        format!("{} +{} more", shown.join(", "), ids.len() - shown.len())
    } else {
        shown.join(", ")
    }
}

fn fmt_pos(p: (f32, f32, f32)) -> String {
    format!("({:.2}, {:.2}, {:.2})", p.0, p.1, p.2)
}

fn world_position(server: &GameServer, obj: &WorldObject) -> (f32, f32, f32) {
    match server.world.world_of(obj) {
        Some(world) => {
            let p = world.world_pos_rot(obj);
            (p.x, p.y, p.z)
        }
        None => {
            let t = obj.transform();
            (t.x, t.y, t.z)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::npc::Npc;
    use crate::net::registry::ClientPacket;
    use crate::systems::ai::manager::run_now;
    use crate::systems::ai::params::AiCommands;
    use crate::systems::ai::path_handler::{AiPathPoint, AiPathPointAction};
    use crate::systems::npcs::npc::spawner::{NpcSpawner, NpcSpawnerInit, NpcSpawnerShared};
    use crate::systems::npcs::npc::spawner_data::NpcSpawnerTemplate;
    use crate::systems::services::commands::scripts::harness;
    use crate::systems::skills::controllers::Vec3;
    use crate::world::Entity;
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Metadata, Subscriber};

    const NPC_OBJ: u32 = 0x900;

    fn command(cmd_id: AiCommandCategory, param1: u32, param2: &str) -> AiCommands {
        AiCommands {
            id: 0,
            cmd_set_id: 0,
            cmd_id,
            param1,
            param2: param2.into(),
        }
    }

    fn spawn_npc(server: &GameServer, template_id: u32) -> NpcRef {
        let mut n = Npc::new(template_id);
        n.unit.name = "Training Dummy".into();
        n.unit.level = 10;
        n.unit.hp = 100;
        n.unit.max_hp = 100;
        n.unit.mp = 5;
        n.unit.max_mp = 10;
        n.unit.model_id = 1001;
        n.unit.faction_id = 101;
        n.unit.transform.set_xyz(112.0, 200.0, 100.0);
        n.unit.transform.zone_id = crate::test_support::TEST_ZONE;
        let npc = Entity::new(NPC_OBJ, n);
        server
            .world
            .world(0)
            .unwrap()
            .spawn(&npc, &server.view_ctx());
        npc
    }

    fn attach_spawner(server: &GameServer, npc: &NpcRef) {
        let position = WorldSpawnPosition {
            world_id: 0,
            zone_id: crate::test_support::TEST_ZONE,
            x: 110.0,
            y: 200.0,
            z: 100.0,
            ..WorldSpawnPosition::default()
        };
        let template = NpcSpawnerTemplate {
            id: 7,
            name: "test".into(),
            ..NpcSpawnerTemplate::default()
        };
        let spawner = NpcSpawner::new(
            NpcSpawnerInit {
                id: 42,
                spawner_id: 7,
                unit_id: 51,
                follow_path: "public_farm".into(),
                follow_npc: 0,
                respawn_time: 15,
                despawn_time: 20,
                special_link: String::new(),
                count: 1,
                npc_spawner_ids: Vec::new(),
                parent_world: Some(0),
                template: Some(Arc::new(template)),
                spawnable_npcs: Vec::new(),
            },
            NpcSpawnerShared::new(position),
        );
        server
            .systems
            .npcs
            .spawns
            .set_npc_spawner(npc.obj_id, Arc::new(spawner));
        npc.data().spawner_id = 42;
    }

    #[tokio::test]
    async fn ai_info_matches_the_testai_fields() {
        let server = harness::server();
        let npc = spawn_npc(&server, 51);
        server.systems.ai.ais.add_ai(&npc, AiKind::HoldPosition);
        run_now(&server, npc.obj_id, |c| {
            c.go_to_talk();
            c.ai.path_handler.ai_path_points = vec![
                AiPathPoint {
                    position: Vec3::new(111.0, 200.0, 100.0),
                    action: AiPathPointAction::None,
                    param: String::new(),
                };
                3
            ];
            c.ai.path_handler
                .ai_path_points_remaining
                .push_back(AiPathPoint {
                    position: Vec3::new(112.0, 200.0, 100.0),
                    action: AiPathPointAction::None,
                    param: String::new(),
                });
            c.ai.commands_queue
                .push_back(command(AiCommandCategory::UseSkill, 18324, ""));
        });

        let info = ai_info(&server, npc.obj_id).expect("AI");
        assert_eq!(info.kind, AiKind::HoldPosition);
        assert_eq!(info.current_behavior, Some(BehaviorKind::Talk));
        assert_eq!(info.path_points, 3);
        assert_eq!(info.path_remaining, 1);
        assert_eq!(info.commands_in_queue, 1);
        assert_eq!(info.commands[0].cmd_id, AiCommandCategory::UseSkill);
        assert_eq!(info.commands[0].param1, 18324);
        assert_eq!(
            behavior_type_name(BehaviorKind::Talk),
            "AAEmu.Game.Models.Game.AI.v2.Behaviors.Common.TalkBehavior"
        );
        assert!(ai_info(&server, 0xdead).is_none());
    }

    #[tokio::test]
    async fn target_log_describes_an_npc_and_is_gated() {
        let server = harness::server();
        let npc = spawn_npc(&server, 51);
        attach_spawner(&server, &npc);
        server.systems.ai.ais.add_ai(&npc, AiKind::HoldPosition);
        run_now(&server, npc.obj_id, |c| {
            c.go_to_talk();
            c.ai.path_handler.ai_path_points = vec![
                AiPathPoint {
                    position: Vec3::new(111.0, 200.0, 100.0),
                    action: AiPathPointAction::None,
                    param: String::new(),
                };
                3
            ];
            c.ai.path_handler
                .ai_path_points_remaining
                .push_back(AiPathPoint {
                    position: Vec3::new(112.0, 200.0, 100.0),
                    action: AiPathPointAction::None,
                    param: String::new(),
                });
            c.ai.current_command = Some(command(AiCommandCategory::FollowPath, 0, "public_farm"));
            c.ai.commands_queue
                .push_back(command(AiCommandCategory::UseSkill, 18324, ""));
        });
        npc.data().unit.buffs.effects.clear();
        npc.data()
            .unit
            .buffs
            .effects
            .push(crate::models::buff::Buff::new(
                crate::data::skills::BuffTemplate::simple(500, crate::data::skills::BuffKind::Good),
                crate::models::buff::SkillCaster::default(),
                None,
                0,
            ));
        let target = WorldObject::from(&npc);

        assert!(target_inspect_log(&server, true, "Gm", Some(&target)).is_none());
        server.config.update(|c| c.world.log_gm_targets = true);
        assert!(target_inspect_log(&server, false, "Gm", Some(&target)).is_none());

        let block = target_inspect_log(&server, true, "Gm", Some(&target)).expect("block");
        for needle in [
            "[target-inspect]",
            "gm=\"Gm\"",
            "obj=2304",
            "name=\"Training Dummy\"",
            "template=51",
            "model=1001",
            "faction=101",
            "level=10",
            "hp=100/100",
            "position=(112.00, 200.00, 100.00)",
            "distance_from_spawn=2.00",
            "spawner=id 42 template 7",
            "follow_path=\"public_farm\"",
            "ai=HoldPosition",
            "current=AAEmu.Game.Models.Game.AI.v2.Behaviors.Common.TalkBehavior",
            "path name=\"public_farm\" points=3 remaining=1",
            "current=\"FollowPath public_farm\"",
            "queue=[UseSkill 18324]",
            "buffs=[500]",
        ] {
            assert!(block.contains(needle), "missing {needle:?} in\n{block}");
        }
    }

    #[derive(Clone, Default)]
    struct LogSink(Arc<Mutex<Vec<String>>>);

    struct SinkSubscriber(LogSink);

    impl Subscriber for SinkSubscriber {
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }
        fn record(&self, _: &Id, _: &Record<'_>) {}
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn event(&self, event: &Event<'_>) {
            struct V(LogSink);
            impl Visit for V {
                fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                    if field.name() == "message" {
                        self.0 .0.lock().unwrap().push(format!("{value:?}"));
                    }
                }
            }
            event.record(&mut V(self.0.clone()));
        }
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}
    }

    fn inspect_events(sink: &LogSink) -> Vec<String> {
        sink.0
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.contains(LOG_PREFIX))
            .cloned()
            .collect()
    }

    #[tokio::test]
    async fn target_change_logs_once_for_a_gm_and_nothing_for_a_non_gm() {
        let server = harness::server();
        let mut gm = harness::pc(&server, 1, "Gm", 100.0, 200.0);
        let npc = spawn_npc(&server, 51);
        server.systems.ai.ais.add_ai(&npc, AiKind::HoldPosition);

        let sink = LogSink::default();
        let subscriber = SinkSubscriber(sink.clone());
        let _guard = tracing::subscriber::set_default(subscriber);

        // Switch off: no line.
        crate::systems::skills::handlers::CSChangeTargetPacket {
            target_id: npc.obj_id,
        }
        .handle(&server, &mut gm.client.conn)
        .await
        .unwrap();
        assert!(inspect_events(&sink).is_empty());
        assert_eq!(gm.p.data().unit.current_target, npc.obj_id);

        // Switch on: one block, not one per repeated packet.
        server.config.update(|c| c.world.log_gm_targets = true);
        crate::systems::skills::handlers::CSChangeTargetPacket {
            target_id: npc.obj_id,
        }
        .handle(&server, &mut gm.client.conn)
        .await
        .unwrap();
        assert!(inspect_events(&sink).is_empty()); // same target as before: no change

        crate::systems::skills::handlers::CSChangeTargetPacket { target_id: 0 }
            .handle(&server, &mut gm.client.conn)
            .await
            .unwrap();
        crate::systems::skills::handlers::CSChangeTargetPacket {
            target_id: npc.obj_id,
        }
        .handle(&server, &mut gm.client.conn)
        .await
        .unwrap();
        let events = inspect_events(&sink);
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(events[0].contains("target=Npc"));
        assert!(events[0].contains("ai=HoldPosition"));

        // A non-GM never logs.
        let mut player = harness::pc(&server, 2, "Player", 120.0, 200.0);
        player.p.data().access_level = 1;
        assert!(!crate::systems::services::character::is_gm(&server, &player.p).await);
        let before = inspect_events(&sink).len();
        crate::systems::skills::handlers::CSChangeTargetPacket {
            target_id: npc.obj_id,
        }
        .handle(&server, &mut player.client.conn)
        .await
        .unwrap();
        assert_eq!(inspect_events(&sink).len(), before);
    }
}
