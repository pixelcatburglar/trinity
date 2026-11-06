//! The Trinity secure handshake, server side, under `cryptoPack=Placebo`.
//!
//! This is `gps.GPSTransport.HandleClientAuthentication`
//! (`common/script/net/GPS.py`), whose client half is `Authenticate` in the
//! same file. Both move in lock step:
//!
//! ```text
//!  server -> (170472, 182, users, 4.10, 45017, "LIVE@ccp")         plain
//!  client -> (170472, 182, 0,     4.10, 45017, "LIVE@ccp")         plain
//!  client -> (None, "VK", CryptoHash(casefold(user)))              plain, logins only
//!  client -> (None, "QC")                                          plain, status polls only
//!  server -> logon queue position                                  plain, after QC only
//!  client -> ("placebo", {})                                       plain: key version, crypto context
//!  client -> (challenge[64], {user_name, user_password_hash, ...}) sealed
//!  server -> password version                                      sealed
//!  client -> (challenge[64], {..., user_password})                 sealed, if version was 1
//!  server -> (challenge[64], (code, 0), {}, {challenge_responsehash, ...})  sealed
//!  client -> (CryptoHash(server challenge), output, result)        sealed, logins only
//!  server -> {user_clientid, session_init, live_updates, ...}      sealed
//! ```
//!
//! Two things differ from the Empyrean-era (2008) handshake: there is no
//! `"OK CC"` acknowledgement of the crypto context, and Trinity's
//! `Placebo_CryptoContext` is not the identity. Its `SymmetricEncryption` is
//! `blue.marshal.Save(plainPacket)`, and `plainPacket` is already
//! `macho.Dumps(x)`, so a sealed frame is a marshalled *string* holding a
//! marshal stream; `EncryptedRead` loads it twice. [`seal`] and [`open`] are
//! those two steps.
//!
//! Under Placebo, `GetRandomBytes` is zeroes and `CryptoHash(*args)` is
//! `str(binascii.crc_hqx(blue.marshal.Save(args), 0))` (`common/script/net/Crypto.py`),
//! so the challenge/response is a formality, but the hash must match the
//! client's byte for byte.

use eve_marshal::{StringTable, Value};

use crate::error::{ProtoError, Result};
use crate::llv;

/// `boot.GetValue('handShakePaddingLength', 64)`: both challenges' length.
pub const HANDSHAKE_PADDING: usize = 64;

/// `getattr(macho, 'publicKeyVersion', 'placebo')`: what the client announces
/// when it has no CryptoAPI key.
pub const PLACEBO_KEY_VERSION: &[u8] = b"placebo";

/// Answering 1 makes the client send its password in clear on a second
/// attempt (`user_password`), which is what this server's accounts store.
pub const PASSWORD_VERSION_PLAIN: i64 = 1;

/// What the client `eval`s: Python 2.5 `marshal.dumps("{}")`.
///
/// CCP shipped `handshake.Execute.func_code` here, a machine-details probe.
/// The client runs `eval(marshal.loads(blob), globals(), context)` inside a
/// try, and `eval` takes a source string as readily as a code object; `"{}"`
/// evaluates to the empty dict it would fall back to anyway. Layout: `'s'`
/// (TYPE_STRING), little-endian u32 length, the bytes.
pub const HANDSHAKE_FUNC: &[u8] = b"s\x02\x00\x00\x00{}";

