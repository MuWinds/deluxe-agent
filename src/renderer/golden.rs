//! Renderer tests: the bundled Wasm Component must turn a message or a tool
//! call into a display list the host protocol accepts, and it must load through
//! the plugin platform like any other built-in plugin.
//!
//! The guest owns all Markdown and tool-card shaping now, so there is no native
//! parser left to compare against. These tests instead pin the *shape* of what
//! comes back — a table is a grid, a card is a fold, the text is all there — so
//! a guest rewrite that quietly drops content is caught here.

use std::sync::Arc;

use tokio::sync::RwLock;

use super::protocol::{
    self, HunkLines, Node, RenderKind, RenderMetrics, ToolRenderRequest, ToolRenderResult,
};
use crate::plugins::capabilities::CapabilityHub;
use crate::plugins::wasm_runtime::{ComponentActor, Operation};
use crate::tools::{ToolRegistry, ToolSettings};

/// Loads the bundled renderer through the plugin platform, exactly as the
/// worker does. Its hub is inert: the renderer imports no host capability.
async fn actor() -> Arc<ComponentActor> {
    let package = crate::plugins::defaults::PLUGINS
        .iter()
        .find(|package| package.name == "transcript-renderer")
        .expect("the application ships a bundled transcript renderer");
    let root = std::env::temp_dir();
    let hub = CapabilityHub::new(
        root.clone(),
        root,
        Default::default(),
        Arc::new(crate::harness::services::RegistryToolRuntime::new(
            Arc::new(ToolRegistry::with_builtins()),
            Arc::new(RwLock::new(ToolSettings::default())),
        )),
    )
    .expect("the renderer's capabilities build");
    ComponentActor::load_bytes(package.component, hub)
        .await
        .expect("the bundled renderer loads as a plugin")
}

fn metrics() -> RenderMetrics {
    RenderMetrics {
        available_width: 820.0,
        char_width: 8.0,
        column_gap: 12.0,
    }
}

const MESSAGE_FIXTURE: &str = "# Title\n\nSome **bold** and *italic* and `code` and a \
[link](https://example.com).\n\n- one\n- two\n  - nested\n\n1. first\n2. second\n\n> quoted\n\n\
```rust\nlet x = 1;\n```\n\n| a | b |\n| --- | :-: |\n| 1 | 2 |\n";

/// Every run of text under `nodes`, concatenated, for a coarse content check.
fn collect_text(nodes: &[Node], out: &mut String) {
    for node in nodes {
        match node {
            Node::Text { runs, .. } => {
                for run in runs {
                    out.push_str(&run.text);
                }
            }
            Node::Column { children } | Node::Row { children, .. } => collect_text(children, out),
            Node::Indent { child, .. }
            | Node::Frame { child, .. }
            | Node::Scroll { child, .. }
            | Node::Align { child, .. } => collect_text(std::slice::from_ref(child), out),
            Node::Collapse { header, body, .. } => {
                for run in header {
                    out.push_str(&run.text);
                }
                collect_text(body, out);
            }
            Node::Grid { header, rows, .. } => {
                for cell in header {
                    for run in cell {
                        out.push_str(&run.text);
                    }
                }
                for row in rows {
                    for cell in row {
                        for run in cell {
                            out.push_str(&run.text);
                        }
                    }
                }
            }
            Node::Icon { .. }
            | Node::CopyButton { .. }
            | Node::Rule
            | Node::Spacer { .. }
            | Node::Grow => {}
        }
    }
}

/// Whether any node in the tree satisfies `pred`.
fn has_node(nodes: &[Node], pred: &impl Fn(&Node) -> bool) -> bool {
    nodes.iter().any(|node| {
        if pred(node) {
            return true;
        }
        match node {
            Node::Column { children } | Node::Row { children, .. } => has_node(children, pred),
            Node::Indent { child, .. }
            | Node::Frame { child, .. }
            | Node::Scroll { child, .. }
            | Node::Align { child, .. } => has_node(std::slice::from_ref(child), pred),
            Node::Collapse { body, .. } => has_node(body, pred),
            _ => false,
        }
    })
}

