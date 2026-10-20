//! Flow tests: a bot spawns, walks, casts and is removed through the same paths as a player.

use std::sync::Arc;

use aaemu_commons::PacketStream;

use crate::data::formulas::FormulaManager;
use crate::data::GameData;
use crate::models::character::Character;
use crate::opcodes::sc;
use crate::systems::skills::data::SkillTemplate;
use crate::systems::skills::types::skill_result;
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};

const BOT_SKILL: u32 = 42_424;

/// Game data with the two character formulas the spawn tail recomputes (`MaxHealth`,
/// `MaxMana`); without them `refresh_cached_stats` leaves every max at 0.
fn game_data() -> GameData {
    let mut data = GameData::empty();
    let c = rusqlite::Connection::open_in_memory().unwrap();
    c.execute_batch(
        "CREATE TABLE unit_formulas (id INTEGER, formula TEXT, kind_id INTEGER, owner_type_id INTEGER);
         CREATE TABLE unit_formula_variables (id INTEGER, unit_formula_id INTEGER, variable_kind_id INTEGER, key INTEGER, value REAL);
         CREATE TABLE wearable_formulas (id INTEGER, kind_id INTEGER, formula TEXT);
         CREATE TABLE formulas (id INTEGER, formula TEXT);
         INSERT INTO unit_formulas VALUES
          (14, '5000', 14, 0),
          (15, '1000', 15, 0);",
    )
    .unwrap();
    data.formulas = FormulaManager::load(&c).unwrap();
    data
}

/// A living character for the bot path (the normal char-load fills `saved_hp` from the row).
/// Like a freshly loaded character it has no visual options yet: `spawn_loaded` must supply them,
/// or `SCUnitStatePacket` cannot be written for the observers.
fn bot_character(id: u32, name: &str, x: f32, y: f32) -> Character {
    let mut c = test_character(id, name, x, y);
    c.visual_options = None;
    c.unit.max_hp = 5_000;
    c.unit.hp = 5_000;
    c.saved_hp = 5_000;
    c.unit.max_mp = 1_000;
    c.unit.mp = 1_000;
    c.saved_mp = 1_000;
    c
}

fn spawn_bot(server: &crate::GameServer, c: Character) -> crate::world::PlayerRef {
    super::spawn::spawn_loaded(server, c, &[]).expect("bot spawns")
}

/// An observer on a fresh allocated obj id (the bot allocator starts at the same low ids).
fn observer(server: &crate::GameServer, mut client: TestClient) -> TestClient {
    let obj_id = server.ids.object.get_next_id().unwrap();
    let _p = spawn_player(
        server,
        &client,
        test_character(1, "Observer", 100.0, 100.0),
        obj_id,
    );
    client.drain();
    client
}

#[tokio::test]
async fn bot_spawn_sends_the_same_packets_as_a_player_spawn() {
    let server = test_server_with(game_data());
    let mut observer = observer(&server, TestClient::new(&server, 1, 1_001));

    let bot = spawn_bot(&server, bot_character(2, "Botty", 105.0, 100.0));
    let bot_frames = observer.drain();
    assert_eq!(
        bot_frames.iter().map(|f| f.0).collect::<Vec<_>>(),
        vec![sc::SCUnitStatePacket, sc::SCUnitPointsPacket]
    );
    assert!(server.systems.bots.is_bot(2));
    assert!(server.world.character_by_id(2).is_some());
    assert!(bot.data().is_online);

    // A real player selected through the normal client path produces the same sequence.
    let mut other = TestClient::new(&server, 2, 1_002);
    let mut other_character = bot_character(3, "Other", 106.0, 100.0);
    other_character.visual_options = bot.data().visual_options.clone();
    crate::world::session::select_loaded(
        &server,
        &mut other.conn,
        other_character,
        crate::world::session::SelectDbData {
            offline_friends: Vec::new(),
            offline_blocked: Vec::new(),
            saved_buffs: Vec::new(),
        },
    )
    .expect("select");
    crate::world::session::notify_in_game_player(
        &server,
        other.conn.active_char.as_ref().expect("active char"),
    );
    let player_frames = observer.drain();
    assert_eq!(
        player_frames.iter().map(|f| f.0).collect::<Vec<_>>(),
        bot_frames.iter().map(|f| f.0).collect::<Vec<_>>()
    );
    assert_eq!(player_frames[0].1.len(), bot_frames[0].1.len());
}

