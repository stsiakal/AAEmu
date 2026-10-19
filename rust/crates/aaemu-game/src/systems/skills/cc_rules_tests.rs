//! Crowd-control rule layer: `buff_breakers` (a stun, sleep or fear ends a glider flight / a
//! song), the `buff_tolerances` diminishing returns on repeated crowd control and the
//! damage-breaks-sleep removal flags, plus the `aoe_diminishings` rate table.

use std::sync::Arc;
use std::time::Duration;

use crate::data::GameData;
use crate::models::npc::Npc;
use crate::server::GameServer;
use crate::systems::skills::data::{BuffTemplate, BuffTolerance};
use crate::systems::skills::types::BuffRemoveOn;
use crate::test_support::test_server_with;
use crate::world::{Entity, NpcRef, WorldObject};

const GLIDER: u32 = 7_001;
const SONG: u32 = 7_002;
const STUN: u32 = 7_003;
const SLEEP: u32 = 7_004;
const HASTE: u32 = 7_005;
const STUN_TAG: u32 = 6;
const SLEEP_TAG: u32 = 97;
const FLIGHT_TAG: u32 = 294;
const TOLERANCE_FINAL: u32 = 7_006;

fn buff(id: u32, duration: i32) -> BuffTemplate {
    BuffTemplate {
        id,
        kind: 1,
        duration,
        ..BuffTemplate::default()
    }
}

fn fixture() -> (Arc<GameServer>, NpcRef, WorldObject) {
    let mut data = GameData::empty();
    let skills = &mut data.systems.skills;
    skills.add_buff(buff(GLIDER, 60_000));
    skills.add_buff(buff(SONG, 60_000));
    skills.add_buff(buff(HASTE, 60_000));
    skills.add_buff(buff(STUN, 10_000));
    skills.add_buff(BuffTemplate {
        remove_on_damaged_etc: true,
        ..buff(SLEEP, 15_000)
    });
    skills.add_buff(buff(TOLERANCE_FINAL, 10_000));
    for (id, tag) in [(GLIDER, FLIGHT_TAG), (STUN, STUN_TAG), (SLEEP, SLEEP_TAG)] {
        skills.buff_tags.entry(id).or_default().push(tag);
        skills.tagged_buffs.entry(tag).or_default().push(id);
    }
    // The glider ends on a stun or sleep, the song on a stun.
    skills
        .buff_breakers
        .insert(GLIDER, vec![STUN_TAG, SLEEP_TAG]);
    skills.buff_breakers.insert(SONG, vec![STUN_TAG]);
    // Stun tolerance (the shipped shape, e.g. tolerance 5: 0, 20, 40, 0): 0 %, 50 %, then a step
    // that does not rise again ends the series with the final-step buff.
    skills.buff_game_data.tolerances.insert(
        STUN_TAG,
        BuffTolerance {
            id: 1,
            buff_tag_id: STUN_TAG,
            step_duration: 30,
            final_step_buff_id: TOLERANCE_FINAL,
            character_time_reduction: 0,
            steps: vec![(1, 100, 0), (2, 100, 50), (3, 100, 0)],
        },
    );
    let server = Arc::new(test_server_with(data));
    server.set_self();
    crate::systems::skills::bind(&server);
    let mut npc = Npc::new(13072);
    npc.unit.hp = 5000;
    npc.unit.max_hp = 5000;
    let npc = Entity::new(0x301, npc);
    let obj = WorldObject::from(&npc);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&obj, &server.view_ctx());
    (server, npc, obj)
}

fn has(npc: &NpcRef, id: u32) -> bool {
    npc.data().unit.buffs.check_buff(id)
}

