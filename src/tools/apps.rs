//! Desktop apps: the installed ones (from their desktop entries, as menus
//! list them, Flatpak and Snap included), starting one, optionally on files
//! or links, and opening a file or link in its default app.
//!
//! An app starts from its own `Exec=` line, split into arguments as the
//! desktop entry spec says, with the files and links filled in as single
//! arguments: no shell sees them. It runs in a systemd unit of its own
//! (`app-orion-….service`), so it lives on after Orion and its output goes
//! to the journal; without a user manager, in a session of its own.
//! Files must be inside the readable roots, and executable files are never
//! opened, so opening can't run a program the AI wrote.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use super::exec::{self, glob_match};
use super::session;
use crate::policy::Policy;
use crate::tool::{parse_args, Preapproval, Risk, Tool};

#[derive(Debug, Clone, Default, PartialEq)]
struct App {
    /// The desktop id: `org.gnome.Nautilus`, `firefox` (without .desktop).
    id: String,
    path: PathBuf,
    name: String,
    generic: String,
    comment: String,
    keywords: String,
    categories: Vec<String>,
    exec: String,
    icon: String,
    terminal: bool,
    hidden: bool,
}

impl App {
    /// What the Exec line takes: "files", "urls" or nothing.
    fn takes(&self) -> Option<&'static str> {
        let e = &self.exec;
        if e.contains("%U") || e.contains("%u") {
            Some("urls")
        } else if e.contains("%F") || e.contains("%f") {
            Some("files")
        } else {
            None
        }
    }

    fn json(&self) -> Value {
        json!({
            "id": self.id, "name": self.name, "kind": (!self.generic.is_empty()).then_some(&self.generic),
            "description": (!self.comment.is_empty()).then_some(&self.comment), "categories": self.categories,
            "opens": self.takes(), "terminal": self.terminal,
        })
    }
}

/// A desktop entry's `[Desktop Entry]` group, unlocalized keys only.
fn parse_entry(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut in_group = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_group = line == "[Desktop Entry]";
            continue;
        }
        if !in_group || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            if !k.contains('[') {
                out.entry(k.to_string()).or_insert_with(|| v.trim().to_string());
            }
        }
    }
    out
}

/// The string escapes of desktop entry values: `\s`, `\n`, `\t`, `\r`, `\\`.
fn unescape(v: &str) -> String {
    let mut out = String::new();
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            // Applied before the Exec quoting, so a quoted argument
            // spells a literal backslash `\\\\`.
            Some('\\') => out.push('\\'),
            Some(o) => {
                // Left for the Exec quoting rules (\" \` \$ \\).
                out.push('\\');
                out.push(o);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// An Exec line as arguments: spaces separate them, double quotes group,
/// and inside quotes a backslash escapes `"`, `` ` ``, `$` and `\`.
fn split_exec(exec: &str) -> Result<Vec<String>> {
    let s = unescape(exec);
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut started = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '\\' if quoted => match chars.next() {
                Some(n @ ('"' | '`' | '$' | '\\')) => cur.push(n),
                Some(n) => {
                    cur.push('\\');
                    cur.push(n);
                }
                None => bail!("the Exec line ends in a backslash"),
            },
            ' ' | '\t' if !quoted => {
                if started || !cur.is_empty() {
                    args.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => cur.push(c),
        }
    }
    ensure!(!quoted, "the Exec line has an unclosed quote");
    if started || !cur.is_empty() {
        args.push(cur);
    }
    ensure!(!args.is_empty(), "the Exec line is empty");
    Ok(args)
}

/// The command for `app` on `targets`: field codes filled in, each target
/// one argument. Several runs when the app takes one file at a time.
fn commands(app: &App, targets: &[String]) -> Result<Vec<Vec<String>>> {
    let words = split_exec(&app.exec)?;
    let many = words.iter().any(|w| w == "%F" || w == "%U");
    let single = !many && words.iter().any(|w| w.contains("%f") || w.contains("%u"));
    ensure!(targets.is_empty() || many || single, "{} doesn't open files or links; start it without them", app.name);
    let runs: Vec<&[String]> = if single && targets.len() > 1 { targets.chunks(1).collect() } else { vec![targets] };
    let mut out = Vec::new();
    for these in runs {
        let mut argv = Vec::new();
        for w in &words {
            match w.as_str() {
                "%F" | "%U" => argv.extend(these.iter().cloned()),
                "%i" if !app.icon.is_empty() => argv.extend(["--icon".to_string(), app.icon.clone()]),
                "%i" => {}
                "%c" => argv.push(app.name.clone()),
                "%k" => argv.push(app.path.display().to_string()),
                "%f" | "%u" => argv.extend(these.first().cloned()),
                // Deprecated codes go; %% is a percent sign.
                "%d" | "%D" | "%n" | "%N" | "%v" | "%m" => {}
                w => {
                    let mut s = String::new();
                    let mut chars = w.chars().peekable();
                    while let Some(c) = chars.next() {
                        if c != '%' {
                            s.push(c);
                            continue;
                        }
                        match chars.next() {
                            Some('%') => s.push('%'),
                            Some('f' | 'u') => s.push_str(these.first().map(String::as_str).unwrap_or("")),
                            Some('c') => s.push_str(&app.name),
                            Some('k') => s.push_str(&app.path.display().to_string()),
                            _ => {}
                        }
                    }
                    if !s.is_empty() {
                        argv.push(s);
                    }
                }
            }
        }
        out.push(argv);
    }
    Ok(out)
}

/// Where desktop entries live, first first: the person's own, the system's,
/// then Flatpak's and Snap's exports.
fn dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let data_home = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".local/share"));
    let data_dirs = std::env::var("XDG_DATA_DIRS").ok().filter(|d| !d.is_empty()).unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    let mut out = vec![data_home.join("applications")];
    out.extend(data_dirs.split(':').filter(|d| !d.is_empty()).map(|d| Path::new(d).join("applications")));
    for extra in [data_home.join("flatpak/exports/share/applications"), "/var/lib/flatpak/exports/share/applications".into(), "/var/lib/snapd/desktop/applications".into()] {
        if !out.contains(&extra) {
            out.push(extra);
        }
    }
    out
}

