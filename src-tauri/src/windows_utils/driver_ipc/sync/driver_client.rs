use super::{client::EventsSubscription, RUNTIME};
use crate::driver_ipc::{
    driver_client::error, v2, ClosedWatch, DriverClient as AsyncDriverClient, EventCommand,
    Handshake, Id, Mode, Monitor,
};

/// Synchronous wrapper over [AsyncDriverClient], blocking on this module's tokio runtime.
///
/// It keeps its own copy of the monitor list (the local state). Edits change only the local
/// state: [DriverClient::notify] sends it to the driver and [DriverClient::persist] saves it
/// across reboots. It can be stale, and is brought up to date with the driver only by
/// [DriverClient::refresh_state].
#[derive(Debug)]
pub struct DriverClient(AsyncDriverClient);

impl DriverClient {
    /// Connect on the default pipe name, `DEFAULT_PIPE_NAME`.
    pub fn new() -> Result<Self, error::InitError> {
        let client = RUNTIME.block_on(AsyncDriverClient::new());
        client.map(Self)
    }

    /// Connect on the named pipe. `name` is ONLY the {name} portion of \\.\pipe\{name}.
    pub fn new_with(name: &str) -> Result<Self, error::InitError> {
        let client = RUNTIME.block_on(AsyncDriverClient::new_with(name));
        client.map(Self)
    }

    /// See the async [`AsyncDriverClient::new_host`].
    pub fn new_host(
        name: &str,
        hello: Option<&v2::Hello>,
        how: Handshake,
    ) -> Result<Self, error::InitError> {
        RUNTIME
            .block_on(AsyncDriverClient::new_host(name, hello, how))
            .map(Self)
    }

    pub fn closed_watch(&self) -> ClosedWatch {
        self.0.closed_watch()
    }

    /// Wait up to `max` for the connection `watch` was taken from to end. `true` if it has.
    pub fn wait_closed(watch: &mut ClosedWatch, max: std::time::Duration) -> bool {
        RUNTIME.block_on(async { tokio::time::timeout(max, watch.closed()).await.is_ok() })
    }

    /// What this connection agreed with the driver.
    pub fn negotiated(&self) -> &v2::Negotiated {
        self.0.negotiated()
    }

    pub fn diagnostics(
        &self,
    ) -> Result<serde_json::Map<String, serde_json::Value>, error::V2Error> {
        RUNTIME.block_on(self.0.diagnostics())
    }

    pub fn set_render_adapter(
        &self,
        luid: Option<v2::Luid>,
    ) -> Result<v2::RenderAdapterReply, error::V2Error> {
        RUNTIME.block_on(self.0.set_render_adapter(luid))
    }

    pub fn set_render_adapter_within(
        &self,
        luid: Option<v2::Luid>,
        wait: std::time::Duration,
    ) -> Result<v2::RenderAdapterReply, error::V2Error> {
        RUNTIME.block_on(self.0.set_render_adapter_within(luid, wait))
    }

    pub fn is_dead(&self) -> bool {
        self.0.is_dead()
    }

