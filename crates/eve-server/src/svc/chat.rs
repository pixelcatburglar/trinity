use eve_marshal::Value;
use tracing::{error, warn};

use crate::services::{Call, Ctx, Outcome};
use crate::session::{Session, ROLE_LOGIN, ROLE_PLAYER};
use crate::store::{self, Db, Row, StoreError, Tx};
use crate::svc::rows::{row, rowset};
use crate::world::{local_system, LocalBot, WorldState};

const OWNER_SYSTEM: i64 = 1;

const CHTMODE_LISTENER: i64 = 1;
const CHTMODE_CONVERSATIONALIST: i64 = 3;
const CHTMODE_CREATOR: i64 = 15;
const CHTMODE_NOTSPECIFIED: i64 = -1;

/// Whether `char_id` may run `channel`: its owner, or listed as an operator
/// (CHTMODE_OPERATOR = 7).
fn operates(db: &Db, channel: &Row, char_id: i64) -> bool {
    channel.int("ownerID") == Some(char_id)
        || db
            .table("channelMods")
            .find(&["channelID"], &[channel.value("channelID").clone()])
            .any(|m| m.int("accessor") == Some(char_id) && m.int("mode").is_some_and(|mode| mode >= 7))
}

const CSPA_MAILBOX: i64 = 127;

const CHANNEL_HEADER: &[&str] = &[
    "channelID", "ownerID", "displayName", "motd", "comparisonKey", "memberless",
    "password", "mailingList", "cspa", "temporary", "mode", "subscribed",
    "estimatedMemberCount", "languageRestriction",
];

const IX_PASSWORD: usize = 6;
const IX_SUBSCRIBED: usize = 11;

const ACL_HEADER: &[&str] =
    &["accessor", "mode", "untilWhen", "reason", "admin", "originalMode"];

const MEMBER_HEADER: &[&str] = &["charID", "corpID", "allianceID", "role", "extra"];

/// The `languageRestriction` column this once added is part of the store's
/// schema; there is nothing left to fix up.
pub fn ensure(_tx: &mut Tx<'_>) -> Result<(), StoreError> {
    Ok(())
}

async fn set_language_restriction(ctx: &Ctx<'_>, channel: i64, enabled: i64) -> Outcome {
    // The Trinity channel dialog exposes this on its game-master tab only.
    if ctx.session.role & (8 | 16) == 0 { return Outcome::custom("Only a game master can change the language restriction."); }
    if !matches!(enabled, 0 | 1) { return Outcome::custom("Invalid language restriction."); }
    let updated = ctx.store.write(|tx| {
        let Some(id) = tx.table("channels").get_id(channel).map(|r| r.id) else { return Ok(false) };
        tx.update("channels", id, &[("languageRestriction", store::Value::Int(enabled))])?;
        Ok::<_, StoreError>(true)
    });
    match updated {
        Ok(false) => Outcome::custom("That channel does not exist."),
        Ok(true) => {
            let world = ctx.world.lock();
            let id = Value::Int(channel);
            announce(&world, &world.channel_members(&id), &id, "SetChannelLanguageRestriction", &who(ctx.session), vec![Value::Int(enabled)]);
            Outcome::Value(Value::None)
        }
        Err(e) => { error!("channel language restriction: {e}"); Outcome::custom("Could not save the language restriction.") }
    }
}

