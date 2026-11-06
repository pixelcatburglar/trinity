use eve_marshal::Value;
use tracing::error;

use crate::services::{Call, Ctx, Outcome};
use crate::store::{Db, Row, StoreError, Value as Cell};
use crate::svc::rows::{row, rowset};

pub async fn charmgr(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        "GetPublicInfo" => ctx.store.read(|db| public_info(db, subject(call, ctx))),
        "GetPublicInfo3" => ctx.store.read(|db| public_info3(db, subject(call, ctx))),
        "GetPrivateInfo" => ctx.store.read(|db| private_info(db, subject(call, ctx))),
        "GetCharacterInfo" => character_info(ctx),
        "GetTopBounties" => ctx.store.read(top_bounties),
        "AddToBounty" => return Some(add_to_bounty(ctx, call).await),
        _ => return None,
    };
    Some(Outcome::Value(value))
}

/// AddToBounty(charID, amount): the caller pays; the bounty goes on the target.
async fn add_to_bounty(ctx: &Ctx<'_>, call: &Call) -> Outcome {
    match place_bounty(ctx, call).await {
        Ok(()) => {
            super::wallet::notify_balance(ctx, ctx.session.char_id.unwrap_or(0)).await;
            Outcome::Value(Value::None)
        }
        Err(outcome) => outcome,
    }
}

/// A refused bounty: returned from the store write, it undoes what the
/// write did.
struct Refused(Outcome);

impl From<StoreError> for Refused {
    fn from(e: StoreError) -> Self {
        error!("AddToBounty: {}", crate::logging::chain(&e));
        Refused(Outcome::custom("The bounty could not be placed. Your payment has not been taken."))
    }
}

async fn place_bounty(ctx: &Ctx<'_>, call: &Call) -> Result<(), Outcome> {
    use crate::svc::wallet::{post, Entry, Transfer, REF_BOUNTY};
    let target = i64::from(i32::try_from(call.arg_id(0)).map_err(|_| Outcome::custom("Invalid bounty target."))?);
    let amount = call.args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0);
    let me = ctx.session.char_id.ok_or_else(|| Outcome::custom("Select a character first."))?;
    // base_missions.py's bounty form requires at least 5,000 ISK.
    if !amount.is_finite() || amount < 5000.0 || target == me || target < 100_000_000 {
        return Err(Outcome::custom("A bounty must be at least 5,000 ISK and name another pilot."));
    }
    ctx.store
        .write(|tx| -> Result<(), Refused> {
            let Some(victim) = tx.table("character_").get_id(target) else {
                return Err(Refused(Outcome::custom("That pilot does not exist.")));
            };
            if victim.float("securityRating").unwrap_or(0.0) > -1.0 {
                return Err(Refused(Outcome::custom("Bounties require a security status of -1.0 or lower.")));
            }
            let victim = victim.id;
            let arg = target.to_string();
            let entry = Entry { payer:me,payee:0,ref_type:REF_BOUNTY,arg_id1:&arg,reason:"Player bounty" };
            post(tx,me,-amount,&entry).map_err(|e| match e {
                Transfer::NotEnough{need,have} => Refused(Outcome::not_enough_money(need,have)),
                Transfer::NoSuchOwner => Refused(Outcome::custom("Your wallet could not be found.")),
                Transfer::Db(e) => Refused::from(e),
            })?;
            let now = crate::bulkdata::filetime_now();
            // The payer's bounty on the target adds to any they placed before.
            match tx.table("bounties").get(&[Cell::Int(target), Cell::Int(me)]).map(|r| (r.id, r.float("bounty").unwrap_or(0.0))) {
                Some((placed, bounty)) => tx.update("bounties", placed, &[("bounty", Cell::Float(bounty + amount)), ("placedAt", Cell::Int(now))])?,
                None => {
                    tx.insert("bounties", &[("characterID", Cell::Int(target)), ("ownerID", Cell::Int(me)), ("bounty", Cell::Float(amount)), ("placedAt", Cell::Int(now))])?;
                }
            }
            let bounty = tx.table("character_").row(victim).and_then(|r| r.float("bounty")).unwrap_or(0.0);
            tx.update("character_", victim, &[("bounty", Cell::Float(bounty + amount))])?;
            Ok(())
        })
        .map_err(|Refused(outcome)| outcome)?;
    tracing::info!("bounty of {amount} ISK placed on {target}");
    Ok(())
}

