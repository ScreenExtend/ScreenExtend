use local_ip_address::list_afinet_netifas;
use local_ip_address::local_ip;
use std::net::IpAddr;

#[tauri::command]
#[specta::specta]
pub fn get_private_ip_addresses() -> Vec<String> {
    let mut private_ips = Vec::new();
    let network_interfaces = list_afinet_netifas().unwrap();
    for (_, ip) in network_interfaces.iter() {
        if let IpAddr::V4(ipv4) = ip {
            let octets = ipv4.octets();
            let is_private = match octets[0] {
                10 => true,
                172 if octets[1] >= 16 && octets[1] <= 31 => true,
                192 if octets[1] == 168 => true,
                _ => false,
            };
            if is_private {
                private_ips.push(ipv4.to_string());
            }
        }
    }
    private_ips
}

#[tauri::command]
#[specta::specta]
pub fn get_private_ip_address() -> String {
    let local_ip = local_ip();
    if let Err(_err) = local_ip {
        "".to_string()
    } else {
        local_ip.unwrap().to_string()
    }
}