//! Screens: the monitors (resolution, refresh rate, scale, position, on or
//! off) and the built-in panel's brightness.
//!
//! Monitors are read and set through what the session offers: `wlr-randr`
//! on wlroots compositors (sway, Hyprland, river, cage…), `kscreen-doctor`
//! on KDE Plasma, `xrandr` on X11. GNOME on Wayland keeps its screens to
//! its own settings, so there only brightness works. Brightness goes
//! through `brightnessctl` when installed, else logind (which lets the
//! person at the machine set it), else the backlight file if writable.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{exec, session};
use crate::policy::Policy;
use crate::tool::{parse_args, Preapproval, Risk, Tool};

#[derive(Debug, Clone, PartialEq)]
struct Mode {
    width: u32,
    height: u32,
    refresh: Option<f64>,
    current: bool,
    preferred: bool,
}

impl Mode {
    fn label(&self) -> String {
        match self.refresh {
            Some(r) => format!("{}x{}@{}", self.width, self.height, (r * 100.0).round() / 100.0),
            None => format!("{}x{}", self.width, self.height),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
struct Output {
    name: String,
    description: String,
    connected: bool,
    enabled: bool,
    primary: bool,
    modes: Vec<Mode>,
    scale: Option<f64>,
    x: i64,
    y: i64,
}

impl Output {
    fn current(&self) -> Option<&Mode> {
        self.modes.iter().find(|m| m.current)
    }

    fn json(&self) -> Value {
        let mut labels: Vec<String> = Vec::new();
        for m in &self.modes {
            if !labels.contains(&m.label()) {
                labels.push(m.label());
            }
        }
        json!({
            "name": self.name, "description": self.description, "connected": self.connected, "enabled": self.enabled,
            "primary": self.primary, "mode": self.current().map(Mode::label), "scale": self.scale,
            "position": { "x": self.x, "y": self.y }, "modes": labels.into_iter().take(30).collect::<Vec<_>>(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Backend {
    Wlr,
    Kscreen,
    Xrandr,
}

impl Backend {
    fn name(self) -> &'static str {
        match self {
            Backend::Wlr => "wlr-randr",
            Backend::Kscreen => "kscreen-doctor",
            Backend::Xrandr => "xrandr",
        }
    }
}

fn backend(env: &BTreeMap<String, String>) -> Result<Backend> {
    let desktop = env.get("XDG_CURRENT_DESKTOP").map(|d| d.to_lowercase()).unwrap_or_default();
    if env.contains_key("WAYLAND_DISPLAY") {
        if desktop.contains("kde") {
            ensure!(exec::installed("kscreen-doctor"), "on KDE Plasma, screens are set with kscreen-doctor, which isn't installed (libkscreen)");
            return Ok(Backend::Kscreen);
        }
        if desktop.contains("gnome") {
            bail!("GNOME keeps its screens to Settings → Displays; Orion can't change them there yet (brightness works)");
        }
        ensure!(exec::installed("wlr-randr"), "this Wayland session's screens are set with wlr-randr, which isn't installed");
        return Ok(Backend::Wlr);
    }
    if env.contains_key("DISPLAY") {
        ensure!(exec::installed("xrandr"), "xrandr isn't installed (x11-xserver-utils, or xorg-xrandr on Arch)");
        return Ok(Backend::Xrandr);
    }
    bail!("no graphical session is running for this user")
}

/// `wlr-randr`: `NAME "description"`, then indented properties and modes
/// (`1920x1080 px, 60.000000 Hz (preferred, current)`).
fn parse_wlr(text: &str) -> Vec<Output> {
    let mut outputs: Vec<Output> = Vec::new();
    let mut in_modes = false;
    for line in text.lines() {
        if !line.starts_with(' ') && !line.trim().is_empty() {
            let (name, desc) = line.split_once(' ').unwrap_or((line, ""));
            outputs.push(Output { name: name.into(), description: desc.trim().trim_matches('"').into(), connected: true, ..Default::default() });
            in_modes = false;
            continue;
        }
        let Some(o) = outputs.last_mut() else { continue };
        let t = line.trim();
        if t == "Modes:" {
            in_modes = true;
            continue;
        }
        if in_modes && line.starts_with("    ") {
            let (size, rest) = t.split_once(" px").unwrap_or((t, ""));
            let Some((w, h)) = size.split_once('x') else { continue };
            let refresh = rest.trim_start_matches(',').split_whitespace().next().and_then(|r| r.parse::<f64>().ok());
            if let (Ok(width), Ok(height)) = (w.parse(), h.parse()) {
                o.modes.push(Mode { width, height, refresh, current: rest.contains("current"), preferred: rest.contains("preferred") });
            }
            continue;
        }
        in_modes = false;
        match t.split_once(": ") {
            Some(("Enabled", v)) => o.enabled = v == "yes",
            Some(("Scale", v)) => o.scale = v.parse().ok(),
            Some(("Position", v)) => {
                let (x, y) = v.split_once(',').unwrap_or(("0", "0"));
                (o.x, o.y) = (x.parse().unwrap_or(0), y.parse().unwrap_or(0));
            }
            Some(("Model", v)) if o.description.is_empty() => o.description = v.into(),
            _ => {}
        }
    }
    outputs
}

/// `xrandr --query`: `NAME connected [primary] WxH+X+Y …`, then mode lines
/// (`   1920x1080     60.01*+  59.97`): `*` current, `+` preferred.
fn parse_xrandr(text: &str) -> Vec<Output> {
    let mut outputs: Vec<Output> = Vec::new();
    for line in text.lines() {
        if line.starts_with("Screen ") {
            continue;
        }
        if !line.starts_with(' ') {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 2 {
                continue;
            }
            let primary = f.get(2) == Some(&"primary");
            let geometry = f.iter().skip(2).find(|g| g.contains('x') && g.contains('+'));
            let (x, y) = geometry
                .and_then(|g| {
                    let mut p = g.split('+').skip(1);
                    Some((p.next()?.parse().ok()?, p.next()?.parse().ok()?))
                })
                .unwrap_or((0, 0));
            outputs.push(Output {
                name: f[0].into(),
                connected: f[1] == "connected",
                enabled: geometry.is_some(),
                primary,
                x,
                y,
                ..Default::default()
            });
            continue;
        }
        let Some(o) = outputs.last_mut() else { continue };
        let mut f = line.split_whitespace();
        let Some((w, h)) = f.next().and_then(|s| s.split_once('x')) else { continue };
        let (Ok(width), Ok(height)) = (w.parse::<u32>(), h.trim_end_matches('i').parse::<u32>()) else { continue };
        for rate in f {
            // A lone "+" or "*" marks the rate before it.
            if rate.chars().all(|c| c == '*' || c == '+') {
                if let Some(m) = o.modes.last_mut() {
                    m.current |= rate.contains('*');
                    m.preferred |= rate.contains('+');
                }
                continue;
            }
            let r: f64 = match rate.trim_end_matches(['*', '+']).parse() {
                Ok(r) => r,
                Err(_) => continue,
            };
            o.modes.push(Mode { width, height, refresh: (r > 0.0).then_some(r), current: rate.contains('*'), preferred: rate.contains('+') });
        }
    }
    outputs
}

/// `kscreen-doctor --json`.
fn parse_kscreen(text: &str) -> Result<Vec<Output>> {
    let v: Value = serde_json::from_str(text).context("kscreen-doctor's JSON")?;
    let list = v["outputs"].as_array().context("no outputs in kscreen-doctor's JSON")?;
    Ok(list
        .iter()
        .map(|o| {
            let current = o["currentModeId"].as_str().map(String::from).or(o["currentModeId"].as_u64().map(|n| n.to_string()));
            let preferred: Vec<String> = o["preferredModes"].as_array().map(|p| p.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
            let modes = o["modes"]
                .as_array()
                .map(|ms| {
                    ms.iter()
                        .filter_map(|m| {
                            let id = m["id"].as_str().map(String::from).or(m["id"].as_u64().map(|n| n.to_string()))?;
                            Some(Mode {
                                width: m["size"]["width"].as_u64()? as u32,
                                height: m["size"]["height"].as_u64()? as u32,
                                refresh: m["refreshRate"].as_f64(),
                                current: current.as_deref() == Some(id.as_str()),
                                preferred: preferred.contains(&id),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Output {
                name: o["name"].as_str().unwrap_or_default().into(),
                description: o["type"].as_str().unwrap_or_default().into(),
                connected: o["connected"].as_bool().unwrap_or(false),
                enabled: o["enabled"].as_bool().unwrap_or(false),
                primary: o["priority"].as_u64() == Some(1),
                modes,
                scale: o["scale"].as_f64(),
                x: o["pos"]["x"].as_i64().unwrap_or(0),
                y: o["pos"]["y"].as_i64().unwrap_or(0),
            }
        })
        .collect())
}

fn run(env: &BTreeMap<String, String>, program: &str, args: &[String]) -> Result<String> {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args).envs(env).env("LC_ALL", "C");
    let done = exec::run(cmd, None, Duration::from_secs(15), 1 << 20)?;
    ensure!(done.success(), "{program}: {}", done.complaint());
    Ok(done.stdout)
}

fn outputs(env: &BTreeMap<String, String>, b: Backend) -> Result<Vec<Output>> {
    Ok(match b {
        Backend::Wlr => parse_wlr(&run(env, "wlr-randr", &[])?),
        Backend::Kscreen => parse_kscreen(&run(env, "kscreen-doctor", &["--json".into()])?)?,
        Backend::Xrandr => parse_xrandr(&run(env, "xrandr", &["--query".into()])?),
    })
}

/// A backlight: its name, brightness and maximum.
#[derive(Debug, PartialEq)]
struct Backlight {
    name: String,
    dir: PathBuf,
    brightness: u64,
    max: u64,
}

impl Backlight {
    fn percent(&self) -> u64 {
        (self.brightness * 100 + self.max / 2).checked_div(self.max).unwrap_or(0)
    }
}

/// The panel's backlight under `root` (/sys/class/backlight): firmware
/// interfaces before platform ones before raw, as the kernel advises.
fn backlight(root: &Path) -> Option<Backlight> {
    let rank = |t: &str| match t {
        "firmware" => 0,
        "platform" => 1,
        _ => 2,
    };
    let mut found: Vec<(u8, Backlight)> = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let dir = e.path();
            let read = |f: &str| std::fs::read_to_string(dir.join(f)).ok().map(|s| s.trim().to_string());
            let max: u64 = read("max_brightness")?.parse().ok()?;
            let brightness: u64 = read("brightness")?.parse().ok()?;
            let t = read("type").unwrap_or_default();
            Some((rank(&t), Backlight { name: e.file_name().to_string_lossy().into_owned(), dir, brightness, max }))
        })
        .filter(|(_, b)| b.max > 0)
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.name.cmp(&b.1.name)));
    found.into_iter().next().map(|(_, b)| b)
}

/// The dimmest Orion sets: some panels go black at 0.
const MIN_BRIGHTNESS: i64 = 5;

fn set_brightness(b: &Backlight, percent: i64) -> Result<&'static str> {
    let value = (b.max as i64 * percent.clamp(MIN_BRIGHTNESS, 100) / 100).max(1) as u64;
    if exec::installed("brightnessctl") {
        exec::output("brightnessctl", &["--quiet", "--device", &b.name, "set", &value.to_string()])?;
        return Ok("brightnessctl");
    }
    if let Some(id) = session::login_session() {
        let path = session::session_object(&id);
        let done = exec::output(
            "busctl",
            &["--system", "call", "org.freedesktop.login1", &path, "org.freedesktop.login1.Session", "SetBrightness", "ssu", "backlight", &b.name, &value.to_string()],
        );
        if done.is_ok() {
            return Ok("logind");
        }
    }
    std::fs::write(b.dir.join("brightness"), value.to_string())
        .with_context(|| format!("can't set {}'s brightness: install brightnessctl, or log in at the machine", b.name))?;
    Ok("sysfs")
}

pub struct DisplayStatus;

impl Tool for DisplayStatus {
    fn name(&self) -> &'static str {
        "display_status"
    }

    fn description(&self) -> &'static str {
        "The screens: each monitor's resolution and refresh rate, scale, position, whether it is on, and the \
         modes it supports; and the built-in panel's brightness."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    fn call(&self, _policy: &Policy, _args: Value) -> Result<Value> {
        let env = session::env();
        let light = backlight(Path::new("/sys/class/backlight"));
        let mut out = json!({
            "brightness": light.as_ref().map(|b| json!({ "device": b.name, "percent": b.percent() })),
        });
        match backend(&env).and_then(|b| Ok((b, outputs(&env, b)?))) {
            Ok((b, list)) => {
                out["backend"] = json!(b.name());
                out["outputs"] = json!(list.iter().map(Output::json).collect::<Vec<_>>());
            }
            Err(e) => out["outputs_error"] = json!(format!("{e:#}")),
        }
        Ok(out)
    }
}

pub struct DisplaySet;

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct SetArgs {
    output: Option<String>,
    enabled: Option<bool>,
    mode: Option<String>,
    scale: Option<f64>,
    x: Option<i64>,
    y: Option<i64>,
    brightness: Option<i64>,
    change_brightness: Option<i64>,
}

impl SetArgs {
    fn touches_outputs(&self) -> bool {
        self.enabled.is_some() || self.mode.is_some() || self.scale.is_some() || self.x.is_some() || self.y.is_some()
    }
}

/// `1920x1080` or `1920x1080@60`.
fn parse_mode(m: &str) -> Result<(u32, u32, Option<f64>)> {
    let (size, rate) = m.split_once('@').map(|(s, r)| (s, Some(r))).unwrap_or((m, None));
    let (w, h) = size.split_once('x').with_context(|| format!("{m:?} isn't a mode like 1920x1080 or 1920x1080@60"))?;
    let rate = rate.map(|r| r.trim_end_matches("Hz").parse::<f64>()).transpose().with_context(|| format!("{m:?}: the refresh rate isn't a number"))?;
    Ok((w.parse().context("the width isn't a number")?, h.parse().context("the height isn't a number")?, rate))
}

/// What the change will do, checked against the screens as they are.
struct Plan {
    env: BTreeMap<String, String>,
    backend: Backend,
    output: Output,
    mode: Option<Mode>,
}

fn plan(a: &SetArgs) -> Result<Option<Plan>> {
    if let Some(b) = a.brightness {
        ensure!((MIN_BRIGHTNESS..=100).contains(&b), "brightness goes from {MIN_BRIGHTNESS} to 100 (percent); lower can turn the panel black");
    }
    ensure!(a.brightness.is_none() || a.change_brightness.is_none(), "give brightness or change_brightness, not both");
    if let Some(s) = a.scale {
        ensure!((0.5..=4.0).contains(&s), "scale goes from 0.5 to 4");
    }
    ensure!(a.x.is_some() == a.y.is_some(), "give both x and y for a position");
    ensure!(a.touches_outputs() || a.brightness.is_some() || a.change_brightness.is_some(), "say what to change");
    if !a.touches_outputs() {
        return Ok(None);
    }
    let env = session::env();
    let backend = backend(&env)?;
    let list = outputs(&env, backend)?;
    let name = match &a.output {
        Some(n) => n.clone(),
        None if list.iter().filter(|o| o.connected).count() == 1 => list.iter().find(|o| o.connected).map(|o| o.name.clone()).unwrap_or_default(),
        None => bail!("name the output: {}", list.iter().filter(|o| o.connected).map(|o| o.name.as_str()).collect::<Vec<_>>().join(", ")),
    };
    let output = list.iter().find(|o| o.name == name).cloned().with_context(|| {
        format!("there is no output {name:?}; there are: {}", list.iter().map(|o| o.name.as_str()).collect::<Vec<_>>().join(", "))
    })?;
    ensure!(output.connected, "{name} has no monitor connected");
    if a.enabled == Some(false) {
        let others = list.iter().filter(|o| o.enabled && o.name != name).count();
        ensure!(others > 0, "{name} is the only screen on; turning it off would leave nothing to see");
    }
    ensure!(a.scale.is_none() || backend != Backend::Xrandr, "X11 has no per-screen scale; set it in the desktop's settings");
    let mode = match &a.mode {
        None => None,
        Some(m) => {
            let (w, h, rate) = parse_mode(m)?;
            let fits = output.modes.iter().filter(|x| x.width == w && x.height == h && rate.is_none_or(|r| x.refresh.is_some_and(|f| (f - r).abs() < 0.5)));
            // The given rate, or the preferred one, or the highest.
            let best = fits.max_by(|p, q| (p.preferred, p.refresh.unwrap_or(0.0)).partial_cmp(&(q.preferred, q.refresh.unwrap_or(0.0))).unwrap_or(std::cmp::Ordering::Equal));
            let labels: Vec<String> = output.modes.iter().map(Mode::label).collect();
            Some(best.cloned().with_context(|| format!("{name} has no mode {m}; it has: {}", labels.join(", ")))?)
        }
    };
    Ok(Some(Plan { env, backend, output, mode }))
}

fn args_for(p: &Plan, a: &SetArgs) -> (&'static str, Vec<String>) {
    let name = &p.output.name;
    match p.backend {
        Backend::Wlr => {
            let mut v = vec!["--output".to_string(), name.clone()];
            match a.enabled {
                Some(true) => v.push("--on".into()),
                Some(false) => v.push("--off".into()),
                None => {}
            }
            if let Some(m) = &p.mode {
                v.push("--mode".into());
                v.push(match m.refresh {
                    Some(r) => format!("{}x{}@{r}Hz", m.width, m.height),
                    None => format!("{}x{}", m.width, m.height),
                });
            }
            if let Some(s) = a.scale {
                v.extend(["--scale".into(), s.to_string()]);
            }
            if let (Some(x), Some(y)) = (a.x, a.y) {
                v.extend(["--pos".into(), format!("{x},{y}")]);
            }
            ("wlr-randr", v)
        }
        Backend::Xrandr => {
            let mut v = vec!["--output".to_string(), name.clone()];
            match a.enabled {
                Some(true) if p.mode.is_none() => v.push("--auto".into()),
                Some(false) => v.push("--off".into()),
                _ => {}
            }
            if let Some(m) = &p.mode {
                v.extend(["--mode".into(), format!("{}x{}", m.width, m.height)]);
                if let Some(r) = m.refresh {
                    v.extend(["--rate".into(), r.to_string()]);
                }
            }
            if let (Some(x), Some(y)) = (a.x, a.y) {
                v.extend(["--pos".into(), format!("{x}x{y}")]);
            }
            ("xrandr", v)
        }
        Backend::Kscreen => {
            let mut v = Vec::new();
            match a.enabled {
                Some(true) => v.push(format!("output.{name}.enable")),
                Some(false) => v.push(format!("output.{name}.disable")),
                None => {}
            }
            if let Some(m) = &p.mode {
                v.push(format!("output.{name}.mode.{}", m.label()));
            }
            if let Some(s) = a.scale {
                v.push(format!("output.{name}.scale.{s}"));
            }
            if let (Some(x), Some(y)) = (a.x, a.y) {
                v.push(format!("output.{name}.position.{x},{y}"));
            }
            ("kscreen-doctor", v)
        }
    }
}

impl Tool for DisplaySet {
    fn name(&self) -> &'static str {
        "display_set"
    }

    fn description(&self) -> &'static str {
        "Change the screens: the built-in panel's brightness (percent, or up and down by some), or a monitor's \
         resolution and refresh rate (a mode from display_status), scale, position, or whether it is on. \
         The last screen that is on is never turned off."
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "output": { "type": "string", "description": "The monitor, by name from display_status. Default: the only one connected." },
                "enabled": { "type": "boolean", "description": "Turn the monitor on or off." },
                "mode": { "type": "string", "description": "Resolution, optionally with refresh rate: 2560x1440 or 2560x1440@144." },
                "scale": { "type": "number", "minimum": 0.5, "maximum": 4, "description": "1.5 makes everything half as big again (Wayland only)." },
                "x": { "type": "integer", "description": "Position of the monitor's left edge, with y." },
                "y": { "type": "integer" },
                "brightness": { "type": "integer", "minimum": MIN_BRIGHTNESS, "maximum": 100, "description": "The built-in panel's brightness, in percent." },
                "change_brightness": { "type": "integer", "minimum": -100, "maximum": 100, "description": "Brighter (or dimmer, negative) by this many percent." }
            },
            "additionalProperties": false
        })
    }

    fn preapproval(&self, policy: &Policy, args: &Value) -> Preapproval {
        match parse_args::<SetArgs>(args.clone()) {
            Ok(a) if a.touches_outputs() => policy.desktop.preapproval("displays"),
            Ok(_) => policy.desktop.preapproval("brightness"),
            Err(e) => Preapproval::Refused(format!("{e:#}")),
        }
    }

    fn precheck(&self, _policy: &Policy, args: &Value) -> Result<()> {
        plan(&parse_args(args.clone())?).map(drop)
    }

    fn call(&self, _policy: &Policy, args: Value) -> Result<Value> {
        let a: SetArgs = parse_args(args)?;
        let p = plan(&a)?;
        let mut out = json!({ "ok": true });
        if a.brightness.is_some() || a.change_brightness.is_some() {
            let b = backlight(Path::new("/sys/class/backlight")).context("this machine has no panel whose brightness Orion can set (external monitors need their own buttons)")?;
            let target = a.brightness.unwrap_or(b.percent() as i64 + a.change_brightness.unwrap_or(0));
            let how = set_brightness(&b, target)?;
            let now = backlight(Path::new("/sys/class/backlight")).map(|b| b.percent());
            out["brightness"] = json!({ "device": b.name, "percent": now, "via": how });
        }
        if let Some(p) = p {
            let (program, argv) = args_for(&p, &a);
            run(&p.env, program, &argv)?;
            let now = outputs(&p.env, p.backend)?;
            out["output"] = now.iter().find(|o| o.name == p.output.name).map(Output::json).unwrap_or(Value::Null);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const XRANDR: &str = "Screen 0: minimum 320 x 200, current 4480 x 1440, maximum 16384 x 16384\n\
        eDP-1 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 344mm x 194mm\n\
        \x20  1920x1080     60.01*+  59.97    59.96    48.00\n\
        \x20  1680x1050     59.95    59.88\n\
        HDMI-1 disconnected (normal left inverted right x axis y axis)\n\
        DP-1 connected 2560x1440+1920+0 (normal left inverted right x axis y axis) 597mm x 336mm\n\
        \x20  2560x1440    143.97 +  59.95*\n\
        \x20  1920x1080i    60.00\n\
        DP-2 connected (normal left inverted right x axis y axis)\n\
        \x20  1920x1080     60.00 +\n";

    #[test]
    fn reads_xrandr() {
        let o = parse_xrandr(XRANDR);
        assert_eq!(o.iter().map(|x| (x.name.as_str(), x.connected, x.enabled)).collect::<Vec<_>>(),
                   [("eDP-1", true, true), ("HDMI-1", false, false), ("DP-1", true, true), ("DP-2", true, false)]);
        assert!(o[0].primary && !o[2].primary);
        assert_eq!((o[2].x, o[2].current().map(Mode::label)), (1920, Some("2560x1440@59.95".into())));
        assert!(o[2].modes.iter().any(|m| m.preferred && m.refresh == Some(143.97)));
        assert_eq!(o[0].modes.len(), 6);
    }

    #[test]
    fn reads_wlr_randr_and_kscreen() {
        let text = "DP-1 \"Dell Inc. DELL U2720Q 7GZ3 (DP-1)\"\n  Make: Dell Inc.\n  Model: DELL U2720Q\n  Physical size: 600x340 mm\n  Enabled: yes\n  Modes:\n\
                    \x20   3840x2160 px, 59.997002 Hz (preferred, current)\n    2560x1440 px, 59.951000 Hz\n  Position: 0,0\n  Transform: normal\n  Scale: 1.500000\n  Adaptive Sync: disabled\n\
                    HEADLESS-1 \"Headless output 1\"\n  Enabled: no\n  Modes:\n    1280x720 px (current)\n  Position: 2560,0\n  Scale: 1.000000\n";
        let o = parse_wlr(text);
        assert_eq!(o.len(), 2);
        assert_eq!((o[0].scale, o[0].current().map(Mode::label), o[0].enabled), (Some(1.5), Some("3840x2160@60".into()), true));
        assert_eq!((o[1].enabled, o[1].x, o[1].current().map(Mode::label)), (false, 2560, Some("1280x720".into())));

        let k = r#"{"outputs":[{"name":"eDP-1","type":"Panel","connected":true,"enabled":true,"priority":1,"currentModeId":"2","preferredModes":["1"],
                    "modes":[{"id":"1","name":"1920x1080@60","refreshRate":60.0,"size":{"width":1920,"height":1080}},{"id":"2","name":"1280x720@60","refreshRate":59.9,"size":{"width":1280,"height":720}}],
                    "pos":{"x":0,"y":0},"scale":1.25}]}"#;
        let o = parse_kscreen(k).unwrap();
        assert_eq!((o[0].primary, o[0].current().map(|m| m.width), o[0].scale), (true, Some(1280), Some(1.25)));
        assert!(o[0].modes[0].preferred);
    }

    #[test]
    fn plans_changes_safely() {
        assert_eq!(parse_mode("2560x1440@144").unwrap(), (2560, 1440, Some(144.0)));
        assert_eq!(parse_mode("1920x1080").unwrap(), (1920, 1080, None));
        assert!(parse_mode("big").is_err());
        let a = |v: Value| plan(&serde_json::from_value(v).unwrap());
        assert!(a(json!({ "brightness": 1 })).is_err());
        assert!(a(json!({ "brightness": 50, "change_brightness": 5 })).is_err());
        assert!(a(json!({ "x": 5 })).is_err());
        assert!(a(json!({})).is_err());
        assert!(a(json!({ "brightness": 40 })).unwrap().is_none());

        let o = parse_xrandr(XRANDR).remove(2);
        let p = Plan { env: BTreeMap::new(), backend: Backend::Xrandr, output: o.clone(), mode: o.modes.first().cloned() };
        let set = SetArgs { mode: Some("2560x1440".into()), x: Some(0), y: Some(0), ..Default::default() };
        assert_eq!(args_for(&p, &set).1.join(" "), "--output DP-1 --mode 2560x1440 --rate 143.97 --pos 0x0");
        let p = Plan { backend: Backend::Wlr, ..p };
        let set = SetArgs { enabled: Some(false), scale: Some(1.25), ..Default::default() };
        assert_eq!(args_for(&Plan { mode: None, ..p }, &set).1.join(" "), "--output DP-1 --off --scale 1.25");
    }

    #[test]
    fn finds_the_backlight() {
        let dir = tempfile::tempdir().unwrap();
        for (name, kind, b, max) in [("acpi_video0", "firmware", "5", "10"), ("intel_backlight", "raw", "96000", "120000")] {
            let d = dir.path().join(name);
            std::fs::create_dir(&d).unwrap();
            std::fs::write(d.join("type"), kind).unwrap();
            std::fs::write(d.join("brightness"), b).unwrap();
            std::fs::write(d.join("max_brightness"), max).unwrap();
        }
        let b = backlight(dir.path()).unwrap();
        assert_eq!((b.name.as_str(), b.percent()), ("acpi_video0", 50));
        assert!(backlight(&dir.path().join("none")).is_none());
    }
}