pub async fn character(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        "GetPrivateInfo" => ctx.store.read(|db| private_info(db, subject(call, ctx))),
        "GetCharacterInfo" => character_info(ctx),
        "GetCharacterDescription" => ctx.store.read(|db| character_description(db, subject(call, ctx))),
        "SetCharacterDescription" => set_character_description(call, ctx),
        "GetCloneTypeID" => clone_type_id(ctx),
        "GetHomeStation" => crate::svc::medical::get_home_station(ctx).await,
        "GetRecentShipKillsAndLosses" => recent_kills(ctx),
        _ => return None,
    };
    Some(Outcome::Value(value))
}

/// Trinity's combat log reads these fields directly, including the XML used
/// by util.CombatLog_CopyText. Older rows have no recoverable item-loss detail.
fn recent_kills(ctx: &Ctx<'_>) -> Value {
    const HEADER: &[&str] = &["killID","killTime","solarSystemID","moonID","victimCharacterID",
        "victimCorporationID","victimAllianceID","victimShipTypeID","victimDamageTaken",
        "finalCharacterID","finalCorporationID","finalAllianceID","finalShipTypeID",
        "finalWeaponTypeID","finalDamageDone","finalSecurityStatus","killBlob"];
    let Some(me) = ctx.session.char_id else { return rowset(HEADER, Vec::new()) };
    ctx.store.read(|db| {
        let participants = db.table("killLogParticipants");
        let mut found: Vec<Row> = db
            .table("killLog")
            .rows()
            .filter(|k| k.int("victimCharID") == Some(me) || k.int("finalBlowID") == Some(me))
            .collect();
        found.sort_by(|a, b| (b.int("killTime"), b.int("killID")).cmp(&(a.int("killTime"), a.int("killID"))));
        found.truncate(100);
        // A column with 0 (or nothing) in it as no value.
        fn set(r: Option<Row>, column: &str) -> Value {
            Value::int_or_none(r.and_then(|r| r.int(column)).filter(|v| *v != 0))
        }
        let lines = found.iter().map(|k| {
            let kill = k.int("killID").unwrap_or(0);
            let final_blow = k.int("finalBlowID").and_then(|id| participants.get(&[Cell::Int(kill), Cell::Int(id)]));
            let mut others: Vec<Row> = participants
                .find(&["killID"], &[Cell::Int(kill)])
                .filter(|p| p.int("finalBlow") == Some(0))
                .collect();
            others.sort_by(|a, b| {
                let damage = |p: &Row| p.float("damageDone").unwrap_or(0.0);
                damage(b).total_cmp(&damage(a)).then(a.int("ownerID").cmp(&b.int("ownerID")))
            });
            let mut blob = String::from("<attackers>");
            for p in others {
                blob.push_str("<a");
                for (column,key) in [("ownerID","c"),("corporationID","r"),("allianceID","a"),("shipTypeID","s"),("weaponTypeID","w")] {
                    if let Some(value) = p.int(column).filter(|v| *v > 0) { blob.push_str(&format!(" {key}=\"{value}\"")); }
                }
                blob.push_str(&format!(" d=\"{}\"",p.float("damageDone").unwrap_or(0.0).max(0.0).round() as i64));
                if let Some(sec) = p.float("securityStatus") { blob.push_str(&format!(" t=\"{sec:.1}\"")); }
                blob.push_str("/>");
            }
            blob.push_str("</attackers>");
            blob.push_str(k.text("killBlob").unwrap_or(""));
            Value::List(vec![
                k.cell("killID"),
                k.cell("killTime"),
                k.cell("solarSystemID"),
                Value::None,
                set(Some(*k), "victimCharID"),
                set(Some(*k), "victimCorpID"),
                set(Some(*k), "victimAllianceID"),
                k.cell("victimShipTypeID"),
                k.cell("totalDamage"),
                Value::int_or_none(k.int("finalBlowID").filter(|v| *v > 0)),
                set(final_blow, "corporationID"),
                set(final_blow, "allianceID"),
                set(final_blow, "shipTypeID"),
                set(final_blow, "weaponTypeID"),
                Value::Double(final_blow.and_then(|p| p.float("damageDone")).unwrap_or(0.0)),
                final_blow.and_then(|p| p.float("securityStatus")).map_or(Value::None, Value::Double),
                Value::text(blob),
            ])
        }).collect();
        rowset(HEADER, lines)
    })
}