/// Every app in `dirs`; an id found earlier hides the same id later.
fn apps(dirs: &[PathBuf]) -> Vec<App> {
    let mut seen = BTreeMap::new();
    for dir in dirs {
        let mut stack = vec![dir.clone()];
        while let Some(d) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&d) else { continue };
            for e in entries.flatten() {
                let path = e.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let Some(rel) = path.strip_prefix(dir).ok().and_then(|r| r.to_str()) else { continue };
                let Some(id) = rel.strip_suffix(".desktop") else { continue };
                let id = id.replace('/', "-");
                if seen.contains_key(&id) {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else { continue };
                let f = parse_entry(&text);
                let get = |k: &str| f.get(k).map(|v| unescape(v)).unwrap_or_default();
                let yes = |k: &str| f.get(k).is_some_and(|v| v == "true");
                let try_exec = get("TryExec");
                let missing = !try_exec.is_empty() && !(Path::new(&try_exec).is_absolute() && Path::new(&try_exec).exists() || exec::installed(&try_exec));
                let app = App {
                    id: id.clone(),
                    path: path.clone(),
                    name: get("Name"),
                    generic: get("GenericName"),
                    comment: get("Comment"),
                    keywords: get("Keywords"),
                    categories: get("Categories").split(';').filter(|c| !c.is_empty()).map(String::from).collect(),
                    exec: f.get("Exec").cloned().unwrap_or_default(),
                    icon: get("Icon"),
                    terminal: yes("Terminal"),
                    hidden: yes("Hidden") || yes("NoDisplay") || missing || f.get("Type").is_some_and(|t| t != "Application"),
                };
                seen.insert(id, if f.get("Hidden").is_some_and(|v| v == "true") { None } else { Some(app) });
            }
        }
    }
    let mut list: Vec<App> = seen.into_values().flatten().filter(|a| !a.exec.is_empty() && !a.name.is_empty()).collect();
    list.sort_by_key(|a| a.name.to_lowercase());
    list
}

/// The name of the installed app that `wanted` names, if exactly one
/// does: for a request like "open firefox" that needs no model.
pub fn resolve(wanted: &str) -> Option<String> {
    let list = apps(&dirs());
    find(&list, wanted).ok().filter(|a| !a.terminal).map(|a| a.name.clone())
}

