use std::{io, sync::Arc};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::windows::named_pipe,
    sync::{broadcast, watch, Notify},
    task,
};

use crate::driver_ipc::*;

use self::client::EOF;

pub struct MockServer {
    server: Arc<named_pipe::NamedPipeServer>,
    state: Vec<Monitor>,
    command_rx: broadcast::Receiver<ServerCommand>,
    command_tx: broadcast::Sender<ServerCommand>,
    notify_closed: Arc<Notify>,
    disconnected: watch::Receiver<bool>,
}

impl MockServer {
    pub fn new(name: &str) -> Self {
        let server = named_pipe::ServerOptions::new()
            .access_inbound(true)
            .access_outbound(true)
            .reject_remote_clients(true)
            .create(format!(r"\\.\pipe\{}", name))
            .unwrap();
        let server = Arc::new(server);

        let notify_closed = Arc::new(Notify::new());

        let (command_tx, command_rx) = broadcast::channel(64);
        let (disconnected_tx, disconnected) = watch::channel(false);

        {
            let server = server.clone();
            let command_tx = command_tx.clone();
            let notify_closed = notify_closed.clone();
            task::spawn(async move {
                let server = unsafe {
                    (server.as_ref() as *const _ as *mut named_pipe::NamedPipeServer)
                        .as_mut()
                        .unwrap()
                };

                server.connect().await.expect("Failed to connect to server");

                loop {
                    let mut buf = vec![];
                    loop {
                        let byte = tokio::select! {
                            _ = notify_closed.notified() => return,
                            r = server.read_u8() => r,
                        };

                        let v = match byte {
                            Ok(v) => v,
                            // client disconnected
                            Err(e)
                                if matches!(
                                    e.kind(),
                                    io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe
                                ) =>
                            {
                                let _ = disconnected_tx.send(true);
                                return;
                            }
                            Err(_) => continue,
                        };
                        if v == EOF {
                            break;
                        }
                        buf.push(v);
                    }

                    let cmd = serde_json::from_slice::<ServerCommand>(&buf)
                        .expect("Failed to deserialize request");

                    command_tx.send(cmd).expect("Failed to send command");
                }
            });
        }

        Self {
            server,
            state: vec![],
            command_rx,
            command_tx,
            notify_closed,
            disconnected,
        }
    }

    /// Becomes `true` when the connected client closes its end of the pipe.
    pub fn disconnected(&self) -> watch::Receiver<bool> {
        self.disconnected.clone()
    }

    pub fn state(&self) -> &[Monitor] {
        &self.state
    }

    pub fn check_next(&mut self, cb: impl FnOnce(ServerCommand) + Send + 'static) {
        let mut rx = self.command_tx.subscribe();

        task::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(cmd) => {
                        cb(cmd);
                        break;
                    }
                    Err(_) => continue,
                }
            }
        });
    }

    pub async fn pump(&mut self) {
        let cmd = self.command_rx.recv().await.unwrap();

        let server = unsafe {
            (self.server.as_ref() as *const _ as *mut named_pipe::NamedPipeServer)
                .as_mut()
                .unwrap()
        };

        let changed = match cmd {
            ServerCommand::Request(RequestCommand::State) => {
                let reply = ReplyCommand::State(self.state.clone());
                let mut reply = serde_json::to_vec(&reply).unwrap();
                reply.push(EOF);

                server
                    .write_all(&reply)
                    .await
                    .expect("Failed to write reply");
                false
            }
            ServerCommand::Driver(DriverCommand::Notify(monitors)) => {
                self.state = monitors;
                true
            }
            ServerCommand::Driver(DriverCommand::Remove(ids)) => {
                self.state.retain(|m| !ids.contains(&m.id));
                true
            }
            ServerCommand::Driver(DriverCommand::RemoveAll) => {
                self.state.clear();
                true
            }
        };

        if changed {
            let event = EventCommand::Changed(self.state.clone());
            let mut event = serde_json::to_vec(&event).unwrap();
            event.push(EOF);

            server
                .write_all(&event)
                .await
                .expect("Failed to write event");
        }
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.notify_closed.notify_waiters();
    }
}

/// How a [`V2Mock`] answers `Hello`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelloMode {
    Answer,
    /// Never answers: the client must give up and fall back.
    Silent,
    /// Writes half of the reply, then closes the connection.
    CloseMidReply,
}