fn subject(call: &Call, ctx: &Ctx<'_>) -> i64 {
    call.args
        .first()
        .and_then(|v| v.as_i64())
        .or(ctx.session.char_id)
        .unwrap_or(0)
}

fn empty_row(header: &[&str]) -> Value {
    row(header, header.iter().map(|_| Value::None).collect())
}

fn int_col(r: &Row, column: &str) -> Value {
    Value::int_or_none(r.int(column))
}

fn long_col(r: &Row, column: &str) -> Value {
    r.int(column).map_or(Value::None, Value::Long)
}

fn dbl_col(r: &Row, column: &str) -> Value {
    r.float(column).map_or(Value::None, Value::Double)
}

fn txt_col(r: &Row, column: &str) -> Value {
    r.text(column).map_or(Value::None, |v| Value::Bytes(v.as_bytes().to_vec()))
}

/// The static bloodline in column `column`, if there is one.
fn bloodline_of(r: &Row, column: &str) -> Option<crate::sde::Row<'static>> {
    crate::sde::table("chrBloodlines").get("bloodlineID", r.int(column)?)
}

/// A static text column as [`txt_col`] sends it.
fn static_txt(r: Option<crate::sde::Row>, column: &str) -> Value {
    r.and_then(|r| r.text(column)).map_or(Value::None, |v| Value::Bytes(v.as_bytes().to_vec()))
}

fn public_info(db: &Db, char_id: i64) -> Value {
    const HEADER: &[&str] = &[
        "characterID",
        "charName",
        "raceID",
        "raceName",
        "bloodlineID",
        "bloodlineName",
        "gender",
        "corporationID",
        "createDateTime",
        // What Trinity's character sheet shows of how the character was made.
        "ancestryID",
        "schoolID",
        "careerID",
        "careerSpecialityID",
    ];
    let Some(r) = db.table("character_").get_id(char_id) else { return empty_row(HEADER) };
    // A character stored without its race (0) has its bloodline's.
    let bloodline = bloodline_of(&r, "bloodlineID");
    let race_id = r.int("raceID").filter(|v| *v != 0).or_else(|| bloodline.and_then(|b| b.int("raceID")));
    let race = race_id.and_then(|id| crate::sde::table("chrRaces").get("raceID", id));
    row(
        HEADER,
        vec![
            int_col(&r, "characterID"),
            txt_col(&r, "characterName"),
            race_id.map_or(Value::None, Value::Int),
            static_txt(race, "raceName"),
            int_col(&r, "bloodlineID"),
            static_txt(bloodline, "bloodlineName"),
            int_col(&r, "gender"),
            int_col(&r, "corporationID"),
            long_col(&r, "createDateTime"),
            int_col(&r, "ancestryID"),
            int_col(&r, "schoolID"),
            int_col(&r, "careerID"),
            int_col(&r, "careerSpecialityID"),
        ],
    )
}

fn public_info3(db: &Db, char_id: i64) -> Value {
    const HEADER: &[&str] = &["description", "startDateTime", "bounty"];
    let lines = db
        .table("character_")
        .get_id(char_id)
        .map(|r| {
            let since = r.int("corporationDateTime").filter(|t| *t > 0).or(r.int("startDateTime"));
            Value::List(vec![txt_col(&r, "description"), since.map_or(Value::None, Value::Long), dbl_col(&r, "bounty")])
        })
        .into_iter()
        .collect();
    rowset(HEADER, lines)
}

