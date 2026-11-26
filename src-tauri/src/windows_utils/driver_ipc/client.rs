use std::{
    io,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use serde::Serialize;
use tokio::{
    net::windows::named_pipe,
    sync::{broadcast, Notify, RwLock},
    task,
    time::timeout,
};
use tokio_stream::{Stream, StreamExt};

use crate::driver_ipc::*;

// terminates every message on the pipe
pub(crate) const EOF: u8 = 0x4;

/// A local pipe write takes microseconds, but unbounded it retries forever while the caller
/// holds the virtual display's `commit` lock (and the cross-process lock in shared-driver mode).
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// Driver events (monitor arrivals, mode changes) burst: at 10 one burst could drop updates.
const EVENT_CAPACITY: usize = 64;

/// How long a reply owed to a request that gave up waiting is still expected.
///
/// An idle driver answers in under a millisecond, so a reply this late is never coming (the
/// request was dropped, or its reply could not be decoded). Waiting for it would make every
/// later request skip its own answer as "late" and time out for the life of the connection.
/// A merely slow reply (seconds) is still recognised as late.
#[cfg(not(test))]
const LOST_REPLY_AFTER: Duration = Duration::from_secs(30);
#[cfg(test)]
const LOST_REPLY_AFTER: Duration = Duration::from_millis(300);

/// Named-pipe client for the Virtual Display Driver: sends changes and receives its events.
///
/// Clones share one connection.
#[derive(Debug)]
pub struct Client {
    shared: Arc<_Shared>,
    command_rx: broadcast::Receiver<Result<Received, error::ReceiveError>>,
}

/// One decoded message from the driver, with its place among the replies of its kind.
#[derive(Debug, Clone)]
struct Received {
    /// 1-based ordinal among this connection's state replies (or its v2 replies); 0 for
    /// anything else. The protocol has no correlation id and the driver answers in order, so
    /// the n-th reply answers the n-th request and a late one is recognisable as late.
    reply_seq: u64,
    command: Incoming,
}

#[derive(Debug, Clone)]
enum Incoming {
    V1(ClientCommand),
    /// A reply only the versioned pipe sends, or why it could not be decoded.
    V2(Result<v2::Reply, String>),
}

/// How long a driver gets to answer `Hello` before the host stops trying the versioned pipe.
#[cfg(not(test))]
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
const HELLO_TIMEOUT: Duration = Duration::from_millis(500);

/// How a connection introduces itself on the versioned pipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Handshake {
    /// How long the `Hello` reply may take.
    pub hello_timeout: Duration,
    /// Whether a failed `Hello` may switch this process to the unversioned pipe for good. Never
    /// for a host whose displays the driver owns: landing there without the token is what lets
    /// the driver remove displays that are still streaming.
    pub may_give_up: bool,
    /// Act as if the driver served no versioned pipe (a developer hook).
    pub skip_v2: bool,
    /// Fall back to the unversioned pipe when the versioned one does not answer. Off for a
    /// probe that only wants to know what the versioned pipe says.
    pub fallback_v1: bool,
    /// Open the versioned pipe and treat the `Hello` as unanswered without sending it (a
    /// developer hook: a driver that does not answer in time, deterministically).
    pub unanswered: bool,
}

impl Handshake {
    /// A host's first connection.
    pub const FIRST: Self = Self {
        hello_timeout: HELLO_TIMEOUT,
        may_give_up: true,
        skip_v2: false,
        fallback_v1: true,
        unanswered: false,
    };

    /// `doctor`: ask the versioned pipe, and touch nothing else.
    pub const PROBE: Self = Self {
        hello_timeout: HELLO_TIMEOUT,
        may_give_up: false,
        skip_v2: false,
        fallback_v1: false,
        unanswered: false,
    };

    /// A reconnect: the token has to reach the driver well inside the grace period, and a slow
    /// answer is tried again rather than given up on.
    pub const RELINK: Self = Self {
        hello_timeout: RELINK_HELLO_TIMEOUT,
        may_give_up: false,
        skip_v2: false,
        fallback_v1: true,
        unanswered: false,
    };
}

/// A reconnect's `Hello` timeout. Against an idle driver the answer takes about a millisecond.
pub const RELINK_HELLO_TIMEOUT: Duration = Duration::from_secs(1);

/// How long any later v2 request waits for its answer.
pub const V2_TIMEOUT: Duration = Duration::from_secs(5);

/// Base pipe names whose versioned pipe failed a `Hello` in this process: their connections
/// use the unversioned pipe from then on, instead of asking again on every reconnect.
static V2_REFUSED: Mutex<Option<std::collections::HashSet<String>>> = Mutex::new(None);

fn v2_refused(base: &str) -> bool {
    V2_REFUSED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|set| set.contains(base))
}

fn refuse_v2(base: &str) {
    V2_REFUSED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(Default::default)
        .insert(base.to_string());
}

#[derive(Debug)]
struct _Shared {
    client: named_pipe::NamedPipeClient,
    abort_receiver: Notify,
    receive_error: RwLock<Option<Arc<io::Error>>>,
    /// State replies received on this connection.
    replies: AtomicU64,
    /// State requests written on this connection.
    requests: AtomicU64,
    /// One state request in flight per connection, so a reply's ordinal names one request.
    request_lock: tokio::sync::Mutex<()>,
    /// When the last request that gave up waiting did so, while its reply is still owed.
    gave_up_at: Mutex<Option<Instant>>,
    /// This is the versioned pipe, so a message named like a v2 reply is decoded as one.
    v2: bool,
    /// The same three, for v2 requests and their replies.
    v2_replies: AtomicU64,
    v2_requests: AtomicU64,
    v2_gave_up_at: Mutex<Option<Instant>>,
}

