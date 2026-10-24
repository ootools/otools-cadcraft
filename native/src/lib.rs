//! CADCraft 的 OTools 插件 native 层：把上游 headless MCP 会话编成宿主可加载的动态库。

pub mod mcp;

use serde_json::{json, Value};

fn dispatch(method: &str, payload: &Value) -> Result<Value, String> {
    match method {
        "__otools_mcp_tools__" => Ok(mcp::tools_payload()),
        "__otools_mcp_call__" => {
            let name = payload
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if name.trim().is_empty() {
                return Err("missing tool name `name`".to_string());
            }
            let arguments = payload
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            Ok(mcp::call_payload(name, &arguments))
        }
        // 上游 MCP 的 resources 能力（活动图纸 / 命令目录）。
        "__otools_mcp_resources__" => mcp::resources_payload(),
        "__otools_mcp_resource_templates__" => mcp::resource_templates_payload(),
        "__otools_mcp_read_resource__" => {
            let uri = payload
                .get("uri")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if uri.trim().is_empty() {
                return Err("missing resource uri".to_string());
            }
            mcp::read_resource_payload(uri)
        }
        // The upstream has no prompts; answer with an empty table rather than
        // "Unknown method" so the host does not log an error per registry build.
        "__otools_mcp_prompts__" => Ok(json!({ "prompts": [] })),
        "__otools_mcp_get_prompt__" => Err("this plugin provides no prompts".to_string()),
        "__otools_shutdown__" => {
            mcp::shutdown_headless();
            Ok(json!({ "ok": true, "data": "otools-cadcraft-native headless session released" }))
        }
        other => Err(format!("Unknown method: {other}")),
    }
}

fn guard_invoke<F>(
    input_ptr: *const u8,
    input_len: usize,
    output_len: *mut usize,
    logic: F,
) -> *mut u8
where
    F: FnOnce(&[u8]) -> Result<Value, String>,
{
    if input_ptr.is_null() || output_len.is_null() {
        return std::ptr::null_mut();
    }
    let input = unsafe { std::slice::from_raw_parts(input_ptr, input_len) };
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| logic(input)));
    let response = match outcome {
        Ok(Ok(value)) if value.get("ok").is_some() => value,
        Ok(Ok(value)) => json!({ "ok": true, "data": value }),
        Ok(Err(error)) => json!({ "ok": false, "error": error }),
        Err(payload) => {
            let message = payload
                .downcast_ref::<&str>()
                .map(|text| (*text).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            eprintln!("[cadcraft-native] plugin panic: {message}");
            json!({ "ok": false, "error": format!("plugin internal error: {message}"), "panic": true })
        }
    };
    let mut bytes = serde_json::to_vec(&response)
        .unwrap_or_else(|_| br#"{"ok":false,"error":"serialize failed"}"#.to_vec());
    if bytes.is_empty() {
        bytes.extend_from_slice(b"{}");
    }
    let len = bytes.len();
    unsafe {
        *output_len = len;
    }
    let ptr = bytes.as_mut_ptr();
    std::mem::forget(bytes);
    ptr
}

fn invoke_impl(input: &[u8]) -> Result<Value, String> {
    let parsed: Value =
        serde_json::from_slice(input).map_err(|error| format!("request parse failed: {error}"))?;
    let method = parsed
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let payload = parsed.get("payload").cloned().unwrap_or(Value::Null);
    dispatch(method, &payload)
}

#[no_mangle]
pub extern "C" fn otools_plugin_invoke(
    input_ptr: *const u8,
    input_len: usize,
    output_len: *mut usize,
) -> *mut u8 {
    guard_invoke(input_ptr, input_len, output_len, invoke_impl)
}

#[no_mangle]
pub extern "C" fn otools_plugin_free(ptr: *mut u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    unsafe {
        drop(Vec::from_raw_parts(ptr, len, len));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_rejects_missing_tool_name() {
        let error = dispatch("__otools_mcp_call__", &json!({})).unwrap_err();
        assert!(error.contains("name"), "got {error}");
    }

    #[test]
    fn dispatch_reports_unknown_method() {
        let error = dispatch("nope", &json!({})).unwrap_err();
        assert!(error.contains("Unknown method"), "got {error}");
    }

    #[test]
    fn invoke_impl_parses_method_and_payload() {
        let request = serde_json::to_vec(&json!({
            "method": "nope",
            "payload": { "a": 1 }
        }))
        .unwrap();
        let error = invoke_impl(&request).unwrap_err();
        assert!(error.contains("Unknown method"), "got {error}");
    }

    #[test]
    fn invoke_impl_rejects_malformed_json() {
        let error = invoke_impl(b"not json").unwrap_err();
        assert!(error.contains("request parse failed"), "got {error}");
    }

    #[test]
    fn shutdown_is_accepted() {
        let result = dispatch("__otools_shutdown__", &json!({})).unwrap();
        assert_eq!(result["ok"], true);
    }
}
