use async_trait::async_trait;
use serde_json::json;
use soul_core::tool::{Tool, ToolOutput};
use soul_core::ToolDefinition;
use tokio::sync::mpsc;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{UiBridge, UiRequest};

/// Tool that asks the user a question and waits for their response.
pub struct AskUserTool {
    bridge: UiBridge,
}

impl AskUserTool {
    pub fn new(bridge: UiBridge) -> Self {
        Self { bridge }
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "ask_user".into(),
            description: "Ask the user a question and wait for their response. Use this when you need clarification, confirmation, or input from the user.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "The question to ask the user"
                    },
                    "options": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional list of choices to present to the user"
                    }
                },
                "required": ["question"]
            }),
        }
    }

    async fn execute(
        &self,
        _call_id: &str,
        arguments: serde_json::Value,
        _partial_tx: Option<mpsc::UnboundedSender<String>>,
    ) -> soul_core::error::SoulResult<ToolOutput> {
        let question = arguments
            .get("question")
            .and_then(|v| v.as_str())
            .unwrap_or("(no question provided)")
            .to_string();

        let options: Vec<String> = arguments
            .get("options")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let id = Uuid::new_v4().to_string();

        debug!(question = %question, option_count = options.len(), "Asking user");

        // Send request to UI
        if self
            .bridge
            .request_tx
            .send(UiRequest::AskUser {
                id: id.clone(),
                question: question.clone(),
                options,
            })
            .is_err()
        {
            warn!("Failed to send question to UI bridge");
            return Ok(ToolOutput::error("Failed to send question to UI"));
        }

        // Wait for response
        let mut rx = self.bridge.response_rx.lock().await;
        while let Some(resp) = rx.recv().await {
            match resp {
                crate::UiResponse::Answer {
                    id: resp_id,
                    text,
                } if resp_id == id => {
                    return Ok(ToolOutput::success(text));
                }
                _ => continue,
            }
        }

        Ok(ToolOutput::error("User did not respond"))
    }
}
