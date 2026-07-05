//! Genesis CLI — one-shot prompts or an interactive session in the terminal.

use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use genesis_engine::agent::{Agent, AgentConfig, AgentEvent};
use genesis_engine::config::{GenesisConfig, ProviderKind};
use genesis_engine::provider::{AnthropicProvider, Compat, OpenAiProvider, Provider};
use genesis_engine::tools::ToolRegistry;

#[derive(Parser)]
#[command(
    name = "genesis",
    version,
    about = "Genesis — a provider-neutral AI agent for your terminal"
)]
struct Cli {
    /// The prompt to run. Omit it for an interactive session.
    prompt: Vec<String>,

    /// Provider: anthropic, openai, or openai_compatible.
    #[arg(short, long)]
    provider: Option<String>,

    /// Model identifier (defaults per provider).
    #[arg(short, long)]
    model: Option<String>,

    /// Workspace root the agent operates in (defaults to the current directory).
    #[arg(short, long)]
    workspace: Option<PathBuf>,

    /// Config file path (defaults to ~/.genesis/config.toml).
    #[arg(long)]
    config: Option<PathBuf>,
}

fn build_provider(config: &GenesisConfig) -> Box<dyn Provider> {
    match config.provider {
        ProviderKind::Anthropic => Box::new(match &config.base_url {
            Some(url) => AnthropicProvider::with_base_url(&config.api_key, url),
            None => AnthropicProvider::new(&config.api_key),
        }),
        ProviderKind::Openai => Box::new(match &config.base_url {
            Some(url) => OpenAiProvider::with_config(&config.api_key, url, Compat::openai()),
            None => OpenAiProvider::new(&config.api_key),
        }),
        ProviderKind::OpenaiCompatible => {
            let url = config
                .base_url
                .as_deref()
                .unwrap_or("http://localhost:11434/v1");
            Box::new(OpenAiProvider::with_config(
                &config.api_key,
                url,
                Compat::openai_compatible(),
            ))
        }
    }
}

fn print_event(event: AgentEvent) {
    match event {
        AgentEvent::Text(text) => println!("{text}"),
        AgentEvent::ToolStart { name, input } => {
            let compact = serde_json::to_string(&input).unwrap_or_default();
            let shown: String = compact.chars().take(120).collect();
            eprintln!("  ⚙ {name} {shown}");
        }
        AgentEvent::ToolEnd {
            name,
            output,
            is_error,
        } => {
            let first_line = output.lines().next().unwrap_or("");
            let shown: String = first_line.chars().take(120).collect();
            if is_error {
                eprintln!("  ✗ {name}: {shown}");
            } else {
                eprintln!("  ✓ {name}: {shown}");
            }
        }
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();

    let config_path = cli.config.clone().or_else(GenesisConfig::default_path);
    let config = GenesisConfig::load(
        config_path.as_deref(),
        cli.provider.as_deref(),
        cli.model.as_deref(),
    )?;

    let workspace = cli
        .workspace
        .clone()
        .unwrap_or(std::env::current_dir()?)
        .canonicalize()
        .context("workspace directory does not exist")?;

    let provider = build_provider(&config);
    let tools = ToolRegistry::builtin(&workspace);
    let mut agent = Agent::new(
        provider,
        tools,
        AgentConfig {
            model: config.model.clone(),
            max_tokens: config.max_tokens,
            max_turns: config.max_turns,
            ..AgentConfig::default()
        },
    );

    eprintln!(
        "genesis · provider={} model={} workspace={}",
        agent_provider_name(&config.provider),
        config.model,
        workspace.display()
    );

    if cli.prompt.is_empty() {
        interactive(&mut agent).await
    } else {
        let prompt = cli.prompt.join(" ");
        agent.run(&prompt, print_event).await?;
        report_usage(&agent);
        Ok(())
    }
}

fn agent_provider_name(kind: &ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::Openai => "openai",
        ProviderKind::OpenaiCompatible => "openai_compatible",
    }
}

fn report_usage(agent: &Agent<Box<dyn Provider>>) {
    let usage = agent.usage();
    eprintln!(
        "· {} input / {} output tokens",
        usage.input_tokens, usage.output_tokens
    );
}

async fn interactive(agent: &mut Agent<Box<dyn Provider>>) -> Result<()> {
    eprintln!("interactive session — empty line or Ctrl-D to exit");
    let stdin = io::stdin();
    loop {
        eprint!("genesis> ");
        io::stderr().flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break;
        }
        let prompt = line.trim();
        if prompt.is_empty() {
            break;
        }
        if let Err(e) = agent.run(prompt, print_event).await {
            eprintln!("error: {e}");
        }
        report_usage(agent);
    }
    Ok(())
}
