use eve_marshal::Value;
use tracing::error;

use crate::sde;
use crate::services::{Call, Ctx, Outcome};
use crate::store::{StoreError, Tx, Value as Cell};
use crate::svc::rows::{key_val, rowset};

pub async fn tutorial_svc(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let arg = |i: usize| call.arg_i64(i);
    let value = match call.method.as_str() {
        "GetTutorials" => get_tutorials(),
        "GetCriterias" => get_criterias(),
        "GetTutorialInfo" => tutorial_info(arg(0).unwrap_or(0)),
        "GetTutorialVersionInfo" => tutorial_info(arg(0).unwrap_or(0)),

        "LogAppClosed" | "LogClosed" | "LogAborted" | "LogCompleted" => return Some(log_progress(call,ctx).await),
        "GetCharacterTutorialState" => character_state(ctx),

        _ => return None,
    };
    Some(Outcome::Value(value))
}

/// The tutorial progress table is the store's own now: nothing to make.
pub fn ensure(_tx: &mut Tx<'_>) -> Result<(), StoreError> {
    Ok(())
}

/// Where the character is in each tutorial, the longest untouched first.
fn character_state(ctx: &Ctx<'_>) -> Value {
    const HEADER: &[&str] = &["tutorialID", "pageID", "eventTypeID"];
    let character = ctx.session.char_id.unwrap_or(0);
    let lines = ctx.store.read(|db| {
        let mut rows: Vec<_> = db.table("chrTutorialState").find(&["characterID"], &[Cell::Int(character)]).collect();
        rows.sort_by_key(|r| (r.int("updatedAt"), r.int("tutorialID")));
        rows.iter().map(|r| Value::List(r.line(HEADER))).collect()
    });
    rowset(HEADER, lines)
}

async fn log_progress(call:&Call,ctx:&Ctx<'_>) -> Outcome {
    let Some(character)=ctx.session.char_id else {return Outcome::custom("Select a character first.")};
    let Some(tutorial)=call.arg_i64(0).and_then(|id|i32::try_from(id).ok()).filter(|id|*id>0) else {return Outcome::custom("Invalid tutorial.")};
    let Some(page)=call.arg_i64(1).and_then(|id|i32::try_from(id).ok()).filter(|id|*id>=0) else {return Outcome::custom("Invalid tutorial page.")};
    let elapsed=call.arg_i64(2).unwrap_or(0).max(0);
    // tutorialsvc.py: 155 finishes a sequence, 158 disables tutorials. The
    // original event IDs for both resumable close events are unavailable;
    // 0 represents their shared client-visible nonterminal behavior.
    let event=match call.method.as_str(){"LogCompleted"=>155i32,"LogAborted"=>158,_=>0};
    let now=crate::bulkdata::filetime_now();
    let result=ctx.store.write(|tx| {
        let key=[Cell::Int(character),Cell::Int(i64::from(tutorial))];
        // A close after a finish or a disable keeps that ending.
        let kept=|old:Option<i64>| match old {Some(e@(155|158)) if event==0=>e,_=>i64::from(event)};
        match tx.table("chrTutorialState").get(&key).map(|r|(r.id,r.int("eventTypeID"))) {
            Some((row,old))=>tx.update("chrTutorialState",row,&[("pageID",Cell::Int(i64::from(page))),("eventTypeID",Cell::Int(kept(old))),
                ("elapsedSeconds",Cell::Int(elapsed)),("updatedAt",Cell::Int(now))]),
            None=>tx.insert("chrTutorialState",&[("characterID",key[0].clone()),("tutorialID",key[1].clone()),("pageID",Cell::Int(i64::from(page))),
                ("eventTypeID",Cell::Int(i64::from(event))),("elapsedSeconds",Cell::Int(elapsed)),("updatedAt",Cell::Int(now))]).map(|_|()),
        }
    });
    match result {
        Ok(_)=>Outcome::Value(Value::None),
        Err(e)=>{error!("saving tutorial progress: {}",crate::logging::chain(&e));Outcome::custom("Tutorial progress could not be saved.")}
    }
}

/// Trinity tutorial IDs this data has no tutorial for, and the tutorial whose
/// content stands in. Trinity starts new pilots at tutorial 109 (uix.tutorial);
/// the data here is the 2006 sequence, which starts at 5 (the station Aura
/// tutorial), so 109 shows 5's pages and continues where 5 does.
const ALIASES: &[(i32, i32)] = &[(109, 5)];

