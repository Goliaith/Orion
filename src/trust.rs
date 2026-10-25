//! Trust levels: three ready-made answers to "what may Orion do without
//! asking?". `orion setup` writes one into the policy; `orion doctor` says
//! which one the policy matches. Hard limits (the writable roots, the
//! protected services, packages and mounts, the sandbox, Flatpak remotes)
//! are the same at every level.

use crate::policy::Decision;
use crate::setup::{set, Edit};
use crate::{Policy, Risk};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    AskEverything,
    Everyday,
    HandsOff,
}

/// Every kind `[desktop] approved` knows.
const DESKTOP_KINDS: [&str; 12] =
    ["volume", "brightness", "displays", "power_profile", "lock", "suspend", "hibernate", "reboot", "poweroff", "bluetooth", "clock", "open"];
const EVERYDAY_DESKTOP: [&str; 4] = ["volume", "brightness", "lock", "open"];
/// What Everyday lets `svc_control` start and stop when the policy names no
/// units of its own: the model servers.
const EVERYDAY_SERVICES: [&str; 2] = ["ollama.service", "llama-server.service"];

impl Level {
    pub const ALL: [Level; 3] = [Level::AskEverything, Level::Everyday, Level::HandsOff];

    pub fn name(self) -> &'static str {
        match self {
            Level::AskEverything => "Ask me everything",
            Level::Everyday => "Everyday",
            Level::HandsOff => "Hands-off",
        }
    }

    /// For `--trust`.
    pub fn id(self) -> &'static str {
        match self {
            Level::AskEverything => "ask",
            Level::Everyday => "everyday",
            Level::HandsOff => "hands-off",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|l| l.id() == s)
    }

    /// What runs without asking, in a line.
    pub fn describe(self) -> &'static str {
        match self {
            Level::AskEverything => "every change waits for your yes, even the volume",
            Level::Everyday => {
                "Flatpak installs, volume and brightness, opening apps and files, and starting the allowed services \
                 run without asking; the rest asks"
            }
            Level::HandsOff => {
                "everything runs without asking (programs, packages, services, files, rebooting), \
                 within the policy's hard limits"
            }
        }
    }

    fn settings(self, current: &Policy) -> Settings {
        let list = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let any = || Some(list(&["*"]));
        match self {
            Level::AskEverything => Settings {
                write: Decision::Ask,
                execute: Decision::Ask,
                desktop: list(&[]),
                apps: Some(list(&[])),
                flatpak: Some(list(&[])),
                packages: Some(list(&[])),
                services: Some(list(&[])),
                commands: Some(list(&[])),
                updates: Some("ask"),
            },
            Level::Everyday => {
                let own = &current.services.allowed;
                let services = if own.is_empty() || own.iter().any(|u| u == "*") { list(&EVERYDAY_SERVICES) } else { own.clone() };
                Settings {
                    write: Decision::Ask,
                    execute: Decision::Ask,
                    desktop: list(&EVERYDAY_DESKTOP),
                    apps: any(),
                    flatpak: any(),
                    packages: None,
                    services: Some(services),
                    commands: None,
                    updates: None,
                }
            }
            Level::HandsOff => Settings {
                write: Decision::Allow,
                execute: Decision::Allow,
                desktop: list(&DESKTOP_KINDS),
                apps: any(),
                flatpak: any(),
                packages: any(),
                services: any(),
                commands: any(),
                // Updates that remove packages are refused rather than run.
                updates: Some("all"),
            },
        }
    }

    /// The changes to orion.toml that set this level.
    pub fn edits(self, current: &Policy) -> Vec<Edit> {
        let s = self.settings(current);
        let array = |v: &[String]| toml_edit::Value::Array(v.iter().map(String::as_str).collect());
        let mut edits = vec![
            set("defaults.write", s.write.as_str()),
            set("defaults.execute", s.execute.as_str()),
            Edit::Set("desktop.approved", array(&s.desktop)),
        ];
        let lists: [(&'static str, &Option<Vec<String>>); 5] = [
            ("desktop.apps", &s.apps),
            ("flatpak.allowed", &s.flatpak),
            ("packages.allowed", &s.packages),
            ("services.allowed", &s.services),
            ("proc.allowed_commands", &s.commands),
        ];
        edits.extend(lists.into_iter().filter_map(|(k, v)| v.as_deref().map(|v| Edit::Set(k, array(v)))));
        if let Some(u) = s.updates {
            edits.push(set("packages.updates", u));
        }
        edits
    }

    /// Where `policy` differs from this level, as `key = value` lines.
    pub fn differences(self, policy: &Policy) -> Vec<String> {
        let s = self.settings(policy);
        let mut out = Vec::new();
        let mut decision = |key: &str, want: Decision, have: Decision| {
            if want != have {
                out.push(format!("{key} = {:?}", have.as_str()));
            }
        };
        decision("defaults.write", s.write, policy.defaults.write);
        decision("defaults.execute", s.execute, policy.defaults.execute);
        let lists: [ListCheck; 6] = [
            ("desktop.approved", Some(&s.desktop), &policy.desktop.approved),
            ("desktop.apps", s.apps.as_deref(), &policy.desktop.apps),
            ("flatpak.allowed", s.flatpak.as_deref(), &policy.flatpak.allowed),
            ("packages.allowed", s.packages.as_deref(), &policy.packages.allowed),
            ("services.allowed", s.services.as_deref(), &policy.services.allowed),
            ("proc.allowed_commands", s.commands.as_deref(), &policy.proc.allowed_commands),
        ];
        for (key, want, have) in lists {
            let Some(want) = want else { continue };
            // Everyday takes the person's own units, whatever they are.
            let same = if key == "services.allowed" && self == Level::Everyday {
                !have.is_empty() && !have.iter().any(|u| u == "*")
            } else {
                same_set(want, have)
            };
            if !same {
                out.push(format!("{key} = {have:?}"));
            }
        }
        if s.updates.is_some_and(|u| u != policy.packages.updates) {
            out.push(format!("packages.updates = {:?}", policy.packages.updates));
        }
        out
    }
}