pub async fn lsc(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        "JoinChannels" => join_channels(ctx, call.args.first()).await,
        "GetChannels" => get_channels(ctx).await,
        "GetMembers" => get_members(ctx, call.args.first()).await,
        "SetChannelLanguageRestriction" => return Some(set_language_restriction(ctx, call.arg_id(0), call.arg_id(1)).await),
        "VoiceStatus" => {
            let Some(channel) = call.args.first() else { return Some(Outcome::custom("Missing channel.")); };
            let status = call.arg_id(1);
            if !(0..=2).contains(&status) { return Some(Outcome::custom("Invalid voice status.")); }
            let world = ctx.world.lock();
            let members = world.channel_members(channel);
            if !members.contains(&ctx.client_id) { return Some(Outcome::custom("Join the channel first.")); }
            announce(&world, &members, channel, "VoiceStatus", &who(ctx.session), vec![Value::Int(status)]);
            Value::None
        }
        // LeaveChannels(channelIDs, unsubscribe) / LeaveChannel(channelID, unsubscribe)
        "LeaveChannels" => {
            for channel in call.args.first().and_then(|v| v.seq()).unwrap_or(&[]) {
                leave(ctx, channel);
            }
            Value::None
        }
        "LeaveChannel" => {
            if let Some(channel) = call.args.first() {
                leave(ctx, channel);
            }
            Value::None
        }
        // SendMessage(channelID, message): everyone with the channel open hears it,
        // the speaker included (lscengine only shows a line when OnLSC brings it).
        "SendMessage" => {
            if let (Some(channel), Some(message)) = (call.args.first(), call.args.get(1)) {
                let world = ctx.world.lock();
                let members = world.channel_members(channel);
                if members.contains(&ctx.client_id) {
                    announce(&world, &members, channel, "SendMessage", &who(ctx.session), vec![message.clone()]);
                } else {
                    warn!("SendMessage to a channel the speaker has not joined");
                }
            }
            Value::None
        }
        "GetMyMessages" => get_my_messages(ctx).await,
        "MarkMessagesRead" => mark_messages_read(ctx, call.args.first()).await,
        "DeleteMessages" => delete_messages(ctx, call.arg_id(0), call.args.get(1)).await,
        "CreateChannel" => create_channel(ctx, call).await,
        "Configure" => return Some(configure(ctx, call).await),
        "RenameChannel" => {
            let name = call.arg_text(1).unwrap_or_default();
            let sets = [("displayName", store::Value::Text(name.as_str().into())), ("comparisonKey", store::Value::Text(name.to_lowercase().into()))];
            if set_owned(ctx, call.arg_id(0), &sets, |_| true, "RenameChannel") {
                announce_change(ctx, call, "RenameChannel", vec![Value::text(name)]);
            }
            Value::None
        }
        "SetChannelMOTD" => {
            let motd = call.arg_text(1).unwrap_or_default();
            let (channel_id, char_id) = (call.arg_id(0), me(ctx));
            let updated = ctx.store.write(|tx| {
                let Some(channel) = tx.table("channels").get_id(channel_id) else { return Ok(false) };
                if !operates(tx.db(), &channel, char_id) {
                    return Ok(false);
                }
                let id = channel.id;
                tx.update("channels", id, &[("motd", store::Value::Text(motd.as_str().into()))])?;
                Ok::<_, StoreError>(true)
            });
            match updated {
                Ok(true) => announce_change(ctx, call, "SetChannelMOTD", vec![Value::text(motd)]),
                Ok(false) => {}
                Err(e) => error!("LSC.SetChannelMOTD({channel_id}): {err}", err = crate::logging::chain(&e)),
            }
            Value::None
        }
        "DestroyChannel" => {
            let (channel_id, char_id) = (call.arg_id(0), me(ctx));
            let destroyed = ctx.store.write(|tx| {
                let Some(channel) = tx.table("channels").get_id(channel_id).filter(|c| c.int("ownerID") == Some(char_id)).map(|c| c.id) else {
                    return Ok(false);
                };
                tx.delete_where("channelChars", &["channelID"], &[store::Value::Int(channel_id)])?;
                tx.delete_where("channelMods", &["channelID"], &[store::Value::Int(channel_id)])?;
                tx.delete("channels", channel)?;
                Ok::<_, StoreError>(true)
            });
            match destroyed {
                Ok(true) => announce_change(ctx, call, "DestroyChannel", Vec::new()),
                Ok(false) => {}
                Err(e) => error!("LSC.DestroyChannel({channel_id}): {err}", err = crate::logging::chain(&e)),
            }
            Value::None
        }
        "ForgetChannel" => {
            let (channel_id, char_id) = (call.arg_id(0), me(ctx));
            let forgotten = ctx.store.write(|tx| tx.delete_where("channelChars", &["channelID", "charID"], &[store::Value::Int(channel_id), store::Value::Int(char_id)]));
            if let Err(e) = forgotten {
                error!("LSC.ForgetChannel({channel_id}): {err}", err = crate::logging::chain(&e));
            }
            Value::None
        }
        // (channelID, accessor, mode, untilWhen=None, reason=None). On the caller's
        // own mailbox (channelID = charID) this is how contacts are blocked.
        "AccessControl" => access_control(ctx, call).await,

        "GetMessageDetails" => message_details(ctx, call.arg_id(0), call.arg_id(1)).await,
        // Invite(otherID, channelID, channelName, addAllowed) (lscengine.py:1057).
        "Invite" => return Some(invite(ctx, call).await),
        // Page(destinations, subject, body, attachments=None) (newmessage.py:173).
        "Page" => return Some(page(ctx, call).await),

        _ => return None,
    };
    Some(Outcome::Value(value))
}

async fn get_my_messages(ctx: &Ctx<'_>) -> Value {
    const HEADER: &[&str] =
        &["channelID", "messageID", "senderID", "subject", "created", "read"];
    let Some(char_id) = ctx.session.char_id else { return rowset(HEADER, Vec::new()) };
    let lines = ctx.store.read(|db| {
        let mut mailboxes = own_entity_ids(ctx);
        mailboxes.extend(mailing_lists(db, char_id));
        mailboxes.sort_unstable();
        mailboxes.dedup();
        let evemail = db.table("evemail");
        let mut rows: Vec<Row> =
            mailboxes.iter().flat_map(|m| evemail.find(&["channelID"], &[store::Value::Int(*m)])).collect();
        rows.sort_by_key(|r| std::cmp::Reverse((r.int("created"), r.int("messageID"))));
        rows.iter().map(|r| Value::List(r.line(HEADER))).collect()
    });
    rowset(HEADER, lines)
}

/// The mailing lists `char_id` belongs to: their mail is theirs to read.
fn mailing_lists(db: &Db, char_id: i64) -> Vec<i64> {
    let channels = db.table("channels");
    db.table("channelChars")
        .find(&["charID"], &[store::Value::Int(char_id)])
        .filter_map(|cc| cc.int("channelID"))
        .filter(|id| channels.get_id(*id).is_some_and(|c| c.int("mailingList").is_some_and(|l| l != 0)))
        .collect()
}

