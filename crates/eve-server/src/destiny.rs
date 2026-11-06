//! The destiny ball stream that SetState and AddBalls carry: a little-endian
//! binary format read by destiny.dll's ReadFullStateFromStream.
//!
//! Record layout verified against the original Trinity 45017 native reader
//! (RVA 0x31640) and writer (RVA 0x31c80). Reproducible listings, binary hash,
//! and the bounded audit are in compatibility/destiny-45017.
//!
//! ```text
//! i32 ballID  u8 mode  f64 radius  f64 x,y,z  u8 flags
//! if mode != RIGID:  f64 mass  u8 cloak  i64 harmonic  i32 corporationID  i32 allianceID
//! if flags & FREE:   f64 maxVelocity  f64 vx,vy,vz  f64 agility  f64 speedFraction
//! u8 formationID     (0xFF = none)
//! <per-mode payload> (44 bytes for WARP; empty for STOP, FIELD and RIGID)
//! u8 nameLength  u16[nameLength] name (UTF-16LE)
//! ```
//!
//! Native Python attribute registrations confirm the conditional field names;
//! cloak is its own non-rigid byte, separate from the packed flag bits.
//!
//! Ordinary moving ships carry STOP plus an immediate course opcode. Warping
//! ships carry the native WARP payload: replaying WarpTo on a midflight reset
//! would discard the original start time and restart the warp curve.

#[allow(dead_code)]
pub mod mode {
    pub const GOTO: u8 = 0;
    pub const FOLLOW: u8 = 1;
    pub const STOP: u8 = 2;
    pub const WARP: u8 = 3;
    pub const ORBIT: u8 = 4;
    /// A force field's native stream mode.
    pub const FIELD: u8 = 10;
    pub const RIGID: u8 = 11;
}

#[allow(dead_code)]
pub mod flags {
    pub const IS_FREE: u8 = 0x01;
    pub const IS_GLOBAL: u8 = 0x02;
    pub const IS_MASSIVE: u8 = 0x04;
    pub const IS_INTERACTIVE: u8 = 0x08;
    /// Native ball+0x39, packed by WriteBall at RVA 0x31cb5. This is not
    /// isCloaked (ball+0x3a), which has its own non-rigid stream byte.
    pub const NATIVE_FLAG_0X10: u8 = 0x10;
    pub const HAS_MINIBALLS: u8 = 0x40;

    pub const SHIP: u8 = IS_FREE | IS_GLOBAL | IS_MASSIVE | IS_INTERACTIVE;
    /// A ship in warp: native Trinity entry clears its massive flag (RVA 0x183f0).
    pub const SHIP_IN_WARP: u8 = SHIP & !IS_MASSIVE;
    pub const CELESTIAL: u8 = IS_GLOBAL | IS_MASSIVE | IS_INTERACTIVE;
    /// Fixed, but ships pass through it: stations, cans.
    pub const CELESTIAL_PASSABLE: u8 = CELESTIAL & !IS_MASSIVE;
}

const NO_FORMATION: u8 = 0xFF;
pub const NO_HARMONIC: i64 = -1;

#[derive(Debug, Clone)]
pub struct Ball {
    pub id: i64,
    pub mode: u8,
    /// Native WARP payload, including the original entry time and distance.
    pub warp: Option<crate::ballpark::WarpSnapshot>,
    pub radius: f64,
    pub pos: (f64, f64, f64),
    pub flags: u8,
    /// Only for non-rigid balls.
    pub mass: f64,
    pub corp_id: i32,
    pub alliance_id: i32,
    /// Only for non-rigid balls: -1 for none, -2 passes every field.
    pub harmonic: i64,
    /// Only for free balls.
    pub max_velocity: f64,
    pub velocity: (f64, f64, f64),
    pub agility: f64,
    pub speed_fraction: f64,
    pub name: String,
    /// Only for non-rigid balls: a cloaked ship (its pilot sees it so).
    pub cloaked: bool,
}

impl Ball {
    /// A ball that never moves; `massive` if ships bounce off it.
    pub fn celestial(id: i64, radius: f64, pos: (f64, f64, f64), name: &str, massive: bool) -> Self {
        Ball {
            id,
            mode: mode::RIGID,
            warp: None,
            radius,
            pos,
            flags: if massive { flags::CELESTIAL } else { flags::CELESTIAL_PASSABLE },
            mass: 0.0,
            corp_id: -1,
            alliance_id: -1,
            harmonic: NO_HARMONIC,
            max_velocity: 0.0,
            velocity: (0.0, 0.0, 0.0),
            agility: 1.0,
            speed_fraction: 0.0,
            name: name.to_string(),
            cloaked: false,
        }
    }
}

pub fn encode(state_stamp: u32, balls: &[Ball]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + balls.len() * 128);
    out.push(0u8); // packet type
    out.extend_from_slice(&state_stamp.to_le_bytes());
    for ball in balls {
        write_ball(&mut out, ball);
    }
    out
}

fn write_ball(out: &mut Vec<u8>, b: &Ball) {
    debug_assert!(
        matches!(b.mode, mode::STOP | mode::FIELD | mode::RIGID | mode::WARP),
        "unsupported native ball mode"
    );
    debug_assert_eq!(b.mode == mode::WARP, b.warp.is_some(), "WARP requires its native payload");
    let f64s = |out: &mut Vec<u8>, vals: &[f64]| {
        for v in vals {
            out.extend_from_slice(&v.to_le_bytes());
        }
    };
    out.extend_from_slice(&(b.id as i32).to_le_bytes());
    out.push(b.mode);
    f64s(out, &[b.radius, b.pos.0, b.pos.1, b.pos.2]);
    out.push(b.flags);

    if b.mode != mode::RIGID {
        f64s(out, &[b.mass]);
        out.push(u8::from(b.cloaked));
        out.extend_from_slice(&b.harmonic.to_le_bytes());
        out.extend_from_slice(&b.corp_id.to_le_bytes());
        out.extend_from_slice(&b.alliance_id.to_le_bytes());
    }
    if b.flags & flags::IS_FREE != 0 {
        f64s(out, &[b.max_velocity, b.velocity.0, b.velocity.1, b.velocity.2, b.agility, b.speed_fraction]);
    }
    out.push(NO_FORMATION);

    if let Some(warp) = &b.warp {
        // ReadBall 0x31b54 and WriteBall 0x3209c: +c8, +c4, +b8, +b4, +c0.
        // +b4 is copied as four raw bytes, but WarpTo stores a float32 range.
        f64s(out, &[warp.target.x, warp.target.y, warp.target.z]);
        out.extend_from_slice(&warp.start.to_le_bytes());
        f64s(out, &[warp.distance]);
        out.extend_from_slice(&warp.range.to_le_bytes());
        out.extend_from_slice(&warp.factor.to_le_bytes());
    }

    let mut utf16: Vec<u16> = b.name.encode_utf16().take(255).collect();
    // The length byte counts code units. Never truncate between the two
    // halves of a supplementary character, which would emit invalid UTF-16.
    if utf16.last().is_some_and(|unit|(0xD800..=0xDBFF).contains(unit)) { utf16.pop(); }
    out.push(utf16.len() as u8);
    for unit in utf16 {
        out.extend_from_slice(&unit.to_le_bytes());
    }
}

/// Destiny ticks are whole seconds; the clock is FILETIME seconds.
pub fn tick_now() -> i64 {
    crate::bulkdata::filetime_now() / 10_000_000
}

/// A tick as the client's 32-bit stamp.
pub fn stamp(tick: i64) -> u32 {
    tick as u32
}
