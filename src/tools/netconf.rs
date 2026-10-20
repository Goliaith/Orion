//! Changing the network: Wi-Fi and saved connections (through
//! NetworkManager's nmcli), and the firewall (ufw or firewalld).
//!
//! What would cut off the person using the machine remotely is refused
//! before anyone is asked: taking down the connection an SSH session runs
//! over, blocking SSH in the firewall, or turning the firewall on without
//! SSH allowed. Wi-Fi passwords never reach the audit log, approval
//! prompts, results, or another process's command line.

use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use super::exec;
use crate::policy::Policy;
use crate::tool::{parse_args, Risk, Tool};

/// Split one line of `nmcli -t` output: fields separated by `:`, with `\:`
/// and `\\` escaped.
pub fn nm_fields(line: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(n) = chars.next() {
                    out.last_mut().unwrap_or(&mut String::new()).push(n);
                }
            }
            ':' => out.push(String::new()),
            c => out.last_mut().unwrap_or(&mut String::new()).push(c),
        }
    }
    out
}

/// Run a program that may need root: as is when Orion is root; otherwise
/// directly first (polkit may allow it, as for nmcli), then elevated if
/// that was refused for lack of permission.
fn admin(policy: &Policy, program: &str, args: &[String], stdin: Option<String>) -> Result<exec::Finished> {
    let go = |prefix: Option<&[&str]>| {
        exec::run(exec::command_as(prefix, program, args), stdin.clone(), Duration::from_secs(90), 256 << 10)
    };
    let done = go(None)?;
    let refused = |d: &exec::Finished| {
        let e = format!("{} {}", d.stderr, d.stdout).to_lowercase();
        ["not authorized", "insufficient privileges", "permission denied", "need to be root", "must be root", "operation not permitted"]
            .iter()
            .any(|w| e.contains(w))
    };
    if done.success() || !refused(&done) {
        return Ok(done);
    }
    match exec::elevation(&policy.admin.elevate)? {
        Some(prefix) => go(Some(&prefix)),
        None => Ok(done),
    }
}

fn admin_ok(policy: &Policy, program: &str, args: &[&str]) -> Result<String> {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let done = admin(policy, program, &args, None)?;
    ensure!(done.success(), "{program}: {}", done.complaint());
    Ok(done.stdout)
}

fn nmcli(policy: &Policy, args: &[&str]) -> Result<String> {
    ensure!(exec::installed("nmcli"), "NetworkManager (nmcli) isn't installed; Orion changes the network through it");
    admin_ok(policy, "nmcli", args)
}

/// Devices whose connection carries an SSH session, and from where.
fn ssh_guard(device: &str) -> Result<()> {
    if let Some((_, remote)) = super::network::ssh_sessions().into_iter().find(|(iface, _)| iface == device) {
        bail!("{device} carries an SSH session from {remote}; changing it would cut that session off");
    }
    Ok(())
}

// ---- Wi-Fi and connections ----

pub struct WifiScan;

impl Tool for WifiScan {
    fn name(&self) -> &'static str {
        "wifi_scan"
    }

    fn description(&self) -> &'static str {
        "List the Wi-Fi networks in range: name, signal, security, channel and speed, strongest first, marking \
         the one in use and those with a saved connection."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct NoArgs {}
        let NoArgs {} = parse_args(args)?;
        let list = nmcli(policy, &["-t", "-e", "yes", "-f", "IN-USE,SSID,BSSID,SIGNAL,SECURITY,CHAN,RATE", "device", "wifi", "list", "--rescan", "auto"])?;
        let saved = saved_connections(policy).unwrap_or_default();
        let networks = parse_wifi(&list);
        Ok(json!({
            "networks": networks.into_iter().map(|mut n| {
                n["saved"] = json!(saved.iter().any(|c| c["type"] == "802-11-wireless" && c["name"] == n["ssid"]));
                n
            }).collect::<Vec<_>>(),
        }))
    }
}

