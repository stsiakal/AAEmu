//! `/bot cast`: the native server skill path for a bot, with a server-chosen target.

use crate::server::GameServer;
use crate::systems::skills::skill;
use crate::systems::skills::types::{skill_result, SkillCastTarget, SkillCaster, SkillObject};
use crate::systems::skills::unit_reqs::skill_result_key;
use crate::world::{PlayerRef, WorldObject};

/// Casts `skill_id` as `bot` on `target_obj_id` (`0` = the bot itself). Returns the native
/// `SkillResult` and its extra value (the same pair `use_skill` gives a client handler).
pub fn cast(server: &GameServer, bot: &PlayerRef, skill_id: u32, target_obj_id: u32) -> (u8, u32) {
    let Some(template) = server.game_data.systems.skills.skill(skill_id).cloned() else {
        tracing::warn!("Bots: skill {skill_id} has no template");
        return (skill_result::INVALID_SKILL, 0);
    };
    let caster = WorldObject::from(bot);
    let instance_id = bot.data().unit.transform.instance_id;
    let target = if target_obj_id == 0 || target_obj_id == bot.obj_id {
        caster.clone()
    } else {
        match server
            .world
            .world(instance_id)
            .and_then(|w| w.object(target_obj_id))
        {
            Some(target) => target,
            None => {
                tracing::warn!(
                    "Bots: cast target {target_obj_id} is not in instance {instance_id}"
                );
                return (skill_result::NO_TARGET, 0);
            }
        }
    };
    let mut target_caster = if target.as_doodad().is_some() {
        SkillCastTarget::Doodad(target.obj_id())
    } else {
        SkillCastTarget::Unit(target.obj_id())
    };
    let skill = skill::new_skill_for(server, template, Some(&caster));
    let (result, value) = skill::use_skill(
        server,
        &skill,
        &caster,
        SkillCaster::Unit(bot.obj_id),
        &mut target_caster,
        Some(SkillObject::default()),
        false,
    );
    if result != skill_result::SUCCESS {
        tracing::warn!(
            character = %bot.data().unit.name,
            skill_id,
            target = target.obj_id(),
            result,
            result_key = skill_result_key(result),
            "Bots: cast failed"
        );
    }
    (result, value)
}
