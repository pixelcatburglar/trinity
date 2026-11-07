//! Corporation titles: sixteen named role bundles per corporation
//! (`crpTitles`), each a single bit of a member's `character_.titleMask`.
//!
//! corpRegistry.GetTitles / UpdateTitle / UpdateTitles / DeleteTitle
//! (bco_titles.py:56-85). The client names an untitled slot itself
//! (bco_titles.py:62-94), so all sixteen are always listed. A member holds the
//! roles of every title in their mask on top of their own
//! ([`effective_roles`]).

use eve_marshal::Value;
use tracing::info;

use crate::services::Ctx;
use crate::session::CORP_ROLE_DIRECTOR;
use crate::store::{self, Db, Tx};
use crate::svc::membership::{access_denied, Refused, Undone};
use crate::svc::rows::{change, index_rowset};

pub const TITLE_HEADER: &[&str] = &[
    "titleID", "titleName", "roles", "grantableRoles", "rolesAtHQ", "grantableRolesAtHQ",
    "rolesAtBase", "grantableRolesAtBase", "rolesAtOther", "grantableRolesAtOther",
];

/// The role columns of a title, in `TITLE_HEADER` order after the name.
const ROLE_COLUMNS: [&str; 8] = [
    "roles", "grantableRoles", "rolesAtHQ", "grantableRolesAtHQ",
    "rolesAtBase", "grantableRolesAtBase", "rolesAtOther", "grantableRolesAtOther",
];

/// The titleIDs: one bit each, 1 to 32768 (bco_titles.py:62-94).
const TITLE_COUNT: u32 = 16;

fn title_ids() -> impl Iterator<Item = i64> {
    (0..TITLE_COUNT).map(|bit| 1i64 << bit)
}

fn is_title_id(id: i64) -> bool {
    title_ids().any(|t| t == id)
}

/// Every bit any title may hold.
pub const ALL_TITLES: i64 = (1 << TITLE_COUNT) - 1;

/// One title's name and role masks; an unset slot is unnamed with no roles.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Title {
    name: String,
    roles: [i64; 8],
}

impl Title {
    fn line(&self, id: i64) -> Vec<Value> {
        let mut line = vec![Value::Long(id), Value::text(self.name.clone())];
        line.extend(self.roles.iter().map(|r| Value::Long(*r)));
        line
    }
}

fn title(db: &Db, corp: i64, id: i64) -> Title {
    match db.table("crpTitles").get(&[store::Value::Int(corp), store::Value::Int(id)]) {
        Some(r) => Title {
            name: r.text("titleName").unwrap_or_default().to_owned(),
            roles: ROLE_COLUMNS.map(|c| r.int(c).unwrap_or(0)),
        },
        None => Title::default(),
    }
}

/// OR of the role masks (in `ROLE_COLUMNS` order) of the titles in `mask`.
pub fn granted(db: &Db, corp: i64, mask: i64) -> [i64; 8] {
    let mut out = [0i64; 8];
    for id in title_ids().filter(|id| mask & id != 0) {
        for (o, r) in out.iter_mut().zip(title(db, corp, id).roles) {
            *o |= r;
        }
    }
    out
}

/// A member's roles at all, at HQ, at base and elsewhere: their own and their
/// titles'.
pub fn effective_roles(db: &Db, char_id: i64) -> [i64; 4] {
    let own = db.table("chrCorporationRoles").get_id(char_id);
    let role = |column: &str| own.and_then(|r| r.int(column)).unwrap_or(0);
    let mut roles = [role("rolesAtAll"), role("rolesAtHQ"), role("rolesAtBase"), role("rolesAtOther")];
    let member = db.table("character_").get_id(char_id);
    let (corp, mask) = (member.and_then(|c| c.int("corporationID")), member.and_then(|c| c.int("titleMask")).unwrap_or(0));
    if let Some(corp) = corp.filter(|_| mask & ALL_TITLES != 0) {
        let t = granted(db, corp, mask);
        // roles, rolesAtHQ, rolesAtBase, rolesAtOther.
        for (r, g) in roles.iter_mut().zip([t[0], t[2], t[4], t[6]]) {
            *r |= g;
        }
    }
    roles
}

/// GetTitles(): an IndexRowset of all sixteen titles, keyed on titleID.
pub async fn get_titles(ctx: &Ctx<'_>) -> Value {
    let corp = ctx.session.corp_id.unwrap_or(0);
    let items = ctx.store.read(|db| {
        title_ids().map(|id| (Value::Long(id), Value::List(title(db, corp, id).line(id)))).collect()
    });
    index_rowset(TITLE_HEADER, "titleID", items)
}

/// What one UpdateTitle asks for: None leaves a value as it is.
struct Wanted {
    id: i64,
    name: Option<String>,
    roles: [Option<i64>; 8],
}