/// `nmcli device wifi list`, one entry per network name (its strongest
/// access point), strongest first.
pub fn parse_wifi(text: &str) -> Vec<Value> {
    let mut best: Vec<Value> = Vec::new();
    for line in text.lines() {
        let f = nm_fields(line);
        if f.len() < 7 || f[1].is_empty() {
            continue;
        }
        let signal: u64 = f[3].parse().unwrap_or(0);
        let entry = json!({
            "ssid": f[1], "in_use": f[0] == "*", "signal_pct": signal,
            "security": if f[4].is_empty() || f[4] == "--" { "open".to_string() } else { f[4].clone() },
            "channel": f[5].parse::<u64>().ok(), "rate": f[6], "bssid": f[2],
        });
        match best.iter_mut().find(|b| b["ssid"] == f[1].as_str()) {
            // The access point in use stands for its network; else the strongest.
            Some(b) if b["in_use"] != true && (entry["in_use"] == true || b["signal_pct"].as_u64() < Some(signal)) => *b = entry,
            Some(_) => {}
            None => best.push(entry),
        }
    }
    best.sort_by_key(|b| std::cmp::Reverse((b["in_use"] == true, b["signal_pct"].as_u64())));
    best
}

fn saved_connections(policy: &Policy) -> Result<Vec<Value>> {
    let text = nmcli(policy, &["-t", "-e", "yes", "-f", "NAME,UUID,TYPE,DEVICE,ACTIVE,AUTOCONNECT", "connection", "show"])?;
    Ok(parse_connections(&text))
}

pub fn parse_connections(text: &str) -> Vec<Value> {
    text.lines()
        .map(nm_fields)
        .filter(|f| f.len() >= 6)
        .map(|f| {
            json!({
                "name": f[0], "uuid": f[1], "type": f[2],
                "device": if f[3].is_empty() || f[3] == "--" { Value::Null } else { json!(f[3]) },
                "active": f[4] == "yes", "autoconnect": f[5] == "yes",
            })
        })
        .collect()
}

pub struct NetConnections;

impl Tool for NetConnections {
    fn name(&self) -> &'static str {
        "net_connections"
    }

    fn description(&self) -> &'static str {
        "List NetworkManager's devices (Ethernet, Wi-Fi, VPN, …) with their state, and the saved connections: \
         which are active, on which device, and which start automatically. Marks connections that carry an SSH \
         session into this machine."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct NoArgs {}
        let NoArgs {} = parse_args(args)?;
        let ssh = super::network::ssh_sessions();
        let devices: Vec<Value> = nmcli(policy, &["-t", "-e", "yes", "-f", "DEVICE,TYPE,STATE,CONNECTION", "device", "status"])?
            .lines()
            .map(nm_fields)
            .filter(|f| f.len() >= 4)
            .map(|f| json!({ "device": f[0], "type": f[1], "state": f[2], "connection": if f[3] == "--" { Value::Null } else { json!(f[3]) } }))
            .collect();
        let radio = nmcli(policy, &["-t", "-f", "WIFI,NETWORKING", "general"]).unwrap_or_default();
        let radio = nm_fields(radio.trim());
        let connections: Vec<Value> = saved_connections(policy)?
            .into_iter()
            .map(|mut c| {
                let dev = c["device"].as_str().unwrap_or_default().to_string();
                if let Some((_, remote)) = ssh.iter().find(|(i, _)| *i == dev) {
                    c["carries_ssh_from"] = json!(remote);
                }
                c
            })
            .collect();
        Ok(json!({ "wifi": radio.first(), "networking": radio.get(1), "devices": devices, "connections": connections }))
    }
}

pub struct WifiConnect;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WifiArgs {
    ssid: String,
    password: Option<String>,
}

fn check_wifi(a: &WifiArgs) -> Result<()> {
    ensure!(!a.ssid.is_empty() && a.ssid.len() <= 32, "a Wi-Fi name is 1 to 32 bytes");
    ensure!(!a.ssid.chars().any(char::is_control), "that isn't a Wi-Fi name");
    if let Some(p) = &a.password {
        let hex = p.len() == 64 && p.chars().all(|c| c.is_ascii_hexdigit());
        ensure!(hex || (8..=63).contains(&p.chars().count()), "a WPA password is 8 to 63 characters");
        ensure!(!p.contains('\n'), "a password can't contain a line break");
    }
    Ok(())
}

