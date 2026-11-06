//! The low-level version exchange and close packets for the Trinity
//! (4.10 build 45017) client.
//!
//! Every value here is compared by `gps.GPSTransport.Authenticate`
//! (`common/script/net/GPS.py`) against the client's `common.ini` and
//! `machoNetVersion.py`; any difference ends the connection.

use crate::error::{ProtoError, Result};
use eve_marshal::Value;

pub const LLV_MAGIC: i64 = 170472;

/// `macho.version` (`common/script/net/machoNetVersion.py`).
pub const MACHO_VERSION: i64 = 182;

/// `boot.version` (`common.ini` version=4.10).
pub const BOOT_VERSION: f64 = 4.10;

/// `boot.build`.
pub const BOOT_BUILD: i64 = 45017;

/// `boot.codename`.
pub const BOOT_CODENAME: &[u8] = b"LIVE";

/// `boot.region`.
pub const BOOT_REGION: &[u8] = b"ccp";

pub const MAX_CLIENT_LLV_BYTES: usize = 1000;

pub const LLV_TIMEOUT_SECS: u64 = 120;

/// `codename@region`, the sixth field of the exchange.
pub fn codename_region() -> Vec<u8> {
    let mut v = BOOT_CODENAME.to_vec();
    v.push(b'@');
    v.extend_from_slice(BOOT_REGION);
    v
}

#[derive(Clone, Debug, PartialEq)]
pub struct VersionChallenge {
    pub magic: i64,
    pub macho_version: i64,
    pub user_count: i64,
    pub boot_version: f64,
    pub boot_build: i64,
    /// `codename@region`.
    pub codename: Vec<u8>,
}

impl VersionChallenge {
    pub fn server(user_count: i64) -> Self {
        VersionChallenge {
            magic: LLV_MAGIC,
            macho_version: MACHO_VERSION,
            user_count,
            boot_version: BOOT_VERSION,
            boot_build: BOOT_BUILD,
            codename: codename_region(),
        }
    }

    pub fn client() -> Self {
        Self::server(0)
    }

    pub fn to_value(&self) -> Value {
        Value::Tuple(vec![
            Value::Int(self.magic),
            Value::Int(self.macho_version),
            Value::Int(self.user_count),
            Value::Double(self.boot_version),
            Value::Int(self.boot_build),
            Value::Bytes(self.codename.clone()),
        ])
    }

    pub fn from_value(v: &Value) -> Result<Self> {
        let items = v.seq().ok_or(ProtoError::VersionMismatch("not a tuple"))?;
        if items.len() != 6 {
            return Err(ProtoError::VersionMismatch("challenge is not a 6-tuple"));
        }
        let int = |i: usize, what: &'static str| {
            items[i].as_i64().ok_or(ProtoError::VersionMismatch(what))
        };
        Ok(VersionChallenge {
            magic: int(0, "magic is not an integer")?,
            macho_version: int(1, "version is not an integer")?,
            user_count: int(2, "user count is not an integer")?,
            boot_version: match &items[3] {
                Value::Double(d) => *d,
                other => other.as_i64().map(|i| i as f64).ok_or(
                    ProtoError::VersionMismatch("boot version is not a number"),
                )?,
            },
            boot_build: int(4, "build is not an integer")?,
            codename: items[5].as_bytes().unwrap_or_default().to_vec(),
        })
    }

    /// The checks `HandleClientAuthentication` makes, in its order. On a
    /// mismatch, the `MACHONET_TRANSPORTCLOSED_*` reason code to close with.
    pub fn mismatch(&self) -> Option<(&'static str, String)> {
        let (codename, region) = match self.codename.iter().position(|&b| b == b'@') {
            Some(at) => (&self.codename[..at], &self.codename[at + 1..]),
            None => (&self.codename[..], &b""[..]),
        };
        if region != BOOT_REGION {
            return Some(("HANDSHAKE_INCOMPATIBLEREGION", format!("region {:?}", String::from_utf8_lossy(region))));
        }
        if codename != BOOT_CODENAME {
            return Some(("HANDSHAKE_INCOMPATIBLERELEASE", format!("codename {:?}", String::from_utf8_lossy(codename))));
        }
        if self.boot_version != BOOT_VERSION {
            return Some(("HANDSHAKE_INCOMPATIBLEVERSION", format!("version {}", self.boot_version)));
        }
        if self.macho_version != MACHO_VERSION {
            return Some(("HANDSHAKE_INCOMPATIBLEPROTOCOL", format!("macho version {}", self.macho_version)));
        }
        if self.boot_build < BOOT_BUILD {
            return Some(("HANDSHAKE_INCOMPATIBLEBUILD", format!("build {}", self.boot_build)));
        }
        None
    }

    pub fn validate(&self) -> Result<()> {
        if self.magic != LLV_MAGIC {
            return Err(ProtoError::VersionMismatch(
                "a low-level-version challenge should be a tuple that starts with my birthday",
            ));
        }
        if self.macho_version != MACHO_VERSION {
            return Err(ProtoError::VersionMismatch("macho protocol version differs"));
        }
        Ok(())
    }

    pub fn llv_info(&self) -> (i64, f64, i64, Vec<u8>) {
        (self.user_count, self.boot_version, self.boot_build, self.codename.clone())
    }
}

