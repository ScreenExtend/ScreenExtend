//! The requests and replies only the driver's versioned pipe understands.
//!
//! Framing is v1's: one JSON value, then the byte `0x04`. Every message here is a JSON object
//! with exactly one key naming it, and no such key exists in v1, so the two are never confused.
//! v2 replies are decoded by externally tagged types, never by the untagged v1 enums in
//! `core.rs`, which accept anything that almost matches.
//!
//! A request is sent only on a connection whose `Hello` reply listed its capability; the
//! driver's version and build are for logs and `doctor`, never for deciding.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const CAP_HELLO: &str = "hello";
pub const CAP_DIAGNOSTICS: &str = "diagnostics";
pub const CAP_RENDER_ADAPTER: &str = "render_adapter";
pub const CAP_OWNED_MONITORS: &str = "owned_monitors";

/// Every reply name this protocol defines. A message whose single key is one of these is a v2
/// reply, decoded strictly; anything else goes to the v1 decoder.
const REPLY_NAMES: [&str; 4] = ["Hello", "Diagnostics", "RenderAdapter", "Error"];

#[derive(Debug, Clone, Serialize)]
pub enum Request {
    Hello(Hello),
    Diagnostics(Empty),
    SetRenderAdapter(SetRenderAdapter),
}

impl Request {
    pub fn name(&self) -> &'static str {
        match self {
            Request::Hello(_) => "Hello",
            Request::Diagnostics(_) => "Diagnostics",
            Request::SetRenderAdapter(_) => "SetRenderAdapter",
        }
    }
}

/// `{}`, not `null`: the driver refuses the latter.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Empty {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Hello {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<Owner>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Owner {
    pub token: OwnerToken,
    pub grace_ms: u32,
}

/// The identifier a host's driver connections present so the driver can remove the host's
/// displays once all of them are gone.
///
/// Secret: presenting it keeps a dead host's displays alive, so it never reaches a log line or
/// a file. `Debug` and `Display` print only the six-character prefix the driver itself logs;
/// `Serialize` alone writes the whole value, onto the pipe.
#[derive(Clone, PartialEq, Eq)]
pub struct OwnerToken(String);

impl OwnerToken {
    /// 128 bits from the OS random source, as 32 hex characters. `None` when the source fails,
    /// and then the host declares no owner at all.
    pub fn generate() -> Option<Self> {
        use rand::TryRngCore;
        let mut bytes = [0u8; 16];
        rand::rngs::OsRng.try_fill_bytes(&mut bytes).ok()?;
        Some(Self(bytes.iter().map(|b| format!("{b:02x}")).collect()))
    }

    /// What may be logged: the same prefix the driver logs and reports.
    pub fn prefix(&self) -> &str {
        &self.0[..self.0.len().min(6)]
    }

    #[cfg(test)]
    pub fn reveal(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for OwnerToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "OwnerToken({}…)", self.prefix())
    }
}

impl fmt::Display for OwnerToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}…", self.prefix())
    }
}

impl Serialize for OwnerToken {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

/// A DXGI adapter LUID, as the driver spells it: `LowPart` and `HighPart`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Luid {
    pub low: u32,
    pub high: i32,
}

impl Luid {
    /// From `AdapterInfo::luid`, which packs `HighPart` above `LowPart`.
    pub fn from_packed(v: i64) -> Self {
        Self {
            low: v as u32,
            high: (v >> 32) as i32,
        }
    }

    pub fn packed(self) -> i64 {
        (i64::from(self.high) << 32) | i64::from(self.low)
    }
}

impl fmt::Display for Luid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:08X}:{:08X}", self.high, self.low)
    }
}

/// `{"luid": null}` clears the preference; the field is always written, because the driver
/// treats a missing one as a malformed request rather than as a clear.
#[derive(Debug, Clone, Serialize)]
pub struct SetRenderAdapter {
    pub luid: Option<Luid>,
}

#[derive(Debug, Clone, Deserialize)]
pub enum Reply {
    Hello(HelloReply),
    Diagnostics(Map<String, Value>),
    RenderAdapter(RenderAdapterReply),
    Error(ErrorReply),
}

