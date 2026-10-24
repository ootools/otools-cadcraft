//! 上游 `cadcraft-mcp` 与宿主 MCP 契约之间的薄适配。
//!
//! 上游的 `tool_definitions` / `call_tool` 是 crate-private，公开入口只有
//! `Server`。这里让一个专用线程持有唯一 `Server`，宿主调用经 channel 串行进入，
//! 因此 `new_drawing → execute → inspect` 共享同一份状态，同时工具描述、参数校验
//! 和错误格式全部由上游决定。
//!
//! **不做 UI-only 工具过滤**：headless 的工具表本来就不含 UI 工具；若上游未来
//! 改成运行时判断，也应原样透传，让调用点给出可读错误。

use cadcraft_engine::cmd::file::{set_entity_codec, set_io, set_write_as, EntityCodec, IoHooks};
use cadcraft_mcp::{Headless, Server};
use serde_json::{json, Value};

enum WorkerRequest {
    Mcp {
        method: String,
        params: Value,
        reply: std::sync::mpsc::Sender<Result<Value, String>>,
    },
    Reset {
        reply: std::sync::mpsc::Sender<Result<Value, String>>,
    },
}

/// 上游 `Server` 拥有 `Box<dyn Backend>`，而该 trait object 没有 `Send` bound。
/// 所以让它固定在一个专用线程里，宿主线程只通过 channel 发请求。这样：
/// - 会话跨调用持久（`new_drawing → execute → inspect` 是同一个 Server）；
/// - 并发调用被自然串行化，不会同时改同一个 Session；
/// - 不需要对非 `Send` 类型做 `unsafe impl Send`。
static WORKER: std::sync::OnceLock<std::sync::Mutex<std::sync::mpsc::Sender<WorkerRequest>>> =
    std::sync::OnceLock::new();

/// 安装与上游 CLI/桌面端一致的 I/O hook。`set_*` 是幂等的（OnceLock）。
fn install_io() {
    set_io(IoHooks {
        read: |bytes, name| cadcraft_io::read(bytes, name).map_err(|e| e.to_string()),
        write: |drawing, name| cadcraft_io::write(drawing, name).map_err(|e| e.to_string()),
        plot: Some(|drawing, space, opts| {
            cadcraft_io::plotter::plot(drawing, space, opts).map_err(|e| e.to_string())
        }),
    });
    set_write_as(|drawing, name, opts| {
        cadcraft_io::write_with(
            drawing,
            name,
            &cadcraft_io::SaveOptions {
                version: opts.version,
                binary: opts.binary,
            },
        )
        .map_err(|e| e.to_string())
    });
    set_entity_codec(EntityCodec {
        groups: cadcraft_io::entity_groups,
        entity: cadcraft_io::entity_from_groups,
    });
}

fn worker() -> &'static std::sync::Mutex<std::sync::mpsc::Sender<WorkerRequest>> {
    WORKER.get_or_init(|| {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("cadcraft-mcp".to_string())
            .spawn(move || worker_loop(receiver))
            .expect("failed to start cadcraft MCP worker");
        std::sync::Mutex::new(sender)
    })
}

fn worker_loop(receiver: std::sync::mpsc::Receiver<WorkerRequest>) {
    install_io();
    let mut server = Server::new(Box::new(Headless::default()));
    while let Ok(message) = receiver.recv() {
        match message {
            WorkerRequest::Mcp {
                method,
                params,
                reply,
            } => {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    request(&mut server, &method, params)
                }));
                let result = match result {
                    Ok(result) => result,
                    Err(panic) => {
                        // Only a panic can leave the backend half-mutated. A normal tool
                        // error is part of the session (e.g. a rejected command), so keep
                        // the drawing/undo state and let the next call continue.
                        server = Server::new(Box::new(Headless::default()));
                        Err(panic_message(panic))
                    }
                };
                let _ = reply.send(result);
            }
            WorkerRequest::Reset { reply } => {
                server = Server::new(Box::new(Headless::default()));
                let _ = reply.send(Ok(json!({ "ok": true })));
            }
        }
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string())
}

