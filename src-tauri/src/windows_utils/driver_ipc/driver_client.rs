use std::collections::HashSet;
use std::sync::Arc;

use tokio::{sync::watch, task};
use tokio_stream::{Stream, StreamExt};

use crate::driver_ipc::client::V2_TIMEOUT;
use crate::driver_ipc::*;

/// Abstraction layer over [Client] that keeps its own copy of the monitor list (the local
/// state).
///
/// Edits change only the local state: [DriverClient::notify] sends it to the driver and
/// [DriverClient::persist] saves it across reboots. It can be stale, and is brought up to date
/// with the driver only by [DriverClient::refresh_state].
#[derive(Debug)]
pub struct DriverClient {
    client: Client,
    state_rx: watch::Receiver<Vec<Monitor>>,
    state: Vec<Monitor>,
    /// What this connection agreed with the driver: the protocol (1 for the shipped driver,
    /// higher for one that serves the versioned pipe) and, after a `Hello`, the capabilities
    /// every newer request is gated on.
    negotiated: Arc<v2::Negotiated>,
    /// Shared by every duplicate; the last one dropped stops the resync task.
    _resync: Arc<AbortOnDrop>,
}

/// Aborts the cache-resync task when the last [`DriverClient`] sharing it is dropped.
///
/// The task holds a `Client` of its own, so left running it keeps a pipe instance open in the
/// driver's host until the driver next sends an event, which on an idle machine is never.
#[derive(Debug)]
struct AbortOnDrop(task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl DriverClient {
    /// Connect on [DEFAULT_PIPE_NAME].
    pub async fn new() -> Result<Self, error::InitError> {
        Self::new_with(DEFAULT_PIPE_NAME).await
    }

    /// Connect on the named pipe. `name` is ONLY the {name} portion of \\.\pipe\{name}.
    pub async fn new_with(name: &str) -> Result<Self, error::InitError> {
        let (client, protocol) = Client::connect_negotiated(name).await?;
        Self::from_client(client, v2::Negotiated::without_hello(protocol)).await
    }

    /// Connect as a host: with `hello`, introduce this connection on the versioned pipe first
    /// ([`Client::connect_handshake`]); without, exactly [`DriverClient::new_with`].
    pub async fn new_host(
        name: &str,
        hello: Option<&v2::Hello>,
        how: Handshake,
    ) -> Result<Self, error::InitError> {
        match hello {
            None => Self::new_with(name).await,
            Some(hello) => {
                let (client, negotiated) = Client::connect_handshake(name, hello, how).await?;
                Self::from_client(client, negotiated).await
            }
        }
    }

    async fn from_client(
        client: Client,
        negotiated: v2::Negotiated,
    ) -> Result<Self, error::InitError> {
        let current_state = client.request_state().await?;

        let (state_tx, state_rx) = watch::channel(current_state.clone());

        let mut stream = client.receive_events_or_lag();

        // own handle for the cache reader: after a dropped `Changed` the cache is wrong, and
        // asking the driver again is the only way back (`refresh_state` reads this same cache)
        let resync = client.clone();
        let resync_task = task::spawn(async move {
            while let Some(event) = stream.next().await {
                let value = match event {
                    Ok(EventOrLag::Event(EventCommand::Changed(value))) => value,
                    Ok(EventOrLag::Lagged(_)) => match resync.request_state().await {
                        Ok(state) => state,
                        Err(e) => {
                            teprintln!("[driver-ipc] resync after dropped events failed: {e}");
                            continue;
                        }
                    },
                    Err(_) => continue,
                };
                if state_tx.send(value).is_err() {
                    // every client was dropped
                    break;
                }
            }
        });

        Ok(Self {
            client,
            state_rx,
            state: current_state,
            negotiated: Arc::new(negotiated),
            _resync: Arc::new(AbortOnDrop(resync_task)),
        })
    }

    /// What this connection agreed with the driver.
    pub fn negotiated(&self) -> &v2::Negotiated {
        &self.negotiated
    }

    /// The driver's diagnostics snapshot. Only on a connection whose driver offered them.
    pub async fn diagnostics(
        &self,
    ) -> Result<serde_json::Map<String, serde_json::Value>, error::V2Error> {
        self.require(v2::CAP_DIAGNOSTICS)?;
        match self
            .client
            .request_v2(&v2::Request::Diagnostics(v2::Empty {}), V2_TIMEOUT)
            .await?
        {
            v2::Reply::Diagnostics(d) => Ok(d),
            other => Err(error::V2Error::Unexpected(other.name())),
        }
    }

    /// Ask the driver to compose its monitors on `luid` (`None` clears the preference). Only on
    /// a connection whose driver offered `render_adapter`.
    pub async fn set_render_adapter(
        &self,
        luid: Option<v2::Luid>,
    ) -> Result<v2::RenderAdapterReply, error::V2Error> {
        self.set_render_adapter_within(luid, V2_TIMEOUT).await
    }

    /// [`DriverClient::set_render_adapter`], waiting at most `wait` for the answer.
    pub async fn set_render_adapter_within(
        &self,
        luid: Option<v2::Luid>,
        wait: std::time::Duration,
    ) -> Result<v2::RenderAdapterReply, error::V2Error> {
        self.require(v2::CAP_RENDER_ADAPTER)?;
        let request = v2::Request::SetRenderAdapter(v2::SetRenderAdapter { luid });
        match self.client.request_v2(&request, wait).await? {
            v2::Reply::RenderAdapter(r) => Ok(r),
            other => Err(error::V2Error::Unexpected(other.name())),
        }
    }

    fn require(&self, capability: &'static str) -> Result<(), error::V2Error> {
        if self.negotiated.has(capability) {
            Ok(())
        } else {
            Err(error::V2Error::NotOffered(capability))
        }
    }

    /// Whether this connection's pipe has died.
    pub fn is_dead(&self) -> bool {
        self.client.is_dead()
    }

    /// See [`Client::closed_watch`].
    pub fn closed_watch(&self) -> ClosedWatch {
        self.client.closed_watch()
    }

    /// See [`Client::closed`].
    pub fn closed(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        self.client.closed()
    }

    /// Get the ID of a monitor from a query: a monitor name or an ID. The name has precedence.
    ///
    /// ### Example
    /// ```ignore
    /// // Monitors are:
    /// //   { id: 0, name: Some("foo") }
    /// //   { id: 1, name: Some("bar") }
    /// //   { id: 2, name: Some("1") }
    ///
    /// assert_eq!(client.find_id("foo"), Some(0));
    /// assert_eq!(client.find_id("bar"), Some(1));
    /// assert_eq!(client.find_id("1"), Some(2));
    /// assert_eq!(client.find_id("0"), Some(0));
    /// assert_eq!(client.find_id("baz"), None);
    /// ```
    pub fn find_id(&self, query: &str) -> Option<Id> {
        let id = query.parse::<Id>();

        for monitor in self.monitors().iter() {
            if let Some(name) = monitor.name.as_deref() {
                if name == query {
                    return Some(monitor.id);
                }
            }

            if let Ok(id) = id {
                if monitor.id == id {
                    return Some(monitor.id);
                }
            }
        }

        None
    }

    /// The wire protocol this connection negotiated.
    pub fn protocol_version(&self) -> u32 {
        self.negotiated.protocol
    }

    /// Ask the driver for its monitor list now, bypassing the event-fed cache. The shipped
    /// driver does not send `Changed` to the client that made a change, so the cache cannot
    /// confirm a commit; shared-driver mode (`virtual_display::shared`) reads this instead.
    pub async fn request_live_state(&self) -> Result<Vec<Monitor>, String> {
        self.client.request_state().await.map_err(|e| e.to_string())
    }

    /// Bring the local state up to date from the event-fed cache (no request is sent).
    pub fn refresh_state(&mut self) -> &[Monitor] {
        self.state = self.state_rx.borrow().clone();

        &self.state
    }

    /// Stream of driver events, including changes made by other processes. It closes only after
    /// every copy of this client ([DriverClient::duplicate]) is dropped.
    pub fn receive_events(&self) -> impl Stream<Item = Result<EventCommand, error::ReceiveError>> {
        self.client.receive_events()
    }

    /// The monitor state stored inside this client (the local state).
    pub fn monitors(&self) -> &[Monitor] {
        &self.state
    }

    /// Replace all monitors.
    pub fn set_monitors(&mut self, monitors: &[Monitor]) -> Result<(), error::DuplicateError> {
        mons_have_duplicates(monitors)?;

        self.state = monitors.to_owned();
        Ok(())
    }

    /// Replace an existing monitor, identified by its ID. Errors if it does not exist.
    pub fn replace_monitor(&mut self, monitor: Monitor) -> Result<(), error::MonNotFound> {
        match self.state.iter_mut().find(|m| m.id == monitor.id) {
            Some(m) => {
                *m = monitor;
                Ok(())
            }
            None => Err(error::MonNotFound(monitor.id)),
        }
    }

    /// Send the local state to the driver. Edits reach the driver no other way.
    pub async fn notify(&mut self) -> Result<(), error::SendError> {
        self.client.notify(&self.state).await
    }

    /// Find the monitor with the given ID.
    pub fn find_monitor(&self, id: Id) -> Option<&Monitor> {
        self.state.iter().find(|monitor| monitor.id == id)
    }

    /// Find the monitor matched by the given query.
    pub fn find_monitor_query(&self, query: &str) -> Option<&Monitor> {
        let id = self.find_id(query)?;
        self.find_monitor(id)
    }

    /// TODO: may leave client in an invalid state
    ///
    /// Find a monitor by ID and call `cb` with a mutable reference to it.
    pub fn find_monitor_mut<R>(&mut self, id: Id, cb: impl FnOnce(&mut Monitor) -> R) -> Option<R> {
        let monitor = self.state.iter_mut().find(|monitor| monitor.id == id)?;

        let r = cb(monitor);

        mons_have_duplicates(&self.state).ok()?;

        Some(r)
    }

    /// Find a monitor by ID and return a mutable reference to it.
    ///
    /// "Unchecked" means no duplicate check (a returned `&mut` cannot be checked afterwards), not
    /// unsafe: the caller must leave no duplicates.
    pub fn find_monitor_mut_unchecked(&mut self, id: Id) -> Option<&mut Monitor> {
        self.state.iter_mut().find(|monitor| monitor.id == id)
    }

    /// Find the monitor matched by the given query and call `cb` with a mutable reference to it.
    pub fn find_monitor_mut_query<R>(
        &mut self,
        query: &str,
        cb: impl FnOnce(&mut Monitor) -> R,
    ) -> Option<R> {
        let id = self.find_id(query)?;
        self.find_monitor_mut(id, cb)
    }

    /// Find the monitor matching the given query and return a mutable reference to it.
    ///
    /// "Unchecked" means no duplicate check (a returned `&mut` cannot be checked afterwards), not
    /// unsafe: the caller must leave no duplicates.
    pub fn find_monitor_mut_query_unchecked(&mut self, query: &str) -> Option<&mut Monitor> {
        let id = self.find_id(query)?;
        self.find_monitor_mut_unchecked(id)
    }

    /// Write the local state to the current user's registry: the driver loads it the next time
    /// it starts (a reboot or a driver restart).
    pub fn persist(&self) -> Result<(), error::PersistError> {
        Client::persist(&self.state)
    }

    /// Get the closest available free ID.
    ///
    /// The driver ignores duplicate monitors sent with [DriverClient::notify].
    pub fn new_id(&self, preferred_id: Option<Id>) -> Option<Id> {
        let existing_ids = self
            .state
            .iter()
            .map(|monitor| monitor.id)
            .collect::<HashSet<_>>();

        if let Some(id) = preferred_id {
            if !existing_ids.contains(&id) {
                return None;
            }

            Some(id)
        } else {
            #[allow(clippy::maybe_infinite_iter)]
            let new_id = (0..)
                .find(|id| !existing_ids.contains(id))
                .expect("failed to get a new ID");
            Some(new_id)
        }
    }

    /// Remove monitors by id, silently skipping IDs that do not exist.
    pub fn remove(&mut self, ids: &[Id]) {
        self.state.retain(|mon| !ids.contains(&mon.id));
    }

    /// Remove all monitors matched by the given queries. Errors if a query matches no monitor.
    pub fn remove_query(
        &mut self,
        queries: &[impl AsRef<str>],
    ) -> Result<(), error::QueryNotFound> {
        let mut ids = Vec::new();
        for id in queries {
            if let Some(id) = self.find_id(id.as_ref()) {
                ids.push(id);
                continue;
            }

            return Err(error::QueryNotFound(id.as_ref().to_owned()));
        }

        self.remove(&ids);
        Ok(())
    }

    /// Remove all monitors.
    pub fn remove_all(&mut self) {
        self.state.clear();
    }

    /// Add a new monitor.
    ///
    /// Errors if a monitor with this ID already exists, or if the monitor has duplicate modes or
    /// a mode with duplicate refresh rates.
    pub fn add(&mut self, monitor: Monitor) -> Result<(), error::DuplicateError> {
        if self.state.iter().any(|mon| mon.id == monitor.id) {
            return Err(error::DuplicateError::Monitor(monitor.id));
        }
        mon_has_duplicates(&monitor)?;

        self.state.push(monitor);

        Ok(())
    }

    /// Set enabled state of all monitors with the given IDs, silently skipping incorrect IDs.
    pub fn set_enabled(&mut self, ids: &[Id], enabled: bool) {
        for mon in self.state.iter_mut() {
            if ids.contains(&mon.id) {
                mon.enabled = enabled;
                continue;
            }
        }
    }

    /// Set enabled state of all monitors matched by the given queries. Errors if a query matches
    /// no monitor.
    pub fn set_enabled_query(
        &mut self,
        queries: &[impl AsRef<str>],
        enabled: bool,
    ) -> Result<(), error::QueryNotFound> {
        let mut ids = Vec::new();
        for id in queries {
            if let Some(id) = self.find_id(id.as_ref()) {
                ids.push(id);
                continue;
            }

            return Err(error::QueryNotFound(id.as_ref().to_owned()));
        }

        self.set_enabled(&ids, enabled);
        Ok(())
    }

    /// Add a mode to the monitor with the given ID.
    ///
    /// Errors if the monitor does not exist, if the mode already exists on it, or if the mode
    /// has duplicate refresh rates.
    pub fn add_mode(&mut self, id: Id, mode: Mode) -> Result<(), error::AddModeError> {
        let Some(mon) = self.state.iter_mut().find(|mon| mon.id == id) else {
            return Err(error::AddModeError::MonNotFound(id));
        };

        match mode_has_duplicates(&mode, id) {
            Ok(_) => Ok(()),
            Err(error::DuplicateError::RefreshRate(rr, w, h, id)) => {
                Err(error::AddModeError::DupRefreshRate(rr, w, h, id))
            }
            Err(_) => unreachable!(),
        }?;

        if mon
            .modes
            .iter()
            .any(|_mode| _mode.height == mode.height && _mode.width == mode.width)
        {
            return Err(error::AddModeError::DupMode(mode.width, mode.height, id));
        }

        let iter = mode.refresh_rates.iter().copied();

        for (i, &rr) in mode.refresh_rates.iter().enumerate() {
            if iter.clone().skip(i).any(|_rr| _rr == rr) {
                return Err(error::AddModeError::DupRefreshRate(
                    rr,
                    mode.width,
                    mode.height,
                    id,
                ));
            }
        }

        mon.modes.push(mode);

        Ok(())
    }

    /// Add a mode to the monitor matched by the given query.
    ///
    /// Errors if the monitor cannot be found, if the mode already exists on it, or if the mode
    /// has duplicate refresh rates.
    pub fn add_mode_query(
        &mut self,
        query: &str,
        mode: Mode,
    ) -> Result<(), error::AddModeQueryError> {
        let id = self
            .find_id(query)
            .ok_or_else(|| error::AddModeQueryError::QueryNotFound(query.to_owned()))?;

        match self.add_mode(id, mode) {
            Ok(_) => Ok(()),
            Err(error::AddModeError::MonNotFound(_)) => {
                unreachable!("Mon must exist")
            }
            Err(error::AddModeError::DupMode(w, h, id)) => {
                Err(error::AddModeQueryError::DupMode(id, w, h))
            }
            Err(error::AddModeError::DupRefreshRate(rr, w, h, id)) => {
                Err(error::AddModeQueryError::DupRefreshRate(rr, w, h, id))
            }
        }?;

        Ok(())
    }

    /// Remove a mode from the monitor with the given ID. Errors if the monitor does not exist;
    /// a mode that does not exist is silently skipped.
    pub fn remove_mode(
        &mut self,
        id: Id,
        resolution: (u32, u32),
    ) -> Result<(), error::MonNotFound> {
        let Some(mon) = self.state.iter_mut().find(|mon| mon.id == id) else {
            return Err(error::MonNotFound(id));
        };

        mon.modes
            .retain(|mode| !(mode.width == resolution.0 && mode.height == resolution.1));

        Ok(())
    }

    /// Remove a mode from the monitor matched by the given query. Errors if the monitor cannot
    /// be found; a mode that does not exist is silently skipped.
    pub fn remove_mode_query(
        &mut self,
        query: &str,
        resolution: (u32, u32),
    ) -> Result<(), error::QueryNotFound> {
        let id = self
            .find_id(query)
            .ok_or_else(|| error::QueryNotFound(query.to_owned()))?;

        self.remove_mode(id, resolution)
            .expect("Monitor must exist");

        Ok(())
    }

    /// Returns a copy of this client with its own independent state.
    ///
    /// Event streams from [DriverClient::receive_events] close only after every copy is
    /// dropped.
    pub fn duplicate(&self) -> Self {
        Self {
            client: self.client.clone(),
            state_rx: self.state_rx.clone(),
            state: self.state.clone(),
            negotiated: self.negotiated.clone(),
            _resync: self._resync.clone(),
        }
    }
}

fn mons_have_duplicates(monitors: &[Monitor]) -> Result<(), error::DuplicateError> {
    let mut monitor_iter = monitors.iter();
    while let Some(monitor) = monitor_iter.next() {
        let duplicate_id = monitor_iter.clone().any(|b| monitor.id == b.id);
        if duplicate_id {
            return Err(error::DuplicateError::Monitor(monitor.id));
        }

        mon_has_duplicates(monitor)?;
    }

    Ok(())
}

fn mon_has_duplicates(monitor: &Monitor) -> Result<(), error::DuplicateError> {
    let mut mode_iter = monitor.modes.iter();
    while let Some(mode) = mode_iter.next() {
        let duplicate_mode = mode_iter
            .clone()
            .any(|m| mode.height == m.height && mode.width == m.width);
        if duplicate_mode {
            return Err(error::DuplicateError::Mode(
                mode.width,
                mode.height,
                monitor.id,
            ));
        }

        mode_has_duplicates(mode, monitor.id)?;
    }

    Ok(())
}

fn mode_has_duplicates(mode: &Mode, id: Id) -> Result<(), error::DuplicateError> {
    let mut refresh_iter = mode.refresh_rates.iter().copied();
    while let Some(rr) = refresh_iter.next() {
        let duplicate_rr = refresh_iter.clone().any(|r| rr == r);
        if duplicate_rr {
            return Err(error::DuplicateError::RefreshRate(
                rr,
                mode.width,
                mode.height,
                id,
            ));
        }
    }

    Ok(())
}

pub mod error {
    use super::*;
    pub use crate::driver_ipc::client::error::*;
    use thiserror::Error;

    #[derive(Debug, Error)]
    pub enum DuplicateError {
        #[error("Duplicate monitor with ID {0}")]
        Monitor(Id),
        #[error("Duplicate mode {1}x{2} on monitor {0}")]
        Mode(u32, u32, Id),
        #[error("Duplicate refresh rate {0} on mode {1}x{2} on monitor {3}")]
        RefreshRate(u32, u32, u32, Id),
    }

    #[derive(Debug, Error)]
    #[error("Query not found: {0}")]
    pub struct QueryNotFound(pub String);

    #[derive(Debug, Error)]
    #[error("Monitor not found: {0}")]
    pub struct MonNotFound(pub Id);

    /// Error returned from [DriverClient::add_mode].
    #[derive(Debug, Error)]
    pub enum AddModeError {
        #[error("Monitor not found: {0}")]
        MonNotFound(Id),
        #[error("Duplicate mode {1}x{2} on monitor {0}")]
        DupMode(Id, u32, u32),
        #[error("Duplicate refresh rate {0} on mode {1}x{2} on monitor {3}")]
        DupRefreshRate(u32, u32, u32, Id),
    }

    /// Error returned from [DriverClient::add_mode_query].
    #[derive(Debug, Error)]
    pub enum AddModeQueryError {
        #[error("Query not found: {0}")]
        QueryNotFound(String),
        #[error("Duplicate mode {1}x{2} on monitor {0}")]
        DupMode(Id, u32, u32),
        #[error("Duplicate refresh rate {0} on mode {1}x{2} on monitor {3}")]
        DupRefreshRate(u32, u32, u32, Id),
    }

    /// Error returned from [DriverClient::new] and [DriverClient::new_with].
    #[derive(Debug, Error)]
    pub enum InitError {
        #[error("Failed to connect to driver: {0}")]
        Connect(#[from] ConnectionError),
        #[error("Failed to request state: {0}")]
        RequestState(#[from] RequestError),
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::driver_ipc::mock::MockServer;
    use std::time::Duration;

    /// Dropping the last `DriverClient` must close its pipe instance, though the resync task
    /// holds a `Client` of its own. Duplicates share the connection, so it closes only when
    /// the last one goes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_the_last_driver_client_closes_its_pipe() {
        const PIPE_NAME: &str = "virtualdisplaydriver-test-drop_closes_pipe";

        let mut server = MockServer::new(PIPE_NAME);
        let mut disconnected = server.disconnected();
        let (client, _) = tokio::join!(DriverClient::new_with(PIPE_NAME), server.pump());
        let client = client.expect("Failed to connect to pipe");
        let copy = client.duplicate();

        drop(client);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !*disconnected.borrow(),
            "a duplicate still holds the connection"
        );

        drop(copy);
        tokio::time::timeout(Duration::from_secs(2), disconnected.wait_for(|gone| *gone))
            .await
            .expect("the pipe was still open after the last client was dropped")
            .expect("the mock's watch closed");
    }

    use crate::driver_ipc::mock::{HelloMode, V2Behaviour, V2Mock};

    fn owner_hello() -> v2::Hello {
        v2::Hello {
            client: Some("test".into()),
            owner: Some(v2::Owner {
                token: v2::OwnerToken::generate().unwrap(),
                grace_ms: 5000,
            }),
        }
    }

    fn v2_requests(mock: &V2Mock) -> Vec<String> {
        mock.received()
            .iter()
            .filter(|m| m.is_v2_request())
            .map(|m| m.name())
            .collect()
    }

    /// Whatever subset of capabilities a driver lists is exactly what the connection may use:
    /// a request whose capability is missing fails here without a byte reaching the pipe.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn negotiation_follows_each_capability_subset() {
        let all = [
            v2::CAP_HELLO,
            v2::CAP_DIAGNOSTICS,
            v2::CAP_RENDER_ADAPTER,
            v2::CAP_OWNED_MONITORS,
        ];
        for mask in 0..(1u32 << all.len()) {
            let caps: Vec<&'static str> = all
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, c)| *c)
                .collect();
            let name = format!("screenextend-test-v2-caps-{}-{mask}", std::process::id());
            let mock = V2Mock::new(
                &name,
                V2Behaviour {
                    capabilities: caps.clone(),
                    ..Default::default()
                },
            );
            let client = DriverClient::new_host(&name, Some(&owner_hello()), Handshake::FIRST)
                .await
                .expect("connect");
            let n = client.negotiated();
            assert_eq!(n.protocol, 2, "{caps:?}");
            assert_eq!(n.capabilities.names(), {
                let mut c = caps.clone();
                c.sort_unstable();
                c
            });
            assert_eq!(n.owns_monitors(), caps.contains(&v2::CAP_OWNED_MONITORS));

            let diag = client.diagnostics().await;
            assert_eq!(
                diag.is_ok(),
                caps.contains(&v2::CAP_DIAGNOSTICS),
                "{caps:?}"
            );
            let render = client.set_render_adapter(None).await;
            assert_eq!(
                render.is_ok(),
                caps.contains(&v2::CAP_RENDER_ADAPTER),
                "{caps:?}"
            );
            if !caps.contains(&v2::CAP_RENDER_ADAPTER) {
                assert!(matches!(render, Err(error::V2Error::NotOffered(_))));
            }
            let mut expected = vec!["Hello".to_string()];
            if caps.contains(&v2::CAP_DIAGNOSTICS) {
                expected.push("Diagnostics".into());
            }
            if caps.contains(&v2::CAP_RENDER_ADAPTER) {
                expected.push("SetRenderAdapter".into());
            }
            assert_eq!(v2_requests(&mock), expected, "{caps:?}");
        }
    }

    /// `doctor`'s probe on the shipped driver: no versioned pipe, so no connection at all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_probe_handshake_never_falls_back_to_v1() {
        let name = format!("screenextend-test-v2-probe-{}", std::process::id());
        let mock = V2Mock::new(
            &name,
            V2Behaviour {
                serve_v2: false,
                ..Default::default()
            },
        );
        let probe = DriverClient::new_host(&name, Some(&owner_hello()), Handshake::PROBE).await;
        assert!(probe.is_err());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(mock.received().is_empty(), "{:?}", mock.received());
    }

    /// The shipped driver serves no versioned pipe: the host is on v1 and sends it nothing new.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_old_driver_gets_v1_and_nothing_new() {
        let name = format!("screenextend-test-v2-old-{}", std::process::id());
        let mock = V2Mock::new(
            &name,
            V2Behaviour {
                serve_v2: false,
                ..Default::default()
            },
        );
        let client = DriverClient::new_host(&name, Some(&owner_hello()), Handshake::FIRST)
            .await
            .expect("connect");
        assert_eq!(client.negotiated().protocol, 1);
        assert!(!client.negotiated().owns_monitors());
        assert!(matches!(
            client.diagnostics().await,
            Err(error::V2Error::NotOffered(_))
        ));
        assert!(client.request_live_state().await.is_ok());
        let received = mock.received();
        assert!(!received.is_empty());
        assert!(received.iter().all(|m| !m.on_v2_pipe && !m.is_v2_request()));
    }

    /// No `Hello` at all (both switches off) on a driver that serves the versioned pipe: the
    /// connection is on that pipe, with v1 messages only.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn without_hello_nothing_new_is_sent() {
        let name = format!("screenextend-test-v2-nohello-{}", std::process::id());
        let mock = V2Mock::new(&name, V2Behaviour::default());
        let client = DriverClient::new_host(&name, None, Handshake::FIRST)
            .await
            .expect("connect");
        assert_eq!(client.negotiated().protocol, 2);
        assert!(client.negotiated().capabilities.names().is_empty());
        assert!(client.diagnostics().await.is_err());
        assert!(client.set_render_adapter(None).await.is_err());
        assert!(client.request_live_state().await.is_ok());
        assert!(mock.received().iter().all(|m| !m.is_v2_request()));
    }

    /// A versioned pipe that never answers `Hello`: the host gives up, lands on v1, and does not
    /// knock on the versioned pipe again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_silent_hello_falls_back_to_v1_and_is_not_retried() {
        let name = format!("screenextend-test-v2-silent-{}", std::process::id());
        let mock = V2Mock::new(
            &name,
            V2Behaviour {
                hello: HelloMode::Silent,
                ..Default::default()
            },
        );
        let started = std::time::Instant::now();
        let first = DriverClient::new_host(&name, Some(&owner_hello()), Handshake::FIRST)
            .await
            .expect("connect");
        assert_eq!(first.negotiated().protocol, 1);
        assert!(started.elapsed() >= Duration::from_millis(400));
        let second = DriverClient::new_host(&name, Some(&owner_hello()), Handshake::FIRST)
            .await
            .expect("connect again");
        assert_eq!(second.negotiated().protocol, 1);
        assert_eq!(v2_requests(&mock), vec!["Hello".to_string()]);
        let on_v1 = mock.received().iter().filter(|m| !m.on_v2_pipe).count();
        assert!(on_v1 >= 2, "both connections ended up on the v1 pipe");
    }

    /// A driver that closes the connection half way through its `Hello` reply.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_hello_cut_off_mid_reply_falls_back_to_v1() {
        let name = format!("screenextend-test-v2-cut-{}", std::process::id());
        let mock = V2Mock::new(
            &name,
            V2Behaviour {
                hello: HelloMode::CloseMidReply,
                ..Default::default()
            },
        );
        let client = DriverClient::new_host(&name, Some(&owner_hello()), Handshake::FIRST)
            .await
            .expect("connect");
        assert_eq!(client.negotiated().protocol, 1);
        assert!(client.request_live_state().await.is_ok());
        assert!(mock
            .received()
            .iter()
            .any(|m| !m.on_v2_pipe && m.name() == "State"));
    }

    /// `Changed` events from other clients land between requests and their replies. Every
    /// request still gets its own answer, and the events still reach the event stream.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn replies_stay_in_order_with_events_in_between() {
        let name = format!("screenextend-test-v2-order-{}", std::process::id());
        let mock = V2Mock::new(
            &name,
            V2Behaviour {
                changed_before_reply: true,
                ..Default::default()
            },
        );
        let client = DriverClient::new_host(&name, Some(&owner_hello()), Handshake::FIRST)
            .await
            .expect("connect");
        assert_eq!(client.negotiated().protocol, 2);
        let events = client.receive_events();
        tokio::pin!(events);

        let luid = v2::Luid {
            low: 97388,
            high: 0,
        };
        let diag = client.diagnostics().await.expect("diagnostics");
        assert_eq!(v2::reported_preference(&diag), Some(None));
        assert!(client.request_live_state().await.unwrap().is_empty());
        let set = client.set_render_adapter(Some(luid)).await.expect("set");
        assert_eq!(set.preferred, Some(luid));
        let diag = client.diagnostics().await.expect("diagnostics again");
        assert_eq!(v2::reported_preference(&diag), Some(Some(luid)));
        assert_eq!(mock.preferred(), Some(luid));

        for _ in 0..3 {
            let event = tokio::time::timeout(Duration::from_secs(2), events.next())
                .await
                .expect("the events reached the stream");
            assert!(matches!(event, Some(Ok(EventCommand::Changed(_)))));
        }
    }

    /// Every error code the driver can answer `SetRenderAdapter` with comes back as a refusal
    /// carrying that code, never as a timeout or a mis-decoded reply.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn render_adapter_errors_carry_their_code() {
        for code in [
            "unknown_adapter",
            "software_adapter",
            "remote_adapter",
            "device_failed",
            "not_supported",
            "driver_error",
            "something_new",
        ] {
            let name = format!("screenextend-test-v2-err-{}-{code}", std::process::id());
            let _mock = V2Mock::new(
                &name,
                V2Behaviour {
                    render_error: Some(code),
                    ..Default::default()
                },
            );
            let client = DriverClient::new_host(&name, Some(&owner_hello()), Handshake::FIRST)
                .await
                .expect("connect");
            let e = client
                .set_render_adapter(Some(v2::Luid { low: 1, high: 0 }))
                .await
                .expect_err("refused");
            assert_eq!(e.code(), Some(code));
            // the connection is still in step
            assert!(client.diagnostics().await.is_ok());
        }
    }
}
