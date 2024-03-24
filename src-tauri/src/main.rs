#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#[macro_use]

extern crate lazy_static;
//extern crate pnet;
extern crate local_ip_address;
//use std::net::IpAddr;

use std::sync::mpsc::Sender;
use std::sync::mpsc::channel;
use std::sync::Mutex;

use windows::core::{IInspectable, Result, HSTRING};
use windows::Devices::WiFiDirect::{
    WiFiDirectAdvertisementPublisher, WiFiDirectAdvertisementPublisherStatus,
    WiFiDirectAdvertisementPublisherStatusChangedEventArgs, WiFiDirectConnectionListener,
    WiFiDirectConnectionRequestedEventArgs, WiFiDirectConnectionStatus, WiFiDirectDevice,
    WiFiDirectError,
};
use windows::Foundation::{AsyncOperationCompletedHandler, AsyncStatus, TypedEventHandler};
use windows::Security::Credentials::PasswordCredential;

pub struct WlanHostedNetworkHelper {
    publisher: Mutex<WiFiDirectAdvertisementPublisher>,
}

impl WlanHostedNetworkHelper {
    pub fn new(
        ssid: &str,
        password: &str,
        success_tx: Sender<bool>,
    ) -> Result<Self> {
        let publisher = start(ssid, password, success_tx.clone())?;
        Ok(WlanHostedNetworkHelper {
            publisher: Mutex::new(publisher),
        })
    }

    pub fn stop(&self) -> Result<()> {
        let publisher = self
            .publisher
            .lock()
            .expect("Couldn't lock publisher mutex.");
        let status = publisher.Status()?;
        if status == WiFiDirectAdvertisementPublisherStatus::Started {
            publisher.Stop()?;
        } else {
            println!("Stop called but WiFiDirectAdvertisementPublisher is not running");
        }
        Ok(())
    }
}

fn start_listener() -> Result<()> {
    let listener = WiFiDirectConnectionListener::new()?;
    let connection_requested_callback = TypedEventHandler::<
        WiFiDirectConnectionListener,
        WiFiDirectConnectionRequestedEventArgs,
    >::new(move |_sender, args| {
        println!("Connection requested...");
        let request = args
            .as_ref()
            .expect("args == None in connection requested callback")
            .GetConnectionRequest()?;
        let device_info = request.DeviceInformation()?;
        let device_id = device_info.Id()?;
        let wifi_direct_device = WiFiDirectDevice::FromIdAsync(&device_id)?;
        let async_operation_completed_callback =
            AsyncOperationCompletedHandler::<WiFiDirectDevice>::new(|async_operation, status| {
                if status == AsyncStatus::Completed {
                    let wfd_device = async_operation
                        .as_ref()
                        .expect("No device in WiFiDirectDevice AsyncOperation callback")
                        .GetResults()?;
                    let endpoint_pairs = wfd_device.GetConnectionEndpointPairs()?;
                    let endpoint_pair = endpoint_pairs.GetAt(0)?;
                    let remote_hostname = endpoint_pair.RemoteHostName()?;
                    let _display_name = remote_hostname.DisplayName();
                    let connection_status_changed_callback = TypedEventHandler::<
                        WiFiDirectDevice,
                        IInspectable,
                    >::new(
                        |sender, _inspectable| {
                            let status = sender
                                .as_ref()
                                .expect("No sender in connection status changed handler")
                                .ConnectionStatus()?;
                            match status {
                                WiFiDirectConnectionStatus::Disconnected => {
                                    let _device_id = sender
                                        .as_ref()
                                        .expect("No sender in connection status changed handler")
                                        .DeviceId()?;
                                }
                                _ => (),
                            }
                            Ok(())
                        },
                    );
                    let _event_registration_token =
                        wfd_device.ConnectionStatusChanged(&connection_status_changed_callback);
                }
                Ok(())
            });
        wifi_direct_device.SetCompleted(&async_operation_completed_callback)?;
        Ok(())
    });
    listener.ConnectionRequested(&connection_requested_callback)?;
    Ok(())
}