/// Mail sent to a mailbox: a character, a corporation, an alliance, or a
/// mailing list.
async fn page(ctx: &Ctx<'_>, call: &Call) -> Outcome {
    const SUBJECT_MAX: usize = 150;
    const BODY_MAX: usize = 8_000;
    let Some(sender) = ctx.session.char_id else { return Outcome::custom("No character.") };
    let destinations: Vec<i64> = call.args.first().and_then(|v| v.seq()).unwrap_or(&[]).iter().filter_map(|v| v.as_i64()).collect();
    let subject: String = call.arg_text(1).unwrap_or_default().chars().take(SUBJECT_MAX).collect();
    let body: String = call.arg_text(2).unwrap_or_default().chars().take(BODY_MAX).collect();
    if call.kwarg("attachments").is_some_and(|a| a.seq().is_some_and(|s| !s.is_empty())) {
        warn!("LSC.Page: attachments are not stored; sending the text alone");
    }
    let sent = ctx.store.write(|tx| {
        let db = tx.db();
        let mailboxes: Vec<i64> = destinations
            .iter()
            .copied()
            .filter(|id| {
                db.table("character_").get_id(*id).is_some()
                    || db.table("corporation").get_id(*id).is_some()
                    || db.table("alliances").get_id(*id).is_some()
                    || db.table("channels").get_id(*id).is_some_and(|c| c.int("mailingList").is_some_and(|l| l != 0))
            })
            .collect();
        if mailboxes.is_empty() {
            return Ok(None);
        }
        deliver(tx, ctx.world, sender, &mailboxes, &subject, &body)?;
        Ok::<_, StoreError>(Some(mailboxes))
    });
    match sent {
        Ok(None) => Outcome::custom("None of those can receive mail."),
        Ok(Some(mailboxes)) => {
            tracing::info!("mail {subject:?} from {sender} to {mailboxes:?}");
            Outcome::Value(Value::None)
        }
        Err(e) => {
            error!("LSC.Page from {sender}: {}", crate::logging::chain(&e));
            Outcome::custom("Your mail could not be sent.")
        }
    }
}

/// How long an invitee has to answer before the invitation lapses quietly.
const INVITE_WAIT: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Asks the invitee's client (LSC.ChatInvite(invitorID, invitorName,
/// invitorGender, channelID), lscengine.py:1411) and returns at once: the
/// invitee may take minutes to decide. Their refusal, a message key, reaches
/// the inviter as OnRemoteMessage(key, {char, channel}). With addAllowed the
/// invitee is let into the channel first.
async fn invite(ctx: &Ctx<'_>, call: &Call) -> Outcome {
    let me = ctx.session.char_id.unwrap_or(0);
    let (other, channel_id) = (call.arg_id(0), call.args.get(1).cloned().unwrap_or(Value::None));
    let channel_name = call.arg_text(2).unwrap_or_default();
    let add_allowed = call.arg_i64(3).unwrap_or(0) != 0;
    let (my_name, other_name, gender) = ctx.store.read(|db| {
        let characters = db.table("character_");
        let name = |id| characters.get_id(id).and_then(|r| r.text("characterName")).unwrap_or_default().to_string();
        (name(me), name(other), characters.get_id(me).and_then(|r| r.int("gender")).unwrap_or(1))
    });
    let char_arg = |key: &str| Outcome::UserError { msg: key.into(), args: vec![("char".into(), Value::text(&other_name))] };
    if other == me {
        return char_arg("ChtCannotInviteSelf");
    }
    let target = ctx.world.lock().clients_of(other).first().copied();
    let Some(target) = target else { return char_arg("ChtCharNotReachable") };
    if let (true, Some(id)) = (add_allowed, channel_id.as_i64()) {
        let allowed = ctx.store.write(|tx| {
            let listed = tx.table("channelMods").find(&["channelID"], &[store::Value::Int(id)]).any(|m| m.int("accessor") == Some(other));
            if !listed {
                let sets = [("channelID", store::Value::Int(id)), ("accessor", store::Value::Int(other)), ("mode", store::Value::Int(CHTMODE_CONVERSATIONALIST))];
                tx.insert("channelMods", &sets)?;
            }
            Ok::<_, StoreError>(())
        });
        if let Err(e) = allowed {
            error!("LSC.Invite: letting {other} into channel {id}: {err}", err = crate::logging::chain(&e));
        }
    }
    let args = vec![Value::Int(me), Value::text(&my_name), Value::Int(gender), channel_id.clone()];
    let Some(answer) = ctx.world.lock().call(target, "LSC", "ChatInvite", args) else { return char_arg("ChtCharNotReachable") };
    tracing::info!("{me} invited {other} to channel {channel_id:?}");
    let (world, inviter) = (ctx.world.clone(), ctx.client_id);
    tokio::spawn(async move {
        let reason = match tokio::time::timeout(INVITE_WAIT, answer).await {
            Ok(Ok(Some(Value::None))) | Err(_) => return,
            Ok(Ok(Some(v))) => v.as_string().unwrap_or_else(|| "ChtInviteException".into()),
            Ok(Ok(None)) | Ok(Err(_)) => "ChtInviteException".into(),
        };
        tracing::info!("{other} did not take the invitation to {channel_id:?}: {reason}");
        let dict = Value::Dict(vec![(Value::bytes("char"), Value::text(other_name)), (Value::bytes("channel"), Value::text(channel_name))]);
        let w = world.lock();
        w.notify(&[inviter], "OnRemoteMessage", Value::Tuple(vec![Value::text(reason), dict]));
    });
    Outcome::Value(Value::None)
}

