//! The network as this machine sees it: interfaces and their addresses,
//! default routes, DNS, and which processes listen on which ports.

use std::collections::HashMap;
use std::ffi::CStr;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::policy::Policy;
use crate::tool::{parse_args, Risk, Tool};

pub struct NetInfo;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    #[serde(default = "yes")]
    listening: bool,
}

fn yes() -> bool {
    true
}

impl Tool for NetInfo {
    fn name(&self) -> &'static str {
        "net_info"
    }

    fn description(&self) -> &'static str {
        "Describe the network: each interface (state, kind, MAC, addresses, traffic), the default routes, \
         the DNS servers, and the TCP and UDP ports processes listen on, with the process behind each."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "listening": { "type": "boolean", "default": true, "description": "Include listening ports." }
            },
            "additionalProperties": false
        })
    }

    fn call(&self, _policy: &Policy, args: Value) -> Result<Value> {
        let a: Args = parse_args(args)?;
        let mut out = json!({ "interfaces": interfaces(), "routes": default_routes(), "dns": dns() });
        if a.listening {
            out["listening"] = listening()
                .into_iter()
                .map(|l| {
                    json!({ "proto": l.proto, "address": l.addr.to_string(), "port": l.port,
                            "pid": l.pid, "process": l.process })
                })
                .collect();
        }
        Ok(out)
    }
}

fn read(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Addresses per interface, from getifaddrs(3).
fn addresses() -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `head` with a list we free below; each entry
    // and its sockaddr stay valid until freeifaddrs.
    unsafe {
        if libc::getifaddrs(&mut head) != 0 {
            return out;
        }
        let mut cur = head;
        while let Some(ifa) = cur.as_ref() {
            cur = ifa.ifa_next;
            if ifa.ifa_addr.is_null() || ifa.ifa_name.is_null() {
                continue;
            }
            let name = CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned();
            let prefix = |mask: *const libc::sockaddr| -> u32 {
                match mask.as_ref().map(|m| m.sa_family as i32) {
                    Some(libc::AF_INET) => (*(mask as *const libc::sockaddr_in)).sin_addr.s_addr.count_ones(),
                    Some(libc::AF_INET6) => {
                        (*(mask as *const libc::sockaddr_in6)).sin6_addr.s6_addr.iter().map(|b| b.count_ones()).sum()
                    }
                    _ => 0,
                }
            };
            let addr = match (*ifa.ifa_addr).sa_family as i32 {
                libc::AF_INET => {
                    let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                    IpAddr::V4(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)))
                }
                libc::AF_INET6 => {
                    let sin6 = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                    IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr))
                }
                _ => continue,
            };
            out.entry(name).or_default().push(format!("{addr}/{}", prefix(ifa.ifa_netmask)));
        }
        libc::freeifaddrs(head);
    }
    out
}

fn interfaces() -> Vec<Value> {
    let addrs = addresses();
    let Ok(entries) = fs::read_dir("/sys/class/net") else { return Vec::new() };
    let mut names: Vec<String> = entries.filter_map(|e| e.ok()?.file_name().into_string().ok()).collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let dir = Path::new("/sys/class/net").join(&name);
            let num = |f: &str| read(dir.join(f)).and_then(|v| v.parse::<i64>().ok());
            let kind = if name == "lo" {
                "loopback"
            } else if dir.join("wireless").exists() || dir.join("phy80211").exists() {
                "wireless"
            } else if dir.join("bridge").exists() {
                "bridge"
            } else if dir.join("device").exists() {
                "ethernet"
            } else {
                "virtual"
            };
            json!({
                "name": name,
                "kind": kind,
                "state": read(dir.join("operstate")),
                "mac": read(dir.join("address")).filter(|m| m != "00:00:00:00:00:00"),
                "mtu": num("mtu"),
                "speed_mbps": num("speed").filter(|s| *s > 0),
                "addresses": addrs.get(&name).cloned().unwrap_or_default(),
                "rx_bytes": num("statistics/rx_bytes"),
                "tx_bytes": num("statistics/tx_bytes"),
            })
        })
        .collect()
}