/// Wi-Fi devices with an SSH session on them.
fn wifi_devices_with_ssh(policy: &Policy) -> Vec<String> {
    let devices = nmcli(policy, &["-t", "-f", "DEVICE,TYPE", "device", "status"]).unwrap_or_default();
    devices
        .lines()
        .map(nm_fields)
        .filter(|f| f.len() >= 2 && f[1] == "wifi")
        .map(|f| f[0].clone())
        .filter(|d| ssh_guard(d).is_err())
        .collect()
}

impl Tool for WifiConnect {
    fn name(&self) -> &'static str {
        "wifi_connect"
    }

    fn description(&self) -> &'static str {
        "Join a Wi-Fi network: a saved one by name, or a new one with its password (saved for next time). Refused \
         if the Wi-Fi carries an SSH session into this machine. The password is kept out of logs and results."
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "ssid": { "type": "string", "description": "The network's name, as wifi_scan lists it." },
                "password": { "type": "string", "description": "For a network not saved yet; leave out for open or saved ones." }
            },
            "required": ["ssid"],
            "additionalProperties": false
        })
    }

    fn redact(&self, args: &Value) -> Value {
        let mut a = args.clone();
        if a.get("password").is_some() {
            a["password"] = json!("••••••");
        }
        a
    }

    fn precheck(&self, policy: &Policy, args: &Value) -> Result<()> {
        check_wifi(&parse_args(args.clone())?)?;
        if let Some(d) = wifi_devices_with_ssh(policy).first() {
            ssh_guard(d)?;
        }
        Ok(())
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: WifiArgs = parse_args(args)?;
        check_wifi(&a)?;
        if let Some(d) = wifi_devices_with_ssh(policy).first() {
            ssh_guard(d)?;
        }
        let saved = saved_connections(policy)?.into_iter().any(|c| c["type"] == "802-11-wireless" && c["name"] == a.ssid.as_str());
        let up = |extra: Vec<String>, stdin: Option<String>| -> Result<exec::Finished> {
            let mut argv: Vec<String> = ["connection", "up", "id", a.ssid.as_str()].map(String::from).to_vec();
            argv.extend(extra);
            admin(policy, "nmcli", &argv, stdin)
        };
        let done = match (&a.password, saved) {
            (None, true) => up(Vec::new(), None)?,
            (None, false) => {
                let argv: Vec<String> = ["device", "wifi", "connect", a.ssid.as_str()].map(String::from).to_vec();
                admin(policy, "nmcli", &argv, None)?
            }
            (Some(password), _) => {
                if !saved {
                    let add = ["connection", "add", "type", "wifi", "con-name", &a.ssid, "ifname", "*", "ssid", &a.ssid, "wifi-sec.key-mgmt", "wpa-psk", "wifi-sec.psk-flags", "0"];
                    nmcli(policy, &add)?;
                }
                // The secret goes in through a file only this user can read,
                // never on a command line other processes can see.
                let file = SecretFile::new(&format!("802-11-wireless-security.psk:{password}\n"))?;
                up(vec!["passwd-file".into(), file.0.display().to_string()], None)?
            }
        };
        if !done.success() {
            let why = done.complaint().replace(a.password.as_deref().unwrap_or("\u{0}"), "••••••");
            bail!("couldn't join {}: {why}", a.ssid);
        }
        let now = saved_connections(policy).unwrap_or_default().into_iter().find(|c| c["name"] == a.ssid.as_str() && c["active"] == true);
        Ok(json!({ "ssid": a.ssid, "connected": now.is_some(), "device": now.and_then(|c| c.get("device").cloned()), "saved": true }))
    }
}

/// A file readable only by this user, removed when dropped.
struct SecretFile(std::path::PathBuf);

impl SecretFile {
    fn new(content: &str) -> Result<Self> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let dir = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let path = dir.join(format!("orion-secret-{}-{}", std::process::id(), crate::chat::store::now_ms()));
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path).context("a file for the password")?;
        let file = Self(path);
        f.write_all(content.as_bytes())?;
        Ok(file)
    }
}

impl Drop for SecretFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub struct NetConnection;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConnArgs {
    name: String,
    action: ConnAction,
}

