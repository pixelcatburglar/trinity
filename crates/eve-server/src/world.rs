//! State shared by every connection: who is in the game, which chat channels
//! they have open, the solar-system ballparks, and a queue per connection for
//! notifications (and session moves) the rest of the server sends it.
//!
//! One lock guards it all, so a change that touches several players (a ship
//! leaving a system, a chat line) is seen by everyone in the same order. Nothing
//! awaits while holding it: notifications are queued, not sent.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use eve_marshal::Value;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::ballpark::Park;

/// A notification for one client: `event(*args)` on its side.
#[derive(Debug, Clone)]
pub struct Notification {
    pub event: &'static str,
    /// The positional arguments, as a tuple.
    pub args: Value,
}

/// Something that happened to a character which its connection has to carry
/// into its session, because the client did not ask for it.
#[derive(Debug, Clone, PartialEq)]
pub enum Move {
    /// The ship was destroyed; the pilot is now in `ship_id`, a capsule, in
    /// the same system.
    ShipLost { ship_id: i64 },
    /// The capsule was destroyed; the pilot wakes in a clone at a station, in a
    /// new capsule if one could be made.
    Podded { station_id: i64, solar_system_id: i64, constellation_id: i64, region_id: i64, ship_id: Option<i64> },
    /// The character's corporation, its alliance or its roles changed; the
    /// connection reads them again.
    Affiliation,
    /// The character joined, left or now leads (or no longer leads) a gang.
    Gang { gang_id: Option<i64>, leader: bool },
    Fleet { gang_id: Option<i64>, role: i64, wing_id: Option<i64>, squad_id: Option<i64> },
    /// A GM's /tr: into a station, or to a point in a system's space (only
    /// from space).
    Teleport { station_id: Option<i64>, solar_system_id: i64, constellation_id: i64, region_id: i64, pos: (f64, f64, f64) },
    /// The pilot is somewhere else, in another ship, already put there (a
    /// clone jump, a jump through a bridge): docked in `station_id`, or in
    /// space in the system.
    Relocate { station_id: Option<i64>, solar_system_id: i64, constellation_id: i64, region_id: i64, ship_id: i64 },
}

/// A call the server makes on one of the client's own services; the answer
/// (None if the client raised, or went away) comes back on `reply`.
#[derive(Debug)]
pub struct ClientCall {
    pub service: &'static str,
    pub method: &'static str,
    pub args: Vec<Value>,
    pub reply: tokio::sync::oneshot::Sender<Option<Value>>,
}

/// What a connection's queue carries, in the order it was queued.
#[derive(Debug)]
pub enum Push {
    Notify(Notification),
    Move(Move),
    Call(ClientCall),
}

/// Where one connected character is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Presence {
    pub char_id: i64,
    pub corp_id: i64,
    pub solar_system_id: i64,
    pub docked: bool,
    /// The station, when docked.
    pub station_id: Option<i64>,
    /// The session's role mask.
    pub role: i64,
}

struct Client {
    tx: UnboundedSender<Push>,
    presence: Option<Presence>,
    /// The system whose ballpark the client has been sent (SetState). Until
    /// then it has no ballpark, and a destiny update would fail in michelle.
    sees: Option<i64>,
    /// Ships in that ballpark the client has not been given, being off its
    /// grid (or cloaked): destiny updates about them are held back
    /// (`space::send`). Behind its own lock so a broadcast, which only reads
    /// the world, can keep it current.
    hidden: Mutex<HashSet<i64>>,
}

struct Channel {
    /// The channel ID as the client knows it.
    id: Value,
    members: HashSet<i64>,
}

