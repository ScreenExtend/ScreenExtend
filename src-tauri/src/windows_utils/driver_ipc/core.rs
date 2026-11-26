use serde::{Deserialize, Serialize};

pub type Id = u32;
pub type Dimen = u32;
pub type RefreshRate = u32;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, PartialOrd)]
pub struct Monitor {
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
    // all one-way, client->server
    //
    // adds or updates monitors
    Notify(Vec<Monitor>),
    Remove(Vec<Id>),
    RemoveAll,
}

/// Request command sent from client->server
#[non_exhaustive]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum RequestCommand {
    // the driver's current monitor list
    State,
}

/// Reply command sent from server->client
#[non_exhaustive]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum ReplyCommand {
    State(Vec<Monitor>),
}

/// Event command sent from server->client
#[non_exhaustive]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum EventCommand {
    // monitor state changed while the client was connected
    Changed(Vec<Monitor>),
}

/// One item from the event stream: an event, or how many were dropped because the consumer
/// fell behind. The gap must be visible: `DriverClient` caches the monitor list from this
/// stream, so a dropped `Changed` leaves the cache wrong until the driver is asked again.
#[derive(Debug, Clone)]
pub enum EventOrLag {
    Event(EventCommand),
    Lagged(u64),
}

/// The wire protocol this build speaks.
///
/// One JSON value per message, terminated by `0x04`, over a named pipe. Both sides decode with
/// `#[serde(untagged)]` enums, so a near-miss is mis-decoded rather than rejected and a new
/// message type looks like corruption. The shipped driver is a signed third-party binary, so
/// that format is fixed; versioning is by pipe name instead. A newer driver also serves
/// [`versioned_pipe_name`]; an older one lacks it and the host falls back to the unversioned
/// pipe. Nothing new is ever sent to an old driver: how it reacts to a message it cannot
/// decode is untested.
pub const PROTOCOL_VERSION: u32 = 2;

/// The pipe a driver speaking [`PROTOCOL_VERSION`] would serve, alongside the unversioned one.
pub fn versioned_pipe_name(base: &str) -> String {
    format!("{base}.v{PROTOCOL_VERSION}")
}

/// Anything the server can receive, as one untagged enum for decoding.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServerCommand {
    Driver(DriverCommand),
    Request(RequestCommand),
}

/// Anything the client can receive, as one untagged enum for decoding.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ClientCommand {
    Reply(ReplyCommand),
    Event(EventCommand),
}