#[derive(Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
enum ConnAction {
    Up,
    Down,
    Forget,
    AutoconnectOn,
    AutoconnectOff,
}

fn check_conn(policy: &Policy, a: &ConnArgs) -> Result<Value> {
    let all = saved_connections(policy)?;
    let c = all
        .into_iter()
        .find(|c| c["name"] == a.name.as_str() || c["uuid"] == a.name.as_str())
        .with_context(|| format!("there is no saved connection {:?} (net_connections lists them)", a.name))?;
    ensure!(c["type"] != "loopback", "the loopback connection isn't Orion's to change");
    if matches!(a.action, ConnAction::Down | ConnAction::Forget) {
        if let Some(dev) = c["device"].as_str() {
            ssh_guard(dev)?;
        }
    }
    Ok(c)
}

impl Tool for NetConnection {
    fn name(&self) -> &'static str {
        "net_connection"
    }

    fn description(&self) -> &'static str {
        "Bring a saved network connection up or down, forget it, or turn starting it automatically on or off. \
         A connection that carries an SSH session into this machine can't be taken down or forgotten."
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The connection's name or UUID." },
                "action": { "type": "string", "enum": ["up", "down", "forget", "autoconnect_on", "autoconnect_off"] }
            },
            "required": ["name", "action"],
            "additionalProperties": false
        })
    }

    fn precheck(&self, policy: &Policy, args: &Value) -> Result<()> {
        check_conn(policy, &parse_args(args.clone())?).map(drop)
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: ConnArgs = parse_args(args)?;
        let c = check_conn(policy, &a)?;
        let uuid = c["uuid"].as_str().unwrap_or_default();
        let argv: Vec<&str> = match a.action {
            ConnAction::Up => vec!["connection", "up", "uuid", uuid],
            ConnAction::Down => vec!["connection", "down", "uuid", uuid],
            ConnAction::Forget => vec!["connection", "delete", "uuid", uuid],
            ConnAction::AutoconnectOn => vec!["connection", "modify", "uuid", uuid, "connection.autoconnect", "yes"],
            ConnAction::AutoconnectOff => vec!["connection", "modify", "uuid", uuid, "connection.autoconnect", "no"],
        };
        nmcli(policy, &argv)?;
        let now = saved_connections(policy).unwrap_or_default().into_iter().find(|x| x["uuid"] == uuid);
        Ok(json!({ "name": c["name"], "done": true, "now": now }))
    }
}

// ---- Firewall ----

#[derive(Clone, Copy, PartialEq, Debug)]
enum Firewall {
    Ufw,
    Firewalld,
}

fn firewall() -> Result<Firewall> {
    if exec::installed("ufw") {
        Ok(Firewall::Ufw)
    } else if exec::installed("firewall-cmd") {
        Ok(Firewall::Firewalld)
    } else {
        bail!("no firewall manager is installed (Orion manages ufw or firewalld)")
    }
}

/// `ufw status verbose`: whether it's on, the defaults, and the rules.
pub fn parse_ufw(text: &str) -> Value {
    let active = text.lines().any(|l| l.trim() == "Status: active");
    let defaults = text.lines().find_map(|l| l.strip_prefix("Default:")).map(str::trim);
    let mut rules = Vec::new();
    let mut in_table = false;
    for line in text.lines() {
        if line.starts_with("--") {
            in_table = true;
            continue;
        }
        if !in_table || line.trim().is_empty() {
            continue;
        }
        // "8080/tcp                   ALLOW IN    Anywhere                   # orion"
        let (rule, comment) = line.split_once(" # ").map_or((line, None), |(r, c)| (r, Some(c.trim())));
        let cols: Vec<&str> = rule.split("  ").map(str::trim).filter(|s| !s.is_empty()).collect();
        if cols.len() >= 3 {
            rules.push(json!({ "to": cols[0], "action": cols[1], "from": cols[2], "comment": comment }));
        }
    }
    json!({ "active": active, "defaults": defaults, "rules": rules })
}

pub struct FwStatus;