/// The app `wanted` names: its id (with or without .desktop), its exact
/// name, or the one app whose name contains it.
fn find<'a>(list: &'a [App], wanted: &str) -> Result<&'a App> {
    let id = wanted.strip_suffix(".desktop").unwrap_or(wanted);
    let low = wanted.to_lowercase();
    if let Some(a) = list.iter().find(|a| a.id == id).or_else(|| list.iter().find(|a| !a.hidden && a.name.to_lowercase() == low)) {
        return Ok(a);
    }
    let found: Vec<&App> = list.iter().filter(|a| !a.hidden && a.name.to_lowercase().contains(&low)).collect();
    match found.as_slice() {
        [a] => Ok(a),
        [] => bail!("no app is called {wanted:?} (app_list shows what is installed)"),
        many => bail!("{wanted:?} could be {}; name one by id", many.iter().map(|a| format!("{} ({})", a.name, a.id)).collect::<Vec<_>>().join(", ")),
    }
}

/// A file or link to open: a web or mail link, or a file inside the
/// readable roots that isn't a program.
fn check_target(policy: &Policy, t: &str) -> Result<String> {
    let low = t.to_lowercase();
    if low.starts_with("http://") || low.starts_with("https://") || low.starts_with("mailto:") {
        ensure!(!t.chars().any(|c| c.is_whitespace() || c.is_control()), "{t:?} isn't a link");
        return Ok(t.to_string());
    }
    ensure!(!t.contains("://") && !low.starts_with("file:"), "only http, https and mailto links open, and plain paths; not {t:?}");
    let path = policy.check_read(t)?;
    let meta = std::fs::metadata(&path).with_context(|| format!("{}", path.display()))?;
    let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    ensure!(meta.is_dir() || meta.permissions().mode() & 0o111 == 0, "{} is a program; Orion doesn't open programs", path.display());
    ensure!(!name.ends_with(".desktop"), "{} is a launcher; start apps with app_open by id", path.display());
    Ok(path.display().to_string())
}

fn unit_name(id: &str) -> String {
    let clean: String = id.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' }).take(60).collect();
    let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    format!("app-orion-{clean}-{stamp}")
}

/// Start `argv` on the desktop, detached. Its systemd unit, if it got one.
fn launch(id: &str, argv: &[String]) -> Result<Option<String>> {
    let env = session::env();
    let user_manager = exec::installed("systemd-run") && {
        let mut c = Command::new("systemctl");
        c.args(["--user", "is-system-running"]);
        exec::run(c, None, Duration::from_secs(5), 4096).is_ok_and(|d| ["running", "degraded", "starting"].contains(&d.stdout.trim()))
    };
    if user_manager {
        let unit = unit_name(id);
        let mut cmd = Command::new("systemd-run");
        cmd.args(["--user", "--quiet", "--collect", "--no-block", &format!("--unit={unit}"), &format!("--description=Started by Orion: {id}")]);
        for (k, v) in &env {
            cmd.arg(format!("--setenv={k}={v}"));
        }
        cmd.arg("--").args(argv);
        let done = exec::run(cmd, None, Duration::from_secs(15), 16 << 10)?;
        ensure!(done.success(), "systemd-run: {}", done.complaint());
        // A program that fails at once (it isn't there) shows by now.
        std::thread::sleep(Duration::from_millis(800));
        let state = exec::output("systemctl", &["--user", "show", "--property=ActiveState,Result", &format!("{unit}.service")]).unwrap_or_default();
        if state.contains("ActiveState=failed") {
            let log = exec::output("journalctl", &["--user", "-u", &format!("{unit}.service"), "-n", "8", "-o", "cat", "--no-pager"]).unwrap_or_default();
            bail!("{} failed to start: {}", argv[0], log.trim());
        }
        return Ok(Some(format!("{unit}.service")));
    }
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).envs(&env).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe; it detaches the app from Orion's session.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn().with_context(|| format!("starting {}", argv[0]))?;
    std::thread::sleep(Duration::from_millis(500));
    if let Ok(Some(status)) = child.try_wait() {
        ensure!(status.success(), "{} exited at once ({status})", argv[0]);
    } else {
        // Reap it whenever it ends.
        std::thread::spawn(move || child.wait());
    }
    Ok(None)
}

pub struct AppList;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    query: Option<String>,
    #[serde(default = "fifty")]
    limit: usize,
}

fn fifty() -> usize {
    50
}