/// Puts a mail in each mailbox and tells whoever is online to look:
/// OnMessage(recipients, messageID, senderID, subject, created) (inbox.py:405).
pub fn deliver(
    tx: &mut Tx<'_>,
    world: &crate::world::World,
    sender: i64,
    mailboxes: &[i64],
    subject: &str,
    body: &str,
) -> Result<(), StoreError> {
    let created = crate::bulkdata::filetime_now();
    for mailbox in mailboxes {
        let row = tx.insert(
            "evemail",
            &[
                ("channelID", store::Value::Int(*mailbox)),
                ("senderID", store::Value::Int(sender)),
                ("subject", store::Value::Text(subject.into())),
                ("created", store::Value::Int(created)),
                ("read", store::Value::Int(0)),
            ],
        )?;
        let message_id = tx.table("evemail").row(row).and_then(|r| r.int("messageID")).unwrap_or(0);
        tx.insert(
            "evemailDetails",
            &[
                ("messageID", store::Value::Int(message_id)),
                ("mimeTypeID", store::Value::Int(1)),
                ("attachment", store::Value::Text(body.into())),
            ],
        )?;
        // Who reads this mailbox: a pilot, a corporation, or a list's members.
        let mut readers: Vec<i64> = vec![*mailbox];
        readers.extend(
            tx.table("channelChars")
                .find(&["channelID"], &[store::Value::Int(*mailbox)])
                .filter_map(|r| r.int("charID"))
                .filter(|c| *c != 0),
        );
        let w = world.lock();
        let mut clients: Vec<i64> = readers.iter().flat_map(|r| w.clients_of(*r)).collect();
        clients.extend(w.corp_members(*mailbox));
        clients.sort_unstable();
        clients.dedup();
        let args = Value::Tuple(vec![
            Value::List(vec![Value::Int(*mailbox)]),
            Value::Int(message_id),
            Value::Int(sender),
            Value::text(subject),
            Value::Long(created),
        ]);
        w.notify(&clients, "OnMessage", args);
    }
    Ok(())
}

/// GetMessageDetails(channelID, messageID) -> (message, attachments): the
/// message row with its text body, and any other attachments (inbox.py:669
/// reads msg.body and each attachment's mimeType).
async fn message_details(ctx: &Ctx<'_>, channel_id: i64, message_id: i64) -> Value {
    const MESSAGE: &[&str] = &["channelID", "messageID", "senderID", "subject", "created", "read", "body"];
    const ATTACHMENT: &[&str] = &["attachmentID", "messageID", "mimeType", "attachment"];
    let mailboxes = own_entity_ids(ctx);
    if !mailboxes.contains(&channel_id) {
        return Value::None;
    }
    // The mime types are static data.
    let mimes = crate::sde::table("evemailMimeType");
    let found = ctx.store.read(|db| {
        let r = db.table("evemail").get_id(message_id).filter(|r| r.int("channelID") == Some(channel_id))?;
        let message = r.line(&MESSAGE[..6]);
        let mut details: Vec<Row> = db.table("evemailDetails").find(&["messageID"], &[store::Value::Int(message_id)]).collect();
        details.sort_by_key(|d| d.int("attachmentID"));
        let attachments: Vec<(String, Vec<Value>)> = details
            .iter()
            .flat_map(move |d| {
                d.int("mimeTypeID").into_iter().flat_map(move |id| mimes.find("mimeTypeID", id)).map(move |m| {
                    let mime = m.text("mimeType").unwrap_or_default().to_string();
                    let line = vec![d.cell("attachmentID"), d.cell("messageID"), m.cell("mimeType"), d.cell("attachment")];
                    (mime, line)
                })
            })
            .collect();
        Some((message, attachments))
    });
    let Some((mut message, rows)) = found else { return Value::None };
    let mut body = Value::text("");
    let mut attachments = Vec::new();
    for (mime, line) in rows {
        if mime == "text/plain" && body == Value::text("") {
            body = line[3].clone();
        } else {
            attachments.push(crate::svc::rows::row(ATTACHMENT, line));
        }
    }
    message.push(body);
    Value::Tuple(vec![crate::svc::rows::row(MESSAGE, message), Value::List(attachments)])
}

async fn mark_messages_read(ctx: &Ctx<'_>, arg: Option<&Value>) -> Value {
    let Some(char_id) = ctx.session.char_id else { return Value::None };
    let ids = id_list(arg);
    if ids.is_empty() {
        return Value::None;
    }
    let marked = ctx.store.write(|tx| {
        let mut mailboxes = own_entity_ids(ctx);
        mailboxes.extend(mailing_lists(tx.db(), char_id));
        let unread: Vec<store::RowId> = ids
            .iter()
            .filter_map(|id| tx.table("evemail").get_id(*id))
            .filter(|r| r.int("channelID").is_some_and(|c| mailboxes.contains(&c)))
            .map(|r| r.id)
            .collect();
        for row in unread {
            tx.update("evemail", row, &[("read", store::Value::Int(1))])?;
        }
        Ok::<_, StoreError>(())
    });
    if let Err(e) = marked {
        error!("LSC.MarkMessagesRead({} ids): {err}", ids.len(), err = crate::logging::chain(&e));
    }
    Value::None
}

async fn delete_messages(ctx: &Ctx<'_>, channel_id: i64, arg: Option<&Value>) -> Value {
    let ids = id_list(arg);
    if ids.is_empty() {
        return Value::None;
    }
    if !own_entity_ids(ctx).contains(&channel_id) {
        warn!("LSC.DeleteMessages: channel {channel_id} is not one of the caller's");
        return Value::None;
    }
    let deleted = ctx.store.write(|tx| {
        let rows: Vec<store::RowId> = ids
            .iter()
            .filter_map(|id| tx.table("evemail").get_id(*id))
            .filter(|r| r.int("channelID") == Some(channel_id))
            .map(|r| r.id)
            .collect();
        for row in rows {
            tx.delete("evemail", row)?;
        }
        Ok::<_, StoreError>(())
    });
    if let Err(e) = deleted {
        error!("LSC.DeleteMessages({channel_id}): {err}", err = crate::logging::chain(&e));
    }
    Value::None
}

/// The message IDs a call names, each once.
fn id_list(arg: Option<&Value>) -> Vec<i64> {
    let mut ids: Vec<i64> = arg.and_then(|v| v.seq()).map(|s| s.iter().filter_map(|v| v.as_i64()).collect()).unwrap_or_default();
    ids.sort_unstable();
    ids.dedup();
    ids
}

