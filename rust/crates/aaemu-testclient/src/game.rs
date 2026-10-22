//! Game server client (C2G / G2C + the proxy channel) for the 1.2 protocol.
//!
//! Every frame payload is `[0xDD][level]...`: level 1 (game) carries `[hash][count][u16 opcode]`,
//! level 2 (proxy / CryNetwork) just `[u16 opcode]`. 1.2 traffic is neither encrypted nor
//! compressed.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use aaemu_commons::helpers;
use aaemu_commons::net::FrameCodec;
use aaemu_commons::PacketStream;
use aaemu_game::opcodes::{cs, name_of, pp, sc};
use aaemu_game::systems::quests::model::status as quest_status;
use aaemu_game::systems::services::commands::COMMAND_PREFIX;
use aaemu_game::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};
use aaemu_game::systems::social::chat::chat_type;
use anyhow::{anyhow, bail, ensure, Context};
use futures::{SinkExt, StreamExt};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::inventory::{self, Bag, ItemInfo};
use crate::quest::{self, QuestInfo};
use crate::record::{Channel, Dir, Recorder};

pub const LEVEL_GAME: u8 = 1;
pub const LEVEL_PROXY: u8 = 2;
/// `CompressedGamePackets` (`DD04`).
pub const LEVEL_COMPRESSED: u8 = 4;

/// A received G2C / proxy packet.
#[derive(Debug)]
pub struct Packet {
    pub level: u8,
    pub opcode: u16,
    pub body: Vec<u8>,
}

impl Packet {
    pub fn name(&self) -> &'static str {
        packet_name(Dir::S2C, self.level, self.opcode)
    }
    pub fn is(&self, level: u8, opcode: u16) -> bool {
        self.level == level && self.opcode == opcode
    }
    pub fn is_sc(&self, opcode: u16) -> bool {
        self.is(LEVEL_GAME, opcode)
    }
    pub fn stream(&self) -> PacketStream {
        PacketStream::from_bytes(self.body.clone())
    }
}

/// C# constant name of a game opcode.
pub fn packet_name(dir: Dir, level: u8, opcode: u16) -> &'static str {
    let table = match (dir, level) {
        (_, LEVEL_PROXY) => pp::NAMES,
        (Dir::S2C, _) => sc::NAMES,
        (Dir::C2S, _) => cs::NAMES,
    };
    name_of(table, opcode).unwrap_or("Unknown")
}

/// Splits the inflated payload of a `CompressedGamePackets` batch
/// (`([u16 0][u16 opcode][body])*`) into its packets. The inner bodies carry no length, so each
/// one is measured by decoding its codec's fields; the first opcode whose body cannot be decoded
/// ends the split (the packets before it are returned).
pub fn split_compressed_batch(payload: &[u8]) -> Vec<(u16, Vec<u8>)> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 4 <= payload.len() {
        if payload[pos..pos + 2] != [0, 0] {
            break;
        }
        let opcode = u16::from_le_bytes([payload[pos + 2], payload[pos + 3]]);
        let Some(len) = inner_body_len(opcode, &payload[pos + 4..]) else {
            break;
        };
        if pos + 4 + len > payload.len() {
            break;
        }
        out.push((opcode, payload[pos + 4..pos + 4 + len].to_vec()));
        pos += 4 + len;
    }
    out
}

/// Wire length of one inner batch packet body, decoded from the body itself.
fn inner_body_len(opcode: u16, body: &[u8]) -> Option<usize> {
    let mut s = PacketStream::from_bytes(body.to_vec());
    match opcode {
        sc::SCUnitPointsPacket => {
            s.read_bc().ok()?;
            s.read_i32().ok()?;
            s.read_i32().ok()?;
        }
        sc::SCSkillStartedPacket => {
            s.read_u32().ok()?;
            s.read_u16().ok()?;
            skip_skill_caster(&mut s)?;
            skip_cast_target(&mut s)?;
            skip_skill_object(&mut s)?;
            s.read_u16().ok()?; // real cast time / 10
            s.read_u16().ok()?; // base cast time / 10
            s.read_u8().ok()?; // synergy
            let flags = s.read_u8().ok()?;
            if flags & 1 != 0 {
                s.read_u8().ok()?;
            }
            if flags & 2 != 0 {
                s.read_u16().ok()?;
            }
            if flags & 4 != 0 {
                s.read_u32().ok()?;
            }
        }
        sc::SCSkillFiredPacket => {
            s.read_u32().ok()?;
            s.read_u16().ok()?;
            skip_skill_caster(&mut s)?;
            skip_cast_target(&mut s)?;
            skip_skill_object(&mut s)?;
            s.read_i16().ok()?; // computed delay / 10 + 10
            s.read_i16().ok()?; // channeling time / 10 + 10
            s.read_u8().ok()?; // f
            s.read_u32().ok()?; // fire anim id
            s.read_u8().ok()?; // flag
        }
        sc::SCSkillEndedPacket => {
            s.read_u16().ok()?;
        }
        sc::SCSkillStoppedPacket => {
            s.read_bc().ok()?;
            s.read_u32().ok()?;
        }
        sc::SCCastingStoppedPacket => {
            s.read_u16().ok()?;
            s.read_u32().ok()?;
        }
        sc::SCBuffCreatedPacket => {
            skip_skill_caster(&mut s)?;
            s.read_u32().ok()?; // caster id
            s.read_bc().ok()?; // owner obj id
            s.read_u32().ok()?; // index
            s.read_u32().ok()?; // buff id
            s.read_u8().ok()?; // caster level
            s.read_i16().ok()?; // ab level
            s.read_u32().ok()?; // skill id
            s.read_pisc_w(4).ok()?; // charge, duration, 0, tick
        }
        sc::SCBuffRemovedPacket => {
            s.read_bc().ok()?;
            s.read_u32().ok()?;
        }
        sc::SCUnitDamagedPacket => {
            skip_cast_action(&mut s)?;
            skip_skill_caster(&mut s)?;
            s.read_bc().ok()?; // caster id
            s.read_bc().ok()?; // target id
            s.read_u8().ok()?; // crime state
            s.read_pisc_w(3).ok()?; // damage, absorbed, 0
            s.read_pisc_w(3).ok()?; // 0, 0, mana burn
            s.read_u8().ok()?; // holdable id
            s.read_u16().ok()?; // 288 | hit type
            s.read_u8().ok()?; // flag
            s.read_u8().ok()?; // result
        }
        sc::SCUnitHealedPacket => {
            skip_cast_action(&mut s)?;
            skip_skill_caster(&mut s)?;
            s.read_bc().ok()?; // target id
            s.read_u8().ok()?; // heal type
            s.read_u8().ok()?; // heal hit type
            s.read_i32().ok()?; // value
            s.read_i32().ok()?; // o
            s.read_u8().ok()?; // result
        }
        sc::SCUnitDeathPacket => {
            s.read_bc().ok()?;
            s.read_u8().ok()?; // kill reason
            s.read_i32().ok()?; // resurrect wait
            s.read_i32().ok()?; // lost exp
            s.read_u8().ok()?; // durability loss rate
            let killer = s.read_bc().ok()?;
            if killer != 0 {
                s.read_u8().ok()?; // game type
                s.read_u16().ok()?; // kill streak
                s.read_u8().ok()?;
                s.read_u8().ok()?;
                s.read_u8().ok()?;
                s.read_string().ok()?; // killer name
            }
        }
        sc::SCAiAggroPacket => {
            s.read_bc().ok()?;
            let count = s.read_i32().ok()?;
            if count > 0 {
                s.read_bc().ok()?; // hostile unit id
                s.read_i32().ok()?; // summarize damage
                s.read_i32().ok()?; // val2
                s.read_i32().ok()?; // val3
                s.read_u8().ok()?; // top flags
            }
        }
        sc::SCSkillCooldownResetPacket => {
            s.read_bc().ok()?;
            s.read_u32().ok()?; // skill id
            s.read_u32().ok()?; // tag id
            s.read_bool().ok()?;
        }
        sc::SCPlotEventPacket => {
            s.read_u16().ok()?; // tl
            s.read_u32().ok()?; // event id
            s.read_u32().ok()?; // skill id
            skip_plot_object(&mut s)?; // caster
            skip_plot_object(&mut s)?; // target
            s.read_u64().ok()?; // item id
            s.read_bc().ok()?; // obj id
            s.read_u16().ok()?; // casting time
            s.read_bc().ok()?;
            s.read_i16().ok()?;
            let targets = s.read_u8().ok()?;
            for _ in 0..targets {
                s.read_bc().ok()?;
            }
            let flag = s.read_u8().ok()?;
            if (flag >> 3) & 1 == 1 {
                for _ in 0..13 {
                    s.read_i32().ok()?;
                }
            }
        }
        sc::SCPlotEndedPacket => {
            s.read_u16().ok()?;
        }
        sc::SCPlotCastingStoppedPacket | sc::SCPlotChannelingStoppedPacket => {
            s.read_u16().ok()?; // tl
            s.read_u32().ok()?; // duration
            s.read_u8().ok()?; // last event
        }
        _ => return None,
    }
    Some(s.pos())
}

fn skip_skill_caster(s: &mut PacketStream) -> Option<()> {
    let kind = s.read_u8().ok()?;
    SkillCaster::read(kind, s).ok()?;
    Some(())
}

fn skip_cast_target(s: &mut PacketStream) -> Option<()> {
    let kind = s.read_u8().ok()?;
    SkillCastTarget::read(kind, s).ok()?;
    Some(())
}

fn skip_skill_object(s: &mut PacketStream) -> Option<()> {
    let flag = s.read_u8().ok()?;
    SkillObject::read(flag, s).ok()?;
    Some(())
}

fn skip_cast_action(s: &mut PacketStream) -> Option<()> {
    match s.read_u8().ok()? {
        0 => {
            s.read_u32().ok()?;
            s.read_u16().ok()?;
        }
        1 => {
            s.read_u32().ok()?;
            s.read_u16().ok()?;
            s.read_u32().ok()?;
            s.read_u32().ok()?;
        }
        2 => {
            s.read_u32().ok()?;
            s.read_bc().ok()?;
            s.read_u32().ok()?;
            s.read_bool().ok()?;
            s.read_bool().ok()?;
        }
        3 => {
            s.read_i32().ok()?;
            s.read_i32().ok()?;
            s.read_bc().ok()?;
        }
        4 => {
            s.read_i32().ok()?;
            s.read_bc().ok()?;
        }
        _ => return None,
    }
    Some(())
}

/// `PlotObject`: `[u8 1][bc objId]` for a unit, `[u8 2][9-byte position][i8 roll][i8 pitch]
/// [i8 yaw]` for a position.
fn skip_plot_object(s: &mut PacketStream) -> Option<()> {
    match s.read_u8().ok()? {
        1 => {
            s.read_bc().ok()?;
        }
        2 => {
            s.read_position().ok()?;
            s.read_i8().ok()?;
            s.read_i8().ok()?;
            s.read_i8().ok()?;
        }
        _ => return None,
    }
    Some(())
}

/// A character of `SCCharacterListPacket` / `SCCreateCharacterResponsePacket`. The first
/// fields are the `Character.Write` head (`models/character.rs`), so they can be read without
/// decoding the variable equipment / model data that follows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CharInfo {
    pub id: u32,
    pub name: String,
    pub race: u8,
    pub gender: u8,
    pub level: u8,
    /// `unit.faction_id` (the race faction, e.g. 101 Nuian / 109 Harani); the quest runner
    /// checks `FactionMatch` / `MotherFaction` requirements against it.
    pub faction: u32,
}

/// A unit seen in `SCUnitStatePacket`.
#[derive(Clone, Debug, PartialEq)]
pub struct UnitInfo {
    pub obj_id: u32,
    pub name: String,
    pub unit_type: u8,
    /// Character id (type 0) or NPC template id (type 1).
    pub id: u32,
    /// Unit level (`SCUnitStatePacket` carries it for every unit type).
    pub level: u8,
    pub pos: (f32, f32, f32),
    /// Transport-layer id of a mate (type 5, `SCUnitStatePacket`); 0 for every other unit.
    pub tl_id: u16,
}

/// A doodad seen in `SCDoodadCreatedPacket` / `SCDoodadsCreatedPacket` (`Doodad.Write`).
#[derive(Clone, Debug, PartialEq)]
pub struct DoodadInfo {
    pub obj_id: u32,
    pub template_id: u32,
    pub pos: (f32, f32, f32),
    /// Current phase (`func_group_id`), updated by `SCDoodadPhaseChangedPacket`.
    pub func_group_id: u32,
    pub owner_obj_id: u32,
    pub parent_obj_id: u32,
    pub quest_glow: u32,
    pub data: i32,
}

