//! MCP over stdio: newline-delimited JSON-RPC 2.0. Hand-rolled on purpose: the
//! surface is five tools and two resources, and owning the wire keeps the tool
//! descriptions (the part agents actually read) exactly as written.

use crate::registry::Registry;
use crate::tools;
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::PathBuf;

pub const GUIDE: &str = include_str!("../docs/agent-guide.md");
const PROTOCOLS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

pub fn serve(registry_path: Option<PathBuf>) -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        // The registry is re-read per request: editing the overlay takes
        // effect without restarting the harness.
        let reg = Registry::load(registry_path.as_deref());
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(Value::Array(batch)) => {
                let rs: Vec<Value> = batch.iter().filter_map(|m| handle(&reg, m)).collect();
                (!rs.is_empty()).then(|| Value::Array(rs))
            }
            Ok(msg) => handle(&reg, &msg),
            Err(e) => Some(
                json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("parse error: {e}")}}),
            ),
        };
        if let Some(r) = reply {
            serde_json::to_writer(&mut out, &r)?;
            out.write_all(b"\n")?;
            out.flush()?;
        }
    }
    Ok(())
}

fn handle(reg: &Registry, msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned();
    let method = msg
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    // Notifications (no id) never get a reply.
    let id = id?;
    let result = match method {
        "initialize" => Ok(initialize(reg, &params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools::definitions(reg)})),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let o = tools::call(reg, name, &args);
            Ok(json!({
                "content": [{"type": "text", "text": o.text}],
                "structuredContent": o.data,
                "isError": o.is_error,
            }))
        }
        "resources/list" => Ok(json!({"resources": [
            {"uri": "s1://guide", "name": "guide", "title": "How to use System One well",
             "description": "Question types, good and poor fits, how to write questions, and the decide → rate → report loop.",
             "mimeType": "text/markdown"},
            {"uri": "s1://models", "name": "models", "title": "Registered System One models",
             "description": "The model registry as JSON: capabilities, latency, weaknesses, calibration notes, status.",
             "mimeType": "application/json"}
        ]})),
        "resources/read" => match params.get("uri").and_then(Value::as_str) {
            Some("s1://guide") => Ok(
                json!({"contents": [{"uri": "s1://guide", "mimeType": "text/markdown", "text": GUIDE}]}),
            ),
            Some("s1://models") => Ok(
                json!({"contents": [{"uri": "s1://models", "mimeType": "application/json",
                "text": serde_json::to_string_pretty(&tools::models(reg, false).data).unwrap_or_default()}]}),
            ),
            other => Err((
                -32002,
                format!("unknown resource {other:?}; see resources/list"),
            )),
        },
        "resources/templates/list" => Ok(json!({"resourceTemplates": []})),
        "prompts/list" => Ok(json!({"prompts": []})),
        _ => Err((-32601, format!("method not found: {method}"))),
    };
    Some(match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err((code, m)) => {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": m}})
        }
    })
}

fn initialize(reg: &Registry, params: &Value) -> Value {
    let asked = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(PROTOCOLS[0]);
    let version = if PROTOCOLS.contains(&asked) {
        asked
    } else {
        PROTOCOLS[0]
    };
    let models = reg
        .models
        .iter()
        .filter(|m| m.is_live())
        .map(|m| format!("{} ({})", m.id, m.inputs.join("+")))
        .collect::<Vec<_>>()
        .join(", ");
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {"listChanged": false}, "resources": {}},
        "serverInfo": {"name": "s1", "title": "System One decisions", "version": env!("CARGO_PKG_VERSION")},
        "instructions": format!("{GUIDE}\nLive models right now: {}.", if models.is_empty() { "none configured" } else { &models }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speaks_the_handshake() {
        let reg = Registry::default();
        let r = handle(&reg, &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}})).unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
        assert!(
            r["result"]["instructions"]
                .as_str()
                .unwrap()
                .contains("noul")
        );
        assert!(
            handle(
                &reg,
                &json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
            )
            .is_none()
        );
        let t = handle(
            &reg,
            &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        )
        .unwrap();
        let names: Vec<&str> = t["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "s1_decide",
                "s1_compare",
                "s1_rate",
                "s1_models",
                "s1_report"
            ]
        );
        let e = handle(&reg, &json!({"jsonrpc": "2.0", "id": 3, "method": "nope"})).unwrap();
        assert_eq!(e["error"]["code"], -32601);
    }

    #[test]
    fn tool_errors_are_results_not_protocol_errors() {
        let reg = Registry::default();
        let r = handle(&reg, &json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": {"name": "s1_decide", "arguments": {"state": "x", "questions": {"q": {"type": "noul", "instructions": "?"}}, "use_case": "test"}}})).unwrap();
        assert_eq!(r["result"]["isError"], true);
        assert!(
            r["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("no System One models")
        );
    }
}
