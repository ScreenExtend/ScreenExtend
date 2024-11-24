use crate::windows_utils::AppState;
use elevated_command::Command;
use std::process::Command as StdCommand;
use std::sync::mpsc::channel;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use tauri::api::process::Command as TauriCommand;
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

fn supports_legacy_hosted_network_() -> bool {
    TauriCommand::new("netsh")
        .args(&["wlan", "show", "drivers"])
        .output()
        .map_or(false, |output| {
            let output_str = output.stdout;
            output_str.contains("Hosted network supported")
                && output_str
                    .split("Hosted network supported")
                    .any(|s| s.trim().starts_with(": Yes"))
        })
}

#[tauri::command]
#[specta::specta]
pub fn start_hosted_network(state: State<'_, AppState>, name: &str, password: &str) -> bool {
    let use_legacy = supports_legacy_hosted_network_();
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
        TauriCommand::new("netsh")
            .args(&["wlan", "show", "hostednetwork"])
            .output()
            .map_or(false, |output| {
                let output_str = output.stdout;
                output_str.contains("Status") && output_str.split("Status").any(|s| s.trim().starts_with(": Started"))
            })
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
pub fn stop_hosted_network(state: State<'_, AppState>) -> bool {
    let use_legacy = supports_legacy_hosted_network_();
    if use_legacy {
        TauriCommand::new("netsh")
            .args(&["wlan", "stop", "hostednetwork"])
            .status()
            .map_or(false, |status| status.success())
    } else {
        if let Some(ref stop_func) = *state.stop_hosted_network.lock().unwrap() {
            stop_func();
            true
        } else {
            false
        }
    }
}
