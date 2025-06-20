#[cfg_attr(mobile, tauri::mobile_entry_point)]

use rand::Rng;
use serde::{Deserialize, Serialize};
use specta::Type;
use specta_typescript::Typescript;
use tauri::path::BaseDirectory;
use tauri::Emitter;
use tauri::Manager;
use tauri_plugin_cli::CliExt;
use tauri_plugin_shell::ShellExt;
use tauri_specta::{collect_commands, collect_events, Builder, Event};
mod server;

#[cfg(target_os = "windows")]
mod windows_utils;
#[cfg(target_os = "windows")]
use windows_utils::*;

#[cfg(target_os = "macos")]
mod macos_utils;
#[cfg(target_os = "macos")]
use macos_utils::*;

#[cfg(target_os = "linux")]
mod linux_utils;
#[cfg(target_os = "linux")]
use linux_utils::*;

#[derive(Serialize, Deserialize, Debug, Clone, Type, Event)]
pub struct DeviceJoin(Device);

#[derive(Serialize, Deserialize, Debug, Clone, Type, Event)]
pub struct DeviceModify(Device);

#[derive(Serialize, Deserialize, Debug, Clone, Type, Event)]
pub struct DeviceModifyAction(Device);

#[derive(Serialize, Deserialize, Debug, Clone, Type, Event)]
pub struct DeviceRemove(Device);

#[derive(Serialize, Deserialize, Debug, Clone, Type, Event)]
pub struct DeviceRemoveAction(Device);

#[derive(Serialize, Deserialize, Debug, Clone, Type, Event)]
pub struct NetworkChange;

#[derive(Serialize, Deserialize, Debug, Clone, Type, Event)]
struct Device {
    ip: String,
    name: String,
    scale: u32,
    orientation: String,
    #[serde(rename = "refreshRate")]
    refresh_rate: u32,
    os: String,
    #[serde(rename = "screenSize")]
    screen_size: String,
}

#[tauri::command]
#[specta::specta]
fn get_devices(app: tauri::AppHandle) {
    let mut rng = rand::thread_rng();
    let device = Device {
        ip: format!(
            "192.168.{}.{}",
            rng.gen_range(1, 256),
            rng.gen_range(1, 256)
        ),
        name: format!("Device {}", rng.gen_range(1, 10)),
        scale: rng.gen_range(1, 9) * 25,
        orientation: if rng.gen_range(0, 2) == 1 {
            "Portrait".to_string()
        } else {
            "Landscape".to_string()
        },
        refresh_rate: rng.gen_range(15, 500),
        os: ["Windows", "MacOS", "Linux", "Android", "iOS", "iPadOS"][rng.gen_range(0, 6)]
            .to_string(),
        screen_size: format!("{}x{}", rng.gen_range(500, 2501), rng.gen_range(1, 2501)),
    };
    let _ = app.emit("device_join", device);
}