async fn join_channels(ctx: &Ctx<'_>, arg: Option<&Value>) -> Value {
    let requested: Vec<Value> =
        arg.and_then(|v| v.seq()).map(|s| s.to_vec()).unwrap_or_default();
    let me = self_member(ctx).await;
    let mut out = Vec::with_capacity(requested.len());
    for channel_id in &requested {
        let result = join_one(ctx, channel_id, me.as_ref()).await;
        let ok = matches!(&result, Value::Tuple(t) if t.get(1) == Some(&Value::Int(1)));
        if ok {
            let mut world = ctx.world.lock();
            let members = world.join_channel(channel_id, ctx.client_id);
            let others: Vec<i64> = members.iter().copied().filter(|c| *c != ctx.client_id).collect();
            let who = who(ctx.session);
            announce_counted(&world, &others, members.len(), channel_id, "JoinChannel", &who, Vec::new());
        }
        out.push(result);
    }
    Value::List(out)
}

fn leave(ctx: &Ctx<'_>, channel: &Value) {
    let mut world = ctx.world.lock();
    let members = world.leave_channel(channel, ctx.client_id);
    announce(&world, &members, channel, "LeaveChannel", &who(ctx.session), Vec::new());
}

/// Tells everyone with the call's channel (argument 0) open about a change to it.
fn announce_change(ctx: &Ctx<'_>, call: &Call, method: &'static str, args: Vec<Value>) {
    let Some(channel) = call.args.first() else { return };
    let world = ctx.world.lock();
    let members = world.channel_members(channel);
    announce(&world, &members, channel, method, &who(ctx.session), args);
}

/// The speaker as OnLSC wants it: (allianceID, corpID, [charID, name, typeID],
/// role, corpRole). The list form lets the client name the speaker without a
/// lookup (lscengine.OnLSC hints cfg.eveowners with it).
pub fn who(session: &Session) -> Value {
    Value::Tuple(vec![
        Value::int_or_none(session.alliance_id),
        Value::int_or_none(session.corp_id),
        Value::List(vec![
            Value::int_or_none(session.char_id),
            Value::text(session.char_name.clone()),
            Value::Int(session.char_type_id),
        ]),
        Value::Long(session.role),
        Value::Long(session.corp_role()),
    ])
}

/// OnLSC(channelID, estimatedMemberCount, method, who, args) to `members`.
pub fn announce(
    world: &WorldState,
    members: &[i64],
    channel: &Value,
    method: &'static str,
    who: &Value,
    args: Vec<Value>,
) {
    announce_counted(world, members, members.len(), channel, method, who, args);
}

pub fn announce_counted(
    world: &WorldState,
    targets: &[i64],
    member_count: usize,
    channel: &Value,
    method: &'static str,
    who: &Value,
    args: Vec<Value>,
) {
    let event = Value::Tuple(vec![
        channel.clone(),
        Value::Int(member_count as i64),
        Value::bytes(method),
        who.clone(),
        Value::Tuple(args),
    ]);
    world.notify(targets, "OnLSC", event);
}

async fn join_one(ctx: &Ctx<'_>, channel_id: &Value, me: Option<&Vec<Value>>) -> Value {
    let id_value = channel_id.clone();

    if let Some(id) = channel_id.as_i64() {
        return match channel_info(ctx, id).await {
            Some((line, default_mode)) => {
                let acl = acl_for(ctx, id, default_mode).await;
                let members = members_for(ctx, id, me).await;
                joined(id_value, row(CHANNEL_HEADER, line), acl, members)
            }
            None => refused(id_value, "That channel does not exist."),
        };
    }

    if channel_id.seq().is_some() {
        let members = local_members(ctx, channel_id, me).await;
        return joined(id_value, Value::None, rowset(ACL_HEADER, Vec::new()), members);
    }

    refused(id_value, "Malformed channel identifier.")
}

fn joined(channel_id: Value, info: Value, acl: Value, members: Value) -> Value {
    Value::Tuple(vec![
        channel_id,
        Value::Int(1),
        Value::Tuple(vec![info, acl, members]),
    ])
}

fn refused(channel_id: Value, why: &str) -> Value {
    Value::Tuple(vec![
        channel_id,
        Value::Int(0),
        Value::Tuple(vec![
            Value::bytes("LSCCannotJoin"),
            Value::Dict(vec![(Value::bytes("msg"), Value::bytes(why))]),
        ]),
    ])
}

async fn channel_info(ctx: &Ctx<'_>, channel_id: i64) -> Option<(Vec<Value>, i64)> {
    let found = ctx.store.read(|db| {
        db.table("channels").get_id(channel_id).map(|r| (channel_line(&r, false), r.int("mode").unwrap_or(CHTMODE_LISTENER)))
    });
    found.or_else(|| own_entity_ids(ctx).contains(&channel_id).then(|| (synthetic_mailbox(channel_id), CHTMODE_LISTENER)))
}

fn channel_line(r: &Row, hide_password: bool) -> Vec<Value> {
    let mut line = r.line(CHANNEL_HEADER);
    if hide_password {
        line[IX_PASSWORD] = Value::None;
    }
    line
}

fn synthetic_mailbox(owner_id: i64) -> Vec<Value> {
    vec![
        Value::Int(owner_id),
        Value::Int(owner_id),
        Value::None,
        Value::None,
        Value::None,
        Value::Int(1),
        Value::None,
        Value::Int(1),
        Value::Int(CSPA_MAILBOX),
        Value::Int(0),
        Value::Int(CHTMODE_LISTENER),
        Value::Int(1),
        Value::Int(1),
        Value::Int(0),
    ]
}

