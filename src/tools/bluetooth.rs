//! Bluetooth, through BlueZ's `bluetoothctl`: the adapter, the devices it
//! knows (paired, connected, their battery), scanning for new ones, and
//! pairing, connecting, disconnecting and forgetting them.

use std::time::Duration;

use anyhow::{bail, ensure, Result};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use super::exec;
use crate::policy::Policy;
use crate::tool::{parse_args, Preapproval, Risk, Tool};

/// `bluetoothctl ARGS…`, failing on what it reports as a failure (it often
/// exits 0 regardless).
fn ctl(args: &[&str], secs: u64) -> Result<String> {
    ensure!(exec::installed("bluetoothctl"), "bluetoothctl isn't installed (bluez, or bluez-utils on Arch)");
    let mut cmd = std::process::Command::new("bluetoothctl");
    cmd.args(args).env("LC_ALL", "C");
    let done = exec::run(cmd, None, Duration::from_secs(secs), 1 << 20)?;
    let all = format!("{}\n{}", done.stdout, done.stderr);
    if done.timed_out && all.contains("Waiting to connect to bluetoothd") {
        bail!("the Bluetooth service isn't running (systemctl start bluetooth)");
    }
    if all.contains("connection != NULL") || all.contains("Failed to connect to the bus") {
        bail!("Bluetooth needs the system D-Bus, which isn't running here");
    }
    if all.contains("No default controller available") {
        bail!("this machine has no Bluetooth adapter (or it is switched off in the firmware or by rfkill)");
    }
    if let Some(line) = all.lines().find(|l| l.contains("Failed to") || l.contains("not available") || l.starts_with("org.bluez.Error")) {
        bail!("{}", line.trim());
    }
    ensure!(done.success() || done.timed_out, "bluetoothctl: {}", done.complaint());
    Ok(done.stdout)
}

/// The `Key: Value` lines under `Controller …` or `Device …`.
fn parse_fields(text: &str) -> Map<String, Value> {
    let mut out = Map::new();
    for line in text.lines() {
        let Some((k, v)) = line.trim().split_once(": ") else { continue };
        if k.contains(' ') && k != "Battery Percentage" {
            continue;
        }
        if k == "UUID" {
            continue;
        }
        out.entry(k.to_string()).or_insert_with(|| json!(v.trim()));
    }
    out
}

fn yes(f: &Map<String, Value>, key: &str) -> bool {
    f.get(key).and_then(Value::as_str) == Some("yes")
}

fn adapter(text: &str) -> Value {
    let f = parse_fields(text);
    let address = text.lines().find_map(|l| l.strip_prefix("Controller ")).and_then(|r| r.split_whitespace().next());
    json!({
        "address": address,
        "name": f.get("Alias").or(f.get("Name")),
        "powered": yes(&f, "Powered"),
        "blocked": f.get("PowerState").and_then(Value::as_str).is_some_and(|s| s.contains("blocked")),
        "discoverable": yes(&f, "Discoverable"),
        "pairable": yes(&f, "Pairable"),
        "scanning": yes(&f, "Discovering"),
    })
}

/// `Device AA:BB:CC:DD:EE:FF Name` lines.
fn parse_devices(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("Device "))
        .filter_map(|r| {
            let (addr, name) = r.split_once(' ').unwrap_or((r, ""));
            valid_address(addr).then(|| (addr.to_string(), name.trim().to_string()))
        })
        .collect()
}

fn device(address: &str, name: &str, info: &str) -> Value {
    let f = parse_fields(info);
    // "Battery Percentage: 0x46 (70)"
    let battery = f.get("Battery Percentage").and_then(Value::as_str).and_then(|b| b.split_once('(')).and_then(|(_, n)| n.trim_end_matches(')').parse::<u64>().ok());
    json!({
        "address": address,
        "name": f.get("Alias").or(f.get("Name")).and_then(Value::as_str).unwrap_or(name),
        "kind": f.get("Icon"),
        "paired": yes(&f, "Paired"),
        "trusted": yes(&f, "Trusted"),
        "connected": yes(&f, "Connected"),
        "blocked": yes(&f, "Blocked"),
        "battery": battery,
    })
}

