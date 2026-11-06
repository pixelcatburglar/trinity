//! Item changes made in one store transaction, and the `OnItemChange` events
//! that tell the client about them.
//!
//! Every item an operation touches is snapshotted the first time it is read;
//! [`ItemTxn::events`] compares each one with how it ended up, so an
//! operation only says what to do and the events follow from the data
//! (invCache.py:111 reads them: an item at quantity 0 is gone, one whose old
//! location is 0 is new). The store runs one write at a time, so an item
//! read here cannot change under the operation before it commits.
//!
//! ```ignore
//! let events = ctx.store.write(|tx| -> Result<_, StoreError> {
//!     let mut items = ItemTxn::new(tx);
//!     items.move_to(id, station, FLAG_HANGAR)?;
//!     Ok(items.events())
//! })?;
//! crate::items::notify(ctx.world, ctx.client_id, events);
//! ```

use std::collections::{HashMap, HashSet};

use eve_marshal::Value;

use crate::store::{self, Row, StoreError, Tx};
use crate::svc::inventory::{item_change, IX_LOCATION_ID, IX_QUANTITY};

pub type DbResult<T> = Result<T, StoreError>;

/// What an operation needs to know about an item.
#[derive(Debug, Clone)]
pub struct Item {
    pub id: i64,
    pub type_id: i64,
    pub owner_id: i64,
    pub location_id: i64,
    pub flag: i64,
    pub singleton: bool,
    pub quantity: i64,
    pub category_id: i64,
    /// Volume of one unit.
    pub volume: f64,
}

/// The client's item row columns (`invItem`): `INVITEM_HEADER`.
pub const INVITEM_COLUMNS: [&str; 11] =
    ["itemID", "typeID", "ownerID", "locationID", "flag", "contraband", "singleton", "quantity", "groupID", "categoryID", "customInfo"];

/// Positions of the group and category in an item row.
const IX_GROUP_ID: usize = 8;
const IX_CATEGORY_ID: usize = 9;

/// An entity row as the client's item row, its group and category from the
/// static data; None for an item of a type it does not know (or whose group
/// it does not know), which the item SQL left out by joining the type
/// tables. `customInfo` is sent as bytes.
pub fn invitem_cells(row: &Row) -> Option<Vec<Value>> {
    let mut cells = item_cells(row)?;
    cells.push(match row.text("customInfo") {
        Some(s) => Value::Bytes(s.as_bytes().to_vec()),
        None => Value::None,
    });
    Some(cells)
}

/// As [`invitem_cells`], with `customInfo` as `svc::rows::cell` sent text
/// (`Value::text`): for listings that sent the item query's rows to the
/// client as they came.
pub fn invitem_line(row: &Row) -> Option<Vec<Value>> {
    let mut cells = item_cells(row)?;
    cells.push(row.cell("customInfo"));
    Some(cells)
}

/// [`invitem_line`] of each row, as rowset lines.
pub fn invitem_lines<'a>(rows: impl IntoIterator<Item = Row<'a>>) -> Vec<Value> {
    rows.into_iter().filter_map(|r| invitem_line(&r)).map(Value::List).collect()
}

fn item_cells(row: &Row) -> Option<Vec<Value>> {
    let type_id = row.int("typeID")?;
    let (group, category) = crate::sde::get().group_and_category(type_id)?;
    let mut cells: Vec<Value> = INVITEM_COLUMNS[..IX_GROUP_ID].iter().map(|c| row.int(c).map_or(Value::None, Value::Int)).collect();
    cells.push(Value::Int(group));
    cells.push(Value::Int(category));
    debug_assert_eq!(cells.len(), IX_CATEGORY_ID + 1);
    Some(cells)
}

/// The item row columns compared for events: itemID up to quantity.
const COMPARED_COLUMNS: usize = 8;

pub struct ItemTxn<'t, 'd> {
    tx: &'t mut Tx<'d>,
    /// Each touched item as it was before this transaction; None if it is new.
    before: HashMap<i64, Option<Vec<Value>>>,
    order: Vec<i64>,
}

impl<'t, 'd> ItemTxn<'t, 'd> {
    pub fn new(tx: &'t mut Tx<'d>) -> Self {
        ItemTxn { tx, before: HashMap::new(), order: Vec::new() }
    }