#[derive(Debug, Clone)]
pub struct V2Behaviour {
    /// Serve `<name>.v2` beside `<name>` from the start. Off is the shipped driver, until
    /// [`V2Mock::serve_v2_now`].
    pub serve_v2: bool,
    pub capabilities: Vec<&'static str>,
    pub hello: HelloMode,
    /// Delay, in ms, before answering the n-th `Hello` this driver receives (0-based); missing
    /// entries answer at once.
    pub hello_delays: Vec<u64>,
    /// The error code every `SetRenderAdapter` is answered with, if any.
    pub render_error: Option<&'static str>,
    /// Write a `Changed` event before every v2 reply, as another client's change would.
    pub changed_before_reply: bool,
    /// Remove an owner's monitors this long after its last connection closes, as the driver
    /// does. `None` never removes anything.
    pub expire_after_ms: Option<u64>,
    /// A monitor has no swap chain (diagnostics report its adapter as `null`) for this long after
    /// it is created.
    pub swapchain_after_ms: u64,
    /// The render adapter preference the driver starts with.
    pub preferred: Option<v2::Luid>,
    /// Never answer `SetRenderAdapter`: a driver that is alive and silent.
    pub render_silent: bool,
}

impl Default for V2Behaviour {
    fn default() -> Self {
        Self {
            serve_v2: true,
            capabilities: vec![
                v2::CAP_HELLO,
                v2::CAP_DIAGNOSTICS,
                v2::CAP_RENDER_ADAPTER,
                v2::CAP_OWNED_MONITORS,
            ],
            hello: HelloMode::Answer,
            hello_delays: Vec::new(),
            render_error: None,
            changed_before_reply: false,
            expire_after_ms: None,
            swapchain_after_ms: 0,
            preferred: None,
            render_silent: false,
        }
    }
}

/// One message a [`V2Mock`] received.
#[derive(Debug, Clone)]
pub struct Received {
    pub connection: usize,
    pub on_v2_pipe: bool,
    pub text: String,
}

impl Received {
    /// The single key of a JSON object message, or the string of a bare one (`"State"`).
    pub fn name(&self) -> String {
        match serde_json::from_str::<serde_json::Value>(&self.text) {
            Ok(serde_json::Value::Object(o)) => o.keys().next().cloned().unwrap_or_default(),
            Ok(serde_json::Value::String(s)) => s,
            _ => String::new(),
        }
    }

    pub fn is_v2_request(&self) -> bool {
        matches!(
            self.name().as_str(),
            "Hello" | "Diagnostics" | "SetRenderAdapter"
        )
    }
}

#[derive(Default)]
struct V2State {
    monitors: Vec<Monitor>,
    created_at: std::collections::HashMap<u32, std::time::Instant>,
    preferred: Option<v2::Luid>,
    received: Vec<Received>,
    next_connection: usize,
    hellos: usize,
    /// id -> owner token.
    owner_of: std::collections::HashMap<u32, String>,
    /// token -> (live connections, generation).
    tokens: std::collections::HashMap<String, (u32, u64)>,
    /// Ids removed because their owner did not come back.
    expired: Vec<u32>,
}

impl V2State {
    /// The driver's own rule on a Notify: ids that are new belong to the sender's token, ids
    /// that left lose their owner.
    fn notified(&mut self, token: Option<&str>, after: Vec<Monitor>) {
        let before: Vec<u32> = self.monitors.iter().map(|m| m.id).collect();
        for m in &after {
            if !before.contains(&m.id) {
                self.created_at.insert(m.id, std::time::Instant::now());
                if let Some(t) = token {
                    self.owner_of.insert(m.id, t.to_string());
                }
            }
        }
        self.monitors = after;
        let present: Vec<u32> = self.monitors.iter().map(|m| m.id).collect();
        self.owner_of.retain(|id, _| present.contains(id));
    }

    /// An owner that did not come back: its ids go, highest first, while the rest stays one run
    /// from 0 or 1 (the driver's `compact`); the others are held, ownerless.
    fn expire(&mut self, token: &str) {
        let mut ids: Vec<u32> = self.monitors.iter().map(|m| m.id).collect();
        let mut theirs: Vec<u32> = self
            .owner_of
            .iter()
            .filter(|(_, t)| t.as_str() == token)
            .map(|(id, _)| *id)
            .collect();
        self.owner_of.retain(|_, t| t.as_str() != token);
        let removed = crate::windows_utils::virtual_display::compact(&mut ids, &mut theirs);
        self.monitors.retain(|m| !removed.contains(&m.id));
        self.expired.extend(removed);
    }
}