fn invoke_worker(method: &str, params: Value) -> Result<Value, String> {
    let sender = worker()
        .lock()
        .map_err(|_| "MCP worker channel is poisoned".to_string())?;
    let (reply, response) = std::sync::mpsc::channel();
    sender
        .send(WorkerRequest::Mcp {
            method: method.to_string(),
            params,
            reply,
        })
        .map_err(|_| "MCP worker is not running".to_string())?;
    response
        .recv()
        .map_err(|_| "MCP worker stopped before replying".to_string())?
}

fn request(server: &mut Server, method: &str, params: Value) -> Result<Value, String> {
    let line = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    })
    .to_string();
    let reply = server
        .handle_line(&line)
        .ok_or_else(|| format!("MCP method `{method}` did not return a response"))?;
    let mut value: Value = serde_json::from_str(&reply)
        .map_err(|error| format!("MCP response is not JSON: {error}"))?;
    if let Some(error) = value.get_mut("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("upstream MCP error")
            .to_string();
        return Err(message);
    }
    Ok(value
        .get_mut("result")
        .map(Value::take)
        .unwrap_or(Value::Null))
}

/// Upstream definitions with host-facing MCP annotations.
///
/// CADCraft's upstream table predates MCP annotations. These are static hints about
/// the tool's contract: reads leave the drawing untouched; `execute` / `command_line`
/// / `script` may mutate it, and `save` writes a file. They only drive grouping and
/// audit labeling in the host, never authorization.
fn annotate_tools(mut value: Value) -> Value {
    let Some(tools) = value.get_mut("tools").and_then(Value::as_array_mut) else {
        return value;
    };
    for tool in tools {
        let Some(name) = tool.get("name").and_then(Value::as_str) else {
            continue;
        };
        let (read_only, destructive) = match name {
            "list_commands" | "inspect_drawing" | "query_entities" | "find_text" | "render" => {
                (true, false)
            }
            "save" => (false, true),
            "execute" | "command_line" | "script" => (false, true),
            _ => (false, false),
        };
        if let Some(object) = tool.as_object_mut() {
            object.insert(
                "annotations".to_string(),
                json!({
                    "readOnlyHint": read_only,
                    "destructiveHint": destructive,
                }),
            );
        }
    }
    value
}

pub fn tools_payload() -> Value {
    invoke_worker("tools/list", json!({}))
        .map(annotate_tools)
        .unwrap_or_else(|error| {
            json!({
                "tools": [],
                "error": error,
            })
        })
}

pub fn call_payload(name: &str, arguments: &Value) -> Value {
    match invoke_worker(
        "tools/call",
        json!({ "name": name, "arguments": arguments }),
    ) {
        Ok(value) => value,
        Err(error) => error_result(&error),
    }
}

fn error_result(message: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": message }],
        "isError": true,
    })
}

/// 上游 `resources/list`。分发在 worker 线程里由上游 `Server` 处理，
/// 因此 URI、name、description 与上游逐字一致，适配层不做任何复制。
pub fn resources_payload() -> Result<Value, String> {
    invoke_worker("resources/list", json!({}))
}

/// 上游 `resources/templates/list`。
///
/// CAD 上游**没有**这个分支（`Server::handle` 会回 method not found），但协议要求
/// 客户端问到就得答得上。这里直接给空表：CAD 的资源是固定两个 URI，没有模板。
pub fn resource_templates_payload() -> Result<Value, String> {
    Ok(json!({ "resourceTemplates": [] }))
}

/// 上游 `resources/read`。未知 uri 由上游给出 `-32002`，这里转成 `Err`。
pub fn read_resource_payload(uri: &str) -> Result<Value, String> {
    invoke_worker("resources/read", json!({ "uri": uri }))
}

