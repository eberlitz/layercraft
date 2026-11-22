//! Model Context Protocol (MCP) JSON-RPC 2.0 server over stdio for LayerCraft.

use std::io::{BufRead, Write};

use base64::Engine as _;
use layercraft_engine::{Session, command_registry, execute};
use layercraft_viewport::{CameraPreset, ColorMode, RenderMode, RenderRequest, render_png};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Stateful MCP server wrapping a [`layercraft_engine::Session`].
#[derive(Debug, Default)]
pub struct McpServer {
    pub session: Session,
}

#[derive(Debug, Deserialize)]
struct JsonRpcReq {
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ExecuteArgs {
    command: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct InspectLayerArgs {
    index: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ScreenshotArgs {
    #[serde(default)]
    mode: RenderMode,
    #[serde(default)]
    camera: CameraPreset,
    max_layer: Option<usize>,
    #[serde(default)]
    single_layer_only: bool,
    #[serde(default)]
    color_mode: ColorMode,
    width: Option<u32>,
    height: Option<u32>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
struct EmptyArgs {}

#[derive(Debug, Serialize)]
struct ToolDef {
    name: &'static str,
    description: &'static str,
    #[serde(rename = "inputSchema")]
    input_schema: Value,
}

fn schema_val<T: JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or_else(|_| json!({ "type": "object" }))
}

fn tool_definitions() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "commands",
            description: "List every LayerCraft engine command and its JSON input/output schemas.",
            input_schema: schema_val::<EmptyArgs>(),
        },
        ToolDef {
            name: "execute",
            description: "Execute a LayerCraft engine command (`plate.add_sample_shape`, `plate.add_stl_bytes`, `config.set`, `slice.run`, `gcode.export`, etc.).",
            input_schema: schema_val::<ExecuteArgs>(),
        },
        ToolDef {
            name: "inspect_plate",
            description: "Return active print config, mesh inspection, and slicing summary for the current plate.",
            input_schema: schema_val::<EmptyArgs>(),
        },
        ToolDef {
            name: "inspect_layer",
            description: "Inspect a single sliced layer (Z height, region count, per-role toolpath lengths).",
            input_schema: schema_val::<InspectLayerArgs>(),
        },
        ToolDef {
            name: "screenshot",
            description: "Render the build plate, sliced 3D toolpaths, or a top-down single layer (`plate`, `preview`, `layer_top_down`) to a PNG image.",
            input_schema: schema_val::<ScreenshotArgs>(),
        },
    ]
}

