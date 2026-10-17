//! Game server client (C2G / G2C + the proxy channel) for the 1.2 protocol.
//!
//! Every frame payload is `[0xDD][level]...`: level 1 (game) carries `[hash][count][u16 opcode]`,
//! level 2 (proxy / CryNetwork) just `[u16 opcode]`. 1.2 traffic is neither encrypted nor
//! compressed.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::time::{Duration, Instant};

use aaemu_commons::helpers;
use aaemu_commons::net::FrameCodec;
use aaemu_commons::PacketStream;
use aaemu_game::opcodes::{cs, name_of, pp, sc};
use aaemu_game::systems::quests::model::status as quest_status;
use aaemu_game::systems::services::commands::COMMAND_PREFIX;
use aaemu_game::systems::social::chat::chat_type;
use anyhow::{anyhow, bail, ensure, Context};
use futures::{SinkExt, StreamExt};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_util::codec::{FramedRead, FramedWrite};

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
}

/// A unit seen in `SCUnitStatePacket`.
#[derive(Clone, Debug, PartialEq)]
pub struct UnitInfo {
    pub obj_id: u32,
    pub name: String,
    pub unit_type: u8,
    /// Character id (type 0) or NPC template id (type 1).
    pub id: u32,
    pub pos: (f32, f32, f32),
}

/// How many recent inbound chat texts [`GameState`] keeps.
pub const CHAT_LOG_LIMIT: usize = 64;

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
    pub units: HashMap<u32, UnitInfo>,
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
    /// First item of the bag (`SCCharacterInvenContentsPacket`, slot type 2): `(item id, slot)`.
    pub bag_items: Vec<(u64, u8, u32)>,
    pub bag_slots: u8,
    pub disconnected: bool,
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
            // ([u16 0][u16 typeId][body])*]. The inner packets carry no lengths; the capture
            // keeps the inflated batch (C# and Rust compress differently, the content is what
            // must match).
            let count = u16::from_le_bytes([frame[2], frame[3]]);
            let body = miniz_oxide::inflate::decompress_to_vec(&frame[4..])
                .map_err(|e| anyhow!("DD04 inflate failed: {e:?}"))?;
            let p = Packet {
                level,
                opcode: count,
                body,
            };
            self.rec.push(
                Channel::Game,
                Dir::S2C,
                level,
                count,
                "CompressedGamePackets",
                &p.body,
            );
            return Ok(p);
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
            _ => return Ok(()),
        };
        let _owner = s.read_string()?;
        let pos = s.read_position()?;
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
                pos,
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
        }
    } else if p.is_sc(sc::SCCharacterInvenContentsPacket) {
        let slot_type = s.read_u8()?;
        let _num_chunks = s.read_u8()?;
        let chunk = s.read_u8()?;
        if slot_type == 2 {
            // 10 items per chunk; only leading items can be located (variable layout).
            let tpl = s.read_u32()?;
            if tpl != 0 {
                let id = s.read_u64()?;
                st.bag_items.push((id, chunk * 10, tpl));
            }
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
    } else if p.is_sc(sc::SCCharacterStatePacket) {
        // `unit_state::character_state`: instance id, guid (sized bytes), rwd, then
        // `Character.Write` (id, name, race, gender, level, ...).
        s.read_u32()?; // instance id
        let guid_len = s.read_u16()?;
        s.read_bytes(usize::from(guid_len))?;
        s.read_i32()?; // rwd
        st.self_info = Some(read_character_head(&mut s)?);
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
        st.self_pos = pos;
        if st.self_obj != 0 {
            if let Some(u) = st.units.get_mut(&st.self_obj) {
                u.pos = pos;
            }
        }
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

/// The fixed head of `Character.Write` (`models/character.rs`): id, name, race, gender, level.
/// The equipment / model data after it is variable and irrelevant to the runner.
fn read_character_head(s: &mut PacketStream) -> anyhow::Result<CharInfo> {
    let id = s.read_u32()?;
    let name = s.read_string()?;
    let race = s.read_u8()?;
    let gender = s.read_u8()?;
    let level = s.read_u8()?;
    Ok(CharInfo {
        id,
        name,
        race,
        gender,
        level,
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

    async fn move_to_with_direction(
        &mut self,
        pos: (f32, f32, f32),
        time: u32,
        direction: i8,
    ) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_bc(self.state.self_obj)
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
    pub async fn teleport(&mut self, pos: (f32, f32, f32)) -> anyhow::Result<()> {
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

    /// `CSTeleportEndedPacket` (the client's answer to `SCTeleportUnitPacket`).
    async fn teleport_ended(&mut self, pos: (f32, f32, f32)) -> anyhow::Result<()> {
        let body = teleport_ended_body(pos);
        self.send_cs(cs::CSTeleportEndedPacket, &body).await
    }

    /// `CSStartSkillPacket`: unit caster (us) on a unit target.
    pub async fn start_skill(&mut self, skill_id: u32, target_obj: u32) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_u32(skill_id)
            .write_u8(0)
            .write_bc(self.state.self_obj)
            .write_u8(0)
            .write_bc(target_obj)
            .write_u8(0); // flag: no skill object
        self.send_cs(cs::CSStartSkillPacket, s.as_bytes()).await
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

/// `CSTeleportEndedPacket` body (`systems/npcs/portal/packets.rs` `read`): the fixed point x/y,
/// z and 16 orientation bytes.
fn teleport_ended_body(pos: (f32, f32, f32)) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_i64(helpers::convert_coord_to_long(pos.0))
        .write_i64(helpers::convert_coord_to_long(pos.1))
        .write_f32(pos.2)
        .write_raw(&[0u8; 16]);
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
        assert_eq!(p.ori, [0u8; 16]);
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
        // which only the head is needed (id, name, race, gender, level; `models/character.rs`).
        let mut st = GameState::default();
        let mut body = PacketStream::new();
        body.write_bool(true)
            .write_u8(1)
            .write_u32(0x1234)
            .write_str("Tester")
            .write_u8(4)
            .write_u8(2)
            .write_u8(7);
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
            .write_u8(3); // the second row is not decoded (variable data follows)
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
            .write_u8(1);
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
            .write_u8(14);
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
            })
        );
    }
}
