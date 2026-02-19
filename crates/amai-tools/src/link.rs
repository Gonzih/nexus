use async_trait::async_trait;
use serde_json::json;
use soul_core::tool::{Tool, ToolOutput};
use soul_core::ToolDefinition;
use tokio::sync::mpsc;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{UiBridge, UiRequest};

/// Tool that opens a link in the user's browser.
pub struct OpenLinkTool {
    bridge: UiBridge,
}

impl OpenLinkTool {
    pub fn new(bridge: UiBridge) -> Self {
        Self { bridge }
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl Tool for OpenLinkTool {
    fn name(&self) -> &str {
        "open_link"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "open_link".into(),
            description: "Ask the user to open a URL in their browser. The link will be rendered as clickable in the terminal. Use this to guide users to external resources.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The URL to open"
                    },
                    "instructions": {
                        "type": "string",
                        "description": "Instructions for the user about what to do at the link"
                    }
                },
                "required": ["url"]
            }),
        }
    }

    async fn execute(
        &self,
        _call_id: &str,
        arguments: serde_json::Value,
        _partial_tx: Option<mpsc::UnboundedSender<String>>,
    ) -> soul_core::error::SoulResult<ToolOutput> {
        let url = arguments
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if url.is_empty() {
            return Ok(ToolOutput::error("No URL provided"));
        }

        let instructions = arguments
            .get("instructions")
            .and_then(|v| v.as_str())
            .unwrap_or("Please open this link")
            .to_string();

        let id = Uuid::new_v4().to_string();

        debug!(url = %url, "Opening link");

        // Send request to UI
        if self
            .bridge
            .request_tx
            .send(UiRequest::OpenLink {
                id: id.clone(),
                url: url.clone(),
                instructions,
            })
            .is_err()
        {
            warn!(url = %url, "Failed to send link request to UI bridge");
            return Ok(ToolOutput::error("Failed to send link request to UI"));
        }

        // Also try to open via web_sys directly
        #[cfg(target_arch = "wasm32")]
        {
            if let Some(window) = web_sys::window() {
                let _ = window.open_with_url_and_target(&url, "_blank");
            }
        }

        // Wait for acknowledgement
        let mut rx = self.bridge.response_rx.lock().await;
        while let Some(resp) = rx.recv().await {
            match resp {
                crate::UiResponse::LinkOpened { id: resp_id } if resp_id == id => {
                    return Ok(ToolOutput::success(format!("Link opened: {url}")));
                }
                _ => continue,
            }
        }

        Ok(ToolOutput::success(format!(
            "Link presented to user: {url}"
        )))
    }
}
