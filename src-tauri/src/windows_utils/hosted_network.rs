use crate::windows_utils::AppState;
use elevated_command::Command;
use std::process::Command as StdCommand;
use std::sync::mpsc::channel;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use tauri::AppHandle;
use tauri::State;
use tauri_plugin_shell::ShellExt;
use windows::core::{Result, HSTRING};
use windows::Devices::Radios::{Radio, RadioAccessStatus, RadioKind, RadioState};
use windows::Devices::WiFiDirect::{
    WiFiDirectAdvertisementPublisher, WiFiDirectAdvertisementPublisherStatus,
    WiFiDirectAdvertisementPublisherStatusChangedEventArgs,
};
use windows::Foundation::TypedEventHandler;
use windows::Security::Credentials::PasswordCredential;

use windows::Networking::{Connectivity::*, NetworkOperators::*};

fn start_wifi_direct_(
    name: &str,
    password: &str,
    success_tx: Sender<bool>,
) -> Result<WiFiDirectAdvertisementPublisher> {
    let connection_profile = NetworkInformation::GetInternetConnectionProfile()?;
    let tethering_manager =
        NetworkOperatorTetheringManager::CreateFromConnectionProfile(&connection_profile)?;
    let initial_state = tethering_manager.TetheringOperationalState()?;
    if initial_state != windows::Networking::NetworkOperators::TetheringOperationalState(2) {
        let _ = success_tx.send(false);
        return Err(windows::core::Error::new(
            windows::core::HRESULT(1),
            "error while starting hotspot",
        ));
    }

    let publisher = WiFiDirectAdvertisementPublisher::new()?;

    let ssid = HSTRING::from(name);
    let password_credential = PasswordCredential::new()?;
    password_credential.SetPassword(&HSTRING::from(password))?;

    let publisher_status_changed_callback = TypedEventHandler::<
        WiFiDirectAdvertisementPublisher,
        WiFiDirectAdvertisementPublisherStatusChangedEventArgs,
    >::new(move |_sender, args| {
        if let Some(args) = args.as_ref() {
            match args.Status()? {
                WiFiDirectAdvertisementPublisherStatus::Started => {
                    let _ = success_tx.send(true);
                }
                WiFiDirectAdvertisementPublisherStatus::Aborted => {
                    let _ = success_tx.send(false);
                }
                _ => (),
            }
        }
        Ok(())
    });
    publisher.StatusChanged(&publisher_status_changed_callback)?;

    let advertisement = publisher.Advertisement()?;
    advertisement.SetIsAutonomousGroupOwnerEnabled(true)?;

    let legacy_settings = advertisement.LegacySettings()?;
    legacy_settings.SetIsEnabled(true)?;
    legacy_settings.SetSsid(&ssid)?;
    legacy_settings.SetPassphrase(&password_credential)?;

    publisher.Start()?;

    Ok(publisher)
}

fn wifi_radios() -> Vec<Radio> {
    let access = match Radio::RequestAccessAsync().and_then(|op| op.join()) {
        Ok(status) => status,
        Err(_) => return Vec::new(),
    };
    if access != RadioAccessStatus::Allowed {
        return Vec::new();
    }
    let radios = match Radio::GetRadiosAsync().and_then(|op| op.join()) {
        Ok(radios) => radios,
        Err(_) => return Vec::new(),
    };
    radios
        .into_iter()
        .filter(|r| r.Kind().map(|k| k == RadioKind::WiFi).unwrap_or(false))
        .collect()
}

#[tauri::command]
#[specta::specta]
pub fn is_wifi_on() -> bool {
    wifi_radios()
        .iter()
        .any(|r| r.State().map(|s| s == RadioState::On).unwrap_or(false))
}

#[tauri::command]
#[specta::specta]
pub fn turn_on_wifi() -> bool {
    let radios = wifi_radios();
    if radios.is_empty() {
        return false;
    }
    let mut any_on = false;
    for radio in radios {
        if radio.State().map(|s| s == RadioState::On).unwrap_or(false) {
            any_on = true;
            continue;
        }
        match radio.SetStateAsync(RadioState::On).and_then(|op| op.join()) {
            Ok(RadioAccessStatus::Allowed) => any_on = true,
            _ => {}
        }
    }
    any_on
}

