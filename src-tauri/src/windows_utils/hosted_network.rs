use crate::windows_utils::AppState;
use elevated_command::Command;
use tauri::AppHandle;
use tauri_plugin_shell::ShellExt;
use std::process::Command as StdCommand;
use std::sync::mpsc::channel;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use tauri::State;
use windows::core::{Result, HSTRING};
use windows::Devices::WiFiDirect::{
    WiFiDirectAdvertisementPublisher, WiFiDirectAdvertisementPublisherStatus,
    WiFiDirectAdvertisementPublisherStatusChangedEventArgs,
};
use windows::Foundation::TypedEventHandler;
use windows::Security::Credentials::PasswordCredential;

fn start_wifi_direct_(
    name: &str,
    password: &str,
    success_tx: Sender<bool>,
) -> Result<WiFiDirectAdvertisementPublisher> {
    let publisher = WiFiDirectAdvertisementPublisher::new()?;

    let ssid = HSTRING::from(name);
    let password_credential = PasswordCredential::new()?;
    password_credential.SetPassword(&HSTRING::from(password))?;

    let publisher_status_changed_callback = TypedEventHandler::<
        WiFiDirectAdvertisementPublisher,
        WiFiDirectAdvertisementPublisherStatusChangedEventArgs,
    >::new(move |_sender, args| {
        let status = args
            .as_ref()
            .expect("args == None in status change callback")
            .Status()?;
        match status {
            WiFiDirectAdvertisementPublisherStatus::Started => {
                success_tx.send(true).expect("Failed to send status")
            }
            WiFiDirectAdvertisementPublisherStatus::Aborted => {
                success_tx.send(false).expect("Failed to send status")
            }
            _ => (),
        }
        Ok(())
    });
    publisher.StatusChanged(&publisher_status_changed_callback)?;

    let advertisement = publisher
        .Advertisement()
        .expect("Error getting advertisement");
    advertisement.SetIsAutonomousGroupOwnerEnabled(true)?;

    let legacy_settings = advertisement.LegacySettings()?;
    legacy_settings.SetIsEnabled(true)?;
    legacy_settings.SetSsid(&ssid)?;
    legacy_settings.SetPassphrase(&password_credential)?;

    publisher.Start()?;

    Ok(publisher)
}

fn supports_legacy_hosted_network_(app: AppHandle) -> bool {
    let output = tauri::async_runtime::block_on(async {
        app.shell().command("netsh").args(&["wlan", "show", "drivers"]).output().await
    });
    output.map_or(false, |output| String::from_utf8_lossy(&output.stdout).contains("Hosted network supported") && String::from_utf8_lossy(&output.stdout).split("Hosted network supported").any(|s| s.trim().starts_with(": Yes")))
}

#[tauri::command]
#[specta::specta]
pub fn start_hosted_network(app: AppHandle, state: State<'_, AppState>, name: &str, password: &str) -> bool {
    let use_legacy = supports_legacy_hosted_network_(app.clone());
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
            app.shell().command("netsh").args(&["wlan", "show", "hostednetwork"]).output().await
        });
        output.map_or(false, |output| String::from_utf8_lossy(&output.stdout).contains("Status") && String::from_utf8_lossy(&output.stdout).split("Status").any(|s| s.trim().starts_with(": Started")))
    } else {
        let (success_tx, success_rx) = channel::<bool>();
        let publisher = match start_wifi_direct_(name, password, success_tx.clone()) {
            Ok(publisher) => publisher,
            Err(_err) => return false,
        };
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
        success_rx.recv().unwrap()
    }
}

#[tauri::command]
#[specta::specta]
pub fn stop_hosted_network(app: AppHandle, state: State<'_, AppState>) -> bool {
    let use_legacy = supports_legacy_hosted_network_(app.clone());
    if use_legacy {
        let status = tauri::async_runtime::block_on(async {
            app.shell().command("netsh").args(&["wlan", "stop", "hostednetwork"]).status().await.unwrap()
        });
        status.success()
    } else {
        if let Some(ref stop_func) = *state.stop_hosted_network.lock().unwrap() {
            stop_func();
            true
        } else {
            false
        }
    }
}