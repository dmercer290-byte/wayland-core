//! v0.6.4 Task 2.4: smoke test for the `mcp-serve` subcommand glue.
//!
//! Verifies that:
//!   1. `tool_registry_to_server_specs` produces a non-empty `Vec<ServerToolSpec>`
//!      whose entries mirror the names/descriptions of the registered tools.
//!   2. An `McpServer` constructed from those specs reports the tool through
//!      `tools/list` (proving the adapter is wired through the assembled server,
//!      not just tested in isolation).
//!
//! Drives the server in-process via `McpServer::handle_request` rather than
//! spinning up stdio/SSE — the transports already have full coverage in
//! `wcore-mcp` and we don't want this test to depend on a port or stdin pipe.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use wcore_agent::policy_gate::PolicyGate;
use wcore_cli::mcp_serve::{build_server, tool_registry_to_server_specs};
use wcore_mcp::{AllowAll, McpServer, ServerJsonRpcRequest};
use wcore_permissions::{Action, Actor, Permission, PolicyEngine, Resource};
use wcore_protocol::events::ToolCategory;
use wcore_tools::Tool;
use wcore_tools::registry::ToolRegistry;
use wcore_types::tool::{JsonSchema, ToolResult};

/// Minimal stand-in tool — just enough to satisfy `Tool` so it can be
/// registered and surfaced through the adapter. Execute path is never
/// exercised here; this test only inspects the `tools/list` capability
/// advertisement.
struct FakeTool;

#[async_trait]
impl Tool for FakeTool {
    fn name(&self) -> &str {
        "fake_smoke_tool"
    }
    fn description(&self) -> &str {
        "smoke-test tool used to verify mcp-serve adapter wiring"
    }
    fn input_schema(&self) -> JsonSchema {
        json!({
            "type": "object",
            "properties": {
                "msg": { "type": "string" }
            },
            "required": ["msg"]
        })
    }
    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }
    async fn execute(&self, input: Value) -> ToolResult {
        // Echo the received msg so the end-to-end `tools/call` test can
        // prove the executor forwarded the arguments to the real tool.
        let msg = input
            .get("msg")
            .and_then(|v| v.as_str())
            .unwrap_or("<none>");
        ToolResult {
            content: format!("echoed:{msg}"),
            is_error: false,
        }
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Info
    }
}

#[tokio::test]
async fn tools_list_includes_registered_fake_tool() {
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(FakeTool));

    let specs = tool_registry_to_server_specs(&registry);
    assert!(
        !specs.is_empty(),
        "adapter must surface registered tools (registry has 1, got {})",
        specs.len()
    );
    assert!(
        specs.iter().any(|s| s.name == "fake_smoke_tool"),
        "adapter output missing fake_smoke_tool: got {:?}",
        specs.iter().map(|s| &s.name).collect::<Vec<_>>()
    );

    let server = McpServer::new(specs, Box::new(AllowAll));
    let req = ServerJsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!(1)),
        method: "tools/list".into(),
        params: None,
    };
    let resp = server.handle_request(req).await;
    let result = resp.result.expect("tools/list returns result");
    let tools = result["tools"].as_array().expect("tools is an array");
    assert!(
        !tools.is_empty(),
        "assembled server must advertise the adapter-derived tools (was empty)"
    );
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(
        names.contains(&"fake_smoke_tool"),
        "assembled server tools/list missing fake_smoke_tool: {names:?}"
    );

    // Spot-check the inputSchema round-trips intact through the adapter.
    let entry = tools
        .iter()
        .find(|t| t["name"] == "fake_smoke_tool")
        .expect("entry");
    assert_eq!(entry["inputSchema"]["type"], "object");
    assert_eq!(entry["inputSchema"]["required"][0], "msg");
}

/// End-to-end wiring proof through the higher crate: `build_server`
/// installs a `RegistryToolExecutor` over the real `ToolRegistry`, so a
/// `tools/call` for a granted, registered tool actually EXECUTES that tool
/// and returns its `ToolResult` as the MCP result envelope — not a
/// `NOT_IMPLEMENTED` stub.
#[tokio::test]
async fn tools_call_executes_registered_tool_end_to_end() {
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(FakeTool));

    // Grant the fake tool so the policy gate permits the call.
    let mut engine = PolicyEngine::new();
    engine.grant(Permission {
        actor: Actor::User("mcp-serve".into()),
        resource: Resource::Tool("fake_smoke_tool".into()),
        action: Action::Invoke,
    });
    let gate = PolicyGate::new(Arc::new(engine), Actor::User("mcp-serve".into()));

    let server = build_server(registry, gate);

    let req = ServerJsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!(7)),
        method: "tools/call".into(),
        params: Some(json!({
            "name": "fake_smoke_tool",
            "arguments": { "msg": "hello" }
        })),
    };
    let resp = server.handle_request(req).await;

    assert!(
        resp.error.is_none(),
        "tools/call must succeed for a granted, registered tool, got error: {:?}",
        resp.error
    );
    let result = resp.result.expect("tools/call returns a result on success");
    assert_eq!(result["isError"], false);
    let text = result["content"][0]["text"]
        .as_str()
        .expect("content[0].text is a string");
    assert_eq!(
        text, "echoed:hello",
        "the real tool executed and its output flowed back through the executor"
    );
}
