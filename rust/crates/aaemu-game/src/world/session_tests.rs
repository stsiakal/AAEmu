//! Entry / leave session guards: a character may have one world session at a time, the
//! reservation is taken before the select's MySQL load and kept until the departure save has
//! finished, and a failed world registration releases it again.

use std::sync::{Arc, Barrier};

use crate::test_support::{spawn_player, test_character, test_server, TestClient};
use crate::world::session::{self, SelectDbData};

#[tokio::test]
async fn the_same_character_cannot_enter_the_world_twice() {
    let server = test_server();
    let mut a = TestClient::new(&server, 1, 1001);
    let mut b = TestClient::new(&server, 2, 1001);
    let c = test_character(7, "Twin", 100.0, 100.0);
    a.conn.characters = vec![c.clone()];
    b.conn.characters = vec![c.clone()];

    session::select_loaded(&server, &mut a.conn, c.clone(), SelectDbData::default())
        .expect("first select");
    let first = a.conn.active_char.clone().expect("first active char");
    assert_eq!(server.enter_world.character_owner(7), Some(1));

    let error = session::select_loaded(&server, &mut b.conn, c, SelectDbData::default())
        .expect_err("a second session must not load the same character");
    assert!(
        error.to_string().contains("already in the world"),
        "unexpected error: {error}"
    );
    assert!(b.conn.active_char.is_none());
    // The failed attempt must not release the first session's reservation or entry.
    assert_eq!(server.enter_world.character_owner(7), Some(1));
    let live = server
        .world
        .character_by_id(7)
        .expect("the character stays in the world");
    assert!(Arc::ptr_eq(&live, &first));
    assert_eq!(server.world.all_characters().len(), 1);
}

/// Two connections enter the same character at the same instant: the reservation is atomic, so
/// exactly one wins and the world holds one player (before the fix both entries succeeded, the
/// second silently replacing nothing but running a second live copy).
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_entries_of_one_character_leave_exactly_one_player() {
    let server = Arc::new(test_server());
    let barrier = Arc::new(Barrier::new(2));
    let mut threads = Vec::new();
    for connection_id in [1u32, 2] {
        let server = server.clone();
        let barrier = barrier.clone();
        threads.push(std::thread::spawn(move || {
            let client = TestClient::new(&server, connection_id, 1001);
            let character = test_character(7, "Twin", 100.0, 100.0);
            barrier.wait();
            session::enter_world(&server, character, client.conn.handle())
                .map(|player| (connection_id, player.obj_id))
        }));
    }
    let results: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().expect("enter thread"))
        .collect();
    let winners: Vec<_> = results.iter().filter(|result| result.is_ok()).collect();
    assert_eq!(winners.len(), 1, "results: {results:?}");
    let (winner_connection, _) = *winners[0].as_ref().expect("winner");
    assert_eq!(
        server.enter_world.character_owner(7),
        Some(winner_connection)
    );
    assert_eq!(server.world.all_characters().len(), 1);
    assert_eq!(
        server.world.character_by_id(7).unwrap().conn.id,
        winner_connection
    );
}

#[tokio::test]
async fn enter_world_refuses_an_occupied_world_entry_and_releases_the_reservation() {
    let server = test_server();
    let client = TestClient::new(&server, 3, 1003);
    let existing = spawn_player(
        &server,
        &client,
        test_character(8, "Ghost", 100.0, 100.0),
        0x108,
    );
    // Same obj id as the entry the new session would use.
    server.world.remember_obj_id(8, 0x108);
    assert_eq!(server.enter_world.character_owner(8), None);

    let other = TestClient::new(&server, 4, 1004);
    let error = session::enter_world(
        &server,
        test_character(8, "Ghost", 0.0, 0.0),
        other.conn.handle(),
    )
    .expect_err("an occupied world entry must be refused");
    assert!(
        error.to_string().contains("world entry"),
        "unexpected error: {error}"
    );
    // The refused attempt released its own reservation.
    assert_eq!(server.enter_world.character_owner(8), None);
    let live = server
        .world
        .character_by_id(8)
        .expect("the existing entry stays");
    assert!(Arc::ptr_eq(&live, &existing));
    assert_eq!(server.world.all_characters().len(), 1);
}

#[tokio::test]
async fn leave_releases_the_character_only_after_the_departure_save() {
    let server = test_server();
    let mut a = TestClient::new(&server, 1, 1001);
    let mut b = TestClient::new(&server, 2, 1001);
    let c = test_character(9, "Returner", 100.0, 100.0);
    a.conn.characters = vec![c.clone()];
    b.conn.characters = vec![c.clone()];
    session::select_loaded(&server, &mut a.conn, c.clone(), SelectDbData::default()).unwrap();
    let first = a.conn.active_char.clone().unwrap();

    // While the first session is live the character is not reservable and cannot be loaded.
    assert!(
        session::select_loaded(&server, &mut b.conn, c.clone(), SelectDbData::default()).is_err()
    );
    assert_eq!(server.enter_world.character_owner(9), Some(1));

    // The leave runs the departure save (it fails against the closed test DB, which keeps it in
    // `pending_departure` for the next select's retry) and only then frees the reservation.
    session::leave_world_player(&server, &first).await;
    assert_eq!(server.enter_world.character_owner(9), None);
    assert!(server.world.character_by_id(9).is_none());
    assert!(server.save.pending_departure(9).is_some());

    session::select_loaded(&server, &mut b.conn, c, SelectDbData::default())
        .expect("re-entry after the leave finished");
    let second = b.conn.active_char.clone().unwrap();
    assert!(
        !Arc::ptr_eq(&second, &first),
        "a fresh Player must be created"
    );
    let live = server.world.character_by_id(9).unwrap();
    assert!(Arc::ptr_eq(&live, &second));
}

#[tokio::test]
async fn select_character_refuses_a_reserved_character_before_loading() {
    let server = test_server();
    let a = TestClient::new(&server, 1, 1001);
    let c = test_character(7, "Twin", 100.0, 100.0);
    session::enter_world(&server, c.clone(), a.conn.handle()).expect("first entry");

    let mut b = TestClient::new(&server, 2, 1001);
    b.conn.characters = vec![c];
    // No MySQL is reachable in the test server: reaching the load would fail with a database
    // error instead of the reservation refusal.
    let error = session::select_character(&server, &mut b.conn, 7)
        .await
        .expect_err("the select must be refused before the load");
    assert!(
        error.to_string().contains("already in the world"),
        "unexpected error: {error}"
    );
    assert!(b.conn.active_char.is_none());
}
