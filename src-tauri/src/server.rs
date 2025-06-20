use axum::Router;
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tokio::task;

pub struct Server {
    host: String,
    port: u16,
    is_running: Arc<Mutex<bool>>,
    router: Option<Router>,
}

impl Server {
    pub fn new(host: &str, port: u16) -> Self {
        Self {
            host: host.to_string(),
            port,
            is_running: Arc::new(Mutex::new(false)),
            router: None,
        }
    }

    pub fn configure(&mut self, router: Router) {
        self.router = Some(router);
    }

    pub async fn start(&self) -> bool {
        let addr = match format!("{}:{}", self.host, self.port).parse::<SocketAddr>() {
            Ok(addr) => addr,
            Err(_) => return false,
        };

        let is_running = Arc::clone(&self.is_running);

        {
            let mut running_guard = is_running.lock().unwrap();
            if *running_guard {
                return false;
            }
            *running_guard = true;
        }

        let app = match &self.router {
            Some(router) => router.clone(),
            None => return false,
        };

        task::spawn(async move {
            let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
            axum::serve(listener, app).await.unwrap();
        });

        true
    }

    pub fn stop(&self) {
        let mut running_guard = self.is_running.lock().unwrap();
        if !*running_guard {}
        *running_guard = false;
    }
}