/// `binascii.crc_hqx`: CRC-CCITT (XModem), MSB first.
pub fn crc_hqx(data: &[u8], seed: u16) -> u16 {
    let mut crc = seed;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

/// `Placebo_CryptoHash(*args)`: the CRC of the marshalled argument tuple, as
/// decimal text.
pub fn placebo_hash(args: &[Value], table: &StringTable) -> Result<String> {
    let raw = eve_marshal::save(&Value::Tuple(args.to_vec()), table)?;
    Ok(crc_hqx(&raw, 0).to_string())
}

/// `Placebo_GetRandomBytes(n)`.
pub fn placebo_random(n: usize) -> Vec<u8> {
    vec![0u8; n]
}

/// An `EncryptedWrite` payload: `blue.marshal.Save(macho.Dumps(v))`.
pub fn seal(v: &Value, table: &StringTable) -> Result<Vec<u8>> {
    let inner = eve_marshal::save(v, table)?;
    Ok(eve_marshal::save(&Value::Bytes(inner), table)?)
}

/// An `EncryptedRead`: unwrap the string, then load what it holds. As the
/// client's does, it falls back to reading the frame as a plain stream, which
/// is how a `GPSTransportClosed` arrives.
///
/// The client's wrapping string is itself a marshal stream, and blue.marshal
/// saves a `~`-prefixed string as a nested stream (`TYPE_STREAM`, 0x2B), not
/// as a plain string; either form is accepted.
pub fn open(frame: &[u8], table: &StringTable) -> Result<Value> {
    let outer = eve_marshal::load(frame, table)?;
    let value = match outer {
        Value::Bytes(inner) | Value::Buffer(inner) | Value::MarshalStream(inner)
            if inner.first() == Some(&eve_marshal::MAGIC) =>
        {
            eve_marshal::load(&inner, table)?
        }
        other => other,
    };
    if let Some(reason) = llv::closed_reason(&value) {
        return Err(ProtoError::PeerClosed { reason, bootver: None });
    }
    Ok(value)
}

/// Unwrap a sealed frame to the bytes it carries, which may be a marshal
/// stream or a zlib deflation of one (MachoTransport.Write compresses before
/// the transport seals). A frame that is not sealed, such as a
/// `GPSTransportClosed`, comes back as it is.
///
/// The client's transport to its server is `SecureNetClientTransport`, whose
/// `Read`/`Write` *are* `EncryptedRead`/`EncryptedWrite`, so every packet for
/// the life of the connection is sealed, not just the handshake's.
pub fn unseal(frame: &[u8], table: &StringTable) -> Vec<u8> {
    if frame.first() != Some(&eve_marshal::MAGIC) {
        return frame.to_vec();
    }
    match eve_marshal::load(frame, table) {
        Ok(Value::Bytes(inner) | Value::Buffer(inner) | Value::MarshalStream(inner)) => inner,
        _ => frame.to_vec(),
    }
}

/// Seal already-marshalled (and maybe compressed) packet bytes, as
/// `EncryptedWrite` does under Placebo.
pub fn seal_bytes(raw: &[u8], table: &StringTable) -> Result<Vec<u8>> {
    Ok(eve_marshal::save(&Value::Bytes(raw.to_vec()), table)?)
}

/// What the client's first plain packet after the version exchange was.
pub enum Opening {
    /// `(None, "VK", key)`: a login; the crypto context follows.
    VipKey,
    /// `(None, "QC")`: a logon-queue poll; answer the position and stop.
    QueueCheck,
    /// `(keyVersion, request)`: the crypto context itself.
    CryptoContext { key_version: Vec<u8> },
}

pub fn opening(v: &Value) -> Result<Opening> {
    let items = v.seq().ok_or(ProtoError::Shape("handshake packet"))?;
    let tag = items.get(1).and_then(|t| t.as_bytes());
    if items.first() == Some(&Value::None) {
        return match tag {
            Some(b"VK") => Ok(Opening::VipKey),
            Some(b"QC") => Ok(Opening::QueueCheck),
            _ => Err(ProtoError::Shape("handshake marker")),
        };
    }
    let key_version = items.first().and_then(|k| k.as_bytes()).unwrap_or_default().to_vec();
    Ok(Opening::CryptoContext { key_version })
}

/// The secure client handshake: `(clientChallenge, request)`.
pub struct ClientHandshake {
    pub challenge: Vec<u8>,
    pub request: Vec<(Value, Value)>,
}

impl ClientHandshake {
    pub fn from_value(v: &Value) -> Result<Self> {
        let items = v.seq().ok_or(ProtoError::Shape("secure client handshake"))?;
        let challenge = items
            .first()
            .and_then(|c| c.as_bytes())
            .ok_or(ProtoError::Shape("client challenge"))?
            .to_vec();
        let request = match items.get(1) {
            Some(Value::Dict(d)) => d.clone(),
            _ => return Err(ProtoError::Shape("handshake request")),
        };
        Ok(ClientHandshake { challenge, request })
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        let k = Value::bytes(key);
        self.request.iter().find(|(a, _)| *a == k).map(|(_, v)| v)
    }

    pub fn text(&self, key: &str) -> Option<String> {
        self.get(key).and_then(text_of)
    }

    /// The same version checks again, over the request dict.
    pub fn mismatch(&self) -> Option<&'static str> {
        let text = |k| self.get(k).and_then(|v| v.as_bytes()).unwrap_or_default();
        if text("boot_region") != llv::BOOT_REGION {
            Some("HANDSHAKE_INCOMPATIBLEREGION")
        } else if text("boot_codename") != llv::BOOT_CODENAME {
            Some("HANDSHAKE_INCOMPATIBLERELEASE")
        } else if self.get("boot_version").and_then(|v| v.as_f64()) != Some(llv::BOOT_VERSION) {
            Some("HANDSHAKE_INCOMPATIBLEVERSION")
        } else if self.get("macho_version").and_then(|v| v.as_i64()) != Some(llv::MACHO_VERSION) {
            Some("HANDSHAKE_INCOMPATIBLEPROTOCOL")
        } else if self.get("boot_build").and_then(|v| v.as_i64()).unwrap_or(-1) < llv::BOOT_BUILD {
            Some("HANDSHAKE_INCOMPATIBLEBUILD")
        } else {
            None
        }
    }

    pub fn has_password(&self) -> bool {
        self.get("user_password").is_some_and(|v| *v != Value::None)
    }
}

