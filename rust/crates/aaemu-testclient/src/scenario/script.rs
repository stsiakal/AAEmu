//! `script`: runs the commands of the file named by `$AAEMU_SCRIPT` after the usual login /
//! select / spawn, printing what the server sends back. Meant for ad-hoc live checks of quest
//! and spawn fixes (GM account needed), so a new check needs no rebuild of the bot.
//!
//! One command per line, `#` starts a comment:
//!
//! ```text
//! gm <command line>                  GM chat command (without the leading slash)
//! tp <x> <y> <z>                     /move there, acknowledge the teleport, wait for the area
//! pump <secs>                        just read packets
//! labor                              the current labor power decoded from the server
//! units [<npc template>...]          NPCs in sight (all within 60 m when no template given)
//! doodads [<doodad template>...]     doodads in sight (all within 40 m when none given)
//! sq <quest> <npc template|0>        CSStartQuestContext from the nearest NPC of that template
//! cq <quest> <npc template|0> [sel]  CSCompleteQuestContext at that NPC
//! target <npc template>              select the nearest NPC of that template
//! kill <npc template>                walk next to the nearest NPC, target it, `/kill`
//! cast <skill> <npc template>        CSStartSkill on the nearest NPC
//! dcast <skill> <doodad template>    doodad-target CSStartSkill
//! item <template> <count>            `/item add self` and remember the stack id
//! icast <skill> <item template> <npc template|self>
//!                                    cast from the remembered item stack
//! bag                                list the bag contents (slot / id / template / count)
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
//! ```

use std::collections::HashMap;
use std::time::Duration;

use aaemu_game::opcodes::sc;
use anyhow::{bail, Context};

use super::questitem::{chat_text, gm_command, quest_state, stack_id};
use super::{pump, skill_response_result, Runner};
use crate::game::{parse_item_task, GameClient, Packet, UnitInfo};

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
            || name.contains("Gold")
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
    for (n, raw) in text.lines().enumerate() {
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
                show(&reply);
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
                clock += 1500;
                game.move_to((d.pos.0 + 1.0, d.pos.1, d.pos.2), clock)
                    .await?;
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
