use async_trait::async_trait;
use serde_json::{json, Value};
use soul_core::tool::{Tool, ToolOutput};
use soul_core::ToolDefinition;
use tokio::sync::mpsc;
use tracing::debug;

/// AMAI Contracts tool — post, browse, accept, settle work contracts on the
/// id-service marketplace. Agents earn trust by fulfilling; lose trust by
/// breaching or being disputed.
pub struct ContractsTool {
    id_service_url: String,
    kid: String,
}

impl ContractsTool {
    pub fn new(id_service_url: impl Into<String>, kid: impl Into<String>) -> Self {
        Self {
            id_service_url: id_service_url.into(),
            kid: kid.into(),
        }
    }

    async fn id_post(&self, path: &str, body: Value) -> Result<Value, String> {
        let client = reqwest::Client::new();
        let url = format!("{}{}", self.id_service_url, path);
        let resp = client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("HTTP error: {e}"))?;
        let result: Value = resp.json().await.map_err(|e| format!("Parse error: {e}"))?;
        if result.get("success").and_then(|v| v.as_bool()) == Some(false) {
            return Err(result
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error")
                .to_string());
        }
        Ok(result.get("data").cloned().unwrap_or(result))
    }

    async fn id_get(&self, path: &str) -> Result<Value, String> {
        let client = reqwest::Client::new();
        let url = format!("{}{}", self.id_service_url, path);
        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("HTTP error: {e}"))?;
        let result: Value = resp.json().await.map_err(|e| format!("Parse error: {e}"))?;
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
impl Tool for ContractsTool {
    fn name(&self) -> &str {
        "amai_contracts"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "amai_contracts".into(),
            description: r#"Interact with the AMAI work contract marketplace on id-service.

Actions:
- "whoami"  → Return YOUR own identity (kid, id_service_url). Use this first to understand your role.
- "list"    → List open contracts on the marketplace (filter: "open" | "mine" | "all")
- "offer"   → Post a new work contract (requires: title, description, payment_usd)
- "accept"  → Accept a pending contract and become the provider (requires: contract_id)
- "settle"  → Settle an active contract (requires: contract_id, outcome, note)
             outcome rules: ONLY THE CLIENT (poster) can settle "fulfilled" or "disputed".
             The PROVIDER (worker) can only settle "breached" (self-report failure).
             If you are the provider and work is done, you CANNOT self-certify fulfilled —
             the client must call settle. You can only call settle with outcome="breached".
             To check if you are the client or provider: use "get" and compare client_kid/provider_kid with your "whoami" kid.
- "cancel"  → Cancel a pending contract you posted (requires: contract_id)
- "get"     → Get details of a specific contract (requires: contract_id)

Trust scores update automatically on settlement. Every contract is recorded permanently on the soulchain."#.into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "description": "Action to perform",
                        "enum": ["whoami", "list", "offer", "accept", "settle", "cancel", "get"]
                    },
                    "filter": {
                        "type": "string",
                        "description": "For list: 'open' (marketplace), 'mine' (your contracts), 'all'",
                        "enum": ["open", "mine", "all"]
                    },
                    "contract_id": {
                        "type": "string",
                        "description": "UUID of the contract (required for accept/settle/cancel/get)"
                    },
                    "title": {
                        "type": "string",
                        "description": "Short title of the work (required for offer)"
                    },
                    "description": {
                        "type": "string",
                        "description": "Full task description and acceptance criteria (required for offer)"
                    },
                    "payment_usd": {
                        "type": "number",
                        "description": "Payment in USD credits (required for offer)"
                    },
                    "deadline_hours": {
                        "type": "number",
                        "description": "Optional deadline in hours from now (for offer)"
                    },
                    "outcome": {
                        "type": "string",
                        "description": "Settlement outcome (required for settle)",
                        "enum": ["fulfilled", "disputed", "breached"]
                    },
                    "note": {
                        "type": "string",
                        "description": "Settlement note or delivery proof (for settle)"
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

        debug!(action, kid = %self.kid, "ContractsTool executing");

        let result = match action {
            "whoami" => {
                // Return own identity — no HTTP call needed
                return Ok(ToolOutput::success(
                    serde_json::to_string_pretty(&json!({
                        "kid": self.kid,
                        "id_service_url": self.id_service_url,
                        "note": "Compare your kid with client_kid/provider_kid in a contract to know your role."
                    }))
                    .unwrap_or_default(),
                ));
            }
            "list" => {
                let filter = arguments
                    .get("filter")
                    .and_then(|v| v.as_str())
                    .unwrap_or("open");
                let path = if filter == "mine" {
                    format!("/contracts?filter=mine&kid={}", urlencoding::encode(&self.kid))
                } else {
                    format!("/contracts?filter={filter}")
                };
                self.id_get(&path).await
            }

            "offer" => {
                let title = arguments.get("title").and_then(|v| v.as_str()).unwrap_or("");
                let description = arguments
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let payment_usd = arguments
                    .get("payment_usd")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);

                if title.is_empty() || description.is_empty() {
                    return Ok(ToolOutput::error(
                        "offer requires: title, description, payment_usd",
                    ));
                }

                let deadline = arguments
                    .get("deadline_hours")
                    .and_then(|v| v.as_f64())
                    .map(|h| {
                        let deadline_ms = chrono::Utc::now().timestamp_millis()
                            + (h * 3_600_000.0) as i64;
                        chrono::DateTime::from_timestamp_millis(deadline_ms)
                            .map(|dt| dt.to_rfc3339())
                    })
                    .flatten();

                self.id_post(
                    "/contracts",
                    json!({
                        "kid": self.kid,
                        "title": title,
                        "description": description,
                        "payment_usd": payment_usd,
                        "deadline": deadline,
                    }),
                )
                .await
            }

            "accept" => {
                let contract_id = match arguments.get("contract_id").and_then(|v| v.as_str()) {
                    Some(id) => id,
                    None => return Ok(ToolOutput::error("accept requires: contract_id")),
                };
                self.id_post(
                    &format!("/contracts/{contract_id}/accept"),
                    json!({ "kid": self.kid }),
                )
                .await
            }

            "settle" => {
                let contract_id = match arguments.get("contract_id").and_then(|v| v.as_str()) {
                    Some(id) => id,
                    None => return Ok(ToolOutput::error("settle requires: contract_id")),
                };
                let outcome = match arguments.get("outcome").and_then(|v| v.as_str()) {
                    Some(o) => o,
                    None => {
                        return Ok(ToolOutput::error(
                            "settle requires: outcome (fulfilled|disputed|breached)",
                        ))
                    }
                };
                let note = arguments.get("note").and_then(|v| v.as_str());
                self.id_post(
                    &format!("/contracts/{contract_id}/settle"),
                    json!({
                        "kid": self.kid,
                        "outcome": outcome,
                        "note": note,
                    }),
                )
                .await
            }

            "cancel" => {
                let contract_id = match arguments.get("contract_id").and_then(|v| v.as_str()) {
                    Some(id) => id,
                    None => return Ok(ToolOutput::error("cancel requires: contract_id")),
                };
                self.id_post(
                    &format!("/contracts/{contract_id}/cancel"),
                    json!({ "kid": self.kid }),
                )
                .await
            }

            "get" => {
                let contract_id = match arguments.get("contract_id").and_then(|v| v.as_str()) {
                    Some(id) => id,
                    None => return Ok(ToolOutput::error("get requires: contract_id")),
                };
                self.id_get(&format!("/contracts/{contract_id}")).await
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

    #[test]
    fn tool_name() {
        let t = ContractsTool::new("http://localhost:8080", "kid_test");
        assert_eq!(t.name(), "amai_contracts");
    }

    #[test]
    fn schema_has_required_action() {
        let t = ContractsTool::new("http://localhost:8080", "kid_test");
        let def = t.definition();
        let required = def.input_schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("action")));
    }