fn default_routes() -> Vec<Value> {
    let mut out = Vec::new();
    for line in read("/proc/net/route").unwrap_or_default().lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() > 2 && f[1] == "00000000" {
            let gw = u32::from_str_radix(f[2], 16).map(|g| Ipv4Addr::from(u32::from_be(g))).ok();
            out.push(json!({ "family": "ipv4", "interface": f[0], "gateway": gw.map(|g| g.to_string()) }));
        }
    }
    for line in read("/proc/net/ipv6_route").unwrap_or_default().lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() == 10 && f[0].chars().all(|c| c == '0') && f[1] == "00" && f[9] != "lo" {
            let gw = u128::from_str_radix(f[4], 16).map(Ipv6Addr::from).ok().filter(|g| !g.is_unspecified());
            out.push(json!({ "family": "ipv6", "interface": f[9], "gateway": gw.map(|g| g.to_string()) }));
        }
    }
    out
}

fn dns() -> Value {
    let parse = |text: String| {
        let mut servers = Vec::new();
        let mut search = Vec::new();
        for line in text.lines() {
            let mut f = line.split_whitespace();
            match f.next() {
                Some("nameserver") => servers.extend(f.next().map(str::to_string)),
                Some("search") | Some("domain") => search.extend(f.map(str::to_string)),
                _ => {}
            }
        }
        (servers, search)
    };
    let (servers, search) = parse(read("/etc/resolv.conf").unwrap_or_default());
    let mut out = json!({ "servers": servers, "search": search });
    // systemd-resolved's stub hides the real servers.
    if servers.iter().any(|s| s == "127.0.0.53") {
        if let Some(text) = read("/run/systemd/resolve/resolv.conf") {
            out["upstream"] = json!(parse(text).0);
        }
    }
    out
}

pub struct Listener {
    pub proto: &'static str,
    pub addr: IpAddr,
    pub port: u16,
    pub pid: Option<u32>,
    pub process: Option<String>,
}

/// An address from /proc/net: IPv4 as one host-order word, IPv6 as four.
fn proc_addr(hex: &str) -> Option<IpAddr> {
    match hex.len() {
        8 => Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(u32::from_str_radix(hex, 16).ok()?)))),
        32 => {
            let mut bytes = [0u8; 16];
            for (i, chunk) in bytes.chunks_mut(4).enumerate() {
                let word = u32::from_str_radix(&hex[i * 8..i * 8 + 8], 16).ok()?;
                chunk.copy_from_slice(&u32::from_be(word).to_be_bytes());
            }
            let v6 = Ipv6Addr::from(bytes);
            Some(v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4))
        }
        _ => None,
    }
}

/// Sockets in LISTEN (TCP) or unconnected (UDP), with their owners where
/// /proc lets us see them (our own processes, or all of them as root).
pub fn listening() -> Vec<Listener> {
    let owners = socket_owners();
    let mut out = Vec::new();
    for (proto, file, state) in
        [("tcp", "tcp", "0A"), ("tcp", "tcp6", "0A"), ("udp", "udp", "07"), ("udp", "udp6", "07")]
    {
        for line in read(format!("/proc/net/{file}")).unwrap_or_default().lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 10 || f[3] != state || (proto == "udp" && !f[2].ends_with(":0000")) {
                continue;
            }
            let Some((addr, port)) = f[1].split_once(':') else { continue };
            let (Some(addr), Ok(port)) = (proc_addr(addr), u16::from_str_radix(port, 16)) else { continue };
            let owner = f[9].parse::<u64>().ok().and_then(|inode| owners.get(&inode));
            out.push(Listener {
                proto,
                addr,
                port,
                pid: owner.map(|o| o.0),
                process: owner.map(|o| o.1.clone()),
            });
        }
    }
    out.sort_by_key(|l| (l.proto, l.port, l.addr));
    out.dedup_by(|a, b| a.proto == b.proto && a.port == b.port && a.addr == b.addr);
    out
}