pub fn shutdown_headless() {
    let Ok(sender) = worker().lock() else {
        return;
    };
    let (reply, response) = std::sync::mpsc::channel();
    if sender.send(WorkerRequest::Reset { reply }).is_ok() {
        let _ = response.recv();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn tools_payload_contains_upstream_tools() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let value = tools_payload();
        let tools = value["tools"].as_array().expect("tools array");
        assert!(tools.len() >= 10, "got {}", tools.len());
        assert!(tools.iter().any(|t| t["name"] == "execute"));
    }

    #[test]
    fn tools_payload_marks_read_only_and_destructive_tools() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let value = tools_payload();
        let tools = value["tools"].as_array().expect("tools array");
        let find = |name: &str| {
            tools
                .iter()
                .find(|tool| tool["name"] == name)
                .unwrap_or_else(|| panic!("{name} missing"))
        };
        assert_eq!(find("inspect_drawing")["annotations"]["readOnlyHint"], true);
        assert_eq!(find("execute")["annotations"]["readOnlyHint"], false);
        assert_eq!(find("save")["annotations"]["destructiveHint"], true);
        assert_eq!(find("render")["annotations"]["readOnlyHint"], true);
    }

    #[test]
    fn call_unknown_tool_returns_is_error() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let result = call_payload("definitely_not_a_tool", &json!({}));
        assert_eq!(result["isError"], true, "result: {result}");
    }

    #[test]
    fn tool_error_does_not_reset_session() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        shutdown_headless();
        let drawn = call_payload(
            "execute",
            &json!({"command": "line", "params": {"points": [[0, 0], [10, 0]]}}),
        );
        assert_ne!(drawn["isError"], true, "draw failed: {drawn}");

        // A valid upstream tool called with bad arguments is a tool error, not a
        // backend panic. It must not discard the drawing or other session state.
        let bad = call_payload("execute", &json!({}));
        assert_eq!(bad["isError"], true, "bad call should fail: {bad}");

        let inspected = call_payload("inspect_drawing", &json!({}));
        assert_ne!(inspected["isError"], true, "inspect failed: {inspected}");
        let text = inspected["content"][0]["text"].as_str().unwrap_or_default();
        assert!(
            text.contains("Line"),
            "tool error reset the headless session: {text}"
        );
    }

    #[test]
    fn session_state_persists_across_calls() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let drawn = call_payload(
            "execute",
            &json!({"command": "line", "params": {"points": [[0, 0], [10, 0]]}}),
        );
        assert_ne!(drawn["isError"], true, "draw failed: {drawn}");
        let inspected = call_payload("inspect_drawing", &json!({}));
        assert_ne!(inspected["isError"], true, "inspect failed: {inspected}");
        let text = inspected["content"][0]["text"].as_str().unwrap_or_default();
        assert!(
            text.contains("Line"),
            "inspect did not see the line: {text}"
        );
    }

    // ── resources ──────────────────────────────────────────────────────────
    //
    // 分发在 worker 线程的上游 `Server` 里，适配层只转发。这三条锁的是
    // 「转发真的通」：列举到上游的两个 URI、能读回内容、未知 uri 报错。

    #[test]
    fn resources_list_matches_upstream() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let value = resources_payload().expect("resources/list");
        let uris: Vec<&str> = value["resources"]
            .as_array()
            .expect("resources array")
            .iter()
            .filter_map(|r| r.get("uri").and_then(Value::as_str))
            .collect();
        assert_eq!(uris.len(), 2, "got {uris:?}");
        assert!(uris.contains(&"cadcraft://drawing"), "got {uris:?}");
        assert!(uris.contains(&"cadcraft://commands"), "got {uris:?}");
    }

    #[test]
    fn resource_templates_is_an_empty_table() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // 上游没有 resources/templates/list 分支；适配层答空表而不是报 Unknown method，
        // 否则宿主会把「没有模板」当成协议错误。
        let value = resource_templates_payload().expect("resources/templates/list");
        let items = value
            .get("resourceTemplates")
            .and_then(Value::as_array)
            .expect("resourceTemplates array");
        assert!(items.is_empty(), "{value}");
    }

    #[test]
    fn read_resource_round_trips_and_rejects_unknown_uri() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let contents = read_resource_payload("cadcraft://commands")
            .expect("read commands resource")
            .get("contents")
            .and_then(Value::as_array)
            .cloned()
            .expect("contents array");
        let text = contents[0]
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(!text.trim().is_empty(), "commands resource came back empty");

        let error = read_resource_payload("cadcraft://nope").expect_err("unknown uri must fail");
        assert!(
            error.contains("not found") || error.contains("unknown resource"),
            "error should say the resource is missing: {error}"
        );
    }
}