impl Tool for FwStatus {
    fn name(&self) -> &'static str {
        "fw_status"
    }

    fn description(&self) -> &'static str {
        "Show the firewall (ufw or firewalld): whether it is on, its default policy, and its rules."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct NoArgs {}
        let NoArgs {} = parse_args(args)?;
        let ssh = super::network::ssh_ports();
        Ok(match firewall()? {
            Firewall::Ufw => {
                let mut v = parse_ufw(&admin_ok(policy, "ufw", &["status", "verbose"])?);
                v["manager"] = json!("ufw");
                if v["active"] == false {
                    // Off, ufw lists no rules; these apply once it's on.
                    let added = admin_ok(policy, "ufw", &["show", "added"]).unwrap_or_default();
                    v["rules_when_on"] = added.lines().filter(|l| l.starts_with("ufw ")).map(|l| json!(l.trim_start_matches("ufw "))).collect();
                }
                v["ssh_ports"] = json!(ssh);
                v
            }
            Firewall::Firewalld => {
                let state = admin(policy, "firewall-cmd", &["--state".to_string()], None)?;
                let all = admin_ok(policy, "firewall-cmd", &["--list-all"]).unwrap_or_default();
                json!({ "manager": "firewalld", "active": state.stdout.trim() == "running", "zone": all.trim(), "ssh_ports": ssh })
            }
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleArgs {
    action: RuleAction,
    port: String,
    #[serde(default = "tcp")]
    proto: String,
    from: Option<String>,
    #[serde(default)]
    remove: bool,
}

fn tcp() -> String {
    "tcp".into()
}

#[derive(Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
enum RuleAction {
    Allow,
    Deny,
}

/// The ports a rule covers, checked: "8080" or "6000-6010".
fn ports(spec: &str) -> Result<(u16, u16)> {
    let (a, b) = spec.split_once(['-', ':']).unwrap_or((spec, spec));
    let (a, b): (u16, u16) = (a.trim().parse().context("a port is a number")?, b.trim().parse().context("a port is a number")?);
    ensure!(a >= 1 && a <= b, "{spec:?} isn't a port or range of ports");
    Ok((a, b))
}

fn check_rule(a: &RuleArgs, ssh: &[u16]) -> Result<(u16, u16)> {
    let (lo, hi) = ports(&a.port)?;
    ensure!(a.proto == "tcp" || a.proto == "udp", "proto is tcp or udp");
    if let Some(from) = &a.from {
        let (ip, prefix) = from.split_once('/').map_or((from.as_str(), None), |(i, p)| (i, Some(p)));
        ensure!(ip.parse::<std::net::IpAddr>().is_ok(), "{from:?} isn't an address or network (like 192.168.1.0/24)");
        if let Some(p) = prefix {
            ensure!(p.parse::<u8>().is_ok_and(|p| p <= 128), "{from:?} has a bad prefix length");
        }
    }
    // Blocking SSH, or removing what lets it in, locks out remote access.
    let hits_ssh = a.proto == "tcp" && ssh.iter().any(|p| (lo..=hi).contains(p));
    let blocks = (a.action == RuleAction::Deny && !a.remove) || (a.action == RuleAction::Allow && a.remove);
    if hits_ssh && blocks {
        bail!("that would block SSH (port {}); Orion won't lock out remote access", ssh.iter().find(|p| (lo..=hi).contains(p)).unwrap_or(&22));
    }
    Ok((lo, hi))
}

/// The ufw command line for a rule.
pub fn ufw_args(action: &str, proto: &str, from: Option<&str>, lo: u16, hi: u16, remove: bool) -> Vec<String> {
    let port = if lo == hi { lo.to_string() } else { format!("{lo}:{hi}") };
    let mut argv: Vec<String> = Vec::new();
    if remove {
        argv.push("delete".into());
    }
    argv.extend([action, "proto", proto, "from", from.unwrap_or("any"), "to", "any", "port", &port].map(String::from));
    if !remove {
        argv.extend(["comment".to_string(), "added by Orion".to_string()]);
    }
    argv
}

/// The firewall-cmd arguments for a rule (before `--reload`).
pub fn firewalld_args(action: &str, proto: &str, from: Option<&str>, lo: u16, hi: u16, remove: bool) -> Vec<String> {
    let port = if lo == hi { lo.to_string() } else { format!("{lo}-{hi}") };
    let verb = if remove { "remove" } else { "add" };
    match (action, from) {
        ("allow", None) => vec!["--permanent".into(), format!("--{verb}-port={port}/{proto}")],
        _ => {
            let family = if from.is_some_and(|f| f.contains(':')) { "ipv6" } else { "ipv4" };
            let source = from.map(|f| format!(" source address=\"{f}\"")).unwrap_or_default();
            let verdict = if action == "allow" { "accept" } else { "reject" };
            vec![
                "--permanent".into(),
                format!("--{verb}-rich-rule=rule family=\"{family}\"{source} port port=\"{port}\" protocol=\"{proto}\" {verdict}"),
            ]
        }
    }
}

pub struct FwRule;

impl Tool for FwRule {
    fn name(&self) -> &'static str {
        "fw_rule"
    }

    fn description(&self) -> &'static str {
        "Add or remove a firewall rule (ufw or firewalld): allow or deny incoming connections to a port or range, \
         optionally only from an address or network. Rules that would block SSH are refused."
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["allow", "deny"] },
                "port": { "type": "string", "description": "\"8080\" or a range \"6000-6010\"." },
                "proto": { "type": "string", "enum": ["tcp", "udp"], "default": "tcp" },
                "from": { "type": "string", "description": "Only from this address or network, like 192.168.1.0/24." },
                "remove": { "type": "boolean", "default": false, "description": "Remove this rule instead of adding it." }
            },
            "required": ["action", "port"],
            "additionalProperties": false
        })
    }

    fn precheck(&self, _policy: &Policy, args: &Value) -> Result<()> {
        firewall()?;
        check_rule(&parse_args(args.clone())?, &super::network::ssh_ports()).map(drop)
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: RuleArgs = parse_args(args)?;
        let (lo, hi) = check_rule(&a, &super::network::ssh_ports())?;
        let action = if a.action == RuleAction::Allow { "allow" } else { "deny" };
        let fw = firewall()?;
        match fw {
            Firewall::Ufw => {
                let argv = ufw_args(action, &a.proto, a.from.as_deref(), lo, hi, a.remove);
                let done = admin(policy, "ufw", &argv, None)?;
                ensure!(done.success(), "ufw: {}", done.complaint());
            }
            Firewall::Firewalld => {
                let argv = firewalld_args(action, &a.proto, a.from.as_deref(), lo, hi, a.remove);
                let done = admin(policy, "firewall-cmd", &argv, None)?;
                ensure!(done.success(), "firewall-cmd: {}", done.complaint());
                admin_ok(policy, "firewall-cmd", &["--reload"])?;
            }
        }
        Ok(json!({
            "done": if a.remove { "removed" } else { "added" },
            "rule": format!("{action} {}/{} from {}", a.port, a.proto, a.from.as_deref().unwrap_or("anywhere")),
            "manager": if fw == Firewall::Ufw { "ufw" } else { "firewalld" },
        }))
    }
}