/// A list's key, what the level wants (`None`: anything), and what it has.
type ListCheck<'a> = (&'a str, Option<&'a [String]>, &'a [String]);

/// What a level sets. `None`: left as the person has it.
struct Settings {
    write: Decision,
    execute: Decision,
    desktop: Vec<String>,
    apps: Option<Vec<String>>,
    flatpak: Option<Vec<String>>,
    packages: Option<Vec<String>>,
    services: Option<Vec<String>>,
    commands: Option<Vec<String>>,
    updates: Option<&'static str>,
}

fn same_set(a: &[String], b: &[String]) -> bool {
    a.iter().all(|x| b.contains(x)) && b.iter().all(|x| a.contains(x))
}

/// The level `policy` is set to, if it matches one exactly.
pub fn detect(policy: &Policy) -> Option<Level> {
    Level::ALL.into_iter().find(|l| l.differences(policy).is_empty())
}

/// Whether `policy` lets run unasked just what Orion does out of the box.
pub fn is_default(policy: &Policy) -> bool {
    let fields = |p: &Policy| {
        format!(
            "{:?}",
            (p.defaults.write, p.defaults.execute, &p.desktop, &p.flatpak.allowed, &p.packages.allowed, &p.packages.updates,
             &p.services.allowed, &p.proc.allowed_commands)
        )
    };
    Policy::from_toml("").is_ok_and(|d| fields(&d) == fields(policy))
}

/// The level `policy` is nearest to, and how it differs.
pub fn closest(policy: &Policy) -> (Level, Vec<String>) {
    Level::ALL.into_iter().map(|l| (l, l.differences(policy))).min_by_key(|(_, d)| d.len()).expect("three levels")
}

