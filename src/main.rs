use std::io;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use orion::chat::terminal::{self, PromptApprover};
use orion::chat::Mode;
use orion::mcp::Server;
use orion::{audit, daemon, Approver, Orion, Policy};

#[derive(Parser)]
#[command(name = "orion", version, about = "The AI interface layer of an AI-first Linux system")]
struct Cli {
    /// Policy file (default: $ORION_CONFIG, then ~/.config/orion/orion.toml).
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum ConversationCmd {
    /// Saved conversations, most recent first (the default).
    List,
    /// Print a conversation's transcript.
    Show { id: String },
    /// Delete a conversation.
    Delete { id: String },
}

#[derive(Subcommand)]
enum MemoryCmd {
    /// What is remembered (the default).
    List,
    /// Forget one note, by its number.
    Forget { id: u32 },
    /// Forget everything about the person.
    Clear,
}

#[derive(Subcommand)]
enum VoiceCmd {
    /// Teach Orion someone's voice; only enrolled people can wake it.
    Enroll {
        /// Whose voice (default: your login name, for the first person).
        #[arg(long)]
        name: Option<String>,
    },
    /// Say something and see who Orion takes it for.
    Test,
    /// Who is enrolled, and what each may do by voice.
    Status,
    /// Measure the room's noise and your voice, and suggest a speech threshold.
    Noise,
    /// Forget someone's voice (or everyone's, with --all).
    Forget {
        name: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// The neural speaker model: install, check or remove it.
    Model {
        #[command(subcommand)]
        action: ModelCmd,
    },
}

#[derive(Subcommand)]
enum ModelCmd {
    /// Download the speaker encoder (about 16 MB, from PyPI, checksum pinned) and convert it.
    Install {
        /// Use this Resemblyzer 0.1.4 wheel, or its pretrained.pt, instead of downloading.
        #[arg(long)]
        from: Option<PathBuf>,
    },
    /// Whether the model is installed.
    Status,
    /// Delete the model: verification falls back to the built-in method.
    Remove,
}

#[derive(Subcommand)]
enum Cmd {
    /// Talk to an AI in the terminal; it works on the machine through Orion's tools.
    Chat {
        /// Always use the model served on this machine.
        #[arg(long, conflicts_with_all = ["claude", "auto"])]
        local: bool,
        /// Always use Claude.
        #[arg(long, conflicts_with = "auto")]
        claude: bool,
        /// Route each message to Claude or the local model (the default).
        #[arg(long)]
        auto: bool,
        /// Use this model instead of the configured one (with --claude or --local).
        #[arg(long)]
        model: Option<String>,
        /// Line-by-line chat instead of the full-screen interface.
        #[arg(long)]
        plain: bool,
        /// Continue a saved conversation: the latest, or the one with this id (or id prefix).
        #[arg(long, value_name = "ID", num_args = 0..=1, default_missing_value = "")]
        resume: Option<String>,
    },
    /// The graphical shell: a local web interface for the chat, for a browser or a kiosk.
    Shell {
        /// Always use the model served on this machine.
        #[arg(long, conflicts_with_all = ["claude", "auto"])]
        local: bool,
        /// Always use Claude.
        #[arg(long, conflicts_with = "auto")]
        claude: bool,
        /// Route each message to Claude or the local model (the default).
        #[arg(long)]
        auto: bool,
        /// Use this model instead of the configured one (with --claude or --local).
        #[arg(long)]
        model: Option<String>,
        /// Continue a saved conversation: the latest, or the one with this id (or id prefix).
        #[arg(long, value_name = "ID", num_args = 0..=1, default_missing_value = "")]
        resume: Option<String>,
        /// Port on 127.0.0.1 (default 7310; 0 picks a free one).
        #[arg(long)]
        port: Option<u16>,
        /// Open it in the default browser.
        #[arg(long)]
        open: bool,
    },
    /// Headless voice: listen for the wake phrase and speak the answers, with
    /// no screen (see contrib/orion-voice.service).
    Listen {
        /// Always use the model served on this machine.
        #[arg(long, conflicts_with_all = ["claude", "auto"])]
        local: bool,
        /// Always use Claude.
        #[arg(long, conflicts_with = "auto")]
        claude: bool,
        /// Route each request to Claude or the local model (the default).
        #[arg(long)]
        auto: bool,
        /// Use this model instead of the configured one (with --claude or --local).
        #[arg(long)]
        model: Option<String>,
        /// Continue a saved conversation: the latest, or the one with this id (or id prefix).
        #[arg(long, value_name = "ID", num_args = 0..=1, default_missing_value = "")]
        resume: Option<String>,
        /// Don't say "Listening." at the start.
        #[arg(long)]
        quiet: bool,
    },
    /// What the AI remembers about each person, across conversations.
    Memory {
        /// Whose memory (default: yours, the login name).
        #[arg(long)]
        person: Option<String>,
        #[command(subcommand)]
        action: Option<MemoryCmd>,
    },
    /// Speaker verification: enroll your voice, test it, or forget it.
    Voice {
        #[command(subcommand)]
        action: VoiceCmd,
    },
    /// List, show or delete saved chat conversations.
    Conversations {
        #[command(subcommand)]
        action: Option<ConversationCmd>,
    },
    /// Serve the tools over MCP on stdin/stdout (this is what AI clients launch).
    /// When the daemon runs, this bridges to it, so all agents share it.
    Serve {
        /// Serve in this process even if the daemon is running.
        #[arg(long)]
        direct: bool,
    },
    /// Run Orion as a background service that AI agents connect to over a
    /// private Unix socket (see contrib/orion.service).
    Daemon {
        /// Listen here instead of $XDG_RUNTIME_DIR/orion/orion.sock.
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// The privileged helper: run as root, it does Orion's root work and
    /// only that, for members of its group (see contrib/orion-helper.service).
    /// Its policy is /etc/orion/helper.toml, or --config.
    Helper {
        /// Listen here instead of [helper] socket (/run/orion/helper.sock).
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Answer AI agents' approval requests: the tools the policy sets to
    /// "ask", called through the daemon. Prompts in this terminal, or with
    /// --notify as desktop notifications (see contrib/orion-approvals.service).
    Approvals {
        /// Desktop notifications with Approve and Decline, instead of prompts.
        #[arg(long)]
        notify: bool,
        /// Show what waits for an answer, and exit.
        #[arg(long)]
        list: bool,
    },
    /// Show whether the daemon is running, and what it is doing.
    Status,
    /// Show every tool, its risk level, and what the policy decides for it.
    Tools,
    /// Call a tool directly, as the AI would, and print the result.
    Call {
        name: String,
        /// Arguments as a JSON object.
        #[arg(default_value = "{}")]
        args: String,
        /// Who is asking, for the audit log (a timer names itself).
        #[arg(long)]
        caller: Option<String>,
    },
    /// Connect a model: choose Claude, Gemini or a local server, paste the
    /// key, and Orion checks it and writes orion.toml and ~/.config/orion/env.
    /// Then choose what Orion may do without asking.
    Setup {
        /// claude, gemini or local (asked otherwise).
        #[arg(long, value_parser = ["claude", "gemini", "local"])]
        provider: Option<String>,
        /// The model to use (otherwise picked from what the key offers).
        #[arg(long)]
        model: Option<String>,
        /// A local server's OpenAI-compatible URL, such as http://127.0.0.1:8080/v1.
        #[arg(long)]
        base_url: Option<String>,
        /// What Orion may do without asking: ask (ask me everything),
        /// everyday or hands-off. Alone, it changes only that.
        #[arg(long, value_parser = ["ask", "everyday", "hands-off"])]
        trust: Option<String>,
        /// Take every default, and the key already in the environment.
        #[arg(long, short)]
        yes: bool,
    },
    /// Check the policy, keys, models, microphone, transcriber, wake word,
    /// sandbox and helper, and offer to fix what's wrong.
    Doctor {
        /// Apply every fix without asking.
        #[arg(long, short, conflicts_with = "no_fix")]
        yes: bool,
        /// Only report; change nothing.
        #[arg(long)]
        no_fix: bool,
    },
    /// Print the most recent audit log entries, or check the log is intact.
    Audit {
        #[arg(short = 'n', long, default_value_t = 20)]
        lines: usize,
        /// Check every entry's link to the one before, and the privileged
        /// helper's checkpoints; exits 1 if anything was changed.
        #[arg(long)]
        verify: bool,
        /// Have the privileged helper checkpoint the log now (the daemon
        /// does it as the log grows).
        #[arg(long)]
        checkpoint: bool,
    },
}

/// What needs attention, for `orion status`.
fn print_attention(found: &[orion::watch::Finding], note: &str) {
    if found.is_empty() {
        println!("attention:    nothing{note}");
        return;
    }
    println!("attention:    {} thing{}{note}", found.len(), if found.len() == 1 { "" } else { "s" });
    for f in found {
        let mark = if f.severity == "critical" { "!!" } else { "!" };
        println!("  {mark:2} {}{}", f.title, if f.detail.is_empty() { String::new() } else { format!(": {}", f.detail) });
        println!("     {}", f.hint);
    }
}

fn main() -> Result<()> {
    // stdout carries the MCP stream, so all logging goes to stderr.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_env("ORION_LOG").unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(io::stderr)
        .init();

    let cli = Cli::parse();
    // The helper has a policy of its own, which only root may change.
    if let Cmd::Helper { socket } = &cli.command {
        return orion::helper::main(cli.config.as_deref(), socket.as_deref());
    }
    // Keys saved by `orion setup` work at once, without a new shell.
    orion::envfile::load();
    // These two fix a policy that doesn't load, so they don't need it to.
    match cli.command {
        Cmd::Setup { provider, model, base_url, trust, yes } => {
            return orion::setup::run(cli.config.as_deref(), orion::setup::Options { provider, model, base_url, trust, yes });
        }
        Cmd::Doctor { yes, no_fix } => return orion::doctor::run(cli.config.as_deref(), orion::doctor::Options { yes, no_fix }),
        _ => {}
    }
    let (policy, source) = Policy::load(cli.config.as_deref())?;
    match &source {
        Some(path) => tracing::debug!("policy: {}", path.display()),
        None => tracing::debug!("policy: built-in defaults"),
    }

    match cli.command {
        Cmd::Chat { local, claude, auto, model, plain, resume } => {
            let mode = match (local, claude, auto) {
                (true, _, _) => Some(Mode::Local),
                (_, true, _) => Some(Mode::Claude),
                (_, _, true) => Some(Mode::Auto),
                _ => None,
            };
            terminal::run(&Orion::new(policy).through_daemon(source.as_deref(), "orion chat"), mode, model.as_deref(), resume.as_deref(), plain)
        }
        Cmd::Shell { local, claude, auto, model, resume, port, open } => {
            let mode = match (local, claude, auto) {
                (true, _, _) => Some(Mode::Local),
                (_, true, _) => Some(Mode::Claude),
                (_, _, true) => Some(Mode::Auto),
                _ => None,
            };
            terminal::run_shell(&Orion::new(policy).through_daemon(source.as_deref(), "orion shell"), mode, model.as_deref(), resume.as_deref(), port, open)
        }
        Cmd::Listen { local, claude, auto, model, resume, quiet } => {
            let mode = match (local, claude, auto) {
                (true, _, _) => Some(Mode::Local),
                (_, true, _) => Some(Mode::Claude),
                (_, _, true) => Some(Mode::Auto),
                _ => None,
            };
            orion::chat::listen::run(&Orion::new(policy).through_daemon(source.as_deref(), "orion listen"), mode, model.as_deref(), resume.as_deref(), quiet)
        }
        Cmd::Memory { person, action } => terminal::memory(
            person.as_deref(),
            match action {
                None | Some(MemoryCmd::List) => None,
                Some(MemoryCmd::Forget { id }) => Some(("forget", Some(id))),
                Some(MemoryCmd::Clear) => Some(("clear", None)),
            },
        ),
        Cmd::Voice { action } => {
            let v = &policy.voice;
            match action {
                VoiceCmd::Enroll { name } => orion::voice::enroll::enroll(v, name.as_deref()),
                VoiceCmd::Test => orion::voice::enroll::test(v),
                VoiceCmd::Status => orion::voice::enroll::status(v),
                VoiceCmd::Noise => orion::voice::enroll::noise(v),
                VoiceCmd::Forget { name, all } => orion::voice::enroll::forget(name.as_deref(), all),
                VoiceCmd::Model { action } => match action {
                    ModelCmd::Install { from } => orion::voice::enroll::model_install(from.as_deref()),
                    ModelCmd::Status => orion::voice::enroll::model_status(),
                    ModelCmd::Remove => orion::voice::enroll::model_remove(),
                },
            }
        }
        Cmd::Daemon { socket } => daemon::run(policy, source, socket),
        Cmd::Helper { .. } | Cmd::Setup { .. } | Cmd::Doctor { .. } => unreachable!("handled before loading the policy"),
        Cmd::Approvals { notify, list } => {
            let mode = match (list, notify) {
                (true, _) => orion::approvals::Mode::List,
                (_, true) => orion::approvals::Mode::Notify,
                _ => orion::approvals::Mode::Terminal,
            };
            orion::approvals::watch(&policy.daemon, mode)
        }
        Cmd::Status => {
            let path = daemon::socket_path(&policy.daemon);
            let st = match daemon::status(&policy.daemon) {
                Ok(st) => st,
                Err(_) => {
                    println!("Orion daemon: not running ({})", path.display());
                    println!("sandbox:      {}", orion::sandbox::describe(&policy));
                    println!("root access:  {}", orion::tools::root_access(&policy));
                    print_attention(&orion::watch::check(&policy, false), " (quick checks, no updates: the daemon checks those)");
                    let auto = orion::watch::describe_automatic(&policy, orion::watch::last_unattended().as_ref());
                    let note = if policy.packages.automatic == "off" { "" } else { " (the daemon runs them)" };
                    println!("updates:      {auto}{note}");
                    std::process::exit(1);
                }
            };
            let up = st["uptime_secs"].as_u64().unwrap_or(0);
            println!("Orion daemon: running, pid {}, up {}h {}m", st["pid"], up / 3600, up / 60 % 60);
            println!("socket:       {}", st["socket"].as_str().unwrap_or_default());
            let reload = match st["reload_error"].as_str() {
                Some(e) => format!(" (last edit rejected: {e})"),
                None => match st["reloads"].as_u64().unwrap_or(0) {
                    1 => " (1 reload)".into(),
                    n => format!(" ({n} reloads)"),
                },
            };
            println!("policy:       {}{reload}", st["policy"].as_str().unwrap_or_default());
            println!("tools:        {} allowed, {} ask, {} denied", st["tools"]["allow"], st["tools"]["ask"], st["tools"]["deny"]);
            println!("sandbox:      {}", st["sandbox"].as_str().unwrap_or_default());
            // Not counting this status query's own connection.
            let active = st["connections"]["active"].as_u64().unwrap_or(1).saturating_sub(1);
            let total = st["connections"]["total"].as_u64().unwrap_or(1).saturating_sub(1);
            println!("agents:       {active} connected, {total} since start");
            println!("tool calls:   {}", st["tool_calls"]);
            let ap = &st["approvals"];
            if ap["offered"] == true {
                let approvers = ap["approvers"].as_u64().unwrap_or(0);
                let who = if approvers == 0 { "nobody is watching (orion approvals)".to_string() } else { format!("{approvers} watching") };
                println!("approvals:    agents may ask; {who}; {} waiting", ap["waiting"]);
            } else if ap.is_object() {
                println!("approvals:    off: agents get only allowed tools");
            }
            let found: Vec<orion::watch::Finding> = serde_json::from_value(st["health"]["findings"].clone()).unwrap_or_default();
            let checked = st["health"]["checked_at"].as_str().map(|t| format!(" (checked {})", &t[t.len().saturating_sub(8)..])).unwrap_or_default();
            print_attention(&found, &checked);
            let last: Option<orion::watch::Unattended> = serde_json::from_value(st["health"]["unattended"].clone()).ok();
            println!("updates:      {}", orion::watch::describe_automatic(&policy, last.as_ref()));
            if let Some(log) = st["audit_log"].as_str() {
                println!("audit log:    {log}");
            }
            Ok(())
        }
        Cmd::Conversations { action } => terminal::conversations(match &action {
            None | Some(ConversationCmd::List) => None,
            Some(ConversationCmd::Show { id }) => Some(("show", id.as_str())),
            Some(ConversationCmd::Delete { id }) => Some(("delete", id.as_str())),
        }),
        Cmd::Serve { direct } => {
            // An explicit --config means this policy, not the daemon's.
            if !direct && cli.config.is_none() {
                if let Some(stream) = daemon::connect(&policy.daemon) {
                    tracing::debug!("bridging to the Orion daemon");
                    return daemon::bridge(stream);
                }
            }
            let orion = Orion::new(policy);
            tracing::info!(
                policy = %source.as_deref().map_or("built-in defaults".into(), |p| p.display().to_string()),
                tools = orion.tools().filter(|(_, d)| *d == orion::Decision::Allow).count(),
                "orion MCP server ready on stdio"
            );
            Server::new(orion).serve(io::stdin().lock(), io::stdout().lock())
        }
        Cmd::Tools => {
            let orion = Orion::new(policy);
            println!("{:<12} {:<8} {:<6} DESCRIPTION", "TOOL", "RISK", "POLICY");
            for (tool, decision) in orion.tools() {
                let summary = tool.description().split(". ").next().unwrap_or_default().trim_end_matches('.');
                println!("{:<12} {:<8} {:<6} {summary}", tool.name(), tool.risk().as_str(), decision.as_str());
            }
            println!("\nprograms run sandboxed: {}", orion::sandbox::describe(orion.policy()));
            println!("root access for the machine tools: {}", orion::tools::root_access(orion.policy()));
            Ok(())
        }
        Cmd::Call { name, args, caller } => {
            let args = serde_json::from_str(&args).context("arguments must be a JSON object")?;
            let orion = Orion::new(policy);
            let approver = PromptApprover::new();
            let value = orion.call_as(&name, args, approver.as_ref().map(|a| a as &dyn Approver), caller.as_deref())?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            // It ran, but what it drove failed (a package manager partway):
            // exit non-zero, so scripts and services see it.
            if let Some(why) = orion::reported_failure(&value) {
                eprintln!("{name} failed: {why}");
                std::process::exit(1);
            }
            Ok(())
        }
        Cmd::Audit { lines, verify, checkpoint } => {
            let Some(path) = policy.audit_path() else { bail!("audit logging is disabled") };
            // A missing log is checked like an empty one: deleting the whole
            // file is what the checkpoints are there to catch.
            if !path.exists() && !verify {
                println!("no audit entries yet ({})", path.display());
                return Ok(());
            }
            if checkpoint {
                let done = audit::anchor(&policy)?;
                println!("{}", if done { "checkpointed by the helper" } else { "nothing to checkpoint (no new entries, or no helper)" });
                return Ok(());
            }
            if verify {
                let text = match std::fs::read_to_string(&path) {
                    Ok(t) => t,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                    Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
                };
                let anchors_file = audit::anchors_path(&policy.helper.anchors);
                let r = audit::check(&text, &std::fs::read_to_string(&anchors_file).unwrap_or_default());
                println!("audit log:    {} ({} entries)", path.display(), r.lines);
                match r.chained_from {
                    None => println!("chain:        none yet (entries from before chaining)"),
                    Some(1) if r.broken.is_empty() => println!("chain:        intact"),
                    Some(n) if r.broken.is_empty() => println!("chain:        intact from entry {n} (the {} before predate chaining)", n - 1),
                    Some(_) => println!("chain:        BROKEN"),
                }
                for b in &r.broken {
                    println!("  !! {b}");
                }
                match &r.last_anchor {
                    None => println!("checkpoints:  none (the privileged helper keeps them, in {})", anchors_file.display()),
                    Some(last) if r.anchors_bad.is_empty() => println!("checkpoints:  {} by the helper, all hold; the latest {last}", r.anchors),
                    Some(_) => println!("checkpoints:  {} by the helper, {} DON'T HOLD", r.anchors, r.anchors_bad.len()),
                }
                for b in &r.anchors_bad {
                    println!("  !! {b}");
                }
                if !r.broken.is_empty() || !r.anchors_bad.is_empty() {
                    std::process::exit(1);
                }
                return Ok(());
            }
            for line in audit::tail(&path, lines)? {
                println!("{line}");
            }
            Ok(())
        }
    }
}