#[tokio::test]
async fn bot_walk_broadcasts_normal_unit_movement() {
    let server = test_server_with(game_data());
    let mut observer = observer(&server, TestClient::new(&server, 1, 1_001));
    let bot = spawn_bot(&server, bot_character(2, "Botty", 105.0, 100.0));
    observer.drain();

    assert!(!super::movement::walk_step(&server, &bot, (108.0, 100.0)));
    let frames = observer.drain();
    let movement = frames
        .iter()
        .find(|f| f.0 == sc::SCOneUnitMovementPacket)
        .expect("movement broadcast");
    let mut s = PacketStream::from_bytes(movement.1.clone());
    assert_eq!(s.read_bc().unwrap(), bot.obj_id);
    assert_eq!(s.read_u8().unwrap(), 1); // MoveTypeEnum.Unit
    s.read_u32().unwrap(); // time
    assert!(s.read_u8().unwrap() & 0x02 == 0x02); // MoveTypeFlags.Moving
    let (x, y, z) = s.read_position().unwrap();
    let vel_x = s.read_i16().unwrap();
    let vel_y = s.read_i16().unwrap();
    assert!((x - 105.54).abs() < 0.01, "x = {x}");
    assert!((y - 100.0).abs() < 0.01, "y = {y}");
    assert!((z - 100.0).abs() < 0.01, "z = {z}");
    assert!((f32::from(vel_x) - 5_400.0).abs() < 1.0, "vel_x = {vel_x}");
    assert_eq!(vel_y, 0);
    assert!((bot.data().unit.transform.x - 105.54).abs() < 0.01);
    assert_eq!(bot.data().unit.transform.y, 100.0);

    // Steps continue until the target is reached (then a stopping move goes out).
    for _ in 0..10 {
        if super::movement::walk_step(&server, &bot, (108.0, 100.0)) {
            break;
        }
    }
    assert!((bot.data().unit.transform.x - 108.0).abs() <= super::movement::STOP_DISTANCE);
    assert!(observer.drain_ops().contains(&sc::SCOneUnitMovementPacket));
}

#[tokio::test]
async fn bot_cast_broadcasts_the_native_skill_packets() {
    let mut data = game_data();
    data.systems.skills.add_skill(SkillTemplate {
        id: BOT_SKILL,
        target_type: 0, // Self
        ..SkillTemplate::default()
    });
    let server = test_server_with(data);
    let mut observer = observer(&server, TestClient::new(&server, 1, 1_001));
    let bot = spawn_bot(&server, bot_character(2, "Botty", 105.0, 100.0));
    observer.drain();

    let (result, _) = super::cast::cast(&server, &bot, BOT_SKILL, 0);
    assert_eq!(result, skill_result::SUCCESS);
    let ops = observer.drain_ops();
    assert!(ops.contains(&sc::SCSkillFiredPacket), "{ops:x?}");
}

#[tokio::test]
async fn bot_revives_through_the_normal_resurrection_path_after_the_wait() {
    let server = test_server_with(game_data());
    let mut observer = observer(&server, TestClient::new(&server, 1, 1_001));
    let bot = spawn_bot(&server, bot_character(2, "Botty", 105.0, 100.0));
    observer.drain();

    {
        let mut c = bot.data();
        c.unit.hp = 0;
        c.dead_time = chrono::Utc::now().timestamp() + 1; // just died
        c.rez_wait_duration = 15_000;
    }
    super::life_step(&server, &bot); // wait not over yet
    assert_eq!(bot.data().unit.hp, 0);

    bot.data().dead_time = chrono::Utc::now().timestamp() - 20;
    super::life_step(&server, &bot);
    assert!(bot.data().unit.hp > 0);
    let ops = observer.drain_ops();
    assert!(ops.contains(&sc::SCCharacterResurrectedPacket), "{ops:x?}");
}

#[tokio::test]
async fn bot_remove_despawns_saves_and_clears_the_roster() {
    let server = test_server_with(game_data());
    let mut observer = observer(&server, TestClient::new(&server, 1, 1_001));
    let bot = spawn_bot(&server, bot_character(2, "Botty", 105.0, 100.0));
    let bot_obj = bot.obj_id;
    observer.drain();

    let entry = server.systems.bots.get(2).expect("roster entry");
    super::spawn::despawn(&server, &entry).await;

    assert!(!server.systems.bots.is_bot(2));
    assert!(server.world.character_by_obj_id(bot_obj).is_none());
    assert!(server.world.character_by_id(2).is_none());
    // `LeaveWorldTask` remembered the departure before the (unreachable here) save.
    assert!(server.save.pending_departure(2).is_some());
    assert!(observer.drain_ops().contains(&sc::SCUnitsRemovedPacket));
}

#[tokio::test]
async fn bot_command_lists_walks_casts_and_removes() {
    use super::commands::BotCommand;
    use crate::systems::services::commands::scripts::harness;

    let mut data = game_data();
    data.systems.skills.add_skill(SkillTemplate {
        id: BOT_SKILL,
        target_type: 0, // Self
        ..SkillTemplate::default()
    });
    let server = harness::server_with(data);
    server
        .systems
        .services
        .commands
        .register_names(&["bot"], Arc::new(BotCommand));
    let mut gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
    let _bot = spawn_bot(&server, bot_character(2, "Botty", 105.0, 100.0));

    let (_, out) = harness::run(&server, &gm.p, "bot list").await;
    assert_eq!(out[0], harness::normal("bot", "1 active bot(s):"));
    assert!(out[1].contains("Botty"), "{out:?}");
    assert!(out[1].contains("alive"), "{out:?}");

    let (_, out) = harness::run(&server, &gm.p, "bot walk Botty 110 100").await;
    assert_eq!(
        out,
        [harness::normal("bot", "'Botty' walks to (110.0, 100.0)")]
    );

    let (_, out) = harness::run(&server, &gm.p, "bot cast Botty 42424").await;
    assert_eq!(
        out,
        [harness::normal("bot", "'Botty' casts 42424 on obj 0")]
    );

    let (_, out) = harness::run(&server, &gm.p, "bot remove Botty").await;
    assert_eq!(out, [harness::normal("bot", "Removed Botty")]);
    assert!(!server.systems.bots.is_bot(2));

    let (_, out) = harness::run(&server, &gm.p, "bot list").await;
    assert_eq!(out, [harness::normal("bot", "No active bots")]);
    gm.frames();
}
