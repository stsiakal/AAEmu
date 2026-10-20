//! `script`: runs the commands of the file named by `$AAEMU_SCRIPT` after the usual login /
//! select / spawn, printing what the server sends back. Meant for ad-hoc live checks of quest
//! and spawn fixes (GM account needed), so a new check needs no rebuild of the bot.
//!
//! One command per line, `#` starts a comment:
//!
//! ```text
//! gm <command line>                  GM chat command (without the leading slash)
//! gmq <command line>                like `gm`, but waits only 0.4 s and prints nothing
//! tp <x> <y> <z>                     /move there, acknowledge the teleport, wait for the area
//! pump <secs>                        just read packets
//! labor                              the current labor power decoded from the server
//! units [<npc template>...]          NPCs in sight (all within 60 m when no template given; the
//!                                    nearest of the given templates becomes the `loot` / `roll` owner)
//! doodads [<doodad template>...]     doodads in sight (all within 40 m when none given)
//! sq <quest> <npc template|0>        CSStartQuestContext from the nearest NPC of that template
//! cq <quest> <npc template|0> [sel]  CSCompleteQuestContext at that NPC
//! target <npc template>              select the nearest NPC of that template
//! kill <npc template>                walk next to the nearest NPC, target it, `/kill`
//! cast <skill> <npc template>        CSStartSkill on the nearest NPC
//! dcast <skill> <doodad template> [far]  doodad-target CSStartSkill (walks to it first, unless `far`)
//! item <template> <count>            `/item add self` and remember the stack id
//! icast <skill> <item template> <npc template|self>
//!                                    cast from the remembered item stack
//! bag                                list the bag contents (slot / id / template / count)
//! equip <template> <equip slot>      `/item add self`, then the client's bag -> equipment move
//!                                    (equipment slot index: head 0, chest 2, waist 3, legs 4,
//!                                    hands 5, feet 6, arms 7)
//! buffs                              the buffs our unit carries (from SCBuffCreated / Removed)
//! split <from slot> <to slot> <count>
//!                                    `CSSplitBagItemPacket` for the bag item in `from slot`
//! swap <from slot> <to slot>         `CSSwapItemsPacket` between two bag slots
//! deposit|withdraw <amount>          `CSDepositMoneyPacket` / `CSWithdrawMoneyPacket`
//! expand [slot type]                 `CSExpandSlotsPacket` (default 2 = inventory)
//! sell <slot> <npc template>         sell the bag item to the nearest merchant NPC
//! others                             the other player units in sight (the first is the trade peer)
//! tstart                             CSCanStartTrade (the invitation) to the trade peer
//! taccept                            wait (60 s) for SCCanStartTrade and answer CSStartTrade
//! tput <slot type> <slot> <amount>   CSPutupTradeItem
//! tmoney <amount>                    CSPutupTradeMoney
//! tlock <0|1>  /  tok  /  tcancel    CSTradeLock / CSTradeOk / CSCancelTrade
//! rawcs <opcode hex> [<body hex>]    send any client packet and print the replies (`SELF` / `SLAVE`
//!                                     in the hex stand for our / the last spawned slave's object id)
//! portalsave <name>                  record the position in the Teleport Book (skill 11215 with a
//!                                    `SavePortalInfo` object; needs a Memory Ink)
//! now                                print the wall clock (seconds, to time portal lifetimes)
//! useportal <npc template> [only_mine]  CSUsePortal on the nearest NPC of that template (3891 = the
//!                                    portal entrance)
//! delportal <type> <id>              CSDeletePortal (type 1 = district, 2 = recorded)
//! portalopen <kind> <id> <dx> <dy>   open a worldgate (skill 11216 with a `PortalInfo` object) at
//!                                    our position + (dx, dy); needs Hereafter Stones
//! aucpost <bag slot> <buyout> <duration>
//!                                    CSAuctionPost of the bag item (remembers the lot id)
//! auccancel                          CSCancelAuction of the remembered lot
//! aucbid <lot id> <money>            CSBidAuction (a bid at or above the buyout price buys it)
//! interact <npc template>            CSInteractNPC + CSStartInteraction (prints the skill list)
//! repairall                          CSRepairAllEquipments (free of the NPC check in the client)
//! maillist                           CSListMail (received mail), remembers the mail ids
//! mailtakeall [<n>]                  CSTakeAllAttachmentItem for the n-th listed mail
//! partyinvite <name>                 CSInviteToTeam (party) by character name
//! partyaccept                        wait for SCAskToJoinTeam and accept it
//! teamact <leave|kick|dismiss> <id>    CSAskRiskyTeamAction on the current team (the way the
//!                                      original and the Bevy client leave / kick / dismiss);
//!                                      `id` is the character id (own id for leave, 0 for dismiss)
//! ```
//!
//! Two-bot duel / party / PvP / loot-dice commands (run one process per bot, each with its own
//! script; both bots stand at the same spot, see `crate::social` for the packet layouts). Every
//! duel / team / faction / loot-dice / zone-state packet the bot reads is printed as
//! `social: <event>` before the next command, and stays queued for the `await` commands.
//!
//! ```text
//! me                                 own character id / name / object id / last known faction
//! target self|peer|<npc template>    select yourself, the other player or the nearest NPC
//! savenpc <file> | loadnpc <file>    write / read the remembered NPC object id (hands one NPC to
//!                                    the other bot)
//! castlast <skill>                   walk next to the remembered NPC and cast the skill on it
//! targetlast                        select the NPC `units <template>` / `kill` / `cast` remembered
//! waitpeer [secs] [name]             read packets until another player is in sight (60 s default)
//! await <Event> [secs]             wait (60 s default) for a social event (`DuelStarted`,
//!                                    `DuelEnded`, `TeamJoined`, `LootDiceSummary`, `ZoneState`,
//!                                    `FactionChanged`, ...; the variant name) and consume it
//! tryawait <Event> [secs]            like `await` (10 s default) but only reports a timeout
//! events                             print every social event received so far
//! duel [name]                        CSChallengeDuel to the peer (first other player)
//! daccept                            wait for SCDuelChallenged and answer CSStartDuel (accept)
//! drefuse                            wait for SCDuelChallenged and refuse it (507)
//! dcancel                            the challenger cancels its own pending / running duel
//! dwait [secs]                       wait (400 s default) for SCDuelEnded and print who won
//! party [name]                       CSInviteToTeam (new party) to the peer
//! paccept | preject                  wait for SCAskToJoinTeam and accept / reject it
//! pleave | pdismiss                  CSAskRiskyTeamAction leave / dismiss the current team
//! plr <method> <grade> <bop 0|1>     CSChangeLootingRule (all four fields; method 0 = free for
//!                                    all, 1 = rotate winner, 2 = loot master)
//! pcast <skill> [name]               walk next to the peer and cast the skill on it
//! pfight <skill> <secs> [name]       recast on the peer every 1.5 s until one side dies
//! pkill [name]                       `/kill` with the peer targeted (GM)
//! loot [npc obj]                     CSLootOpenBag (loot all) at the last killed / cast NPC
//! roll roll|pass [index] [npc obj]   CSLootDice for the item at `index` (default 0)
//! gcreate <name>                     CSCreateExpedition (a guild; needs the party and the gold)
//! ginvite [name]                     CSInviteToExpedition to the peer (or the named character)
//! gaccept                            wait for the guild invitation and join
//! grole <role> <character id>        CSChangeExpeditionMemberRole (255 owner, 2 officer)
//! drop                               end the script without logging out (a disconnect)
//! ```

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use aaemu_game::opcodes::sc;
use anyhow::{bail, Context};