/// The reply counter a request should start from once the replies owed to earlier requests
/// are taken to be lost; `None` to leave it alone.
///
/// Never on a single timeout: a reply merely later than 5 s would then count as the next
/// request's answer, the stale list this ordering exists to refuse.
fn lost_reply_baseline(requests: u64, replies: u64, gave_up_ago: Option<Duration>) -> Option<u64> {
    match gave_up_ago {
        Some(ago) if ago >= LOST_REPLY_AFTER && replies < requests => Some(requests),
        _ => None,
    }
}

/// Takes a request's ordinal back if the request is dropped or fails before it is written, so
/// the counter only ever counts requests the driver can have seen.
struct Ordinal<'a> {
    requests: &'a AtomicU64,
    written: bool,
}

impl Drop for Ordinal<'_> {
    fn drop(&mut self) {
        if !self.written {
            self.requests.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

impl Client {
    /// Connect on [DEFAULT_PIPE_NAME]. Async only because it needs a running tokio reactor.
    pub async fn connect() -> Result<Self, error::ConnectionError> {
        Self::connect_to(DEFAULT_PIPE_NAME).await
    }

    /// Connect, preferring a driver that speaks [`PROTOCOL_VERSION`].
    ///
    /// The negotiation is which pipe exists (`{name}.v{PROTOCOL_VERSION}`, else the unversioned
    /// one), never a message: the shipped driver is a signed third-party binary, and how it
    /// reacts to a message it cannot decode is unknown.
    ///
    /// Returns the connection and the protocol version it negotiated.
    pub async fn connect_negotiated(base: &str) -> Result<(Self, u32), error::ConnectionError> {
        let versioned = versioned_pipe_name(base);
        // no `Path::exists` first: it connects too, taking the pipe instance this open needs
        match Self::open_versioned(&versioned).await {
            Ok(client) => {
                tprintln!("[driver-ipc] negotiated protocol v{PROTOCOL_VERSION}");
                return Ok((client, PROTOCOL_VERSION));
            }
            Err(e) if is_absent(&e) => {}
            Err(e) => teprintln!(
                "[driver-ipc] the v{PROTOCOL_VERSION} pipe exists but did not accept a \
                 connection ({e}); falling back to the unversioned pipe"
            ),
        }
        Ok((Self::connect_to(base).await?, 1))
    }

    /// Connect and introduce this host with `hello` on the versioned pipe.
    ///
    /// Any failure (no versioned pipe, a refused connection, a `Hello` refused, malformed or not
    /// answered within `how.hello_timeout`) lands on the unversioned pipe with nothing new sent,
    /// or returns the error when `how.fallback_v1` is off. With `how.may_give_up`, a failed
    /// `Hello` is remembered: later such connections skip the versioned pipe of `base`.
    pub async fn connect_handshake(
        base: &str,
        hello: &v2::Hello,
        how: Handshake,
    ) -> Result<(Self, v2::Negotiated), error::ConnectionError> {
        let versioned = versioned_pipe_name(base);
        let mut absent = false;
        let why = if how.skip_v2 {
            absent = true;
            "the versioned pipe is skipped (SE_DRIVER_FIRST_LINK_V1)".to_string()
        } else if how.may_give_up && v2_refused(base) {
            "the versioned pipe failed an earlier Hello in this process".to_string()
        } else {
            // opened directly: `Path::exists` connects too and takes the instance this open needs
            match Self::open_versioned(&versioned).await {
                Err(e) if is_absent(&e) => {
                    absent = true;
                    "the driver serves no versioned pipe".to_string()
                }
                Err(e) => format!("the versioned pipe did not accept a connection ({e})"),
                Ok(client) => {
                    let answer = if how.unanswered {
                        Err("not answered in time (simulated)".to_string())
                    } else {
                        client
                            .request_v2(&v2::Request::Hello(hello.clone()), how.hello_timeout)
                            .await
                            .map_err(|e| e.to_string())
                            .and_then(v2::negotiate)
                    };
                    match answer {
                        Ok(negotiated) => {
                            tprintln!(
                                "[driver-ipc] protocol v2: driver {} build {}, capabilities [{}]",
                                negotiated.driver.as_deref().unwrap_or("?"),
                                negotiated.build.as_deref().unwrap_or("?"),
                                negotiated.capabilities.names().join(", ")
                            );
                            return Ok((client, negotiated));
                        }
                        Err(e) if how.may_give_up => {
                            refuse_v2(base);
                            format!("Hello on the versioned pipe failed ({e}); not trying it again")
                        }
                        Err(e) => {
                            format!("Hello on the versioned pipe failed ({e}); will try again")
                        }
                    }
                }
            }
        };
        if !how.fallback_v1 {
            return Err(error::ConnectionError::Failed(io::Error::new(
                io::ErrorKind::NotFound,
                why,
            )));
        }
        let client = Self::connect_to(base).await?;
        tprintln!("[driver-ipc] protocol v1: {why}");
        Ok((
            client,
            v2::Negotiated {
                v2_absent: absent,
                ..v2::Negotiated::without_hello(1)
            },
        ))
    }

    /// Connect on the named pipe. `name` is ONLY the {name} portion of \\.\pipe\{name}.
    /// Async only because it needs a running tokio reactor.
    pub async fn connect_to(name: &str) -> Result<Self, error::ConnectionError> {
        Self::connect_pipe(name, false)
    }

    /// Open the versioned pipe, waiting briefly while every instance is taken: the driver
    /// creates the next instance only after a client has connected to the last one.
    async fn open_versioned(name: &str) -> Result<Self, error::ConnectionError> {
        const ERROR_PIPE_BUSY: i32 = 231;
        let mut tries = 0;
        loop {
            match Self::connect_pipe(name, true) {
                Err(error::ConnectionError::Failed(e))
                    if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && tries < 25 =>
                {
                    tries += 1;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                other => return other,
            }
        }
    }

    /// `v2` says whether replies named like v2 replies are to be decoded as such: only the
    /// versioned pipe sends them.
    fn connect_pipe(name: &str, v2: bool) -> Result<Self, error::ConnectionError> {
        let client = named_pipe::ClientOptions::new()
            .read(true)
            .write(true)
            .pipe_mode(named_pipe::PipeMode::Byte)
            .open(format!(r"\\.\pipe\{name}"))?;

        let abort_receiver = Notify::new();

        let shared = Arc::new(_Shared {
            client,
            abort_receiver,
            receive_error: RwLock::new(None),
            replies: AtomicU64::new(0),
            requests: AtomicU64::new(0),
            request_lock: tokio::sync::Mutex::new(()),
            gave_up_at: Mutex::new(None),
            v2,
            v2_replies: AtomicU64::new(0),
            v2_requests: AtomicU64::new(0),
            v2_gave_up_at: Mutex::new(None),
        });

        let (command_tx, command_rx) =
            broadcast::channel::<Result<Received, error::ReceiveError>>(EVENT_CAPACITY);

        {
            let shared = shared.clone();
            task::spawn(async move {
                let r = receive_command(&shared, &command_tx).await;
                if let Err(e) = r {
                    let error = Arc::new(e);
                    shared.receive_error.write().await.replace(error.clone());
                    let _ = command_tx.send(Err(error::ReceiveError(error.clone())));
                }
            });
        }

        Ok(Self { shared, command_rx })
    }

    /// The error that ended the receive task, if one has: the pipe is dead, and a write to it
    /// would only find that out more slowly.
    async fn dead(&self) -> Option<io::Error> {
        self.shared
            .receive_error
            .read()
            .await
            .as_ref()
            // always `BrokenPipe`, whatever ended the read: `TimedOut` on a send means a live
            // driver that is not draining the pipe (see `send_command`), and a caller decides
            // whether to replace the connection on exactly that difference
            .map(|e| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    format!("the driver pipe has closed: {e}"),
                )
            })
    }

    /// Send new state to the driver.
    pub async fn notify(&self, monitors: &[Monitor]) -> Result<(), error::SendError> {
        if let Some(e) = self.dead().await {
            return Err(error::SendError::PipeBroken(e));
        }
        let command = DriverCommand::Notify(monitors.to_owned());

        send_command(&self.shared.client, &command).await?;
        Ok(())
    }

    /// Remove all monitors with the specified IDs.
    pub async fn remove(&self, ids: &[Id]) -> Result<(), error::SendError> {
        if let Some(e) = self.dead().await {
            return Err(error::SendError::PipeBroken(e));
        }
        let command = DriverCommand::Remove(ids.to_owned());

        send_command(&self.shared.client, &command).await?;
        Ok(())
    }

    /// Remove all monitors.
    pub async fn remove_all(&self) -> Result<(), error::SendError> {
        if let Some(e) = self.dead().await {
            return Err(error::SendError::PipeBroken(e));
        }
        let command = DriverCommand::RemoveAll;

        send_command(&self.shared.client, &command).await?;
        Ok(())
    }

    /// Request the current state of the driver; a `Timeout` error if it does not respond within
    /// 5 seconds.
    pub async fn request_state(&self) -> Result<Vec<Monitor>, error::RequestError> {
        use broadcast::error::RecvError;

        let _one_at_a_time = self.shared.request_lock.lock().await;
        if let Some(e) = self.dead().await {
            return Err(error::RequestError::Send(e));
        }

        forget_lost_replies(
            "state",
            &self.shared.requests,
            &self.shared.replies,
            &self.shared.gave_up_at,
        );

        let mut rx = self.command_rx.resubscribe();

        // counted before the write, so the receive task never sees a reply to a request it does
        // not know was made; taken back if the write does not happen
        let mine = self.shared.requests.fetch_add(1, Ordering::SeqCst) + 1;
        let mut ordinal = Ordinal {
            requests: &self.shared.requests,
            written: false,
        };
        send_command(&self.shared.client, &RequestCommand::State).await?;
        ordinal.written = true;
        drop(ordinal);

        let fut = async {
            loop {
                match rx.recv().await {
                    Ok(Ok(Received {
                        reply_seq,
                        command: Incoming::V1(ClientCommand::Reply(ReplyCommand::State(monitors))),
                    })) => {
                        if reply_seq < mine {
                            // answer to an earlier request that timed out: an older list
                            continue;
                        }
                        break Ok(monitors);
                    }
                    Ok(Err(e)) => break Err(error::RequestError::Receive(e.0.clone())),
                    Ok(_) => continue,
                    Err(RecvError::Lagged(_n)) => continue,
                    Err(RecvError::Closed) => match self.shared.receive_error.read().await.as_ref()
                    {
                        Some(e) => break Err(error::RequestError::Receive(e.clone())),
                        None => {
                            break Err(error::RequestError::Receive(Arc::new(io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                "Pipe closed",
                            ))))
                        }
                    },
                }
            }
        };

        let result = timeout(Duration::from_secs(5), fut).await;
        match result {
            Ok(result) => result,
            Err(_) => {
                // our reply is now owed: still recognised as late if it comes, assumed lost only
                // after `LOST_REPLY_AFTER` (`lost_reply_baseline`)
                *self
                    .shared
                    .gave_up_at
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
                Err(error::RequestError::Timeout(Duration::from_secs(5)))
            }
        }
    }

    /// Send one v2 request and wait for its answer.
    ///
    /// Only on the versioned pipe, and only after the caller has checked the capability: this
    /// layer knows the framing, [`DriverClient`] knows what the driver offered. An `Error` reply
    /// comes back as [`error::V2Error::Refused`].
    pub async fn request_v2(
        &self,
        request: &v2::Request,
        wait: Duration,
    ) -> Result<v2::Reply, error::V2Error> {
        use broadcast::error::RecvError;

        if !self.shared.v2 {
            return Err(error::V2Error::NotVersioned);
        }
        // one request of either kind in flight: the driver answers in order, and each kind's
        // ordinal only names a request while no other request of that kind is outstanding
        let _one_at_a_time = self.shared.request_lock.lock().await;
        if let Some(e) = self.dead().await {
            return Err(error::V2Error::Send(e));
        }
        forget_lost_replies(
            "v2",
            &self.shared.v2_requests,
            &self.shared.v2_replies,
            &self.shared.v2_gave_up_at,
        );

        let mut rx = self.command_rx.resubscribe();
        let mine = self.shared.v2_requests.fetch_add(1, Ordering::SeqCst) + 1;
        let mut ordinal = Ordinal {
            requests: &self.shared.v2_requests,
            written: false,
        };
        send_command(&self.shared.client, request)
            .await
            .map_err(|e| match e {
                error::SendCommandError::PipeBroken(e) => error::V2Error::Send(e),
                error::SendCommandError::Encode(e) => error::V2Error::Malformed(e.to_string()),
            })?;
        ordinal.written = true;
        drop(ordinal);

        let fut = async {
            loop {
                match rx.recv().await {
                    Ok(Ok(Received {
                        reply_seq,
                        command: Incoming::V2(reply),
                    })) => {
                        if reply_seq < mine {
                            continue;
                        }
                        break reply.map_err(error::V2Error::Malformed);
                    }
                    Ok(Err(e)) => break Err(error::V2Error::Receive(e.0.clone())),
                    Ok(_) | Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => {
                        let e = self.shared.receive_error.read().await.clone();
                        break Err(error::V2Error::Receive(e.unwrap_or_else(|| {
                            Arc::new(io::Error::new(io::ErrorKind::BrokenPipe, "Pipe closed"))
                        })));
                    }
                }
            }
        };
        match timeout(wait, fut).await {
            Ok(Ok(v2::Reply::Error(e))) => Err(error::V2Error::Refused {
                request: e.request,
                code: e.code,
                message: e.message,
            }),
            Ok(result) => result,
            Err(_) => {
                *self
                    .shared
                    .v2_gave_up_at
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
                Err(error::V2Error::Timeout(wait))
            }
        }
    }

    /// Whether the receive task has ended with an error: the pipe is gone.
    pub fn is_dead(&self) -> bool {
        match self.shared.receive_error.try_read() {
            Ok(e) => e.is_some(),
            // only the receive task takes the write lock, and only to record its own end
            Err(_) => true,
        }
    }

    /// A handle that can be waited on, repeatedly and with a deadline, for this connection to end.
    pub fn closed_watch(&self) -> ClosedWatch {
        ClosedWatch(self.command_rx.resubscribe())
    }

    /// Resolves once this connection's receive task has ended: the pipe died, or the last copy
    /// of the client was dropped. Holds no reference to the connection, so waiting on it never
    /// keeps a pipe open.
    pub fn closed(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let mut rx = self.command_rx.resubscribe();
        async move {
            loop {
                if let Err(broadcast::error::RecvError::Closed) = rx.recv().await {
                    return;
                }
            }
        }
    }

    /// Stream of the driver events that arrive after this call. May be called more than once.
    /// The stream closes only after every copy of this client is dropped.
    pub fn receive_events(&self) -> impl Stream<Item = Result<EventCommand, error::ReceiveError>> {
        self.receive_events_or_lag().filter_map(|item| match item {
            Ok(EventOrLag::Event(event)) => Some(Ok(event)),
            Ok(EventOrLag::Lagged(_)) => None,
            Err(e) => Some(Err(e)),
        })
    }

    /// The same stream, but with the lag itself as an item rather than a log line.
    ///
    /// A consumer that caches driver state cannot ignore a gap: the cache is now wrong, and this
    /// is its only cue to ask the driver again.
    pub fn receive_events_or_lag(
        &self,
    ) -> impl Stream<Item = Result<EventOrLag, error::ReceiveError>> {
        use tokio_stream::wrappers::*;

        let stream = BroadcastStream::new(self.command_rx.resubscribe());

        stream.filter_map(|cmd| match cmd {
            Ok(Ok(Received {
                command: Incoming::V1(ClientCommand::Event(event)),
                ..
            })) => Some(Ok(EventOrLag::Event(event))),
            Ok(Err(e)) => Some(Err(e)),
            Err(errors::BroadcastStreamRecvError::Lagged(n)) => {
                teprintln!("[driver-ipc] dropped {n} driver events; resynchronising");
                Some(Ok(EventOrLag::Lagged(n)))
            }
            _ => None,
        })
    }

    /// Write `monitors` to the current user's registry: the driver loads them the next time it
    /// starts (a reboot or a driver restart).
    pub fn persist(monitors: &[Monitor]) -> Result<(), error::PersistError> {
        use winreg::*;

        let hklm = RegKey::predef(enums::HKEY_CURRENT_USER);
        let key = r"SOFTWARE\VirtualDisplayDriver";

        let mut reg_key = hklm.open_subkey_with_flags(key, enums::KEY_WRITE);

        if reg_key.is_err() {
            reg_key = hklm.create_subkey(key).map(|(key, _)| key);

            if let Err(e) = reg_key {
                return Err(error::PersistError::Open(e));
            }
        }

        let reg_key = reg_key.unwrap();

        let data = serde_json::to_string(monitors)?;

        reg_key
            .set_value("data", &data)
            .map_err(error::PersistError::Set)?;

        Ok(())
    }
}