/// Reads one `Doodad.Write` entry (see `models/doodad.rs` of the game crate).
fn read_doodad(s: &mut PacketStream) -> anyhow::Result<DoodadInfo> {
    let obj_id = s.read_bc()?;
    let template_id = s.read_u32()?;
    let owner_obj_id = s.read_bc()?;
    let parent_obj_id = s.read_bc()?;
    s.read_u8()?; // attach point
    let pos = s.read_position()?;
    s.read_i16()?; // roll
    s.read_i16()?; // pitch
    s.read_i16()?; // yaw
    s.read_f32()?; // scale
    s.read_bool()?; // has loot item
    let func_group_id = s.read_u32()?;
    s.read_u32()?; // owner id
    s.read_u64()?; // ucc id
    s.read_u32()?; // item template id
    s.read_u32()?; // type2
    s.read_u32()?; // time left
    s.read_i64()?; // plant time
    let quest_glow = s.read_u32()?;
    s.read_i32()?; // family
    s.read_i32()?; // puzzle group
    s.read_u8()?; // owner type
    s.read_u32()?; // owner db id
    let data = s.read_i32()?;
    Ok(DoodadInfo {
        obj_id,
        template_id,
        pos,
        func_group_id,
        owner_obj_id,
        parent_obj_id,
        quest_glow,
        data,
    })
}

/// One buff a unit carries, tracked from `SCBuffCreatedPacket` / `SCBuffRemovedPacket`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuffInfo {
    /// `buffs.id` of the applied buff.
    pub buff_id: u32,
    /// `Skill.Template.Id` when the buff is a skill's toggle buff, else 0.
    pub skill_id: u32,
    /// The owner's buff slot (`SCBuffRemovedPacket.index` removes by it).
    pub index: u32,
}

/// The last `SCMateSpawnedPacket`: the owner's own view of the summoned mate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MateSpawn {
    pub tl_id: u16,
    pub db_id: u32,
    pub item_id: u64,
    pub user_state: u8,
}

/// One item of an `SCItemTaskSuccessPacket` task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskItem {
    /// `ItemAction` of the task (5 create, 9 update detail, ...).
    pub task: u8,
    /// Template id (0 for tasks that do not carry it, e.g. `UpdateDetail`).
    pub template: u32,
    pub id: u64,
    /// Stack size of the task (0 for `UpdateDetail`).
    pub count: i32,
    /// `SummonMate` detail block: `(exp, dead flag, level)`; a non-zero dead flag marks a corpse.
    pub mate: Option<(i32, u8, u8)>,
}

/// One `SCExpChangedPacket` (`obj_id` gained `exp` experience).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExpChange {
    pub obj_id: u32,
    pub exp: i32,
    /// Monotonic order among the value-check observations of this state.
    pub seq: u64,
    /// Arrival time (ms since the first observation of this process).
    pub t_ms: u64,
}

/// One `SCLevelChangedPacket`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LevelChange {
    pub obj_id: u32,
    pub level: u8,
}

/// One NPC death (`SCUnitDeathPacket`) with the unit state known at that moment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KillInfo {
    pub obj_id: u32,
    /// NPC template id (`UnitInfo.id` of a type-1 unit in `SCUnitStatePacket`).
    pub template_id: u32,
    /// The level the server announced for the unit (0 when it was never announced).
    pub level: u8,
    /// Object id the death packet named as the killer (0 = none).
    pub killer_obj_id: u32,
    /// The player's own level when the unit died (the kill-XP oracle needs it).
    pub player_level: u8,
    /// Monotonic order among the value-check observations of this state.
    pub seq: u64,
    /// Arrival time (ms since the first observation of this process).
    pub t_ms: u64,
}

/// One `SCLootItemTookPacket`: an item taken from a loot owner (corpse / doodad).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LootTook {
    pub item_template_id: u32,
    pub owner_obj_id: u32,
    pub count: i32,
}

/// Milliseconds since the first value-check observation of this process. The kill-XP pairing
/// needs the `SCExpChangedPacket` the server sends immediately before the death packet.
pub(crate) fn observation_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
}

/// The readable part of an `SCItemTaskSuccessPacket`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ItemTaskInfo {
    /// `ItemTaskType` (59 mate death, 58 repair pets, 11 GM, ...).
    pub action: u8,
    pub items: Vec<TaskItem>,
    /// Sum of the `MoneyChange` tasks (copper; negative = deducted).
    pub money: i64,
}

/// Something the server told us that scenarios assert on, in arrival order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GameEvent {
    /// `SCErrorMsgPacket` (error code).
    ErrorMsg(i16),
    ItemTask(ItemTaskInfo),
}

/// `ItemAction` values of the tasks [`parse_item_task`] reads.
mod item_action {
    pub const CHANGE_MONEY_AMOUNT: u8 = 1;
    pub const CHANGE_BANK_MONEY_AMOUNT: u8 = 2;
    pub const CREATE: u8 = 5;
    pub const TAKE: u8 = 6;
    pub const UPDATE_DETAIL: u8 = 9;
    pub const CHANGE_OWNER: u8 = 15;
}

/// Best-effort decode of an `SCItemTaskSuccessPacket` body (`[u8 reason][u8 count]`, then the
/// tasks, see `models/item_task.rs` of the game crate). Reading stops at the first task kind it
/// does not know or on truncation; what was read so far is returned.
pub fn parse_item_task(body: &[u8]) -> ItemTaskInfo {
    fn details(s: &mut PacketStream) -> anyhow::Result<Option<(i32, u8, u8)>> {
        let len = s.read_i16()?;
        if len != 128 {
            bail!("task details length {len}");
        }
        let mut block = [0u8; 128];
        for b in &mut block {
            *b = s.read_u8()?;
        }
        // detail type 3 = SummonMate: [i32 exp][u8 dead][u8 level]
        Ok((block[0] == 3).then(|| {
            (
                i32::from_le_bytes([block[1], block[2], block[3], block[4]]),
                block[5],
                block[6],
            )
        }))
    }
    fn read(s: &mut PacketStream, info: &mut ItemTaskInfo) -> anyhow::Result<()> {
        info.action = s.read_u8()?;
        let count = s.read_u8()?;
        for _ in 0..count {
            let task = s.read_u8()?;
            match task {
                item_action::CHANGE_MONEY_AMOUNT | item_action::CHANGE_BANK_MONEY_AMOUNT => {
                    info.money += i64::from(s.read_i32()?);
                }
                item_action::CREATE | item_action::CHANGE_OWNER | item_action::TAKE => {
                    s.read_u8()?; // slot type
                    s.read_u8()?; // slot
                    let template = s.read_u32()?;
                    let id = s.read_u64()?;
                    s.read_u8()?; // grade
                    s.read_u8()?; // flags
                    let count = s.read_i32()?;
                    info.items.push(TaskItem {
                        task,
                        template,
                        id,
                        count,
                        mate: None,
                    });
                    let mate = details(s)?;
                    if let Some(item) = info.items.last_mut() {
                        item.mate = mate;
                    }
                    s.read_i64()?; // create time
                    s.read_i32()?; // lifespan
                    s.read_u32()?; // made unit id
                    s.read_u8()?; // world id
                    s.read_i64()?; // unsecure time
                    s.read_i64()?; // unpack time
                }
                item_action::UPDATE_DETAIL => {
                    s.read_u8()?; // slot type
                    s.read_u8()?; // slot
                    let id = s.read_u64()?;
                    let mate = details(s)?;
                    info.items.push(TaskItem {
                        task,
                        template: 0,
                        id,
                        count: 0,
                        mate,
                    });
                }
                _ => break,
            }
        }
        Ok(())
    }
    let mut s = PacketStream::from_bytes(body.to_vec());
    let mut info = ItemTaskInfo::default();
    let _ = read(&mut s, &mut info);
    info
}

/// How many recent inbound chat texts [`GameState`] keeps.
pub const CHAT_LOG_LIMIT: usize = 64;

/// `CSTeleportEndedPacket` orientation: the identity quaternion `(0, 0, 0, 1)`.
const TELEPORT_ENDED_ORIENTATION: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x80, 0x3f];

/// Move time reported by the landing `CSMoveUnitPacket` after a teleport.
const TELEPORT_LAND_MS: u32 = 1000;

/// What the bot learned from the server so far.
#[derive(Debug, Default)]
pub struct GameState {
    pub token: u32,
    pub stream_port: u16,
    pub last_change_state: Option<i32>,
    pub characters: Vec<CharInfo>,
    /// Characters the last `CSListCharacterPacket` burst reported (the sum of the per-packet
    /// counts; each packet carries up to two characters and only the first head is decoded).
    pub character_count: u32,
    pub char_list_done: bool,
    /// The selected character's `Character.Write` head, from the `SCCharacterStatePacket`
    /// select snapshot.
    pub self_info: Option<CharInfo>,
    pub selected_char: u32,
    pub self_obj: u32,
    pub self_pos: (f32, f32, f32),
    /// The world instance the last `SCLoadInstancePacket` reported (0 = the main world); every
    /// unit / doodad seen before it is stale (the observer clears them).
    pub world_instance: u32,
    /// The zone key of that world load (`SCLoadInstancePacket.zone_id`).
    pub world_zone: u32,
    pub units: HashMap<u32, UnitInfo>,
    /// Doodads in view (`SCDoodadCreatedPacket` / `SCDoodadsCreatedPacket` /
    /// `SCDoodadsRemovedPacket`), by object id.
    pub doodads: HashMap<u32, DoodadInfo>,
    /// Bag contents by slot (`SCCharacterInvenContentsPacket` + `SCItemTaskSuccessPacket`).
    pub bag: Bag,
    /// Item id -> template id of every item the task stream named (`SCItemTaskSuccessPacket`
    /// create rows): `ItemMove` tasks (action 8) carry no template, so bag / equipment moves
    /// are resolved with it.
    pub item_templates: HashMap<u64, u32>,
    /// Active quests by template id (`SCQuestsPacket`, whose select snapshot is sent in chunks
    /// of 20, and the `SCQuestContext*` updates).
    pub quests: BTreeMap<u32, QuestInfo>,
    /// Quest template ids completed during this session (also the `SCCompletedQuestsPacket`
    /// snapshot of the character select).
    pub completed_quests: BTreeSet<u32>,
    /// Quest template ids whose status went to `FAILED` (or an `SCQuestContextFailedPacket`).
    pub failed_quests: BTreeSet<u32>,
    /// Live completion / failure notifications (never the character-select snapshot), so the
    /// audit runner can tell a quest that finished during the session from one that was already
    /// finished before it.
    pub last_completed: Option<u32>,
    pub last_failed: Option<u32>,
    /// Template id of the last `SCQuestContextResetPacket`.
    pub last_reset: Option<u32>,
    /// The last [`CHAT_LOG_LIMIT`] inbound `SCChatMessagePacket` texts (say, system and GM
    /// command replies).
    pub chat_log: VecDeque<String>,
    /// Total chat messages captured (monotonic; lets callers window [`GameState::chat_log`]).
    pub chat_count: u64,
    /// Bag slots the character-select snapshot announced (`SCCharacterInvenInitPacket`).
    pub bag_slots: u8,
    /// Latest `SCUnitPointsPacket` per unit: `(hp, mp)` (the wire carries them times 100).
    pub unit_points: HashMap<u32, (i32, i32)>,
    /// Buffs per unit object id, by the owner's buff slot (`SCBuffCreatedPacket` /
    /// `SCBuffRemovedPacket`); the skill sweep reads what a cast applied.
    pub buffs: HashMap<u32, BTreeMap<u32, BuffInfo>>,
    /// Units announced dead by `SCUnitDeathPacket` (and not removed since).
    pub dead_units: HashSet<u32>,
    /// Experience gains (`SCExpChangedPacket`) in arrival order; the quest value checks pair
    /// them with kills and quest rewards.
    pub exp_changes: Vec<ExpChange>,
    /// Level-ups (`SCLevelChangedPacket`) in arrival order.
    pub level_changes: Vec<LevelChange>,
    /// NPC deaths (`SCUnitDeathPacket`) with the unit state known at death.
    pub kill_log: Vec<KillInfo>,
    /// Items taken from a loot owner (`SCLootItemTookPacket`).
    pub loot_log: Vec<LootTook>,
    /// Monotonic sequence shared by [`GameState::exp_changes`] and [`GameState::kill_log`], so
    /// a kill can be paired with the experience gain the server sent just before it.
    pub observation_seq: u64,
    /// Last `SCMateSpawnedPacket`.
    pub mate_spawned: Option<MateSpawn>,
    /// Number of `SCMateSpawnedPacket`s seen so far.
    pub mate_spawn_count: u32,
    /// `SCErrorMsgPacket`s and `SCItemTaskSuccessPacket`s in arrival order; scenarios remember
    /// `events.len()` before an action and inspect what was appended.
    pub events: Vec<GameEvent>,
    /// Position of the last `SCTeleportUnitPacket` (a GM `/move` only tells the client to go
    /// there; the server adopts the position from the client's next movement packet).
    pub last_teleport: Option<(f32, f32, f32)>,
    /// Absolute labor power from the select snapshot, kept current by
    /// `SCCharacterLaborPowerChangedPacket` (the live durability checks read it after a relog).
    pub labor: Option<i32>,
    /// Attackers of our own character: object id -> `observation_ms` of the last
    /// `SCUnitDamagedPacket` that hit us (player mode answers the ones still alive).
    pub attackers: HashMap<u32, u64>,
    /// The selected character's active abilities (`SCCharacterStatePacket`; None until read).
    pub abilities: Option<[u8; 3]>,
    /// Duel / team / faction / loot-dice / zone-state notifications in arrival order
    /// ([`crate::social`]), so the two-bot script commands can wait for an event a `drain`
    /// already read.
    pub social: Vec<crate::social::SocialEvent>,
    pub disconnected: bool,
}

