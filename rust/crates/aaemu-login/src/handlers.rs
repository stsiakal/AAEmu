//! Packet handlers (`AAEmu.Login.Core.PacketHandlers.C2L` / `.G2L`).

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;

use crate::auth::{
    AuthFlow, KoreaAuthFlow, KoreaStep, LoginClient, PasswordAuthFlow, ReconnectAuthFlow,
};
use crate::models::GsRegisterResult;
use crate::network::internal::InternalConnection;
use crate::network::login::LoginConnection;
use crate::packets::c2l::C2lPacket;
use crate::packets::g2l::G2lPacket;
use crate::packets::l2g::LgRegisterGameServer;
use crate::server::LoginServer;
use crate::services::password::Password;

/// Dispatches a client packet.
pub async fn handle_c2l(
    server: &Arc<LoginServer>,
    conn: &Arc<LoginConnection>,
    packet: C2lPacket,
) -> anyhow::Result<()> {
    match packet {
        C2lPacket::CancelEnterWorld(_) => {
            conn.cancel_enter_world();
            Ok(())
        }
        C2lPacket::ChallengeResponse2(p) => {
            conn.continue_auth(server, KoreaStep::ChallengeV2 { ch: p.ch })
                .await
        }
        C2lPacket::ChallengeResponse(p) => {
            conn.continue_auth(server, KoreaStep::ChallengeV1 { ch: p.ch, pw: p.pw })
                .await
        }
        C2lPacket::EnterWorld(p) => conn.initiate_enter_world(server, p.gs_id).await,
        C2lPacket::ListWorld(_) => {
            let list = server.games.get_world_list(conn).await?;
            let worlds = list.game_servers.iter().map(|g| g.world_info()).collect();
            conn.send_world_list(worlds, list.characters)?;
            Ok(())
        }
        C2lPacket::OtpNumber(p) => {
            conn.continue_auth(server, KoreaStep::Otp(p.otp_number))
                .await
        }
        C2lPacket::PcCertNumber(p) => {
            conn.continue_auth(server, KoreaStep::Cert(p.cert_number))
                .await
        }
        C2lPacket::RequestAuthGameOn(_) => Ok(()),
        C2lPacket::RequestAuthMailRu(p) => {
            // Convert.ToHexString: upper-case hex of the raw token.
            let token_hex = hex::encode_upper(&p.token);
            let flow = PasswordAuthFlow {
                username: Some(p.id),
                password: Some(Password::from_sha256_hex(token_hex)),
                client_ip: conn.ip(),
            };
            conn.authenticate(server, AuthFlow::Password(flow)).await
        }
        C2lPacket::RequestAuth(p) => {
            let flow = KoreaAuthFlow::new(p.account, conn.ip());
            conn.authenticate(server, AuthFlow::Korea(Box::new(flow)))
                .await
        }
        // CARequestAuthTencentPacketHandler is a no-op.
        C2lPacket::RequestAuthTencent(_) => Ok(()),
        C2lPacket::RequestAuthTrion(p) => {
            let flow = PasswordAuthFlow {
                username: p.username,
                password: p.password.map(Password::from_sha256_hex),
                client_ip: conn.ip(),
            };
            conn.authenticate(server, AuthFlow::Password(flow)).await
        }
        C2lPacket::RequestReconnect(p) => {
            let flow = ReconnectAuthFlow {
                gs_id: p.gs_id,
                account_id: p.account_id,
                token: p.cookie,
            };
            conn.authenticate(server, AuthFlow::Reconnect(flow)).await
        }
    }
}

/// Constant-time comparison of the shared secret.
fn secret_matches(given: &str, expected: &str) -> bool {
    given.len() == expected.len()
        && given
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |d, (a, b)| d | (a ^ b))
            == 0
}

/// Dispatches a game server packet.
pub async fn handle_g2l(
    server: &Arc<LoginServer>,
    conn: &Arc<InternalConnection>,
    packet: G2lPacket,
) -> anyhow::Result<()> {
    // Deviation: the C# handled every packet from any peer. Only `RegisterGameServer` (which
    // carries the shared secret) is honoured before the peer is registered; anything else
    // from an unauthenticated peer drops the connection.
    if conn.game_server().is_none() && !matches!(packet, G2lPacket::RegisterGameServer(_)) {
        tracing::warn!(
            "Connection {}: packet before registration from an unauthenticated peer, closing",
            conn.ip
        );
        conn.close();
        return Ok(());
    }
    match packet {
        G2lPacket::GameServerLoad(p) => {
            // C#: `connection.GameServer!.Load` (NullReferenceException if not registered).
            let gs_id = conn
                .game_server()
                .context("Object reference not set to an instance of an object (GameServer)")?;
            server.games.set_load(gs_id, p.load)
        }
        G2lPacket::PlayerEnter(p) => {
            if !conn.may_act_for(&server.games, p.gs_id) {
                tracing::warn!(
                    "Connection {}: enter-world result for game server {} not sent by it, ignored",
                    conn.ip,
                    p.gs_id
                );
                return Ok(());
            }
            server
                .games
                .route_enter_world_response(p.connection_id, p.gs_id, p.result);
            Ok(())
        }
        G2lPacket::PlayerReconnect(p) => {
            // Deviation: the C# accepted tokens from any internal peer, so an unregistered
            // connection could mint reconnect logins for any account. Only the registered game
            // server (or one of its mirrors) may add tokens for its id.
            if !conn.may_act_for(&server.games, p.gs_id) {
                tracing::warn!(
                    "Connection {}: reconnect token for game server {} not sent by it, ignored",
                    conn.ip,
                    p.gs_id
                );
                return Ok(());
            }
            server
                .logins
                .add_reconnection_token(conn, p.gs_id, p.account_id, p.token);
            Ok(())
        }
        G2lPacket::RegisterGameServer(p) => {
            if conn.game_server().is_some() {
                tracing::warn!("Connection {}: already registered, ignoring", conn.ip);
                return Ok(());
            }
            if !secret_matches(&p.secret_key, &server.config.secret_key) {
                tracing::error!("Connection {}, bad secret key", conn.ip);
                // Deviation: the C# kept the connection open after the error reply.
                let conn = conn.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    conn.send_packet(&LgRegisterGameServer {
                        result: GsRegisterResult::Error,
                    });
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    conn.close();
                });
                return Ok(());
            }
            server.games.add(p.gs_id, p.mirrors, conn);
            Ok(())
        }
        G2lPacket::RequestInfo(p) => {
            let login_connection = server.connections.get(p.connection_id);
            if !p.characters.is_empty() {
                // C#: `loginConnection!` / `connection.GameServer!` throw before ReleaseId.
                let login_connection = login_connection.context(
                    "Object reference not set to an instance of an object (login connection)",
                )?;
                let gs_id = conn
                    .game_server()
                    .context("Object reference not set to an instance of an object (GameServer)")?;
                login_connection.add_characters(gs_id, p.characters)?;
            }
            server.requests.release_id(p.request_id);
            Ok(())
        }
    }
}
