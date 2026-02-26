use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use ed25519_dalek::{SigningKey, Signer};
use serde_json::{json, Value};
use soul_core::tool::{Tool, ToolOutput};
use soul_core::ToolDefinition;
use tokio::sync::mpsc;

/// AMAI Shepherd tool — spawn and manage agent sessions via the Shepherd HTTP API.
///
/// Handles Ed25519 request signing internally so agents never need to shell out
/// to Python or manually construct signed envelopes.
pub struct ShepherdTool {
    shepherd_url: String,
    id_service_url: String,
    kid: String,
    signing_key: SigningKey,
}

impl ShepherdTool {
    /// Create from a raw 32-byte Ed25519 seed.
    pub fn new(
        shepherd_url: impl Into<String>,
        id_service_url: impl Into<String>,
        kid: impl Into<String>,
        secret_key_bytes: [u8; 32],
    ) -> Self {
        Self {
            shepherd_url: shepherd_url.into(),
            id_service_url: id_service_url.into(),
            kid: kid.into(),
            signing_key: SigningKey::from_bytes(&secret_key_bytes),
        }
    }

    /// Build a signed envelope around a payload.
    /// Signature = Ed25519(JSON.stringify(payload)) — matches id-service auth.rs.
    fn sign(&self, payload: &Value) -> Value {
        let payload_json = serde_json::to_string(payload).unwrap_or_default();
        let sig_bytes = self.signing_key.sign(payload_json.as_bytes());
        let signature = B64.encode(sig_bytes.to_bytes());

        // Timestamp in RFC3339 +00:00 format — matches Rust's to_rfc3339()
        let ts = chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%S+00:00")
            .to_string();
        let nonce = {
            use rand::RngCore;
            let mut bytes = [0u8; 32];
            rand::thread_rng().fill_bytes(&mut bytes);
            hex::encode(bytes)
        };

        json!({
            "payload": payload,
            "signature": signature,
            "kid": self.kid,
            "timestamp": ts,
            "nonce": nonce,
        })
    }

    async fn shepherd_post(&self, path: &str, payload: &Value) -> Result<Value, String> {
        let envelope = self.sign(payload);
        let client = reqwest::Client::new();
        let url = format!("{}{}", self.shepherd_url, path);
        let resp = client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&envelope)
            .send()
            .await
            .map_err(|e| format!("HTTP error: {e}"))?;
        let result: Value = resp
            .json()
            .await
            .map_err(|e| format!("Parse error: {e}"))?;
        if result.get("success").and_then(|v| v.as_bool()) == Some(false) {
            return Err(result
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error")
                .to_string());
        }
        Ok(result.get("data").cloned().unwrap_or(result))
    }

    async fn shepherd_get(&self, path: &str) -> Result<Value, String> {
        let client = reqwest::Client::new();
        let url = format!("{}{}", self.shepherd_url, path);
        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("HTTP error: {e}"))?;
        let result: Value = resp
            .json()
            .await
            .map_err(|e| format!("Parse error: {e}"))?;
        if result.get("success").and_then(|v| v.as_bool()) == Some(false) {
            return Err(result
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error")
                .to_string());
        }
        Ok(result.get("data").cloned().unwrap_or(result))
    }
}