impl Wanted {
    /// [titleID, titleName, roles, grantableRoles, ... grantableRolesAtOther].
    fn parse(values: &[Value]) -> Refused<Self> {
        let id = values.first().and_then(Value::as_i64).filter(|id| is_title_id(*id));
        let Some(id) = id else { return Err(access_denied("that is not one of the corporation's titles")) };
        let name = values.get(1).and_then(Value::as_string);
        let mut roles = [None; 8];
        for (i, r) in roles.iter_mut().enumerate() {
            *r = match values.get(i + 2) {
                None | Some(Value::None) => None,
                Some(v) => Some(v.as_i64().ok_or_else(|| access_denied("a title's roles must be role masks"))?),
            };
        }
        Ok(Wanted { id, name, roles })
    }
}

fn may_manage(ctx: &Ctx<'_>, db: &Db) -> Refused<i64> {
    let corp = ctx.session.corp_id.unwrap_or(0);
    let me = ctx.session.char_id.unwrap_or(0);
    if !crate::svc::membership::is_player_corp(db, corp)? {
        return Err(access_denied("only player corporations have titles to manage"));
    }
    let ceo = db.table("corporation").get_id(corp).and_then(|c| c.int("ceoID"));
    // Directorship is held personally (as with UpdateMember): read it from
    // the member's own roles, not from titles.
    let own = db.table("chrCorporationRoles").get_id(me).and_then(|r| r.int("rolesAtAll")).unwrap_or(0);
    if ceo != Some(me) && own & CORP_ROLE_DIRECTOR == 0 {
        return Err(access_denied("only the CEO and directors may manage titles"));
    }
    Ok(corp)
}

/// A title as it was and as it is now.
type Changed = (i64, Title, Title);

/// Write the titles asked for, inside the caller's transaction.
fn apply(tx: &mut Tx<'_>, corp: i64, wanted: Vec<Wanted>) -> Result<Vec<Changed>, Undone> {
    if wanted.len() > TITLE_COUNT as usize {
        return Err(access_denied("a corporation has sixteen titles").into());
    }
    let mut changed = Vec::new();
    for w in wanted {
        let old = title(tx.db(), corp, w.id);
        let mut new = old.clone();
        if let Some(name) = w.name {
            let name = name.trim().to_owned();
            if name.chars().count() > 100 {
                return Err(access_denied("a title's name is at most 100 characters").into());
            }
            new.name = name;
        }
        for (r, w) in new.roles.iter_mut().zip(w.roles) {
            if let Some(w) = w {
                *r = w;
            }
        }
        // Directorship is given to members one by one, by the CEO.
        if new.roles.iter().any(|r| r & CORP_ROLE_DIRECTOR != 0) {
            return Err(access_denied("titles cannot grant the Director role").into());
        }
        if new == old {
            continue;
        }
        let mut sets: Vec<(&str, store::Value)> = vec![("titleName", store::Value::Text(new.name.as_str().into()))];
        sets.extend(ROLE_COLUMNS.iter().zip(new.roles).map(|(c, r)| (*c, store::Value::Int(r))));
        let mut insert = vec![("corporationID", store::Value::Int(corp)), ("titleID", store::Value::Int(w.id))];
        insert.extend(sets.iter().cloned());
        tx.upsert("crpTitles", &["corporationID", "titleID"], &insert, &sets)?;
        changed.push((w.id, old, new));
    }
    Ok(changed)
}

/// After the commit: tell the corporation what changed (OnTitleChanged,
/// bco_titles.py:20; OnCorporationMemberChanged for masks, bco_members.py)
/// and send a fresh session to each member whose roles came from a changed
/// title.
fn notify(ctx: &Ctx<'_>, corp: i64, changed: &[Changed], unassigned: &[(i64, i64, i64)]) {
    if changed.is_empty() && unassigned.is_empty() {
        return;
    }
    let me = ctx.session.char_id.unwrap_or(0);
    let mask = changed.iter().fold(0, |m, (id, ..)| m | id);
    let mut holders: Vec<i64> = ctx.store.read(|db| {
        db.table("character_")
            .find(&["corporationID"], &[store::Value::Int(corp)])
            .filter(|c| c.int("titleMask").unwrap_or(0) & mask != 0)
            .filter_map(|c| c.int("characterID"))
            .collect()
    });
    holders.extend(unassigned.iter().map(|(id, ..)| *id));
    holders.sort_unstable();
    holders.dedup();
    let w = ctx.world.lock();
    let members = w.corp_members(corp);
    for (id, old, new) in changed {
        let diff = change(TITLE_HEADER, Some(&old.line(*id)), Some(&new.line(*id)));
        w.notify(&members, "OnTitleChanged", Value::Tuple(vec![Value::Int(corp), Value::Long(*id), diff]));
        info!("{me} updated title {id} of corporation {corp}");
    }
    for (char_id, old, new) in unassigned {
        let diff = Value::Dict(vec![(Value::bytes("titleMask"), Value::Tuple(vec![Value::Long(*old), Value::Long(*new)]))]);
        w.notify(&members, "OnCorporationMemberChanged", Value::Tuple(vec![Value::Int(*char_id), diff]));
    }
    w.reaffiliate(&[], &holders);
}

