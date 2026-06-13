use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::AppHandle;
use tauri_specta::Event;

const MAX_BACKLOG: usize = 2000;

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
        println!();
        $crate::logbus::push_line(String::new());
    }};
    ($($arg:tt)*) => {{
        let __line = format!($($arg)*);
        println!("{}", __line);
        $crate::logbus::push_line(__line);
    }};
}

#[macro_export]
macro_rules! teprintln {
    () => {{
        eprintln!();
        $crate::logbus::push_line(String::new());
    }};
    ($($arg:tt)*) => {{
        let __line = format!($($arg)*);
        eprintln!("{}", __line);
        $crate::logbus::push_line(__line);
    }};
}