async fn settle(mut done: impl FnMut() -> bool) {
    for _ in 0..100 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn a_breaker_tag_ends_the_buffs_it_breaks() {
    let (server, npc, obj) = fixture();
    super::buffs::add_buff_by_id(&server, &obj, GLIDER, &obj, 0);
    super::buffs::add_buff_by_id(&server, &obj, SONG, &obj, 0);
    super::buffs::add_buff_by_id(&server, &obj, HASTE, &obj, 0);
    assert!(has(&npc, GLIDER) && has(&npc, SONG) && has(&npc, HASTE));

    super::buffs::add_buff_by_id(&server, &obj, STUN, &obj, 0);
    settle(|| !has(&npc, GLIDER) && !has(&npc, SONG)).await;
    assert!(!has(&npc, GLIDER), "stun ends the glider flight");
    assert!(!has(&npc, SONG), "stun ends the song");
    assert!(has(&npc, STUN), "the breaker itself stays");
    assert!(has(&npc, HASTE), "a buff without breakers is untouched");
}

#[tokio::test]
async fn a_breaker_tag_not_in_the_list_leaves_the_buff() {
    let (server, npc, obj) = fixture();
    super::buffs::add_buff_by_id(&server, &obj, SONG, &obj, 0);
    // Sleep is a breaker of the glider only, not of the song.
    super::buffs::add_buff_by_id(&server, &obj, SLEEP, &obj, 0);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(has(&npc, SONG));
    assert!(has(&npc, SLEEP));
}

#[tokio::test]
async fn a_breakable_buff_ends_at_once_under_an_active_breaker() {
    let (server, npc, obj) = fixture();
    super::buffs::add_buff_by_id(&server, &obj, STUN, &obj, 0);
    super::buffs::add_buff_by_id(&server, &obj, GLIDER, &obj, 0);
    settle(|| !has(&npc, GLIDER)).await;
    assert!(!has(&npc, GLIDER), "a glider cannot start while stunned");
    assert!(has(&npc, STUN));
}

#[tokio::test]
async fn repeated_stuns_follow_the_tolerance_steps() {
    let (server, npc, obj) = fixture();
    let durations = |npc: &NpcRef| -> Vec<i32> {
        npc.data()
            .unit
            .buffs
            .effects
            .iter()
            .filter(|e| e.template.id == STUN)
            .map(|e| e.duration)
            .collect()
    };
    super::buffs::add_buff_by_id(&server, &obj, STUN, &obj, 0);
    assert_eq!(durations(&npc), vec![10_000], "first stun: full duration");
    // The refresh keeps only the longer instance, so read each application right away.
    super::buffs::remove_buff(&server, &obj, STUN);
    settle(|| !has(&npc, STUN)).await;
    super::buffs::add_buff_by_id(&server, &obj, STUN, &obj, 0);
    assert_eq!(durations(&npc), vec![5_000], "second stun: halved");
    super::buffs::remove_buff(&server, &obj, STUN);
    settle(|| !has(&npc, STUN)).await;
    // The third application is the end of the series: the final-step buff follows.
    super::buffs::add_buff_by_id(&server, &obj, STUN, &obj, 0);
    settle(|| has(&npc, TOLERANCE_FINAL)).await;
    assert!(
        has(&npc, TOLERANCE_FINAL),
        "final step buff after the last step"
    );
}

#[tokio::test]
async fn damage_removes_a_breakable_sleep() {
    let (server, npc, obj) = fixture();
    super::buffs::add_buff_by_id(&server, &obj, SLEEP, &obj, 0);
    assert!(has(&npc, SLEEP));
    super::buffs::trigger_remove_on(&server, &obj, BuffRemoveOn::DamagedEtc, 0);
    settle(|| !has(&npc, SLEEP)).await;
    assert!(!has(&npc, SLEEP), "damage ends the sleep");
}

#[test]
fn aoe_diminishing_table_ranks() {
    let mut data = GameData::empty();
    for (id, rate) in [(1, 100.0), (2, 90.0), (3, 80.0), (10, 10.0)] {
        data.systems.skills.aoe_diminishings.insert(id, rate);
    }
    let s = &data.systems.skills;
    assert_eq!(s.aoe_diminishing_multiplier(0), 1.0);
    assert_eq!(s.aoe_diminishing_multiplier(1), 1.0);
    assert!((s.aoe_diminishing_multiplier(2) - 0.9).abs() < 1e-6);
    assert!((s.aoe_diminishing_multiplier(3) - 0.8).abs() < 1e-6);
    assert!((s.aoe_diminishing_multiplier(25) - 0.1).abs() < 1e-6);
    assert_eq!(
        GameData::empty()
            .systems
            .skills
            .aoe_diminishing_multiplier(4),
        1.0
    );
}

fn second_npc(server: &GameServer) -> (NpcRef, WorldObject) {
    let mut npc = Npc::new(13072);
    npc.unit.hp = 5000;
    npc.unit.max_hp = 5000;
    let npc = Entity::new(0x302, npc);
    let obj = WorldObject::from(&npc);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&obj, &server.view_ctx());
    (npc, obj)
}

fn explode(server: &GameServer, caster: &WorldObject, target: &WorldObject) {
    use super::effects::{special, EffectArgs, EffectSource};
    use super::geometry::Target;
    use super::types::{CastAction, SkillCastTarget, SkillCaster, SkillObject};
    let skill_object = SkillObject::None;
    special::execute(
        server,
        46,
        &EffectArgs {
            caster: Some(caster.clone()),
            caster_caster: SkillCaster::Unit(caster.obj_id()),
            target: Target::Obj(target.clone()),
            target_caster: SkillCastTarget::Unit(target.obj_id()),
            cast: CastAction::Skill {
                skill_id: 16410,
                tl_id: 1,
            },
            source: EffectSource::default(),
            skill_object: &skill_object,
        },
        [1000, 20, 20, 0],
    );
}

#[tokio::test]
async fn explode_buff_burns_good_buffs_and_only_then_damages() {
    let (server, npc, obj) = fixture();
    let (caster_npc, caster) = second_npc(&server);
    // No beneficial effect: no damage.
    explode(&server, &caster, &obj);
    assert_eq!(npc.data().unit.hp, 5000);
    // A beneficial effect is burned away and the damage lands.
    super::buffs::add_buff_by_id(&server, &obj, HASTE, &obj, 0);
    assert!(has(&npc, HASTE));
    explode(&server, &caster, &obj);
    settle(|| !has(&npc, HASTE)).await;
    assert!(!has(&npc, HASTE), "the beneficial effect is burned away");
    assert_eq!(npc.data().unit.hp, 4000, "value1 is the fixed damage");
    assert_eq!(caster_npc.data().unit.hp, 5000);
}

#[tokio::test]
async fn anchor_controller_lands_at_the_stage_speed() {
    use super::controllers::{self, Behavior, SkillControllerKind};
    use super::data::SkillControllerTemplate;
    use super::geometry::Target;
    let (server, _npc, obj) = fixture();
    let mut template = SkillControllerTemplate {
        id: 7736,
        kind_id: SkillControllerKind::Anchor as u32,
        end_skill_id: 23437,
        ..SkillControllerTemplate::default()
    };
    template.value[1] = 30_000;
    template.value[2] = 900_000;
    let sc = controllers::create(
        &server,
        &template,
        &obj,
        &Target::Obj(obj.clone()),
        0.0,
        0.0,
        0.0,
        0.0,
    )
    .expect("anchor controller");
    let Behavior::Anchor(a) = sc.behavior() else {
        panic!("not an anchor controller");
    };
    assert_eq!(a.speed, 30.0);
    assert_eq!(a.timeout_ms, 900_000.0);
    assert_eq!(a.end_skill_id, 23437);
    assert_eq!(sc.kind(), SkillControllerKind::Anchor);
}