async fn write_titles(ctx: &Ctx<'_>, wanted: Vec<Wanted>) -> Refused<Value> {
    let (corp, changed) = ctx
        .store
        .write(|tx| -> Result<_, Undone> {
            let corp = may_manage(ctx, tx.db())?;
            Ok((corp, apply(tx, corp, wanted)?))
        })
        .map_err(|Undone(o)| o)?;
    notify(ctx, corp, &changed, &[]);
    Ok(Value::None)
}

/// UpdateTitle(titleID, titleName, roles, grantableRoles, rolesAtHQ,
/// grantableRolesAtHQ, rolesAtBase, grantableRolesAtBase, rolesAtOther,
/// grantableRolesAtOther) (bco_titles.py:76).
pub async fn update_title(ctx: &Ctx<'_>, args: &[Value]) -> Refused<Value> {
    write_titles(ctx, vec![Wanted::parse(args)?]).await
}

/// UpdateTitles(rows): a list of UpdateTitle argument lists
/// (corp_ui_member_titles.py:391-394).
pub async fn update_titles(ctx: &Ctx<'_>, rows: Option<&Value>) -> Refused<Value> {
    let rows = rows.and_then(Value::seq).unwrap_or(&[]);
    let wanted = rows
        .iter()
        .map(|r| r.seq().ok_or_else(|| access_denied("each title is a list of its values")).and_then(Wanted::parse))
        .collect::<Refused<Vec<_>>>()?;
    write_titles(ctx, wanted).await
}

/// DeleteTitle(titleID): the slot goes back to unnamed with no roles, and
/// nobody holds it any more.
pub async fn delete_title(ctx: &Ctx<'_>, title_id: i64) -> Refused<Value> {
    if !is_title_id(title_id) {
        return Err(access_denied("that is not one of the corporation's titles"));
    }
    let (corp, changed, unassigned) = ctx
        .store
        .write(|tx| -> Result<_, Undone> {
            let corp = may_manage(ctx, tx.db())?;
            let held: Vec<(store::RowId, i64, i64)> = tx
                .table("character_")
                .find(&["corporationID"], &[store::Value::Int(corp)])
                .filter_map(|c| Some((c.id, c.int("characterID")?, c.int("titleMask").unwrap_or(0))))
                .filter(|(.., mask)| mask & title_id != 0)
                .collect();
            let mut unassigned = Vec::new();
            for (row, char_id, mask) in held {
                tx.update("character_", row, &[("titleMask", store::Value::Int(mask & !title_id))])?;
                unassigned.push((char_id, mask, mask & !title_id));
            }
            let reset = Wanted { id: title_id, name: Some(String::new()), roles: [Some(0); 8] };
            Ok((corp, apply(tx, corp, vec![reset])?, unassigned))
        })
        .map_err(|Undone(o)| o)?;
    notify(ctx, corp, &changed, &unassigned);
    Ok(Value::None)
}

/// The titleMask an UpdateMember asks to set, checked: only the bits of the
/// sixteen titles.
pub fn checked_mask(mask: i64) -> Refused<i64> {
    if mask & !ALL_TITLES != 0 || mask < 0 {
        return Err(access_denied("that is not a set of the corporation's titles"));
    }
    Ok(mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_are_single_bits() {
        assert_eq!(title_ids().count(), 16);
        assert!(is_title_id(1) && is_title_id(32768));
        assert!(!is_title_id(0) && !is_title_id(3) && !is_title_id(65536));
        assert_eq!(ALL_TITLES, 0xFFFF);
    }

    #[test]
    fn masks_outside_the_titles_are_refused() {
        assert_eq!(checked_mask(0b1010).ok(), Some(0b1010));
        assert!(checked_mask(1 << 16).is_err());
        assert!(checked_mask(-1).is_err());
    }

    #[test]
    fn update_title_arguments_parse() {
        let mut args = vec![Value::Long(4), Value::bytes("Hauler")];
        args.extend((0..8).map(|i| if i == 3 { Value::None } else { Value::Long(i) }));
        let w = Wanted::parse(&args).ok().unwrap();
        assert_eq!((w.id, w.name.as_deref()), (4, Some("Hauler")));
        assert_eq!(w.roles, [Some(0), Some(1), Some(2), None, Some(4), Some(5), Some(6), Some(7)]);
        assert!(Wanted::parse(&[Value::Long(3)]).is_err());
        let mut bad = args.clone();
        bad[2] = Value::bytes("x");
        assert!(Wanted::parse(&bad).is_err());
    }

    #[test]
    fn title_lines_follow_the_header() {
        let t = Title { name: "Hauler".into(), roles: [1, 2, 3, 4, 5, 6, 7, 8] };
        let line = t.line(8);
        assert_eq!(line.len(), TITLE_HEADER.len());
        assert_eq!(line[0], Value::Long(8));
        assert_eq!(line[9], Value::Long(8));
    }
}