/// Write and execute tools that `[tools]` (or tuning.toml) lets run without
/// asking, which the level alone wouldn't.
pub fn allowed_overrides(policy: &Policy) -> Vec<String> {
    if policy.defaults.write == Decision::Allow && policy.defaults.execute == Decision::Allow {
        return Vec::new();
    }
    crate::tools::builtin()
        .iter()
        .filter(|t| matches!(t.risk(), Risk::Write | Risk::Execute))
        .filter(|t| policy.tools.get(t.name()) == Some(&Decision::Allow) && policy.decide(t.name(), t.risk()) == Decision::Allow)
        .map(|t| t.name().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::edited;

    fn with(level: Level, text: &str) -> Policy {
        let current = Policy::from_toml(text).unwrap();
        Policy::from_toml(&edited(Some(text), &level.edits(&current)).unwrap()).unwrap()
    }

    #[test]
    fn each_level_is_named_once_written() {
        for level in Level::ALL {
            assert_eq!(detect(&with(level, "")), Some(level), "{level:?}");
            // From the example, comments and all, and from another level.
            let example = include_str!("../orion.example.toml");
            assert_eq!(detect(&with(level, example)), Some(level), "{level:?} from the example");
            for from in Level::ALL {
                let text = edited(Some(""), &from.edits(&Policy::from_toml("").unwrap())).unwrap();
                assert_eq!(detect(&with(level, &text)), Some(level), "{from:?} to {level:?}");
            }
            assert_eq!(Level::parse(level.id()), Some(level));
        }
        assert_eq!(Level::parse("yolo"), None);
    }

    #[test]
    fn the_defaults_are_no_level() {
        let p = Policy::from_toml("").unwrap();
        assert_eq!(detect(&p), None);
        assert!(is_default(&p) && !is_default(&with(Level::Everyday, "")));
        let (near, diff) = closest(&p);
        assert_eq!(near, Level::AskEverything);
        assert_eq!(diff, vec![r#"desktop.approved = ["volume", "brightness", "lock"]"#.to_string()]);
    }

    #[test]
    fn everyday_keeps_the_persons_own_lists() {
        let text = "[services]\nallowed = [\"speaches.service\"]\n[proc]\nallowed_commands = [\"git\"]\n\
                    [packages]\nallowed = [\"jq\"]\nupdates = \"security\"\n";
        let p = with(Level::Everyday, text);
        assert_eq!(p.services.allowed, vec!["speaches.service"]);
        assert_eq!(p.proc.allowed_commands, vec!["git"]);
        assert_eq!((p.packages.allowed.clone(), p.packages.updates.as_str()), (vec!["jq".to_string()], "security"));
        assert_eq!(p.flatpak.allowed, vec!["*"]);
        assert!(p.desktop.approved.contains(&"open".to_string()));
        // With no units of its own, the model servers.
        assert_eq!(with(Level::Everyday, "").services.allowed, EVERYDAY_SERVICES.map(String::from));
        // Ask me everything clears them all.
        let p = with(Level::AskEverything, text);
        assert!(p.services.allowed.is_empty() && p.proc.allowed_commands.is_empty() && p.packages.allowed.is_empty());
        assert_eq!(p.packages.updates, "ask");
    }

    #[test]
    fn levels_decide_as_described() {
        use crate::tools::builtin;
        let tool = |n: &str| builtin().into_iter().find(|t| t.name() == n).unwrap();
        let runs_unasked = |p: &Policy, n: &str, args: serde_json::Value| {
            let t = tool(n);
            t.preapproval(p, &args).unasked(p.decide(n, t.risk()))
        };
        let flatpak = serde_json::json!({ "app": "org.kde.krita" });
        let volume = serde_json::json!({ "volume": "+5%" });
        let ollama = serde_json::json!({ "unit": "ollama", "action": "start" });
        let pkg = serde_json::json!({ "packages": ["htop"] });

        let p = with(Level::AskEverything, "");
        assert!(!runs_unasked(&p, "flatpak_install", flatpak.clone()));
        assert!(!runs_unasked(&p, "audio_set", volume.clone()));
        assert!(!runs_unasked(&p, "svc_control", ollama.clone()));

        let p = with(Level::Everyday, "");
        assert!(runs_unasked(&p, "flatpak_install", flatpak.clone()));
        assert!(runs_unasked(&p, "audio_set", volume.clone()));
        assert!(runs_unasked(&p, "svc_control", ollama.clone()));
        assert!(!runs_unasked(&p, "svc_control", serde_json::json!({ "unit": "nginx", "action": "start" })));
        assert!(!runs_unasked(&p, "pkg_install", pkg.clone()));
        assert!(!runs_unasked(&p, "fs_write", serde_json::json!({})));

        let p = with(Level::HandsOff, "");
        assert!(runs_unasked(&p, "pkg_install", pkg));
        assert!(runs_unasked(&p, "proc_run", serde_json::json!({ "command": "ls" })));
        assert!(runs_unasked(&p, "svc_control", serde_json::json!({ "unit": "nginx", "action": "start" })));
        // A person still agrees to every change to what Orion may do.
        assert_eq!(p.decide("settings_tune", Risk::Write), Decision::Ask);
    }

    #[test]
    fn names_tools_allowed_on_top_of_the_level() {
        let p = with(Level::Everyday, "[tools]\nfs_write = \"allow\"\nfs_read = \"allow\"\nproc_run = \"ask\"\n");
        assert_eq!(detect(&p), Some(Level::Everyday));
        assert_eq!(allowed_overrides(&p), vec!["fs_write"]);
        assert!(allowed_overrides(&with(Level::HandsOff, "[tools]\nfs_write = \"allow\"\n")).is_empty());
    }
}
