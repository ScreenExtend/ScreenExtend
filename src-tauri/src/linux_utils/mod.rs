//pub mod hosted_network;
//
//use std::process::Command;
//
//fn command_exists(cmd: &str) -> bool {
//    Command::new("which")
//        .arg(cmd)
//        .output()
//        .map(|output| output.status.success())
//        .unwrap_or(false)
//}
//
//pub async fn setup() -> bool {
//    command_exists("nmcli")
//}