async fn get_channels(ctx: &Ctx<'_>) -> Value {
    let Some(char_id) = ctx.session.char_id else { return rowset(CHANNEL_HEADER, Vec::new()) };
    let p_corp = ctx.session.corp_id.unwrap_or(-1);
    let p_alliance = ctx.session.alliance_id.unwrap_or(-1);
    let owners = [char_id, p_corp, p_alliance, OWNER_SYSTEM];

    // Each channel the caller may see, and whether they are a member.
    let mut found: Vec<(i64, Vec<Value>, bool)> = ctx.store.read(|db| {
        let members = db.table("channelChars");
        db.table("channels")
            .rows()
            .filter_map(|c| {
                let id = c.int("channelID")?;
                let member = members.get(&[store::Value::Int(id), store::Value::Int(char_id)]).is_some();
                let owned = c.int("ownerID").is_some_and(|o| owners.contains(&o));
                (owned || member).then(|| (id, channel_line(&c, true), member))
            })
            .collect()
    });
    found.sort_by_key(|(id, _, _)| *id);

    let mut lines = Vec::new();
    let mut seen: Vec<i64> = Vec::new();
    for (id, mut line, member) in found {
        seen.push(id);
        let subscribed = member || own_entity_ids(ctx).contains(&id);
        line[IX_SUBSCRIBED] = Value::Int(subscribed as i64);
        lines.push(Value::List(line));
    }

    for id in own_entity_ids(ctx) {
        if !seen.contains(&id) {
            lines.push(Value::List(synthetic_mailbox(id)));
        }
    }
    rowset(CHANNEL_HEADER, lines)
}

async fn get_members(ctx: &Ctx<'_>, arg: Option<&Value>) -> Value {
    let me = self_member(ctx).await;
    match arg.and_then(|v| v.as_i64()) {
        Some(id) => members_for(ctx, id, me.as_ref()).await,
        None if arg.is_some_and(|v| v.seq().is_some()) => local_members(ctx, arg.expect("checked"), me.as_ref()).await,
        None => {
            let lines = me.map(|m| vec![Value::List(m)]).unwrap_or_default();
            rowset(MEMBER_HEADER, lines)
        }
    }
}

async fn members_for(ctx: &Ctx<'_>, channel_id: i64, me: Option<&Vec<Value>>) -> Value {
    let my_id = ctx.session.char_id.unwrap_or(0);
    let mut have_self = false;
    // The primary key (channelID, charID) gives them in charID order.
    let mut lines: Vec<Value> = ctx.store.read(|db| {
        let characters = db.table("character_");
        db.table("channelChars")
            .find(&["channelID"], &[store::Value::Int(channel_id)])
            .map(|r| {
                let id = r.int("charID").unwrap_or(0);
                have_self |= id == my_id;
                let ch = characters.get_id(id);
                Value::List(vec![
                    Value::Int(id),
                    Value::int_or_none(nonzero(r.int("corpID"))),
                    Value::int_or_none(nonzero(r.int("allianceID"))),
                    Value::Long(r.int("role").unwrap_or(0)),
                    owner_hint(
                        id,
                        ch.and_then(|c| c.text("characterName")).unwrap_or_default().to_string(),
                        ch.and_then(|c| c.int("typeID")).unwrap_or(0),
                    ),
                ])
            })
            .collect()
    });
    if !have_self {
        if let Some(m) = me {
            lines.push(Value::List(m.clone()));
        }
    }
    rowset(MEMBER_HEADER, lines)
}

/// Who is in a Local channel (or another channel named by its session
/// attributes): the players with it open, the caller, and in Local the bots
/// online in that system.
async fn local_members(ctx: &Ctx<'_>, channel_id: &Value, me: Option<&Vec<Value>>) -> Value {
    let my_id = ctx.session.char_id.unwrap_or(0);
    let (players, bots): (Vec<(i64, i64)>, Vec<LocalBot>) = {
        let world = ctx.world.lock();
        let players = world
            .channel_members(channel_id)
            .iter()
            .filter_map(|c| world.presence_of_client(*c))
            .map(|p| (p.char_id, p.role))
            .filter(|(c, _)| *c != my_id)
            .collect();
        let bots = local_system(channel_id)
            .and_then(|s| world.bot_local.get(&s))
            .map(|list| list.iter().filter_map(|i| world.bot_roster.get(*i as usize).cloned()).collect())
            .unwrap_or_default();
        (players, bots)
    };
    let mut lines: Vec<Value> = me.map(|m| vec![Value::List(m.clone())]).unwrap_or_default();
    if !players.is_empty() {
        let mut ids: Vec<i64> = players.iter().map(|(c, _)| *c).collect();
        ids.sort_unstable();
        ids.dedup();
        ctx.store.read(|db| {
            let corporations = db.table("corporation");
            for c in ids.iter().filter_map(|id| db.table("character_").get_id(*id)) {
                let id = c.int("characterID").unwrap_or(0);
                let role = players.iter().find(|(c, _)| *c == id).map_or(ROLE_LOGIN | ROLE_PLAYER, |(_, role)| *role);
                let corp = c.int("corporationID");
                let alliance = corp.and_then(|co| corporations.get_id(co)).and_then(|co| co.int("allianceID"));
                let name = c.text("characterName").unwrap_or_default().to_string();
                lines.push(member_line(id, corp, nonzero(alliance), role, name, c.int("typeID").unwrap_or(0)));
            }
        });
    }
    for b in bots.into_iter().filter(|b| b.char_id != my_id) {
        lines.push(member_line(b.char_id, Some(b.corp_id), b.alliance_id, ROLE_LOGIN | ROLE_PLAYER, b.name, b.type_id));
    }
    rowset(MEMBER_HEADER, lines)
}

