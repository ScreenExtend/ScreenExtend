use std::{any::Any, panic, thread};

use tokio::sync::{mpsc, oneshot};
use tokio_stream::StreamExt;

use super::RUNTIME;
use crate::driver_ipc::{client::error, Client as AsyncClient, EventCommand, Id, Monitor};

/// Synchronous wrapper over [AsyncClient], blocking on this module's single-worker tokio runtime.
///
/// Clones share one connection.
#[derive(Debug, Clone)]
pub struct Client(AsyncClient);

impl Client {
    /// Connect on the default pipe name, `DEFAULT_PIPE_NAME`.
    pub fn connect() -> Result<Self, error::ConnectionError> {
        let client = RUNTIME.block_on(AsyncClient::connect())?;
        Ok(Self(client))
    }

    /// Connect on the named pipe. `name` is ONLY the {name} portion of \\.\pipe\{name}.
    pub fn connect_to(name: &str) -> Result<Self, error::ConnectionError> {
        let client = RUNTIME.block_on(AsyncClient::connect_to(name))?;
        Ok(Self(client))
    }

    /// Send new state to the driver.
    pub fn notify(&self, monitors: &[Monitor]) -> Result<(), error::SendError> {
        RUNTIME.block_on(self.0.notify(monitors))
    }

    /// Remove all monitors with the specified IDs.
    pub fn remove(&self, ids: &[Id]) -> Result<(), error::SendError> {
        RUNTIME.block_on(self.0.remove(ids))
    }

    /// Remove all monitors.
    pub fn remove_all(&self) -> Result<(), error::SendError> {
        RUNTIME.block_on(self.0.remove_all())
    }

    /// Block until the next driver event that arrives after this call.
    pub fn receive_event(&mut self) -> Result<EventCommand, error::ReceiveError> {
        RUNTIME.block_on(async {
            self.0
                .receive_events()
                .next()
                .await
                .expect("Stream should never finish")
        })
    }

    /// Call `cb` for every driver event; the returned object cancels the subscription.
    ///
    /// `cb` runs on this module's tokio runtime and blocks every other operation until it
    /// returns, so it must be quick. The receiver closes only after every copy of this client
    /// is dropped.
    pub fn add_event_receiver(
        &self,
        cb: impl FnMut(Result<EventCommand, error::ReceiveError>) + Send + panic::UnwindSafe + 'static,
    ) -> EventsSubscription {
        let stream = self.0.receive_events();
        EventsSubscription::start_subscriber(cb, stream)
    }

    /// Request the current state of the driver; a `Timeout` error if it does not respond within
    /// 5 seconds.
    pub fn request_state(&self) -> Result<Vec<Monitor>, error::RequestError> {
        RUNTIME.block_on(self.0.request_state())
    }

    /// Write `monitors` to the current user's registry: the driver loads them the next time it
    /// starts (a reboot or a driver restart).
    pub fn persist(monitors: &[Monitor]) -> Result<(), error::PersistError> {
        AsyncClient::persist(monitors)
    }
}

pub struct EventsSubscription {
    pub(crate) abort_tx: mpsc::Sender<()>,
    result_rx: Option<oneshot::Receiver<Box<dyn Any + Send>>>,
}

impl EventsSubscription {
    pub(crate) fn start_subscriber(
        mut cb: impl FnMut(Result<EventCommand, error::ReceiveError>)
            + Send
            + panic::UnwindSafe
            + 'static,
        mut stream: impl tokio_stream::Stream<Item = Result<EventCommand, error::ReceiveError>>
            + Unpin
            + Send
            + 'static,
    ) -> Self {
        let (abort_tx, mut abort_rx) = mpsc::channel(1);
        let (result_tx, result_rx) = oneshot::channel();

        RUNTIME.spawn(async move {
            while let Some(event) = if abort_rx.is_closed() {
                stream.next().await
            } else {
                tokio::select! {
                    event = stream.next() => event,
                    _ = abort_rx.recv() => None,
                }
            } {
                let mut cb = panic::AssertUnwindSafe(&mut cb);
                let event = panic::AssertUnwindSafe(event);
                let res = panic::catch_unwind(move || {
                    cb(event.clone());
                });
                if let Err(e) = res {
                    if let Err(e) = result_tx.send(e) {
                        teprintln!(
                            "Event receiver panicked, but subscription is already dropped: {:?}",
                            e
                        );
                    };
                    break;
                }
            }
        });

        Self {
            abort_tx,
            result_rx: Some(result_rx),
        }
    }

