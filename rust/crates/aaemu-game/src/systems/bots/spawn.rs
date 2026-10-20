//! Loading and joining a bot character to the world through the normal select / spawn path.

use std::net::SocketAddr;

use crate::db_character::{
    fetch_active_buffs, load_character, load_character_details, SavedBuffRow,
};
use crate::error::{GameError, Result};
use crate::models::character::{Character, DEFAULT_INSTANCE_ID};
use crate::net::connection::ConnectionHandle;
use crate::server::GameServer;
use crate::world::session;
use crate::world::PlayerRef;

/// The connection id a detached bot handle carries (`ConnectionHandle` ids start at 1, so no
/// real connection ever collides).
pub const DETACHED_CONNECTION_ID: u32 = 0;

/// `Character.Load()` + `Character.Load()` details, then [`spawn_loaded`]: the `/bot add` path.
pub async fn load_and_spawn(server: &GameServer, character_id: u32) -> Result<PlayerRef> {
    if server.world.character_by_id(character_id).is_some() {
        return Err(GameError::Protocol(format!(
            "character {character_id} is already online"
        )));
    }
    let account_id = server.names.get_character_account(character_id);
    let Some((mut c, _)) = load_character(server, character_id, account_id).await? else {
        return Err(GameError::Protocol(format!(
            "no character with id {character_id}"
        )));
    };
    // Force the character into main_world, like a character select does.
    c.unit.transform.instance_id = DEFAULT_INSTANCE_ID;
    load_character_details(server, &mut c).await?;
    // Character.Load(): Mates.Load (vehicles subsystem)
    crate::systems::vehicles::load_character(server, c.id).await;
    let saved_buffs = fetch_active_buffs(server, c.id).await;
    spawn_loaded(server, c, &saved_buffs)
}

/// Joins an already loaded [`Character`] to the world exactly like a normal login: `enter_world`
/// (obj id, `TryAddCharacter`, per-character subsystems), the buff / stat tail, the notify-in-game
/// world join and the spawn.
pub fn spawn_loaded(
    server: &GameServer,
    c: Character,
    saved_buffs: &[SavedBuffRow],
) -> Result<PlayerRef> {
    let character_id = c.id;
    if server.systems.bots.is_bot(character_id)
        || server.world.character_by_id(character_id).is_some()
    {
        return Err(GameError::Protocol(format!(
            "character {character_id} is already active"
        )));
    }
    let conn = ConnectionHandle::detached(
        DETACHED_CONNECTION_ID,
        SocketAddr::from(([0, 0, 0, 0], 0)),
        c.account_id,
    );
    let player = session::enter_world(server, c, conn)?;
    session::finish_character_entry(server, &player, saved_buffs);
    session::notify_in_game_player(server, &player);
    session::notify_in_game_completed_player(server, &player);
    super::attach(server, player.clone());
    tracing::info!(
        "Bots: spawned '{}' (character {}, obj {}) at ({:.1}, {:.1}, {:.1}) zone {}",
        player.data().unit.name,
        player.character_id,
        player.obj_id,
        player.data().unit.transform.x,
        player.data().unit.transform.y,
        player.data().unit.transform.z,
        player.data().unit.transform.zone_id,
    );
    Ok(player)
}

/// `/bot remove`: saves and removes the bot like a normal logout.
pub async fn despawn(server: &GameServer, bot: &super::Bot) {
    bot.cancel_tasks();
    server.systems.bots.remove(bot.player.character_id);
    session::leave_world_player(server, &bot.player).await;
    tracing::info!(
        "Bots: removed '{}' (character {})",
        bot.player.data().unit.name,
        bot.player.character_id,
    );
}
