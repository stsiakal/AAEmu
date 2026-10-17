//! Quest-audit plumbing: the quest rows the bot reads back from the `SCQuestsPacket` /
//! `SCQuestContext*Packet` bodies and the `CS*` quest bodies it sends.
//!
//! The wire layouts mirror `rust/crates/aaemu-game/src/systems/quests/packets.rs` and
//! `models/char_data.rs` (`ActiveQuest::write` / the `ClientPacket::read` implementations);
//! the tests below encode with the server's own packet types wherever they are public.

use aaemu_commons::PacketStream;
use anyhow::ensure;

/// One active quest as written by `Quest.Write`: template id, status (see
/// `aaemu_game::systems::quests::model::status`) and the five objective counters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestInfo {
    pub template_id: u32,
    pub status: u8,
    pub objectives: [i32; 5],
}

/// Reads one `ActiveQuest` row (`Quest.Write`, `models/char_data.rs`) from a
/// `SCQuestsPacket` / `SCQuestContext*Packet` body.
pub fn read_quest_row(s: &mut PacketStream) -> anyhow::Result<QuestInfo> {
    s.read_i64()?; // quest instance id (long)
    let template_id = s.read_u32()?;
    let status = s.read_u8()?;
    let mut objectives = [0i32; 5];
    for o in objectives.iter_mut() {
        *o = s.read_i32()?;
    }
    s.read_bool()?; // isCheckSet
    s.read_bc()?; // obj id
    s.read_u32()?; // type id
    s.read_bc()?; // obj id
    s.read_bc()?; // obj id
    s.read_i32()?; // left time
    s.read_u32()?; // component id (0 without a time limit)
    s.read_i64()?; // doodad id
    s.read_i64()?; // accept time (seconds)
    s.read_u8()?; // acceptor type
    s.read_u32()?; // acceptor id
    Ok(QuestInfo {
        template_id,
        status,
        objectives,
    })
}

/// `SCQuestsPacket` body (`[i32 count][ActiveQuest...]`). The count is untrusted: negatives are
/// rejected and the rows are parsed incrementally (no capacity reservation from the wire value).
pub fn quest_rows(body: &[u8]) -> anyhow::Result<Vec<QuestInfo>> {
    let mut s = PacketStream::from_bytes(body.to_vec());
    let count = s.read_i32()?;
    ensure!(count >= 0, "negative SCQuestsPacket quest count {count}");
    let mut rows = Vec::new();
    for _ in 0..count {
        rows.push(read_quest_row(&mut s)?);
    }
    Ok(rows)
}

/// `SCCompletedQuestsPacket` body (`[i32 count]([u16 quest id][u8; 8 flags])*`).
pub fn completed_quest_rows(body: &[u8]) -> anyhow::Result<Vec<u32>> {
    let mut s = PacketStream::from_bytes(body.to_vec());
    let count = s.read_i32()?;
    ensure!(
        count >= 0,
        "negative SCCompletedQuestsPacket quest count {count}"
    );
    let mut ids = Vec::new();
    for _ in 0..count {
        ids.push(u32::from(s.read_u16()?));
        s.read_u64()?; // completed flags (BitArray of 64)
    }
    Ok(ids)
}

// ----- C->S bodies -----

/// `CSCompleteQuestContextPacket` body: quest context id, NPC object, doodad object and the
/// selected reward index, exactly the fields its `ClientPacket::read` consumes.
pub fn complete_quest_body(quest_id: u32, npc_obj: u32, doodad_obj: u32, selected: i32) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_u32(quest_id)
        .write_bc(npc_obj)
        .write_bc(doodad_obj)
        .write_i32(selected);
    s.into_bytes()
}

/// `CSQuestTalkMadePacket` body: NPC object, quest context id, component id, act id.
pub fn quest_talk_body(npc_obj: u32, quest_id: u32, component_id: u32, act_id: u32) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_bc(npc_obj)
        .write_u32(quest_id)
        .write_u32(component_id)
        .write_u32(act_id);
    s.into_bytes()
}

/// `CSDropQuestContextPacket` body.
pub fn drop_quest_body(quest_id: u32) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_u32(quest_id);
    s.into_bytes()
}

#[cfg(test)]
mod tests {
    use aaemu_commons::Marshal;
    use aaemu_game::models::char_data::{ActiveQuest, CompletedQuest};
    use aaemu_game::net::registry::ClientPacket;
    use aaemu_game::opcodes::sc;
    use aaemu_game::packets::world::{
        SCChatMessagePacket, SCCompletedQuestsPacket, SCQuestsPacket,
    };
    use aaemu_game::systems::quests::model::{acceptor, kind, status};
    use aaemu_game::systems::quests::packets::{
        CSCompleteQuestContextPacket, CSDropQuestContextPacket, CSQuestTalkMadePacket,
        SCQuestContextCompletedPacket, SCQuestContextResetPacket, SCQuestContextStartedPacket,
        SCQuestContextUpdatedPacket,
    };
    use aaemu_game::systems::skills::packets_special::SCTeleportUnitPacket;
    use aaemu_game::systems::social::chat::chat_type;