impl GameState {
    /// Total count of `template_ids` in the bag.
    pub fn bag_count(&self, template_ids: &[u32]) -> i32 {
        self.bag
            .values()
            .filter(|i| template_ids.contains(&i.template_id))
            .map(|i| i.count)
            .sum()
    }

    /// The first (lowest-slot) bag item; the scenarios use it for the inventory move step.
    pub fn first_bag_item(&self) -> Option<ItemInfo> {
        self.bag.values().next().copied()
    }

    /// The nearest doodad of `template_id` (by our own position).
    pub fn find_doodad(&self, template_id: u32) -> Option<DoodadInfo> {
        let me = self.self_pos;
        self.doodads
            .values()
            .filter(|d| d.template_id == template_id)
            .min_by(|a, b| dist2(a.pos, me).total_cmp(&dist2(b.pos, me)))
            .cloned()
    }

    /// Updates the state from a packet.
    pub fn observe(&mut self, p: &Packet) -> anyhow::Result<()> {
        observe_into(self, p)
    }
}

pub struct GameClient {
    writer: FramedWrite<OwnedWriteHalf, FrameCodec>,
    rx: mpsc::UnboundedReceiver<Vec<u8>>,
    rec: Recorder,
    count: u8,
    pub account_id: u32,
    pub state: GameState,
    pub timeout: Duration,
    /// Keep-alive: the servers drop connections without a `PingPacket` for 30 s.
    pub ping_every: Duration,
    last_ping: Option<Instant>,
    started: Instant,
    /// Inner packets of the last `CompressedGamePackets` batch, decoded once and returned one
    /// by one (the sweep needs every packet of a batch, not only the batch itself).
    pending: VecDeque<Packet>,
    /// Player mode: walking instead of `/move`, real fights instead of `/kill`, GM shortcuts
    /// counted (see `crate::player`). `None` for every other runner.
    pub player: Option<Box<crate::player::PlayerMode>>,
}

impl GameClient {
    pub async fn connect(addr: &str, rec: Recorder) -> anyhow::Result<Self> {
        let sock = TcpStream::connect(addr)
            .await
            .with_context(|| format!("connecting to game server {addr}"))?;
        sock.set_nodelay(true)?;
        let (rd, wr) = sock.into_split();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut reader = FramedRead::new(rd, FrameCodec);
            loop {
                match reader.next().await {
                    Some(Ok(frame)) => {
                        if tx.send(frame).is_err() {
                            break;
                        }
                    }
                    Some(Err(e)) => {
                        tracing::warn!("game socket read error: {e}");
                        break;
                    }
                    None => {
                        tracing::debug!("game socket closed by the server");
                        break;
                    }
                }
            }
        });
        Ok(Self {
            writer: FramedWrite::new(wr, FrameCodec),
            rx,
            rec,
            count: 0,
            account_id: 0,
            state: GameState::default(),
            timeout: Duration::from_secs(20),
            ping_every: Duration::from_secs(3),
            last_ping: None,
            started: Instant::now(),
            pending: VecDeque::new(),
            player: None,
        })
    }

    pub fn recorder(&self) -> &Recorder {
        &self.rec
    }

    /// Sends a level-1 (`CS*`) packet.
    pub async fn send_cs(&mut self, opcode: u16, body: &[u8]) -> anyhow::Result<()> {
        self.rec.push(
            Channel::Game,
            Dir::C2S,
            LEVEL_GAME,
            opcode,
            packet_name(Dir::C2S, LEVEL_GAME, opcode),
            body,
        );
        let mut p = Vec::with_capacity(body.len() + 6);
        p.extend_from_slice(&[0xDD, LEVEL_GAME, 0, self.count]);
        self.count = self.count.wrapping_add(1);
        p.extend_from_slice(&opcode.to_le_bytes());
        p.extend_from_slice(body);
        self.writer.send(p).await?;
        Ok(())
    }

    /// Sends a level-2 (proxy) packet.
    pub async fn send_proxy(&mut self, opcode: u16, body: &[u8]) -> anyhow::Result<()> {
        self.rec.push(
            Channel::Game,
            Dir::C2S,
            LEVEL_PROXY,
            opcode,
            packet_name(Dir::C2S, LEVEL_PROXY, opcode),
            body,
        );
        let mut p = Vec::with_capacity(body.len() + 4);
        p.extend_from_slice(&[0xDD, LEVEL_PROXY]);
        p.extend_from_slice(&opcode.to_le_bytes());
        p.extend_from_slice(body);
        self.writer.send(p).await?;
        Ok(())
    }

    fn decode(&mut self, frame: Vec<u8>) -> anyhow::Result<Packet> {
        if frame.len() < 4 {
            bail!("short game frame ({} bytes)", frame.len());
        }
        let level = frame[1];
        if level == LEVEL_COMPRESSED {
            // `CompressedGamePackets`: [dd][04][u16 count][raw deflate of
            // ([u16 0][u16 typeId][body])*]. The inner packets carry no lengths; the split
            // decodes each body with its codec's field sizes. The capture keeps the inflated
            // batch (C# and Rust compress differently, the content is what must match); the
            // inner packets are handed out one by one like normal level-1 packets, so every
            // parser and `wait_for` sees them.
            let count = u16::from_le_bytes([frame[2], frame[3]]);
            let body = miniz_oxide::inflate::decompress_to_vec(&frame[4..])
                .map_err(|e| anyhow!("DD04 inflate failed: {e:?}"))?;
            self.rec.push(
                Channel::Game,
                Dir::S2C,
                level,
                count,
                "CompressedGamePackets",
                &body,
            );
            let packets = split_compressed_batch(&body);
            if packets.len() != usize::from(count) {
                tracing::debug!(
                    "CompressedGamePackets: decoded {} of {} inner packets",
                    packets.len(),
                    count
                );
            }
            for (opcode, inner) in packets {
                let p = Packet {
                    level: LEVEL_GAME,
                    opcode,
                    body: inner,
                };
                if let Err(e) = self.observe(&p) {
                    tracing::debug!("could not parse {}: {e}", p.name());
                }
                self.pending.push_back(p);
            }
            if let Some(p) = self.pending.pop_front() {
                return Ok(p);
            }
            return Ok(Packet {
                level,
                opcode: count,
                body,
            });
        }
        let (opcode, body) = if level == LEVEL_GAME {
            if frame.len() < 6 {
                bail!("short level-1 frame");
            }
            (
                u16::from_le_bytes([frame[4], frame[5]]),
                frame[6..].to_vec(),
            )
        } else {
            (
                u16::from_le_bytes([frame[2], frame[3]]),
                frame[4..].to_vec(),
            )
        };
        let p = Packet {
            level,
            opcode,
            body,
        };
        self.rec
            .push(Channel::Game, Dir::S2C, level, opcode, p.name(), &p.body);
        if let Err(e) = self.observe(&p) {
            tracing::debug!("could not parse {}: {e}", p.name());
        }
        Ok(p)
    }

    /// `PingPacket` (proxy): `tPhy`, `ping` and `local`, like the client's keep-alive.
    pub async fn ping(&mut self) -> anyhow::Result<()> {
        let ms = self.started.elapsed().as_millis() as i64;
        let mut s = PacketStream::new();
        s.write_i64(ms).write_i64(ms).write_u32(ms as u32);
        self.last_ping = Some(Instant::now());
        self.send_proxy(pp::PingPacket, s.as_bytes()).await
    }

    async fn keep_alive(&mut self) -> anyhow::Result<()> {
        if self.account_id != 0
            && self
                .last_ping
                .is_none_or(|t| t.elapsed() >= self.ping_every)
        {
            self.ping().await?;
        }
        Ok(())
    }

    /// Next packet, or `None` after `wait` without one. Sends keep-alive pings as needed.
    pub async fn recv_within(&mut self, wait: Duration) -> anyhow::Result<Option<Packet>> {
        // Inner packets of a received batch come first, in batch order.
        if let Some(p) = self.pending.pop_front() {
            return Ok(Some(p));
        }
        self.keep_alive().await?;
        let wait = wait.min(self.ping_every);
        match tokio::time::timeout(wait, self.rx.recv()).await {
            Err(_) => Ok(None),
            Ok(None) => {
                self.state.disconnected = true;
                Err(anyhow!("game server closed the connection"))
            }
            Ok(Some(frame)) => Ok(Some(self.decode(frame)?)),
        }
    }

    /// Reads packets until one matches `pred` (returned) or the timeout elapses.
    pub async fn wait_for<F>(&mut self, what: &str, mut pred: F) -> anyhow::Result<Packet>
    where
        F: FnMut(&Packet) -> bool,
    {
        let deadline = Instant::now() + self.timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!("timed out waiting for {what}");
            }
            match self.recv_within(left).await? {
                Some(p) if pred(&p) => return Ok(p),
                Some(_) | None => {}
            }
        }
    }

    /// Waits for a level-1 packet with `opcode`.
    pub async fn wait_sc(&mut self, opcode: u16) -> anyhow::Result<Packet> {
        let name = packet_name(Dir::S2C, LEVEL_GAME, opcode);
        self.wait_for(name, |p| p.is_sc(opcode)).await
    }

    /// Reads everything that arrives until the connection is quiet for `quiet` (or `max`).
    pub async fn drain(&mut self, quiet: Duration, max: Duration) -> anyhow::Result<Vec<Packet>> {
        let deadline = Instant::now() + max;
        let mut out = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(out);
            }
            match self.recv_within(quiet.min(left)).await? {
                Some(p) => out.push(p),
                None => return Ok(out),
            }
        }
    }

    /// Updates [`GameState`] from a packet.
    fn observe(&mut self, p: &Packet) -> anyhow::Result<()> {
        observe_into(&mut self.state, p)
    }
}