#[derive(Default)]
pub struct WorldState {
    clients: HashMap<i64, Client>,
    /// Open chat channels, by `channel_key`.
    channels: HashMap<String, Channel>,
    /// Player trades under way, by trade container.
    pub trades: HashMap<i64, crate::svc::pvptrade::Trade>,
    /// Solar system -> its ballpark. Created when the first ship arrives.
    pub parks: HashMap<i64, Park>,
    /// Original ship configuration is transient and clears on leaving space.
    pub maintenance: HashMap<i64, crate::svc::maintenance::Configuration>,
    pub maintenance_repairs: HashMap<i64, crate::svc::maintenance::Repair>,
    pub pending_station_arrivals: HashMap<i64, crate::losses::pending::Pending>,
    pub pending_travel: HashMap<i64, crate::travel::Pending>,
    pub pending_portals: HashMap<i64, crate::svc::jumps::portal::Pending>,
    /// Disconnected hulls retained through an accepted cycle or reservation.
    pub retained_logouts: HashMap<i64,(i64,i64)>,
    pub pending_logouts: HashMap<i64,crate::conn::logout::Pending>,
    /// Accepted generator cycles outlive loss of a usable portal destination.
    pub portal_cycles: HashMap<i64, Bridge>,
    pub portal_retry_cursor: i64,
    pub travel_retry_cursor:i64,
    pub pending_station_arrival_cursor: i32,
    /// Confirmed space losses retained until their durable settlement succeeds.
    pub pending_ship_losses: HashMap<i64, crate::losses::Loss>,
    /// Round-robin durable loss scan, so quarantined records cannot starve later losses.
    pub pending_ship_loss_cursor: i32,
    /// Work produced by an atomically published first module cycle, drained by
    /// the heartbeat together with its own jobs.
    pub pending_jobs: Vec<crate::heartbeat::Job>,
    pub gangs: crate::svc::gang::Gangs,
    /// Skills injected since each client last refreshed its skill list
    /// (client -> (character, skill type)). The client's `skills` service
    /// keeps a snapshot it renews only when a skill finishes or it logs in,
    /// and its skill history crashes on a skill missing from it, so the
    /// history leaves these out until then (`skills::history::of`).
    pub skills_unseen: HashMap<i64, HashSet<(i64, i64)>>,
    /// The online bots by solar system: (in space, docked), for the star map.
    pub bot_counts: HashMap<i64, (i64, i64)>,
    /// How the bot population stands, for the web dashboard.
    pub bot_stats: BotStats,
    /// Every bot as Local shows it, by its index in the bot population.
    pub bot_roster: Arc<Vec<LocalBot>>,
    /// The online bots in each solar system, docked or not, as indexes into
    /// `bot_roster`: who is in Local there.
    pub bot_local: HashMap<i64, Vec<u32>>,
    /// Every bot's character, to its index in the bot population.
    pub bot_index: HashMap<i64, u32>,
    /// Offers to hire a bot (gang invitations to one): (player, bot), for
    /// the simulation to answer.
    pub hire_requests: Vec<(i64, i64)>,
    /// Clones offered in ships' clone vat bays: target -> (offerer, ship).
    pub clone_offers: HashMap<i64, (i64, i64)>,
    /// Titans' open jump portals, by titan.
    pub bridges: HashMap<i64, Bridge>,
    /// Portal startups whose source and publication slot are reserved.
    pub pending_bridges: HashSet<i64>,
    /// Scanner and survey probes out, by probe.
    pub probes: HashMap<i64, crate::svc::scan::Probe>,
    /// Each pilot's last probe analysis.
    pub scan_results: HashMap<i64, Vec<crate::svc::scan::Found>>,
    /// What the dashboard shows of the bots beyond `bot_stats`, as JSON.
    pub bot_panels: BotPanels,
}

/// A titan's jump portal, open to a member's field or an alliance beacon.
#[derive(Debug, Clone, PartialEq)]
pub struct Bridge {
    pub generation: i64,
    pub started_ms: i64,
    pub titan_pilot: i64,
    pub generator_id: i64,
    pub system: i64,
    pub to_system: i64,
    pub beacon: i64,
    /// None identifies a fixed alliance cynosural generator array.
    pub member: Option<i64>,
    pub until_ms: i64,
    /// Strontium a kilogram a light year, for each ship through.
    pub mass_factor: f64,
    /// Minimum original attribute1253; covert portals require two.
    pub harmonics: i64,
}

/// The bot simulation's dashboard windows, ready to send.
#[derive(Debug, Clone, Default)]
pub struct BotPanels {
    pub market: serde_json::Value,
    pub relations: serde_json::Value,
    pub followed: serde_json::Value,
    pub events: serde_json::Value,
}