use super::questitem::{chat_text, gm_command, quest_state, stack_id};
use super::{pump, skill_response_result, Runner};
use crate::game::{parse_item_task, GameClient, Packet, UnitInfo};
use crate::social::{self, SocialEvent};

fn dist(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2) + (a.2 - b.2).powi(2)).sqrt()
}

fn show(packets: &[Packet]) {
    let mut other: std::collections::BTreeMap<&'static str, usize> = Default::default();
    for p in packets {
        let name = p.name();
        if let Some(t) = chat_text(p) {
            println!("    chat: {t}");
        } else if let Some((q, st)) = quest_state(p) {
            println!(
                "    {name} quest {q} status {} objectives {:?}",
                st.status, st.objectives
            );
        } else if p.is_sc(sc::SCErrorMsgPacket) {
            println!("    SCErrorMsg {:?}", p.stream().read_i16());
        } else if p.is_sc(sc::SCItemTaskSuccessPacket) {
            let t = parse_item_task(&p.body);
            let items: Vec<String> = t
                .items
                .iter()
                .map(|i| format!("task{} tpl{} id{}", i.task, i.template, i.id))
                .collect();
            println!(
                "    ItemTask action {} money {} items [{}]",
                t.action,
                t.money,
                items.join("; ")
            );
        } else if name.contains("Quest")
            || name.contains("Exp")
            || name.contains("Death")
            || name.contains("Teleport")
            || name.contains("Mail")
            || name.contains("Trade")
            || name.contains("Interaction")
            || name.contains("Gold")
            || name.contains("Portal")
            || name.contains("UnitsRemoved")
            || name.contains("SlaveCreated")
        {
            let hex: String = p.body.iter().take(48).map(|b| format!("{b:02x}")).collect();
            println!("    {name} len {} {hex}", p.body.len());
        } else {
            *other.entry(name).or_default() += 1;
        }
    }
    if !other.is_empty() {
        let o: Vec<String> = other.iter().map(|(k, v)| format!("{k}x{v}")).collect();
        println!("    (other: {})", o.join(", "));
    }
}

/// Reads packets for `secs` and prints only the quest ones.
async fn show_quiet(game: &mut GameClient, secs: f32) -> anyhow::Result<()> {
    let ms = (secs * 1000.0) as u64;
    let got = game
        .drain(Duration::from_millis(ms), Duration::from_millis(ms))
        .await?;
    for p in got.iter().filter(|p| p.name().contains("Quest")) {
        if let Some((q, st)) = quest_state(p) {
            println!(
                "    {} quest {q} status {} objectives {:?}",
                p.name(),
                st.status,
                st.objectives
            );
        } else {
            println!("    {}", p.name());
        }
    }
    Ok(())
}

fn nearest(game: &GameClient, tpl: u32) -> Option<UnitInfo> {
    let me = game.state.self_pos;
    game.state
        .units
        .values()
        .filter(|u| u.unit_type == 1 && u.id == tpl)
        .min_by(|a, b| dist(a.pos, me).total_cmp(&dist(b.pos, me)))
        .cloned()
}

fn num<T: std::str::FromStr>(parts: &[&str], i: usize) -> anyhow::Result<T> {
    parts
        .get(i)
        .with_context(|| format!("missing argument {i}"))?
        .parse()
        .map_err(|_| anyhow::anyhow!("bad number '{}'", parts[i]))
}

/// The other player to duel / party / fight: `name`, else the first other player in sight.
fn peer(game: &GameClient, name: Option<&str>) -> anyhow::Result<UnitInfo> {
    let me = game.state.self_obj;
    game.state
        .units
        .values()
        .filter(|u| u.unit_type == 0 && u.obj_id != me)
        .filter(|u| name.is_none_or(|n| u.name.eq_ignore_ascii_case(n)))
        .min_by_key(|u| u.obj_id)
        .cloned()
        .context("no other player in sight (run `pump` after both bots teleported)")
}

/// Variant name of a social event (`DuelEnded { .. }` -> `DuelEnded`).
fn event_kind(e: &SocialEvent) -> String {
    let d = format!("{e:?}");
    d.split([' ', '{', '(']).next().unwrap_or("").to_string()
}

/// Waits for the first not yet consumed social event named `kind` (case-insensitive),
/// reading packets for at most `secs`.
async fn await_event(
    game: &mut GameClient,
    consumed: &mut HashSet<usize>,
    kind: &str,
    secs: u64,
) -> anyhow::Result<SocialEvent> {
    let want = kind.to_ascii_lowercase();
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let found = game
            .state
            .social
            .iter()
            .enumerate()
            .find(|(i, e)| !consumed.contains(i) && event_kind(e).to_ascii_lowercase() == want)
            .map(|(i, e)| (i, e.clone()));
        if let Some((i, e)) = found {
            consumed.insert(i);
            return Ok(e);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!("timed out ({secs} s) waiting for social event '{kind}'");
        }
        game.recv_within(left.min(Duration::from_millis(500)))
            .await?;
    }
}

/// The team id of the newest `TeamJoined` event that no `TeamLeft` / `TeamDismissed` followed.
fn current_team(game: &GameClient) -> Option<u32> {
    for e in game.state.social.iter().rev() {
        match e {
            SocialEvent::TeamJoined { team_id, .. } => return Some(*team_id),
            SocialEvent::TeamLeft { .. } | SocialEvent::TeamDismissed { .. } => return None,
            _ => {}
        }
    }
    None
}

/// Walks next to the target and casts `skill` on it.
async fn attack(
    game: &mut GameClient,
    clock: &mut u32,
    skill: u32,
    target: &UnitInfo,
) -> anyhow::Result<()> {
    *clock += 1500;
    game.move_to((target.pos.0 + 1.0, target.pos.1, target.pos.2), *clock)
        .await?;
    game.change_target(target.obj_id).await?;
    game.start_skill(skill, target.obj_id).await
}