/// Applies one received packet to `state` (a free function so tests can drive the parsing
/// without a connection).
pub(crate) fn observe_into(st: &mut GameState, p: &Packet) -> anyhow::Result<()> {
    if let Some(ev) = crate::social::decode(p) {
        st.social.push(ev);
    }
    let mut s = p.stream();
    if p.is(LEVEL_PROXY, pp::ChangeStatePacket) {
        st.last_change_state = Some(s.read_i32()?);
    } else if p.is_sc(sc::X2EnterWorldResponsePacket) {
        let _reason = s.read_i16()?;
        let _gm = s.read_bool()?;
        st.token = s.read_u32()?;
        st.stream_port = s.read_u16()?;
    } else if p.is_sc(sc::SCCharacterListPacket) {
        let last = s.read_bool()?;
        let n = s.read_u8()?;
        // A packet carries up to two rows (`SCCharacterListPacket::chunked`); only the first
        // head can be decoded (the equipment / model data after it is variable), so the total
        // comes from the per-packet count.
        st.character_count = st.character_count.saturating_add(u32::from(n));
        if n > 0 {
            let info = read_character_head(&mut s)?;
            if !st.characters.iter().any(|c| c.id == info.id) {
                st.characters.push(info);
            }
        }
        st.char_list_done |= last;
    } else if p.is_sc(sc::SCCreateCharacterResponsePacket) {
        let info = read_character_head(&mut s)?;
        if !st.characters.iter().any(|c| c.id == info.id) {
            st.characters.push(info);
        }
    } else if p.is_sc(sc::SCUnitStatePacket) {
        let obj_id = s.read_bc()?;
        let name = s.read_string()?;
        let unit_type = s.read_u8()?;
        let mut tl_id = 0;
        let id = match unit_type {
            0 => {
                let id = s.read_u32()?;
                s.read_u64()?;
                id
            }
            1 => {
                s.read_bc()?;
                let tpl = s.read_u32()?;
                s.read_u32()?;
                s.read_u8()?;
                tpl
            }
            5 => {
                // Mate: tl id, template id, owner character id.
                tl_id = s.read_u16()?;
                let tpl = s.read_u32()?;
                s.read_u32()?;
                tpl
            }
            _ => return Ok(()),
        };
        let _owner = s.read_string()?;
        let pos = s.read_position()?;
        s.read_f32()?; // scale
        let level = s.read_u8()?;
        if unit_type == 0 && id == st.selected_char && id != 0 {
            st.self_obj = obj_id;
            st.self_pos = pos;
        }
        st.units.insert(
            obj_id,
            UnitInfo {
                obj_id,
                name,
                unit_type,
                id,
                level,
                pos,
                tl_id,
            },
        );
    } else if p.is_sc(sc::SCOneUnitMovementPacket) {
        // objId, MoveTypeEnum (1 = Unit), time, flags [+ scType, phase], position.
        let obj_id = s.read_bc()?;
        if s.read_u8()? == 1 {
            s.read_u32()?;
            let flags = s.read_u8()?;
            if flags & 0x10 != 0 {
                s.read_u32()?;
                s.read_u8()?;
            }
            let pos = s.read_position()?;
            if let Some(u) = st.units.get_mut(&obj_id) {
                u.pos = pos;
            }
        }
    } else if p.is_sc(sc::SCUnitsRemovedPacket) {
        let n = s.read_u16()?;
        for _ in 0..n {
            let id = s.read_bc()?;
            st.units.remove(&id);
            st.unit_points.remove(&id);
            st.dead_units.remove(&id);
            st.buffs.remove(&id);
        }
    } else if p.is_sc(sc::SCUnitPointsPacket) {
        let obj_id = s.read_bc()?;
        let hp = s.read_i32()? / 100;
        let mp = s.read_i32()? / 100;
        st.unit_points.insert(obj_id, (hp, mp));
        // A revived unit (the own character after the resurrection) is alive again.
        if hp > 0 {
            st.dead_units.remove(&obj_id);
        }
    } else if p.is_sc(sc::SCUnitDeathPacket) {
        let obj_id = s.read_bc()?;
        st.dead_units.insert(obj_id);
        // Best-effort kill metadata: the packet may be truncated like any other server packet.
        let killer = (|| -> anyhow::Result<u32> {
            s.read_u8()?; // kill reason
            s.read_i32()?; // resurrect wait
            s.read_i32()?; // lost exp
            s.read_u8()?; // durability loss rate
            Ok(s.read_bc()?) // killer object id (0 = none)
        })();
        if let Ok(killer_obj_id) = killer {
            if let Some(unit) = st.units.get(&obj_id).filter(|u| u.unit_type == 1) {
                st.observation_seq += 1;
                let player_level = st.self_info.as_ref().map_or(0, |info| info.level);
                st.kill_log.push(KillInfo {
                    obj_id,
                    template_id: unit.id,
                    level: unit.level,
                    killer_obj_id,
                    player_level,
                    seq: st.observation_seq,
                    t_ms: observation_ms(),
                });
            }
        }
    } else if p.is_sc(sc::SCExpChangedPacket) {
        let obj_id = s.read_bc()?;
        let exp = s.read_i32()?;
        s.read_bool()?; // shouldAddAbilityExp
        st.observation_seq += 1;
        let seq = st.observation_seq;
        st.exp_changes.push(ExpChange {
            obj_id,
            exp,
            seq,
            t_ms: observation_ms(),
        });
    } else if p.is_sc(sc::SCLevelChangedPacket) {
        let obj_id = s.read_bc()?;
        let level = s.read_u8()?;
        st.level_changes.push(LevelChange { obj_id, level });
        if obj_id == st.self_obj {
            if let Some(info) = st.self_info.as_mut() {
                info.level = level;
            }
        }
    } else if p.is_sc(sc::SCUnitDamagedPacket) {
        if let Some(hit) = crate::skillrun::parse_unit_damaged(p) {
            if hit.target_id == st.self_obj && hit.caster_obj != st.self_obj && hit.damage > 0 {
                st.attackers.insert(hit.caster_obj, observation_ms());
            }
        }
    } else if p.is_sc(sc::SCLootItemTookPacket) {
        // `SCLootItemTookPacket`: [u32 template][u16 index][u16 owner type][bc owner][u8 0]
        // [i32 count] (`systems/economy/loot_packets.rs`).
        let item_template_id = s.read_u32()?;
        s.read_u16()?; // item index
        s.read_u16()?; // loot owner type
        let owner_obj_id = s.read_bc()?;
        s.read_u8()?;
        let count = s.read_i32()?;
        st.loot_log.push(LootTook {
            item_template_id,
            owner_obj_id,
            count,
        });
    } else if p.is_sc(sc::SCBuffCreatedPacket) {
        // `SCBuffCreatedPacket::write`: caster (type byte + body), caster id, owner obj id,
        // index, buff id, caster level, ab level, skill id, then `Buff.WriteData`.
        let caster_kind = s.read_u8()?;
        SkillCaster::read(caster_kind, &mut s)?;
        s.read_u32()?; // caster_id (character id; 0 for a pure unit caster)
        let owner_obj_id = s.read_bc()?;
        let index = s.read_u32()?;
        let buff_id = s.read_u32()?;
        s.read_u8()?; // caster_level
        s.read_i16()?; // ab_level
        let skill_id = s.read_u32()?;
        st.buffs.entry(owner_obj_id).or_default().insert(
            index,
            BuffInfo {
                buff_id,
                skill_id,
                index,
            },
        );
    } else if p.is_sc(sc::SCBuffRemovedPacket) {
        let obj_id = s.read_bc()?;
        let index = s.read_u32()?;
        if let Some(buffs) = st.buffs.get_mut(&obj_id) {
            buffs.remove(&index);
        }
    } else if p.is_sc(sc::SCErrorMsgPacket) {
        st.events.push(GameEvent::ErrorMsg(s.read_i16()?));
    } else if p.is_sc(sc::SCMateSpawnedPacket) {
        st.mate_spawned = Some(MateSpawn {
            tl_id: s.read_u16()?,
            db_id: s.read_u32()?,
            item_id: s.read_u64()?,
            user_state: s.read_u8()?,
        });
        st.mate_spawn_count += 1;
    } else if p.is_sc(sc::SCDoodadPhaseChangedPacket) {
        let id = s.read_bc()?;
        let group = s.read_u32()?;
        if let Some(d) = st.doodads.get_mut(&id) {
            d.func_group_id = group;
        }
    } else if p.is_sc(sc::SCCharacterInvenContentsPacket) {
        let slot_type = s.read_u8()?;
        let _num_chunks = s.read_u8()?;
        let chunk = s.read_u8()?;
        if slot_type == inventory::SLOT_TYPE_INVENTORY {
            // Ten `Item.Write` rows per packet; parse until the body ends (empty slots are
            // `i32 0`), so every row in the chunk is decoded.
            let mut i = 0u8;
            while s.remaining() > 0 && i < 10 {
                if let Some(mut item) = inventory::read_item(&mut s)? {
                    item.slot = chunk * 10 + i;
                    st.bag.insert(item.slot, item);
                }
                i += 1;
            }
        }
    } else if p.is_sc(sc::SCItemTaskSuccessPacket) {
        st.events
            .push(GameEvent::ItemTask(parse_item_task(&p.body)));
        inventory::apply_item_task_success(&mut st.bag, &mut st.item_templates, &p.body)?;
    } else if p.is_sc(sc::SCInvenExpandedPacket) {
        // `SCInvenExpandedPacket { slot_type: u8, num_slots: u8 }` (`item_packets.rs`).
        let slot_type = s.read_u8()?;
        let num_slots = s.read_u8()?;
        if slot_type == inventory::SLOT_TYPE_INVENTORY {
            st.bag_slots = st.bag_slots.max(num_slots);
        }
    } else if p.is_sc(sc::SCDoodadCreatedPacket) {
        let doodad = read_doodad(&mut s)?;
        st.doodads.insert(doodad.obj_id, doodad);
    } else if p.is_sc(sc::SCDoodadsCreatedPacket) {
        let n = s.read_u8()?;
        for _ in 0..n {
            let doodad = read_doodad(&mut s)?;
            st.doodads.insert(doodad.obj_id, doodad);
        }
    } else if p.is_sc(sc::SCDoodadRemovedPacket) {
        let obj_id = s.read_bc()?;
        s.read_bool()?; // e: true = despawned, false = deleted
        st.doodads.remove(&obj_id);
    } else if p.is_sc(sc::SCDoodadsRemovedPacket) {
        // `SCDoodadsRemovedPacket`: u16 count, `last`, then `(bc id, bool e)` per id.
        let n = s.read_u16()?;
        let _last = s.read_bool()?;
        for _ in 0..n {
            let obj_id = s.read_bc()?;
            s.read_bool()?;
            st.doodads.remove(&obj_id);
        }
    } else if p.is_sc(sc::SCQuestsPacket) {
        // The select snapshot arrives in chunks of 20; upsert row by row.
        for q in quest::quest_rows(&p.body)? {
            upsert_quest(st, q, false);
        }
    } else if p.is_sc(sc::SCCompletedQuestsPacket) {
        for id in quest::completed_quest_rows(&p.body)? {
            st.completed_quests.insert(id);
        }
    } else if p.is_sc(sc::SCQuestContextStartedPacket) || p.is_sc(sc::SCQuestContextUpdatedPacket) {
        upsert_quest(st, quest::read_quest_row(&mut s)?, true);
    } else if p.is_sc(sc::SCQuestContextCompletedPacket) {
        let quest_id = s.read_u32()?;
        s.read_u64()?; // body8
        s.read_u32()?; // component id
        st.completed_quests.insert(quest_id);
        st.last_completed = Some(quest_id);
        st.quests.remove(&quest_id);
    } else if p.is_sc(sc::SCQuestContextFailedPacket) {
        // No Rust definition: the 1.2 table declares the opcode but neither server sends
        // it. The layout follows the Completed / Reset siblings (`quests/packets.rs`).
        let quest_id = s.read_u32()?;
        s.read_u64()?; // body8
        s.read_u32()?; // component id
        st.failed_quests.insert(quest_id);
        st.last_failed = Some(quest_id);
        st.quests.remove(&quest_id);
    } else if p.is_sc(sc::SCQuestContextResetPacket) {
        let quest_id = s.read_u32()?;
        s.read_u64()?; // body8
        s.read_u32()?; // component id
        st.last_reset = Some(quest_id);
        st.quests.remove(&quest_id);
    } else if p.is_sc(sc::SCCharacterLaborPowerChangedPacket) {
        // `SCCharacterLaborPowerChangedPacket`: the signed change, then action / point / step.
        let amount = s.read_i32()?;
        if let Some(labor) = st.labor {
            st.labor = Some(i32::from((labor as i16).wrapping_add(amount as i16)));
        }
    } else if p.is_sc(sc::SCCharacterStatePacket) {
        // `unit_state::character_state`: instance id, guid (sized bytes), rwd, then
        // `Character.Write` (id, name, race, gender, level, ...). The select snapshot is
        // authoritative for the world instance (character select forces the main world,
        // `world/session.rs::select_character`).
        st.world_instance = s.read_u32()?;
        let guid_len = s.read_u16()?;
        s.read_bytes(usize::from(guid_len))?;
        s.read_i32()?; // rwd
        st.self_info = Some(read_character_head(&mut s)?);
        st.labor = crate::craftrun::parse_state_labor(&p.body).ok();
        st.abilities = crate::craftrun::parse_state_equipment(&p.body)
            .ok()
            .map(|(_, _, abilities)| abilities);
        // A character-select snapshot starts here; the quest / completed packets follow.
        st.quests.clear();
        st.completed_quests.clear();
        st.failed_quests.clear();
        st.last_completed = None;
        st.last_failed = None;
        st.last_reset = None;
    } else if p.is_sc(sc::SCChatMessagePacket) {
        if let Some(text) = chat_text(p) {
            if st.chat_log.len() == CHAT_LOG_LIMIT {
                st.chat_log.pop_front();
            }
            st.chat_log.push_back(text);
            st.chat_count = st.chat_count.saturating_add(1);
        }
    } else if p.is_sc(sc::SCTeleportUnitPacket) {
        // Every teleport (GM `/move` included) confirms the own position with this packet;
        // the client answers with `CSTeleportEndedPacket`.
        s.read_u8()?; // reason
        s.read_i16()?; // error message
        let pos = s.read_position()?;
        s.read_f32()?; // z2 (yaw)
        st.last_teleport = Some(pos);
        st.self_pos = pos;
        if st.self_obj != 0 {
            if let Some(u) = st.units.get_mut(&st.self_obj) {
                u.pos = pos;
            }
        }
    } else if p.is_sc(sc::SCLoadInstancePacket) {
        // The server moved the character to another world instance (portal, an enter-instance
        // doodad, a dungeon / system instance or a return to the main world;
        // `systems/instances/dungeon.rs::move_character`, `worlds.rs::return_to_main_world`).
        // Everything the bot knew about the old world is stale; the position comes from the
        // packet and the real client answers with `CSInstanceLoadedPacket`.
        let instance_id = s.read_u32()?;
        let zone_id = s.read_u32()?;
        let pos = (s.read_f32()?, s.read_f32()?, s.read_f32()?);
        s.read_f32()?; // ang_x
        s.read_f32()?; // ang_y
        s.read_f32()?; // ang_z
        st.world_instance = instance_id;
        st.world_zone = zone_id;
        st.self_pos = pos;
        st.units.clear();
        st.doodads.clear();
        st.buffs.clear();
    }
    Ok(())
}

/// Records `q` as an active quest; a `FAILED` status is also remembered in
/// [`GameState::failed_quests`]. `live` is set for the `SCQuestContext*` update packets (never
/// for the character-select snapshot), so the audit runner can see session-only failures.
fn upsert_quest(st: &mut GameState, q: QuestInfo, live: bool) {
    if q.status == quest_status::FAILED {
        st.failed_quests.insert(q.template_id);
        if live {
            st.last_failed = Some(q.template_id);
        }
    }
    st.quests.insert(q.template_id, q);
}

/// Message text of an `SCChatMessagePacket` (the second string, after the sender), `None` for
/// any other packet or a malformed body.
pub fn chat_text(p: &Packet) -> Option<String> {
    if !p.is_sc(sc::SCChatMessagePacket) {
        return None;
    }
    let mut s = p.stream();
    s.read_i16().ok()?; // chat type
    s.read_i16().ok()?; // packed sub type
    s.read_u32().ok()?; // packed faction
    s.read_bc().ok()?; // sender object id
    s.read_u32().ok()?; // sender character id
    s.read_u8().ok()?; // language type
    s.read_u8().ok()?; // race
    s.read_u32().ok()?; // faction
    s.read_string().ok()?; // sender name
    s.read_string().ok()
}