fn valid_address(a: &str) -> bool {
    a.len() == 17 && a.split(':').count() == 6 && a.split(':').all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()))
}

fn devices() -> Result<Vec<Value>> {
    let mut list: Vec<Value> = parse_devices(&ctl(&["devices"], 10)?)
        .into_iter()
        .take(40)
        .map(|(addr, name)| device(&addr, &name, &ctl(&["info", &addr], 10).unwrap_or_default()))
        .collect();
    // Connected first, then paired, then the rest.
    list.sort_by_key(|d| (!d["connected"].as_bool().unwrap_or(false), !d["paired"].as_bool().unwrap_or(false)));
    Ok(list)
}

pub struct BtStatus;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusArgs {
    #[serde(default)]
    scan_secs: u64,
}

impl Tool for BtStatus {
    fn name(&self) -> &'static str {
        "bt_status"
    }

    fn description(&self) -> &'static str {
        "Bluetooth: whether the adapter is on, and the devices it knows (headphones, keyboards, phones), which \
         are paired and connected, and their battery. With scan_secs it first looks for devices nearby that \
         are ready to pair."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "scan_secs": { "type": "integer", "minimum": 0, "maximum": 30, "default": 0,
                               "description": "Scan this long for new devices first (put them in pairing mode)." }
            },
            "additionalProperties": false
        })
    }

    fn call(&self, _policy: &Policy, args: Value) -> Result<Value> {
        let a: StatusArgs = parse_args(args)?;
        ensure!(a.scan_secs <= 30, "scan for at most 30 seconds");
        let adapter = adapter(&ctl(&["show"], 10)?);
        if a.scan_secs > 0 {
            ensure!(adapter["powered"] == true, "the Bluetooth adapter is off; bt_device with power_on turns it on");
            let secs = a.scan_secs.to_string();
            ctl(&["--timeout", &secs, "scan", "on"], a.scan_secs + 10)?;
        }
        Ok(json!({ "adapter": adapter, "devices": devices()? }))
    }
}

pub struct BtDevice;

#[derive(Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Action {
    PowerOn,
    PowerOff,
    Pair,
    Connect,
    Disconnect,
    Remove,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeviceArgs {
    action: Action,
    address: Option<String>,
}

fn check(a: &DeviceArgs) -> Result<Option<&str>> {
    match (a.action, a.address.as_deref()) {
        (Action::PowerOn | Action::PowerOff, _) => Ok(None),
        (_, Some(addr)) if valid_address(addr) => Ok(Some(addr)),
        (_, Some(addr)) => bail!("{addr:?} isn't a Bluetooth address (like 00:1A:7D:DA:71:13); bt_status lists them"),
        (_, None) => bail!("name the device by its address (bt_status lists them)"),
    }
}