    /// Block until this connection has ended (see the async `closed`). The wait holds no
    /// reference to the connection.
    pub fn closed_waiter(&self) -> impl FnOnce() + Send + 'static {
        let closed = self.0.closed();
        move || RUNTIME.block_on(closed)
    }

    /// ID of the monitor matching `query`: a monitor name or an ID, the name taking precedence.
    ///
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
        self.0.find_id(query)
    }

    /// Manually synchronize with the driver.
    pub fn refresh_state(&mut self) -> &[Monitor] {
        self.0.refresh_state()
    }

    /// Call `cb` with every event the driver sends, whichever process caused the change. The
    /// returned object cancels the subscription.
    ///
    /// `cb` runs on the library's tokio runtime and delays every other library event until it
    /// returns, so it must return quickly.
    ///
    /// The receiver closes only after every copy made by [DriverClient::duplicate] is dropped.
    pub fn add_event_receiver(
        &self,
        cb: impl FnMut(Result<EventCommand, error::ReceiveError>)
            + Send
            + std::panic::UnwindSafe
            + 'static,
    ) -> EventsSubscription {
        let stream = self.0.receive_events();
        EventsSubscription::start_subscriber(cb, stream)
    }

    /// The monitors in the local state.
    pub fn monitors(&self) -> &[Monitor] {
        self.0.monitors()
    }

    /// Replace all monitors in the local state.
    pub fn set_monitors(&mut self, monitors: &[Monitor]) -> Result<(), error::DuplicateError> {
        self.0.set_monitors(monitors)
    }

    /// Replace the local monitor that has the same ID. Errors if there is none.
    pub fn replace_monitor(&mut self, monitor: Monitor) -> Result<(), error::MonNotFound> {
        self.0.replace_monitor(monitor)
    }

    /// Send the local state to the driver. Nothing reaches the driver until this is called.
    pub fn notify(&mut self) -> Result<(), error::SendError> {
        RUNTIME.block_on(self.0.notify())
    }

    /// The driver's monitor list, asked for now rather than read from the cache.
    pub fn request_live_state(&self) -> Result<Vec<Monitor>, String> {
        RUNTIME.block_on(self.0.request_live_state())
    }

    /// Find the monitor with the given ID.
    pub fn find_monitor(&self, id: Id) -> Option<&Monitor> {
        self.0.find_monitor(id)
    }

    /// Find the monitor matched by the given query.
    pub fn find_monitor_query(&self, query: &str) -> Option<&Monitor> {
        self.0.find_monitor_query(query)
    }

    /// Find a monitor by ID and call `cb` with a mutable reference to it.
    pub fn find_monitor_mut<R>(&mut self, id: Id, cb: impl FnOnce(&mut Monitor) -> R) -> Option<R> {
        self.0.find_monitor_mut(id, cb)
    }

    /// Find a monitor by ID and return a mutable reference to it.
    ///
    /// "Unchecked" means duplicates are not checked (the caller must not introduce any), not
    /// that the method is unsafe.
    pub fn find_monitor_mut_unchecked(&mut self, id: Id) -> Option<&mut Monitor> {
        self.0.find_monitor_mut_unchecked(id)
    }

    /// Find the monitor matched by the given query and call `cb` with a mutable reference to it.
    pub fn find_monitor_mut_query<R>(
        &mut self,
        query: &str,
        cb: impl FnOnce(&mut Monitor) -> R,
    ) -> Option<R> {
        self.0.find_monitor_mut_query(query, cb)
    }

    /// [DriverClient::find_monitor_mut_unchecked], with the monitor matched by the given query.
    pub fn find_monitor_mut_query_unchecked(&mut self, query: &str) -> Option<&mut Monitor> {
        self.0.find_monitor_mut_query_unchecked(query)
    }

    /// Write the local state to the current user's registry, where the driver loads it the
    /// next time it starts (a reboot or a driver restart).
    pub fn persist(&self) -> Result<(), error::PersistError> {
        self.0.persist()
    }

    /// Get the closest available free ID.
    ///
    /// The driver ignores duplicate monitors sent with [DriverClient::notify].
    pub fn new_id(&self, preferred_id: Option<Id>) -> Option<Id> {
        self.0.new_id(preferred_id)
    }

    /// Remove monitors from the local state by ID, silently skipping IDs that do not exist.
    pub fn remove(&mut self, ids: &[Id]) {
        self.0.remove(ids)
    }

    /// Remove the monitors matched by the given queries from the local state. Errors if a query
    /// matches no monitor.
    pub fn remove_query(
        &mut self,
        queries: &[impl AsRef<str>],
    ) -> Result<(), error::QueryNotFound> {
        self.0.remove_query(queries)
    }

    /// Remove all monitors from the local state.
    pub fn remove_all(&mut self) {
        self.0.remove_all()
    }

    /// Add a monitor to the local state.
    ///
    /// Errors if the ID is taken, or if the monitor has duplicate modes or a mode with duplicate
    /// refresh rates.
    pub fn add(&mut self, monitor: Monitor) -> Result<(), error::DuplicateError> {
        self.0.add(monitor)
    }

    /// Set the enabled flag of the local monitors with the given IDs, silently skipping unknown
    /// IDs.
    pub fn set_enabled(&mut self, ids: &[Id], enabled: bool) {
        self.0.set_enabled(ids, enabled)
    }

    /// Set the enabled flag of the local monitors matched by the given queries. Errors if a
    /// query matches no monitor.
    pub fn set_enabled_query(
        &mut self,
        queries: &[impl AsRef<str>],
        enabled: bool,
    ) -> Result<(), error::QueryNotFound> {
        self.0.set_enabled_query(queries, enabled)
    }

    /// Add a mode to the local monitor with the given ID.
    ///
    /// Errors if the monitor does not exist, already has the mode, or the mode has duplicate
    /// refresh rates.
    pub fn add_mode(&mut self, id: Id, mode: Mode) -> Result<(), error::AddModeError> {
        self.0.add_mode(id, mode)
    }

    /// Add a mode to the local monitor matched by the given query.
    ///
    /// Errors if no monitor matches, the monitor already has the mode, or the mode has
    /// duplicate refresh rates.
    pub fn add_mode_query(
        &mut self,
        query: &str,
        mode: Mode,
    ) -> Result<(), error::AddModeQueryError> {
        self.0.add_mode_query(query, mode)
    }

    /// Remove a mode from the local monitor with the given ID. Errors if the monitor does not
    /// exist; a mode that does not exist is silently skipped.
    pub fn remove_mode(
        &mut self,
        id: Id,
        resolution: (u32, u32),
    ) -> Result<(), error::MonNotFound> {
        self.0.remove_mode(id, resolution)
    }

    /// Remove a mode from the local monitor matched by the given query. Errors if no monitor
    /// matches; a mode that does not exist is silently skipped.
    pub fn remove_mode_query(
        &mut self,
        query: &str,
        resolution: (u32, u32),
    ) -> Result<(), error::QueryNotFound> {
        self.0.remove_mode_query(query, resolution)
    }

    /// A copy of this client with its own local state, sharing the connection. Event receivers
    /// from [DriverClient::add_event_receiver] close only after every copy is dropped.
    pub fn duplicate(&self) -> Self {
        Self(self.0.duplicate())
    }
}