/// The fixed head of `Character.Write` (`models/character.rs`): id, name, race, gender, level,
/// hp, mp, zone, faction. The equipment / model data after it is variable and irrelevant to the
/// runner.
fn read_character_head(s: &mut PacketStream) -> anyhow::Result<CharInfo> {
    let id = s.read_u32()?;
    let name = s.read_string()?;
    let race = s.read_u8()?;
    let gender = s.read_u8()?;
    let level = s.read_u8()?;
    s.read_i32()?; // hp
    s.read_i32()?; // mp
    s.read_u32()?; // zone id
    let faction = s.read_u32()?;
    Ok(CharInfo {
        id,
        name,
        race,
        gender,
        level,
        faction,
    })
}

impl GameClient {
    // ----- C2G packets -----

    /// `X2EnterWorldPacket`.
    pub async fn enter_world(&mut self, account_id: u32, cookie: u32) -> anyhow::Result<()> {
        self.account_id = account_id;
        let mut s = PacketStream::new();
        s.write_u32(0) // pFrom
            .write_u32(0) // pTo
            .write_u32(account_id)
            .write_u32(cookie)
            .write_i32(0) // zoneId
            .write_u8(0) // tb
            .write_u64(0); // revision
        self.send_cs(cs::X2EnterWorldPacket, s.as_bytes()).await
    }