/// A driver that speaks the versioned protocol, served on its own runtime tasks: it accepts any
/// number of connections, in sequence or at once, on `<name>` and (optionally) `<name>.v2`,
/// records every message, and answers like the real one, ownership included. Like the real one
/// it sends no `Changed` to the client that made a change.
pub struct V2Mock {
    name: String,
    state: Arc<std::sync::Mutex<V2State>>,
    behaviour: Arc<std::sync::Mutex<V2Behaviour>>,
    /// Bumped to close every open connection.
    kill: watch::Sender<u64>,
    tasks: Vec<task::JoinHandle<()>>,
    serving_v2: bool,
}

impl V2Mock {
    /// Must run inside a tokio runtime.
    pub fn new(name: &str, behaviour: V2Behaviour) -> Self {
        let state = Arc::new(std::sync::Mutex::new(V2State {
            preferred: behaviour.preferred,
            ..V2State::default()
        }));
        let (kill, _) = watch::channel(0u64);
        let serve_v2 = behaviour.serve_v2;
        let behaviour = Arc::new(std::sync::Mutex::new(behaviour));
        let mut mock = Self {
            name: name.to_string(),
            state,
            behaviour,
            kill,
            tasks: Vec::new(),
            serving_v2: false,
        };
        mock.tasks
            .push(mock.accept(format!(r"\\.\pipe\{name}"), false));
        if serve_v2 {
            mock.serve_v2_now();
        }
        mock
    }

    /// Start serving the versioned pipe, as a driver whose v2 task comes up late does.
    pub fn serve_v2_now(&mut self) {
        if self.serving_v2 {
            return;
        }
        self.serving_v2 = true;
        let path = format!(r"\\.\pipe\{}", versioned_pipe_name(&self.name));
        let task = self.accept(path, true);
        self.tasks.push(task);
    }

    pub fn received(&self) -> Vec<Received> {
        self.state.lock().unwrap().received.clone()
    }

    pub fn monitors(&self) -> Vec<Monitor> {
        self.state.lock().unwrap().monitors.clone()
    }

    pub fn preferred(&self) -> Option<v2::Luid> {
        self.state.lock().unwrap().preferred
    }

    /// Ids removed because their owner's connections were gone for longer than the grace.
    pub fn expired(&self) -> Vec<u32> {
        self.state.lock().unwrap().expired.clone()
    }

    /// Put a monitor in the list as another client would, owned by `token` with `connections`
    /// live connections (0: its owner is gone and its removal pending).
    pub fn seed(&self, monitor: Monitor, owner: Option<(&str, u32)>) {
        let mut s = self.state.lock().unwrap();
        if let Some((t, c)) = owner {
            s.owner_of.insert(monitor.id, t.to_string());
            s.tokens.insert(t.to_string(), (c, 0));
        }
        s.created_at.insert(monitor.id, std::time::Instant::now());
        s.monitors.push(monitor);
    }

    pub fn set_behaviour(&self, f: impl FnOnce(&mut V2Behaviour)) {
        f(&mut self.behaviour.lock().unwrap());
    }

    /// Close every open connection; the driver itself stays, monitors and preference included.
    pub fn drop_connections(&self) {
        self.kill.send_modify(|g| *g += 1);
    }

    /// The driver's host process restarted: connections closed, monitors, owners and preference
    /// gone.
    pub fn restart(&self) {
        {
            let mut s = self.state.lock().unwrap();
            s.monitors.clear();
            s.owner_of.clear();
            s.tokens.clear();
            s.preferred = None;
        }
        self.drop_connections();
    }

    fn accept(&self, path: String, v2: bool) -> task::JoinHandle<()> {
        let state = self.state.clone();
        let behaviour = self.behaviour.clone();
        let kill = self.kill.clone();
        // the first instance exists before this returns, so a client can connect at once
        let mut server = named_pipe::ServerOptions::new()
            .first_pipe_instance(true)
            .access_inbound(true)
            .access_outbound(true)
            .reject_remote_clients(true)
            .create(&path)
            .expect("create the mock pipe");
        task::spawn(async move {
            loop {
                if server.connect().await.is_err() {
                    return;
                }
                let next = named_pipe::ServerOptions::new()
                    .access_inbound(true)
                    .access_outbound(true)
                    .reject_remote_clients(true)
                    .create(&path)
                    .expect("create the next mock pipe instance");
                let connected = std::mem::replace(&mut server, next);
                let id = {
                    let mut s = state.lock().unwrap();
                    s.next_connection += 1;
                    s.next_connection
                };
                task::spawn(Self::serve(
                    connected,
                    id,
                    v2,
                    state.clone(),
                    behaviour.clone(),
                    kill.subscribe(),
                ));
            }
        })
    }

