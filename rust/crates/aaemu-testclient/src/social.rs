//! Two-bot social packets: duel, party / team, faction and loot-dice encoders and decoders.
//!
//! The decoders turn the S->C packets of those flows into [`SocialEvent`]s that the game client
//! records in arrival order (`GameState::social`), so a script command can wait for an event
//! that an earlier `drain` already read. The layouts mirror the game crate
//! (`systems/social/packets/{duel,team,faction}.rs`, `systems/economy/loot_packets.rs`).

use aaemu_commons::PacketStream;
use aaemu_game::opcodes::{cs, sc};
use anyhow::Context;

use crate::game::Packet;

/// Something a social packet told the bot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SocialEvent {
    /// `SCDuelChallengedPacket` (we are the challenged one).
    DuelChallenged { challenger_id: u32 },
    /// `SCDuelStartedPacket`.
    DuelStarted {
        challenger_obj: u32,
        challenged_obj: u32,
    },
    /// `SCDuelStartCountdownPacket`.
    DuelCountdown,
    /// `SCDuelStatePacket` (flag 0 = the duel ended).
    DuelState { unit_obj: u32, flag_obj: u32 },
    /// `SCDuelEndedPacket` (`det`: 0 lose, 1 win, 2 surrender, 3 draw; the first id is the
    /// winner unless the duel is a draw).
    DuelEnded {
        challenger_id: u32,
        challenged_id: u32,
        challenger_obj: u32,
        challenged_obj: u32,
        det: u8,
    },
    /// `SCUnitFactionChangedPacket`.
    FactionChanged {
        obj_id: u32,
        name: String,
        old: u32,
        new: u32,
        temp: bool,
    },
    /// `SCAskToJoinTeamPacket`.
    TeamAsk {
        team_id: u32,
        sender_id: u32,
        sender_name: String,
        is_party: bool,
    },
    /// `SCRejectedTeamPacket`.
    TeamRejected { name: String },
    /// `SCJoinedTeamPacket` (head only).
    TeamJoined {
        team_id: u32,
        owner_id: u32,
        is_party: bool,
    },
    /// `SCTeamMemberJoinedPacket` (head only).
    TeamMemberJoined { team_id: u32, member_id: u32 },
    /// `SCTeamMemberLeavedPacket`.
    TeamMemberLeft {
        team_id: u32,
        member_id: u32,
        kicked: bool,
    },
    /// `SCLeavedTeamPacket`.
    TeamLeft {
        team_id: u32,
        kicked: bool,
        dismissed: bool,
    },
    /// `SCTeamDismissedPacket`.
    TeamDismissed { team_id: u32 },
    /// `SCTeamLootingRuleChangedPacket`.
    LootingRule {
        team_id: u32,
        method: u8,
        min_grade: u8,
        loot_master: u32,
        roll_bop: bool,
        flags: u8,
    },
    /// `SCLootDicePacket`: a roll is asked for this item (`Item.Write` head).
    LootDice { template: u32, grade: u8 },
    /// `SCLootDiceNotifyPacket`: `name` rolled `dice` (-1 = passed).
    LootDiceNotify {
        name: String,
        template: u32,
        dice: i8,
    },
    /// `SCLootDiceSummaryPacket`.
    LootDiceSummary {
        item_index: u16,
        owner_type: u16,
        owner_obj: u32,
        rolls: Vec<(u32, i8)>,
    },
    /// `SCConflictZoneStatePacket`.
    ZoneState {
        zone_group_id: u16,
        state: u8,
        end_time: i64,
    },
}

/// Decodes a social S->C packet; `None` for any other (or a malformed) packet.
pub fn decode(p: &Packet) -> Option<SocialEvent> {
    decode_inner(p).ok().flatten()
}