impl Clone for Client {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
            command_rx: self.command_rx.resubscribe(),
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // the receive task holds the other reference. `notify_one` stores a permit when the task
        // is between reads (not parked on `notified()`); `notify_waiters` would lose the abort
        // there and leave the pipe open until the driver next sent something
        if Arc::strong_count(&self.shared) == 2 {
            self.shared.abort_receiver.notify_one();
        }
    }
}

/// See [`Client::closed_watch`]. Holds no reference to the connection.
pub struct ClosedWatch(broadcast::Receiver<Result<Received, error::ReceiveError>>);

impl ClosedWatch {
    /// Resolves once the connection's receive task has ended.
    pub async fn closed(&mut self) {
        loop {
            if let Err(broadcast::error::RecvError::Closed) = self.0.recv().await {
                return;
            }
        }
    }
}

/// Before a request: if the replies earlier requests of this kind are owed have been owed past
/// [`LOST_REPLY_AFTER`], stop waiting for them (see [`lost_reply_baseline`]).
fn forget_lost_replies(
    kind: &str,
    requests: &AtomicU64,
    replies: &AtomicU64,
    gave_up_at: &Mutex<Option<Instant>>,
) {
    let mut gave_up = gave_up_at.lock().unwrap_or_else(|e| e.into_inner());
    let asked = requests.load(Ordering::SeqCst);
    let answered = replies.load(Ordering::SeqCst);
    let ago = gave_up.map(|at| at.elapsed());
    if let Some(base) = lost_reply_baseline(asked, answered, ago) {
        teprintln!(
            "[driver-ipc] {} owed {kind} reply(ies) never came in {:?}; no longer waiting for \
             them",
            base - answered,
            LOST_REPLY_AFTER
        );
        // `fetch_max`, never a store: a reply the receive task counts in the meantime must not
        // be counted twice
        replies.fetch_max(base, Ordering::SeqCst);
    }
    if answered >= asked || ago.is_some_and(|a| a >= LOST_REPLY_AFTER) {
        *gave_up = None;
    }
}