fn member_line(char_id: i64, corp_id: Option<i64>, alliance_id: Option<i64>, role: i64, name: String, type_id: i64) -> Value {
    Value::List(vec![
        Value::Int(char_id),
        Value::int_or_none(corp_id),
        Value::int_or_none(alliance_id),
        Value::Long(role),
        owner_hint(char_id, name, type_id),
    ])
}

/// A bot as OnLSC names a speaker, for its comings and goings in Local.
pub fn bot_who(b: &LocalBot) -> Value {
    Value::Tuple(vec![
        Value::int_or_none(b.alliance_id),
        Value::Int(b.corp_id),
        Value::List(vec![Value::Int(b.char_id), Value::text(b.name.clone()), Value::Int(b.type_id)]),
        Value::Long(ROLE_LOGIN | ROLE_PLAYER),
        Value::Long(0),
    ])
}

async fn self_member(ctx: &Ctx<'_>) -> Option<Vec<Value>> {
    let char_id = ctx.session.char_id?;
    let (name, type_id) = ctx.store.read(|db| {
        db.table("character_")
            .get_id(char_id)
            .map(|r| (r.text("characterName").unwrap_or_default().to_string(), r.int("typeID").unwrap_or(0)))
            .unwrap_or_default()
    });
    Some(vec![
        Value::Int(char_id),
        Value::int_or_none(ctx.session.corp_id),
        Value::int_or_none(ctx.session.alliance_id),
        Value::Long(ctx.session.role),
        owner_hint(char_id, name, type_id),
    ])
}

fn owner_hint(owner_id: i64, name: String, type_id: i64) -> Value {
    Value::List(vec![
        Value::Int(owner_id),
        Value::Bytes(name.into_bytes()),
        Value::Int(type_id),
    ])
}

async fn acl_for(ctx: &Ctx<'_>, channel_id: i64, default_mode: i64) -> Value {
    let mut has_default = false;
    let mut lines: Vec<Value> = ctx.store.read(|db| {
        let mut mods: Vec<Row> = db.table("channelMods").find(&["channelID"], &[store::Value::Int(channel_id)]).collect();
        mods.sort_by_key(|r| r.int("id"));
        mods.iter()
            .map(|r| {
                let accessor = r.int("accessor");
                has_default |= accessor.is_none();
                Value::List(vec![
                    Value::int_or_none(accessor),
                    Value::int_or_none(r.int("mode")),
                    r.int("untilWhen").map(Value::Long).unwrap_or(Value::None),
                    r.text("reason").map_or(Value::None, Value::text),
                    r.text("admin").map_or(Value::None, Value::text),
                    Value::int_or_none(r.int("originalMode")),
                ])
            })
            .collect()
    });
    if !has_default {
        lines.insert(
            0,
            Value::List(vec![
                Value::None,
                Value::Int(default_mode),
                Value::None,
                Value::None,
                Value::None,
                Value::None,
            ]),
        );
    }
    rowset(ACL_HEADER, lines)
}

fn own_entity_ids(ctx: &Ctx<'_>) -> Vec<i64> {
    let mut ids = Vec::with_capacity(3);
    ids.extend(ctx.session.char_id);
    ids.extend(ctx.session.corp_id);
    ids.extend(ctx.session.alliance_id);
    ids
}

fn nonzero(v: Option<i64>) -> Option<i64> {
    v.filter(|n| *n != 0)
}

fn me(ctx: &Ctx<'_>) -> i64 {
    ctx.session.char_id.unwrap_or(0)
}

fn flag(call: &Call, name: &str) -> i64 {
    i64::from(call.kwarg(name).and_then(|v| v.as_i64()).unwrap_or(0) != 0)
}

/// Sets columns of channel `channel_id` if the caller owns it and `also`
/// holds for it: whether it did.
fn set_owned(ctx: &Ctx<'_>, channel_id: i64, sets: store::Sets, also: impl Fn(&Row) -> bool, what: &str) -> bool {
    let char_id = me(ctx);
    let updated = ctx.store.write(|tx| {
        let owned = tx.table("channels").get_id(channel_id).filter(|c| c.int("ownerID") == Some(char_id) && also(c));
        let Some(id) = owned.map(|c| c.id) else { return Ok(false) };
        tx.update("channels", id, sets)?;
        Ok::<_, StoreError>(true)
    });
    updated.unwrap_or_else(|e| {
        error!("LSC.{what}({channel_id}): {err}", err = crate::logging::chain(&e));
        false
    })
}

