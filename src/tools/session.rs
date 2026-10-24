//! The person's desktop session, for the tools that act on it (sound,
//! screens, apps): its display, Wayland compositor and session bus. The
//! daemon runs as a systemd user service, which doesn't inherit them, so
//! they come from the user manager's environment (desktops import them
//! there), and failing that from the sockets in the runtime directory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{ensure, Result};

use super::exec;

/// The variables a program on the desktop needs.
const KEYS: [&str; 10] = [
    "WAYLAND_DISPLAY",
    "DISPLAY",
    "XAUTHORITY",
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_RUNTIME_DIR",
    "XDG_CURRENT_DESKTOP",
    "XDG_SESSION_TYPE",
    "XDG_SESSION_DESKTOP",
    "SWAYSOCK",
    "HYPRLAND_INSTANCE_SIGNATURE",
];

fn uid() -> u32 {
    // SAFETY: getuid has no preconditions.
    unsafe { libc::getuid() }
}

/// `KEY=VALUE` lines, as `systemctl --user show-environment` prints them.
fn parse_environment(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|l| l.split_once('='))
        .filter(|(k, _)| KEYS.contains(k))
        .map(|(k, v)| (k.to_string(), v.trim_matches('\'').to_string()))
        .collect()
}

/// Fill in what is missing from the sockets the session left: Wayland's in
/// the runtime directory, X's in /tmp/.X11-unix, the bus in the runtime
/// directory.
fn from_sockets(env: &mut BTreeMap<String, String>, runtime: &Path, x11: &Path) {
    let first = |dir: &Path, prefix: &str| -> Option<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .ok()?
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.strip_prefix(prefix).is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())))
            .collect();
        names.sort();
        names.into_iter().next()
    };
    if !env.contains_key("WAYLAND_DISPLAY") && !env.contains_key("DISPLAY") {
        if let Some(w) = first(runtime, "wayland-") {
            env.insert("WAYLAND_DISPLAY".into(), w);
        } else if let Some(x) = first(x11, "X") {
            env.insert("DISPLAY".into(), format!(":{}", &x[1..]));
        }
    }
    if !env.contains_key("DBUS_SESSION_BUS_ADDRESS") && runtime.join("bus").exists() {
        env.insert("DBUS_SESSION_BUS_ADDRESS".into(), format!("unix:path={}", runtime.join("bus").display()));
    }
}

/// The session's variables: ours, then the user manager's, then the sockets.
pub fn env() -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> =
        KEYS.iter().filter_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()).map(|v| (k.to_string(), v))).collect();
    if KEYS.iter().any(|k| !env.contains_key(*k)) && exec::installed("systemctl") {
        let mut cmd = Command::new("systemctl");
        cmd.args(["--user", "show-environment"]);
        if let Ok(done) = exec::run(cmd, None, Duration::from_secs(5), 64 << 10) {
            for (k, v) in parse_environment(&done.stdout) {
                env.entry(k).or_insert(v);
            }
        }
    }
    let runtime = PathBuf::from(env.get("XDG_RUNTIME_DIR").cloned().unwrap_or_else(|| format!("/run/user/{}", uid())));
    if runtime.is_dir() {
        env.entry("XDG_RUNTIME_DIR".into()).or_insert_with(|| runtime.display().to_string());
    }
    from_sockets(&mut env, &runtime, Path::new("/tmp/.X11-unix"));
    env
}

/// `program`, set up to talk to the session.
pub fn command(program: &str) -> Command {
    let mut cmd = Command::new(program);
    cmd.envs(env()).env("LC_ALL", "C");
    cmd
}

/// Run `program args…` on the session and return what it printed.
pub fn output(program: &str, args: &[&str], secs: u64) -> Result<String> {
    let mut cmd = command(program);
    cmd.args(args);
    let done = exec::run(cmd, None, Duration::from_secs(secs), 4 << 20)?;
    ensure!(done.success(), "{program}: {}", done.complaint());
    Ok(done.stdout)
}

/// The id of this user's graphical login session, as logind knows it.
pub fn login_session() -> Option<String> {
    let uid = uid().to_string();
    let id = exec::output("loginctl", &["show-user", &uid, "--property=Display", "--value"]).ok()?;
    let id = id.trim();
    (!id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric())).then(|| id.to_string())
}

/// logind's D-Bus object path for session `id` (systemd's bus label escaping).
pub fn session_object(id: &str) -> String {
    let mut label = String::new();
    for (i, b) in id.bytes().enumerate() {
        if b.is_ascii_alphabetic() || (b.is_ascii_digit() && i > 0) {
            label.push(b as char);
        } else {
            label.push_str(&format!("_{b:02x}"));
        }
    }
    format!("/org/freedesktop/login1/session/{label}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_session() {
        let env = parse_environment("HOME=/home/a\nWAYLAND_DISPLAY=wayland-1\nDISPLAY=':0'\nPATH=/usr/bin\n");
        assert_eq!(env.len(), 2);
        assert_eq!((env["WAYLAND_DISPLAY"].as_str(), env["DISPLAY"].as_str()), ("wayland-1", ":0"));

        let dir = tempfile::tempdir().unwrap();
        let (run, x11) = (dir.path().join("run"), dir.path().join("x11"));
        std::fs::create_dir_all(&run).unwrap();
        std::fs::create_dir_all(&x11).unwrap();
        for f in ["wayland-1", "wayland-0", "wayland-0.lock", "bus"] {
            std::fs::write(run.join(f), "").unwrap();
        }
        let mut env = BTreeMap::new();
        from_sockets(&mut env, &run, &x11);
        assert_eq!(env["WAYLAND_DISPLAY"], "wayland-0");
        assert_eq!(env["DBUS_SESSION_BUS_ADDRESS"], format!("unix:path={}", run.join("bus").display()));

        // X only: the display comes from its socket.
        std::fs::write(x11.join("X1"), "").unwrap();
        let mut env = BTreeMap::new();
        from_sockets(&mut env, &x11, &x11);
        assert_eq!(env["DISPLAY"], ":1");

        assert_eq!(session_object("2"), "/org/freedesktop/login1/session/_32");
        assert_eq!(session_object("c12"), "/org/freedesktop/login1/session/c12");
    }
}