pub(super) async fn run(
    game: &mut GameClient,
    cfg: &super::BotConfig,
    r: &mut Runner<'_>,
) -> anyhow::Result<()> {
    let path = std::env::var("AAEMU_SCRIPT").context("set AAEMU_SCRIPT to the script file")?;
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?;
    let s = r.begin("game.script");
    let mut items: HashMap<u32, u64> = HashMap::new();
    let mut clock = 40_000u32;
    let mut mate: Option<(u32, u16)> = None;
    let mut lot_id: u64 = 0;
    let mut mail_ids: Vec<i64> = Vec::new();
    let mut team_id: u32 = 0;
    let mut consumed: HashSet<usize> = (0..game.state.social.len()).collect();
    // The login burst (the zone states of every conflict zone) is not part of the script.
    let mut printed = game.state.social.len();
    let mut last_npc = 0u32;
    let mut last_slave = 0u32;
    for (n, raw) in text.lines().enumerate() {
        for e in &game.state.social[printed.min(game.state.social.len())..] {
            println!("    social: {e:?}");
        }
        printed = game.state.social.len();
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        println!("> [{}] {line}", n + 1);
        let (cmd, rest) = line.split_once(' ').unwrap_or((line, ""));
        let parts: Vec<&str> = rest.split_whitespace().collect();
        match cmd {
            "gm" => {
                let reply = gm_command(game, rest).await?;
                // SCSlaveCreatedPacket: owner bc, tl_id u16, slave bc (`SLAVE` in `rawcs`).
                for p in reply.iter().filter(|p| p.is_sc(sc::SCSlaveCreatedPacket)) {
                    let mut st = p.stream();
                    if st.read_bc().is_ok() && st.read_u16().is_ok() {
                        last_slave = st.read_bc().unwrap_or(0);
                        println!("    slave created, obj {last_slave}");
                    }
                }
                for p in reply.iter().filter(|p| p.is_sc(sc::SCJoinedTeamPacket)) {
                    team_id = p.stream().read_u32()?;
                    println!("    joined team {team_id}");
                }
                show(&reply);
            }
            "gmq" => {
                // Fire and forget (0.4 s): for loops of GM commands whose output is not needed.
                game.chat(0, &format!("/{rest}")).await?;
                game.drain(Duration::from_millis(400), Duration::from_millis(900))
                    .await?;
            }
            "tp" => {
                let (x, y, z): (f32, f32, f32) =
                    (num(&parts, 0)?, num(&parts, 1)?, num(&parts, 2)?);
                game.state.last_teleport = None;
                game.chat(0, &format!("/move {x} {y} {z}")).await?;
                game.wait_for("SCTeleportUnitPacket", |p| {
                    p.is_sc(sc::SCTeleportUnitPacket)
                })
                .await?;
                let at = game.state.self_pos;
                game.teleport_ended(at).await?;
                clock += 1500;
                game.move_to(at, clock).await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(8))
                    .await?;
                println!(
                    "    teleported to {at:?}; {} units, {} doodads in sight",
                    game.state.units.len(),
                    game.state.doodads.len()
                );
                show(&got);
            }
            "pump" => show(&pump(game, num(&parts, 0)?).await?),
            "labor" => match game.state.labor {
                Some(labor) => println!("    labor {labor}"),
                None => println!("    labor unknown (no select snapshot decoded)"),
            },
            "units" => {
                let tpls: Vec<u32> = parts.iter().filter_map(|p| p.parse().ok()).collect();
                let me = game.state.self_pos;
                let mut us: Vec<&UnitInfo> = game
                    .state
                    .units
                    .values()
                    .filter(|u| {
                        u.unit_type == 1
                            && if tpls.is_empty() {
                                dist(u.pos, me) < 60.0
                            } else {
                                tpls.contains(&u.id)
                            }
                    })
                    .collect();
                us.sort_by(|a, b| dist(a.pos, me).total_cmp(&dist(b.pos, me)));
                println!("    me {me:?}, {} NPC(s):", us.len());
                if let (false, Some(first)) = (tpls.is_empty(), us.first()) {
                    last_npc = first.obj_id; // the default owner of `loot` / `roll`
                }
                for u in us {
                    println!(
                        "    npc tpl {} obj {} '{}' at ({:.1}, {:.1}, {:.1}) dist {:.1}",
                        u.id,
                        u.obj_id,
                        u.name,
                        u.pos.0,
                        u.pos.1,
                        u.pos.2,
                        dist(u.pos, me)
                    );
                }
            }
            "doodads" => {
                let tpls: Vec<u32> = parts.iter().filter_map(|p| p.parse().ok()).collect();
                let me = game.state.self_pos;
                let mut ds: Vec<_> = game
                    .state
                    .doodads
                    .values()
                    .filter(|d| {
                        if tpls.is_empty() {
                            dist(d.pos, me) < 40.0
                        } else {
                            tpls.contains(&d.template_id)
                        }
                    })
                    .collect();
                ds.sort_by(|a, b| dist(a.pos, me).total_cmp(&dist(b.pos, me)));
                println!("    me {me:?}, {} doodad(s):", ds.len());
                for d in ds {
                    println!(
                        "    doodad tpl {} obj {} at ({:.1}, {:.1}, {:.1}) phase {} dist {:.1}",
                        d.template_id,
                        d.obj_id,
                        d.pos.0,
                        d.pos.1,
                        d.pos.2,
                        d.func_group_id,
                        dist(d.pos, me)
                    );
                }
            }
            "sq" | "cq" => {
                let quest: u32 = num(&parts, 0)?;
                let tpl: u32 = num(&parts, 1)?;
                let mut obj = 0;
                if tpl != 0 {
                    let Some(npc) = nearest(game, tpl) else {
                        bail!("NPC {tpl} not in sight");
                    };
                    clock += 1500;
                    game.move_to((npc.pos.0 + 1.0, npc.pos.1, npc.pos.2), clock)
                        .await?;
                    game.drain(Duration::from_millis(300), Duration::from_secs(1))
                        .await?;
                    obj = npc.obj_id;
                }
                if cmd == "sq" {
                    game.start_quest(quest, obj).await?;
                } else {
                    let selected: i32 = parts.get(2).and_then(|x| x.parse().ok()).unwrap_or(0);
                    let mut b = aaemu_commons::PacketStream::new();
                    b.write_u32(quest)
                        .write_bc(obj)
                        .write_bc(0)
                        .write_i32(selected);
                    game.send_cs(
                        aaemu_game::opcodes::cs::CSCompleteQuestContextPacket,
                        b.as_bytes(),
                    )
                    .await?;
                }
                let got = game
                    .drain(Duration::from_millis(1200), Duration::from_secs(6))
                    .await?;
                show(&got);
            }
            "savenpc" => {
                std::fs::write(rest.trim(), last_npc.to_string())
                    .with_context(|| format!("writing {rest}"))?;
                println!("    saved npc obj {last_npc}");
            }
            "loadnpc" => {
                // The other bot wrote the object id (`savenpc`); wait up to 90 s for the file.
                let deadline = Instant::now() + Duration::from_secs(90);
                loop {
                    if let Ok(v) = std::fs::read_to_string(rest.trim()) {
                        if let Ok(obj) = v.trim().parse::<u32>() {
                            last_npc = obj;
                            println!("    loaded npc obj {obj}");
                            break;
                        }
                    }
                    if Instant::now() >= deadline {
                        bail!("{rest} did not appear within 90 s");
                    }
                    game.recv_within(Duration::from_millis(500)).await?;
                }
            }
            "castlast" => {
                let skill: u32 = num(&parts, 0)?;
                let Some(npc) = game.state.units.get(&last_npc).cloned() else {
                    println!("    NPC obj {last_npc} is gone (killed?), nothing to cast at");
                    continue;
                };
                if game.state.dead_units.contains(&last_npc) {
                    println!("    NPC obj {last_npc} is dead, nothing to cast at");
                    continue;
                }
                attack(game, &mut clock, skill, &npc).await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(8))
                    .await?;
                show(&got);
            }
            "targetlast" => {
                if last_npc == 0 {
                    bail!("no NPC remembered: run `units <template>` first");
                }
                game.change_target(last_npc).await?;
                game.drain(Duration::from_millis(300), Duration::from_secs(1))
                    .await?;
                println!("    target obj {last_npc}");
            }
            "target" if matches!(parts.first().copied(), Some("self" | "peer")) => {
                let obj = if parts[0] == "self" {
                    game.state.self_obj
                } else {
                    peer(game, None)?.obj_id
                };
                game.change_target(obj).await?;
                game.drain(Duration::from_millis(300), Duration::from_secs(1))
                    .await?;
                println!("    target obj {obj}");
            }
            "target" => {
                let tpl: u32 = num(&parts, 0)?;
                let npc = nearest(game, tpl).with_context(|| format!("NPC {tpl} not in sight"))?;
                game.change_target(npc.obj_id).await?;
                println!("    target obj {}", npc.obj_id);
            }
            "kill" => {
                let tpl: u32 = num(&parts, 0)?;
                let npc = nearest(game, tpl).with_context(|| format!("NPC {tpl} not in sight"))?;
                clock += 1500;
                game.move_to((npc.pos.0 + 1.0, npc.pos.1, npc.pos.2), clock)
                    .await?;
                game.change_target(npc.obj_id).await?;
                last_npc = npc.obj_id;
                game.drain(Duration::from_millis(300), Duration::from_secs(1))
                    .await?;
                let got = gm_command(game, "kill").await?;
                println!("    killed obj {} (tpl {tpl})", npc.obj_id);
                show(&got);
            }
            "cast" => {
                let skill: u32 = num(&parts, 0)?;
                let tpl: u32 = num(&parts, 1)?;
                let npc = nearest(game, tpl).with_context(|| format!("NPC {tpl} not in sight"))?;
                clock += 1500;
                game.move_to((npc.pos.0 + 1.0, npc.pos.1, npc.pos.2), clock)
                    .await?;
                game.change_target(npc.obj_id).await?;
                last_npc = npc.obj_id;
                game.start_skill(skill, npc.obj_id).await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(8))
                    .await?;
                show(&got);
            }
            "dcast" => {
                let skill: u32 = num(&parts, 0)?;
                let tpl: u32 = num(&parts, 1)?;
                let me = game.state.self_pos;
                let d = game
                    .state
                    .doodads
                    .values()
                    .filter(|d| d.template_id == tpl)
                    .min_by(|a, b| dist(a.pos, me).total_cmp(&dist(b.pos, me)))
                    .cloned()
                    .with_context(|| format!("doodad {tpl} not in sight"))?;
                if parts.get(2).copied() != Some("far") {
                    clock += 1500;
                    game.move_to((d.pos.0 + 1.0, d.pos.1, d.pos.2), clock)
                        .await?;
                }
                game.start_doodad_skill(skill, d.obj_id).await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(8))
                    .await?;
                show(&got);
            }
            "item" => {
                let tpl: u32 = num(&parts, 0)?;
                let count: u32 = num(&parts, 1)?;
                let reply = gm_command(game, &format!("item add self {tpl} {count}")).await?;
                if let Some(id) = stack_id(&reply, tpl) {
                    items.insert(tpl, id);
                    println!("    item {tpl} stack id {id}");
                }
                show(&reply);
            }
            "icast" => {
                let skill: u32 = num(&parts, 0)?;
                let item_tpl: u32 = num(&parts, 1)?;
                let item = *items
                    .get(&item_tpl)
                    .with_context(|| format!("item {item_tpl} not added with `item`"))?;
                let target = match parts.get(2).copied() {
                    Some("self") | None => game.state.self_obj,
                    Some(t) => {
                        let tpl: u32 = t.parse()?;
                        let npc = nearest(game, tpl)
                            .with_context(|| format!("NPC {tpl} not in sight"))?;
                        clock += 1500;
                        game.move_to((npc.pos.0 + 1.0, npc.pos.1, npc.pos.2), clock)
                            .await?;
                        game.change_target(npc.obj_id).await?;
                        npc.obj_id
                    }
                };
                game.start_item_skill(skill, item, item_tpl, target).await?;
                let got = game
                    .drain(Duration::from_millis(2000), Duration::from_secs(10))
                    .await?;
                for p in &got {
                    if p.is_sc(sc::SCSkillStartedPacket) {
                        println!("    SCSkillStarted");
                    }
                    if p.is_sc(sc::SCSkillEndedPacket) {
                        println!("    SCSkillEnded");
                    }
                    if let Ok(res) = skill_response_result(p) {
                        println!("    {} result {res}", p.name());
                    }
                }
                show(&got);
            }
            "walk" => {
                // Steps of 4 m every 800 ms (5 m/s).
                let to: (f32, f32, f32) = (num(&parts, 0)?, num(&parts, 1)?, num(&parts, 2)?);
                let mut at = game.state.self_pos;
                while dist(at, to) > 0.5 {
                    let d = dist(at, to);
                    let k = (4.0 / d).min(1.0);
                    at = (
                        at.0 + (to.0 - at.0) * k,
                        at.1 + (to.1 - at.1) * k,
                        at.2 + (to.2 - at.2) * k,
                    );
                    clock += 800;
                    game.move_to(at, clock).await?;
                    show_quiet(game, 0.8).await?;
                }
                println!("    walked to {:?}", game.state.self_pos);
            }
            "mate" => {
                let reply =
                    gm_command(game, &format!("item add self {} 1", cfg.mate_item_template))
                        .await?;
                let item =
                    stack_id(&reply, cfg.mate_item_template).context("mate item was not added")?;
                let view = super::summon_mate(game, cfg, item, None).await?;
                mate = Some((view.obj_id, view.tl_id));
                println!(
                    "    mate obj {} tl {} hp {}",
                    view.obj_id, view.tl_id, view.hp
                );
            }
            "mount" => {
                let (_, tl) = mate.context("no mate: run `mate` first")?;
                let mut b = aaemu_commons::PacketStream::new();
                b.write_u16(tl).write_u8(1).write_u8(0);
                game.send_cs(aaemu_game::opcodes::cs::CSMountMatePacket, b.as_bytes())
                    .await?;
                let got = game
                    .drain(Duration::from_millis(1000), Duration::from_secs(4))
                    .await?;
                show(&got);
            }
            "mwalk" => {
                let (obj, _) = mate.context("no mate: run `mate` first")?;
                let to: (f32, f32, f32) = (num(&parts, 0)?, num(&parts, 1)?, num(&parts, 2)?);
                let mut at = game
                    .state
                    .units
                    .get(&obj)
                    .map_or(game.state.self_pos, |u| u.pos);
                while dist(at, to) > 0.5 {
                    let d = dist(at, to);
                    let k = (4.0 / d).min(1.0);
                    at = (
                        at.0 + (to.0 - at.0) * k,
                        at.1 + (to.1 - at.1) * k,
                        at.2 + (to.2 - at.2) * k,
                    );
                    clock += 800;
                    game.move_obj_to(obj, at, clock).await?;
                    show_quiet(game, 0.8).await?;
                }
                println!("    mate walked to {at:?}");
            }
            "equip" => {
                let tpl: u32 = num(&parts, 0)?;
                let equip_slot: u8 = num(&parts, 1)?;
                let reply = gm_command(game, &format!("item add self {tpl} 1")).await?;
                show(&reply);
                let item = game
                    .state
                    .bag
                    .values()
                    .copied()
                    .find(|i| i.template_id == tpl)
                    .with_context(|| format!("item {tpl} not in the bag after /item add"))?;
                game.equip_item(item.item_id, item.slot, equip_slot).await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(6))
                    .await?;
                show(&got);
            }
            "buffs" => {
                let own = game.state.self_obj;
                let mut ids: Vec<u32> = game
                    .state
                    .buffs
                    .get(&own)
                    .map(|b| b.values().map(|i| i.buff_id).collect())
                    .unwrap_or_default();
                ids.sort_unstable();
                println!("    buffs on self ({}): {ids:?}", ids.len());
            }
            "bag" => {
                let mut slots: Vec<(u8, crate::inventory::ItemInfo)> = game
                    .state
                    .bag
                    .iter()
                    .map(|(slot, item)| (*slot, *item))
                    .collect();
                slots.sort_by_key(|(slot, _)| *slot);
                println!(
                    "    {} bag item(s), {} slot(s):",
                    slots.len(),
                    game.state.bag_slots
                );
                for (slot, item) in slots {
                    println!(
                        "    slot {slot} id {} tpl {} count {}",
                        item.item_id, item.template_id, item.count
                    );
                }
            }
            "split" => {
                let from_slot: u8 = num(&parts, 0)?;
                let to_slot: u8 = num(&parts, 1)?;
                let count: i32 = num(&parts, 2)?;
                let item = game
                    .state
                    .bag
                    .get(&from_slot)
                    .copied()
                    .with_context(|| format!("no bag item in slot {from_slot}"))?;
                let mut b = aaemu_commons::PacketStream::new();
                b.write_u64(item.item_id)
                    .write_u64(0)
                    .write_u8(2)
                    .write_u8(from_slot)
                    .write_u8(2)
                    .write_u8(to_slot)
                    .write_i32(count);
                game.send_cs(aaemu_game::opcodes::cs::CSSplitBagItemPacket, b.as_bytes())
                    .await?;
                let got = game
                    .drain(Duration::from_millis(1200), Duration::from_secs(6))
                    .await?;
                show(&got);
            }
            "swap" => {
                let from_slot: u8 = num(&parts, 0)?;
                let to_slot: u8 = num(&parts, 1)?;
                let from = game
                    .state
                    .bag
                    .get(&from_slot)
                    .copied()
                    .with_context(|| format!("no bag item in slot {from_slot}"))?;
                let to_item = game.state.bag.get(&to_slot).map_or(0, |i| i.item_id);
                game.swap_items(from.item_id, from_slot, to_item, to_slot)
                    .await?;
                let got = game
                    .drain(Duration::from_millis(1200), Duration::from_secs(6))
                    .await?;
                show(&got);
            }
            "deposit" | "withdraw" => {
                let amount: i32 = num(&parts, 0)?;
                let mut b = aaemu_commons::PacketStream::new();
                b.write_i32(amount).write_i32(0);
                let opcode = if cmd == "deposit" {
                    aaemu_game::opcodes::cs::CSDepositMoneyPacket
                } else {
                    aaemu_game::opcodes::cs::CSWithdrawMoneyPacket
                };
                game.send_cs(opcode, b.as_bytes()).await?;
                let got = game
                    .drain(Duration::from_millis(1200), Duration::from_secs(6))
                    .await?;
                show(&got);
            }
            "expand" => {
                let slot_type: i32 = parts.first().and_then(|x| x.parse().ok()).unwrap_or(2);
                let mut b = aaemu_commons::PacketStream::new();
                b.write_i32(slot_type).write_bool(false);
                game.send_cs(aaemu_game::opcodes::cs::CSExpandSlotsPacket, b.as_bytes())
                    .await?;
                let got = game
                    .drain(Duration::from_millis(1200), Duration::from_secs(6))
                    .await?;
                show(&got);
                println!("    bag slots now {}", game.state.bag_slots);
            }
            "sell" => {
                let slot: u8 = num(&parts, 0)?;
                let tpl: u32 = num(&parts, 1)?;
                let npc = nearest(game, tpl).with_context(|| format!("NPC {tpl} not in sight"))?;
                let item = game
                    .state
                    .bag
                    .get(&slot)
                    .copied()
                    .with_context(|| format!("no bag item in slot {slot}"))?;
                clock += 1500;
                game.move_to((npc.pos.0 + 1.0, npc.pos.1, npc.pos.2), clock)
                    .await?;
                game.drain(Duration::from_millis(300), Duration::from_secs(1))
                    .await?;
                let mut b = aaemu_commons::PacketStream::new();
                b.write_bc(npc.obj_id)
                    .write_bc(0)
                    .write_u8(1)
                    .write_u8(2)
                    .write_u8(slot)
                    .write_u64(item.item_id)
                    .write_u32(0);
                game.send_cs(aaemu_game::opcodes::cs::CSSellItemsPacket, b.as_bytes())
                    .await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(8))
                    .await?;
                show(&got);
            }
            "others" => {
                let me = game.state.self_obj;
                for u in game
                    .state
                    .units
                    .values()
                    .filter(|u| u.unit_type == 0 && u.obj_id != me)
                {
                    println!("    player '{}' obj {} char {}", u.name, u.obj_id, u.id);
                }
            }
            "tstart" | "taccept" | "tput" | "tmoney" | "tlock" | "tok" | "tcancel" => {
                use aaemu_game::opcodes::cs;
                let mut b = aaemu_commons::PacketStream::new();
                let op = match cmd {
                    "tstart" => {
                        let me = game.state.self_obj;
                        let peer = game
                            .state
                            .units
                            .values()
                            .find(|u| u.unit_type == 0 && u.obj_id != me)
                            .context("no other player in sight")?
                            .obj_id;
                        b.write_bc(peer);
                        cs::CSCanStartTradePacket
                    }
                    "taccept" => {
                        let p = game
                            .wait_for("SCCanStartTradePacket", |p| {
                                p.is_sc(sc::SCCanStartTradePacket)
                            })
                            .await?;
                        let obj = p.stream().read_bc()?;
                        b.write_bc(obj);
                        cs::CSStartTradePacket
                    }
                    "tput" => {
                        b.write_u8(num(&parts, 0)?)
                            .write_u8(num(&parts, 1)?)
                            .write_i32(num(&parts, 2)?);
                        cs::CSPutupTradeItemPacket
                    }
                    "tmoney" => {
                        b.write_i32(num(&parts, 0)?);
                        cs::CSPutupTradeMoneyPacket
                    }
                    "tlock" => {
                        b.write_bool(num::<u8>(&parts, 0)? != 0);
                        cs::CSTradeLockPacket
                    }
                    "tok" => cs::CSTradeOkPacket,
                    _ => {
                        b.write_i32(0);
                        cs::CSCancelTradePacket
                    }
                };
                game.send_cs(op, b.as_bytes()).await?;
                let got = game
                    .drain(Duration::from_millis(1000), Duration::from_secs(4))
                    .await?;
                show(&got);
            }
            "me" => {
                let info = game.state.self_info.clone();
                let obj = game.state.self_obj;
                let faction = game
                    .state
                    .social
                    .iter()
                    .rev()
                    .find_map(|e| match e {
                        SocialEvent::FactionChanged { obj_id, new, .. } if *obj_id == obj => {
                            Some(*new)
                        }
                        _ => None,
                    })
                    .or(info.as_ref().map(|i| i.faction));
                match info {
                    Some(i) => println!(
                        "    me '{}' char {} obj {obj} level {} faction {faction:?}",
                        i.name, i.id, i.level
                    ),
                    None => println!("    me obj {obj} (no character head decoded)"),
                }
            }
            "waitpeer" => {
                let secs: u64 = parts.first().and_then(|x| x.parse().ok()).unwrap_or(60);
                let deadline = Instant::now() + Duration::from_secs(secs);
                loop {
                    if let Ok(p) = peer(game, parts.get(1).copied()) {
                        println!("    peer '{}' char {} obj {}", p.name, p.id, p.obj_id);
                        break;
                    }
                    if Instant::now() >= deadline {
                        bail!("no other player in sight after {secs} s");
                    }
                    game.recv_within(Duration::from_millis(500)).await?;
                }
            }
            "await" => {
                let kind = parts.first().context("await needs an event name")?;
                let secs: u64 = parts.get(1).and_then(|x| x.parse().ok()).unwrap_or(60);
                let e = await_event(game, &mut consumed, kind, secs).await?;
                println!("    awaited: {e:?}");
            }
            "tryawait" => {
                let kind = parts.first().context("tryawait needs an event name")?;
                let secs: u64 = parts.get(1).and_then(|x| x.parse().ok()).unwrap_or(10);
                match await_event(game, &mut consumed, kind, secs).await {
                    Ok(e) => println!("    awaited: {e:?}"),
                    Err(e) => println!("    no {kind} within {secs} s ({e})"),
                }
            }
            "events" => {
                for (i, e) in game.state.social.iter().enumerate() {
                    println!("    #{i} {e:?}");
                }
            }
            "duel" => {
                let target = peer(game, parts.first().copied())?;
                let (op, body) = social::challenge_duel(target.id);
                game.send_cs(op, &body).await?;
                println!("    challenged '{}' (char {})", target.name, target.id);
                game.drain(Duration::from_millis(600), Duration::from_secs(3))
                    .await?;
            }
            "daccept" | "drefuse" => {
                let SocialEvent::DuelChallenged { challenger_id } =
                    await_event(game, &mut consumed, "DuelChallenged", 60).await?
                else {
                    bail!("not a duel challenge");
                };
                let err = if cmd == "daccept" { 0 } else { 507 };
                let (op, body) = social::start_duel(challenger_id, err);
                game.send_cs(op, &body).await?;
                println!("    answered challenger {challenger_id} with {err}");
                game.drain(Duration::from_millis(600), Duration::from_secs(3))
                    .await?;
            }
            "dcancel" => {
                let id = game
                    .state
                    .self_info
                    .as_ref()
                    .context("no own character")?
                    .id;
                let (op, body) = social::start_duel(id, 507);
                game.send_cs(op, &body).await?;
                game.drain(Duration::from_millis(600), Duration::from_secs(3))
                    .await?;
            }
            "dwait" => {
                let secs: u64 = parts.first().and_then(|x| x.parse().ok()).unwrap_or(400);
                let e = await_event(game, &mut consumed, "DuelEnded", secs).await?;
                if let SocialEvent::DuelEnded {
                    challenger_id,
                    challenged_id,
                    det,
                    ..
                } = e
                {
                    let me = game.state.self_info.as_ref().map_or(0, |i| i.id);
                    let verdict = match det {
                        3 => "draw",
                        _ if challenger_id == me => "I won",
                        _ if challenged_id == me => "I lost",
                        _ => "?",
                    };
                    println!(
                        "    duel ended det {det} (first {challenger_id}, second {challenged_id}): {verdict}"
                    );
                }
            }
            "party" => {
                let target = peer(game, parts.first().copied())?;
                let (op, body) = social::invite_to_team(0, true, &target.name);
                game.send_cs(op, &body).await?;
                println!("    invited '{}'", target.name);
                game.drain(Duration::from_millis(600), Duration::from_secs(3))
                    .await?;
            }
            "paccept" | "preject" => {
                let SocialEvent::TeamAsk {
                    team_id,
                    sender_id,
                    is_party,
                    ..
                } = await_event(game, &mut consumed, "TeamAsk", 60).await?
                else {
                    bail!("not a team invitation");
                };
                let name = game
                    .state
                    .self_info
                    .as_ref()
                    .map_or(String::new(), |i| i.name.clone());
                let (op, body) = social::reply_to_join_team(
                    team_id,
                    is_party,
                    sender_id,
                    cmd == "preject",
                    &name,
                );
                game.send_cs(op, &body).await?;
                game.drain(Duration::from_millis(800), Duration::from_secs(3))
                    .await?;
            }
            "pleave" | "pdismiss" => {
                let team = current_team(game).context("not in a team (no SCJoinedTeam seen)")?;
                let me = game
                    .state
                    .self_info
                    .as_ref()
                    .context("no own character")?
                    .id;
                let action = if cmd == "pdismiss" { 2 } else { 0 };
                let (op, body) = social::risky_team_action(team, me, action);
                game.send_cs(op, &body).await?;
                game.drain(Duration::from_millis(800), Duration::from_secs(3))
                    .await?;
            }
            "plr" => {
                let team = current_team(game).context("not in a team (no SCJoinedTeam seen)")?;
                let (method, grade, bop): (u8, u8, u8) =
                    (num(&parts, 0)?, num(&parts, 1)?, num(&parts, 2)?);
                let (op, body) =
                    social::change_looting_rule(team, method, grade, 0, bop != 0, 0x0F);
                game.send_cs(op, &body).await?;
                game.drain(Duration::from_millis(800), Duration::from_secs(3))
                    .await?;
            }
            "pcast" => {
                let skill: u32 = num(&parts, 0)?;
                let target = peer(game, parts.get(1).copied())?;
                attack(game, &mut clock, skill, &target).await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(8))
                    .await?;
                show(&got);
            }
            "pfight" => {
                let skill: u32 = num(&parts, 0)?;
                let secs: u64 = num(&parts, 1)?;
                let target = peer(game, parts.get(2).copied())?;
                let me = game.state.self_obj;
                let deadline = Instant::now() + Duration::from_secs(secs);
                let mut casts = 0u32;
                let social_from = game.state.social.len();
                let outcome = loop {
                    if game.state.social[social_from..]
                        .iter()
                        .any(|e| matches!(e, SocialEvent::DuelEnded { .. }))
                    {
                        break "duel ended";
                    }
                    if game.state.dead_units.contains(&target.obj_id) {
                        break "peer died";
                    }
                    if game.state.dead_units.contains(&me) {
                        break "I died";
                    }
                    if Instant::now() >= deadline {
                        break "timed out";
                    }
                    let cur = game
                        .state
                        .units
                        .get(&target.obj_id)
                        .cloned()
                        .unwrap_or_else(|| target.clone());
                    attack(game, &mut clock, skill, &cur).await?;
                    casts += 1;
                    game.drain(Duration::from_millis(1200), Duration::from_millis(1800))
                        .await?;
                };
                println!(
                    "    pfight: {outcome} after {casts} cast(s); peer hp {:?}, my hp {:?}",
                    game.state.unit_points.get(&target.obj_id),
                    game.state.unit_points.get(&me)
                );
            }
            "pkill" => {
                let target = peer(game, parts.first().copied())?;
                clock += 1500;
                game.move_to((target.pos.0 + 1.0, target.pos.1, target.pos.2), clock)
                    .await?;
                game.change_target(target.obj_id).await?;
                game.drain(Duration::from_millis(300), Duration::from_secs(1))
                    .await?;
                let got = gm_command(game, "kill").await?;
                show(&got);
            }
            "loot" => {
                let obj: u32 = parts
                    .first()
                    .and_then(|x| x.parse().ok())
                    .unwrap_or(last_npc);
                if obj == 0 {
                    bail!("no NPC to loot: run `kill`/`cast` first or pass the object id");
                }
                game.loot_all(obj).await?;
                let got = game
                    .drain(Duration::from_millis(1000), Duration::from_secs(4))
                    .await?;
                show(&got);
            }
            "roll" => {
                let roll = match parts.first().copied() {
                    Some("roll") => true,
                    Some("pass") => false,
                    _ => bail!("roll roll|pass [index] [npc obj]"),
                };
                let index: u16 = parts.get(1).and_then(|x| x.parse().ok()).unwrap_or(0);
                let obj: u32 = parts
                    .get(2)
                    .and_then(|x| x.parse().ok())
                    .unwrap_or(last_npc);
                let (op, body) = social::loot_dice(index, 1, obj, roll);
                game.send_cs(op, &body).await?;
                let got = game
                    .drain(Duration::from_millis(1000), Duration::from_secs(4))
                    .await?;
                show(&got);
            }
            "gcreate" => {
                let (op, body) = social::create_expedition(rest.trim());
                game.send_cs(op, &body).await?;
                game.drain(Duration::from_millis(800), Duration::from_secs(4))
                    .await?;
            }
            "ginvite" => {
                let target = peer(game, parts.first().copied())?;
                let (op, body) = social::invite_to_expedition(&target.name);
                game.send_cs(op, &body).await?;
                game.drain(Duration::from_millis(800), Duration::from_secs(3))
                    .await?;
            }
            "gaccept" => {
                let SocialEvent::GuildInvite { faction_id, .. } =
                    await_event(game, &mut consumed, "GuildInvite", 60).await?
                else {
                    bail!("not a guild invitation");
                };
                let (op, body) = social::reply_expedition_invitation(faction_id, true);
                game.send_cs(op, &body).await?;
                game.drain(Duration::from_millis(800), Duration::from_secs(3))
                    .await?;
            }
            "grole" => {
                let (role, id): (u8, u32) = (num(&parts, 0)?, num(&parts, 1)?);
                let (op, body) = social::change_expedition_member_role(role, id);
                game.send_cs(op, &body).await?;
                game.drain(Duration::from_millis(800), Duration::from_secs(3))
                    .await?;
            }
            "drop" => {
                r.end(s, "connection dropped without logout");
                return Ok(());
            }
            "loaded" => {
                // The real client's answer to SCLoadInstancePacket, then the world settles.
                game.confirm_instance_loaded().await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(6))
                    .await?;
                show(&got);
            }
            "now" => {
                let t = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0.0, |d| d.as_secs_f64());
                println!("    now {t:.1}");
            }
            "useportal" => {
                let tpl: u32 = num(&parts, 0)?;
                let only_mine = parts.get(1).is_some_and(|v| *v == "1");
                let npc = nearest(game, tpl).with_context(|| format!("NPC {tpl} not in sight"))?;
                let mut b = aaemu_commons::PacketStream::new();
                b.write_bc(npc.obj_id).write_bool(only_mine);
                game.send_cs(aaemu_game::opcodes::cs::CSUsePortalPacket, b.as_bytes())
                    .await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(8))
                    .await?;
                show(&got);
            }
            "delportal" => {
                let kind: u8 = num(&parts, 0)?;
                let id: u32 = num(&parts, 1)?;
                let mut b = aaemu_commons::PacketStream::new();
                b.write_u8(kind).write_u32(id);
                game.send_cs(aaemu_game::opcodes::cs::CSDeletePortalPacket, b.as_bytes())
                    .await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(6))
                    .await?;
                show(&got);
            }
            "portalsave" => {
                let name = parts.join(" ");
                let mut o = aaemu_commons::PacketStream::new();
                o.write_u8(2).write_i32(0).write_str(&name);
                game.start_skill_with_object(11215, o.as_bytes()).await?;
                let got = game
                    .drain(Duration::from_millis(2000), Duration::from_secs(12))
                    .await?;
                show(&got);
            }
            "portalopen" => {
                let kind: u8 = num(&parts, 0)?;
                let id: i32 = num(&parts, 1)?;
                let (dx, dy): (f32, f32) = (num(&parts, 2)?, num(&parts, 3)?);
                let at = game.state.self_pos;
                let mut o = aaemu_commons::PacketStream::new();
                o.write_u8(1)
                    .write_u8(kind)
                    .write_i32(id)
                    .write_i64(aaemu_commons::helpers::convert_coord_to_long(at.0 + dx))
                    .write_i64(aaemu_commons::helpers::convert_coord_to_long(at.1 + dy))
                    .write_f32(at.2);
                game.start_skill_with_object(11216, o.as_bytes()).await?;
                let got = game
                    .drain(Duration::from_millis(2000), Duration::from_secs(14))
                    .await?;
                show(&got);
            }
            "rawcs" => {
                let opcode = u16::from_str_radix(parts.first().context("opcode")?, 16)?;
                // `SELF` / `SLAVE` stand for our own / the last created slave's object id in the
                // packet's `bc` encoding.
                let bc = |id: u32| {
                    format!(
                        "{:02x}{:02x}{:02x}",
                        id & 0xff,
                        (id >> 8) & 0xff,
                        (id >> 16) & 0xff
                    )
                };
                let hex = parts
                    .get(1)
                    .copied()
                    .unwrap_or("")
                    .replace("SELF", &bc(game.state.self_obj))
                    .replace("SLAVE", &bc(last_slave));
                let body: Vec<u8> = (0..hex.len() / 2)
                    .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16))
                    .collect::<Result<_, _>>()?;
                game.send_cs(opcode, &body).await?;
                let got = game
                    .drain(Duration::from_millis(1200), Duration::from_secs(6))
                    .await?;
                show(&got);
            }
            "aucpost" => {
                let slot: u8 = num(&parts, 0)?;
                let buyout: i32 = num(&parts, 1)?;
                let duration: u8 = num(&parts, 2)?;
                let item = game
                    .state
                    .bag
                    .get(&slot)
                    .copied()
                    .with_context(|| format!("no bag item in slot {slot}"))?;
                let mut b = aaemu_commons::PacketStream::new();
                b.write_bc(0x200)
                    .write_bc(0x200)
                    .write_u64(item.item_id)
                    .write_i32(0)
                    .write_i32(buyout)
                    .write_u8(duration);
                game.send_cs(aaemu_game::opcodes::cs::CSAuctionPostPacket, b.as_bytes())
                    .await?;
                let got = game
                    .drain(Duration::from_millis(1200), Duration::from_secs(6))
                    .await?;
                for p in got.iter().filter(|p| p.is_sc(sc::SCAuctionPostedPacket)) {
                    lot_id = p.stream().read_u64()?;
                    println!("    auction lot {lot_id}");
                }
                show(&got);
            }
            "auccancel" => {
                // CSCancelAuction carries the whole lot (`AuctionLot.Read`); the server only
                // uses its id.
                let mut b = aaemu_commons::PacketStream::new();
                b.write_bc(0x200).write_bc(0x200);
                b.write_u64(lot_id).write_u8(0);
                // empty item: template 0 reads only the template id
                b.write_u32(0);
                b.write_u8(1)
                    .write_u32(1)
                    .write_str("x")
                    .write_i32(0)
                    .write_i32(0);
                b.write_i64(0)
                    .write_u8(255)
                    .write_u32(0)
                    .write_str("")
                    .write_i32(0)
                    .write_i32(0);
                game.send_cs(aaemu_game::opcodes::cs::CSCancelAuctionPacket, b.as_bytes())
                    .await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(6))
                    .await?;
                show(&got);
            }
            "aucbid" => {
                // CSBidAuction: the lot (only its id is used) then the bid (lot id, money).
                let id: u64 = num(&parts, 0)?;
                let money: i32 = num(&parts, 1)?;
                let mut b = aaemu_commons::PacketStream::new();
                b.write_bc(0x200).write_bc(0x200);
                b.write_u64(id).write_u8(0);
                b.write_u32(0);
                b.write_u8(1)
                    .write_u32(1)
                    .write_str("x")
                    .write_i32(0)
                    .write_i32(0);
                b.write_i64(0)
                    .write_u8(255)
                    .write_u32(0)
                    .write_str("")
                    .write_i32(0)
                    .write_i32(0);
                b.write_u64(id)
                    .write_u8(1)
                    .write_u32(0)
                    .write_str("")
                    .write_i32(money);
                game.send_cs(aaemu_game::opcodes::cs::CSBidAuctionPacket, b.as_bytes())
                    .await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(6))
                    .await?;
                show(&got);
            }
            "interact" => {
                // CSInteractNPC (sets the current interaction object) then CSStartInteraction
                // (asks for the NPC's interaction skill list).
                let tpl: u32 = num(&parts, 0)?;
                let npc = nearest(game, tpl).with_context(|| format!("NPC {tpl} not in sight"))?;
                let mut b = aaemu_commons::PacketStream::new();
                b.write_bc(npc.obj_id).write_bool(false);
                game.send_cs(aaemu_game::opcodes::cs::CSInteractNPCPacket, b.as_bytes())
                    .await?;
                game.drain(Duration::from_millis(500), Duration::from_secs(3))
                    .await?;
                let mut b = aaemu_commons::PacketStream::new();
                b.write_bc(npc.obj_id)
                    .write_bc(0)
                    .write_i32(0)
                    .write_i32(0)
                    .write_u8(1)
                    .write_i32(0);
                game.send_cs(
                    aaemu_game::opcodes::cs::CSStartInteractionPacket,
                    b.as_bytes(),
                )
                .await?;
                let got = game
                    .drain(Duration::from_millis(1200), Duration::from_secs(6))
                    .await?;
                show(&got);
            }
            "repairall" => {
                let mut b = aaemu_commons::PacketStream::new();
                b.write_bool(false);
                game.send_cs(
                    aaemu_game::opcodes::cs::CSRepairAllEquipmentsPacket,
                    b.as_bytes(),
                )
                .await?;
                let got = game
                    .drain(Duration::from_millis(1200), Duration::from_secs(6))
                    .await?;
                show(&got);
            }
            "maillist" => {
                game.send_cs(aaemu_game::opcodes::cs::CSListMailPacket, &[])
                    .await?;
                let got = game
                    .drain(Duration::from_millis(1200), Duration::from_secs(6))
                    .await?;
                mail_ids.clear();
                for p in got.iter().filter(|p| p.is_sc(sc::SCMailListPacket)) {
                    let mut st = p.stream();
                    let _sent = st.read_bool()?;
                    let n = st.read_i32()?;
                    for _ in 0..n {
                        let id = st.read_i64()?;
                        let kind = st.read_u8()?;
                        let status = st.read_u8()?;
                        let title = st.read_string()?;
                        let sender = st.read_string()?;
                        let attachments = st.read_u8()?;
                        let _receiver = st.read_string()?;
                        let _open = st.read_i64()?;
                        let _returned = st.read_bool()?;
                        let _extra = st.read_i64()?;
                        println!(
                            "    mail {id} type {kind} status {status} '{title}' from '{sender}' attachments {attachments}"
                        );
                        mail_ids.push(id);
                    }
                }
                show(&got);
            }
            "mailtakeall" => {
                let n: usize = parts.first().and_then(|x| x.parse().ok()).unwrap_or(0);
                let id = *mail_ids
                    .get(n)
                    .context("no such listed mail (run maillist)")?;
                game.send_cs(
                    aaemu_game::opcodes::cs::CSTakeAllAttachmentItemPacket,
                    &id.to_le_bytes(),
                )
                .await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(8))
                    .await?;
                show(&got);
            }
            "partyinvite" => {
                let mut b = aaemu_commons::PacketStream::new();
                b.write_u32(0)
                    .write_bool(true)
                    .write_str(parts.first().context("name")?);
                game.send_cs(aaemu_game::opcodes::cs::CSInviteToTeamPacket, b.as_bytes())
                    .await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(6))
                    .await?;
                for p in got.iter().filter(|p| p.is_sc(sc::SCJoinedTeamPacket)) {
                    team_id = p.stream().read_u32()?;
                    println!("    joined team {team_id}");
                }
                show(&got);
            }
            "partyaccept" => {
                let p = game
                    .wait_for("SCAskToJoinTeamPacket", |p| {
                        p.is_sc(sc::SCAskToJoinTeamPacket)
                    })
                    .await?;
                let mut st = p.stream();
                let ask_team = st.read_u32()?;
                let sender = st.read_u32()?;
                let _sender_name = st.read_string()?;
                let is_party = st.read_bool()?;
                let mut b = aaemu_commons::PacketStream::new();
                b.write_u32(ask_team)
                    .write_bool(is_party)
                    .write_u32(sender)
                    .write_bool(false)
                    .write_str(
                        game.state
                            .self_info
                            .as_ref()
                            .map_or("", |c| c.name.as_str()),
                    )
                    .write_bool(false);
                game.send_cs(
                    aaemu_game::opcodes::cs::CSReplyToJoinTeamPacket,
                    b.as_bytes(),
                )
                .await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(6))
                    .await?;
                for p in got.iter().filter(|p| p.is_sc(sc::SCJoinedTeamPacket)) {
                    team_id = p.stream().read_u32()?;
                    println!("    joined team {team_id}");
                }
                show(&got);
            }
            "myid" => println!("    character id {}", game.state.selected_char),
            "teamact" => {
                anyhow::ensure!(
                    team_id != 0,
                    "no team yet (partyinvite / partyaccept first)"
                );
                let (action, target) = match parts.first().copied() {
                    Some("leave") => (0u8, num::<u32>(&parts, 1)?),
                    Some("kick") => (1u8, num::<u32>(&parts, 1)?),
                    Some("dismiss") => (2u8, 0),
                    _ => bail!("teamact <leave|kick|dismiss> <char id>"),
                };
                let mut b = aaemu_commons::PacketStream::new();
                b.write_u32(team_id).write_u32(target).write_u8(action);
                game.send_cs(
                    aaemu_game::opcodes::cs::CSAskRiskyTeamActionPacket,
                    b.as_bytes(),
                )
                .await?;
                let got = game
                    .drain(Duration::from_millis(1500), Duration::from_secs(6))
                    .await?;
                show(&got);
            }
            other => bail!("unknown script command '{other}' at line {}", n + 1),
        }
    }
    r.end(s, format!("{path} done"));

    let s = r.begin("game.leave");
    game.leave_world(0).await?;
    game.wait_sc(sc::SCPrepareLeaveWorldPacket).await.ok();
    game.drain(Duration::from_millis(300), Duration::from_secs(2))
        .await
        .ok();
    r.end(s, "left");
    Ok(())
}
