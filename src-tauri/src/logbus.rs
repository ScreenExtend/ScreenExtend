use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::AppHandle;
use tauri_specta::Event;

const MAX_BACKLOG: usize = 2000;

static VERBOSE: AtomicBool = AtomicBool::new(false);

pub fn set_verbose(on: bool) {
    VERBOSE.store(on, Ordering::Relaxed);
}

#[inline]
pub fn verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

#[derive(Serialize, Deserialize, Debug, Clone, Type, Event)]
pub struct LogLine(pub String);

struct LogBus {
    backlog: Mutex<VecDeque<String>>,
    app: OnceLock<AppHandle>,
}

static BUS: OnceLock<LogBus> = OnceLock::new();

fn bus() -> &'static LogBus {
    BUS.get_or_init(|| LogBus {
        backlog: Mutex::new(VecDeque::with_capacity(MAX_BACKLOG)),
        app: OnceLock::new(),
    })
}

pub fn push_line(line: String) {
    let bus = bus();
    {
        let mut backlog = bus.backlog.lock().unwrap();
        if backlog.len() >= MAX_BACKLOG {
            backlog.pop_front();
        }
        backlog.push_back(line.clone());
    }
    if let Some(app) = bus.app.get() {
        let _ = LogLine(line).emit(app);
    }
}

pub fn attach(app: AppHandle) {
    let _ = bus().app.set(app);
}

#[tauri::command]
#[specta::specta]
pub fn get_log_backlog() -> Vec<String> {
    bus().backlog.lock().unwrap().iter().cloned().collect()
}

#[macro_export]
macro_rules! tprintln {
    () => {{
        if $crate::logbus::verbose() {
            println!();
        }
        $crate::logbus::push_line(String::new());
    }};
    ($($arg:tt)*) => {{
        let __line = format!($($arg)*);
        if $crate::logbus::verbose() {
            println!("{}", __line);
        }
        $crate::logbus::push_line(__line);
    }};
}

#[macro_export]
macro_rules! teprintln {
    () => {{
        if $crate::logbus::verbose() {
            eprintln!();
        }
        $crate::logbus::push_line(String::new());
    }};
    ($($arg:tt)*) => {{
        let __line = format!($($arg)*);
        if $crate::logbus::verbose() {
            eprintln!("{}", __line);
        }
        $crate::logbus::push_line(__line);
    }};
}

const MAX_FRONTEND_ERROR_CHARS: usize = 2000;

/// The webview's `console.error` has nowhere to go in a release build -- there are no
/// devtools attached and nothing captures it -- so failures in the desktop UI simply
/// vanished. This puts them on the same bus as everything else, which means they reach
/// the Settings log terminal, `get_log_backlog` and `ScreenExtend logs`.
///
/// Defined below the macros because a `macro_rules!` is only in scope after its definition
/// within its own file.
#[tauri::command]
#[specta::specta]
pub fn log_frontend_error(message: String) {
    let mut text = message.replace(['\r', '\n'], " ");
    if text.chars().count() > MAX_FRONTEND_ERROR_CHARS {
        text = text.chars().take(MAX_FRONTEND_ERROR_CHARS).collect();
        text.push('…');
    }
    teprintln!("[ui] {}", text);
}