/// A siege as the dashboard shows it.
#[derive(Debug, Clone)]
pub struct SiegeView {
    pub attacker: String,
    pub defender: String,
    pub system: String,
    pub region: String,
    pub security: f64,
    /// When it begins (began), FILETIME.
    pub starts: i64,
    /// Ships still fighting on each side, once it is being fought.
    pub fleets: Option<(usize, usize)>,
}

/// A bot as a Local channel lists it.
#[derive(Debug, Clone)]
pub struct LocalBot {
    pub char_id: i64,
    pub corp_id: i64,
    pub alliance_id: Option<i64>,
    pub name: String,
    pub type_id: i64,
}

/// The bot population at its last save.
#[derive(Debug, Clone, Default)]
pub struct BotStats {
    pub total: i64,
    pub online: i64,
    pub in_space: i64,
    pub corporations: i64,
    pub alliances: i64,
    pub wars: i64,
    /// Bots lost in fights since the server started.
    pub lost: u64,
    /// Bots lost in each bot war, by warID.
    pub war_losses: HashMap<i64, u32>,
    /// What happened in null-sec lately, oldest first: (FILETIME, what).
    pub news: Vec<(i64, String)>,
    /// Sieges announced or being fought.
    pub sieges: Vec<SiegeView>,
    pub running: bool,
}

/// A handle on the shared world; clones share it, for tasks that outlive a call.
#[derive(Default, Clone)]
pub struct World {
    state: Arc<Mutex<WorldState>>,
}

impl World {
    pub fn lock(&self) -> WorldGuard<'_> {
        let guard = self.state.lock().expect("world lock poisoned");
        WORLD_HELD.with(|h| h.set(h.get() + 1));
        WorldGuard { guard }
    }
}

thread_local! {
    /// How many world guards this thread holds.
    static WORLD_HELD: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Whether this thread holds the world lock. The store is never entered
/// while it does: a store write may lock the world (store first, then
/// world), so the other order could deadlock.
pub fn world_held() -> bool {
    WORLD_HELD.with(|h| h.get() > 0)
}

/// The locked world.
pub struct WorldGuard<'a> {
    guard: MutexGuard<'a, WorldState>,
}

impl Drop for WorldGuard<'_> {
    fn drop(&mut self) {
        WORLD_HELD.with(|h| h.set(h.get() - 1));
    }
}

impl std::ops::Deref for WorldGuard<'_> {
    type Target = WorldState;
    fn deref(&self) -> &WorldState {
        &self.guard
    }
}

impl std::ops::DerefMut for WorldGuard<'_> {
    fn deref_mut(&mut self) -> &mut WorldState {
        &mut self.guard
    }
}

impl WorldState {
    /// Registers a connection; notifications for it arrive on the receiver.
    pub fn connect(&mut self, client_id: i64) -> UnboundedReceiver<Push> {
        let (tx, rx) = unbounded_channel();
        self.clients.insert(client_id, Client { tx, presence: None, sees: None, hidden: Mutex::default() });
        rx
    }

    /// Forgets a connection and its chat memberships; returns each channel it
    /// was in with the members left, for the caller to announce the departure.
    pub fn disconnect(&mut self, client_id: i64) -> Vec<(Value, Vec<i64>)> {
        if let Some(char_id) = self.clients.get(&client_id).and_then(|c| c.presence).map(|p| p.char_id) {
            crate::svc::gang::logged_off(self, char_id);
        }
        self.clients.remove(&client_id);
        self.skills_unseen.remove(&client_id);
        let mut left = Vec::new();
        for channel in self.channels.values_mut() {
            if channel.members.remove(&client_id) {
                left.push((channel.id.clone(), channel.members.iter().copied().collect()));
            }
        }
        self.channels.retain(|_, c| !c.members.is_empty());
        left
    }

    pub fn set_presence(&mut self, client_id: i64, presence: Option<Presence>) {
        if let Some(c) = self.clients.get_mut(&client_id) {
            // Docked or gone elsewhere, its ballpark is gone with it.
            if presence.is_none_or(|p| p.docked || Some(p.solar_system_id) != c.sees) {
                c.sees = None;
                if let Ok(h) = c.hidden.get_mut() {
                    h.clear();
                }
            }
            c.presence = presence;
        }
    }