fn decode_inner(p: &Packet) -> anyhow::Result<Option<SocialEvent>> {
    if p.level != crate::game::LEVEL_GAME {
        return Ok(None);
    }
    let mut s = p.stream();
    let ev = match p.opcode {
        sc::SCDuelChallengedPacket => SocialEvent::DuelChallenged {
            challenger_id: s.read_u32()?,
        },
        sc::SCDuelStartedPacket => SocialEvent::DuelStarted {
            challenger_obj: s.read_bc()?,
            challenged_obj: s.read_bc()?,
        },
        sc::SCDuelStartCountdownPacket => SocialEvent::DuelCountdown,
        sc::SCDuelStatePacket => SocialEvent::DuelState {
            unit_obj: s.read_bc()?,
            flag_obj: s.read_bc()?,
        },
        sc::SCDuelEndedPacket => SocialEvent::DuelEnded {
            challenger_id: s.read_u32()?,
            challenged_id: s.read_u32()?,
            challenger_obj: s.read_bc()?,
            challenged_obj: s.read_bc()?,
            det: s.read_u8()?,
        },
        sc::SCUnitFactionChangedPacket => SocialEvent::FactionChanged {
            obj_id: s.read_bc()?,
            name: s.read_string()?,
            old: s.read_u32()?,
            new: s.read_u32()?,
            temp: s.read_bool()?,
        },
        sc::SCAskToJoinTeamPacket => SocialEvent::TeamAsk {
            team_id: s.read_u32()?,
            sender_id: s.read_u32()?,
            sender_name: s.read_string()?,
            is_party: s.read_bool()?,
        },
        sc::SCRejectedTeamPacket => SocialEvent::TeamRejected {
            name: s.read_string()?,
        },
        sc::SCJoinedTeamPacket => SocialEvent::TeamJoined {
            team_id: s.read_u32()?,
            owner_id: s.read_u32()?,
            is_party: s.read_bool()?,
        },
        sc::SCTeamMemberJoinedPacket => SocialEvent::TeamMemberJoined {
            team_id: s.read_u32()?,
            member_id: s.read_u32()?,
        },
        sc::SCTeamMemberLeavedPacket => SocialEvent::TeamMemberLeft {
            team_id: s.read_u32()?,
            member_id: s.read_u32()?,
            kicked: s.read_bool()?,
        },
        sc::SCLeavedTeamPacket => SocialEvent::TeamLeft {
            team_id: s.read_u32()?,
            kicked: s.read_bool()?,
            dismissed: s.read_bool()?,
        },
        sc::SCTeamDismissedPacket => SocialEvent::TeamDismissed {
            team_id: s.read_u32()?,
        },
        sc::SCTeamLootingRuleChangedPacket => SocialEvent::LootingRule {
            team_id: s.read_u32()?,
            method: s.read_u8()?,
            min_grade: s.read_u8()?,
            loot_master: s.read_u32()?,
            roll_bop: s.read_bool()?,
            flags: s.read_u8()?,
        },
        sc::SCLootDicePacket => {
            let (template, grade) = item_head(&mut s)?;
            SocialEvent::LootDice { template, grade }
        }
        sc::SCLootDiceNotifyPacket => {
            let name = s.read_string()?;
            let (template, _) = item_head(&mut s)?;
            // The item body is long and variable; the dice is the last byte of the packet.
            let dice = *p.body.last().context("empty body")? as i8;
            SocialEvent::LootDiceNotify {
                name,
                template,
                dice,
            }
        }
        sc::SCLootDiceSummaryPacket => {
            let item_index = s.read_u16()?;
            let owner_type = s.read_u16()?;
            let owner_obj = s.read_bc()?;
            s.read_u8()?;
            let n = s.read_i32()?;
            let mut rolls = Vec::new();
            for _ in 0..n.clamp(0, 64) {
                rolls.push((s.read_u32()?, s.read_u8()? as i8));
            }
            SocialEvent::LootDiceSummary {
                item_index,
                owner_type,
                owner_obj,
                rolls,
            }
        }
        sc::SCConflictZoneStatePacket => SocialEvent::ZoneState {
            zone_group_id: s.read_u16()?,
            state: s.read_u8()?,
            end_time: s.read_i64()?,
        },
        _ => return Ok(None),
    };
    Ok(Some(ev))
}