pub fn run() {
    let builder = Builder::<tauri::Wry>::new()
        .commands(collect_commands![
            setup,
            get_devices,
            set_current_user,
            networking::get_network_adapters,
            hosted_network::start_hosted_network,
            hosted_network::stop_hosted_network,
            hosted_network::is_hosted_network,
            virtual_display::install_drivers,
            virtual_display::create_display,
            virtual_display::update_display,
            virtual_display::remove_display,
            virtual_display::remove_all_displays
        ])
        .events(collect_events![
            DeviceJoin,
            DeviceModify,
            DeviceModifyAction,
            DeviceRemove,
            DeviceRemoveAction,
            NetworkChange
        ]);

//    #[cfg(debug_assertions)]
    builder
        .export(Typescript::default(), "../src/lib/bindings.ts")
        .expect("error while exporting typescript bindings");

    tauri::Builder::default()
        .plugin(tauri_plugin_os::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_cli::init())
        .plugin(tauri_plugin_http::init())
        .plugin(tauri_plugin_store::Builder::default().build())
        .invoke_handler(builder.invoke_handler())
        .setup(move |app| {
            if let Ok(matches) = app.cli().matches() {
                match matches.subcommand {
                    Some(command) if command.name == "hostednetwork" => {
                        let mut ssid = String::new();
                        let mut password = String::new();
                        for (key, arg_data) in &command.matches.args {
                            match key.as_str() {
                                "ssid" => {
                                    ssid = arg_data
                                        .value
                                        .as_str()
                                        .map(|s| s.to_string())
                                        .expect("no ssid");
                                }
                                "password" => {
                                    password = arg_data
                                        .value
                                        .as_str()
                                        .map(|s| s.to_string())
                                        .expect("no password");
                                }
                                _ => {}
                            }
                        }
                        tauri::async_runtime::block_on(async {
                            app.shell()
                                .command("netsh")
                                .args(&[
                                    "wlan",
                                    "set",
                                    "hostednetwork",
                                    "mode=allow",
                                    &format!("ssid={}", ssid),
                                    &format!("key={}", password),
                                ])
                                .output()
                                .await
                                .unwrap();
                            app.shell()
                                .command("netsh")
                                .args(&["wlan", "start", "hostednetwork"])
                                .output()
                                .await
                                .unwrap();
                        });
                        app.handle().exit(0);
                    }
                    Some(command) if command.name == "installdrivers" => {
                        tauri::async_runtime::block_on(async {
                            let resource_path = |file: &str| {
                                app.path()
                                    .resolve(file, BaseDirectory::Resource)
                                    .unwrap()
                                    .into_os_string()
                                    .into_string()
                                    .unwrap()
                            };
                            app.shell()
                                .command("certutil")
                                .args(&[
                                    "-addstore",
                                    "-f",
                                    "root",
                                    &resource_path("resources/ScreenExtend.cer"),
                                ])
                                .current_dir(app.path().resource_dir().unwrap())
                                .output()
                                .await
                                .unwrap();
                            app.shell()
                                .command("certutil")
                                .args(&[
                                    "-addstore",
                                    "-f",
                                    "TrustedPublisher",
                                    &resource_path("resources/ScreenExtend.cer"),
                                ])
                                .current_dir(app.path().resource_dir().unwrap())
                                .output()
                                .await
                                .unwrap();
                            app.shell()
                                .command("nefconc")
                                .args(&[
                                    "--remove-device-node",
                                    "--hardware-id",
                                    "Root\\VirtualDisplayDriver",
                                    "--class-guid",
                                    "4D36E968-E325-11CE-BFC1-08002BE10318",
                                ])
                                .current_dir(app.path().resource_dir().unwrap())
                                .output()
                                .await
                                .unwrap();
                            app.shell()
                                .command("nefconc")
                                .args(&[
                                    "--create-device-node",
                                    "--class-name",
                                    "Display",
                                    "--class-guid",
                                    "4D36E968-E325-11CE-BFC1-08002BE10318",
                                    "--hardware-id",
                                    "Root\\VirtualDisplayDriver",
                                ])
                                .current_dir(app.path().resource_dir().unwrap())
                                .output()
                                .await
                                .unwrap();
                            app.shell()
                                .command("nefconc")
                                .args(&[
                                    "--install-driver",
                                    "--inf-path",
                                    &resource_path("resources/VirtualDisplayDriver.inf"),
                                ])
                                .current_dir(app.path().resource_dir().unwrap())
                                .output()
                                .await
                                .unwrap();
                        });
                        app.handle().exit(0);
                    }
                    _ => {
                        match tokio::task::block_in_place(|| std::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 5363)))) {
                            Ok(_) => {
                                tauri::async_runtime::spawn(async move {
                                    let app = axum::Router::new().route("/", axum::routing::get(|| async { "ScreenExtend Echo Server" }));
                                    axum_server::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 5363))).serve(app.into_make_service()).await.unwrap();
                                });
                            }
                            Err(_) => {
                                std::process::exit(0);
                            }
                        }
                        // first time - iter over networks and ip addresses, host server on ip/port, if successful then break, otherwise continue, if all ips are out, show blurred qr code and error (underlying qr code is blank)
                        // store list of interface indexes stored and remove/add based on network changes (use wim api for it, not web browser but see)
                        builder.mount_events(app);
                        tauri::WebviewWindowBuilder::new(
                            app,
                            "main".to_string(),
                            tauri::WebviewUrl::App("index.html".into()),
                        )
                        .min_inner_size(1050.0, 650.0)
                        .inner_size(1200.0, 675.0)
                        .title("ScreenExtend")
                        .resizable(true)
                        .maximized(true)
                        .build()?;
                    }
                }
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