/// Whether `ufw show added` has a rule letting `port` in over TCP from
/// anywhere.
pub fn ufw_allows(added: &str, port: u16) -> bool {
    let covers = |spec: &str| match spec.split_once(':') {
        Some((a, b)) => a.parse::<u16>().is_ok_and(|a| a <= port) && b.parse::<u16>().is_ok_and(|b| port <= b),
        None => spec == port.to_string(),
    };
    added.lines().any(|line| {
        let w: Vec<&str> = line.split_whitespace().collect();
        if !w.contains(&"allow") || w.contains(&"udp") {
            return false;
        }
        // Allowed only from some addresses: others would be locked out.
        if let Some(i) = w.iter().position(|x| *x == "from") {
            if w.get(i + 1) != Some(&"any") {
                return false;
            }
        }
        if port == 22 && w.contains(&"OpenSSH") {
            return true;
        }
        w.iter().any(|x| {
            let (spec, proto) = x.split_once('/').unwrap_or((x, "tcp"));
            proto == "tcp" && covers(spec)
        })
    })
}

pub struct FwEnable;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnableArgs {
    on: bool,
}

/// Turning the firewall on must leave SSH reachable: every SSH port needs
/// an allow rule first.
fn check_enable(policy: &Policy, on: bool) -> Result<()> {
    if !on {
        return Ok(());
    }
    let ssh = super::network::ssh_ports();
    let sessions = super::network::ssh_sessions();
    let sshd_seen = super::network::listening().iter().any(|l| l.process.as_deref() == Some("sshd"));
    if !sshd_seen && sessions.is_empty() {
        return Ok(());
    }
    match firewall()? {
        Firewall::Ufw => {
            // While ufw is off its status lists no rules; `show added` lists
            // the rules it will apply, as commands ("ufw allow 22/tcp").
            let added = admin_ok(policy, "ufw", &["show", "added"]).unwrap_or_default();
            for p in &ssh {
                ensure!(ufw_allows(&added, *p), "turning the firewall on would block SSH on port {p}; allow it first (fw_rule allow {p})");
            }
        }
        Firewall::Firewalld => {
            let services = admin_ok(policy, "firewall-cmd", &["--permanent", "--list-services"]).unwrap_or_default();
            let ports = admin_ok(policy, "firewall-cmd", &["--permanent", "--list-ports"]).unwrap_or_default();
            for p in &ssh {
                let allowed = (*p == 22 && services.split_whitespace().any(|s| s == "ssh")) || ports.split_whitespace().any(|x| x == format!("{p}/tcp"));
                ensure!(allowed, "turning the firewall on would block SSH on port {p}; allow it first (fw_rule allow {p})");
            }
        }
    }
    Ok(())
}

