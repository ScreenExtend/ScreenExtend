use axum::Router;
use std::{net::SocketAddr, sync::{Arc, Mutex}};
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

    pub async fn start(&self) -> Result<(), bool> {
        let addr = match format!("{}:{}", self.host, self.port).parse::<SocketAddr>() {
            Ok(addr) => addr,
            Err(_) => return Err(false),
        };

        let is_running = Arc::clone(&self.is_running);

        {
            let mut running_guard = is_running.lock().unwrap();
            if *running_guard {
                return Err(false);
            }
            *running_guard = true;
        }

        let app = match &self.router {
            Some(router) => router.clone(),
            None => return Err(false),
        };

        task::spawn(async move {
            let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
            axum::serve(listener, app).await.unwrap();
        });

        Ok(())
    }

    pub fn stop(&self) -> Result<(), String> {
        let mut running_guard = self.is_running.lock().unwrap();
        if !*running_guard {
            return Err("Server is not running".to_string());
        }

        *running_guard = false;
        println!("Server stopped.");
        Ok(())
    }
}