    /// The client has been sent the ballpark of `solar_system_id`.
    pub fn sees(&mut self, client_id: i64, solar_system_id: i64) {
        if let Some(c) = self.clients.get_mut(&client_id) {
            c.sees = Some(solar_system_id);
        }
    }

    /// Runs `f` on the ships hidden from `client_id`; None if it is not connected.
    pub fn with_hidden<R>(&self, client_id: i64, f: impl FnOnce(&mut HashSet<i64>) -> R) -> Option<R> {
        let c = self.clients.get(&client_id)?;
        let mut hidden = c.hidden.lock().unwrap_or_else(|e| e.into_inner());
        Some(f(&mut hidden))
    }

    /// The connections playing `char_id` (at most one, in practice).
    pub fn clients_of(&self, char_id: i64) -> Vec<i64> {
        self.clients
            .iter()
            .filter(|(_, c)| c.presence.is_some_and(|p| p.char_id == char_id))
            .map(|(id, _)| *id)
            .collect()
    }

    /// Where each player in the game is.
    pub fn players(&self) -> Vec<Presence> {
        self.clients.values().filter_map(|c| c.presence).collect()
    }

    /// Characters in the game: players logged in, and the bots online.
    pub fn online_count(&self) -> i64 {
        let players = self.clients.values().filter(|c| c.presence.is_some()).count() as i64;
        let bots: i64 = self.bot_counts.values().map(|(space, docked)| space + docked).sum();
        players + bots
    }

    /// Clients whose session holds every bit of `role`.
    pub fn with_role(&self, role: i64) -> Vec<i64> {
        self.clients.iter().filter(|(_, c)| c.presence.is_some_and(|p| p.role & role == role)).map(|(id, _)| *id).collect()
    }

    /// Where `char_id` is, if online.
    pub fn presence_of(&self, char_id: i64) -> Option<Presence> {
        self.clients.values().filter_map(|c| c.presence).find(|p| p.char_id == char_id)
    }

    /// The station `char_id` is docked in, if online and docked.
    pub fn station_of(&self, char_id: i64) -> Option<i64> {
        self.clients.values().filter_map(|c| c.presence).find(|p| p.char_id == char_id).and_then(|p| p.station_id)
    }

    pub fn is_online(&self, char_id: i64) -> bool {
        self.clients.values().any(|c| c.presence.is_some_and(|p| p.char_id == char_id))
    }

    /// Whether `char_id` is online and undocked.
    pub fn is_in_space(&self, char_id: i64) -> bool {
        self.clients.values().any(|c| c.presence.is_some_and(|p| p.char_id == char_id && !p.docked))
    }

    /// Characters per solar system: (in space, docked). Every occupied system has
    /// an in-space entry, if only 0: the star map iterates that dict's keys alone.
    pub fn counts(&self) -> (HashMap<i64, i64>, HashMap<i64, i64>) {
        let (mut space, mut docked) = (HashMap::new(), HashMap::new());
        for p in self.clients.values().filter_map(|c| c.presence) {
            let in_space = space.entry(p.solar_system_id).or_insert(0);
            if p.docked {
                *docked.entry(p.solar_system_id).or_insert(0) += 1;
            } else {
                *in_space += 1;
            }
        }
        for (system, (in_space, in_station)) in &self.bot_counts {
            *space.entry(*system).or_insert(0) += in_space;
            if *in_station > 0 {
                *docked.entry(*system).or_insert(0) += in_station;
            }
        }
        (space, docked)
    }

    /// Clients whose character belongs to `corp_id`.
    pub fn corp_members(&self, corp_id: i64) -> Vec<i64> {
        self.clients
            .iter()
            .filter(|(_, c)| c.presence.is_some_and(|p| p.corp_id == corp_id))
            .map(|(id, _)| *id)
            .collect()
    }

    /// Clients whose character is in space in `solar_system_id` and that
    /// have its ballpark: one just undocked or arrived gets what happened
    /// meanwhile in its SetState.
    pub fn in_space(&self, solar_system_id: i64) -> Vec<i64> {
        self.clients
            .iter()
            .filter(|(_, c)| {
                c.sees == Some(solar_system_id)
                    && c.presence.is_some_and(|p| p.solar_system_id == solar_system_id && !p.docked)
            })
            .map(|(id, _)| *id)
            .collect()
    }