    /// `FinishStatePacket` (proxy).
    pub async fn finish_state(&mut self, state: i32) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_i32(state);
        self.send_proxy(pp::FinishStatePacket, s.as_bytes()).await
    }

    /// `CSListCharacterPacket`. Resets the previous list result, so
    /// [`GameState::character_count`] and [`GameState::characters`] describe this request.
    pub async fn list_characters(&mut self) -> anyhow::Result<()> {
        self.state.characters.clear();
        self.state.character_count = 0;
        self.state.char_list_done = false;
        let mut s = PacketStream::new();
        s.write_i32(0).write_bytes_sized(&[]);
        self.send_cs(cs::CSListCharacterPacket, s.as_bytes()).await
    }

    /// `CSCreateCharacterPacket`: Nuian (1) male (1) Fighter (ability 1), default appearance.
    pub async fn create_character(&mut self, name: &str) -> anyhow::Result<()> {
        self.create_character_as(name, 1, 1).await
    }

    /// `CSCreateCharacterPacket` with the chosen race / gender (Fighter ability 1, default
    /// appearance). The server needs a `characters` template for the pair
    /// (`data.characters.template`, keyed `16 * gender + race`).
    pub async fn create_character_as(
        &mut self,
        name: &str,
        race: u8,
        gender: u8,
    ) -> anyhow::Result<()> {
        let body = create_character_body(name, race, gender);
        self.send_cs(cs::CSCreateCharacterPacket, &body).await
    }

    /// `CSSelectCharacterPacket`.
    pub async fn select_character(&mut self, id: u32) -> anyhow::Result<()> {
        self.state.selected_char = id;
        let mut s = PacketStream::new();
        s.write_u32(id).write_bool(false).write_u8(0);
        self.send_cs(cs::CSSelectCharacterPacket, s.as_bytes())
            .await
    }

    /// `CSSpawnCharacterPacket` (visual options: none).
    /// `CSSpawnCharacterPacket` with the visual options the client sends (all flags set).
    pub async fn spawn_character(&mut self) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_u8(0x1F) // stp, helmet, back holdable, cosplay, cosplay backpack
            .write_raw(&[0u8; 6])
            .write_bool(true)
            .write_bool(true)
            .write_bool(true)
            .write_bool(true);
        self.send_cs(cs::CSSpawnCharacterPacket, s.as_bytes()).await
    }

    pub async fn notify_in_game(&mut self) -> anyhow::Result<()> {
        self.send_cs(cs::CSNotifyInGamePacket, &[]).await
    }

    pub async fn notify_in_game_completed(&mut self) -> anyhow::Result<()> {
        self.send_cs(cs::CSNotifyInGameCompletedPacket, &[]).await
    }

    /// `CSMoveUnitPacket` with a `UnitMoveType` for our own character.
    pub async fn move_to(&mut self, pos: (f32, f32, f32), time: u32) -> anyhow::Result<()> {
        self.move_to_with_direction(pos, time, 0).await
    }

    /// Move into range while facing a unit, so angle restrictions and auto attacks apply.
    pub async fn move_to_facing(
        &mut self,
        pos: (f32, f32, f32),
        target: (f32, f32, f32),
        time: u32,
    ) -> anyhow::Result<()> {
        self.move_to_with_direction(pos, time, facing_direction(pos, target))
            .await
    }

    /// `CSMoveUnitPacket` for another object of ours (the mate we ride), without touching
    /// our own tracked position.
    pub async fn move_obj_to(
        &mut self,
        obj_id: u32,
        pos: (f32, f32, f32),
        time: u32,
    ) -> anyhow::Result<()> {
        let own = self.state.self_pos;
        self.move_obj_inner(obj_id, pos, time, 0).await?;
        self.state.self_pos = own;
        if let Some(u) = self.state.units.get_mut(&obj_id) {
            u.pos = pos;
        }
        Ok(())
    }

    async fn move_to_with_direction(
        &mut self,
        pos: (f32, f32, f32),
        time: u32,
        direction: i8,
    ) -> anyhow::Result<()> {
        let obj = self.state.self_obj;
        self.move_obj_inner(obj, pos, time, direction).await
    }

    async fn move_obj_inner(
        &mut self,
        obj_id: u32,
        pos: (f32, f32, f32),
        time: u32,
        direction: i8,
    ) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_bc(obj_id)
            .write_u8(1) // MoveTypeEnum.Unit
            .write_u32(time)
            .write_u8(0x02) // Moving
            .write_position(pos.0, pos.1, pos.2)
            .write_i16(0)
            .write_i16(0)
            .write_i16(0) // velocity
            .write_i8(0)
            .write_i8(0)
            .write_i8(direction) // yaw
            .write_i8(0)
            .write_i8(0)
            .write_i8(0) // delta movement
            .write_i8(1) // stance: idle
            .write_u8(0) // alertness
            .write_u8(0); // actor flags
        self.state.self_pos = pos;
        self.send_cs(cs::CSMoveUnitPacket, s.as_bytes()).await
    }

    /// Teleports over the `/move x y z` GM command and waits for the server's
    /// `SCTeleportUnitPacket` confirmation (which updates [`GameState::self_pos`]). Completes the
    /// client handshake (`CSTeleportEndedPacket` clears the server's disabled-set-position flag
    /// and re-sends the visible objects; the landing move then applies the new position), so
    /// the region-change `SCUnitsRemovedPacket`s that follow prune the stale units.
    ///
    /// In player mode (`self.player`) this walks to the position instead (see
    /// `crate::player::walk_to`); the GM teleport stays available as a counted fallback
    /// ([`GameClient::teleport_gm`]).
    pub async fn teleport(&mut self, pos: (f32, f32, f32)) -> anyhow::Result<()> {
        if self.player.is_some() {
            return crate::player::walk_to(self, pos).await;
        }
        self.teleport_gm(pos).await
    }

    /// The `/move` GM teleport itself.
    pub async fn teleport_gm(&mut self, pos: (f32, f32, f32)) -> anyhow::Result<()> {
        let command = format!("{COMMAND_PREFIX}move {} {} {}", pos.0, pos.1, pos.2);
        self.chat(chat_type::WHITE, &command).await?;
        self.wait_sc(sc::SCTeleportUnitPacket).await?;
        let (x, y, z) = self.state.self_pos;
        ensure!(
            (x - pos.0).abs() < 0.5 && (y - pos.1).abs() < 0.5 && (z - pos.2).abs() < 0.5,
            "server confirmed a teleport to ({x}, {y}, {z}), not {pos:?}"
        );
        self.teleport_ended(pos).await?;
        self.move_to(pos, TELEPORT_LAND_MS).await
    }

    /// `CSSendChatMessagePacket`.
    pub async fn chat(&mut self, chat_type: i16, message: &str) -> anyhow::Result<()> {
        let body = chat_message_body(chat_type, "", message);
        self.send_cs(cs::CSSendChatMessagePacket, &body).await
    }

    /// Runs a GM chat command (`/name args`, see `aaemu_game::systems::services::commands::
    /// COMMAND_PREFIX`; the server strips the slash from any chat type) and returns the
    /// `SCChatMessagePacket` texts received within `wait` (the command replies are system
    /// chat).
    pub async fn gm(&mut self, command: &str, wait: Duration) -> anyhow::Result<Vec<String>> {
        // Player mode: forbidden commands are refused, every other one is a counted fallback.
        if let Some(player) = self.player.as_mut() {
            player.gm_command(command)?;
        }
        let text = if command.starts_with(COMMAND_PREFIX) {
            command.to_string()
        } else {
            format!("{COMMAND_PREFIX}{command}")
        };
        self.chat(chat_type::WHITE, &text).await?;
        let deadline = Instant::now() + wait;
        let mut messages = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(messages);
            }
            let slice = left.min(Duration::from_millis(250));
            if let Some(p) = self.recv_within(slice).await? {
                if let Some(text) = chat_text(&p) {
                    messages.push(text);
                }
            }
        }
    }

    /// `CSTeleportEndedPacket`: the client's answer to `SCTeleportUnitPacket` (the server
    /// then accepts our movement packets again and re-announces the objects around).
    pub async fn teleport_ended(&mut self, pos: (f32, f32, f32)) -> anyhow::Result<()> {
        let body = teleport_ended_body(pos);
        self.send_cs(cs::CSTeleportEndedPacket, &body).await
    }

    /// `CSLearnSkillPacket` (0x092): learn a skill of an active ability (the server answers
    /// `SCSkillLearnedPacket`, or stays silent when the ability is inactive / points are short).
    pub async fn learn_skill(&mut self, skill_id: u32) -> anyhow::Result<()> {
        self.send_cs(cs::CSLearnSkillPacket, &learn_skill_body(skill_id))
            .await
    }

    /// `CSSwapAbilityPacket` (0x096): replace the active ability `old_ability` with `ability`
    /// (the server resets the old ability's learned skills).
    pub async fn swap_ability(&mut self, old_ability: u8, ability: u8) -> anyhow::Result<()> {
        let body = swap_ability_body(self.state.self_obj, old_ability, ability);
        self.send_cs(cs::CSSwapAbilityPacket, &body).await
    }

    /// `CSResetSkillsPacket` (0x094): reset the learned skills of `ability_id`.
    pub async fn reset_skills(&mut self, ability_id: u8) -> anyhow::Result<()> {
        self.send_cs(cs::CSResetSkillsPacket, &reset_skills_body(ability_id))
            .await
    }

    /// `CSSwapItemsPacket` between any slot types (`MoveArgs` block).
    pub async fn move_item(
        &mut self,
        from_item: u64,
        from_slot_type: u8,
        from_slot: u8,
        to_item: u64,
        to_slot_type: u8,
        to_slot: u8,
    ) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_u64(from_item)
            .write_u64(to_item)
            .write_u8(from_slot_type)
            .write_u8(from_slot)
            .write_u8(to_slot_type)
            .write_u8(to_slot);
        self.send_cs(cs::CSSwapItemsPacket, s.as_bytes()).await
    }

    /// Equips a bag item into an `EquipmentItemSlot` index (`1` = equipment container).
    pub async fn equip_item(
        &mut self,
        item_id: u64,
        bag_slot: u8,
        equip_slot: u8,
    ) -> anyhow::Result<()> {
        self.move_item(item_id, 2, bag_slot, 0, 1, equip_slot).await
    }

    /// `CSStartSkillPacket`: unit caster (us) on a unit target.
    pub async fn start_skill(&mut self, skill_id: u32, target_obj: u32) -> anyhow::Result<()> {
        self.start_skill_as(
            skill_id,
            SkillCaster::Unit(self.state.self_obj),
            SkillCastTarget::Unit(target_obj),
        )
        .await
    }

    /// `CSStartSkillPacket` with an explicit caster / target (`start_skill_body`).
    pub async fn start_skill_as(
        &mut self,
        skill_id: u32,
        caster: SkillCaster,
        target: SkillCastTarget,
    ) -> anyhow::Result<()> {
        let body = start_skill_body(skill_id, &caster, &target);
        self.send_cs(cs::CSStartSkillPacket, &body).await
    }

    /// `CSStartSkillPacket` from our unit on ourselves carrying a skill object (the type byte and
    /// body, e.g. the portal book's `PortalInfo` / `SavePortalInfo`).
    pub async fn start_skill_with_object(
        &mut self,
        skill_id: u32,
        object: &[u8],
    ) -> anyhow::Result<()> {
        let me = self.state.self_obj;
        let mut s = PacketStream::new();
        s.write_u32(skill_id);
        SkillCaster::Unit(me).write(&mut s);
        SkillCastTarget::Unit(me).write(&mut s);
        s.write_raw(object);
        self.send_cs(cs::CSStartSkillPacket, s.as_bytes()).await
    }

    /// Uses a doodad with an interaction skill: `CSStartSkillPacket` with our unit as the caster
    /// and the doodad as the target (the server's `InteractionEffect` -> `doodad.Use`).
    pub async fn use_doodad(&mut self, skill_id: u32, doodad_obj: u32) -> anyhow::Result<()> {
        self.start_skill_as(
            skill_id,
            SkillCaster::Unit(self.state.self_obj),
            SkillCastTarget::Doodad(doodad_obj),
        )
        .await
    }

    /// The real client's answer to `SCLoadInstancePacket`: an empty `CSInstanceLoadedPacket`.
    /// The server then sends the own `SCUnitStatePacket` (and the time of day) and clears its
    /// `disabled_set_position` (`systems/instances/worlds.rs::instance_loaded`).
    pub async fn confirm_instance_loaded(&mut self) -> anyhow::Result<()> {
        self.send_cs(cs::CSInstanceLoadedPacket, &[]).await
    }

    /// Uses a bag item with its `use_skill_id`: `CSStartSkillPacket` with a `SkillItem` caster
    /// (the server checks `item_use_skill` and consumes / applies the item).
    pub async fn use_item(
        &mut self,
        item_id: u64,
        item_template_id: u32,
        skill_id: u32,
        target: SkillCastTarget,
    ) -> anyhow::Result<()> {
        self.start_skill_as(
            skill_id,
            SkillCaster::Item {
                obj_id: item_id as u32,
                item_id,
                item_template_id,
                type1: 0,
                type2: 0,
            },
            target,
        )
        .await
    }

    /// `CSBuyItemsPacket`: buy `count` of `item_id` from merchant `npc_obj` (or purchase doodad
    /// `doodad_obj` when `npc_obj` is 0), money currency. The server checks the pack, the 3 m
    /// distance and the price (`systems/economy/store.rs`).
    pub async fn buy_item(
        &mut self,
        npc_obj: u32,
        doodad_obj: u32,
        item_id: u32,
        count: i32,
    ) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_bc(npc_obj)
            .write_bc(doodad_obj)
            .write_u32(0) // unk id
            .write_u8(1) // buy count
            .write_u8(0) // buy-back count
            .write_u32(item_id)
            .write_u8(0) // grade
            .write_i32(count)
            .write_u8(0) // currency: money
            .write_bool(false); // use AA point
        self.send_cs(cs::CSBuyItemsPacket, s.as_bytes()).await
    }

    /// `CSDestroyItemPacket`: destroy `count` of the bag item `item_id` at `slot`
    /// (`slot_type::INVENTORY` = 2; `economy/item_handlers.rs`). Count <= 0 destroys the stack.
    pub async fn destroy_item(&mut self, item_id: u64, slot: u8, count: i32) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_u64(item_id)
            .write_u8(2) // SlotType.Inventory
            .write_u8(slot)
            .write_i32(count);
        self.send_cs(cs::CSDestroyItemPacket, s.as_bytes()).await
    }

    /// `CSExpandSlotsPacket`: ask the server to expand the bag (`slot_type` 2, `inventory.rs
    /// ::expand_slot` consumes the expansion item / money it requires).
    pub async fn expand_slots(
        &mut self,
        slot_type: i32,
        auto_use_aa_point: bool,
    ) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_i32(slot_type).write_bool(auto_use_aa_point);
        self.send_cs(cs::CSExpandSlotsPacket, s.as_bytes()).await
    }

    /// `CSLootOpenBagPacket` with `lootAll`: the server `OpenBag(.., true)` auto-takes every
    /// loot entry into the bag (`systems/economy/looting.rs`).
    pub async fn loot_all(&mut self, owner_obj: u32) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_bc(owner_obj).write_bc(0).write_bool(true);
        self.send_cs(cs::CSLootOpenBagPacket, s.as_bytes()).await
    }

    /// `CSLootCloseBagPacket`: releases the loot container of `owner_obj`.
    pub async fn close_loot_bag(&mut self, owner_obj: u32) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_u16(0) // item index
            .write_u16(0) // owner type
            .write_bc(owner_obj)
            .write_u8(0);
        self.send_cs(cs::CSLootCloseBagPacket, s.as_bytes()).await
    }

    /// `CSCreateDoodadPacket` (0x0e6): plant / place doodad `id` from the bag item `item_id`.
    pub async fn create_doodad(
        &mut self,
        id: u32,
        pos: (f32, f32, f32),
        z_rot: f32,
        scale: f32,
        item_id: u64,
    ) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_u32(id)
            .write_i64(aaemu_commons::helpers::convert_coord_to_long(pos.0))
            .write_i64(aaemu_commons::helpers::convert_coord_to_long(pos.1))
            .write_f32(pos.2)
            .write_f32(z_rot)
            .write_f32(scale)
            .write_u64(item_id);
        self.send_cs(cs::CSCreateDoodadPacket, s.as_bytes()).await
    }

    /// `CSStartSkillPacket`: unit caster (us) on a doodad target (`SkillCastDoodadTarget`), what
    /// the client sends for a doodad interaction skill.
    pub async fn start_doodad_skill(
        &mut self,
        skill_id: u32,
        doodad_obj: u32,
    ) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_u32(skill_id)
            .write_u8(0) // SkillCasterType.Unit
            .write_bc(self.state.self_obj)
            .write_u8(4) // SkillCastTargetType.Doodad
            .write_bc(doodad_obj)
            .write_u8(0); // flag: no skill object
        self.send_cs(cs::CSStartSkillPacket, s.as_bytes()).await
    }

    /// `CSStartSkillPacket` cast from an inventory item (the item's use skill) on ourselves,
    /// as the client does for a summon item.
    pub async fn use_item_skill(
        &mut self,
        skill_id: u32,
        item_id: u64,
        item_template_id: u32,
    ) -> anyhow::Result<()> {
        let me = self.state.self_obj;
        self.start_item_skill(skill_id, item_id, item_template_id, me)
            .await
    }

    /// `CSStartSkillPacket` cast by an item (the 1.2 client's item-use skill): item caster
    /// (our object, the item id and template), unit target, no skill object.
    pub async fn start_item_skill(
        &mut self,
        skill_id: u32,
        item_id: u64,
        item_template_id: u32,
        target_obj: u32,
    ) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_u32(skill_id)
            .write_u8(2) // SkillCasterType.Item
            .write_bc(self.state.self_obj)
            .write_u64(item_id)
            .write_u32(item_template_id)
            .write_u8(0) // type1
            .write_u32(0) // type2
            .write_u8(0) // SkillCastUnitTarget
            .write_bc(target_obj)
            .write_u8(0); // flag: no skill object
        self.send_cs(cs::CSStartSkillPacket, s.as_bytes()).await
    }

    /// `CSStartSkillPacket` of a summon item with a `SkillCastPositionTarget` (the `SummonPos`
    /// spot the client picked): `pos` is `(x, y, z)`, `rot` the yaw.
    pub async fn start_item_skill_at(
        &mut self,
        skill_id: u32,
        item_id: u64,
        item_template_id: u32,
        pos: (f32, f32, f32),
        rot: f32,
    ) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_u32(skill_id)
            .write_u8(2) // SkillCasterType.Item
            .write_bc(self.state.self_obj)
            .write_u64(item_id)
            .write_u32(item_template_id)
            .write_u8(0) // type1
            .write_u32(0) // type2
            .write_u8(1) // SkillCastTargetType.Position
            .write_i64(aaemu_commons::helpers::convert_coord_to_long(pos.0))
            .write_i64(aaemu_commons::helpers::convert_coord_to_long(pos.1))
            .write_f32(pos.2)
            .write_f32(rot)
            .write_bc(0)
            .write_bc(0)
            .write_u8(0); // flag: no skill object
        self.send_cs(cs::CSStartSkillPacket, s.as_bytes()).await
    }

    /// `CSRemoveMatePacket`: dismiss the mate with transport id `tl_id`.
    pub async fn remove_mate(&mut self, tl_id: u16) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_u16(tl_id);
        self.send_cs(cs::CSRemoveMatePacket, s.as_bytes()).await
    }

    /// `CSRepairPetItemsPacket`: the stablehand revive of dead summon items; the NPC must be the
    /// current interaction object (`CSInteractNPCPacket` first).
    pub async fn repair_pet_items(&mut self, npc_obj: u32) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_bc(npc_obj);
        self.send_cs(cs::CSRepairPetItemsPacket, s.as_bytes()).await
    }

    /// `CSInteractNPCPacket`.
    pub async fn interact_npc(&mut self, npc_obj: u32, target_changed: bool) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_bc(npc_obj).write_bool(target_changed);
        self.send_cs(cs::CSInteractNPCPacket, s.as_bytes()).await
    }

    /// `CSStartInteractionPacket` (asks the NPC for its interaction skill list).
    pub async fn start_interaction(&mut self, npc_obj: u32) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_bc(npc_obj)
            .write_bc(npc_obj)
            .write_i32(0) // extra info
            .write_i32(0) // pick id
            .write_u8(1) // mouse button
            .write_i32(0); // modifier keys
        self.send_cs(cs::CSStartInteractionPacket, s.as_bytes())
            .await
    }

    /// Select the unit used by auto attacks, as the real client does before attacking.
    pub async fn change_target(&mut self, target_obj: u32) -> anyhow::Result<()> {
        let mut packet = PacketStream::new();
        packet.write_bc(target_obj);
        self.send_cs(cs::CSChangeTargetPacket, packet.as_bytes())
            .await
    }

    /// `CSSwapItemsPacket` (bag -> bag).
    pub async fn swap_items(
        &mut self,
        from_item: u64,
        from_slot: u8,
        to_item: u64,
        to_slot: u8,
    ) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_u64(from_item)
            .write_u64(to_item)
            .write_u8(2)
            .write_u8(from_slot)
            .write_u8(2)
            .write_u8(to_slot);
        self.send_cs(cs::CSSwapItemsPacket, s.as_bytes()).await
    }

    /// `CSStartQuestContextPacket` without an NPC / doodad / sphere.
    pub async fn start_quest(&mut self, quest_id: u32, npc_obj: u32) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_u32(quest_id)
            .write_bc(npc_obj)
            .write_bc(0)
            .write_u32(0);
        self.send_cs(cs::CSStartQuestContextPacket, s.as_bytes())
            .await
    }

    /// `CSStartQuestContextPacket` with the doodad acceptor
    /// (`systems::quests::events::add_quest_from_doodad`).
    pub async fn start_quest_doodad(
        &mut self,
        quest_id: u32,
        doodad_obj: u32,
    ) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_u32(quest_id)
            .write_bc(0)
            .write_bc(doodad_obj)
            .write_u32(0);
        self.send_cs(cs::CSStartQuestContextPacket, s.as_bytes())
            .await
    }

    /// `CSCompleteQuestContextPacket`: turn in / report a quest at an object (`obj_id == 0`
    /// reports without one). The server resolves the NPC field first and the doodad field
    /// second (`systems::quests::events::on_report`), so a tracked NPC unit goes into the NPC
    /// field and any other object into the doodad field.
    pub async fn complete_quest(
        &mut self,
        quest_id: u32,
        obj_id: u32,
        selected_reward: i32,
    ) -> anyhow::Result<()> {
        let is_npc = self
            .state
            .units
            .get(&obj_id)
            .is_some_and(|u| u.unit_type == 1);
        let (npc_obj, doodad_obj) = if is_npc { (obj_id, 0) } else { (0, obj_id) };
        let body = quest::complete_quest_body(quest_id, npc_obj, doodad_obj, selected_reward);
        self.send_cs(cs::CSCompleteQuestContextPacket, &body).await
    }

    /// `CSCompleteQuestContextPacket` with the doodad field explicitly set (report at a doodad).
    pub async fn complete_quest_doodad(
        &mut self,
        quest_id: u32,
        doodad_obj: u32,
        selected_reward: i32,
    ) -> anyhow::Result<()> {
        let body = quest::complete_quest_body(quest_id, 0, doodad_obj, selected_reward);
        self.send_cs(cs::CSCompleteQuestContextPacket, &body).await
    }

    /// `CSQuestTalkMadePacket`: the answer to a quest talk act while the objective runs.
    pub async fn quest_talk(
        &mut self,
        quest_id: u32,
        npc_obj: u32,
        component_id: u32,
        act_id: u32,
    ) -> anyhow::Result<()> {
        let body = quest::quest_talk_body(npc_obj, quest_id, component_id, act_id);
        self.send_cs(cs::CSQuestTalkMadePacket, &body).await
    }

    /// `CSDropQuestContextPacket`.
    pub async fn drop_quest(&mut self, quest_id: u32) -> anyhow::Result<()> {
        let body = quest::drop_quest_body(quest_id);
        self.send_cs(cs::CSDropQuestContextPacket, &body).await
    }

    /// `CSLeaveWorldPacket` (0 = quit, 1 = character select, 2 = server select).
    pub async fn leave_world(&mut self, target: u8) -> anyhow::Result<()> {
        self.send_cs(cs::CSLeaveWorldPacket, &[target]).await
    }

    /// Closest unit of `unit_type` to our position.
    pub fn nearest_unit(&self, unit_type: u8) -> Option<UnitInfo> {
        let me = self.state.self_pos;
        self.state
            .units
            .values()
            .filter(|u| u.unit_type == unit_type && u.obj_id != self.state.self_obj)
            .min_by(|a, b| dist2(a.pos, me).total_cmp(&dist2(b.pos, me)))
            .cloned()
    }

    /// Closest unit of `unit_type` with the given template / character id (type 1: NPC template
    /// id, type 0: character id), measured from our own position.
    pub fn find_unit(&self, unit_type: u8, template_id: u32) -> Option<UnitInfo> {
        let me = self.state.self_pos;
        self.state
            .units
            .values()
            .filter(|u| {
                u.unit_type == unit_type && u.id == template_id && u.obj_id != self.state.self_obj
            })
            .min_by(|a, b| dist2(a.pos, me).total_cmp(&dist2(b.pos, me)))
            .cloned()
    }

    /// A unit of `unit_type` within `range` picked independently of object ids and wandering:
    /// the lowest template / character id, then the closest. Used where two servers must be
    /// driven identically (parity runs).
    pub fn stable_unit(&self, unit_type: u8, range: f32) -> Option<UnitInfo> {
        let me = self.state.self_pos;
        self.state
            .units
            .values()
            .filter(|u| {
                u.unit_type == unit_type
                    && u.obj_id != self.state.self_obj
                    && dist2(u.pos, me) <= range * range
            })
            .min_by(|a, b| {
                a.id.cmp(&b.id)
                    .then(dist2(a.pos, me).total_cmp(&dist2(b.pos, me)))
            })
            .cloned()
    }
}