impl Tool for FwEnable {
    fn name(&self) -> &'static str {
        "fw_enable"
    }

    fn description(&self) -> &'static str {
        "Turn the firewall on or off (and on at boot). Turning it on is refused unless SSH is allowed, so remote \
         access survives."
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": { "on": { "type": "boolean" } }, "required": ["on"], "additionalProperties": false })
    }

    fn precheck(&self, policy: &Policy, args: &Value) -> Result<()> {
        let a: EnableArgs = parse_args(args.clone())?;
        check_enable(policy, a.on)
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: EnableArgs = parse_args(args)?;
        check_enable(policy, a.on)?;
        match firewall()? {
            Firewall::Ufw => drop(admin_ok(policy, "ufw", if a.on { &["--force", "enable"] } else { &["disable"] })?),
            Firewall::Firewalld => {
                let argv: &[&str] = if a.on { &["--no-ask-password", "enable", "--now", "firewalld"] } else { &["--no-ask-password", "disable", "--now", "firewalld"] };
                admin_ok(policy, "systemctl", argv)?;
            }
        }
        Ok(json!({ "firewall": if a.on { "on" } else { "off" } }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nmcli() {
        assert_eq!(nm_fields(r"*:Home 5G:AA\:BB\:CC\:DD\:EE\:FF:82"), ["*", "Home 5G", "AA:BB:CC:DD:EE:FF", "82"]);
        assert_eq!(nm_fields(r"a\\b:c"), [r"a\b", "c"]);
        let wifi = parse_wifi(concat!(
            " :Cafe:11\\:22\\:33\\:44\\:55\\:66:40:--:6:54 Mbit/s\n",
            "*:Home:AA\\:BB\\:CC\\:DD\\:EE\\:01:71:WPA2:36:540 Mbit/s\n",
            " :Home:AA\\:BB\\:CC\\:DD\\:EE\\:02:90:WPA2:1:130 Mbit/s\n",
            " :::30:WPA2:11:65 Mbit/s\n",
        ));
        assert_eq!(wifi.len(), 2, "one per name; hidden networks left out");
        assert_eq!(wifi[0]["ssid"], "Home");
        assert_eq!(wifi[0]["in_use"], true, "the one in use is kept over a stronger one");
        assert_eq!(wifi[1]["security"], "open");
        let conns = parse_connections("Home:6f1c-…:802-11-wireless:wlp3s0:yes:yes\nWired connection 1:9a2b:802-3-ethernet::no:yes\n");
        assert_eq!(conns[0]["device"], "wlp3s0");
        assert_eq!(conns[1]["device"], Value::Null);
        assert_eq!(conns[1]["active"], false);
    }

    #[test]
    fn keeps_wifi_passwords_out_of_sight() {
        let shown = WifiConnect.redact(&json!({ "ssid": "Home", "password": "correct horse" }));
        assert_eq!(shown, json!({ "ssid": "Home", "password": "••••••" }));
        assert!(check_wifi(&WifiArgs { ssid: "Home".into(), password: Some("short".into()) }).is_err());
        assert!(check_wifi(&WifiArgs { ssid: "x".repeat(33), password: None }).is_err());
        assert!(check_wifi(&WifiArgs { ssid: "Home".into(), password: Some("long enough".into()) }).is_ok());
    }

    #[test]
    fn firewall_rules_never_lock_out_ssh() {
        let rule = |action: RuleAction, port: &str, remove: bool| RuleArgs { action, port: port.into(), proto: "tcp".into(), from: None, remove };
        let ssh = [22];
        assert!(check_rule(&rule(RuleAction::Allow, "8080", false), &ssh).is_ok());
        assert!(check_rule(&rule(RuleAction::Deny, "22", false), &ssh).is_err());
        assert!(check_rule(&rule(RuleAction::Deny, "20-25", false), &ssh).is_err());
        assert!(check_rule(&rule(RuleAction::Allow, "22", true), &ssh).is_err(), "removing SSH's allow rule");
        assert!(check_rule(&rule(RuleAction::Deny, "22", true), &ssh).is_ok(), "removing a deny is fine");
        assert!(check_rule(&rule(RuleAction::Deny, "2222", false), &[2222]).is_err());
        assert!(check_rule(&rule(RuleAction::Allow, "0", false), &ssh).is_err());
        assert!(check_rule(&rule(RuleAction::Allow, "9-1", false), &ssh).is_err());
        let mut r = rule(RuleAction::Allow, "5432", false);
        r.from = Some("192.168.1.0/24".into());
        assert!(check_rule(&r, &ssh).is_ok());
        r.from = Some("anywhere; rm -rf /".into());
        assert!(check_rule(&r, &ssh).is_err());

        assert_eq!(
            ufw_args("allow", "tcp", Some("192.168.1.0/24"), 5432, 5432, false).join(" "),
            "allow proto tcp from 192.168.1.0/24 to any port 5432 comment added by Orion"
        );
        assert_eq!(ufw_args("allow", "udp", None, 6000, 6010, true).join(" "), "delete allow proto udp from any to any port 6000:6010");
        assert_eq!(firewalld_args("allow", "tcp", None, 8080, 8080, false), ["--permanent", "--add-port=8080/tcp"]);
        assert_eq!(
            firewalld_args("deny", "tcp", Some("10.0.0.5"), 3306, 3306, false)[1],
            "--add-rich-rule=rule family=\"ipv4\" source address=\"10.0.0.5\" port port=\"3306\" protocol=\"tcp\" reject"
        );

        assert!(ufw_allows("Added user rules (see 'ufw status' for running firewall):\nufw allow 22/tcp\nufw allow 8080\n", 22));
        assert!(ufw_allows("ufw allow OpenSSH\n", 22));
        assert!(ufw_allows("ufw allow proto tcp from any to any port 22 comment 'added by Orion'\n", 22));
        assert!(!ufw_allows("ufw allow proto tcp from 10.0.0.0/8 to any port 22\n", 22), "only from one network");
        assert!(!ufw_allows("ufw allow 22/udp\nufw allow 2222/tcp\n", 22));
        assert!(ufw_allows("ufw allow proto tcp from any to any port 20:30\n", 22));
        let status = parse_ufw("Status: active\nLogging: on (low)\nDefault: deny (incoming), allow (outgoing), disabled (routed)\nNew profiles: skip\n\nTo                         Action      From\n--                         ------      ----\n22/tcp                     ALLOW IN    Anywhere\n8080/tcp                   ALLOW IN    192.168.1.0/24             # added by Orion\n");
        assert_eq!(status["active"], true);
        assert_eq!(status["rules"][1], json!({ "to": "8080/tcp", "action": "ALLOW IN", "from": "192.168.1.0/24", "comment": "added by Orion" }));
    }
}
