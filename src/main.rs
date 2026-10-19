use std::io;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use orion::chat::terminal::{self, PromptApprover};
use orion::chat::Provider;
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
enum Cmd {
    /// Talk to an AI in the terminal; it works on the machine through Orion's tools.
    Chat {
        /// Use a model served on this machine instead of Claude.
        #[arg(long)]
        local: bool,
        /// Use this model instead of the configured one.
        #[arg(long)]
        model: Option<String>,
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
        Cmd::Chat { local, model } => {
            let provider = local.then_some(Provider::Local);
            terminal::run(&Orion::new(policy), provider, model.as_deref())
        }
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