fn dist2(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
    let (dx, dy, dz) = (a.0 - b.0, a.1 - b.1, a.2 - b.2);
    dx * dx + dy * dy + dz * dz
}

/// `CSSendChatMessagePacket` body as the bot sends chat (no target, language 0, ability 0),
/// matching the server's `read`.
fn chat_message_body(chat_type: i16, target_name: &str, message: &str) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_i16(chat_type)
        .write_i16(0)
        .write_i32(0)
        .write_str(target_name)
        .write_str(message)
        .write_u8(0) // language
        .write_i32(0); // ability
    s.into_bytes()
}

/// `CSCreateCharacterPacket` body as the bot sends it: name, race, gender, default body items,
/// default appearance, Fighter ability 1/None/None and level 1. Matches the server's `read`.
fn create_character_body(name: &str, race: u8, gender: u8) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_str(name).write_u8(race).write_u8(gender);
    for _ in 0..7 {
        s.write_u32(0); // body items: template defaults
    }
    write_custom_model(&mut s);
    s.write_u8(1) // ability1: Fight
        .write_u8(11) // ability2: None
        .write_u8(11) // ability3: None
        .write_u8(1); // level
    s.into_bytes()
}

/// `CSStartSkillPacket` body: skill id, caster and target (their own `write` methods emit the
/// type bytes the reader consumes) and the "no skill object" flag.
fn start_skill_body(skill_id: u32, caster: &SkillCaster, target: &SkillCastTarget) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_u32(skill_id);
    caster.write(&mut s);
    target.write(&mut s);
    s.write_u8(0); // flag: no skill object
    s.into_bytes()
}

/// `CSLearnSkillPacket` body: just the skill id (`systems/skills/handlers.rs` `read`).
fn learn_skill_body(skill_id: u32) -> Vec<u8> {
    skill_id.to_le_bytes().to_vec()
}

/// `CSSwapAbilityPacket` body: `objId` (24-bit), old ability, new ability, `auap` bool.
fn swap_ability_body(obj_id: u32, old_ability: u8, ability: u8) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_bc(obj_id)
        .write_u8(old_ability)
        .write_u8(ability)
        .write_bool(false);
    s.into_bytes()
}

/// `CSResetSkillsPacket` body: ability id and the `ausp` bool.
fn reset_skills_body(ability_id: u8) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_u8(ability_id).write_bool(false);
    s.into_bytes()
}

/// `CSTeleportEndedPacket` body (`systems/npcs/portal/packets.rs` `read`): the fixed point x/y,
/// z and 16 orientation bytes (the identity quaternion).
fn teleport_ended_body(pos: (f32, f32, f32)) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_i64(helpers::convert_coord_to_long(pos.0))
        .write_i64(helpers::convert_coord_to_long(pos.1))
        .write_f32(pos.2)
        .write_raw(&TELEPORT_ENDED_ORIENTATION);
    s.into_bytes()
}

fn facing_direction(pos: (f32, f32, f32), target: (f32, f32, f32)) -> i8 {
    let (dx, dy) = (target.0 - pos.0, target.1 - pos.1);
    if dx == 0.0 && dy == 0.0 {
        return 0;
    }
    let yaw = (f64::from(dy).atan2(f64::from(dx)) - std::f64::consts::FRAC_PI_2
        + std::f64::consts::PI)
        .rem_euclid(std::f64::consts::TAU)
        - std::f64::consts::PI;
    aaemu_game::models::movement::convert_radian_to_direction(yaw)
}

/// `UnitCustomModelParams` with a full face block (type 3), all defaults.
fn write_custom_model(s: &mut PacketStream) {
    s.write_u8(3) // UnitCustomModelType.Face
        .write_u32(0) // hair color
        .write_u32(0) // skin color
        .write_u32(0); // model id
                       // UnitCustomFace
    s.write_u32(0)
        .write_f32(0.0)
        .write_f32(0.0)
        .write_f32(0.0)
        .write_i16(0)
        .write_i16(0);
    for _ in 0..4 {
        s.write_u32(0).write_f32(0.0); // fixed decal assets
    }
    for _ in 0..3 {
        s.write_u32(0); // diffuse, normal, eyelash maps
    }
    s.write_f32(0.0); // normal map weight
    for _ in 0..5 {
        s.write_u32(0); // lip, pupils, eyebrow, deco colors
    }
    s.write_bytes_sized(&[0u8; 128]); // modifier
}

#[cfg(test)]
mod facing_tests {
    use super::*;

    #[test]
    fn movement_faces_each_compass_target_using_the_1_2_yaw() {
        let origin = (0.0, 0.0, 0.0);
        for (target, direction) in [
            ((0.0, 1.0, 0.0), 0),
            ((-1.0, 0.0, 0.0), 32),
            ((1.0, 0.0, 0.0), -32),
            ((0.0, -1.0, 0.0), 64),
        ] {
            assert_eq!(facing_direction(origin, target), direction);
        }
    }
}

#[cfg(test)]
mod body_tests {
    use super::*;
    use aaemu_commons::Marshal;
    use aaemu_game::net::registry::ClientPacket;

    fn packet(opcode: u16, body: Vec<u8>) -> Packet {
        Packet {
            level: LEVEL_GAME,
            opcode,
            body,
        }
    }

    #[test]
    fn chat_body_decodes_with_the_server_reader() {
        let body = chat_message_body(chat_type::WHITE, "", "/move 1 2 3");
        let p = aaemu_game::packets::c2g_world::CSSendChatMessagePacket::read(
            &mut PacketStream::from_bytes(body),
        )
        .unwrap();
        assert_eq!(p.chat_type, chat_type::WHITE);
        assert_eq!(p.target_name, "");
        assert_eq!(p.message, "/move 1 2 3");
        assert_eq!(p.language_type, 0);
        assert_eq!(p.ability, 0);
    }

    #[test]
    fn teleport_ended_body_decodes_with_the_server_reader() {
        let body = teleport_ended_body((1234.5, -678.25, 78.5));
        let p = aaemu_game::systems::npcs::portal::packets::CSTeleportEndedPacket::read(
            &mut PacketStream::from_bytes(body),
        )
        .unwrap();
        assert_eq!((p.x, p.y, p.z), (1234.5, -678.25, 78.5));
        assert_eq!(p.ori, TELEPORT_ENDED_ORIENTATION);
    }

    #[test]
    fn learn_swap_and_reset_bodies_decode_with_the_server_readers() {
        use aaemu_game::systems::skills::handlers::{
            CSLearnSkillPacket, CSResetSkillsPacket, CSSwapAbilityPacket,
        };

        let p = CSLearnSkillPacket::read(&mut PacketStream::from_bytes(learn_skill_body(10375)))
            .unwrap();
        assert_eq!(p.skill_id, 10375);

        let p = CSSwapAbilityPacket::read(&mut PacketStream::from_bytes(swap_ability_body(
            0x1234, 11, 7,
        )))
        .unwrap();
        assert_eq!((p.obj_id, p.old_ability, p.ability), (0x1234, 11, 7));
        assert!(!p.auap);

        let p =
            CSResetSkillsPacket::read(&mut PacketStream::from_bytes(reset_skills_body(4))).unwrap();
        assert_eq!(p.ability_id, 4);
        assert!(!p.ausp);
    }

    #[test]
    fn buff_packets_track_created_and_removed() {
        use aaemu_game::systems::skills::packets::{SCBuffCreatedPacket, SCBuffRemovedPacket};

        let mut st = GameState::default();
        let created = SCBuffCreatedPacket {
            skill_caster: SkillCaster::Unit(0x100),
            caster_id: 42,
            owner_obj_id: 0x100,
            index: 3,
            buff_id: 2206,
            caster_level: 55,
            ab_level: 55,
            skill_id: 10375,
            charge: 1,
            duration: 10_000,
            tick: 0.0,
        };
        let mut body = PacketStream::new();
        created.write(&mut body);
        observe_into(&mut st, &packet(sc::SCBuffCreatedPacket, body.into_bytes())).unwrap();
        assert_eq!(
            st.buffs[&0x100][&3],
            BuffInfo {
                buff_id: 2206,
                skill_id: 10375,
                index: 3,
            }
        );

        let mut body = PacketStream::new();
        SCBuffRemovedPacket {
            obj_id: 0x100,
            index: 3,
        }
        .write(&mut body);
        observe_into(&mut st, &packet(sc::SCBuffRemovedPacket, body.into_bytes())).unwrap();
        assert!(st.buffs[&0x100].is_empty());
    }

    #[test]
    fn create_character_body_decodes_with_the_server_reader() {
        let body = create_character_body("SomeBot", 6, 2);
        let p = aaemu_game::packets::c2g_world::CSCreateCharacterPacket::read(
            &mut PacketStream::from_bytes(body),
        )
        .unwrap()
        .0;
        assert_eq!(p.name, "SomeBot");
        assert_eq!(p.race, 6);
        assert_eq!(p.gender, 2);
        assert_eq!(p.body_items, [0; 7]);
        assert_eq!((p.ability1, p.ability2, p.ability3), (1, 11, 11));
        assert_eq!(p.level, 1);
    }

    #[test]
    fn character_list_head_carries_count_and_race_gender_level() {
        // `SCCharacterListPacket::write`: [last][u8 count], then `Character.Write` rows of
        // which only the head is needed (id, name, race, gender, level, hp, mp, zone, faction;
        // `models/character.rs`).
        let mut st = GameState::default();
        let mut body = PacketStream::new();
        body.write_bool(true)
            .write_u8(1)
            .write_u32(0x1234)
            .write_str("Tester")
            .write_u8(4)
            .write_u8(2)
            .write_u8(7)
            .write_i32(1200)
            .write_i32(300)
            .write_u32(2)
            .write_u32(101);
        observe_into(
            &mut st,
            &packet(sc::SCCharacterListPacket, body.into_bytes()),
        )
        .unwrap();
        assert_eq!(st.character_count, 1);
        assert_eq!(
            st.characters,
            vec![CharInfo {
                id: 0x1234,
                name: "Tester".into(),
                race: 4,
                gender: 2,
                level: 7,
                faction: 101,
            }]
        );
        assert!(st.char_list_done);

        // Pairs: the count counts both rows, the parsed list keeps the first of each packet.
        let mut body = PacketStream::new();
        body.write_bool(false)
            .write_u8(2)
            .write_u32(1)
            .write_str("A")
            .write_u8(1)
            .write_u8(1)
            .write_u8(3)
            .write_i32(120)
            .write_i32(30)
            .write_u32(1)
            .write_u32(101); // the second row is not decoded (variable data follows)
        observe_into(
            &mut st,
            &packet(sc::SCCharacterListPacket, body.into_bytes()),
        )
        .unwrap();
        assert_eq!(st.character_count, 3);
        assert_eq!(st.characters.len(), 2);
        assert_eq!(st.characters[1].name, "A");
    }

    #[test]
    fn create_response_head_carries_race_gender_level() {
        let mut body = PacketStream::new();
        body.write_u32(0x99)
            .write_str("NewBot")
            .write_u8(5)
            .write_u8(1)
            .write_u8(1)
            .write_i32(500)
            .write_i32(100)
            .write_u32(1)
            .write_u32(109);
        let mut st = GameState::default();
        observe_into(
            &mut st,
            &packet(sc::SCCreateCharacterResponsePacket, body.into_bytes()),
        )
        .unwrap();
        assert_eq!(
            st.characters,
            vec![CharInfo {
                id: 0x99,
                name: "NewBot".into(),
                race: 5,
                gender: 1,
                level: 1,
                faction: 109,
            }]
        );
    }

