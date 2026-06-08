//! End-to-end test for the greplm MCP server.
//!
//! Spawns the real `greplm-mcp` binary and drives it with rmcp's own client over
//! a child-process stdio transport, so the full Model Context Protocol handshake
//! (initialize → tools/list → tools/call) and version negotiation are exercised
//! exactly as a real agent would. This is the contract MCP clients depend on.
//!
//! The child's `XDG_RUNTIME_DIR` is redirected to an isolated, empty directory
//! so the server can't reach a global greplm daemon that may be running on the
//! host — keeping the test hermetic and forcing the in-process query path.

use std::path::Path;

use rmcp::model::CallToolRequestParams;
use rmcp::transport::TokioChildProcess;
use rmcp::ServiceExt;

fn write(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

fn unique_tmp(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let mut p = std::env::temp_dir();
    p.push(format!("greplm-mcp-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn args_object(value: serde_json::Value) -> rmcp::model::JsonObject {
    value
        .as_object()
        .cloned()
        .expect("arguments must be a JSON object")
}

/// Pull the single text payload out of a tool result.
fn result_text(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .find_map(|c| c.as_text().map(|t| t.text.clone()))
        .expect("tool result should carry text content")
}

#[tokio::test]
async fn mcp_server_lists_tools_and_serves_queries() {
    let root = unique_tmp("e2e");
    write(
        &root,
        "src/main.rs",
        "fn main() {\n    let total = compute_sum(1, 2);\n    println!(\"{}\", total);\n}\n\nfn compute_sum(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
    );
    let isolated_runtime = unique_tmp("rt");

    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_greplm-mcp"));
    cmd.arg(&root)
        .env("XDG_RUNTIME_DIR", &isolated_runtime)
        .env("GREPLM_LOG", "off");

    let service =
        ().serve(TokioChildProcess::new(cmd).expect("spawn greplm-mcp"))
            .await
            .expect("MCP initialize handshake should succeed");

    // tools/list must advertise the core toolset.
    let tools = service.list_all_tools().await.expect("list tools");
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    for expected in ["index_project", "search_code", "find_symbol"] {
        assert!(
            names.contains(&expected),
            "tools/list should advertise {expected}, got {names:?}"
        );
    }

    // index_project (force) should succeed and report two files.
    let mut index_params = CallToolRequestParams::default();
    index_params.name = "index_project".into();
    index_params.arguments = Some(args_object(serde_json::json!({ "force": true })));
    let indexed = service
        .call_tool(index_params)
        .await
        .expect("index_project");
    assert_ne!(
        indexed.is_error,
        Some(true),
        "index_project errored: {indexed:?}"
    );
    let stats: serde_json::Value = serde_json::from_str(&result_text(&indexed)).unwrap();
    assert_eq!(stats["files_indexed"], serde_json::json!(1));

    // search_code should find the function across the indexed tree.
    let mut search_params = CallToolRequestParams::default();
    search_params.name = "search_code".into();
    search_params.arguments = Some(args_object(serde_json::json!({ "query": "compute_sum" })));
    let found = service.call_tool(search_params).await.expect("search_code");
    assert_ne!(found.is_error, Some(true), "search_code errored: {found:?}");
    let hits: Vec<serde_json::Value> = serde_json::from_str(&result_text(&found)).unwrap();
    assert!(
        hits.iter().any(|h| h["path"] == "src/main.rs"),
        "search_code should find compute_sum in src/main.rs, got {hits:?}"
    );

    // find_symbol (exact) should resolve the definition.
    let mut symbol_params = CallToolRequestParams::default();
    symbol_params.name = "find_symbol".into();
    symbol_params.arguments = Some(args_object(
        serde_json::json!({ "name": "compute_sum", "exact": true }),
    ));
    let sym = service.call_tool(symbol_params).await.expect("find_symbol");
    let syms: Vec<serde_json::Value> = serde_json::from_str(&result_text(&sym)).unwrap();
    assert!(
        syms.iter()
            .any(|s| s["name"] == "compute_sum" && s["path"] == "src/main.rs"),
        "find_symbol should resolve compute_sum, got {syms:?}"
    );

    service.cancel().await.ok();
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&isolated_runtime).ok();
}