/// `Item.Write` head: template id, then (unless a body part) id and grade.
fn item_head(s: &mut PacketStream) -> anyhow::Result<(u32, u8)> {
    let template = s.read_u32()?;
    if template == 0 {
        return Ok((0, 0));
    }
    s.read_u64()?;
    Ok((template, s.read_u8()?))
}

// ---- C->S bodies ----

/// `CSChallengeDuelPacket`.
pub fn challenge_duel(challenged_id: u32) -> (u16, Vec<u8>) {
    let mut b = PacketStream::new();
    b.write_u32(challenged_id);
    (cs::CSChallengeDuelPacket, b.into_bytes())
}

/// `CSStartDuelPacket`: `error_message` 0 accepts, 507 (`TargetRejectedDuel`) refuses or
/// cancels.
pub fn start_duel(challenger_id: u32, error_message: i16) -> (u16, Vec<u8>) {
    let mut b = PacketStream::new();
    b.write_u32(challenger_id).write_i16(error_message);
    (cs::CSStartDuelPacket, b.into_bytes())
}

/// `CSInviteToTeamPacket` (`team_id` 0 = a new party).
pub fn invite_to_team(team_id: u32, is_party: bool, target: &str) -> (u16, Vec<u8>) {
    let mut b = PacketStream::new();
    b.write_u32(team_id).write_bool(is_party).write_str(target);
    (cs::CSInviteToTeamPacket, b.into_bytes())
}

/// `CSReplyToJoinTeamPacket`.
pub fn reply_to_join_team(
    team_id: u32,
    is_party: bool,
    owner_id: u32,
    reject: bool,
    char_name: &str,
) -> (u16, Vec<u8>) {
    let mut b = PacketStream::new();
    b.write_u32(team_id)
        .write_bool(is_party)
        .write_u32(owner_id)
        .write_bool(reject)
        .write_str(char_name)
        .write_bool(false);
    (cs::CSReplyToJoinTeamPacket, b.into_bytes())
}

/// `CSAskRiskyTeamActionPacket` (`action`: 0 leave, 1 kick, 2 dismiss).
pub fn risky_team_action(team_id: u32, target_id: u32, action: u8) -> (u16, Vec<u8>) {
    let mut b = PacketStream::new();
    b.write_u32(team_id).write_u32(target_id).write_u8(action);
    (cs::CSAskRiskyTeamActionPacket, b.into_bytes())
}

/// `CSChangeLootingRulePacket` (`flags`: 1 method, 2 grade, 4 master, 8 roll for BoP).
pub fn change_looting_rule(
    team_id: u32,
    method: u8,
    min_grade: u8,
    loot_master: u32,
    roll_bop: bool,
    flags: u8,
) -> (u16, Vec<u8>) {
    let mut b = PacketStream::new();
    b.write_u32(team_id)
        .write_u8(method)
        .write_u8(min_grade)
        .write_u32(loot_master)
        .write_bool(roll_bop)
        .write_u8(flags);
    (cs::CSChangeLootingRulePacket, b.into_bytes())
}