impl Tool for BtDevice {
    fn name(&self) -> &'static str {
        "bt_device"
    }

    fn description(&self) -> &'static str {
        "Bluetooth changes: turn the adapter on or off, pair a device found by a scan (then trust and connect \
         it), connect or disconnect a paired device, or remove (forget) one. Devices are named by address, \
         from bt_status."
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["power_on", "power_off", "pair", "connect", "disconnect", "remove"] },
                "address": { "type": "string", "description": "The device, like 00:1A:7D:DA:71:13 (not for power)." }
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }

    fn preapproval(&self, policy: &Policy, args: &Value) -> Preapproval {
        // Connecting what is already paired is everyday; pairing and forgetting are not.
        match parse_args::<DeviceArgs>(args.clone()).map(|a| a.action) {
            Ok(Action::Connect | Action::Disconnect | Action::PowerOn | Action::PowerOff) => policy.desktop.preapproval("bluetooth"),
            _ => Preapproval::NotApplicable,
        }
    }

    fn precheck(&self, _policy: &Policy, args: &Value) -> Result<()> {
        check(&parse_args(args.clone())?).map(drop)
    }

    fn call(&self, _policy: &Policy, args: Value) -> Result<Value> {
        let a: DeviceArgs = parse_args(args)?;
        let address = check(&a)?;
        match (a.action, address) {
            (Action::PowerOn, _) => drop(ctl(&["power", "on"], 15)?),
            (Action::PowerOff, _) => drop(ctl(&["power", "off"], 15)?),
            (Action::Pair, Some(addr)) => {
                ctl(&["--agent", "NoInputNoOutput", "--timeout", "30", "pair", addr], 40)?;
                ctl(&["trust", addr], 10)?;
                ctl(&["--timeout", "20", "connect", addr], 30)?;
            }
            (Action::Connect, Some(addr)) => drop(ctl(&["--timeout", "20", "connect", addr], 30)?),
            (Action::Disconnect, Some(addr)) => drop(ctl(&["disconnect", addr], 15)?),
            (Action::Remove, Some(addr)) => drop(ctl(&["remove", addr], 15)?),
            _ => unreachable!("checked"),
        }
        let adapter = adapter(&ctl(&["show"], 10)?);
        let dev = match (a.action, address) {
            (Action::Remove, _) | (_, None) => Value::Null,
            (_, Some(addr)) => device(addr, "", &ctl(&["info", addr], 10).unwrap_or_default()),
        };
        Ok(json!({ "ok": true, "adapter": adapter, "device": dev }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_bluetoothctl() {
        let show = "Controller 00:1A:7D:DA:71:13 (public)\n\tManufacturer: 0x000a (10)\n\tName: orion-pc\n\tAlias: orion-pc\n\
                    \tClass: 0x006c010c (7078156)\n\tPowered: yes\n\tPowerState: on\n\tDiscoverable: no\n\tPairable: yes\n\
                    \tUUID: Audio Sink                (0000110b-0000-1000-8000-00805f9b34fb)\n\tDiscovering: no\n";
        let a = adapter(show);
        assert_eq!((a["address"].as_str(), a["name"].as_str(), a["powered"].as_bool()), (Some("00:1A:7D:DA:71:13"), Some("orion-pc"), Some(true)));
        assert_eq!(adapter("Controller 00:1A:7D:DA:71:13 (public)\n\tPowered: no\n\tPowerState: off-blocked\n")["blocked"], true);

        let list = parse_devices("Device 38:18:4C:12:34:56 WH-1000XM4\nDevice 40:CB:C0:AA:BB:CC Magic Keyboard\n[CHG] Controller x\nDevice nope x\n");
        assert_eq!(list, [("38:18:4C:12:34:56".to_string(), "WH-1000XM4".to_string()), ("40:CB:C0:AA:BB:CC".into(), "Magic Keyboard".into())]);
        let info = "Device 38:18:4C:12:34:56 (public)\n\tName: WH-1000XM4\n\tAlias: WH-1000XM4\n\tClass: 0x00240404 (2360324)\n\
                    \tIcon: audio-headset\n\tPaired: yes\n\tBonded: yes\n\tTrusted: yes\n\tBlocked: no\n\tConnected: yes\n\
                    \tLegacyPairing: no\n\tUUID: Headset                   (00001108-0000-1000-8000-00805f9b34fb)\n\
                    \tBattery Percentage: 0x46 (70)\n";
        let d = device("38:18:4C:12:34:56", "", info);
        assert_eq!((d["name"].as_str(), d["kind"].as_str(), d["connected"].as_bool(), d["battery"].as_u64()), (Some("WH-1000XM4"), Some("audio-headset"), Some(true), Some(70)));

        let args = |v: Value| serde_json::from_value::<DeviceArgs>(v).unwrap();
        assert!(check(&args(json!({ "action": "connect" }))).is_err());
        assert!(check(&args(json!({ "action": "connect", "address": "38:18:4C:12:34:56; rm" }))).is_err());
        assert_eq!(check(&args(json!({ "action": "pair", "address": "38:18:4C:12:34:56" }))).unwrap(), Some("38:18:4C:12:34:56"));
        assert_eq!(check(&args(json!({ "action": "power_on" }))).unwrap(), None);
    }
}
