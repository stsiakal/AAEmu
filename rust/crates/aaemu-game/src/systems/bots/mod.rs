//! Phase-0 player bots: connectionless characters driven by the server.
//!
//! A bot is a normal [`PlayerRef`] whose `ConnectionHandle` is detached, loaded from the same
//! character rows and spawned through the same world path as a login
//! ([`crate::world::session::enter_world`] / [`crate::world::session::notify_in_game_player`]).
//! Nothing bot-specific reaches a client: the roster lives here, on the server only.
//!
//! | module | role |
//! |---|---|
//! | [`spawn`] | load a character and join it to the world like a normal select |
//! | [`movement`] | straight-line walker broadcasting `SCOneUnitMovementPacket` |
//! | [`cast`] | the native `skills::skill::use_skill` path with a server-chosen target |
//! | [`commands`] | the operator `/bot` command (`add`, `remove`, `list`, `walk`, `cast`) |

pub mod cast;
pub mod commands;
pub mod movement;
pub mod spawn;

#[cfg(test)]
mod tests;

use std::sync::{Arc, Mutex};

use dashmap::DashMap;

use crate::net::registry::PacketRegistry;
use crate::server::GameServer;
use crate::world::PlayerRef;

/// One active bot: its player handle and the cancellable background tasks (walk / life).
#[derive(Debug)]
pub struct Bot {
    pub player: PlayerRef,
    walk_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    life_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Bot {
    fn new(player: PlayerRef) -> Self {
        Self {
            player,
            walk_task: Mutex::new(None),
            life_task: Mutex::new(None),
        }
    }

    /// Replaces the running walk task (the previous one is aborted).
    pub fn set_walk_task(&self, task: tokio::task::JoinHandle<()>) {
        let previous = self
            .walk_task
            .lock()
            .expect("bot walk task poisoned")
            .replace(task);
        if let Some(previous) = previous {
            previous.abort();
        }
    }

    /// Stops the walk task (e.g. the bot was removed or died).
    pub fn cancel_walk(&self) {
        let task = self
            .walk_task
            .lock()
            .expect("bot walk task poisoned")
            .take();
        if let Some(task) = task {
            task.abort();
        }
    }

    fn set_life_task(&self, task: tokio::task::JoinHandle<()>) {
        *self.life_task.lock().expect("bot life task poisoned") = Some(task);
    }

    /// Aborts every background task of this bot.
    pub fn cancel_tasks(&self) {
        self.cancel_walk();
        let task = self
            .life_task
            .lock()
            .expect("bot life task poisoned")
            .take();
        if let Some(task) = task {
            task.abort();
        }
    }
}

/// Runtime state of the subsystem (`server.systems.bots`): the server-side roster.
#[derive(Debug, Default)]
pub struct System {
    bots: DashMap<u32, Arc<Bot>>,
}

impl System {
    pub fn is_bot(&self, character_id: u32) -> bool {
        self.bots.contains_key(&character_id)
    }

    pub fn get(&self, character_id: u32) -> Option<Arc<Bot>> {
        self.bots.get(&character_id).map(|b| b.clone())
    }

    pub fn get_by_name(&self, name: &str) -> Option<Arc<Bot>> {
        self.bots
            .iter()
            .find(|b| b.player.data().unit.name.eq_ignore_ascii_case(name))
            .map(|b| b.clone())
    }

    /// The active bots ordered by name (stable `/bot list` output).
    pub fn list(&self) -> Vec<Arc<Bot>> {
        let mut bots: Vec<Arc<Bot>> = self.bots.iter().map(|b| b.clone()).collect();
        bots.sort_by_key(|b| b.player.data().unit.name.to_lowercase());
        bots
    }

    /// Adds a spawned bot to the roster and returns it.
    pub fn insert(&self, player: PlayerRef) -> Arc<Bot> {
        let bot = Arc::new(Bot::new(player));
        self.bots.insert(bot.player.character_id, bot.clone());
        bot
    }

    pub fn remove(&self, character_id: u32) -> Option<Arc<Bot>> {
        self.bots.remove(&character_id).map(|(_, b)| b)
    }

    /// Startup: registers the operator `/bot` command (the scripts themselves stay untouched).
    pub async fn start(server: &Arc<GameServer>) -> anyhow::Result<()> {
        server
            .systems
            .services
            .commands
            .register_names(&["bot"], Arc::new(commands::BotCommand));
        Ok(())
    }
}

/// No C->S packets: bots are driven by the server, not by a socket.
pub fn register(_reg: &mut PacketRegistry) {}

/// Registers a spawned bot and starts its server-side life watcher (the automatic use of the
/// normal resurrection path once the death wait has elapsed). Needs the shared server handle;
/// without one (unit tests) the caller drives the life step itself.
pub fn attach(server: &GameServer, player: PlayerRef) -> Arc<Bot> {
    let bot = server.systems.bots.insert(player.clone());
    if let Some(server) = server.self_arc() {
        let task_player = player.clone();
        let task = tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                tick.tick().await;
                if !server.systems.bots.is_bot(task_player.character_id) {
                    break;
                }
                life_step(&server, &task_player);
            }
        });
        bot.set_life_task(task);
    }
    bot
}

/// One life-watcher step: a dead bot asks for the normal in-place resurrection once its
/// `rez_wait_duration` elapsed (the request a client would send).
pub fn life_step(server: &GameServer, player: &PlayerRef) {
    let (hp, dead_time, wait_ms, online) = {
        let c = player.data();
        (c.unit.hp, c.dead_time, c.rez_wait_duration, c.is_online)
    };
    if !online {
        return;
    }
    if hp > 0 {
        return;
    }
    let elapsed_ms = (chrono::Utc::now().timestamp() - dead_time).saturating_mul(1000);
    if elapsed_ms >= i64::from(wait_ms) {
        crate::systems::skills::resurrect::resurrect_character(server, player, true);
    }
}