/// `CSLootDicePacket`: `roll` true rolls, false passes.
pub fn loot_dice(item_index: u16, owner_type: u16, owner_obj: u32, roll: bool) -> (u16, Vec<u8>) {
    let mut b = PacketStream::new();
    b.write_u16(item_index)
        .write_u16(owner_type)
        .write_bc(owner_obj)
        .write_u8(0)
        .write_bool(roll);
    (cs::CSLootDicePacket, b.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::LEVEL_GAME;

    fn pkt(opcode: u16, body: Vec<u8>) -> Packet {
        Packet {
            level: LEVEL_GAME,
            opcode,
            body,
        }
    }

    #[test]
    fn encoders_are_byte_exact() {
        assert_eq!(
            challenge_duel(0x01020304),
            (cs::CSChallengeDuelPacket, vec![4, 3, 2, 1])
        );
        assert_eq!(
            start_duel(9, 507),
            (cs::CSStartDuelPacket, vec![9, 0, 0, 0, 0xFB, 0x01])
        );
        assert_eq!(start_duel(9, 0).1, vec![9, 0, 0, 0, 0, 0]);
        let mut e = PacketStream::new();
        e.write_u32(0).write_bool(true).write_str("Bo");
        assert_eq!(
            invite_to_team(0, true, "Bo"),
            (cs::CSInviteToTeamPacket, e.into_bytes())
        );
        assert_eq!(
            reply_to_join_team(3, true, 7, false, "Al"),
            (
                cs::CSReplyToJoinTeamPacket,
                vec![3, 0, 0, 0, 1, 7, 0, 0, 0, 0, 2, 0, b'A', b'l', 0]
            )
        );
        assert_eq!(
            risky_team_action(5, 6, 0),
            (
                cs::CSAskRiskyTeamActionPacket,
                vec![5, 0, 0, 0, 6, 0, 0, 0, 0]
            )
        );
        assert_eq!(
            change_looting_rule(5, 0, 1, 0, false, 3),
            (
                cs::CSChangeLootingRulePacket,
                vec![5, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 3]
            )
        );
        assert_eq!(
            loot_dice(2, 1, 0x0A0B0C, true),
            (
                cs::CSLootDicePacket,
                vec![2, 0, 1, 0, 0x0C, 0x0B, 0x0A, 0, 1]
            )
        );
    }

    #[test]
    fn encoders_round_trip_through_the_server_readers() {
        use aaemu_game::net::registry::ClientPacket;
        use aaemu_game::systems::social::packets::duel::{
            CSChallengeDuelPacket, CSStartDuelPacket,
        };
        use aaemu_game::systems::social::packets::team::{
            CSAskRiskyTeamActionPacket, CSChangeLootingRulePacket, CSInviteToTeamPacket,
            CSReplyToJoinTeamPacket,
        };
        let mut s = PacketStream::from_bytes(challenge_duel(77).1);
        assert_eq!(
            CSChallengeDuelPacket::read(&mut s).unwrap().challenged_id,
            77
        );
        let mut s = PacketStream::from_bytes(start_duel(8, 507).1);
        let d = CSStartDuelPacket::read(&mut s).unwrap();
        assert_eq!((d.challenger_id, d.error_message), (8, 507));
        let mut s = PacketStream::from_bytes(invite_to_team(0, true, "Bo").1);
        let i = CSInviteToTeamPacket::read(&mut s).unwrap();
        assert_eq!(
            (i.team_id, i.is_party, i.target_name.as_str()),
            (0, true, "Bo")
        );
        let mut s = PacketStream::from_bytes(reply_to_join_team(3, true, 7, true, "Al").1);
        let r = CSReplyToJoinTeamPacket::read(&mut s).unwrap();
        assert_eq!(
            (r.team_id, r.owner_id, r.is_reject, r.char_name.as_str()),
            (3, 7, true, "Al")
        );
        let mut s = PacketStream::from_bytes(risky_team_action(5, 6, 2).1);
        let a = CSAskRiskyTeamActionPacket::read(&mut s).unwrap();
        assert_eq!((a.team_id, a.target_id, a.risky_action), (5, 6, 2));
        let mut s = PacketStream::from_bytes(change_looting_rule(5, 0, 1, 9, true, 0x0F).1);
        let l = CSChangeLootingRulePacket::read(&mut s).unwrap();
        assert_eq!(
            (
                l.team_id,
                l.looting_rule.loot_method,
                l.looting_rule.minimum_grade,
                l.change_flags
            ),
            (5, 0, 1, 0x0F)
        );
    }

    #[test]
    fn duel_and_faction_decoders_read_the_server_layouts() {
        assert_eq!(
            decode(&pkt(sc::SCDuelChallengedPacket, vec![4, 3, 2, 1])),
            Some(SocialEvent::DuelChallenged {
                challenger_id: 0x01020304
            })
        );
        assert_eq!(
            decode(&pkt(
                sc::SCDuelEndedPacket,
                vec![1, 0, 0, 0, 2, 0, 0, 0, 1, 2, 3, 0x10, 0, 0, 3]
            )),
            Some(SocialEvent::DuelEnded {
                challenger_id: 1,
                challenged_id: 2,
                challenger_obj: 0x030201,
                challenged_obj: 0x10,
                det: 3
            })
        );
        assert_eq!(
            decode(&pkt(
                sc::SCDuelStartedPacket,
                vec![0x11, 0, 0, 0x22, 0x22, 0x22]
            )),
            Some(SocialEvent::DuelStarted {
                challenger_obj: 0x11,
                challenged_obj: 0x222222
            })
        );
        assert_eq!(
            decode(&pkt(sc::SCDuelStatePacket, vec![0x11, 0, 0, 0, 0, 0])),
            Some(SocialEvent::DuelState {
                unit_obj: 0x11,
                flag_obj: 0
            })
        );
        assert_eq!(
            decode(&pkt(
                sc::SCUnitFactionChangedPacket,
                vec![0x10, 0, 0, 1, 0, b'A', 101, 0, 0, 0, 161, 0, 0, 0, 0]
            )),
            Some(SocialEvent::FactionChanged {
                obj_id: 0x10,
                name: "A".into(),
                old: 101,
                new: 161,
                temp: false
            })
        );
        assert_eq!(decode(&pkt(sc::SCUnitStatePacket, vec![])), None);
    }

    #[test]
    fn team_and_loot_decoders() {
        let mut b = PacketStream::new();
        b.write_u32(3).write_u32(7).write_str("Al").write_bool(true);
        assert_eq!(
            decode(&pkt(sc::SCAskToJoinTeamPacket, b.into_bytes())),
            Some(SocialEvent::TeamAsk {
                team_id: 3,
                sender_id: 7,
                sender_name: "Al".into(),
                is_party: true
            })
        );
        let mut b = PacketStream::new();
        b.write_u32(3).write_bool(true).write_bool(false);
        assert_eq!(
            decode(&pkt(sc::SCLeavedTeamPacket, b.into_bytes())),
            Some(SocialEvent::TeamLeft {
                team_id: 3,
                kicked: true,
                dismissed: false
            })
        );
        let mut b = PacketStream::new();
        b.write_u32(3)
            .write_u8(0)
            .write_u8(1)
            .write_u32(0)
            .write_bool(true)
            .write_u8(1);
        assert_eq!(
            decode(&pkt(sc::SCTeamLootingRuleChangedPacket, b.into_bytes())),
            Some(SocialEvent::LootingRule {
                team_id: 3,
                method: 0,
                min_grade: 1,
                loot_master: 0,
                roll_bop: true,
                flags: 1
            })
        );
        let mut b = PacketStream::new();
        b.write_u16(2)
            .write_u16(1)
            .write_bc(0x55)
            .write_u8(0)
            .write_i32(2)
            .write_u32(10)
            .write_u8(42)
            .write_u32(11)
            .write_u8(0xFF);
        assert_eq!(
            decode(&pkt(sc::SCLootDiceSummaryPacket, b.into_bytes())),
            Some(SocialEvent::LootDiceSummary {
                item_index: 2,
                owner_type: 1,
                owner_obj: 0x55,
                rolls: vec![(10, 42), (11, -1)]
            })
        );
        let mut b = PacketStream::new();
        b.write_u32(8001).write_u64(99).write_u8(3).write_u8(0);
        assert_eq!(
            decode(&pkt(sc::SCLootDicePacket, b.into_bytes())),
            Some(SocialEvent::LootDice {
                template: 8001,
                grade: 3
            })
        );
        let mut b = PacketStream::new();
        b.write_str("Bo")
            .write_u32(8001)
            .write_u64(99)
            .write_u8(3)
            .write_u8(0)
            .write_u8(0xFF);
        assert_eq!(
            decode(&pkt(sc::SCLootDiceNotifyPacket, b.into_bytes())),
            Some(SocialEvent::LootDiceNotify {
                name: "Bo".into(),
                template: 8001,
                dice: -1
            })
        );
        let mut b = PacketStream::new();
        b.write_u16(9).write_u8(2).write_i64(1234);
        assert_eq!(
            decode(&pkt(sc::SCConflictZoneStatePacket, b.into_bytes())),
            Some(SocialEvent::ZoneState {
                zone_group_id: 9,
                state: 2,
                end_time: 1234
            })
        );
    }
}