    /// Cancel the subscription without waiting for the callback.
    ///
    /// `Ok(true)` if this call cancelled it, `Ok(false)` if it was already cancelled, `Err(e)`
    /// once if the callback panicked (later calls return `Ok(false)`). A panic in the callback
    /// after this call is not caught.
    pub fn cancel(&mut self) -> thread::Result<bool> {
        let Some(ref mut rx) = self.result_rx else {
            // already cancelled by `cancel_async`
            return Ok(false);
        };

        match rx.try_recv() {
            Err(oneshot::error::TryRecvError::Empty) => {
                // fails if the buffer (size 1) is full or the receiver is closed: already cancelled
                Ok(self.abort_tx.try_send(()).is_ok())
            }
            // the receiver is spent once it has answered: polling it again (`cancel_async`
            // awaits it) panics inside tokio with "called after complete"
            Err(oneshot::error::TryRecvError::Closed) => {
                self.result_rx = None;
                Ok(false)
            }
            Ok(e) => {
                self.result_rx = None;
                Err(e)
            }
        }
    }

    /// [EventsSubscription::cancel], but blocking until the callback either returns or panics.
    pub fn cancel_blocking(&mut self) -> thread::Result<bool> {
        RUNTIME.block_on(self.cancel_async())
    }

    /// [EventsSubscription::cancel], but waiting until the callback either returns or panics.
    pub async fn cancel_async(&mut self) -> thread::Result<bool> {
        let Some(rx) = self.result_rx.take() else {
            // already cancelled by `cancel_async`
            return Ok(false);
        };

        let res = self.abort_tx.try_send(()).is_ok();
        match rx.await {
            Err(_) => Ok(res),
            Ok(e) => Err(e),
        }
    }
}

impl Drop for EventsSubscription {
    fn drop(&mut self) {
        let Some(ref mut rx) = self.result_rx else {
            return;
        };
        // `Ok` is the panic payload the callback sent; `Err` is only "nothing was sent" (the
        // normal case), not a panic
        if let Ok(e) = rx.try_recv() {
            teprintln!(
                "Event receiver panicked, but subscription is dropped without canceling: {:?}",
                e
            );
        }
    }
}

#[cfg(test)]
mod test {
    use std::{
        sync::{Arc, Mutex},
        thread::sleep,
    };

    use super::*;
    use crate::driver_ipc::mock::*;

    #[test]
    fn event_receiver_not_canceled_after_drop() {
        const PIPE_NAME: &str = "virtualdisplaydriver-sync-event_receiver_not_canceled_after_drop";

        let mut server = RUNTIME.block_on(async { MockServer::new(PIPE_NAME) });

        let client = Client::connect_to(PIPE_NAME).unwrap();

        let call_count = Arc::new(Mutex::new(0));

        let sub = client.add_event_receiver({
            let call_count = call_count.clone();
            move |_| {
                *call_count.lock().unwrap() += 1;
            }
        });

        drop(sub);

        client.notify(&[]).unwrap();
        RUNTIME.block_on(server.pump());

        // wait for the callback itself: a fixed sleep loses the race on a loaded machine
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while *call_count.lock().unwrap() == 0 && std::time::Instant::now() < deadline {
            sleep(std::time::Duration::from_millis(2));
        }

        assert_eq!(*call_count.lock().unwrap(), 1);
    }

    #[test]
    fn catch_unwind_when_receiver_panics() {
        const PIPE_NAME: &str = "virtualdisplaydriver-sync-catch_unwind_when_receiver_panics";

        let mut server = RUNTIME.block_on(async { MockServer::new(PIPE_NAME) });

        let client = Client::connect_to(PIPE_NAME).unwrap();

        let mut sub1 = client.add_event_receiver(move |_| {
            panic!("Panic1 in callback");
        });
        let mut sub2 = client.add_event_receiver(move |_| {
            panic!("Panic2 in callback");
        });

        client.notify(&[]).unwrap();
        RUNTIME.block_on(server.pump());

        // give the callback time to run
        sleep(std::time::Duration::from_millis(100));

        sub1.cancel_blocking().expect_err("Callback should panic");
        assert!(!sub1
            .cancel_blocking()
            .expect("Error should already be handled"));

        sub2.cancel().expect_err("Callback should panic");
        assert!(!sub2.cancel().expect("Error should already be handled"));
    }