fn supports_legacy_hosted_network_(app: AppHandle) -> bool {
    let output = tauri::async_runtime::block_on(async {
        app.shell()
            .command("netsh")
            .args(&["wlan", "show", "drivers"])
            .output()
            .await
    });
    output.map_or(false, |output| {
        String::from_utf8_lossy(&output.stdout).contains("Hosted network supported")
            && String::from_utf8_lossy(&output.stdout)
                .split("Hosted network supported")
                .any(|s| s.trim().starts_with(": Yes"))
    })
}

#[tauri::command]
#[specta::specta]
pub fn start_hosted_network(
    app: AppHandle,
    state: State<'_, AppState>,
    name: &str,
    password: &str,
) -> bool {
    let use_legacy = supports_legacy_hosted_network_(app.clone());
    let to_return;
    if use_legacy {
        let exe_path = match std::env::current_exe() {
            Ok(exe_path) => exe_path.into_os_string().into_string().unwrap(),
            _ => "".to_string(),
        };
        let mut cmd = StdCommand::new(exe_path);
        cmd.arg("hostednetwork");
        cmd.arg(name);
        cmd.arg(password);
        let _ = Command::new(cmd).output();
        let output = tauri::async_runtime::block_on(async {
            app.shell()
                .command("netsh")
                .args(&["wlan", "show", "hostednetwork"])
                .output()
                .await
        });
        to_return = output.map_or(false, |output| {
            String::from_utf8_lossy(&output.stdout).contains("Status")
                && String::from_utf8_lossy(&output.stdout)
                    .split("Status")
                    .any(|s| s.trim().starts_with(": Started"))
        });
    } else {
        let (success_tx, success_rx) = channel::<bool>();
        let publisher = match start_wifi_direct_(name, password, success_tx.clone()) {
            Ok(publisher) => publisher,
            Err(_) => {
                *state.hosted_network_running.lock().unwrap() = false;
                return *state.hosted_network_running.lock().unwrap();
            }
        };
        if !success_rx.recv().unwrap_or(false) {
            *state.hosted_network_running.lock().unwrap() = false;
            return *state.hosted_network_running.lock().unwrap();
        }
        let wlan_hosted_network_helper = Arc::new(Mutex::new(publisher));
        let mut stop_func = state.stop_hosted_network.lock().unwrap();
        *stop_func = Some(Box::new(move || {
            let publisher = wlan_hosted_network_helper.lock().unwrap();
            match publisher.Status() {
                Ok(status) => {
                    if status == WiFiDirectAdvertisementPublisherStatus::Started {
                        match publisher.Stop() {
                            _ => (),
                        }
                    }
                }
                _ => (),
            };
        }));
        to_return = true;
    }
    *state.hosted_network_running.lock().unwrap() = to_return;
    to_return
}

#[tauri::command]
#[specta::specta]
pub fn stop_hosted_network(app: AppHandle, state: State<'_, AppState>) -> bool {
    let use_legacy = supports_legacy_hosted_network_(app.clone());
    let to_return;
    if use_legacy {
        let status = tauri::async_runtime::block_on(async {
            app.shell()
                .command("netsh")
                .args(&["wlan", "stop", "hostednetwork"])
                .status()
                .await
                .unwrap()
        });
        to_return = status.success();
    } else {
        if let Some(ref stop_func) = *state.stop_hosted_network.lock().unwrap() {
            stop_func();
            to_return = true;
        } else {
            to_return = false;
        }
    }
    *state.hosted_network_running.lock().unwrap() = !to_return;
    to_return
}

#[tauri::command]
#[specta::specta]
pub fn is_hosted_network(app: AppHandle, state: State<'_, AppState>) -> bool {
    let use_legacy = supports_legacy_hosted_network_(app.clone());
    if use_legacy {
        let output = tauri::async_runtime::block_on(async {
            app.shell()
                .command("netsh")
                .args(&["wlan", "show", "hostednetwork"])
                .output()
                .await
        });
        output.map_or(false, |output| {
            String::from_utf8_lossy(&output.stdout).contains("Status")
                && String::from_utf8_lossy(&output.stdout)
                    .split("Status")
                    .any(|s| s.trim().starts_with(": Started"))
        })
    } else {
        return *state.hosted_network_running.lock().unwrap();
    }
}
