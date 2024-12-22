#[cfg_attr(mobile, tauri::mobile_entry_point)]

mod global_utils;

use rand::Rng;
use serde::{Serialize, Deserialize};
use specta_typescript::Typescript;
use tauri_specta::{collect_commands, Builder};
use specta::Type;
use tauri::Manager;
//use tauri::WindowBuilder;
use tauri_plugin_shell::ShellExt;
use tauri::path::BaseDirectory;
use tauri_plugin_cli::CliExt;
use tauri::Emitter;

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

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
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
        id: rng.gen_range(1, 10)
    };
    let _ = app.emit("device_join", device);
}

pub fn run() {
    let builder = Builder::<tauri::Wry>::new()
        .commands(collect_commands![
            setup, get_devices,
            global_utils::get_private_ip_addresses,
            global_utils::get_private_ip_address,
            hosted_network::start_hosted_network,
            hosted_network::stop_hosted_network,
            virtual_display::install_drivers,
            virtual_display::create_display,
            virtual_display::update_display,
            virtual_display::remove_display,
            virtual_display::remove_all_displays
        ]);

    #[cfg(debug_assertions)]
    builder
        .export(Typescript::default(), "../src/lib/bindings.ts")
        .expect("Failed to export typescript bindings");

    tauri::Builder::default()
        .plugin(tauri_plugin_clipboard_manager::init())
//        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_cli::init())
        .plugin(tauri_plugin_http::init())
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            let _ = app.get_webview_window("main").expect("no main window").set_focus();
        }))
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
                                    ssid = arg_data.value.as_str().map(|s| s.to_string()).expect("No password");
                                }
                                "password" => {
                                    password = arg_data.value.as_str().map(|s| s.to_string()).expect("No password");
                                }
                                _ => {}
                            }
                        }
                        tauri::async_runtime::block_on(async {
                            app.shell().command("netsh").args(&["wlan", "set", "hostednetwork", "mode=allow", &format!("ssid={}", ssid), &format!("key={}", password)]).output().await.unwrap();
                            app.shell().command("netsh").args(&["wlan", "start", "hostednetwork"]).output().await.unwrap();
                        });
                        app.handle().exit(0);
                    }
                    Some(command) if command.name == "installdrivers" => {
                        tauri::async_runtime::block_on(async {
                            let resource_path = |file: &str| app.path().resolve(file, BaseDirectory::Resource).unwrap().into_os_string().into_string().unwrap();
                            app.shell().command("certutil").args(&["-addstore", "-f", "root", &resource_path("ScreenExtend.cer")]).current_dir(app.path().resource_dir().unwrap()).output().await.unwrap();
                            app.shell().command("certutil").args(&["-addstore", "-f", "TrustedPublisher", &resource_path("ScreenExtend.cer")]).current_dir(app.path().resource_dir().unwrap()).output().await.unwrap();
                            app.shell().command("nefconc").args(&["--remove-device-node", "--hardware-id", "Root\\VirtualDisplayDriver", "--class-guid", "4D36E968-E325-11CE-BFC1-08002BE10318"]).current_dir(app.path().resource_dir().unwrap()).output().await.unwrap();
                            app.shell().command("nefconc").args(&["--create-device-node", "--class-name", "Display", "--class-guid", "4D36E968-E325-11CE-BFC1-08002BE10318", "--hardware-id", "Root\\VirtualDisplayDriver"]).current_dir(app.path().resource_dir().unwrap()).output().await.unwrap();
                            app.shell().command("nefconc").args(&["--install-driver", "--inf-path", &resource_path("VirtualDisplayDriver.inf")]).current_dir(app.path().resource_dir().unwrap()).output().await.unwrap();
                        });
                        app.handle().exit(0);
                    }
                    _ => {
                        builder.mount_events(app);
                        tauri::WebviewWindowBuilder::new(
                            app,
                            "main".to_string(),
                            tauri::WebviewUrl::App("index.html".into()),
                        )
                        .min_inner_size(1050.0, 650.0)
                        .inner_size(1200.0, 675.0)
                        .title("Screen Extend")
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