    #[test]
    fn character_state_head_sets_self_info() {
        // `unit_state::character_state`: instance id, sized guid, rwd, `Character.Write`.
        let mut body = PacketStream::new();
        body.write_u32(100)
            .write_u16(16)
            .write_raw(&[0u8; 16])
            .write_i32(0)
            .write_u32(0x77)
            .write_str("Me")
            .write_u8(6)
            .write_u8(2)
            .write_u8(14)
            .write_i32(2000)
            .write_i32(800)
            .write_u32(184)
            .write_u32(113);
        let mut st = GameState::default();
        observe_into(
            &mut st,
            &packet(sc::SCCharacterStatePacket, body.into_bytes()),
        )
        .unwrap();
        assert_eq!(
            st.self_info,
            Some(CharInfo {
                id: 0x77,
                name: "Me".into(),
                race: 6,
                gender: 2,
                level: 14,
                faction: 113,
            })
        );
        // The snapshot's instance id is tracked too (0 = the main world).
        assert_eq!(st.world_instance, 100);
    }

    /// `SCLoadInstancePacket`: the server moved the character to another world instance; the
    /// old world's units / doodads are stale and the packet carries the new position.
    #[test]
    fn load_instance_packet_resets_the_world_view() {
        let mut st = GameState::default();
        st.units.insert(
            7,
            UnitInfo {
                obj_id: 7,
                name: "Old world npc".into(),
                unit_type: 1,
                id: 100,
                level: 3,
                pos: (1.0, 2.0, 3.0),
                tl_id: 0,
            },
        );
        st.doodads.insert(
            9,
            DoodadInfo {
                obj_id: 9,
                template_id: 200,
                pos: (0.0, 0.0, 0.0),
                func_group_id: 0,
                owner_obj_id: 0,
                parent_obj_id: 0,
                quest_glow: 0,
                data: 0,
            },
        );
        let mut body = PacketStream::new();
        body.write_u32(1) // instance id (arche_mall_world)
            .write_u32(260) // zone key
            .write_f32(3680.5)
            .write_f32(4572.25)
            .write_f32(156.0)
            .write_f32(0.0)
            .write_f32(0.0)
            .write_f32(0.0);
        observe_into(
            &mut st,
            &packet(sc::SCLoadInstancePacket, body.into_bytes()),
        )
        .unwrap();
        assert_eq!(st.world_instance, 1);
        assert_eq!(st.world_zone, 260);
        assert_eq!(st.self_pos, (3680.5, 4572.25, 156.0));
        assert!(st.units.is_empty());
        assert!(st.doodads.is_empty());
    }

    #[test]
    fn doodad_visibility_packets_track_templates_and_positions() {
        use aaemu_game::models::doodad::Doodad;
        use aaemu_game::models::transform::PositionAndRotation;
        use aaemu_game::packets::world_objects::{
            SCDoodadRemovedPacket, SCDoodadsCreatedPacket, SCDoodadsRemovedPacket,
        };

        // `world::visibility::doodad_body` writes `Doodad.Write` (`models/doodad.rs`); with the
        // default System attach point the local transform is written.
        let mut one = Doodad::new(1000);
        one.func_group_id = 5;
        one.transform.x = 10.0;
        one.transform.y = 20.0;
        one.transform.z = 30.0;
        let mut body = PacketStream::new();
        one.write(
            0x1234,
            &PositionAndRotation::new(0.0, 0.0, 0.0, 0.0, 0.0, 0.0),
            0,
            &mut body,
        );
        let mut st = GameState::default();
        observe_into(
            &mut st,
            &packet(sc::SCDoodadCreatedPacket, body.into_bytes()),
        )
        .unwrap();
        let doodad = st.doodads[&0x1234].clone();
        assert_eq!(doodad.template_id, 1000);
        assert!((doodad.pos.0 - 10.0).abs() < 0.01);
        assert!((doodad.pos.1 - 20.0).abs() < 0.01);
        assert!((doodad.pos.2 - 30.0).abs() < 0.01);

        // `SCDoodadsCreatedPacket`: u8 count + N bodies.
        let mut second = Doodad::new(2000);
        second.transform.x = 1.0;
        second.transform.y = 2.0;
        second.transform.z = 3.0;
        let mut body2 = PacketStream::new();
        second.write(
            0x2222,
            &PositionAndRotation::new(0.0, 0.0, 0.0, 0.0, 0.0, 0.0),
            0,
            &mut body2,
        );
        let mut batch = PacketStream::new();
        SCDoodadsCreatedPacket {
            bodies: vec![body2.into_bytes()],
        }
        .write(&mut batch);
        observe_into(
            &mut st,
            &packet(sc::SCDoodadsCreatedPacket, batch.into_bytes()),
        )
        .unwrap();
        assert_eq!(st.doodads[&0x2222].template_id, 2000);
        assert!(st.find_doodad(2000).is_some());
        assert_eq!(st.find_doodad(3000), None);

        // Removals by batch and by single id.
        let mut remove = PacketStream::new();
        SCDoodadsRemovedPacket {
            last: true,
            ids: vec![0x2222],
        }
        .write(&mut remove);
        observe_into(
            &mut st,
            &packet(sc::SCDoodadsRemovedPacket, remove.into_bytes()),
        )
        .unwrap();
        assert!(!st.doodads.contains_key(&0x2222));

        let mut remove = PacketStream::new();
        SCDoodadRemovedPacket { obj_id: 0x1234 }.write(&mut remove);
        observe_into(
            &mut st,
            &packet(sc::SCDoodadRemovedPacket, remove.into_bytes()),
        )
        .unwrap();
        assert!(st.doodads.is_empty());
    }
}

#[cfg(test)]
mod compressed_batch_tests {
    use super::*;
    use aaemu_game::systems::skills::packets::{
        body_of, deflate_stored, CompressedGamePackets, SCAiAggroPacket, SCBuffCreatedPacket,
        SCBuffRemovedPacket, SCPlotEndedPacket, SCPlotEventPacket, SCSkillEndedPacket,
        SCSkillStartedPacket, SCUnitDamagedPacket,
    };
    use aaemu_game::systems::skills::types::{CastAction, PlotObject};

    /// The exact payload the server builds in `CompressedGamePackets::raw_payload`.
    fn payload(packets: &[CompressedGamePackets]) -> Vec<u8> {
        packets.iter().flat_map(|p| p.raw_payload()).collect()
    }

    fn unit_points() -> CompressedGamePackets {
        let mut b = CompressedGamePackets::new();
        b.add(&aaemu_game::packets::world::SCUnitPointsPacket {
            obj_id: 0x1234,
            hp: 555,
            mp: 777,
        });
        b
    }

    fn skill_started() -> CompressedGamePackets {
        let mut b = CompressedGamePackets::new();
        let mut p = SCSkillStartedPacket::new(
            10375,
            7,
            SkillCaster::Unit(0x100),
            SkillCastTarget::Unit(0x200),
            SkillObject::default(),
        );
        p.real_cast_time_div10 = 150;
        p.base_cast_time_div10 = 150;
        p.set_skill_result(8);
        p.set_result_uint(0xDEAD_BEEF);
        b.add(&p);
        b
    }

    fn buff_created() -> CompressedGamePackets {
        let mut b = CompressedGamePackets::new();
        b.add(&SCBuffCreatedPacket {
            skill_caster: SkillCaster::Unit(0x100),
            caster_id: 42,
            owner_obj_id: 0x100,
            index: 3,
            buff_id: 870,
            caster_level: 55,
            ab_level: 55,
            skill_id: 10644,
            charge: 2,
            duration: 10_000,
            tick: 1.5,
        });
        b
    }

    fn plot_event() -> CompressedGamePackets {
        let mut b = CompressedGamePackets::new();
        b.add(&SCPlotEventPacket {
            tl: 9,
            event_id: 5315,
            skill_id: 10644,
            caster: PlotObject::Unit(0x100),
            target: PlotObject::Position {
                x: 10.0,
                y: 20.0,
                z: 30.0,
                roll: 1,
                pitch: 2,
                yaw: 3,
            },
            obj_id: 0,
            casting_time: 0,
            flag: 2,
            item_id: 0,
            target_unit_count: 1,
        });
        b
    }

    fn unit_damaged() -> CompressedGamePackets {
        let mut b = CompressedGamePackets::new();
        let mut p = SCUnitDamagedPacket::new(
            CastAction::Skill {
                skill_id: 19050,
                tl_id: 9,
            },
            SkillCaster::Unit(0x100),
            0x100,
            0x200,
            163,
            0,
        );
        p.mana_burn = 12;
        p.holdable_id = 3;
        p.hit_type = aaemu_game::systems::skills::types::SkillHitType::MeleeHit;
        b.add(&p);
        b
    }

    #[test]
    fn decodes_every_inner_packet_of_a_batch() {
        let batch = payload(&[
            unit_points(),
            skill_started(),
            buff_created(),
            {
                let mut b = CompressedGamePackets::new();
                b.add(&SCBuffRemovedPacket {
                    obj_id: 0x100,
                    index: 3,
                });
                b
            },
            plot_event(),
            {
                let mut b = CompressedGamePackets::new();
                b.add(&SCPlotEndedPacket { tl: 9 });
                b
            },
            unit_damaged(),
            {
                let mut b = CompressedGamePackets::new();
                b.add(&SCSkillEndedPacket { tl: 7 });
                b
            },
            {
                let mut b = CompressedGamePackets::new();
                b.add(&SCAiAggroPacket {
                    npc_id: 0x200,
                    count: 1,
                    hostile_unit_id: 0x100,
                    summarize_damage: 163,
                    val2: 0,
                    val3: 0,
                });
                b
            },
        ]);

        let decoded = split_compressed_batch(&batch);
        let expected = vec![
            (
                sc::SCUnitPointsPacket,
                body_of(&aaemu_game::packets::world::SCUnitPointsPacket {
                    obj_id: 0x1234,
                    hp: 555,
                    mp: 777,
                }),
            ),
            (
                sc::SCSkillStartedPacket,
                body_of(&{
                    let mut p = SCSkillStartedPacket::new(
                        10375,
                        7,
                        SkillCaster::Unit(0x100),
                        SkillCastTarget::Unit(0x200),
                        SkillObject::default(),
                    );
                    p.real_cast_time_div10 = 150;
                    p.base_cast_time_div10 = 150;
                    p.set_skill_result(8);
                    p.set_result_uint(0xDEAD_BEEF);
                    p
                }),
            ),
            (
                sc::SCBuffCreatedPacket,
                body_of(&SCBuffCreatedPacket {
                    skill_caster: SkillCaster::Unit(0x100),
                    caster_id: 42,
                    owner_obj_id: 0x100,
                    index: 3,
                    buff_id: 870,
                    caster_level: 55,
                    ab_level: 55,
                    skill_id: 10644,
                    charge: 2,
                    duration: 10_000,
                    tick: 1.5,
                }),
            ),
            (
                sc::SCBuffRemovedPacket,
                body_of(&SCBuffRemovedPacket {
                    obj_id: 0x100,
                    index: 3,
                }),
            ),
            (
                sc::SCPlotEventPacket,
                body_of(&SCPlotEventPacket {
                    tl: 9,
                    event_id: 5315,
                    skill_id: 10644,
                    caster: PlotObject::Unit(0x100),
                    target: PlotObject::Position {
                        x: 10.0,
                        y: 20.0,
                        z: 30.0,
                        roll: 1,
                        pitch: 2,
                        yaw: 3,
                    },
                    obj_id: 0,
                    casting_time: 0,
                    flag: 2,
                    item_id: 0,
                    target_unit_count: 1,
                }),
            ),
            (sc::SCPlotEndedPacket, body_of(&SCPlotEndedPacket { tl: 9 })),
            (
                sc::SCUnitDamagedPacket,
                body_of(&{
                    let mut p = SCUnitDamagedPacket::new(
                        CastAction::Skill {
                            skill_id: 19050,
                            tl_id: 9,
                        },
                        SkillCaster::Unit(0x100),
                        0x100,
                        0x200,
                        163,
                        0,
                    );
                    p.mana_burn = 12;
                    p.holdable_id = 3;
                    p.hit_type = aaemu_game::systems::skills::types::SkillHitType::MeleeHit;
                    p
                }),
            ),
            (
                sc::SCSkillEndedPacket,
                body_of(&SCSkillEndedPacket { tl: 7 }),
            ),
            (
                sc::SCAiAggroPacket,
                body_of(&SCAiAggroPacket {
                    npc_id: 0x200,
                    count: 1,
                    hostile_unit_id: 0x100,
                    summarize_damage: 163,
                    val2: 0,
                    val3: 0,
                }),
            ),
        ];
        assert_eq!(decoded, expected);
    }

    #[test]
    fn the_deflated_wire_batch_decodes_to_the_same_packets() {
        let batch = payload(&[unit_points(), plot_event()]);
        let wire = deflate_stored(&batch);
        let inflated = miniz_oxide::inflate::decompress_to_vec(&wire).unwrap();
        assert_eq!(inflated, batch);
        let decoded = split_compressed_batch(&inflated);
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].0, sc::SCUnitPointsPacket);
        assert_eq!(decoded[1].0, sc::SCPlotEventPacket);
    }

    #[test]
    fn an_unknown_inner_opcode_stops_the_split() {
        let mut raw = payload(&[unit_points()]);
        let before = raw.len();
        raw.extend_from_slice(&[0, 0, 0x34, 0x12, 1, 2, 3]);
        let decoded = split_compressed_batch(&raw);
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].0, sc::SCUnitPointsPacket);
        assert_eq!(decoded[0].1.len(), before - 4);
    }
}
