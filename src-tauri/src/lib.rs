//use rand::Rng;
use serde::{Deserialize, Serialize};
use specta::Type;
#[cfg(debug_assertions)]
use specta_typescript::Typescript;
use tauri::path::BaseDirectory;
//use tauri::Emitter;
use tauri::Manager;
use tauri_plugin_cli::CliExt;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
use tauri_plugin_shell::ShellExt;
use tauri_specta::{collect_commands, collect_events, Builder, Event};

#[macro_use]
mod logbus;
mod cli;
mod single_instance;
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
pub struct HostedNetworkNoPassword;

#[derive(Serialize, Deserialize, Debug, Clone, Type, Event)]
pub struct CloudStatusChange {
    pub state: String,
    pub detail: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Type)]
pub struct PermissionStatus {
    pub key: String,
    pub name: String,
    pub description: String,
    pub granted: bool,
    pub required: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Type, Event)]
pub struct SessionIdChange {
    #[serde(rename = "sessionId")]
    pub session_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Type, Event)]
pub struct JoinAttemptsPaused {
    #[serde(rename = "retryAfterSecs")]
    pub retry_after_secs: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, Type)]
pub struct UnsupportedApi {
    pub name: String,
    pub description: String,
    pub required_version: String,
    pub severity: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Type)]
pub struct CompatibilityReport {
    pub os_name: String,
    pub os_version: String,
    pub min_os_version: String,
    pub os_supported: bool,
    pub unsupported_apis: Vec<UnsupportedApi>,
    pub audio_backend: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Type, Event)]
pub struct Device {
    pub ip: String,
    pub token: String,
    pub name: String,
    pub scale: u32,
    pub orientation: String,
    #[serde(rename = "refreshRate")]
    pub refresh_rate: u32,
    #[serde(rename = "videoScale")]
    pub video_scale: u32,
    #[serde(rename = "videoQuality")]
    pub video_quality: u32,
    #[serde(rename = "remoteControl")]
    pub control_enabled: bool,
    #[serde(rename = "systemAudio")]
    pub audio_enabled: bool,
    pub os: String,
    #[serde(rename = "screenSize")]
    pub screen_size: String,
    pub dpr: f64,
    #[serde(rename = "maxDpr")]
    pub max_dpr: f64,
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
        let cap = crate::streamer::platform::max_display_dpr();
        let native_dpr = if info.dpr.is_finite() && info.dpr >= 1.0 {
            info.dpr.min(cap)
        } else {
            1.0
        };
        Self {
            ip: info.ip,
            token: info.token,
            name: info.name,
            scale: 100,
            orientation: if info.portrait {
                "Portrait"
            } else {
                "Landscape"
            }
            .to_string(),
            refresh_rate,
            video_scale: 100,
            video_quality: 15,
            control_enabled: true,
            audio_enabled: false,
            os: info.os,
            screen_size: info.screen_size,
            dpr: native_dpr,
            max_dpr: native_dpr,
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
        remove_all_displays(&state.virtual_display);
    }
    app.exit(0);
}

#[tauri::command]
#[specta::specta]
fn get_username() -> String {
    whoami::username().unwrap_or_else(|_| "".to_string())
}

fn avatar_path(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("avatar.png"))
}

#[tauri::command]
#[specta::specta]
fn set_avatar(app: tauri::AppHandle, bytes: Vec<u8>) -> bool {
    match avatar_path(&app)
        .and_then(|path| std::fs::write(&path, &bytes).map_err(|e| e.to_string()))
    {
        Ok(()) => true,
        Err(e) => {
            log::error!("failed to save avatar: {e}");
            false
        }
    }
}

#[tauri::command]
#[specta::specta]
fn get_avatar(app: tauri::AppHandle) -> Option<Vec<u8>> {
    let path = avatar_path(&app).ok()?;
    match std::fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            log::error!("failed to read avatar: {e}");
            None
        }
    }
}

#[tauri::command]
#[specta::specta]
fn remove_avatar(app: tauri::AppHandle) -> bool {
    let path = match avatar_path(&app) {
        Ok(path) => path,
        Err(e) => {
            log::error!("failed to resolve avatar path: {e}");
            return false;
        }
    };
    match std::fs::remove_file(&path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            log::error!("failed to remove avatar: {e}");
            false
        }
    }
}

#[cfg(target_os = "macos")]
fn build_menu(handle: &tauri::AppHandle) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use tauri::menu::{AboutMetadata, MenuBuilder, PredefinedMenuItem, SubmenuBuilder};

    let about = PredefinedMenuItem::about(
        handle,
        Some("About ScreenExtend"),
        Some(AboutMetadata {
            name: Some("ScreenExtend".into()),
            ..Default::default()
        }),
    )?;
    let quit = PredefinedMenuItem::quit(handle, Some("Quit ScreenExtend"))?;
    let app_menu = SubmenuBuilder::new(handle, "ScreenExtend")
        .item(&about)
        .separator()
        .item(&quit)
        .build()?;
    MenuBuilder::new(handle).item(&app_menu).build()
}