fn start(
    ssid: &str,
    password: &str,
    success_tx: Sender<bool>,
) -> Result<WiFiDirectAdvertisementPublisher> {
    let publisher = WiFiDirectAdvertisementPublisher::new()?;
    let _ssid = ssid.to_string();
    let publisher_status_changed_callback = TypedEventHandler::<
        WiFiDirectAdvertisementPublisher,
        WiFiDirectAdvertisementPublisherStatusChangedEventArgs,
    >::new(move |_sender, args| {
        let status = args
            .as_ref()
            .expect("args == None in status change callback")
            .Status()?;
        match status {
            WiFiDirectAdvertisementPublisherStatus::Created => println!("Hosted network created"),
            WiFiDirectAdvertisementPublisherStatus::Stopped => println!("Hosted network stopped"),
            WiFiDirectAdvertisementPublisherStatus::Started => {
                start_listener()?;
                println!("Hosted network {} has started", _ssid);
                success_tx
                    .send(true)
                    .expect("Couldn't send hotspot creation success");
            }
            WiFiDirectAdvertisementPublisherStatus::Aborted => {
                let err = match args
                    .as_ref()
                    .expect("args == None in status change callback")
                    .Error()
                    .expect("Couldn't get error")
                {
                    WiFiDirectError::RadioNotAvailable => "Radio not available",
                    WiFiDirectError::ResourceInUse => "Resource in use",
                    WiFiDirectError::Success => "No WiFi Direct-capable card or other error",
                    _ => panic!("got bad WiFiDirectError"),
                };
                println!("Hosted network aborted: {}", err);
                success_tx
                    .send(false)
                    .expect("Couldn't send hotspot creation failure");
            }
            _ => panic!("Bad status received in callback."),
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
    let _ssid = HSTRING::from(ssid);
    legacy_settings.SetSsid(&_ssid)?;
    let password_credential = PasswordCredential::new()?;
    password_credential.SetPassword(&HSTRING::from(password))?;
    legacy_settings.SetPassphrase(&password_credential)?;
    publisher.Start()?;
    Ok(publisher)
}

lazy_static! {
    static ref STOP_CURRENT_HOSTED_NETWORK: Mutex<Option<Box<dyn Fn() + Send + 'static>>> = Mutex::new(None);
}

#[tauri::command]
fn start_hosted_network(ssid: &str, password: &str) -> bool {
    let (success_tx, success_rx) = channel::<bool>();
    let wlan_hosted_network_helper = WlanHostedNetworkHelper::new(ssid, password, success_tx).unwrap();
    let mut stop_func_global = STOP_CURRENT_HOSTED_NETWORK.lock().unwrap();
    *stop_func_global = Some(Box::new(move || wlan_hosted_network_helper.stop().expect("Couldn't stop hosted network.")));
    success_rx.recv().unwrap()
}

#[tauri::command]
fn stop_hosted_network() -> bool {
    if let Some(ref stop_func) = *STOP_CURRENT_HOSTED_NETWORK.lock().unwrap() {
        stop_func();
        true
    } else {
        false
    }
}

extern crate ping;
extern crate rand;

use std::time::Duration;

use rand::random;

use ipnetwork::IpNetwork;
use std::net::IpAddr;

fn list_ips_two(ip_with_subnet: &str) -> Vec<IpAddr> {
    let ip_network = ip_with_subnet.parse::<IpNetwork>().expect("Invalid IP/Subnet");
    let mut ips = Vec::new();

    match ip_network {
        IpNetwork::V4(net) => {
            for ip in net.network().octets()[3]..=net.broadcast().octets()[3] {
                let addr = format!("{}.{}.{}.{}", net.network().octets()[0], net.network().octets()[1], net.network().octets()[2], ip);
                ips.push(addr.parse().unwrap());
            }
        },
        IpNetwork::V6(_net) => {
            // Handling IPv6 subnets can be significantly more complex due to the larger address space.
            // For simplicity, this example focuses on IPv4.
            println!("IPv6 listing is not implemented in this example.");
        },
    }

    ips
}

#[tauri::command]
fn list_ips() {
////    for iface in pnet::datalink::interfaces() {
////        println!("{:?}", iface.ips);
////    }
//    let network_interfaces = local_ip_address::list_afinet_netifas().unwrap();
//    for (name, ip) in network_interfaces.iter() {
//        if matches!(ip, IpAddr::V4(_)) {
//            println!("{}:\t{:?}", name, ip);
//        }
//    }
    for ip in list_ips_two("172.30.192.1/255.255.240.0") { //192.168.88.16/255.255.255.0
        let result = ping::ping(ip, Some(Duration::from_secs(1)), Some(166), Some(3), Some(5), Some(&random()));
        match result {
            Ok(_value) => {
                println!("{:?}", ip);
            },
            Err(_e) => {}
        }
    }
}

use tauri::Manager;


#[derive(Clone, serde::Serialize)]
struct Payload {
    args: Vec<String>,
    cwd: String,
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, argv, cwd| {
            println!("{}, {argv:?}, {cwd}", app.package_info().name);
            app.emit_all("single-instance", Payload { args: argv, cwd }).unwrap();
        }))
        .invoke_handler(tauri::generate_handler![start_hosted_network, stop_hosted_network, list_ips])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}