impl McpServer {
    /// Handle one JSON-RPC 2.0 request line, returning `None` for notifications.
    pub fn handle_line(&mut self, line: &str) -> Option<String> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }
        let req: JsonRpcReq = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                return Some(
                    json!({
                        "jsonrpc": "2.0",
                        "id": Value::Null,
                        "error": { "code": -32700, "message": format!("parse error: {e}") }
                    })
                    .to_string(),
                );
            }
        };

        // Notifications have no `id` and must not emit a response.
        let id = req.id?;

        let response = match req.method.as_str() {
            "initialize" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "serverInfo": { "name": "layercraft-mcp", "version": env!("CARGO_PKG_VERSION") },
                    "capabilities": { "tools": {} }
                }
            }),
            "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            "tools/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "tools": tool_definitions() }
            }),
            "tools/call" => self.handle_tool_call(id, req.params),
            other => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("method not found: {other}") }
            }),
        };

        Some(response.to_string())
    }

    fn handle_tool_call(&mut self, id: Value, params: Value) -> Value {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32602, "message": "missing tool `name`" }
            });
        };
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));

        match self.dispatch_tool(name, args) {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(msg) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "isError": true,
                    "content": [{ "type": "text", "text": msg }]
                }
            }),
        }
    }

    fn dispatch_tool(&mut self, name: &str, args: Value) -> Result<Value, String> {
        match name {
            "commands" => {
                let text =
                    serde_json::to_string_pretty(&command_registry()).map_err(|e| e.to_string())?;
                Ok(json!({ "content": [{ "type": "text", "text": text }] }))
            }
            "execute" => {
                let parsed: ExecuteArgs =
                    serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))?;
                let res = execute(&mut self.session, &parsed.command, parsed.params)
                    .map_err(|e| e.to_string())?;
                let text = serde_json::to_string_pretty(&res).map_err(|e| e.to_string())?;
                Ok(json!({ "content": [{ "type": "text", "text": text }] }))
            }
            "inspect_plate" => {
                let mesh_info = execute(&mut self.session, "plate.inspect", Value::Null).ok();
                let layer_count = self.session.sliced.as_ref().map(|s| s.layers.len());
                let gcode_stats = self.session.last_gcode.as_ref().map(|g| &g.stats);
                let payload = json!({
                    "mesh": mesh_info,
                    "layer_count": layer_count,
                    "gcode_stats": gcode_stats,
                    "config": self.session.config,
                });
                let text = serde_json::to_string_pretty(&payload).map_err(|e| e.to_string())?;
                Ok(json!({ "content": [{ "type": "text", "text": text }] }))
            }
            "inspect_layer" => {
                let parsed: InspectLayerArgs =
                    serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))?;
                let res = execute(
                    &mut self.session,
                    "layer.inspect",
                    json!({ "index": parsed.index }),
                )
                .map_err(|e| e.to_string())?;
                let text = serde_json::to_string_pretty(&res).map_err(|e| e.to_string())?;
                Ok(json!({ "content": [{ "type": "text", "text": text }] }))
            }
            "screenshot" => {
                let parsed: ScreenshotArgs =
                    serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))?;
                let req = RenderRequest {
                    width: parsed.width.unwrap_or(800),
                    height: parsed.height.unwrap_or(600),
                    mode: parsed.mode,
                    camera_preset: parsed.camera,
                    max_layer: parsed.max_layer,
                    single_layer_only: parsed.single_layer_only,
                    color_mode: parsed.color_mode,
                };
                let png_bytes = render_png(
                    self.session.prepared_mesh.as_ref(),
                    self.session.sliced.as_ref(),
                    &self.session.config,
                    &req,
                )
                .map_err(|e| e.to_string())?;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
                let layer_desc = match (self.session.sliced.as_ref(), req.max_layer) {
                    (Some(s), Some(idx)) => format!(
                        "layer {}/{}",
                        idx.min(s.layers.len().saturating_sub(1)) + 1,
                        s.layers.len()
                    ),
                    (Some(s), None) => format!("{} layers", s.layers.len()),
                    (None, _) => "unsliced".to_owned(),
                };
                Ok(json!({
                    "content": [
                        {
                            "type": "image",
                            "data": b64,
                            "mimeType": "image/png"
                        },
                        {
                            "type": "text",
                            "text": format!("Rendered {}x{} {:?} ({layer_desc})", req.width.clamp(64, 1920), req.height.clamp(64, 1920), req.mode)
                        }
                    ]
                }))
            }
            other => Err(format!("unknown MCP tool `{other}`")),
        }
    }

    /// Run blocking JSON-RPC 2.0 loop over stdin/stdout until EOF.
    pub fn serve_stdio(&mut self) -> Result<(), String> {
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        for line_res in stdin.lock().lines() {
            let line = line_res.map_err(|e| format!("stdin read error: {e}"))?;
            if let Some(resp) = self.handle_line(&line) {
                writeln!(out, "{resp}").map_err(|e| format!("stdout write error: {e}"))?;
                out.flush()
                    .map_err(|e| format!("stdout flush error: {e}"))?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_lifecycle_slice_and_screenshot_png() {
        let mut srv = McpServer::default();

        let init: Value = serde_json::from_str(
            &srv.handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "layercraft-mcp");

        // Notifications emit no response.
        assert!(
            srv.handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .is_none()
        );

        let list: Value = serde_json::from_str(
            &srv.handle_line(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 5);

        let load_resp: Value = serde_json::from_str(
            &srv.handle_line(
                r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"execute","arguments":{"command":"plate.add_sample_shape","params":{"shape":"cube20"}}}}"#,
            )
            .unwrap(),
        )
        .unwrap();
        assert!(load_resp["result"]["isError"].is_null());

        let slice_resp: Value = serde_json::from_str(
            &srv.handle_line(
                r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"execute","arguments":{"command":"slice.run"}}}"#,
            )
            .unwrap(),
        )
        .unwrap();
        assert!(slice_resp["result"]["isError"].is_null());

        let shot: Value = serde_json::from_str(
            &srv.handle_line(
                r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"screenshot","arguments":{"mode":"preview","max_layer":25,"width":320,"height":240}}}"#,
            )
            .unwrap(),
        )
        .unwrap();
        let b64 = shot["result"]["content"][0]["data"].as_str().unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(bytes.len() > 1000);
    }

    #[test]
    fn hostile_json_rpc_never_panics() {
        let mut srv = McpServer::default();
        assert!(srv.handle_line("").is_none());
        let bad = srv.handle_line("{not json").unwrap();
        let v: Value = serde_json::from_str(&bad).unwrap();
        assert_eq!(v["error"]["code"], -32700);
        let unknown = srv
            .handle_line(r#"{"jsonrpc":"2.0","id":9,"method":"bogus"}"#)
            .unwrap();
        let vu: Value = serde_json::from_str(&unknown).unwrap();
        assert_eq!(vu["error"]["code"], -32601);
    }
}
