#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#[macro_use]

extern crate lazy_static;
//extern crate pnet;
extern crate local_ip_address;
//use std::net::IpAddr;

mod windows_utils;

use windows_utils::hosted_network;
use tauri::Window;

#[tauri::command]
fn start_hosted_network(ssid: &str, password: &str) -> bool {
    hosted_network::start_hosted_network(ssid, password)
}

#[tauri::command]
fn stop_hosted_network() -> bool {
    hosted_network::stop_hosted_network()
}

#[tauri::command]
fn fetch_urls(window: Window) {
    let _ = window.emit("local_url", "https://192.168.88.1:8000/");
    let _ = window.emit("global_url", "https://screenextend.tech/sess/abcdefgh");
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
        .invoke_handler(tauri::generate_handler![start_hosted_network, stop_hosted_network, list_ips, fetch_urls])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}