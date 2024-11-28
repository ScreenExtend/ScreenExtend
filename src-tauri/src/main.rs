#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod global_utils;

use rand::Rng;
use serde::Serialize;
use specta::collect_types;
use specta::Type;
use std::process::Command as StdCommand;
use tauri::api::process::Command as TauriCommand;
use tauri::Manager;
use tauri::Window;
use tauri::WindowBuilder;
use tauri_specta::ts;

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

//#[tauri::command]
//#[specta::specta]
//fn fetch_urls(window: Window) {
//    let _ = window.emit("local_url", "https://192.168.88.1:8000/");
//    let _ = window.emit("global_url", "https://screenextend.app/session/abcdefgh");
//}

#[derive(Debug, Clone, Serialize, Type)]
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
    id: u32,
}

#[tauri::command]
#[specta::specta]
fn get_devices(window: Window) {
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
        id: rng.gen_range(1, 10)
    };
    let _ = window.emit("device_join", device);
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            match app.get_cli_matches() {
                Ok(matches) => {
                    match matches.subcommand {
                        Some(command) => {
                            if command.name == "hostednetwork" {
                                let mut ssid = String::new();
                                let mut password = String::new();
                                for (key, arg_data) in &command.matches.args {
                                    match key.as_str() {
                                        "ssid" => {
                                            ssid = arg_data.value.as_str().map(|s| s.to_string()).expect("No password");
                                        }
                                        "password" => {
                                            password = arg_data.value.as_str().map(|s| s.to_string()).expect("No password");
                                        }
                                        _ => {}
                                    }
                                }
                                let mut set_cmd = StdCommand::new("netsh");
                                set_cmd.args(&[
                                    "wlan",
                                    "set",
                                    "hostednetwork",
                                    "mode=allow",
                                    &format!("ssid={}", ssid),
                                    &format!("key={}", password),
                                ]);
                                let _ = set_cmd.output();
                                let mut start_cmd = StdCommand::new("netsh");
                                start_cmd.args(&["wlan", "start", "hostednetwork"]);
                                let _ = start_cmd.output();
                                app.app_handle().exit(0);
                            } else if command.name == "installdrivers" {
                                let cert_root_cmd = TauriCommand::new("certutil")
                                    .current_dir(app.path_resolver().resource_dir().unwrap())
                                    .args(&[
                                        "-addstore",
                                        "-f",
                                        "root",
                                        app.path_resolver()
                                            .resolve_resource("ScreenExtend.cer")
                                            .unwrap()
                                            .to_str()
                                            .unwrap(),
                                    ]);
                                let _a = cert_root_cmd.output();
                                // println!("{:?}", a.unwrap().stdout);
                                let cert_publisher_cmd = TauriCommand::new("certutil")
                                    .current_dir(app.path_resolver().resource_dir().unwrap())
                                    .args(&[
                                        "-addstore",
                                        "-f",
                                        "TrustedPublisher",
                                        app.path_resolver()
                                            .resolve_resource("ScreenExtend.cer")
                                            .unwrap()
                                            .to_str()
                                            .unwrap(),
                                    ]);
                                let _b = cert_publisher_cmd.output();
                                // println!("{:?}", b.unwrap().stdout);
                                let remove_cmd = TauriCommand::new_sidecar("nefconc")
                                    .expect("Couldn't find nefconc")
                                    .current_dir(app.path_resolver().resource_dir().unwrap())
                                    .args(&[
                                        "--remove-device-node",
                                        "--hardware-id",
                                        "Root\\VirtualDisplayDriver",
                                        "--class-guid",
                                        "4D36E968-E325-11CE-BFC1-08002BE10318",
                                    ]);
                                let _c = remove_cmd.output();
                                // println!("{:?}", c.unwrap().stdout);
                                let create_cmd = TauriCommand::new_sidecar("nefconc")
                                    .expect("Couldn't find nefconc")
                                    .current_dir(app.path_resolver().resource_dir().unwrap())
                                    .args(&[
                                        "--create-device-node",
                                        "--class-name",
                                        "Display",
                                        "--class-guid",
                                        "4D36E968-E325-11CE-BFC1-08002BE10318",
                                        "--hardware-id",
                                        "Root\\VirtualDisplayDriver",
                                    ]);
                                let _d = create_cmd.output();
                                // println!("{:?}", d.unwrap().stdout);
                                let install_cmd = TauriCommand::new_sidecar("nefconc")
                                    .expect("Couldn't find nefconc")
                                    .current_dir(app.path_resolver().resource_dir().unwrap())
                                    .args(&[
                                        "--install-driver",
                                        "--inf-path",
                                        app.path_resolver()
                                            .resolve_resource("VirtualDisplayDriver.inf")
                                            .unwrap()
                                            .to_str()
                                            .unwrap(),
                                    ]);
                                let _e = install_cmd.output();
                                // println!("{:?}", e.unwrap().stdout);
                                app.app_handle().exit(0);
                            }
                        }
                        None => {
                            ts::export(
                                collect_types![
                                    setup,
//                                    fetch_urls,
                                    get_devices,
                                    global_utils::get_private_ip_addresses,
                                    global_utils::get_private_ip_address,
                                    hosted_network::start_hosted_network,
                                    hosted_network::stop_hosted_network,
                                    virtual_display::install_drivers,
                                    virtual_display::create_display,
                                    virtual_display::update_display,
                                    virtual_display::remove_display,
                                    virtual_display::remove_all_displays
                                ],
                                "../src/lib/bindings.ts",
                            )
                            .unwrap();
                            WindowBuilder::new(
                                app,
                                "main".to_string(),
                                tauri::WindowUrl::App("index.html".into()),
                            )
                            .min_inner_size(1050.0, 650.0)
                            .inner_size(1200.0, 675.0)
                            .title("Screen Extend")
                            .resizable(true)
                            .maximized(true)
                            .build()?;
                        }
                    };
                }
                Err(_) => {}
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            setup,
//            fetch_urls,
            get_devices,
            global_utils::get_private_ip_addresses,
            global_utils::get_private_ip_address,
            hosted_network::start_hosted_network,
            hosted_network::stop_hosted_network,
            virtual_display::install_drivers,
            virtual_display::create_display,
            virtual_display::update_display,
            virtual_display::remove_display,
            virtual_display::remove_all_displays
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
