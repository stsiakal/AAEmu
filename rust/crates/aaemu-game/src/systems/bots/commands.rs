//! The operator `/bot` command: lifecycle, walking and casting for the in-server bots.
//!
//! Every sub command is an operator tool (the command access level defaults to 100 like every
//! unlisted GM command). A bot itself never runs a command and nothing here is visible to
//! clients.

use futures::future::BoxFuture;

use crate::managers::name::normalize_name;
use crate::server::GameServer;
use crate::systems::services::commands::output::MessageOutput;
use crate::systems::services::commands::{Command, CommandContext, CommandManager};
use crate::systems::skills::unit_reqs::skill_result_key;

/// `/bot`.
pub struct BotCommand;

impl BotCommand {
    fn usage(&self, out: &mut dyn MessageOutput) {
        out.send_message(
            "|cFFFFFFFFUsage|r: /bot add <name> | remove <name> | list | walk <name> <x> <y> | \
             cast <name> <skill id> [target obj id]",
        );
    }
}

impl Command for BotCommand {
    fn command_names(&self) -> Vec<String> {
        vec!["bot".into()]
    }

    fn execute(
        &self,
        _ctx: &CommandContext<'_>,
        _args: &[String],
        out: &mut dyn MessageOutput,
    ) -> anyhow::Result<()> {
        self.usage(out);
        Ok(())
    }

    fn execute_async<'a>(
        &'a self,
        ctx: &'a CommandContext<'a>,
        args: &'a [String],
        out: &'a mut dyn MessageOutput,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let server = ctx.server()?;
            let Some(sub) = args.first().map(|s| s.to_lowercase()) else {
                self.usage(out);
                return Ok(());
            };
            match sub.as_str() {
                "add" => add(server, self, args, out).await,
                "remove" => remove(server, self, args, out).await,
                "list" => list(server, self, out),
                "walk" => walk(server, self, args, out),
                "cast" => cast(server, self, args, out),
                other => CommandManager::send_error_text(
                    self,
                    out,
                    &format!("Unknown sub command '{other}', try /bot"),
                ),
            }
            Ok(())
        })
    }

    fn get_command_line_help(&self) -> String {
        "add <name> | remove <name> | list | walk <name> <x> <y> | cast <name> <skill id> \
         [target obj id]"
            .into()
    }

    fn get_command_help_text(&self) -> String {
        "Connectionless player bots: load an existing character into the world, walk it in a \
         straight line, cast a skill as it, list or remove it. Bots use the normal world, skill \
         and save paths; nothing bot-specific is sent to clients."
            .into()
    }
}

async fn add(
    server: &GameServer,
    command: &BotCommand,
    args: &[String],
    out: &mut dyn MessageOutput,
) {
    let Some(name) = args.get(1) else {
        CommandManager::send_error_text(command, out, "Usage: /bot add <character name>");
        return;
    };
    let character_id = server.names.get_character_id(&normalize_name(name));
    if character_id == 0 {
        CommandManager::send_error_text(command, out, &format!("No character named '{name}'"));
        return;
    }
    if server.systems.bots.is_bot(character_id)
        || server.world.character_by_id(character_id).is_some()
    {
        CommandManager::send_error_text(
            command,
            out,
            &format!("Character '{name}' is already online"),
        );
        return;
    }
    match crate::systems::bots::spawn::load_and_spawn(server, character_id).await {
        Ok(player) => CommandManager::send_normal_text(
            command,
            out,
            &format!(
                "Added {} (character id {}, obj id {})",
                player.data().unit.name,
                player.character_id,
                player.obj_id
            ),
        ),
        Err(e) => {
            CommandManager::send_error_text(command, out, &format!("Could not add '{name}': {e}"))
        }
    }
}