/// SSH sessions into this machine: (interface, remote address). Changing
/// that interface's connection, or the firewall for SSH, would cut them off.
pub fn ssh_sessions() -> Vec<(String, String)> {
    let ports = ssh_ports();
    let addrs = addresses();
    let iface_of = |ip: &IpAddr| {
        addrs
            .iter()
            .find(|(_, list)| list.iter().any(|a| a.split('/').next() == Some(ip.to_string().as_str())))
            .map(|(name, _)| name.clone())
    };
    let mut out = Vec::new();
    for file in ["tcp", "tcp6"] {
        let text = read(format!("/proc/net/{file}")).unwrap_or_default();
        for (local, remote) in established(&text, &ports) {
            if let Some(iface) = iface_of(&local).filter(|i| i != "lo") {
                out.push((iface, remote));
            }
        }
    }
    out
}

/// The ports sshd listens on (22 if it can't be seen).
pub fn ssh_ports() -> Vec<u16> {
    let mut ports: Vec<u16> = listening().iter().filter(|l| l.proto == "tcp" && l.process.as_deref() == Some("sshd")).map(|l| l.port).collect();
    if ports.is_empty() {
        ports.push(22);
    }
    ports.sort();
    ports.dedup();
    ports
}

/// Established TCP connections to one of `ports`: (local address, remote).
pub fn established(proc_net: &str, ports: &[u16]) -> Vec<(IpAddr, String)> {
    proc_net
        .lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 4 || f[3] != "01" {
                return None;
            }
            let (la, lp) = f[1].split_once(':')?;
            let (ra, rp) = f[2].split_once(':')?;
            let port = u16::from_str_radix(lp, 16).ok()?;
            if !ports.contains(&port) {
                return None;
            }
            let remote = format!("{}:{}", proc_addr(ra)?, u16::from_str_radix(rp, 16).ok()?);
            Some((proc_addr(la)?, remote))
        })
        .collect()
}

/// Socket inode → (pid, process name).
fn socket_owners() -> HashMap<u64, (u32, String)> {
    let mut out = HashMap::new();
    let Ok(procs) = fs::read_dir("/proc") else { return out };
    for pid in procs.filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok()) {
        let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) else { continue };
        let name = read(format!("/proc/{pid}/comm")).unwrap_or_default();
        for fd in fds.flatten() {
            let Ok(target) = fs::read_link(fd.path()) else { continue };
            let target = target.to_string_lossy();
            if let Some(inode) = target.strip_prefix("socket:[").and_then(|t| t.strip_suffix(']')) {
                if let Ok(inode) = inode.parse() {
                    out.entry(inode).or_insert_with(|| (pid, name.clone()));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_ssh_sessions() {
        let tcp = "  sl  local_address rem_address   st tx_queue rx_queue\n   0: 0F02000A:0016 1402A8C0:D431 01 00000000:00000000\n   1: 00000000:0016 00000000:0000 0A 00000000:00000000\n   2: 0F02000A:1F90 1402A8C0:D432 01 00000000:00000000\n";
        let s = established(tcp, &[22]);
        assert_eq!(s, [("10.0.2.15".parse::<IpAddr>().unwrap(), "192.168.2.20:54321".to_string())]);
        assert!(established(tcp, &[2222]).is_empty());
    }

    #[test]
    fn decodes_proc_addresses() {
        assert_eq!(proc_addr("0100007F"), Some(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert_eq!(proc_addr("00000000000000000000000001000000"), Some(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert_eq!(proc_addr("0000000000000000FFFF00000100007F"), Some(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert_eq!(proc_addr("zz"), None);
    }

    #[test]
    fn sees_loopback_and_our_listener() {
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = socket.local_addr().unwrap().port();
        let r = NetInfo.call(&Policy::default(), json!({})).unwrap();
        let lo = r["interfaces"].as_array().unwrap().iter().find(|i| i["name"] == "lo").unwrap();
        assert_eq!(lo["kind"], "loopback");
        assert!(lo["addresses"].as_array().unwrap().iter().any(|a| a == "127.0.0.1/8"), "{lo}");
        let mine = r["listening"].as_array().unwrap().iter().find(|l| l["port"] == port).cloned();
        let mine = mine.unwrap_or_else(|| panic!("port {port} missing: {r}"));
        assert_eq!(mine["proto"], "tcp");
        assert_eq!(mine["address"], "127.0.0.1");
        assert_eq!(mine["pid"], std::process::id());
    }
}