async fn send_command(
    client: &named_pipe::NamedPipeClient,
    command: &impl Serialize,
) -> Result<(), error::SendCommandError> {
    // one message per write: the driver splits on the EOF byte, so messages must not interleave
    let mut message = serde_json::to_vec(command)?;
    message.push(EOF);

    // outside the future, so a timeout can still tell "nothing went out" from "half a message
    // went out"
    let mut written = 0;
    let write_all = async {
        loop {
            client.writable().await?;

            match client.try_write(&message[written..]) {
                Ok(n) if written + n < message.len() => {
                    written += n;
                    continue;
                }

                Ok(n) if written + n >= message.len() => {
                    break;
                }

                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    continue;
                }

                Err(e) => {
                    return Err(e);
                }

                _ => unreachable!(),
            }
        }
        Ok(())
    };

    let outcome = timeout(WRITE_TIMEOUT, write_all).await;
    match outcome {
        Ok(result) => result.map_err(Into::into),
        // nothing went out: the driver is alive but not draining the pipe. The connection is
        // still in step, which `TimedOut` tells a caller (`SendError::is_busy`)
        Err(_) if written == 0 => Err(error::SendCommandError::PipeBroken(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "the display driver did not accept a write within {:?}",
                WRITE_TIMEOUT
            ),
        ))),
        // part of a message went out: the driver splits on the EOF byte, so whatever is sent
        // next is glued to the fragment. The connection is unusable, hence `BrokenPipe`
        Err(_) => Err(error::SendCommandError::PipeBroken(io::Error::new(
            io::ErrorKind::BrokenPipe,
            format!(
                "the display driver took {written} of {} bytes and then stopped reading for {:?}; \
                 the connection is out of step",
                message.len(),
                WRITE_TIMEOUT
            ),
        ))),
    }
}

