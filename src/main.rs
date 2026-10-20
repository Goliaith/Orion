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
enum VoiceCmd {
    /// Teach Orion your voice, so only you can wake it.
    Enroll,
    /// Say something and see whether Orion takes it for your voice.
    Test,
    /// Whether a voice is enrolled.
    Status,
    /// Delete the voiceprint: anyone can wake Orion again.
    Forget,
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
    },
    /// Print the most recent audit log entries.
    Audit {
        #[arg(short = 'n', long, default_value_t = 20)]
        lines: usize,
    },
}

fn main() -> Result<()> {
    // stdout carries the MCP stream, so all logging goes to stderr.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_env("ORION_LOG").unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(io::stderr)
        .init();

    let cli = Cli::parse();
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
            terminal::run(&Orion::new(policy), mode, model.as_deref(), resume.as_deref(), plain)
        }
        Cmd::Shell { local, claude, auto, model, resume, port, open } => {
            let mode = match (local, claude, auto) {
                (true, _, _) => Some(Mode::Local),
                (_, true, _) => Some(Mode::Claude),
                (_, _, true) => Some(Mode::Auto),
                _ => None,
            };
            terminal::run_shell(&Orion::new(policy), mode, model.as_deref(), resume.as_deref(), port, open)
        }
        Cmd::Listen { local, claude, auto, model, resume, quiet } => {
            let mode = match (local, claude, auto) {
                (true, _, _) => Some(Mode::Local),
                (_, true, _) => Some(Mode::Claude),
                (_, _, true) => Some(Mode::Auto),
                _ => None,
            };
            orion::chat::listen::run(&Orion::new(policy), mode, model.as_deref(), resume.as_deref(), quiet)
        }
        Cmd::Voice { action } => {
            let v = &policy.voice;
            match action {
                VoiceCmd::Enroll => orion::voice::enroll::enroll(v),
                VoiceCmd::Test => orion::voice::enroll::test(v),
                VoiceCmd::Status => orion::voice::enroll::status(v),
                VoiceCmd::Forget => orion::voice::enroll::forget(),
                VoiceCmd::Model { action } => match action {
                    ModelCmd::Install { from } => orion::voice::enroll::model_install(from.as_deref()),
                    ModelCmd::Status => orion::voice::enroll::model_status(),
                    ModelCmd::Remove => orion::voice::enroll::model_remove(),
                },
            }
        }
        Cmd::Daemon { socket } => daemon::run(policy, source, socket),
        Cmd::Status => {
            let path = daemon::socket_path(&policy.daemon);
            let st = match daemon::status(&policy.daemon) {
                Ok(st) => st,
                Err(_) => {
                    println!("Orion daemon: not running ({})", path.display());
                    println!("sandbox:      {}", orion::sandbox::describe(&policy));
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
            Ok(())
        }
        Cmd::Call { name, args } => {
            let args = serde_json::from_str(&args).context("arguments must be a JSON object")?;
            let orion = Orion::new(policy);
            let approver = PromptApprover::new();
            let value = orion.call(&name, args, approver.as_ref().map(|a| a as &dyn Approver))?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        Cmd::Audit { lines } => {
            let Some(path) = policy.audit_path() else { bail!("audit logging is disabled") };
            if !path.exists() {
                println!("no audit entries yet ({})", path.display());
                return Ok(());
            }
            for line in audit::tail(&path, lines)? {
                println!("{line}");
            }
            Ok(())
        }
    }
}
