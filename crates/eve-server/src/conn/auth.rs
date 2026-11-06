//! The Trinity secure handshake and account login.
//!
//! The protocol itself, and why each step is shaped as it is, is in
//! `eve_proto::handshake`. This drives it over the connection, as
//! `__WatchThatSwitch2` and `HandleClientAuthentication`
//! (`common/script/net/machoNet.py`, `GPS.py`) do on CCP's proxy: a status
//! poll leaves the socket open, and the proxy starts over three seconds later,
//! because the client keeps that transport and logs in over it.

use std::time::Duration;

use eve_marshal::Value;
use eve_proto::handshake::{self, ClientHandshake, Opening};
use eve_proto::{decode_payload, encode_frame, llv, VersionChallenge};
use tracing::{debug, error, info, warn};

use super::{Conn, ConnError, Result};
use crate::db;
use crate::session::{ROLE_LOGIN, ROLE_PLAYER};
use crate::store::{Store, StoreError, Value as StoreValue};
use crate::svc::language::{canonical_language, resolve_language};
use crate::services::Call;

/// How the handshake went: a login, or a status poll to answer again.
enum Round {
    LoggedIn,
    StatusOnly,
}

impl Conn {
    pub(super) async fn handshake(&mut self) -> Result {
        let mut first = true;
        loop {
            if !first {
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            first = false;
            match self.handshake_round().await? {
                Round::LoggedIn => return Ok(()),
                Round::StatusOnly => debug!("status poll answered; waiting for the login"),
            }
        }
    }

    async fn handshake_round(&mut self) -> Result<Round> {
        // 1. Version exchange. The server's carries how many are playing, which
        //    the login screen shows (machoNet.GetServerStatus).
        let playing = self.server.world.lock().online_count();
        self.write_plain(&VersionChallenge::server(playing).to_value()).await?;

        let frame = self.expect_frame("low-level version reply").await?;
        if frame.len() > llv::MAX_CLIENT_LLV_BYTES {
            return self.close_round("A low-level-version challenge response may not exceed 1000 bytes", None).await;
        }
        let value = match decode_payload(&frame, &self.server.table) {
            Ok(v) => v,
            Err(eve_proto::ProtoError::PeerClosed { reason, .. }) => {
                info!("client closed the connection during the handshake: {reason}");
                return Err(ConnError::Closed(format!("client closed: {reason}")));
            }
            Err(e) => return Err(e.into()),
        };
        let peer = match VersionChallenge::from_value(&value) {
            Ok(p) => p,
            Err(e) => {
                let shown = format!("{value:?}");
                error!("version reply not understood: {}", &shown[..shown.len().min(400)]);
                self.dump_frame(&frame, &e);
                return Err(e.into());
            }
        };
        if let Some((code, detail)) = peer.mismatch() {
            warn!("client version rejected: {detail}");
            return self.close_round(&format!("Handshake Failed - {detail}"), Some(code)).await;
        }
        debug!("version exchange ok: build {} {:?}", peer.boot_build, String::from_utf8_lossy(&peer.codename));

        // 2. VIP key, queue check, or the crypto context.
        let mut packet = self.read_plain().await?;
        if matches!(handshake::opening(&packet)?, Opening::VipKey) {
            packet = self.read_plain().await?;
        }
        let key_version = match handshake::opening(&packet)? {
            Opening::QueueCheck => {
                self.write_plain(&Value::Int(1)).await?;
                return Ok(Round::StatusOnly);
            }
            Opening::VipKey => return Err(ConnError::Protocol("two VIP keys".into())),
            Opening::CryptoContext { key_version } => key_version,
        };
        if key_version != handshake::PLACEBO_KEY_VERSION {
            warn!("client offered crypto key {:?}; it must run with cryptoPack=Placebo", String::from_utf8_lossy(&key_version));
            return self
                .close_round(
                    "Handshake Failed - this server only speaks cryptoPack=Placebo; patch blue.dll and set it in common.ini",
                    Some("HANDSHAKE_INCOMPATIBLEPUBLICKEY"),
                )
                .await;
        }

        // 3. The secure client handshake, then the password version, then (for
        //    version 1) the same handshake again with the password in clear.
        let mut hello = self.read_client_handshake().await?;
        self.write_sealed(&Value::Int(handshake::PASSWORD_VERSION_PLAIN)).await?;
        let user_name = hello.text("user_name").unwrap_or_default();
        if !user_name.is_empty() && !hello.has_password() {
            hello = self.read_client_handshake().await?;
        }
        if let Some(code) = hello.mismatch() {
            warn!("client handshake rejected: {code}");
            return self.close_round("Handshake Failed - version mismatch", Some(code)).await;
        }
        if hello.challenge.len() != handshake::HANDSHAKE_PADDING {
            return self.close_round("Handshake Failed - incorrect padding", Some("HANDSHAKE_INCORRECTPADDING")).await;
        }

        // 4. Our challenge, the function the client evaluates, and the proof
        //    that we hashed its challenge.
        let (challenge, packet) =
            handshake::server_challenge(&hello.challenge, playing, self.server.node_id, &self.server.table)?;
        self.write_sealed(&packet).await?;
        // No user name: a status query, and the client stops here.
        if user_name.is_empty() {
            return Ok(Round::StatusOnly);
        }
        // A user name means the client goes on to answer the challenge, so a
        // password that cannot be read must fail the login, not be taken for
        // a poll (which would leave the two sides a step apart).
        let password = hello.text("user_password").unwrap_or_default();
        if password.is_empty() {
            let shown = format!("{:?}", hello.get("user_password"));
            error!("login for {user_name:?} has no readable password: {}", &shown[..shown.len().min(200)]);
            let _ = self.read_sealed().await;
            return self.close_round("LoginAuthFailed", Some("LoginAuthFailed")).await;
        }

        // 5. The client's answer to our challenge.
        let response = self.read_sealed().await?;
        if !handshake::response_matches(&response, &challenge, &self.server.table)? {
            error!("the client's challenge response does not match; the marshal encodings disagree");
            return self.close_round("Handshake Failed - hash mismatch", Some("HANDSHAKE_FAILEDHASHMISMATCH")).await;
        }

        // 6. The account.
        let account = match db::authenticate(&self.server.store, &user_name, &password) {
            Ok(Some(a)) => a,
            Ok(None) => {
                info!("login rejected for {user_name:?}");
                return self.close_round("LoginAuthFailed", Some("LoginAuthFailed")).await;
            }
            Err(e) => {
                error!("account lookup for {user_name:?} failed: {}", crate::logging::chain(&e));
                self.close_round("AutClusterStarting", Some("AutClusterStarting")).await?;
                return Err(e.into());
            }
        };

        self.session.user_id = Some(account.account_id);
        self.session.role = ROLE_LOGIN | ROLE_PLAYER | account.role;
        self.session.address = Some(self.peer.to_string());
        self.session.language_id = Some(login_language(
            hello.text("user_languageid").as_deref(), account.language_id.as_deref()).into());
        self.session.char_id = None;
        self.authenticated = true;
        self.span.record("user", account.account_id);

        // 7. The ack. The client keeps only session_init['userid'] from it;
        //    its session is set by the change held back here until it is
        //    ready for one (flush_pending_session_change).
        self.pending_session_change = Some(self.session_change(self.session.change_body()));
        self.write_sealed(&handshake::auth_ack(self.client_id, self.session_init())).await?;

        info!(role = self.session.role, sid = self.session.sid, "logged in as {:?}", account.name);
        Ok(Round::LoggedIn)
    }

    /// gameui.SetLanguage/systemmenu change the preference and then reboot.
    /// Commit the account setting before publishing the changed session.
    pub(super) async fn set_language_id(&mut self, request: &eve_proto::MachoPacket, call: &Call) -> Result {
        if !self.authenticated || call.args.len() != 1 || !call.kwargs.is_empty() {
            return self.send_error(request, crate::exceptions::custom_error("A language change requires an authenticated account and one language ID.")).await;
        }
        let Some(language) = call.arg_text(0) else {
            return self.send_error(request, crate::exceptions::custom_error("The language ID must be text.")).await;
        };
        let before = self.session.clone();
        if let Err(error) = persist_language(&self.server.store, &mut self.session, &language).await {
            return self.send_error(request, crate::exceptions::custom_error(&error)).await;
        }
        if self.pending_session_change.is_some() {
            // A first post-login call must release the up-to-date initial
            // session, not the language captured before this RPC committed.
            self.pending_session_change = Some(self.session_change(self.session.change_body()));
        } else if before.language_id != self.session.language_id {
            self.send_session_change_from(&before).await?;
        }
        self.reply(request, &Value::None).await?;
        self.flush_pending_session_change(call).await
    }

    /// The attributes the proxy sets on the new session.
    fn session_init(&self) -> Value {
        let s = &self.session;
        Value::Dict(vec![
            (Value::bytes("userid"), Value::int_or_none(s.user_id)),
            (Value::bytes("userType"), Value::int_or_none(s.user_type)),
            (Value::bytes("role"), Value::Long(s.role)),
            (Value::bytes("address"), s.address.as_deref().map_or(Value::None, Value::text)),
            (Value::bytes("languageID"), s.language_id.as_deref().map_or(Value::None, Value::text)),
        ])
    }

    // ---- handshake wire ----------------------------------------------------
    //
    // Nothing in the handshake is compressed: the client's UnEncryptedRead
    // and UnEncryptedWrite take frames as they are.

    async fn expect_frame(&mut self, what: &str) -> Result<Vec<u8>> {
        self.read_frame().await?.ok_or_else(|| ConnError::Protocol(format!("connection ended before the {what}")))
    }

    async fn read_plain(&mut self) -> Result<Value> {
        let frame = self.expect_frame("handshake").await?;
        Ok(decode_payload(&frame, &self.server.table)?)
    }

    /// The secure client handshake. A malformed one is logged as decoded and
    /// its frame saved, since only a real client produces one.
    async fn read_client_handshake(&mut self) -> Result<ClientHandshake> {
        let frame = self.expect_frame("secure client handshake").await?;
        let parsed = handshake::open(&frame, &self.server.table).and_then(|v| {
            ClientHandshake::from_value(&v).map_err(|e| {
                let shown = format!("{v:?}");
                error!("secure client handshake not understood: {}", &shown[..shown.len().min(400)]);
                e
            })
        });
        match parsed {
            Ok(h) => Ok(h),
            Err(e) => {
                self.dump_frame(&frame, &e);
                Err(e.into())
            }
        }
    }

    async fn read_sealed(&mut self) -> Result<Value> {
        let frame = self.expect_frame("secure handshake").await?;
        Ok(handshake::open(&frame, &self.server.table)?)
    }

    async fn write_plain(&mut self, v: &Value) -> Result {
        let raw = eve_marshal::save(v, &self.server.table)?;
        self.write_frame_raw(&raw).await
    }

    async fn write_sealed(&mut self, v: &Value) -> Result {
        let raw = handshake::seal(v, &self.server.table)?;
        self.write_frame_raw(&raw).await
    }

    async fn write_frame_raw(&mut self, raw: &[u8]) -> Result {
        use tokio::io::AsyncWriteExt;
        let frame = encode_frame(raw)?;
        self.stream.write_all(&frame).await?;
        Ok(())
    }

    /// Close during the handshake with a reason code the client may localise.
    async fn close_round<T>(&mut self, reason: &str, code: Option<&str>) -> Result<T> {
        match self.close_coded(reason, code).await {
            Err(e) => Err(e),
            Ok(()) => Err(ConnError::Closed(reason.to_string())),
        }
    }
}

fn login_language(requested: Option<&str>, saved: Option<&str>) -> &'static str {
    // GPS.Authenticate sends the client's loaded prefs.languageID. Honor a valid
    // explicit value; use the durable account preference if it is absent or
    // unavailable, then the client's implicit English fallback.
    requested.and_then(canonical_language)
        .unwrap_or_else(|| resolve_language(saved.unwrap_or("EN")))
}

async fn persist_language(store: &Store, session: &mut crate::session::Session, language: &str) -> std::result::Result<(), String> {
    let Some(language) = canonical_language(language) else {
        return Err("That language is not available.".into());
    };
    let Some(account) = session.user_id.filter(|id| i32::try_from(*id).is_ok_and(|id| id > 0)) else {
        return Err("A language change requires an authenticated account.".into());
    };
    let saved = store.write(|tx| {
        let Some(row) = tx.table("account").get_id(account).map(|r| r.id) else { return Ok(false) };
        tx.update("account", row, &[("languageID", StoreValue::Text(language.into()))])?;
        Ok::<_, StoreError>(true)
    });
    match saved {
        Ok(true) => { session.language_id = Some(language.into()); Ok(()) }
        Ok(false) => Err("The account could not be found.".into()),
        Err(error) => {
            error!("saving account language: {}", crate::logging::chain(&error));
            Err("The language preference could not be saved. Please try again.".into())
        }
    }
}