// the receive task: decodes every message off the pipe and broadcasts it
async fn receive_command(
    shared: &_Shared,
    tx: &broadcast::Sender<Result<Received, error::ReceiveError>>,
) -> Result<(), io::Error> {
    let client = &shared.client;
    let abort = &shared.abort_receiver;
    let mut buf = vec![0; 4096];
    let mut recv_buf = Vec::with_capacity(4096);

    loop {
        tokio::select! {
            r = client.readable() => r?,
            _ = abort.notified() => return Ok(()),
        }

        match client.try_read(&mut buf) {
            // zero-length read: the driver closed the pipe. Named here because GetLastError is
            // stale at this point and can read "The operation completed successfully"
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "the display driver closed the pipe",
                ))
            }
            Ok(n) => recv_buf.extend(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        }

        let eof_iter =
            recv_buf.iter().enumerate().filter_map(
                |(i, &byte)| {
                    if byte == EOF {
                        Some(i)
                    } else {
                        None
                    }
                },
            );

        let mut offset = 0;
        for pos in eof_iter {
            let data = &recv_buf[offset..pos];
            offset = pos + 1;

            // on the versioned pipe a message named like a v2 reply is decoded strictly as one;
            // everything else goes to the untagged v1 enums
            let v2_reply = if shared.v2 {
                v2::decode_reply(data)
            } else {
                None
            };
            if let Some(Err(e)) = &v2_reply {
                let preview = String::from_utf8_lossy(&data[..data.len().min(160)]);
                teprintln!("[driver-ipc] undecodable v2 reply ({e}): {preview}");
            }
            let command = match v2_reply {
                Some(reply) => Incoming::V2(reply),
                None => match serde_json::from_slice::<ClientCommand>(data) {
                    Ok(command) => Incoming::V1(command),
                    Err(e) => {
                        // ClientCommand is #[serde(untagged)] with no version, so this may be
                        // corruption or a message a newer driver added: never dropped silently
                        let preview = String::from_utf8_lossy(&data[..data.len().min(160)]);
                        teprintln!(
                            "[driver-ipc] dropped an undecodable driver message ({e}): {preview}"
                        );
                        continue;
                    }
                },
            };

            let reply_seq = match command {
                Incoming::V1(ClientCommand::Reply(ReplyCommand::State(_))) => {
                    next_reply_seq(&shared.replies, &shared.requests)
                }
                Incoming::V2(_) => next_reply_seq(&shared.v2_replies, &shared.v2_requests),
                _ => 0,
            };
            if tx.send(Ok(Received { reply_seq, command })).is_err() {
                // no receivers: every client was dropped
                return Ok(());
            }
        }

        recv_buf.drain(..offset);
    }
}

