use std::io;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use orion::chat::terminal::{self, PromptApprover};
use orion::chat::Mode;
use orion::mcp::Server;
use orion::{audit, Approver, Orion, Policy};

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
    /// List, show or delete saved chat conversations.
    Conversations {
        #[command(subcommand)]
        action: Option<ConversationCmd>,
    },
    /// Serve the tools over MCP on stdin/stdout (this is what AI clients launch).
    Serve,
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
        Cmd::Conversations { action } => terminal::conversations(match &action {
            None | Some(ConversationCmd::List) => None,
            Some(ConversationCmd::Show { id }) => Some(("show", id.as_str())),
            Some(ConversationCmd::Delete { id }) => Some(("delete", id.as_str())),
        }),
        Cmd::Serve => {
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
