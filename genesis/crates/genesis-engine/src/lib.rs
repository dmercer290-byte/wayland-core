//! Genesis engine — a clean-room, provider-neutral AI agent engine.
//!
//! Layering (dependencies flow downward only):
//!
//! - [`types`] — provider-neutral message / tool / request / response types.
//! - [`error`] — the engine's public error type.
//! - [`shell`] — the single cross-platform process-spawning chokepoint.
//! - [`provider`] — the [`provider::Provider`] trait plus Anthropic and
//!   OpenAI-compatible implementations. Provider differences are expressed
//!   through [`provider::Compat`] configuration, never hardcoded conditionals.
//! - [`tools`] — the [`tools::Tool`] trait and the built-in tool set
//!   (read_file, write_file, edit_file, bash, glob, grep).
//! - [`agent`] — the tool-use loop that ties a provider and a tool registry
//!   together into a working agent.
//! - [`config`] — configuration loading (`~/.genesis/config.toml` + env).

pub mod agent;
pub mod config;
pub mod error;
pub mod provider;
pub mod shell;
pub mod tools;
pub mod types;
