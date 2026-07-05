# Genesis (clean-room)

**All-new code.** This workspace is the from-scratch Genesis implementation — it
shares no code with the rebranded upstream fork living at the root of this
repository. The fork remains in place as the shipping product and as the
reference for behavior; this tree replaces it piece by piece as the clean-room
implementation reaches parity.

## What works today (v0)

A provider-neutral AI agent engine and CLI:

- **Providers** — Anthropic Messages API and OpenAI Chat Completions, behind a
  single `Provider` trait. One OpenAI implementation also drives Ollama, vLLM,
  LM Studio and other compatible servers. Provider differences are expressed
  through `Compat` configuration presets, never hardcoded conditionals — the
  same rule the full Genesis architecture is built on.
- **Tools** — `read_file`, `write_file`, `edit_file` (unique exact-match
  replace), `glob`, `grep`, and `bash` (timeout-bounded, workspace-rooted).
  File tools are confined to the workspace root; every tool's output is capped
  before it reaches the model.
- **Agent loop** — completion → tool runs → tool results → repeat, with a
  turn cap, cumulative token accounting, and host-facing events
  (`Text` / `ToolStart` / `ToolEnd`) so a GUI can render progress the same way
  the CLI does.
- **Config** — CLI flags → `GENESIS_*` env vars → `~/.genesis/config.toml` →
  defaults. API keys via `GENESIS_API_KEY`, `ANTHROPIC_API_KEY`, or
  `OPENAI_API_KEY`.

## Try it

```bash
cd genesis
cargo build --release

export ANTHROPIC_API_KEY=sk-ant-...
./target/release/genesis "read Cargo.toml and explain this workspace"

# Ollama, no key needed:
./target/release/genesis -p openai_compatible -m llama3 "summarize README.md"

# No prompt → interactive session
./target/release/genesis
```

## Layout

```
crates/
  genesis-engine   types · error · shell · provider/ · tools/ · agent · config
  genesis-cli      the `genesis` binary
```

The engine is a library first: the CLI is ~200 lines on top of it, and a
desktop host embeds the same `Agent` + `AgentEvent` surface.

## Roadmap to parity

Roughly in order; each stage lands with tests:

1. Streaming responses (SSE) and a `LlmEvent` stream surface.
2. Tool approval policy (ask / allowlist / yolo) + session persistence.
3. JSON-Lines host protocol for the desktop app.
4. MCP client (stdio transport first).
5. Skills, hooks, memory.
6. Sandboxed shell execution, egress chokepoint, additional providers
   (Bedrock, Vertex).

## Development

```bash
cargo test        # unit + integration tests (no network, no API keys needed)
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```
