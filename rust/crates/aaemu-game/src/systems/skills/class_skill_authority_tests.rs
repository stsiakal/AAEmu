//! Client casts of ability-tree skills need an active ability and (when `need_learn`) a learned
//! skill; the 1.2 client enforces this locally, the server must too.

use std::sync::Arc;

use crate::data::GameData;
use crate::net::registry::ClientPacket;
use crate::opcodes::sc;
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
use crate::world::PlayerRef;

use crate::systems::skills::data::SkillTemplate;
use crate::systems::skills::types::skill_result;

use super::{class_skill_denial, CSStartSkillPacket, SkillCastTarget, SkillCaster, SkillObject};

const ABILITY: u8 = 3;
const TREE_SKILL: u32 = 31001;
const PLAIN_SKILL: u32 = 31002;

fn fixture(active: bool, learned: bool) -> (Arc<crate::GameServer>, TestClient, PlayerRef) {
    let mut data = GameData::empty();
    data.systems.skills.add_skill(SkillTemplate {
        id: TREE_SKILL,
        ability_id: ABILITY,
        need_learn: true,
        target_type: 9,
        max_range: 4,
        ..SkillTemplate::default()
    });
    // A doodad / quest style skill: no ability, never learned.
    data.systems.skills.add_skill(SkillTemplate {
        id: PLAIN_SKILL,
        target_type: 9,
        max_range: 4,
        ..SkillTemplate::default()
    });
    let server = Arc::new(test_server_with(data));
    server.set_self();
    crate::systems::skills::bind(&server);
    let mut client = TestClient::new(&server, 1, 1001);
    let mut ch = test_character(1, "Caster", 100.0, 100.0);
    ch.ability1 = if active { ABILITY } else { 1 };
    if learned {
        ch.skills.skills.push(crate::models::char_data::Skill {
            id: TREE_SKILL,
            level: 1,
        });
    }
    let player = spawn_player(&server, &client, ch, 0x100);
    client.conn.active_char = Some(player.clone());
    client.drain();
    (server, client, player)
}

async fn cast(server: &crate::GameServer, client: &mut TestClient, p: &PlayerRef, skill: u32) {
    CSStartSkillPacket {
        skill_id: skill,
        caster: SkillCaster::Unit(p.obj_id),
        target: SkillCastTarget::Unit(p.obj_id),
        flag: 0,
        skill_object: SkillObject::None,
    }
    .handle(server, &mut client.conn)
    .await
    .unwrap();
}

#[tokio::test]
async fn unlearned_ability_skill_is_refused() {
    let (server, mut client, p) = fixture(true, false);
    cast(&server, &mut client, &p, TREE_SKILL).await;
    assert_eq!(p.data().unit.skill_last_used_ms, 0);
    let frames = client.drain();
    assert!(frames.iter().any(|(op, _)| *op == sc::SCSkillStartedPacket));
    assert!(!frames.iter().any(|(op, _)| *op == sc::SCSkillFiredPacket));
}

#[tokio::test]
async fn learned_skill_of_an_inactive_ability_is_refused() {
    let (server, mut client, p) = fixture(false, true);
    cast(&server, &mut client, &p, TREE_SKILL).await;
    assert_eq!(p.data().unit.skill_last_used_ms, 0);
    let frames = client.drain();
    assert!(frames.iter().any(|(op, _)| *op == sc::SCSkillStartedPacket));
    assert!(!frames.iter().any(|(op, _)| *op == sc::SCSkillFiredPacket));
}

#[tokio::test]
async fn learned_skill_of_an_active_ability_casts() {
    let (server, mut client, p) = fixture(true, true);
    cast(&server, &mut client, &p, TREE_SKILL).await;
    assert!(p.data().unit.skill_last_used_ms > 0);
}

#[tokio::test]
async fn skills_without_an_ability_stay_castable() {
    let (server, mut client, p) = fixture(true, false);
    cast(&server, &mut client, &p, PLAIN_SKILL).await;
    assert!(p.data().unit.skill_last_used_ms > 0);
}

#[tokio::test]
async fn denial_codes() {
    let (server, _c, p) = fixture(true, false);
    assert_eq!(
        class_skill_denial(&server, &p, TREE_SKILL),
        Some(skill_result::INVALID_SKILL)
    );
    assert_eq!(class_skill_denial(&server, &p, PLAIN_SKILL), None);
    let (server, _c, p) = fixture(false, true);
    assert_eq!(
        class_skill_denial(&server, &p, TREE_SKILL),
        Some(skill_result::INACTIVE_ABILITY)
    );
}