    /// Queues `event(*args)` for each of `clients`. A client that has gone away
    /// is skipped.
    pub fn notify(&self, clients: &[i64], event: &'static str, args: Value) {
        for id in clients {
            if let Some(c) = self.clients.get(id) {
                let _ = c.tx.send(Push::Notify(Notification { event, args: args.clone() }));
            }
        }
    }

    /// Calls `service.method(*args)` on a client; the receiver yields its
    /// answer. None if the client is not connected.
    pub fn call(&self, client_id: i64, service: &'static str, method: &'static str, args: Vec<Value>) -> Option<tokio::sync::oneshot::Receiver<Option<Value>>> {
        let c = self.clients.get(&client_id)?;
        let (reply, answer) = tokio::sync::oneshot::channel();
        c.tx.send(Push::Call(ClientCall { service, method, args, reply })).ok()?;
        Some(answer)
    }

    /// Queues a session move for one client, after anything already queued.
    pub fn push_move(&self, client_id: i64, m: Move) {
        if let Some(c) = self.clients.get(&client_id) {
            let _ = c.tx.send(Push::Move(m));
        }
    }

    /// Tells the connections of everyone in `corp_ids`, and of `char_ids`,
    /// to read their corporation and alliance again.
    pub fn reaffiliate(&self, corp_ids: &[i64], char_ids: &[i64]) {
        let mut clients: Vec<i64> = corp_ids.iter().flat_map(|c| self.corp_members(*c)).collect();
        clients.extend(char_ids.iter().flat_map(|c| self.clients_of(*c)));
        clients.sort_unstable();
        clients.dedup();
        for client in clients {
            self.push_move(client, Move::Affiliation);
        }
    }

    /// Opens a chat channel for a client; returns everyone who has it open now.
    pub fn join_channel(&mut self, channel_id: &Value, client_id: i64) -> Vec<i64> {
        let channel = self
            .channels
            .entry(channel_key(channel_id))
            .or_insert_with(|| Channel { id: channel_id.clone(), members: HashSet::new() });
        channel.members.insert(client_id);
        channel.members.iter().copied().collect()
    }

    /// Closes a chat channel for a client; returns who still has it open.
    pub fn leave_channel(&mut self, channel_id: &Value, client_id: i64) -> Vec<i64> {
        let key = channel_key(channel_id);
        let Some(channel) = self.channels.get_mut(&key) else { return Vec::new() };
        channel.members.remove(&client_id);
        let rest = channel.members.iter().copied().collect();
        if channel.members.is_empty() {
            self.channels.remove(&key);
        }
        rest
    }

    /// The Local channels open: (solar system, the channel as the clients
    /// name it, who has it open).
    pub fn local_channels(&self) -> Vec<(i64, Value, Vec<i64>)> {
        self.channels
            .values()
            .filter_map(|c| Some((local_system(&c.id)?, c.id.clone(), c.members.iter().copied().collect())))
            .collect()
    }

    /// The character in the game on a connection.
    pub fn presence_of_client(&self, client_id: i64) -> Option<Presence> {
        self.clients.get(&client_id).and_then(|c| c.presence)
    }

    /// Everyone who has a chat channel open.
    pub fn channel_members(&self, channel_id: &Value) -> Vec<i64> {
        self.channels
            .get(&channel_key(channel_id))
            .map(|c| c.members.iter().copied().collect())
            .unwrap_or_default()
    }
}

/// The solar system of a Local channel, `[('solarsystemid2', id)]`.
pub fn local_system(channel_id: &Value) -> Option<i64> {
    let pair = channel_id.seq()?.first()?.seq()?;
    match pair {
        [Value::Unicode(key), id] if key == "solarsystemid2" => id.as_i64(),
        [key, id] if key.as_bytes() == Some(b"solarsystemid2".as_slice()) => id.as_i64(),
        _ => None,
    }
}

/// A chat channel's identity as a map key. Channel IDs are ints for ordinary
/// channels and tuples like `(('solarsystemid2', 30002780),)` for local chat.
fn channel_key(channel_id: &Value) -> String {
    format!("{channel_id:?}")
}