async fn remove(
    server: &GameServer,
    command: &BotCommand,
    args: &[String],
    out: &mut dyn MessageOutput,
) {
    let Some(name) = args.get(1) else {
        CommandManager::send_error_text(command, out, "Usage: /bot remove <name>");
        return;
    };
    let Some(bot) = server.systems.bots.get_by_name(name) else {
        CommandManager::send_error_text(command, out, &format!("No active bot named '{name}'"));
        return;
    };
    let bot_name = bot.player.data().unit.name.clone();
    crate::systems::bots::spawn::despawn(server, &bot).await;
    CommandManager::send_normal_text(command, out, &format!("Removed {bot_name}"));
}

fn list(server: &GameServer, command: &BotCommand, out: &mut dyn MessageOutput) {
    let bots = server.systems.bots.list();
    if bots.is_empty() {
        CommandManager::send_normal_text(command, out, "No active bots");
        return;
    }
    CommandManager::send_normal_text(command, out, &format!("{} active bot(s):", bots.len()));
    for bot in bots {
        let c = bot.player.data();
        let t = c.unit.transform;
        out.send_message(&format!(
            "  {} (character {}, obj {}, level {}, {}, {:.1}, {:.1})",
            c.unit.name,
            c.id,
            bot.player.obj_id,
            c.unit.level,
            if c.unit.hp > 0 { "alive" } else { "dead" },
            t.x,
            t.y,
        ));
    }
}

fn walk(server: &GameServer, command: &BotCommand, args: &[String], out: &mut dyn MessageOutput) {
    let Some(name) = args.get(1) else {
        CommandManager::send_error_text(command, out, "Usage: /bot walk <name> <x> <y>");
        return;
    };
    let (Some(x), Some(y)) = (
        args.get(2).and_then(|v| v.parse::<f32>().ok()),
        args.get(3).and_then(|v| v.parse::<f32>().ok()),
    ) else {
        CommandManager::send_error_text(command, out, "Usage: /bot walk <name> <x> <y>");
        return;
    };
    let Some(bot) = server.systems.bots.get_by_name(name) else {
        CommandManager::send_error_text(command, out, &format!("No active bot named '{name}'"));
        return;
    };
    if bot.player.data().unit.hp == 0 {
        CommandManager::send_error_text(command, out, &format!("'{name}' is dead"));
        return;
    }
    let Some(server_arc) = server.self_arc() else {
        CommandManager::send_error_text(command, out, "Server unavailable");
        return;
    };
    crate::systems::bots::movement::start(&server_arc, &bot, x, y);
    CommandManager::send_normal_text(
        command,
        out,
        &format!(
            "'{}' walks to ({x:.1}, {y:.1})",
            bot.player.data().unit.name
        ),
    );
}

fn cast(server: &GameServer, command: &BotCommand, args: &[String], out: &mut dyn MessageOutput) {
    let Some(name) = args.get(1) else {
        CommandManager::send_error_text(
            command,
            out,
            "Usage: /bot cast <name> <skill id> [target obj id]",
        );
        return;
    };
    let Some(skill_id) = args.get(2).and_then(|v| v.parse::<u32>().ok()) else {
        CommandManager::send_error_text(
            command,
            out,
            "Usage: /bot cast <name> <skill id> [target obj id]",
        );
        return;
    };
    let target_id = args.get(3).and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
    let Some(bot) = server.systems.bots.get_by_name(name) else {
        CommandManager::send_error_text(command, out, &format!("No active bot named '{name}'"));
        return;
    };
    if bot.player.data().unit.hp == 0 {
        CommandManager::send_error_text(command, out, &format!("'{name}' is dead"));
        return;
    }
    let (result, value) =
        crate::systems::bots::cast::cast(server, &bot.player, skill_id, target_id);
    if result == crate::systems::skills::types::skill_result::SUCCESS {
        CommandManager::send_normal_text(
            command,
            out,
            &format!(
                "'{}' casts {skill_id} on obj {target_id}",
                bot.player.data().unit.name
            ),
        );
    } else {
        CommandManager::send_error_text(
            command,
            out,
            &format!(
                "'{}' cast {skill_id} failed: {} ({value})",
                bot.player.data().unit.name,
                skill_result_key(result)
            ),
        );
    }
}
