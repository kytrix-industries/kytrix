//! WPAD URL discovery through NetworkManager and the resolver search list.

use std::process::Command;

use crate::system_proxy::linux::command::bounded_command_output;

pub(super) fn discover_wpad_url() -> Option<String> {
    if let Some(url) = network_manager_wpad_url() {
        return Some(url);
    }
    dns_wpad_url()
}

fn network_manager_wpad_url() -> Option<String> {
    let mut command = network_manager_wpad_command();
    let output = bounded_command_output(&mut command)?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    network_manager_wpad_url_from_output(&text)
}

pub(super) fn network_manager_wpad_command() -> Command {
    let mut command = Command::new("nmcli");
    command.args(["-t", "-e", "no", "-f", "DHCP4.OPTION", "device", "show"]);
    command
}

pub(super) fn network_manager_wpad_url_from_output(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        let Some(value) = line
            .split_once("wpad = ")
            .or_else(|| line.split_once("wpad:"))
            .map(|(_, value)| value.trim())
        else {
            continue;
        };
        if value.starts_with("http://") || value.starts_with("https://") {
            return Some(value.to_owned());
        }
    }
    None
}

fn dns_wpad_url() -> Option<String> {
    let search = std::fs::read_to_string("/etc/resolv.conf").ok()?;
    for line in search.lines() {
        let line = line.trim();
        let domains = line
            .strip_prefix("search ")
            .or_else(|| line.strip_prefix("domain "));
        let Some(domains) = domains else {
            continue;
        };
        for domain in domains.split_whitespace() {
            if domain.is_empty() || domain == "." {
                continue;
            }
            return Some(format!("http://wpad.{domain}/wpad.dat"));
        }
    }
    None
}
