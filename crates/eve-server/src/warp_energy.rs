//! Capacitor used by a warp, separate from Destiny's kinematic curve.
//!
//! Trinity attribute 153 describes a cost per kilogram per AU. The supplied
//! `WarpingWithAvailablePower` message confirms that insufficient charge makes
//! a shorter warp. The coefficient passed here is the fitted ship's effective
//! dogma value, including skill, implant and rig modifiers.

pub const WARP_CAPACITOR_NEED: i32 = 153;
pub const AU_METRES: f64 = 149_597_870_700.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quote {
    pub distance_m: f64,
    pub cost: f64,
    pub limited: bool,
}

/// Quote the actual travel distance at warp entry. This does not debit charge:
/// an alignment that is cancelled must not consume capacitor. Invalid state
/// must fail closed instead of injecting NaN into the ship or its capacitor.
pub fn quote(distance_m: f64, mass_kg: f64, need_per_kg_au: f64, available: f64) -> Option<Quote> {
    if ![distance_m, mass_kg, need_per_kg_au, available].iter().all(|v| v.is_finite())
        || distance_m < 0.0 || mass_kg <= 0.0 || need_per_kg_au < 0.0 || available < 0.0
    {
        return None;
    }
    let cost = (distance_m / AU_METRES) * mass_kg * need_per_kg_au;
    if !cost.is_finite() {
        return None;
    }
    if cost <= available {
        Some(Quote { distance_m, cost, limited: false })
    } else {
        Some(Quote { distance_m: distance_m * (available / cost), cost: available, limited: true })
    }
}