    use super::*;
    use crate::game::{observe_into, GameState, Packet, UnitInfo, LEVEL_GAME};

    fn quest(template_id: u32, quest_status: u8) -> ActiveQuest {
        ActiveQuest {
            id: 7,
            template_id,
            status: quest_status,
            objectives: [1, 0, 0, 0, 0],
            step: kind::PROGRESS,
            acceptor_type: acceptor::NPC,
            component_id: 1001,
            acceptor_id: 3597,
            time_ms: ActiveQuest::DATETIME_MIN_MS,
        }
    }

    fn info(template_id: u32) -> QuestInfo {
        QuestInfo {
            template_id,
            status: status::PROGRESS,
            objectives: [0; 5],
        }
    }

    fn packet(opcode: u16, body: Vec<u8>) -> Packet {
        Packet {
            level: LEVEL_GAME,
            opcode,
            body,
        }
    }

    #[test]
    fn sc_quests_packet_rows_carry_status_and_objectives() {
        let mut body = PacketStream::new();
        SCQuestsPacket {
            quests: vec![quest(330, status::PROGRESS), quest(331, status::READY)],
            now_ms: 0,
        }
        .write(&mut body);
        let rows = quest_rows(body.as_bytes()).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].template_id, 330);
        assert_eq!(rows[0].status, status::PROGRESS);
        assert_eq!(rows[0].objectives, [1, 0, 0, 0, 0]);
        assert_eq!(rows[1].template_id, 331);
        assert_eq!(rows[1].status, status::READY);
    }

    #[test]
    fn quest_rows_rejects_negative_and_malformed_counts() {
        let mut negative = PacketStream::new();
        negative.write_i32(-1);
        assert!(quest_rows(negative.as_bytes()).is_err());

        assert!(quest_rows(&[]).is_err());

        let mut body = PacketStream::new();
        body.write_i32(2);
        quest(330, status::PROGRESS).write(&mut body, 0);
        quest(331, status::READY).write(&mut body, 0);
        let mut bytes = body.into_bytes();
        bytes.truncate(bytes.len() - 8);
        assert!(quest_rows(&bytes).is_err());
    }

    #[test]
    fn read_quest_row_parses_incrementally_and_errors_on_truncation() {
        let mut body = PacketStream::new();
        quest(330, status::PROGRESS).write(&mut body, 0);
        quest(331, status::READY).write(&mut body, 0);
        let mut bytes = body.into_bytes();
        bytes.truncate(bytes.len() - 8);

        let mut s = PacketStream::from_bytes(bytes);
        assert_eq!(read_quest_row(&mut s).unwrap().template_id, 330);
        assert!(read_quest_row(&mut s).is_err());
    }

    #[test]
    fn sc_quests_snapshot_populates_the_active_quests() {
        let mut body = PacketStream::new();
        SCQuestsPacket {
            quests: vec![quest(330, status::PROGRESS), quest(331, status::READY)],
            now_ms: 0,
        }
        .write(&mut body);
        let mut st = GameState::default();
        observe_into(&mut st, &packet(sc::SCQuestsPacket, body.into_bytes())).unwrap();
        assert_eq!(st.quests.len(), 2);
        assert_eq!(st.quests[&330].status, status::PROGRESS);
        assert_eq!(st.quests[&330].objectives, [1, 0, 0, 0, 0]);
        assert_eq!(st.quests[&331].status, status::READY);
    }

    #[test]
    fn context_started_and_updated_upsert_and_track_failures() {
        let mut st = GameState::default();

        let mut body = PacketStream::new();
        SCQuestContextStartedPacket {
            quest: quest(330, status::INVALID),
            component_id: 1001,
            now_ms: 0,
        }
        .write(&mut body);
        observe_into(
            &mut st,
            &packet(sc::SCQuestContextStartedPacket, body.into_bytes()),
        )
        .unwrap();
        assert_eq!(st.quests[&330].status, status::INVALID);

        let mut body = PacketStream::new();
        SCQuestContextUpdatedPacket::new(quest(330, status::FAILED), 1001, 0).write(&mut body);
        observe_into(
            &mut st,
            &packet(sc::SCQuestContextUpdatedPacket, body.into_bytes()),
        )
        .unwrap();
        assert_eq!(st.quests[&330].status, status::FAILED);
        assert!(st.failed_quests.contains(&330));
    }

    #[test]
    fn completed_reset_and_failed_packets_update_the_session_state() {
        let mut st = GameState::default();
        st.quests.insert(330, info(330));
        st.quests.insert(331, info(331));
        st.quests.insert(332, info(332));

        let mut body = PacketStream::new();
        SCQuestContextCompletedPacket {
            quest_id: 330,
            body: [0; 8],
            component_id: 7,
        }
        .write(&mut body);
        observe_into(
            &mut st,
            &packet(sc::SCQuestContextCompletedPacket, body.into_bytes()),
        )
        .unwrap();
        assert!(st.completed_quests.contains(&330));
        assert!(!st.quests.contains_key(&330));

        let mut body = PacketStream::new();
        SCQuestContextResetPacket {
            quest_id: 331,
            body: [0; 8],
            component_id: 7,
        }
        .write(&mut body);
        observe_into(
            &mut st,
            &packet(sc::SCQuestContextResetPacket, body.into_bytes()),
        )
        .unwrap();
        assert_eq!(st.last_reset, Some(331));
        assert!(!st.quests.contains_key(&331));

        // `SCQuestContextFailedPacket` has no Rust definition (the opcode is declared in
        // `opcodes.rs` but neither server sends it): the hand-written body follows the
        // Completed / Reset sibling layout (`[u32 questId][u8; 8 body][u32 componentId]`).
        let mut body = PacketStream::new();
        body.write_u32(332).write_raw(&[0u8; 8]).write_u32(7);
        observe_into(
            &mut st,
            &packet(sc::SCQuestContextFailedPacket, body.into_bytes()),
        )
        .unwrap();
        assert!(st.failed_quests.contains(&332));
        assert!(!st.quests.contains_key(&332));
    }

    #[test]
    fn completed_snapshot_adds_the_quest_ids() {
        let mut body = PacketStream::new();
        SCCompletedQuestsPacket {
            quests: vec![CompletedQuest::new(330), CompletedQuest::new(331)],
        }
        .write(&mut body);
        let mut st = GameState::default();
        observe_into(
            &mut st,
            &packet(sc::SCCompletedQuestsPacket, body.into_bytes()),
        )
        .unwrap();
        assert!(st.completed_quests.contains(&330));
        assert!(st.completed_quests.contains(&331));
    }

    #[test]
    fn teleport_confirmation_moves_self_and_the_own_unit() {
        let mut st = GameState {
            self_obj: 0x100,
            ..Default::default()
        };
        st.units.insert(
            0x100,
            UnitInfo {
                obj_id: 0x100,
                name: "Me".into(),
                unit_type: 0,
                id: 7,
                pos: (1.0, 2.0, 3.0),
            },
        );
        let mut body = PacketStream::new();
        SCTeleportUnitPacket {
            reason: 0,
            error_message: 0,
            x: 10.0,
            y: 20.0,
            z: 30.0,
            z2: 0.0,
        }
        .write(&mut body);
        observe_into(
            &mut st,
            &packet(sc::SCTeleportUnitPacket, body.into_bytes()),
        )
        .unwrap();
        // The 9-byte packed position quantizes z to about 1 mm.
        let close = |a: (f32, f32, f32), b: (f32, f32, f32)| {
            (a.0 - b.0).abs() < 0.01 && (a.1 - b.1).abs() < 0.01 && (a.2 - b.2).abs() < 0.01
        };
        assert!(close(st.self_pos, (10.0, 20.0, 30.0)), "{:?}", st.self_pos);
        assert!(close(st.units[&0x100].pos, (10.0, 20.0, 30.0)));
    }

    #[test]
    fn chat_capture_keeps_only_the_last_64_messages() {
        let mut st = GameState::default();
        for i in 0..70 {
            let mut body = PacketStream::new();
            SCChatMessagePacket {
                chat_type: chat_type::SYSTEM,
                sender: None,
                message: format!("m{i}"),
                ability: 0,
                language_type: 0,
            }
            .write(&mut body);
            observe_into(&mut st, &packet(sc::SCChatMessagePacket, body.into_bytes())).unwrap();
        }
        assert_eq!(st.chat_log.len(), 64);
        assert_eq!(st.chat_log.front().unwrap(), "m6");
        assert_eq!(st.chat_log.back().unwrap(), "m69");
    }

    #[test]
    fn complete_quest_body_matches_the_server_reader() {
        let mut s = PacketStream::from_bytes(complete_quest_body(330, 0x101, 0, 2));
        assert_eq!(
            CSCompleteQuestContextPacket::read(&mut s).unwrap(),
            CSCompleteQuestContextPacket {
                quest_context_id: 330,
                npc_obj_id: 0x101,
                doodad_obj_id: 0,
                selected: 2,
            }
        );

        let mut s = PacketStream::from_bytes(complete_quest_body(331, 0, 0x202, -1));
        assert_eq!(
            CSCompleteQuestContextPacket::read(&mut s).unwrap(),
            CSCompleteQuestContextPacket {
                quest_context_id: 331,
                npc_obj_id: 0,
                doodad_obj_id: 0x202,
                selected: -1,
            }
        );
    }

    #[test]
    fn quest_talk_body_matches_the_server_reader() {
        let mut s = PacketStream::from_bytes(quest_talk_body(0x101, 330, 1001, 55));
        assert_eq!(
            CSQuestTalkMadePacket::read(&mut s).unwrap(),
            CSQuestTalkMadePacket {
                npc_obj_id: 0x101,
                quest_context_id: 330,
                quest_comp_id: 1001,
                quest_act_id: 55,
            }
        );
    }

    #[test]
    fn drop_quest_body_matches_the_server_reader() {
        let mut s = PacketStream::from_bytes(drop_quest_body(330));
        assert_eq!(
            CSDropQuestContextPacket::read(&mut s).unwrap(),
            CSDropQuestContextPacket { quest_id: 330 }
        );
    }
}