    async fn serve(
        mut pipe: named_pipe::NamedPipeServer,
        id: usize,
        v2: bool,
        state: Arc<std::sync::Mutex<V2State>>,
        behaviour: Arc<std::sync::Mutex<V2Behaviour>>,
        mut kill: watch::Receiver<u64>,
    ) {
        kill.borrow_and_update();
        let mut token: Option<String> = None;
        let mut hello_seen = false;
        let mut pending: Vec<u8> = Vec::new();
        let mut buf = vec![0u8; 4096];
        'conn: loop {
            let n = tokio::select! {
                _ = kill.changed() => break 'conn,
                r = pipe.read(&mut buf) => match r {
                    Ok(0) | Err(_) => break 'conn,
                    Ok(n) => n,
                },
            };
            pending.extend_from_slice(&buf[..n]);
            while let Some(end) = pending.iter().position(|b| *b == EOF) {
                let text = String::from_utf8_lossy(&pending[..end]).into_owned();
                pending.drain(..=end);
                let message = Received {
                    connection: id,
                    on_v2_pipe: v2,
                    text,
                };
                state.lock().unwrap().received.push(message.clone());
                let b = behaviour.lock().unwrap().clone();
                let (replies, delay) = if v2 && message.is_v2_request() {
                    match Self::answer_v2(&message, &b, &state, &mut token, &mut hello_seen) {
                        Some(r) => r,
                        None => continue,
                    }
                } else {
                    (Self::answer_v1(&message.text, &state, token.as_deref()), 0)
                };
                if delay > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
                for reply in replies {
                    if pipe.write_all(&reply).await.is_err() {
                        break 'conn;
                    }
                    if reply.last() != Some(&EOF) {
                        // half a reply, and then the driver goes away
                        break 'conn;
                    }
                }
            }
        }
        drop(pipe);
        // once the owner's last connection is gone, remove its monitors after the grace, unless
        // a connection with the same token comes back first
        let Some(t) = token else { return };
        let generation = {
            let mut s = state.lock().unwrap();
            let entry = s.tokens.entry(t.clone()).or_insert((0, 0));
            entry.0 = entry.0.saturating_sub(1);
            if entry.0 > 0 {
                return;
            }
            entry.1 += 1;
            entry.1
        };
        let Some(after) = behaviour.lock().unwrap().expire_after_ms else {
            return;
        };
        task::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(after)).await;
            let mut s = state.lock().unwrap();
            if s.tokens.get(&t) == Some(&(0, generation)) {
                s.expire(&t);
            }
        });
    }

    fn frame(value: &serde_json::Value) -> Vec<u8> {
        let mut v = serde_json::to_vec(value).unwrap();
        v.push(EOF);
        v
    }

    fn answer_v1(
        text: &str,
        state: &std::sync::Mutex<V2State>,
        token: Option<&str>,
    ) -> Vec<Vec<u8>> {
        let Ok(command) = serde_json::from_str::<ServerCommand>(text) else {
            return Vec::new();
        };
        let mut s = state.lock().unwrap();
        match command {
            ServerCommand::Request(RequestCommand::State) => {
                let mut reply =
                    serde_json::to_vec(&ReplyCommand::State(s.monitors.clone())).unwrap();
                reply.push(EOF);
                vec![reply]
            }
            ServerCommand::Driver(DriverCommand::Notify(m)) => {
                s.notified(token, m);
                Vec::new()
            }
            ServerCommand::Driver(DriverCommand::Remove(ids)) => {
                let after: Vec<Monitor> = s
                    .monitors
                    .iter()
                    .filter(|m| !ids.contains(&m.id))
                    .cloned()
                    .collect();
                s.notified(None, after);
                Vec::new()
            }
            ServerCommand::Driver(DriverCommand::RemoveAll) => {
                s.monitors.clear();
                s.owner_of.clear();
                Vec::new()
            }
        }
    }

    /// `None`: say nothing at all. Otherwise the frames to write and a delay before them.
    fn answer_v2(
        message: &Received,
        b: &V2Behaviour,
        state: &std::sync::Mutex<V2State>,
        token: &mut Option<String>,
        hello_seen: &mut bool,
    ) -> Option<(Vec<Vec<u8>>, u64)> {
        use serde_json::json;
        let request: serde_json::Value = serde_json::from_str(&message.text).ok()?;
        let mut s = state.lock().unwrap();
        let mut out = Vec::new();
        let mut delay = 0;
        if b.changed_before_reply {
            let event = serde_json::to_vec(&EventCommand::Changed(s.monitors.clone())).unwrap();
            out.push([event, vec![EOF]].concat());
        }
        let error = |request: &str, code: &str| json!({ "Error": { "request": request, "code": code, "message": "refused by the mock" } });
        let reply = match message.name().as_str() {
            "Hello" => {
                delay = b.hello_delays.get(s.hellos).copied().unwrap_or(0);
                s.hellos += 1;
                if *hello_seen {
                    error("Hello", "hello_twice")
                } else {
                    *hello_seen = true;
                    let owner = request["Hello"]["owner"].as_object().cloned();
                    let valid = owner.as_ref().is_none_or(|o| {
                        o["token"].as_str().is_some_and(|t| {
                            !t.is_empty()
                                && t.len() <= 64
                                && t.bytes()
                                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
                        })
                    });
                    if !valid {
                        error("Hello", "bad_request")
                    } else {
                        let owner = owner.map(|o| {
                            let t = o["token"].as_str().unwrap_or("").to_string();
                            let entry = s.tokens.entry(t.clone()).or_insert((0, 0));
                            entry.0 += 1;
                            entry.1 += 1;
                            let mut monitors: Vec<u32> = s
                                .owner_of
                                .iter()
                                .filter(|(_, owner)| **owner == t)
                                .map(|(id, _)| *id)
                                .collect();
                            monitors.sort_unstable();
                            *token = Some(t.clone());
                            json!({
                                "token_prefix": &t[..t.len().min(6)],
                                "grace_ms": o.get("grace_ms").cloned().unwrap_or(json!(5000)),
                                "monitors": monitors,
                            })
                        });
                        let reply = json!({ "Hello": {
                            "protocol": 2,
                            "driver": "1.0.4",
                            "build": "mock",
                            "iddcx_version": 68608,
                            "capabilities": b.capabilities,
                            "connection": message.connection,
                            "owner": owner,
                        }});
                        match b.hello {
                            HelloMode::Answer => reply,
                            HelloMode::Silent => return None,
                            HelloMode::CloseMidReply => {
                                let full = serde_json::to_vec(&reply).unwrap();
                                out.push(full[..full.len() / 2].to_vec());
                                return Some((out, delay));
                            }
                        }
                    }
                }
            }
            "Diagnostics" => {
                let swapchain_after = std::time::Duration::from_millis(b.swapchain_after_ms);
                let objects: Vec<serde_json::Value> = s
                    .monitors
                    .iter()
                    .map(|m| {
                        let has_swapchain = s
                            .created_at
                            .get(&m.id)
                            .is_none_or(|at| at.elapsed() >= swapchain_after);
                        json!({
                            "id": m.id,
                            "departed": false,
                            "active": true,
                            "render_adapter": has_swapchain.then(|| {
                                s.preferred.unwrap_or(v2::Luid { low: 74978, high: 0 })
                            }),
                            "mode": null,
                            "swapchains": u32::from(has_swapchain),
                        })
                    })
                    .collect();
                let owned: Vec<serde_json::Value> = s
                    .monitors
                    .iter()
                    .map(|m| {
                        json!({
                            "id": m.id,
                            "owner_prefix": s.owner_of.get(&m.id).map(|t| t[..t.len().min(6)].to_string()),
                        })
                    })
                    .collect();
                let tokens: Vec<serde_json::Value> = s
                    .tokens
                    .iter()
                    .map(|(t, (c, _))| {
                        json!({ "token_prefix": &t[..t.len().min(6)], "connections": c, "removal_pending": *c == 0 })
                    })
                    .collect();
                json!({ "Diagnostics": {
                    "render_adapter_preference": { "preferred": s.preferred, "description": null },
                    "monitor_objects": objects,
                    "monitors_listed": s.monitors.iter().map(|m| m.id).collect::<Vec<_>>(),
                    "owners": { "monitors": owned, "tokens": tokens },
                }})
            }
            "SetRenderAdapter" if b.render_silent => return None,
            "SetRenderAdapter" => match b.render_error {
                Some(code) => error("SetRenderAdapter", code),
                None => {
                    let luid: Option<v2::Luid> =
                        serde_json::from_value(request["SetRenderAdapter"]["luid"].clone()).ok()?;
                    s.preferred = luid;
                    json!({ "RenderAdapter": {
                        "preferred": luid,
                        "description": luid.map(|_| "Mock GPU"),
                    }})
                }
            },
            _ => return None,
        };
        out.push(Self::frame(&reply));
        Some((out, delay))
    }
}

impl Drop for V2Mock {
    fn drop(&mut self) {
        self.drop_connections();
        for t in &self.tasks {
            t.abort();
        }
    }
}