    /// The transaction, for changes that are not item changes (wallets,
    /// orders).
    pub fn tx(&mut self) -> &mut Tx<'d> {
        self.tx
    }

    /// The data as the transaction has made it so far.
    pub fn db(&self) -> &store::Db {
        self.tx.db()
    }

    fn entity_row(&self, id: i64) -> Option<store::RowId> {
        self.tx.table("entity").get_id(id).map(|r| r.id)
    }

    /// Read `id`; the first read is the snapshot.
    pub fn item(&mut self, id: i64) -> DbResult<Option<Item>> {
        let entity = self.tx.table("entity");
        let Some(row) = entity.get_id(id) else { return Ok(None) };
        // An item of a type the static data does not know is no item here,
        // as when the type tables were joined.
        let Some(cells) = invitem_cells(&row) else { return Ok(None) };
        let type_id = row.int("typeID").unwrap_or(0);
        let item = Item {
            id,
            type_id,
            owner_id: row.int("ownerID").unwrap_or(0),
            location_id: row.int("locationID").unwrap_or(0),
            flag: row.int("flag").unwrap_or(0),
            singleton: row.int("singleton").unwrap_or(0) != 0,
            quantity: row.int("quantity").unwrap_or(0),
            category_id: cells[IX_CATEGORY_ID].as_i64().unwrap_or(0),
            volume: crate::sde::table("invTypes").get("typeID", type_id).and_then(|t| t.float("volume")).unwrap_or(0.0),
        };
        if let std::collections::hash_map::Entry::Vacant(slot) = self.before.entry(id) {
            slot.insert(Some(cells));
            self.order.push(id);
        }
        Ok(Some(item))
    }

    /// Read several items; missing ones are left out.
    pub fn items(&mut self, ids: &[i64]) -> DbResult<Vec<Item>> {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            out.extend(self.item(*id)?);
        }
        Ok(out)
    }

    fn touch(&mut self, id: i64) -> DbResult<()> {
        if !self.before.contains_key(&id) {
            self.item(id)?;
        }
        Ok(())
    }

    /// Set columns of the entity row of `id` (nothing if there is none, as
    /// an UPDATE matching no row did).
    fn set(&mut self, id: i64, sets: store::Sets) -> DbResult<()> {
        match self.entity_row(id) {
            Some(row) => self.tx.update("entity", row, sets),
            None => Ok(()),
        }
    }

    pub fn move_to(&mut self, id: i64, location_id: i64, flag: i64) -> DbResult<()> {
        self.touch(id)?;
        self.set(id, &[("locationID", store::Value::Int(location_id)), ("flag", store::Value::Int(flag))])
    }

    /// Set a stack's size; 0 destroys the item.
    pub fn set_quantity(&mut self, id: i64, quantity: i64) -> DbResult<()> {
        if quantity <= 0 {
            return self.destroy(id);
        }
        self.touch(id)?;
        self.set(id, &[("quantity", store::Value::Int(quantity))])
    }

    pub fn set_owner(&mut self, id: i64, owner_id: i64) -> DbResult<()> {
        self.touch(id)?;
        self.set(id, &[("ownerID", store::Value::Int(owner_id))])
    }

    /// Where an item floats in space (not compared for events: the client
    /// learns positions from destiny).
    pub fn set_position(&mut self, id: i64, pos: (f64, f64, f64)) -> DbResult<()> {
        self.set(id, &[("x", store::Value::Float(pos.0)), ("y", store::Value::Float(pos.1)), ("z", store::Value::Float(pos.2))])
    }

    /// Where every item touched so far was before this transaction.
    pub fn origins(&self) -> Vec<i64> {
        let mut out: Vec<i64> =
            self.before.values().flatten().filter_map(|cells| cells.get(IX_LOCATION_ID as usize).and_then(|v| v.as_i64())).collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    pub fn set_singleton(&mut self, id: i64, singleton: bool) -> DbResult<()> {
        self.touch(id)?;
        self.set(id, &[("singleton", store::Value::Int(i64::from(singleton)))])
    }

    /// Delete an item and its stored attributes (and, through the store's
    /// foreign keys, what was kept for it).
    pub fn destroy(&mut self, id: i64) -> DbResult<()> {
        self.touch(id)?;
        self.tx.delete_where("entity_attributes", &["itemID"], &[store::Value::Int(id)])?;
        if let Some(row) = self.entity_row(id) {
            self.tx.delete("entity", row)?;
        }
        Ok(())
    }

    /// The items directly inside `id`, by itemID.
    fn children(&self, id: i64) -> Vec<i64> {
        let mut ids: Vec<i64> = self.tx.table("entity").find(&["locationID"], &[store::Value::Int(id)]).filter_map(|r| r.int("itemID")).collect();
        ids.sort_unstable();
        ids
    }

    /// Destroy `root` and everything inside it. With `expected_parent`, only
    /// if `root` is still there; `protected` items (and what is in them) are
    /// kept, so a ship loss retains its pilot even in malformed nesting.
    pub fn destroy_tree(&mut self, root: i64, expected_parent: Option<i64>, protected: &[i64]) -> DbResult<()> {
        let mut pending = vec![(root, expected_parent)];
        let mut seen = HashSet::new();
        while let Some((id, parent)) = pending.pop() {
            if protected.contains(&id) || seen.contains(&id) {
                continue;
            }
            let Some(item) = self.item(id)? else { continue };
            if parent.is_some_and(|parent| item.location_id != parent) {
                continue;
            }
            seen.insert(id);
            for child in self.children(id).into_iter().rev() {
                pending.push((child, Some(id)));
            }
            self.destroy(id)?;
        }
        Ok(())
    }

    /// Hand `root` and everything inside it from owner `from` to `to`. False
    /// means custody was invalid; callers must refuse (returning an error
    /// from the store write undoes what this did).
    pub fn transfer_tree(&mut self, root: i64, from: i64, to: i64) -> DbResult<bool> {
        let mut pending = vec![(root, None)];
        let mut seen = HashSet::new();
        while let Some((id, parent)) = pending.pop() {
            if !seen.insert(id) {
                return Ok(false);
            }
            let Some(item) = self.item(id)? else {
                if id == root {
                    return Ok(false);
                }
                continue;
            };
            if parent.is_some_and(|parent| item.location_id != parent) {
                continue;
            }
            if item.owner_id != from || matches!(item.flag, 7 | 36 | 57 | 61 | 88 | 89) {
                return Ok(false);
            }
            // Characters and their active ships cannot be handed over as cargo.
            let characters = self.tx.table("character_");
            if characters.get_id(id).is_some() || characters.find(&["shipID"], &[store::Value::Int(id)]).next().is_some() {
                return Ok(false);
            }
            for child in self.children(id).into_iter().rev() {
                pending.push((child, Some(id)));
            }
            self.set_owner(id, to)?;
        }
        Ok(true)
    }

    /// A new item; returns its ID.
    pub fn create(&mut self, new: NewItem) -> DbResult<i64> {
        let id = self.tx.nextval("dyn_id_seq")?;
        self.tx.insert(
            "entity",
            &[
                ("itemID", store::Value::Int(id)),
                ("itemName", store::Value::Text("".into())),
                ("typeID", store::Value::Int(new.type_id)),
                ("ownerID", store::Value::Int(new.owner_id)),
                ("locationID", store::Value::Int(new.location_id)),
                ("flag", store::Value::Int(new.flag)),
                ("contraband", store::Value::Int(0)),
                ("singleton", store::Value::Int(i64::from(new.singleton))),
                ("quantity", store::Value::Int(new.quantity)),
                ("x", store::Value::Float(0.0)),
                ("y", store::Value::Float(0.0)),
                ("z", store::Value::Float(0.0)),
                ("customInfo", store::Value::Null),
            ],
        )?;
        self.before.insert(id, None);
        self.order.push(id);
        Ok(id)
    }

    /// Add `quantity` of a type to a stack at (owner, location, flag), making
    /// one if there is none. Returns the stack's ID.
    pub fn stack_onto(&mut self, new: NewItem) -> DbResult<i64> {
        let existing = self
            .tx
            .table("entity")
            .find(&["ownerID", "locationID", "flag"], &[store::Value::Int(new.owner_id), store::Value::Int(new.location_id), store::Value::Int(new.flag)])
            .filter(|r| r.int("typeID") == Some(new.type_id) && r.int("singleton") == Some(0))
            .filter_map(|r| r.int("itemID"))
            .min();
        match existing {
            Some(id) => {
                let Some(item) = self.item(id)? else { return self.create(new) };
                if item.quantity.checked_add(new.quantity).is_none_or(|n| n > i32::MAX as i64) {
                    return self.create(new);
                }
                self.set_quantity(id, item.quantity + new.quantity)?;
                Ok(id)
            }
            None => self.create(new),
        }
    }

    /// The `OnItemChange` argument tuples for everything this transaction
    /// changed. Return them from the store write and publish them once it
    /// has committed.
    pub fn events(&self) -> Vec<Value> {
        let entity = self.tx.table("entity");
        let mut events = Vec::new();
        for id in &self.order {
            let before = self.before.get(id).cloned().flatten();
            let after = entity.get_id(*id).and_then(|r| invitem_cells(&r));
            let event = match (before, after) {
                (Some(old), Some(now)) => {
                    let changed: Vec<(i64, Value)> =
                        (0..COMPARED_COLUMNS).filter(|&i| old[i] != now[i]).map(|i| (i as i64, old[i].clone())).collect();
                    (!changed.is_empty()).then(|| item_change(now, changed))
                }
                (Some(mut old), None) => {
                    let quantity = old[IX_QUANTITY as usize].clone();
                    old[IX_QUANTITY as usize] = Value::Int(0);
                    Some(item_change(old, vec![(IX_QUANTITY, quantity)]))
                }
                (None, Some(now)) => Some(item_change(now, vec![(IX_LOCATION_ID, Value::Int(0)), (IX_QUANTITY, Value::Int(0))])),
                (None, None) => None,
            };
            events.extend(event);
        }
        events
    }
}

pub struct NewItem {
    pub type_id: i64,
    pub owner_id: i64,
    pub location_id: i64,
    pub flag: i64,
    pub quantity: i64,
    pub singleton: bool,
}

/// Queue `OnItemChange` events for one client.
pub fn notify(world: &crate::world::World, client_id: i64, events: Vec<Value>) {
    let world = world.lock();
    for event in events {
        world.notify(&[client_id], "OnItemChange", event);
    }
}
