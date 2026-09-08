use serde::{Deserialize, Serialize};

pub type Id = u32;
pub type Dimen = u32;
pub type RefreshRate = u32;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, PartialOrd)]
pub struct Monitor {
    // identifier
    pub id: Id,
    pub name: Option<String>,
    pub enabled: bool,
    pub modes: Vec<Mode>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, PartialOrd)]
pub struct Mode {
    pub width: Dimen,
    pub height: Dimen,
    pub refresh_rates: Vec<RefreshRate>,
}

#[non_exhaustive]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum DriverCommand {
    // Single line of communication client->server
    // Driver commands
    //
    // Notify of monitor changes (whether adding or updating)
    Notify(Vec<Monitor>),
    // Remove a monitor from system
    Remove(Vec<Id>),
    // Remove all monitors from system
    RemoveAll,
}

/// Request command sent from client->server
#[non_exhaustive]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum RequestCommand {
    // Request information on the current system monitor state
    State,
}

/// Reply command sent from server->client
#[non_exhaustive]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum ReplyCommand {
    // Reply to previous current system monitor state request
    State(Vec<Monitor>),
}

/// An event happened
#[non_exhaustive]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum EventCommand {
    // Monitor state was changed while client was connected
    Changed(Vec<Monitor>),
}

/// One item from the event stream: either an event, or the number of events that were dropped
/// because the consumer fell behind.
///
/// The gap has to be observable. `DriverClient` caches the monitor list from this stream and
/// `refresh_state` reads that cache, so a dropped `Changed` leaves the cache disagreeing with
/// the driver and nothing downstream can repair it — the symptom being "the virtual display
/// quietly did not appear, or quietly did not change resolution".
#[derive(Debug, Clone)]
pub enum EventOrLag {
    Event(EventCommand),
    Lagged(u64),
}

/// The wire contract this build speaks.
///
/// The format is one JSON object per message terminated by `0x04` over a named pipe, with
/// `#[serde(untagged)]` enums on both sides — so a message that almost matches a variant is
/// mis-decoded rather than rejected, and a genuinely new message type is indistinguishable from
/// corruption. Fixing that means changing a wire format shared with a signed third-party binary,
/// which cannot be done without rebuilding the driver.
///
/// What *can* be done compatibly is negotiate: a rebuilt driver serves an additional pipe named
/// [`versioned_pipe_name`], the host prefers it, and a driver that predates versioning simply
/// does not have it, so the host falls back to the unversioned pipe and behaves exactly as
/// before. No new message is ever sent to an old driver — which matters, because how the shipped
/// driver reacts to a message it cannot decode is not something this project can test.
pub const PROTOCOL_VERSION: u32 = 2;

/// The pipe a driver speaking [`PROTOCOL_VERSION`] would serve, alongside the unversioned one.
pub fn versioned_pipe_name(base: &str) -> String {
    format!("{base}.v{PROTOCOL_VERSION}")
}

/// An untagged enum of commands to be used with deserialization.
/// This makes the deserialization process much easier to handle
/// when a received command could be of multiple types
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServerCommand {
    Driver(DriverCommand),
    Request(RequestCommand),
}

/// An untagged enum of commands to be used with deserialization.
/// This makes the deserialization process much easier to handle
/// when a received command could be of multiple types
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ClientCommand {
    Reply(ReplyCommand),
    Event(EventCommand),
}