/// CreateChannel(displayName, joinExisting=1, memberless=0, mailingList=0,
/// temporary=False): lscengine unpacks (info, acl, memberList).
async fn create_channel(ctx: &Ctx<'_>, call: &Call) -> Value {
    let name = call.arg_text(0).unwrap_or_default();
    let key = name.to_lowercase();
    let char_id = me(ctx);
    let join_existing = flag(call, "joinExisting") != 0;
    let made = ctx.store.write(|tx| {
        let existing = if join_existing {
            tx.table("channels")
                .rows()
                .find(|c| c.text("comparisonKey") == Some(key.as_str()) && c.int("temporary") == Some(0))
                .and_then(|c| c.int("channelID"))
        } else {
            None
        };
        if let Some(id) = existing {
            return Ok((id, true));
        }
        let row = tx.insert(
            "channels",
            &[
                ("ownerID", store::Value::Int(char_id)),
                ("displayName", store::Value::Text(name.as_str().into())),
                ("motd", store::Value::Text("".into())),
                ("comparisonKey", store::Value::Text(key.as_str().into())),
                ("memberless", store::Value::Int(flag(call, "memberless"))),
                ("mailingList", store::Value::Int(flag(call, "mailingList"))),
                ("temporary", store::Value::Int(flag(call, "temporary"))),
                ("mode", store::Value::Int(CHTMODE_CONVERSATIONALIST)),
                ("estimatedMemberCount", store::Value::Int(1)),
            ],
        )?;
        Ok::<_, StoreError>((tx.table("channels").row(row).and_then(|c| c.int("channelID")).unwrap_or(0), false))
    });
    let (channel_id, existed) = match made {
        Ok(made) => made,
        Err(e) => {
            error!("LSC.CreateChannel({name:?}): {err}", err = crate::logging::chain(&e));
            return Value::None;
        }
    };
    let role = if existed { CHTMODE_CONVERSATIONALIST } else { CHTMODE_CREATOR };
    let corp = ctx.session.corp_id.unwrap_or(0);
    let alliance = ctx.session.alliance_id.unwrap_or(0);
    let sets = [
        ("channelID", store::Value::Int(channel_id)),
        ("charID", store::Value::Int(char_id)),
        ("corpID", store::Value::Int(corp)),
        ("allianceID", store::Value::Int(alliance)),
        ("role", store::Value::Int(role)),
    ];
    if let Err(e) = ctx.store.write(|tx| tx.insert_or_ignore("channelChars", &sets)) {
        error!("LSC.CreateChannel: joining {channel_id}: {err}", err = crate::logging::chain(&e));
    }

    let Some((line, default_mode)) = channel_info(ctx, channel_id).await else { return Value::None };
    let acl = acl_for(ctx, channel_id, default_mode).await;
    let members = members_for(ctx, channel_id, self_member(ctx).await.as_ref()).await;
    Value::Tuple(vec![row(CHANNEL_HEADER, line), acl, members])
}

/// Configure(channelID, displayName=, motd=, cspa=, memberless=, oldPassword=,
/// newPassword=): the channel settings dialog. Owner only; the password changes
/// only if oldPassword matches.
async fn configure(ctx: &Ctx<'_>, call: &Call) -> Outcome {
    let channel_id = call.arg_id(0);
    let text_kw = |name: &str| call.kwarg(name).and_then(|v| v.as_string());
    let int_kw = |name: &str| call.kwarg(name).and_then(|v| v.as_i64());
    if let Some(value) = int_kw("languageRestriction") {
        let result = set_language_restriction(ctx, channel_id, value).await;
        if !matches!(result, Outcome::Value(_)) { return result; }
    }
    if let Some(name) = text_kw("displayName") {
        let sets = [("displayName", store::Value::Text(name.as_str().into())), ("comparisonKey", store::Value::Text(name.to_lowercase().into()))];
        set_owned(ctx, channel_id, &sets, |_| true, "Configure");
    }
    if let Some(motd) = text_kw("motd") {
        set_owned(ctx, channel_id, &[("motd", store::Value::Text(motd.into()))], |_| true, "Configure");
    }
    for column in ["cspa", "memberless"] {
        if let Some(v) = int_kw(column) {
            set_owned(ctx, channel_id, &[(column, store::Value::Int(v))], |_| true, "Configure");
        }
    }
    if call.kwarg("newPassword").is_some() {
        let new = text_kw("newPassword").filter(|p| !p.is_empty());
        let old = text_kw("oldPassword").unwrap_or_default();
        let password = new.map_or(store::Value::Null, |p| store::Value::Text(p.into()));
        set_owned(ctx, channel_id, &[("password", password)], |c| c.text("password").unwrap_or("") == old, "Configure");
    }
    Outcome::Value(Value::None)
}

async fn access_control(ctx: &Ctx<'_>, call: &Call) -> Value {
    let channel_id = call.arg_id(0);
    let accessor = call.arg_id(1);
    let mode = call.arg_i64(2).unwrap_or(CHTMODE_NOTSPECIFIED);
    let until = call.arg_i64(3);
    let reason = call.arg_text(4);
    let char_id = me(ctx);

    let allowed = channel_id == char_id
        || ctx.store.read(|db| db.table("channels").get_id(channel_id).is_some_and(|c| operates(db, &c, char_id)));
    if !allowed {
        warn!("AccessControl on channel {channel_id} by a non-operator");
        return Value::None;
    }
    let cleared = ctx.store.write(|tx| tx.delete_where("channelMods", &["channelID", "accessor"], &[store::Value::Int(channel_id), store::Value::Int(accessor)]));
    if let Err(e) = cleared {
        error!("LSC.AccessControl({channel_id}, {accessor}): {err}", err = crate::logging::chain(&e));
    }
    if mode != CHTMODE_NOTSPECIFIED {
        let set = ctx.store.write(|tx| {
            let admin = tx.table("character_").get_id(char_id).map_or(store::Value::Null, |c| c.value("characterName").clone());
            let sets = [
                ("channelID", store::Value::Int(channel_id)),
                ("accessor", store::Value::Int(accessor)),
                ("mode", store::Value::Int(mode)),
                ("untilWhen", until.map_or(store::Value::Null, store::Value::Int)),
                ("reason", reason.as_deref().map_or(store::Value::Null, |r| store::Value::Text(r.into()))),
                ("admin", admin),
            ];
            tx.insert("channelMods", &sets)
        });
        if let Err(e) = set {
            error!("LSC.AccessControl({channel_id}, {accessor}): {err}", err = crate::logging::chain(&e));
        }
    }
    Value::None
}