impl Tool for AppList {
    fn name(&self) -> &'static str {
        "app_list"
    }

    fn description(&self) -> &'static str {
        "The desktop apps installed (as the app menu shows them, Flatpak and Snap included): id, name, what \
         kind of app, and whether it opens files or links. query filters by name, kind, keywords or category."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Only apps matching this: \"browser\", \"photo\", \"terminal\"." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 500, "default": 50 }
            },
            "additionalProperties": false
        })
    }

    fn call(&self, _policy: &Policy, args: Value) -> Result<Value> {
        let a: ListArgs = parse_args(args)?;
        let all = apps(&dirs());
        let q = a.query.as_deref().map(str::to_lowercase);
        let matches: Vec<&App> = all
            .iter()
            .filter(|x| !x.hidden)
            .filter(|x| {
                q.as_ref().is_none_or(|q| {
                    [&x.id, &x.name, &x.generic, &x.comment, &x.keywords].iter().any(|f| f.to_lowercase().contains(q.as_str()))
                        || x.categories.iter().any(|c| c.to_lowercase().contains(q.as_str()))
                })
            })
            .collect();
        Ok(json!({
            "count": matches.len(),
            "apps": matches.iter().take(a.limit.clamp(1, 500)).map(|x| x.json()).collect::<Vec<_>>(),
        }))
    }
}

pub struct AppOpen;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenArgs {
    app: Option<String>,
    #[serde(default)]
    targets: Vec<String>,
}

/// What to run, worked out before any approval.
fn plan(policy: &Policy, a: &OpenArgs) -> Result<(String, Vec<Vec<String>>)> {
    ensure!(a.targets.len() <= 20, "open at most 20 at once");
    ensure!(a.app.is_some() || !a.targets.is_empty(), "name an app, or files and links to open");
    let targets: Vec<String> = a.targets.iter().map(|t| check_target(policy, t)).collect::<Result<_>>()?;
    match &a.app {
        Some(wanted) => {
            let list = apps(&dirs());
            let app = find(&list, wanted)?;
            ensure!(!app.terminal, "{} runs in a terminal; start it from a terminal app instead", app.name);
            Ok((app.id.clone(), commands(app, &targets)?))
        }
        None => {
            ensure!(exec::installed("xdg-open"), "opening in the default app needs xdg-open (xdg-utils)");
            Ok(("xdg-open".into(), targets.into_iter().map(|t| vec!["xdg-open".to_string(), t]).collect()))
        }
    }
}