/// The renderer is an ordinary plugin: it loads, and it contributes no tools,
/// no event handlers, and no UI — only its render entry points.
#[tokio::test]
async fn the_bundled_renderer_is_a_plugin_with_no_tools_or_handlers() {
    let actor = actor().await;
    assert_eq!(
        actor.call(Operation::ListTools).await.expect("lists tools"),
        "[]",
        "the renderer contributes no tools",
    );
    assert_eq!(
        actor
            .call(Operation::ListEventHandlers)
            .await
            .expect("lists handlers"),
        "[]",
        "the renderer contributes no event handlers",
    );
}

#[tokio::test]
async fn a_message_renders_to_a_display_list() {
    let actor = actor().await;
    let json = actor
        .call(Operation::RenderMessage(protocol::message_request(
            1,
            MESSAGE_FIXTURE,
            metrics(),
        )))
        .await
        .expect("renders");
    let nodes = protocol::decode(&json, 1, RenderKind::Message).expect("valid response");
    assert!(!nodes.is_empty(), "a non-empty message renders to nodes");

    let mut text = String::new();
    collect_text(&nodes, &mut text);
    for expected in [
        "Title",
        "bold",
        "italic",
        "code",
        "link",
        "one",
        "nested",
        "first",
        "quoted",
        "let x = 1;",
        "a",
        "b",
    ] {
        assert!(
            text.contains(expected),
            "expected `{expected}` in the rendered text, got `{text}`"
        );
    }
    assert!(
        has_node(&nodes, &|node| matches!(node, Node::Grid { .. })),
        "a Markdown table renders as a grid"
    );
}

#[tokio::test]
async fn a_tool_renders_to_a_display_list() {
    let actor = actor().await;
    let patch = "*** Begin Patch\n*** Update File: src/main.rs\n@@\n-old\n+new\n*** End Patch";
    let arguments = serde_json::json!({ "patch": patch });
    let result = ToolRenderResult {
        outcome: "executed".into(),
        output: "Applied 1 patch operation".into(),
        hunks: vec![HunkLines {
            path: "src/main.rs".into(),
            lines: vec![Some(40), Some(41), Some(41)],
        }],
        duration_ms: 12,
    };
    let request = ToolRenderRequest::new(1, "apply_patch", arguments, Some(result), metrics());
    let json = serde_json::to_string(&request).expect("request serializes");
    let response = actor
        .call(Operation::RenderTool(json))
        .await
        .expect("renders");
    let nodes = protocol::decode(&response, 1, RenderKind::Tool).expect("valid response");

    let mut text = String::new();
    collect_text(&nodes, &mut text);
    assert!(
        text.contains("src/main.rs"),
        "the card names the file it patched, got `{text}`"
    );
    assert!(
        text.contains("+ new") && text.contains("- old"),
        "the card shows the diff, got `{text}`"
    );
    assert!(
        has_node(&nodes, &|node| matches!(node, Node::Collapse { .. })),
        "the card is a fold"
    );
}

/// A burst of render requests larger than the actor's queue capacity must all
/// be served. The GUI dispatches one request per transcript item in a single
/// frame, so a session longer than the queue would otherwise lose its newest
/// items to a non-blocking send and leave them on the fallback forever.
#[tokio::test]
async fn a_burst_of_renders_larger_than_the_queue_all_succeed() {
    let actor = actor().await;
    let mut tasks = Vec::new();
    for revision in 0..64u64 {
        let actor = actor.clone();
        tasks.push(tokio::spawn(async move {
            actor
                .call(Operation::RenderMessage(protocol::message_request(
                    revision,
                    "hello",
                    metrics(),
                )))
                .await
        }));
    }
    let mut failures = 0;
    for task in tasks {
        if task.await.expect("task").is_err() {
            failures += 1;
        }
    }
    assert_eq!(failures, 0, "every render in the burst should be served");
}

#[tokio::test]
async fn a_guest_error_does_not_poison_the_actor() {
    let actor = actor().await;
    // Malformed request JSON: the guest returns `Err`, which is normal control
    // flow and must leave the Store usable.
    assert!(actor
        .call(Operation::RenderMessage("not json".into()))
        .await
        .is_err());
    let json = actor
        .call(Operation::RenderMessage(protocol::message_request(
            2,
            "hi",
            metrics(),
        )))
        .await
        .expect("the actor still works");
    assert!(protocol::decode(&json, 2, RenderKind::Message).is_ok());
}