    #[test]
    fn schema_action_enum() {
        let t = ContractsTool::new("http://localhost:8080", "kid_test");
        let def = t.definition();
        let actions = def.input_schema["properties"]["action"]["enum"]
            .as_array()
            .unwrap();
        for a in ["whoami", "list", "offer", "accept", "settle", "cancel", "get"] {
            assert!(actions.contains(&json!(a)), "missing action: {a}");
        }
    }

    #[tokio::test]
    async fn whoami_returns_kid() {
        let t = ContractsTool::new("http://localhost:8080", "kid_abc123");
        let out = t.execute("id", json!({"action": "whoami"}), None).await.unwrap();
        assert!(!out.is_error, "whoami should succeed: {}", out.content);
        let val: serde_json::Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(val["kid"].as_str().unwrap(), "kid_abc123");
        assert_eq!(val["id_service_url"].as_str().unwrap(), "http://localhost:8080");
    }

    #[tokio::test]
    async fn empty_action_returns_error() {
        let t = ContractsTool::new("http://localhost:8080", "kid_test");
        let out = t.execute("id", json!({"action": ""}), None).await.unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn offer_missing_title_returns_error() {
        let t = ContractsTool::new("http://localhost:8080", "kid_test");
        let out = t
            .execute("id", json!({"action": "offer", "payment_usd": 10.0}), None)
            .await
            .unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn accept_missing_contract_id_returns_error() {
        let t = ContractsTool::new("http://localhost:8080", "kid_test");
        let out = t
            .execute("id", json!({"action": "accept"}), None)
            .await
            .unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn settle_missing_outcome_returns_error() {
        let t = ContractsTool::new("http://localhost:8080", "kid_test");
        let out = t
            .execute(
                "id",
                json!({"action": "settle", "contract_id": "abc-123"}),
                None,
            )
            .await
            .unwrap();
        assert!(out.is_error);
    }
}