impl Tool for AppOpen {
    fn name(&self) -> &'static str {
        "app_open"
    }

    fn description(&self) -> &'static str {
        "Start a desktop app (by id or name from app_list), optionally on files or links; or, with no app, \
         open files or links in their default apps (a web page in the browser, a PDF in the viewer). Files \
         must be inside the readable roots; links must be http, https or mailto. Programs are never opened."
    }

    fn risk(&self) -> Risk {
        Risk::Execute
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "app": { "type": "string", "description": "The app's id (\"org.gnome.Nautilus\", \"firefox\") or name." },
                "targets": { "type": "array", "items": { "type": "string" }, "maxItems": 20,
                             "description": "Files (paths) or links to open." }
            },
            "additionalProperties": false
        })
    }

    fn preapproval(&self, policy: &Policy, args: &Value) -> Preapproval {
        let Ok(a) = parse_args::<OpenArgs>(args.clone()) else { return Preapproval::NotApplicable };
        match &a.app {
            Some(app) => {
                let id = app.strip_suffix(".desktop").unwrap_or(app);
                let resolved = apps(&dirs()).into_iter().find(|x| x.id == id || x.name.eq_ignore_ascii_case(app)).map(|x| x.id);
                let id = resolved.as_deref().unwrap_or(id);
                if policy.desktop.apps.iter().any(|p| glob_match(p, id)) { Preapproval::Granted } else { Preapproval::NotApplicable }
            }
            None => policy.desktop.preapproval("open"),
        }
    }

    fn precheck(&self, policy: &Policy, args: &Value) -> Result<()> {
        plan(policy, &parse_args(args.clone())?).map(drop)
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: OpenArgs = parse_args(args)?;
        let (id, runs) = plan(policy, &a)?;
        let mut units = Vec::new();
        for argv in &runs {
            units.extend(launch(&id, argv)?);
        }
        Ok(json!({ "ok": true, "app": id, "started": runs.len(), "units": units }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(exec: &str) -> App {
        App { id: "x".into(), path: "/usr/share/applications/x.desktop".into(), name: "X Viewer".into(), exec: exec.into(), icon: "x-icon".into(), ..Default::default() }
    }

    #[test]
    fn splits_exec_lines() {
        assert_eq!(split_exec(r#"firefox --new-window %u"#).unwrap(), ["firefox", "--new-window", "%u"]);
        assert_eq!(split_exec(r#""/opt/My App/run" --name="a \"b\"" %F"#).unwrap(), ["/opt/My App/run", "--name=a \"b\"", "%F"]);
        assert_eq!(split_exec(r#"sh -c "echo \$HOME""#).unwrap(), ["sh", "-c", "echo $HOME"]);
        assert_eq!(split_exec(r"code\s--wait").unwrap(), ["code", "--wait"]);
        // Four backslashes in the file are one in the argument.
        assert_eq!(split_exec(r#"sh -c "printf '%%s\\\\n' \"\$@\"" x"#).unwrap(), ["sh", "-c", r#"printf '%%s\n' "$@""#, "x"]);
        assert!(split_exec(r#"broken "quote"#).is_err());

        let t = |e: &str, targets: &[&str]| commands(&app(e), &targets.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(t("viewer %F", &["/a b.pdf", "/c.pdf"]).unwrap(), [vec!["viewer", "/a b.pdf", "/c.pdf"]]);
        assert_eq!(t("viewer %f", &["/a.pdf", "/c.pdf"]).unwrap(), [vec!["viewer", "/a.pdf"], vec!["viewer", "/c.pdf"]]);
        assert_eq!(t("viewer %i --title=%c %U", &[]).unwrap(), [vec!["viewer", "--icon", "x-icon", "--title=X Viewer"]]);
        assert_eq!(t("viewer --file=%f 100%%", &["/a; rm -rf ~"]).unwrap(), [vec!["viewer", "--file=/a; rm -rf ~", "100%"]]);
        assert!(t("settings", &["/a.pdf"]).is_err());
        assert_eq!(t("flatpak run --file-forwarding org.x.App @@u %U @@", &["https://x.org"]).unwrap(),
                   [vec!["flatpak", "run", "--file-forwarding", "org.x.App", "@@u", "https://x.org", "@@"]]);
    }

    #[test]
    fn finds_apps_and_checks_targets() {
        let dir = tempfile::tempdir().unwrap();
        let (home, sys) = (dir.path().join("home"), dir.path().join("sys"));
        std::fs::create_dir_all(home.join("kde")).unwrap();
        std::fs::create_dir_all(&sys).unwrap();
        let entry = |name: &str, extra: &str| format!("[Desktop Entry]\nType=Application\nName={name}\nName[de]=Anders\nExec={}\n{extra}\n[Desktop Action new]\nName=New\nExec=other\n", name.to_lowercase());
        std::fs::write(sys.join("firefox.desktop"), entry("Firefox", "GenericName=Web Browser\nCategories=Network;WebBrowser;\nKeywords=internet;web;")).unwrap();
        std::fs::write(sys.join("hidden.desktop"), entry("Hidden", "NoDisplay=true")).unwrap();
        std::fs::write(sys.join("gone.desktop"), entry("Gone", "TryExec=orion-no-such-program")).unwrap();
        std::fs::write(home.join("kde/okular.desktop"), entry("Okular", "Exec=okular %U")).unwrap();
        // The person's own entry hides the system's.
        std::fs::write(home.join("firefox.desktop"), entry("Firefox Mine", "")).unwrap();
        let list = apps(&[home.clone(), sys.clone()]);
        let visible: Vec<&str> = list.iter().filter(|a| !a.hidden).map(|a| a.id.as_str()).collect();
        assert_eq!(visible, ["firefox", "kde-okular"]);
        assert_eq!(list.iter().find(|a| a.id == "firefox").unwrap().name, "Firefox Mine");
        assert_eq!(find(&list, "kde-okular.desktop").unwrap().name, "Okular");
        assert_eq!(find(&list, "okul").unwrap().id, "kde-okular");
        assert!(find(&list, "nothing").is_err());

        let work = dir.path().join("work");
        std::fs::create_dir(&work).unwrap();
        std::fs::write(work.join("report.pdf"), "x").unwrap();
        std::fs::write(work.join("run.sh"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(work.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(work.join("evil.desktop"), "x").unwrap();
        let p = Policy::from_toml(&format!("[fs]\nread_roots = [{work:?}]\n")).unwrap();
        assert!(check_target(&p, work.join("report.pdf").to_str().unwrap()).is_ok());
        assert!(check_target(&p, "https://example.org/a?b=c").is_ok());
        for bad in [work.join("run.sh").display().to_string(), work.join("evil.desktop").display().to_string(), "/etc/shadow".into(),
                    "steam://run/1".into(), "file:///etc/passwd".into(), "https://x.org/a b".into()] {
            assert!(check_target(&p, &bad).is_err(), "{bad}");
        }
        assert!(unit_name("org.gnome/Nautilus").starts_with("app-orion-org.gnome_Nautilus-"));
    }
}