/// No such pipe: nothing serves that name.
fn is_absent(e: &error::ConnectionError) -> bool {
    let error::ConnectionError::Failed(e) = e;
    matches!(e.raw_os_error(), Some(2) | Some(3))
}

/// Count one reply and return its ordinal.
///
/// Requests are counted before they are written, so replies outnumber them only if the driver
/// sends an unsolicited one. The counter is then clamped back to the requests rather than
/// carrying the excess forever.
fn next_reply_seq(replies: &AtomicU64, requests: &AtomicU64) -> u64 {
    let seq = replies.fetch_add(1, Ordering::SeqCst) + 1;
    let asked = requests.load(Ordering::SeqCst);
    if seq > asked {
        replies.fetch_min(asked, Ordering::SeqCst);
        asked
    } else {
        seq
    }
}

pub mod error {
    use super::*;
    use thiserror::Error;

    /// Error returned from [Client::connect] and [Client::connect_to].
    #[derive(Debug, Error)]
    pub enum ConnectionError {
        #[error("Failed to open pipe: {0}")]
        Failed(#[from] io::Error),
    }

    /// Error returned from [send_command]
    #[derive(Debug, Error)]
    pub(super) enum SendCommandError {
        #[error("Failed to encode message: {0}")]
        Encode(#[from] serde_json::Error),
        #[error("Failed to send message: {0}")]
        PipeBroken(#[from] io::Error),
    }

    /// Error returned from [Client::notify], [Client::remove] and [Client::remove_all].
    #[derive(Debug, Error)]
    pub enum SendError {
        #[error("Failed to send message: {0}")]
        PipeBroken(#[from] io::Error),
    }

    /// Error returned from [Client::request_state].
    #[derive(Debug, Error)]
    pub enum RequestError {
        #[error("Failed to send message (pipe broken): {0}")]
        Send(io::Error),
        #[error("Failed to receive message (pipe broken): {0}")]
        Receive(Arc<io::Error>),
        #[error("Did not get a response in time ({0:?})")]
        Timeout(Duration),
    }

    /// Error returned from a v2 request.
    #[derive(Debug, Error)]
    pub enum V2Error {
        #[error("the driver did not offer `{0}` on this connection")]
        NotOffered(&'static str),
        #[error("this connection is not on the versioned pipe")]
        NotVersioned,
        #[error("Failed to send message (pipe broken): {0}")]
        Send(io::Error),
        #[error("Failed to receive message (pipe broken): {0}")]
        Receive(Arc<io::Error>),
        #[error("Did not get a response in time ({0:?})")]
        Timeout(Duration),
        #[error("the driver refused {request}: {code} ({message})")]
        Refused {
            request: String,
            code: String,
            message: String,
        },
        #[error("undecodable reply: {0}")]
        Malformed(String),
        #[error("unexpected reply: {0}")]
        Unexpected(&'static str),
    }

    impl V2Error {
        /// The driver's error code, when it answered with one.
        pub fn code(&self) -> Option<&str> {
            match self {
                Self::Refused { code, .. } => Some(code),
                _ => None,
            }
        }
    }

    /// Error returned from [Client::receive_events].
    #[derive(Debug, Error, Clone)]
    #[error("Failed to receive event: {0}")]
    pub struct ReceiveError(#[from] pub Arc<io::Error>);

    /// Error returned from [Client::persist].
    #[derive(Debug, Error)]
    pub enum PersistError {
        #[error("Failed to open registry key: {0}")]
        Open(io::Error),
        #[error("Failed to set registry value: {0}")]
        Set(io::Error),
        #[error("Failed to serialize monitors: {0}")]
        Serialize(#[from] serde_json::Error),
    }

    impl SendError {
        /// The driver is alive and did not take the message within the write deadline, and
        /// nothing of it went out: the connection is still in step and worth keeping. Every
        /// other failure means the connection itself is gone or out of step.
        pub fn is_busy(&self) -> bool {
            matches!(self, Self::PipeBroken(e) if e.kind() == io::ErrorKind::TimedOut)
        }
    }

    impl From<SendCommandError> for SendError {
        fn from(e: SendCommandError) -> Self {
            match e {
                SendCommandError::PipeBroken(e) => Self::PipeBroken(e),
                SendCommandError::Encode(e) => unreachable!("{:?}", e),
            }
        }
    }

    impl From<SendCommandError> for RequestError {
        fn from(e: SendCommandError) -> Self {
            match e {
                SendCommandError::PipeBroken(e) => Self::Send(e),
                SendCommandError::Encode(e) => unreachable!("{:?}", e),
            }
        }
    }
}

#[cfg(test)]
mod test {
    use tokio::time::sleep;

    use super::*;
    use crate::driver_ipc::mock::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn receiver_stops_when_client_closed() {
        const PIPE_NAME: &str = "virtualdisplaydriver-test-receiver_stops_when_client_closed";

        let mut server = MockServer::new(PIPE_NAME);

        let client1 = Client::connect_to(PIPE_NAME)
            .await
            .expect("Failed to connect to pipe");
        let stream1 = client1.receive_events();

        let client2 = client1.clone();
        let stream2 = client2.receive_events();

        client1.notify(&[]).await.expect("Failed to notify");
        server.pump().await;

        sleep(Duration::from_millis(50)).await;

        drop(client1);

        client2.notify(&[]).await.expect("Failed to notify");
        server.pump().await;

        sleep(Duration::from_millis(50)).await;

        drop(client2);

        let events1: Vec<_> = stream1.collect().await;
        let events2: Vec<_> = stream2.collect().await;

        assert_eq!(events1.len(), 2, "{:?}", events1);
        assert_eq!(events2.len(), 2, "{:?}", events2);
    }

    /// A consumer that falls behind must be told: `DriverClient` caches its monitor list off
    /// this stream, and the gap is the only moment the cache can know it has gone wrong.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn a_lagging_consumer_is_told_how_much_it_missed() {
        const PIPE_NAME: &str = "virtualdisplaydriver-test-lag_is_surfaced";

        let mut server = MockServer::new(PIPE_NAME);
        let client = Client::connect_to(PIPE_NAME)
            .await
            .expect("Failed to connect to pipe");

        // subscribed but never polled, so the broadcast overruns it
        let stream = client.receive_events_or_lag();
        tokio::pin!(stream);

        for _ in 0..(EVENT_CAPACITY * 2) {
            client.notify(&[]).await.expect("Failed to notify");
            server.pump().await;
        }
        sleep(Duration::from_millis(50)).await;

        let first = stream.next().await.expect("stream ended");
        assert!(
            matches!(first, Ok(EventOrLag::Lagged(n)) if n > 0),
            "expected a lag report, got {first:?}"
        );
    }

    /// A reply to a request that gave up waiting arrives while the next request waits,
    /// and must not be taken for the next request's answer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_late_reply_does_not_answer_the_next_request() {
        const PIPE_NAME: &str = "virtualdisplaydriver-test-late_reply_is_not_the_answer";

        let mut server = MockServer::new(PIPE_NAME);
        let client = Client::connect_to(PIPE_NAME)
            .await
            .expect("Failed to connect to pipe");

        // nobody pumps: the first request times out with its reply still owed
        assert!(matches!(
            client.request_state().await,
            Err(error::RequestError::Timeout(_))
        ));

        let mons = [Monitor {
            id: 0,
            enabled: true,
            name: Some("after".to_string()),
            modes: vec![Mode {
                width: 1920,
                height: 1080,
                refresh_rates: vec![60],
            }],
        }];
        client.notify(&mons).await.expect("Failed to notify");

        // the pipe now holds the owed request, the notify, and the new request. The driver
        // answers the first with the old (empty) list after the new request has subscribed
        let (state, _) = tokio::join!(client.request_state(), async {
            server.pump().await;
            server.pump().await;
            server.pump().await;
        });
        assert_eq!(state.expect("the second request is answered"), mons);
    }

    /// A reply that is merely slow stays "late" (it must never answer a newer
    /// request); only one owed for `LOST_REPLY_AFTER` is given up on, so a driver that dropped a
    /// request does not leave every later request timing out.
    #[test]
    fn an_owed_reply_is_given_up_on_only_after_the_deadline() {
        let soon = Some(LOST_REPLY_AFTER / 10);
        let late = Some(LOST_REPLY_AFTER);
        // one reply owed, asked again within the window: keep waiting for it
        assert_eq!(lost_reply_baseline(2, 1, soon), None);
        // owed past the window: it is not coming
        assert_eq!(lost_reply_baseline(2, 1, late), Some(2));
        assert_eq!(lost_reply_baseline(5, 2, late), Some(5));
        // nothing owed, or no request ever gave up: nothing to do
        assert_eq!(lost_reply_baseline(2, 2, late), None);
        assert_eq!(lost_reply_baseline(2, 1, None), None);
    }

    /// A write that timed out with nothing sent is "busy" (keep the connection);
    /// a closed pipe or a half-written message is not.
    #[test]
    fn only_an_untouched_write_timeout_is_busy() {
        let err = |kind| error::SendError::PipeBroken(io::Error::new(kind, "x"));
        assert!(err(io::ErrorKind::TimedOut).is_busy());
        assert!(!err(io::ErrorKind::BrokenPipe).is_busy());
        assert!(!err(io::ErrorKind::Other).is_busy());
    }

    /// The pipe dying is reported as `BrokenPipe` by every later send, never as a timeout, so
    /// the virtual display replaces the connection instead of waiting on a dead one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dead_pipe_is_never_busy() {
        const PIPE_NAME: &str = "virtualdisplaydriver-test-dead_pipe_not_busy";

        let server = MockServer::new(PIPE_NAME);
        let client = Client::connect_to(PIPE_NAME)
            .await
            .expect("Failed to connect to pipe");
        drop(server);
        let mut last = None;
        for _ in 0..50 {
            match client.notify(&[]).await {
                Ok(()) => tokio::time::sleep(Duration::from_millis(20)).await,
                Err(e) => {
                    last = Some(e);
                    break;
                }
            }
        }
        let e = last.expect("a send to a closed pipe fails");
        assert!(!e.is_busy(), "{e}");
    }

    #[test]
    fn the_versioned_pipe_is_a_suffix_of_the_shipped_one() {
        // a driver that predates versioning never serves this name, so the host does not find
        // it and connects on the unversioned pipe
        assert_eq!(
            versioned_pipe_name("virtualdisplaydriver"),
            format!("virtualdisplaydriver.v{PROTOCOL_VERSION}")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn an_unversioned_driver_still_connects_and_reports_v1() {
        const PIPE_NAME: &str = "virtualdisplaydriver-test-negotiate_falls_back";

        let _server = MockServer::new(PIPE_NAME);
        let (_client, protocol) = Client::connect_negotiated(PIPE_NAME)
            .await
            .expect("Failed to connect to pipe");
        assert_eq!(protocol, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn receiver_stops_when_server_closed() {
        const PIPE_NAME: &str = "virtualdisplaydriver-test-receiver_stops_when_server_closed";

        let server = MockServer::new(PIPE_NAME);

        let client = Client::connect_to(PIPE_NAME)
            .await
            .expect("Failed to connect to pipe");

        let stream = client.receive_events();

        sleep(Duration::from_millis(50)).await;

        drop(server);

        let events: Vec<_> = stream.collect().await;

        tprintln!("{:?}", events);

        assert!(
            matches!(events[..], [Err(error::ReceiveError(ref e))] if e.kind() == io::ErrorKind::BrokenPipe)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn general_test_1() {
        const PIPE_NAME: &str = "virtualdisplaydriver-test-general_test_1";

        let mut server = MockServer::new(PIPE_NAME);

        let client = Client::connect_to(PIPE_NAME)
            .await
            .expect("Failed to connect to pipe");

        let stream = client.receive_events();

        server.check_next(|cmd| {
            assert!(matches!(cmd, ServerCommand::Request(RequestCommand::State)));
        });

        let (state, _) = tokio::join!(client.request_state(), server.pump());

        let state = state.expect("Failed to request state");
        assert!(state.is_empty());

        let mons1 = [Monitor {
            id: 0,
            enabled: true,
            name: Some("test".to_string()),
            modes: vec![Mode {
                width: 1920,
                height: 1080,
                refresh_rates: vec![60],
            }],
        }];

        let fut = client.notify(&mons1);

        tokio::join!(fut, server.pump())
            .0
            .expect("Failed to notify");

        assert_eq!(server.state(), &mons1);

        let (state, _) = tokio::join!(client.request_state(), server.pump());

        let state = state.expect("Failed to request state");
        assert_eq!(&state, &mons1);

        let mons2 = [
            Monitor {
                id: 0,
                enabled: false,
                name: Some("test1".to_string()),
                modes: vec![Mode {
                    width: 100,
                    height: 200,
                    refresh_rates: vec![80, 90],
                }],
            },
            Monitor {
                id: 1,
                enabled: true,
                name: Some("test2".to_string()),
                modes: vec![Mode {
                    width: 300,
                    height: 400,
                    refresh_rates: vec![50],
                }],
            },
        ];

        tokio::join!(client.notify(&mons2), server.pump())
            .0
            .expect("Failed to notify");

        assert_eq!(server.state(), &mons2);

        // let the server send the last event
        sleep(Duration::from_millis(50)).await;

        let stream2 = client.receive_events();

        tokio::join!(client.remove(&[0]), server.pump())
            .0
            .expect("Failed to remove");

        assert_eq!(server.state().len(), 1);
        assert_eq!(server.state()[0].id, 1);

        tokio::join!(client.remove_all(), server.pump())
            .0
            .expect("Failed to remove all");

        assert!(server.state().is_empty());

        // let the server send the last event
        sleep(Duration::from_millis(50)).await;

        drop(client);

        let events: Vec<_> = stream.collect().await;

        assert!(matches!(events[..], [
                Ok(EventCommand::Changed(ref e1)),
                Ok(EventCommand::Changed(ref e2)),
                Ok(EventCommand::Changed(ref e3)),
                Ok(EventCommand::Changed(ref e4)),
            ] if *e1 == mons1
                && *e2 == mons2
                && *e3 == mons2[1..]
                && e4.is_empty()
        ));

        let events: Vec<_> = stream2.collect().await;

        assert!(matches!(events[..], [
                Ok(EventCommand::Changed(ref e1)),
                Ok(EventCommand::Changed(ref e2)),
            ] if  *e1 == mons2[1..]
                && e2.is_empty()
        ));
    }
}