#[cfg(not(target_os = "macos"))]
fn build_menu(handle: &tauri::AppHandle) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    tauri::menu::MenuBuilder::new(handle).build()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    cli::fast_path();

    let builder = Builder::<tauri::Wry>::new()
        .commands(collect_commands![
            compatibility::check_system_requirements,
            permissions::check_permissions,
            permissions::request_permission,
            permissions::open_permission_settings,
            setup,
            //            get_devices,
            set_session_credentials,
            register_cloud_session,
            unregister_cloud_session,
            get_cloud_status,
            exit_app,
            get_username,
            set_avatar,
            get_avatar,
            remove_avatar,
            networking::get_network_adapters,
            networking::watch_for_network_changes,
            hosted_network::start_hosted_network,
            hosted_network::stop_hosted_network,
            hosted_network::is_hosted_network,
            hosted_network::is_wifi_on,
            hosted_network::turn_on_wifi,
            install_drivers,
            remove_drivers,
            install_audio_driver,
            uninstall_audio_driver,
            audio_driver_status,
            set_device_override,
            remove_device_override,
            set_device_banned,
            set_device_approved,
            set_disconnect_grace,
            get_disconnect_grace,
            set_turn_config,
            get_turn_config,
            set_server_ports,
            get_server_ports,
            set_disable_gpu_encode,
            get_disable_gpu_encode,
            logbus::get_log_backlog
        ])
        .events(collect_events![
            DeviceJoin,
            DeviceModify,
            DeviceModifyAction,
            DeviceRemove,
            DeviceRemoveAction,
            NetworkChange,
            HostedNetworkNoPassword,
            CloudStatusChange,
            SessionIdChange,
            JoinAttemptsPaused,
            logbus::LogLine
        ]);

    #[cfg(debug_assertions)]
    if std::env::args_os().nth(1).is_none() {
        builder
            .export(Typescript::default(), "../src/lib/bindings.ts")
            .expect("error while exporting typescript bindings");
        let bindings_path = "../src/lib/bindings.ts";
        let contents = std::fs::read_to_string(bindings_path)
            .expect("error while reading typescript bindings");
        std::fs::write(bindings_path, format!("// @ts-nocheck\n{contents}"))
            .expect("error while writing typescript bindings");
    }

    tauri::Builder::default()
        .plugin(
            tauri_plugin_updater::Builder::new()
                .default_version_comparator(|current, update| update.version != current)
                .build(),
        )
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_os::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_cli::init())
        .plugin(tauri_plugin_http::init())
        .plugin(tauri_plugin_store::Builder::default().build())
        .menu(build_menu)
        .invoke_handler(builder.invoke_handler())
        .setup(move |app| {
            builder.mount_events(app);
            let _ = crate::cli::dispatch(app.handle());
            crate::streamer::input::prime();
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
                                .args([
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
                                .args(["wlan", "start", "hostednetwork"])
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
                                .args([
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
                                .args([
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
                                .args([
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
                                .args([
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
                                .args([
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
                    Some(command) if command.name == "removedrivers" => {
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
                                .command("nefconc")
                                .args([
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
                                .args([
                                    "--uninstall-driver",
                                    "--inf-path",
                                    &resource_path("resources/VirtualDisplayDriver.inf"),
                                ])
                                .current_dir(app.path().resource_dir().unwrap())
                                .output()
                                .await
                                .unwrap();
                            app.shell()
                                .command("certutil")
                                .args(["-delstore", "root", "ScreenExtend"])
                                .current_dir(app.path().resource_dir().unwrap())
                                .output()
                                .await
                                .unwrap();
                            app.shell()
                                .command("certutil")
                                .args(["-delstore", "TrustedPublisher", "ScreenExtend"])
                                .current_dir(app.path().resource_dir().unwrap())
                                .output()
                                .await
                                .unwrap();
                        });
                        app.handle().exit(0);
                    }
                    _ => {
                        let lock_dir = app.path().app_local_data_dir().unwrap();
                        let _ = std::fs::create_dir_all(&lock_dir);
                        let lock_path = lock_dir.join(single_instance::LOCK_FILE);
                        let ctrl_path = lock_dir.join(single_instance::CTRL_FILE);

                        let mut lock = single_instance::acquire_lock(&lock_path);

                        if lock.is_none() {
                            let quit_other = app
                                .dialog()
                                .message("ScreenExtend is already running.")
                                .title("ScreenExtend")
                                .buttons(MessageDialogButtons::OkCancelCustom(
                                    "Quit running instance".into(),
                                    "Show running instance".into(),
                                ))
                                .blocking_show();

                            if quit_other {
                                single_instance::signal_running_instance(
                                    &ctrl_path,
                                    single_instance::Command::Quit,
                                );
                                lock = single_instance::wait_for_lock(
                                    &lock_path,
                                    std::time::Duration::from_secs(10),
                                );
                                if lock.is_none() {
                                    std::process::exit(0);
                                }
                            } else {
                                single_instance::signal_running_instance(
                                    &ctrl_path,
                                    single_instance::Command::Focus,
                                );
                                std::process::exit(0);
                            }
                        }

                        if let Some(file) = lock {
                            std::mem::forget(file);
                        }

                        logbus::attach(app.handle().clone());
                        let window = tauri::WebviewWindowBuilder::new(
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

                        focus_main_window(&window);

                        if let Err(e) = single_instance::start_control_server(
                            app.handle().clone(),
                            ctrl_path,
                            focus_main_window,
                        ) {
                            teprintln!("[single-instance] failed to start control server: {e}");
                        }
                    }
                }
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