/// `exceptions.GPSTransportClosed(reason, reasonCode, reasonArgs, machoVersion,
/// version, build, codename, region, origin, loggedOnUserCount)`, the packet a
/// Trinity peer closes with (`GPS.CreateClosedPacket`).
///
/// `GPSTransportClosed` derives from `StandardError`, a new-style class in
/// Python 2.5, so it travels as a reduce over its whitelisted guid. The client
/// raises it out of `macho.Loads`, and turns `reasonCode` into its own
/// localised `MACHONET_TRANSPORTCLOSED_*` text where it has one.
pub fn transport_closed(reason: &str, reason_code: Option<&str>, reason_args: Vec<(Value, Value)>) -> Value {
    Value::reduce(
        "exceptions.GPSTransportClosed",
        vec![
            Value::text(reason),
            reason_code.map_or(Value::None, Value::bytes),
            Value::Dict(reason_args),
            Value::Int(MACHO_VERSION),
            Value::Double(BOOT_VERSION),
            Value::Int(BOOT_BUILD),
            Value::Bytes(BOOT_CODENAME.to_vec()),
            Value::Bytes(BOOT_REGION.to_vec()),
            Value::bytes("proxy"),
            Value::Int(0),
        ],
    )
}

/// The marshalled close packet for `reason`.
pub fn close_reason(reason: &str, table: &eve_marshal::StringTable) -> Result<Vec<u8>> {
    close_packet(reason, None, table)
}

/// The marshalled close packet, with a reason code the client may localise.
pub fn close_packet(reason: &str, reason_code: Option<&str>, table: &eve_marshal::StringTable) -> Result<Vec<u8>> {
    Ok(eve_marshal::save(&transport_closed(reason, reason_code, Vec::new()), table)?)
}

/// The reason in a `GPSTransportClosed` the peer sent, if this is one.
///
/// Matched on the class name alone, whatever module the peer's marshal names
/// it by, and in any of the forms an object can travel: a reduce over the
/// class, a newobj whose args start with it, or an old-style instance. The
/// reason is the first constructor argument, or `reason` in the state.
pub fn closed_reason(v: &Value) -> Option<String> {
    fn is_closed_class(c: &Value) -> bool {
        match c {
            Value::Global(g) | Value::Bytes(g) => g.ends_with(b"GPSTransportClosed"),
            _ => false,
        }
    }
    let text = |v: Option<&Value>| v.and_then(|r| r.as_string());
    let from_state = |s: &Value| text(s.get_str("reason"));
    match v {
        Value::Reduce { callable, args, state, .. } if is_closed_class(callable) => Some(
            text(args.seq().and_then(|a| a.first()))
                .or_else(|| state.as_deref().and_then(from_state))
                .unwrap_or_default(),
        ),
        Value::NewObj { args, dict_items, .. }
            if args.seq().and_then(|a| a.first()).is_some_and(is_closed_class) =>
        {
            let state = Value::Dict(dict_items.clone());
            Some(from_state(&state).or_else(|| text(args.seq().and_then(|a| a.get(1)))).unwrap_or_default())
        }
        Value::Instance { class, state } if is_closed_class(class) => Some(from_state(state).unwrap_or_default()),
        _ => None,
    }
}
