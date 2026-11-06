use std::collections::HashMap;

use eve_marshal::Value;

use crate::services::{Call, Ctx, Outcome};
use crate::svc::{agent_missions, bound};
use crate::svc::rows::{key_val, rowset};

const HEADER: &[&str] =
    &["agentID", "divisionID", "corporationID", "stationID", "level", "quality", "agentTypeID",
      "bloodlineID"];

pub async fn agent_mgr(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        "GetAgents" => get_agents(ctx).await,
        // All the pilot's missions, or (bound) one agent's.
        "GetMyJournalDetails" => agent_missions::journal(ctx, agent_of(call)).await,
        // Bound per agent: moniker.GetAgent(agentID) (moniker.py:530).
        "DoAction" => agent_missions::conversation(ctx, agent_of(call)?, action_id(call)).await,
        "GetInfoServiceDetails" => info_service_details(ctx, agent_of(call)?).await,
        "GetMissionJournalInfo" => agent_missions::mission_page(ctx, agent_of(call)?).await,
        // Offers are the loyalty point store's, which there is none of.
        "GetOfferJournalInfo" => Value::None,
        // (locationType, locationNumber, warpRange, gang) (menusvc.py:1663).
        "WarpToLocation" => {
            let range = call.args.get(2).and_then(|v| v.as_f64()).unwrap_or(15_000.0);
            return Some(agent_missions::go_to(ctx, call.arg_id(1), range, false).await);
        }
        "GotoLocation" => return Some(agent_missions::go_to(ctx, call.arg_id(1), 0.0, true).await),
        _ => return None,
    };
    Some(Outcome::Value(value))
}

fn agent_of(call: &Call) -> Option<i64> {
    bound::parse(&call.service).and_then(|b| b.params.first().copied())
}

/// DoAction(actionID): None opens the conversation. The agent window's links
/// pass the ID back as text (agents.py:273), so accept either form.
fn action_id(call: &Call) -> Option<i64> {
    call.arg_i64(0).or_else(|| call.arg_text(0)?.trim().parse().ok())
}

/// The agent's Show Info panel (infosvc.py:1404-1429). Effective quality is the
/// plain quality: the skill and standing adjustments are not modelled.
async fn info_service_details(ctx: &Ctx<'_>, agent_id: i64) -> Value {
    let Some(r) = crate::sde::table("agtAgents").get("agentID", (agent_id as i32).into()) else { return Value::None };
    let quality = Value::int_or_none(r.int("quality"));
    let me = ctx.session.char_id.unwrap_or(0) as i32;
    let lp = crate::svc::lp::lp(ctx, me, r.int("corporationID").unwrap_or(0) as i32).await;
    key_val(vec![
        ("stationID", Value::int_or_none(r.int("stationID"))),
        ("level", Value::int_or_none(r.int("level"))),
        ("quality", quality.clone()),
        ("effectiveQuality", quality),
        // The pilot's LP with the agent's corporation.
        ("loyaltyPoints", Value::Int(lp)),
        ("services", Value::List(Vec::new())),
        ("incompatible", Value::None),
    ])
}

async fn get_agents(ctx: &Ctx<'_>) -> Value {
    let mut agents: Vec<crate::sde::Row> = crate::sde::table("agtAgents").rows().collect();
    agents.sort_by_key(|a| a.int("agentID"));
    // An agent is also a character; its bloodline lives on that character
    // row (the agent window reads a.bloodlineID, agents.py:289).
    let bloodlines: HashMap<i64, Value> = ctx.store.read(|db| {
        let characters = db.table("character_");
        agents
            .iter()
            .filter_map(|a| a.int("agentID"))
            .filter_map(|id| Some((id, characters.get_id(id)?.cell("bloodlineID"))))
            .collect()
    });
    let lines = agents
        .iter()
        .map(|a| {
            // The agent's own columns, then its character's bloodline (None
            // for an agent without a character row).
            let mut line = a.line(&HEADER[..7]);
            line.push(a.int("agentID").and_then(|id| bloodlines.get(&id)).cloned().unwrap_or(Value::None));
            Value::List(line)
        })
        .collect();
    rowset(HEADER, lines)
}