fn alias_source(id: i32) -> i32 {
    ALIASES.iter().find(|(a, _)| *a == id).map_or(id, |(_, s)| *s)
}

/// A tutorial's row under the ID it is asked by.
fn tutorial_line(id: i32, tutorial: &sde::Row) -> Value {
    let mut line = vec![Value::Int(id as i64)];
    line.extend(tutorial.line(&["tutorialVName", "nextTutorialID"]));
    Value::List(line)
}

/// Every tutorial, and each alias the data has no tutorial for under its
/// source's name and successor, by ID.
fn get_tutorials() -> Value {
    const HEADER: &[&str] = &["tutorialID", "tutorialVName", "nextTutorialID"];
    let tutorials = sde::table("tutorials");
    let mut found: Vec<(Option<i64>, Value)> =
        tutorials.rows().map(|t| (t.int("tutorialID"), Value::List(t.line(HEADER)))).collect();
    for (alias, source) in ALIASES {
        if tutorials.get("tutorialID", *alias as i64).is_some() {
            continue;
        }
        found.extend(tutorials.find("tutorialID", *source as i64).map(|t| (Some(*alias as i64), tutorial_line(*alias, &t))));
    }
    found.sort_by_key(|(id, _)| *id);
    rowset(HEADER, found.into_iter().map(|(_, line)| line).collect())
}

fn get_criterias() -> Value {
    const HEADER: &[&str] = &["criteriaID", "criteriaName", "messageText", "audioPath"];
    let mut found: Vec<sde::Row> = sde::table("tutorial_criteria").rows().collect();
    found.sort_by_key(|r| r.int("criteriaID"));
    rowset(HEADER, found.iter().map(|r| Value::List(r.line(HEADER))).collect())
}

fn tutorial_info(tutorial_id: i64) -> Value {
    const TUTORIAL_HEADER: &[&str] = &["tutorialID", "tutorialVName", "nextTutorialID"];
    const PAGE_HEADER: &[&str] =
        &["pageID", "pageNumber", "pageName", "text", "imagePath", "audioPath"];
    const CRITERIA_HEADER: &[&str] = &["criteriaID"];
    const PAGE_CRITERIA_HEADER: &[&str] = &["pageID", "criteriaID"];

    // An aliased tutorial shows its source's pages under its own ID.
    let tutorials = sde::table("tutorials");
    let asked = tutorial_id as i32;
    let id = if tutorials.get("tutorialID", asked as i64).is_some() {
        asked
    } else {alias_source(asked)};

    let tutorial: Vec<Value> = tutorials.find("tutorialID", id as i64).map(|t| tutorial_line(asked, &t)).collect();
    if tutorial.is_empty() {
        return Value::None;
    }

    let mut pages: Vec<sde::Row> = sde::table("tutorial_pages").find("tutorialID", id as i64).collect();
    pages.sort_by_key(|p| p.int("pageNumber"));

    let mut criterias: Vec<sde::Row> = sde::table("tutorials_criterias").find("tutorialID", id as i64).collect();
    criterias.sort_by_key(|c| c.int("criteriaID"));

    // The criteria of the tutorial's pages, by page number, then criterion.
    let page_criteria = sde::table("tutorial_page_criteria");
    let mut pagecriterias: Vec<(Option<i64>, Option<i64>, Value)> = pages
        .iter()
        .flat_map(|p| {
            let found = p.int("pageID").into_iter().flat_map(|id| page_criteria.find("pageID", id));
            found.map(|pc| (p.int("pageNumber"), pc.int("criteriaID"), Value::List(pc.line(PAGE_CRITERIA_HEADER))))
        })
        .collect();
    pagecriterias.sort_by_key(|(page, criterion, _)| (*page, *criterion));

    key_val(vec![
        ("tutorial", rowset(TUTORIAL_HEADER, tutorial)),
        ("pages", rowset(PAGE_HEADER, pages.iter().map(|p| Value::List(p.line(PAGE_HEADER))).collect())),
        ("criterias", rowset(CRITERIA_HEADER, criterias.iter().map(|c| Value::List(c.line(CRITERIA_HEADER))).collect())),
        ("pagecriterias", rowset(PAGE_CRITERIA_HEADER, pagecriterias.into_iter().map(|(_, _, line)| line).collect())),
    ])
}