/// The text in a value, looking through string subclasses.
///
/// The login screen hands its password over as `util.PasswordString`, a
/// `unicode` subclass (common/script/util/format.py), which marshals as an
/// object around the text rather than as a plain string: a reduce or newobj
/// whose arguments carry it, or an old-style instance whose state does.
pub fn text_of(v: &Value) -> Option<String> {
    fn find(v: &Value, depth: u32) -> Option<String> {
        if depth > 4 {
            return None;
        }
        match v {
            Value::Bytes(_) | Value::Buffer(_) | Value::Unicode(_) => v.as_string(),
            Value::Reduce { args, state, .. } => args
                .seq()
                .and_then(|a| a.iter().find_map(|x| find(x, depth + 1)))
                .or_else(|| state.as_deref().and_then(|s| find(s, depth + 1))),
            // The class is a global, never text, so every argument can be
            // searched. The client sends PasswordString as
            // newobj(((util.PasswordString, u"pw"), {})).
            Value::NewObj { args, .. } => args.seq().and_then(|a| a.iter().find_map(|x| find(x, depth + 1))),
            Value::Instance { state, .. } => find(state, depth + 1),
            Value::Tuple(items) | Value::List(items) => items.iter().find_map(|x| find(x, depth + 1)),
            _ => None,
        }
    }
    find(v, 0)
}

/// `(serverChallenge, macho.Sign(code), {}, dict)`: the server's challenge.
pub fn server_challenge(client_challenge: &[u8], user_count: i64, node_id: i64, table: &StringTable) -> Result<(Vec<u8>, Value)> {
    let challenge = placebo_random(HANDSHAKE_PADDING);
    let response_hash = placebo_hash(&[Value::Bytes(client_challenge.to_vec())], table)?;
    let dict = Value::Dict(vec![
        (Value::bytes("challenge_responsehash"), Value::bytes(response_hash)),
        (Value::bytes("macho_version"), Value::Int(llv::MACHO_VERSION)),
        (Value::bytes("boot_version"), Value::Double(llv::BOOT_VERSION)),
        (Value::bytes("boot_build"), Value::Int(llv::BOOT_BUILD)),
        (Value::bytes("boot_codename"), Value::Bytes(llv::BOOT_CODENAME.to_vec())),
        (Value::bytes("boot_region"), Value::Bytes(llv::BOOT_REGION.to_vec())),
        (Value::bytes("cluster_usercount"), Value::Int(user_count)),
        (Value::bytes("proxy_nodeid"), Value::Int(node_id)),
        (Value::bytes("user_logonqueueposition"), Value::Int(1)),
    ]);
    let packet = Value::Tuple(vec![
        Value::Bytes(challenge.clone()),
        // macho.Sign under Placebo: (data, 0).
        Value::Tuple(vec![Value::Bytes(HANDSHAKE_FUNC.to_vec()), Value::Int(0)]),
        Value::Dict(Vec::new()),
        dict,
    ]);
    Ok((challenge, packet))
}

/// Whether the client's challenge response hashes our challenge correctly.
pub fn response_matches(response: &Value, server_challenge: &[u8], table: &StringTable) -> Result<bool> {
    let theirs = response.seq().and_then(|r| r.first()).and_then(|h| h.as_bytes()).unwrap_or_default();
    let ours = placebo_hash(&[Value::Bytes(server_challenge.to_vec())], table)?;
    Ok(theirs == ours.as_bytes())
}

/// The challenge-response ack: the handshake's last packet, which the client
/// merges over the challenge dict. It keeps `session_init['userid']` and
/// `proxy_nodeid` (`machoNet.ConnectToServer`) and runs any `live_updates`.
pub fn auth_ack(client_id: i64, session_init: Value) -> Value {
    Value::Dict(vec![
        (Value::bytes("user_clientid"), Value::Int(client_id)),
        (Value::bytes("session_init"), session_init),
        (Value::bytes("live_updates"), Value::List(Vec::new())),
        (Value::bytes("client_hashes"), Value::List(Vec::new())),
    ])
}