fn private_info(db: &Db, char_id: i64) -> Value {
    const HEADER: &[&str] = &[
        "characterID",
        "charName",
        "corporationID",
        "title",
        "description",
        "balance",
        "bounty",
        "securityRating",
        "logonMinutes",
        "createDateTime",
        "startDateTime",
        "intelligence",
        "memory",
        "charisma",
        "perception",
        "willpower",
    ];
    let Some(r) = db.table("character_").get_id(char_id) else { return empty_row(HEADER) };
    // The attributes shown are the bloodline's starting ones.
    let bloodline = bloodline_of(&r, "bloodlineID");
    let attribute = |column: &str| bloodline.and_then(|b| b.int(column)).map_or(Value::None, Value::Int);
    row(
        HEADER,
        vec![
            int_col(&r, "characterID"),
            txt_col(&r, "characterName"),
            int_col(&r, "corporationID"),
            txt_col(&r, "title"),
            txt_col(&r, "description"),
            dbl_col(&r, "balance"),
            dbl_col(&r, "bounty"),
            dbl_col(&r, "securityRating"),
            int_col(&r, "logonMinutes"),
            long_col(&r, "createDateTime"),
            long_col(&r, "startDateTime"),
            attribute("intelligence"),
            attribute("memory"),
            attribute("charisma"),
            attribute("perception"),
            attribute("willpower"),
        ],
    )
}

fn character_info(ctx: &Ctx<'_>) -> Value {
    const HEADER: &[&str] = &["characterID", "characterName"];
    let Some(user_id) = ctx.session.user_id else { return rowset(HEADER, Vec::new()) };
    ctx.store.read(|db| {
        let mut characters: Vec<Row> = db.table("character_").find(&["accountID"], &[Cell::Int(user_id)]).collect();
        characters.sort_by_key(|r| r.int("characterID"));
        rowset(HEADER, characters.iter().map(|r| Value::List(vec![int_col(r, "characterID"), txt_col(r, "characterName")])).collect())
    })
}

fn top_bounties(db: &Db) -> Value {
    const HEADER: &[&str] = &["characterID", "ownerName", "bounty", "online"];
    let mut wanted: Vec<Row> = db.table("character_").rows().filter(|r| r.float("bounty").is_some_and(|b| b > 0.0)).collect();
    wanted.sort_by(|a, b| b.float("bounty").unwrap_or(0.0).total_cmp(&a.float("bounty").unwrap_or(0.0)));
    wanted.truncate(10);
    let lines = wanted
        .iter()
        .map(|r| Value::List(vec![int_col(r, "characterID"), txt_col(r, "characterName"), dbl_col(r, "bounty"), Value::Int(0)]))
        .collect();
    rowset(HEADER, lines)
}

fn character_description(db: &Db, char_id: i64) -> Value {
    const HEADER: &[&str] = &["description"];
    match db.table("character_").get_id(char_id) {
        Some(r) => row(HEADER, vec![txt_col(&r, "description")]),
        None => Value::None,
    }
}

fn set_character_description(call: &Call, ctx: &Ctx<'_>) -> Value {
    let Some(char_id) = ctx.session.char_id else { return Value::None };
    let text = match call.args.first() {
        Some(Value::Bytes(b)) | Some(Value::Buffer(b)) => String::from_utf8_lossy(b).into_owned(),
        Some(Value::Unicode(s)) => s.clone(),
        _ => String::new(),
    };
    let text: String = text.chars().take(4000).collect();
    let result = ctx.store.write(|tx| {
        let Some(row) = tx.table("character_").get_id(char_id).map(|r| r.id) else { return Ok(()) };
        tx.update("character_", row, &[("description", Cell::Text(text.into()))])
    });
    if let Err(e) = result {
        error!("character.SetCharacterDescription({char_id}): {err}", err = crate::logging::chain(&e));
    }
    Value::None
}

fn clone_type_id(ctx: &Ctx<'_>) -> Value {
    let Some(char_id) = ctx.session.char_id else { return Value::None };
    let clones = crate::sde::get().types_in_group(23);
    ctx.store.read(|db| {
        let best = db
            .table("entity")
            .find(&["ownerID"], &[Cell::Int(char_id)])
            .filter_map(|e| e.int("typeID"))
            .filter(|t| clones.contains(&(*t as i32)))
            .max();
        Value::int_or_none(best)
    })
}