impl Reply {
    pub fn name(&self) -> &'static str {
        match self {
            Reply::Hello(_) => "Hello",
            Reply::Diagnostics(_) => "Diagnostics",
            Reply::RenderAdapter(_) => "RenderAdapter",
            Reply::Error(_) => "Error",
        }
    }
}

/// Unknown fields are ignored, so a later driver can add one; every field read here must be
/// present and of its type, or the whole reply is refused.
#[derive(Debug, Clone, Deserialize)]
pub struct HelloReply {
    pub protocol: u32,
    pub driver: String,
    pub build: String,
    #[serde(default)]
    pub iddcx_version: u32,
    pub capabilities: Vec<String>,
    pub connection: u64,
    pub owner: Option<OwnerReply>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OwnerReply {
    pub token_prefix: String,
    pub grace_ms: u32,
    pub monitors: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RenderAdapterReply {
    pub preferred: Option<Luid>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ErrorReply {
    pub request: String,
    pub code: String,
    #[serde(default)]
    pub message: String,
}

/// `None` when the message is not a v2 reply, so it belongs to the v1 decoder. `Some(Err(_))`
/// when it names a v2 reply and does not parse as one: it is still the answer to a request, and
/// the request is told so rather than left to time out.
pub fn decode_reply(bytes: &[u8]) -> Option<Result<Reply, String>> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    let object = value.as_object()?;
    if object.len() != 1 || !object.keys().all(|k| REPLY_NAMES.contains(&k.as_str())) {
        return None;
    }
    Some(serde_json::from_value::<Reply>(value).map_err(|e| e.to_string()))
}

/// The capability names one connection's driver listed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capabilities(BTreeSet<String>);

impl Capabilities {
    pub fn has(&self, name: &str) -> bool {
        self.0.contains(name)
    }

    pub fn names(&self) -> Vec<&str> {
        self.0.iter().map(String::as_str).collect()
    }
}

impl<S: Into<String>> FromIterator<S> for Capabilities {
    fn from_iter<I: IntoIterator<Item = S>>(iter: I) -> Self {
        Self(iter.into_iter().map(Into::into).collect())
    }
}

/// What one connection agreed with the driver.
#[derive(Debug, Clone, Default)]
pub struct Negotiated {
    /// 1 on the unversioned pipe; 2 on the versioned one, with or without a `Hello`.
    pub protocol: u32,
    /// Empty unless a `Hello` was answered, so nothing new is ever sent without one.
    pub capabilities: Capabilities,
    pub driver: Option<String>,
    pub build: Option<String>,
    pub iddcx_version: u32,
    /// The driver's view of this connection's owner, when it accepted one.
    pub owner: Option<OwnerReply>,
    /// The driver served no versioned pipe at all (as opposed to one that did not answer).
    pub v2_absent: bool,
}

impl Negotiated {
    /// A connection that sent no `Hello`: it carries v1 messages only.
    pub fn without_hello(protocol: u32) -> Self {
        Self {
            protocol,
            ..Self::default()
        }
    }

    pub fn has(&self, name: &str) -> bool {
        self.capabilities.has(name)
    }

    /// The driver removes this connection's monitors when the host goes: it listed the
    /// capability and accepted the owner.
    pub fn owns_monitors(&self) -> bool {
        self.has(CAP_OWNED_MONITORS) && self.owner.is_some()
    }
}

/// Turn the answer to a `Hello` into a negotiated connection, or say why it is not one.
pub fn negotiate(reply: Reply) -> Result<Negotiated, String> {
    match reply {
        Reply::Hello(h) if h.protocol >= 2 => Ok(Negotiated {
            protocol: 2,
            capabilities: h.capabilities.iter().cloned().collect(),
            driver: Some(h.driver),
            build: Some(h.build),
            iddcx_version: h.iddcx_version,
            owner: h.owner,
            v2_absent: false,
        }),
        Reply::Hello(h) => Err(format!(
            "the driver answered Hello with protocol {}",
            h.protocol
        )),
        Reply::Error(e) => Err(format!(
            "the driver refused Hello: {} {}",
            e.code, e.message
        )),
        other => Err(format!("the driver answered Hello with {}", other.name())),
    }
}

/// The preference the driver reports in its diagnostics: `Some(None)` for "no preference",
/// `None` when the diagnostics do not say.
pub fn reported_preference(diag: &Map<String, Value>) -> Option<Option<Luid>> {
    let pref = diag.get("render_adapter_preference")?.get("preferred")?;
    if pref.is_null() {
        return Some(None);
    }
    serde_json::from_value::<Luid>(pref.clone()).ok().map(Some)
}

/// The adapter Windows composes monitor `id` on, from the driver's live monitor objects (a
/// departed one is skipped: IddCx can keep it around after departure).
pub fn composing_adapter(diag: &Map<String, Value>, id: u32) -> Option<Luid> {
    diag.get("monitor_objects")?
        .as_array()?
        .iter()
        .filter(|m| m.get("id").and_then(Value::as_u64) == Some(u64::from(id)))
        .filter(|m| m.get("departed").and_then(Value::as_bool) == Some(false))
        .filter_map(|m| m.get("render_adapter"))
        .find_map(|l| serde_json::from_value::<Luid>(l.clone()).ok())
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn requests_are_written_as_the_driver_parses_them() {
        let token = OwnerToken("0123456789abcdef0123456789abcdef".into());
        let hello = Request::Hello(Hello {
            client: Some("ScreenExtend 1".into()),
            owner: Some(Owner {
                token,
                grace_ms: 5000,
            }),
        });
        assert_eq!(
            serde_json::to_string(&hello).unwrap(),
            r#"{"Hello":{"client":"ScreenExtend 1","owner":{"token":"0123456789abcdef0123456789abcdef","grace_ms":5000}}}"#
        );
        let bare = Request::Hello(Hello {
            client: None,
            owner: None,
        });
        assert_eq!(serde_json::to_string(&bare).unwrap(), r#"{"Hello":{}}"#);
        assert_eq!(
            serde_json::to_string(&Request::Diagnostics(Empty {})).unwrap(),
            r#"{"Diagnostics":{}}"#
        );
        let set = Request::SetRenderAdapter(SetRenderAdapter {
            luid: Some(Luid {
                low: 97388,
                high: -2,
            }),
        });
        assert_eq!(
            serde_json::to_string(&set).unwrap(),
            r#"{"SetRenderAdapter":{"luid":{"low":97388,"high":-2}}}"#
        );
        let clear = Request::SetRenderAdapter(SetRenderAdapter { luid: None });
        assert_eq!(
            serde_json::to_string(&clear).unwrap(),
            r#"{"SetRenderAdapter":{"luid":null}}"#
        );
    }

    #[test]
    fn the_token_prints_only_its_prefix() {
        let token = OwnerToken::generate().expect("the OS random source");
        assert_eq!(token.reveal().len(), 32);
        assert!(token.reveal().bytes().all(|b| b.is_ascii_hexdigit()));
        for shown in [format!("{token:?}"), format!("{token}")] {
            assert!(!shown.contains(token.reveal()), "{shown}");
            assert!(shown.contains(token.prefix()), "{shown}");
        }
        assert_ne!(OwnerToken::generate(), Some(token));
    }

    #[test]
    fn v1_messages_are_left_to_the_v1_decoder() {
        for m in [
            r#"{"State":[]}"#,
            r#"{"Changed":[{"id":0,"name":null,"enabled":true,"modes":[]}]}"#,
            r#""State""#,
            r#"{"Hello":{},"State":[]}"#,
            "not json",
        ] {
            assert!(decode_reply(m.as_bytes()).is_none(), "{m}");
        }
    }

    #[test]
    fn replies_decode_strictly() {
        let hello = br#"{"Hello":{"protocol":2,"driver":"1.0.4","build":"155e5fb","iddcx_version":68608,"capabilities":["hello","diagnostics","render_adapter","owned_monitors"],"connection":7,"owner":{"token_prefix":"abcdef","grace_ms":5000,"monitors":[0]},"later_field":1}}"#;
        let Some(Ok(Reply::Hello(h))) = decode_reply(hello) else {
            panic!("a Hello reply, extra field ignored");
        };
        assert_eq!(h.owner.unwrap().monitors, vec![0]);

        let err =
            br#"{"Error":{"request":"SetRenderAdapter","code":"unknown_adapter","message":"x"}}"#;
        assert!(
            matches!(decode_reply(err), Some(Ok(Reply::Error(e))) if e.code == "unknown_adapter")
        );

        let ra = br#"{"RenderAdapter":{"preferred":{"low":97388,"high":0},"description":"GPU"}}"#;
        assert!(matches!(
            decode_reply(ra),
            Some(Ok(Reply::RenderAdapter(RenderAdapterReply {
                preferred: Some(Luid {
                    low: 97388,
                    high: 0
                }),
                ..
            })))
        ));

        // names a v2 reply, does not parse: an answer that says so, never a v1 message
        for bad in [
            &br#"{"Hello":{"protocol":"two"}}"#[..],
            br#"{"Hello":{"protocol":2}}"#,
            br#"{"RenderAdapter":{"preferred":{"low":-1,"high":0},"description":null}}"#,
            br#"{"Diagnostics":[]}"#,
            br#"{"Error":{"code":"x"}}"#,
        ] {
            assert!(
                matches!(decode_reply(bad), Some(Err(_))),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    fn hello_with(caps: &[&str], owner: bool) -> Reply {
        Reply::Hello(HelloReply {
            protocol: 2,
            driver: "1.0.4".into(),
            build: "b".into(),
            iddcx_version: 0,
            capabilities: caps.iter().map(|c| c.to_string()).collect(),
            connection: 1,
            owner: owner.then(|| OwnerReply {
                token_prefix: "abcdef".into(),
                grace_ms: 5000,
                monitors: vec![],
            }),
        })
    }

    #[test]
    fn negotiation_reads_capabilities_never_versions() {
        let all = negotiate(hello_with(
            &[
                CAP_HELLO,
                CAP_DIAGNOSTICS,
                CAP_RENDER_ADAPTER,
                CAP_OWNED_MONITORS,
            ],
            true,
        ))
        .unwrap();
        assert!(all.owns_monitors() && all.has(CAP_RENDER_ADAPTER));
        // listed but the owner was not accepted, or accepted without the capability
        assert!(!negotiate(hello_with(&[CAP_OWNED_MONITORS], false))
            .unwrap()
            .owns_monitors());
        assert!(!negotiate(hello_with(&[CAP_HELLO], true))
            .unwrap()
            .owns_monitors());
        let none = negotiate(hello_with(&[], false)).unwrap();
        assert!(none.capabilities.names().is_empty());
        assert!(negotiate(Reply::Error(ErrorReply {
            request: "Hello".into(),
            code: "hello_twice".into(),
            message: String::new(),
        }))
        .is_err());
        assert!(negotiate(Reply::Diagnostics(Map::new())).is_err());
        assert!(!Negotiated::without_hello(2).has(CAP_HELLO));
    }

    #[test]
    fn diagnostics_name_the_preference_and_the_composing_adapter() {
        let Value::Object(diag) = serde_json::json!({
            "render_adapter_preference": { "preferred": { "low": 97388, "high": 0 }, "description": "N" },
            "monitor_objects": [
                { "id": 0, "departed": true, "render_adapter": { "low": 1, "high": 0 } },
                { "id": 0, "departed": false, "render_adapter": { "low": 74978, "high": 0 } },
                { "id": 1, "departed": false, "render_adapter": null }
            ]
        }) else {
            unreachable!()
        };
        assert_eq!(
            reported_preference(&diag),
            Some(Some(Luid {
                low: 97388,
                high: 0
            }))
        );
        assert_eq!(
            composing_adapter(&diag, 0),
            Some(Luid {
                low: 74978,
                high: 0
            })
        );
        assert_eq!(composing_adapter(&diag, 1), None);
        assert_eq!(composing_adapter(&diag, 2), None);
        let Value::Object(cleared) =
            serde_json::json!({ "render_adapter_preference": { "preferred": null } })
        else {
            unreachable!()
        };
        assert_eq!(reported_preference(&cleared), Some(None));
        assert_eq!(reported_preference(&Map::new()), None);
    }

    #[test]
    fn a_luid_round_trips_through_the_packed_form() {
        let l = Luid {
            low: 0xFFFF_0001,
            high: -2,
        };
        assert_eq!(Luid::from_packed(l.packed()), l);
        assert_eq!(l.to_string(), "FFFFFFFE:FFFF0001");
    }
}
