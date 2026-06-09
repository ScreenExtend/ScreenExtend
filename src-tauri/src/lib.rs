//use rand::Rng;
use serde::{Deserialize, Serialize};
use specta::Type;
use specta_typescript::Typescript;
use tauri::path::BaseDirectory;
//use tauri::Emitter;
use fs4::fs_std::FileExt;
use std::fs::OpenOptions;
use tauri::Manager;
use tauri_plugin_cli::CliExt;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
use tauri_plugin_shell::ShellExt;
use tauri_specta::{collect_commands, collect_events, Builder, Event};
mod streamer;

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
pub struct Device {
    pub ip: String,
    pub name: String,
    pub scale: u32,
    pub orientation: String,
    #[serde(rename = "refreshRate")]
    pub refresh_rate: u32,
    #[serde(rename = "videoScale")]
    pub video_scale: u32,
    #[serde(rename = "videoQuality")]
    pub video_quality: u32,
    pub os: String,
    #[serde(rename = "screenSize")]
    pub screen_size: String,
}

impl Device {
    pub fn defaults(info: crate::streamer::session::DeviceInfo) -> Self {
        let refresh_rate = if info.refresh_rate == 0 {
            60
        } else {
            info.refresh_rate.clamp(
                crate::streamer::server::MIN_REFRESH_RATE,
                crate::streamer::server::MAX_REFRESH_RATE,
            )
        };
        Self {
            ip: info.ip,
            name: info.name,
            scale: 100,
            orientation: "Landscape".to_string(),
            refresh_rate,
            video_scale: 100,
            video_quality: 23,
            os: info.os,
            screen_size: info.screen_size,
        }
    }
}

//#[tauri::command]
//#[specta::specta]
//fn get_devices(app: tauri::AppHandle) {
//    let mut rng = rand::thread_rng();
//    let device = Device {
//        ip: format!(
//            "192.168.{}.{}",
//            rng.gen_range(1, 256),
//            rng.gen_range(1, 256)
//        ),
//        name: format!("Device {}", rng.gen_range(1, 10)),
//        scale: rng.gen_range(1, 9) * 25,
//        orientation: if rng.gen_range(0, 2) == 1 {
//            "Portrait".to_string()
//        } else {
//            "Landscape".to_string()
//        },
//        refresh_rate: rng.gen_range(15, 500),
//        os: ["Windows", "MacOS", "Linux", "Android", "iOS", "iPadOS"][rng.gen_range(0, 6)]
//            .to_string(),
//        screen_size: format!("{}x{}", rng.gen_range(500, 2501), rng.gen_range(1, 2501)),
//    };
//    let _ = app.emit("device_join", device);
//}

#[tauri::command]
#[specta::specta]
fn exit_app(app: tauri::AppHandle) {
    if let Some(state) = app.try_state::<AppState>() {
        windows_utils::remove_all_displays(&state.virtual_display);
    }
    app.exit(0);
}

#[tauri::command]
#[specta::specta]
fn get_username() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .ok()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "User".to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = Builder::<tauri::Wry>::new()
        .commands(collect_commands![
            setup,
            //            get_devices,
            set_current_user,
            set_session_credentials,
            exit_app,
            get_username,
            networking::get_network_adapters,
            networking::watch_for_network_changes,
            hosted_network::start_hosted_network,
            hosted_network::stop_hosted_network,
            hosted_network::is_hosted_network,
            install_drivers,
            set_device_override,
            remove_device_override
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
        .plugin(tauri_plugin_dialog::init())
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
                        let lock_file_path = app.path().resource_dir().unwrap().join("app.lock");
                        let file = OpenOptions::new().write(true).create(true).open(lock_file_path);
                        let mut result = true;
                        if let Err(_) = file {
                            result = app.dialog()
                                .message("Another instance of ScreenExtend has been detected. It is highly recommended to only run one instance at a time. Click OK to continue or Cancel to exit.")
                                .title("ScreenExtend")
                                .buttons(MessageDialogButtons::OkCancel)
                                .blocking_show();
                        } else if let Ok(file) = file {
                            let try_lock = file.try_lock_exclusive();
                            if let Err(_) = try_lock {
                                result = app.dialog()
                                    .message("Another instance of ScreenExtend has been detected. It is highly recommended to only run one instance at a time. Click OK to continue or Cancel to exit.")
                                    .title("ScreenExtend")
                                    .buttons(MessageDialogButtons::OkCancel)
                                    .blocking_show();
                            } else if let Ok(can_lock) = try_lock {
                                if can_lock {
                                    tauri::async_runtime::spawn(async move {
                                        let _ = file.lock_exclusive();
                                    });
                                } else {
                                    result = app.dialog()
                                        .message("Another instance of ScreenExtend has been detected. It is highly recommended to only run one instance at a time. Click OK to continue or Cancel to exit.")
                                        .title("ScreenExtend")
                                        .buttons(MessageDialogButtons::OkCancel)
                                        .blocking_show();
                                }
                            }
                        }
                        if !result {
                            std::process::exit(0);
                        }
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