    /// The subscriber has already ended when `cancel` is asked again, so the receiver answers
    /// `Closed`; a later `cancel_async` must not poll that spent receiver.
    #[test]
    fn cancelling_after_the_subscriber_ended_does_not_poll_a_spent_receiver() {
        const PIPE_NAME: &str = "virtualdisplaydriver-sync-cancel_after_end";

        let _server = RUNTIME.block_on(async { MockServer::new(PIPE_NAME) });
        let client = Client::connect_to(PIPE_NAME).unwrap();
        let mut sub = client.add_event_receiver(|_| {});
        assert!(sub.cancel().expect("Callback should not panic"));
        // let the subscriber task see the abort and drop its sender
        sleep(std::time::Duration::from_millis(200));
        assert!(!sub.cancel().expect("Callback should not panic"));
        assert!(!RUNTIME
            .block_on(sub.cancel_async())
            .expect("Callback should not panic"));
    }

    #[test]
    fn event_receiver() {
        const PIPE_NAME: &str = "virtualdisplaydriver-sync-event_receiver";

        let mut server = RUNTIME.block_on(async { MockServer::new(PIPE_NAME) });

        let client = Client::connect_to(PIPE_NAME).unwrap();

        let events = Arc::new(Mutex::new(vec![]));

        let mut sub = client.add_event_receiver({
            let events = events.clone();
            move |event| {
                events.lock().unwrap().push(event);
            }
        });

        client.notify(&[]).unwrap();
        RUNTIME.block_on(server.pump());
        // wait for the event itself: a fixed sleep loses the race on a loaded machine
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while events.lock().unwrap().is_empty() && std::time::Instant::now() < deadline {
            sleep(std::time::Duration::from_millis(2));
        }

        assert!(sub.cancel().expect("Callback should not panic"));
        assert!(!sub.cancel().expect("Callback should not panic"));
        assert!(!sub.cancel().expect("Callback should not panic"));
        // the subscriber task has ended once this returns, so nothing after it can be delivered
        RUNTIME
            .block_on(sub.cancel_async())
            .expect("Callback should not panic");

        client.notify(&[]).unwrap();
        RUNTIME.block_on(server.pump());

        assert!(matches!(
            events.lock().unwrap().as_slice(),
            [Ok(EventCommand::Changed(mons))] if mons.is_empty()
        ))
    }

    #[test]
    fn event_receiver_cancel_from_cb() {
        const PIPE_NAME: &str = "virtualdisplaydriver-sync-event_receiver_cancel_from_cb";

        let mut server = RUNTIME.block_on(async { MockServer::new(PIPE_NAME) });

        let client = Client::connect_to(PIPE_NAME).unwrap();

        let shared_sub = Arc::new(Mutex::new(None::<EventsSubscription>));
        let shared_flag = Arc::new(Mutex::new(false));

        let sub = client.add_event_receiver({
            let shared_sub = shared_sub.clone();
            let shared_flag = shared_flag.clone();
            move |event| {
                assert!(
                    matches!(event, Ok(EventCommand::Changed(mons)) if mons.is_empty()),
                    "Wrong event received"
                );
                assert!(
                    shared_sub
                        .lock()
                        .unwrap()
                        .as_mut()
                        .unwrap()
                        .cancel()
                        .expect("Callback should not panic"),
                    "Should not be cancelled by now"
                );
                *shared_flag.lock().unwrap() = true;
            }
        });

        *shared_sub.lock().unwrap() = Some(sub);

        client.notify(&[]).unwrap();
        RUNTIME.block_on(server.pump());
        sleep(std::time::Duration::from_millis(100));

        assert!(
            !shared_sub
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .cancel()
                .expect("Callback should not panic"),
            "Should already be cancelled"
        );
        assert!(*shared_flag.lock().unwrap(), "Callback was not run");
    }
}