#[async_trait]
impl Tool for ShepherdTool {
    fn name(&self) -> &str {
        "shepherd"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "shepherd".into(),
            description: r#"Spawn and manage AI agent sessions via the AMAI Shepherd API.
Signing is handled automatically — you never need to construct envelopes manually.

Actions:
- "spawn"         → Register an agent definition and start a session.
                    Returns: { agent_id, session_id, status }
                    Required: task_prompt (what the sub-agent should do)
                    Optional: contract_id (pass to sub-agent so it knows its contract),
                              budget_usd (default 3.0), binary_path, config_path, cwd,
                              extra_args (e.g. ["--purpose", "research"]),
                              env (key-value pairs to inject, e.g. ANTHROPIC_API_KEY)

- "session_status" → Get current status of a session.
                    Returns: { id, status, spent_usd, budget_usd }
                    Required: session_id

- "poll_result"   → Check if a session has produced an agent.result event yet.
                    Returns: { done: bool, result_text?, turns_used? }
                    Required: session_id

- "stop"          → Stop a running session.
                    Required: session_id

Use this flow for A2A delegation:
1. (Optionally) post a contract via amai_contracts first, get contract_id
2. shepherd spawn(task_prompt, contract_id?) → session_id
3. Loop: shepherd poll_result(session_id) every 30s until done=true
4. (If used contract) amai_contracts settle(fulfilled/disputed)
"#
            .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["spawn", "session_status", "poll_result", "stop"],
                        "description": "Action to perform"
                    },
                    "task_prompt": {
                        "type": "string",
                        "description": "The task instruction delivered to the sub-agent (for spawn)"
                    },
                    "session_id": {
                        "type": "string",
                        "description": "Session ID (for session_status, poll_result, stop)"
                    },
                    "contract_id": {
                        "type": "string",
                        "description": "Optional contract ID to embed in the sub-agent's task context"
                    },
                    "budget_usd": {
                        "type": "number",
                        "description": "Budget for the session in USD (default: 3.0)"
                    },
                    "binary_path": {
                        "type": "string",
                        "description": "Path to amai binary on the host (uses default if omitted)"
                    },
                    "config_path": {
                        "type": "string",
                        "description": "Path to agent config TOML (uses default if omitted)"
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Working directory for the sub-agent (uses default if omitted)"
                    },
                    "extra_args": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Extra CLI args for the agent binary (e.g. [\"--purpose\", \"research\"])"
                    },
                    "env": {
                        "type": "object",
                        "description": "Environment variables to inject into the sub-agent process",
                        "additionalProperties": { "type": "string" }
                    }
                },
                "required": ["action"]
            }),
        }
    }

    async fn execute(
        &self,
        _call_id: &str,
        arguments: Value,
        _partial_tx: Option<mpsc::UnboundedSender<String>>,
    ) -> soul_core::error::SoulResult<ToolOutput> {
        let action = arguments
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let result = match action {
            "spawn" => {
                let task_prompt = match arguments.get("task_prompt").and_then(|v| v.as_str()) {
                    Some(p) if !p.is_empty() => p,
                    _ => return Ok(ToolOutput::error("spawn requires: task_prompt")),
                };

                let budget_usd = arguments
                    .get("budget_usd")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(3.0);

                let binary_path = arguments
                    .get("binary_path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("/Users/boxy/amai-infra/amai/target/release/amai");
                let config_path = arguments
                    .get("config_path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("/Users/boxy/amai-anthropic.toml");
                let cwd = arguments
                    .get("cwd")
                    .and_then(|v| v.as_str())
                    .unwrap_or("/Users/boxy/amai-infra/research");

                let extra_args: Vec<String> = arguments
                    .get("extra_args")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_else(|| vec!["--purpose".into(), "research".into()]);

                // Embed contract_id in system prompt if provided
                let contract_note = arguments
                    .get("contract_id")
                    .and_then(|v| v.as_str())
                    .map(|cid| {
                        format!(
                            "\n\nYou are operating under contract {cid}. \
                             IMPORTANT: Your first action must be to call amai_contracts accept(contract_id=\"{cid}\") \
                             to move the contract from Pending to Active. Then complete the work. \
                             If you succeed, do nothing after completion — the client will assess and settle. \
                             If you cannot complete the work, call amai_contracts settle(contract_id=\"{cid}\", outcome=\"breached\", note=\"<reason>\")."
                        )
                    })
                    .unwrap_or_default();

                let system_prompt = format!("{task_prompt}{contract_note}");

                // Register agent definition
                let agent_payload = json!({
                    "name": "spawned-agent",
                    "description": "Sub-agent spawned via shepherd tool",
                    "source": {
                        "type": "native",
                        "binaryPath": binary_path,
                        "configPath": config_path,
                        "cwd": cwd,
                        "args": extra_args,
                    },
                    "config": {
                        "maxBudgetUsd": budget_usd,
                        "network": true,
                    },
                    "profile": {
                        "systemPrompt": system_prompt,
                    }
                });

                let agent_def = match self.shepherd_post("/agents", &agent_payload).await {
                    Ok(v) => v,
                    Err(e) => return Ok(ToolOutput::error(format!("Register agent failed: {e}"))),
                };
                let agent_id = agent_def["id"].as_str().unwrap_or("").to_string();

                // Build session start payload with optional env
                let mut session_payload = json!({ "budgetUsd": budget_usd });
                if let Some(env_obj) = arguments.get("env").filter(|v| v.is_object()) {
                    session_payload["env"] = env_obj.clone();
                }

                let session = match self
                    .shepherd_post(&format!("/agents/{agent_id}/start"), &session_payload)
                    .await
                {
                    Ok(v) => v,
                    Err(e) => return Ok(ToolOutput::error(format!("Start session failed: {e}"))),
                };

                let session_id = session["id"].as_str().unwrap_or("").to_string();
                let status = session["status"].as_str().unwrap_or("unknown");

                Ok(json!({
                    "agent_id": agent_id,
                    "session_id": session_id,
                    "status": status,
                    "note": "Poll with poll_result(session_id) every 30s until done=true"
                }))
            }

            "session_status" => {
                let session_id = match arguments.get("session_id").and_then(|v| v.as_str()) {
                    Some(id) if !id.is_empty() => id,
                    _ => return Ok(ToolOutput::error("session_status requires: session_id")),
                };
                self.shepherd_get(&format!("/sessions/{session_id}")).await
            }

            "poll_result" => {
                let session_id = match arguments.get("session_id").and_then(|v| v.as_str()) {
                    Some(id) if !id.is_empty() => id,
                    _ => return Ok(ToolOutput::error("poll_result requires: session_id")),
                };

                // Check session status first
                let session = match self
                    .shepherd_get(&format!("/sessions/{session_id}"))
                    .await
                {
                    Ok(v) => v,
                    Err(e) => return Ok(ToolOutput::error(format!("Get session failed: {e}"))),
                };

                let status = session["status"].as_str().unwrap_or("unknown");

                // Check for result event
                let events = self
                    .shepherd_get(&format!(
                        "/sessions/{session_id}/events/history?types=agent.result&limit=1"
                    ))
                    .await;

                let result_text = events.ok().and_then(|ev| {
                    ev.get("events")
                        .and_then(|e| e.as_array())
                        .and_then(|arr| arr.first())
                        .and_then(|e| e.get("text"))
                        .and_then(|t| t.as_str())
                        .map(String::from)
                });

                let terminal = matches!(status, "stopped" | "failed" | "budget_exceeded");

                if let Some(text) = result_text {
                    Ok(json!({
                        "done": true,
                        "result_text": text,
                        "status": status,
                    }))
                } else if terminal {
                    Ok(json!({
                        "done": true,
                        "result_text": null,
                        "status": status,
                        "note": "Session ended without producing a result"
                    }))
                } else {
                    Ok(json!({
                        "done": false,
                        "status": status,
                        "note": "Still running — poll again in 30s"
                    }))
                }
            }

            "stop" => {
                let session_id = match arguments.get("session_id").and_then(|v| v.as_str()) {
                    Some(id) if !id.is_empty() => id,
                    _ => return Ok(ToolOutput::error("stop requires: session_id")),
                };
                self.shepherd_post(
                    &format!("/sessions/{session_id}/stop"),
                    &json!({}),
                )
                .await
            }

            _ => return Ok(ToolOutput::error(format!("Unknown action: {action}"))),
        };

        match result {
            Ok(data) => Ok(ToolOutput::success(
                serde_json::to_string_pretty(&data).unwrap_or_else(|_| data.to_string()),
            )),
            Err(e) => Ok(ToolOutput::error(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tool() -> ShepherdTool {
        ShepherdTool::new(
            "http://localhost:8094",
            "http://localhost:8080",
            "kid_test",
            [0u8; 32],
        )
    }

    #[test]
    fn tool_name() {
        assert_eq!(make_tool().name(), "shepherd");
    }

    #[test]
    fn schema_has_required_action() {
        let def = make_tool().definition();
        let required = def.input_schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("action")));
    }

    #[test]
    fn schema_action_enum() {
        let def = make_tool().definition();
        let actions = def.input_schema["properties"]["action"]["enum"]
            .as_array()
            .unwrap();
        for a in ["spawn", "session_status", "poll_result", "stop"] {
            assert!(actions.contains(&json!(a)), "missing action: {a}");
        }
    }

    #[tokio::test]
    async fn spawn_missing_task_returns_error() {
        let t = make_tool();
        let out = t
            .execute("id", json!({"action": "spawn"}), None)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("task_prompt"));
    }

    #[tokio::test]
    async fn poll_missing_session_returns_error() {
        let t = make_tool();
        let out = t
            .execute("id", json!({"action": "poll_result"}), None)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("session_id"));
    }

    #[test]
    fn sign_produces_valid_envelope() {
        let t = make_tool();
        let payload = json!({"foo": "bar"});
        let envelope = t.sign(&payload);
        assert!(envelope["signature"].is_string());
        assert!(envelope["kid"].as_str().unwrap() == "kid_test");
        assert!(envelope["timestamp"].is_string());
        assert!(envelope["nonce"].is_string());
    }
}
