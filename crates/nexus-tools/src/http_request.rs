use async_trait::async_trait;
use serde_json::json;
use soul_core::tool::{Tool, ToolOutput};
use soul_core::ToolDefinition;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Maximum response body size (100KB).
const MAX_RESPONSE_BYTES: usize = 102_400;

/// Default timeout in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Maximum timeout in seconds.
const MAX_TIMEOUT_SECS: u64 = 120;

/// General-purpose HTTP request tool supporting all common methods.
///
/// Unlike `web_fetch` (which only does GET through a proxy for WASM/CORS),
/// this tool supports GET, POST, PUT, PATCH, DELETE with custom headers,
/// request body, and configurable timeout. Designed for native agent
/// environments where direct HTTP access is available.
///
/// Use cases:
/// - Calling REST APIs
/// - Posting data to webhooks
/// - Interacting with external services
/// - Testing HTTP endpoints
pub struct HttpRequestTool;

impl HttpRequestTool {
    pub fn new() -> Self {
        Self
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl Tool for HttpRequestTool {
    fn name(&self) -> &str {
        "http_request"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "http_request".into(),
            description: "Make an HTTP request to any URL. Supports GET, POST, PUT, PATCH, DELETE methods with custom headers and request body. Returns the response status, headers, and body.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The URL to send the request to"
                    },
                    "method": {
                        "type": "string",
                        "description": "HTTP method: GET, POST, PUT, PATCH, DELETE (default: GET)",
                        "enum": ["GET", "POST", "PUT", "PATCH", "DELETE"]
                    },
                    "headers": {
                        "type": "object",
                        "description": "Optional HTTP headers as key-value pairs",
                        "additionalProperties": { "type": "string" }
                    },
                    "body": {
                        "type": "string",
                        "description": "Optional request body (for POST, PUT, PATCH)"
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "Request timeout in seconds (default: 30, max: 120)"
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

        let method = arguments
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("GET")
            .to_uppercase();

        let timeout_secs = arguments
            .get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .min(MAX_TIMEOUT_SECS);

        debug!(
            url = %url,
            method = %method,
            timeout_secs,
            "Executing HTTP request"
        );

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .user_agent("Nexus-Agent/1.0")
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        let mut request = match method.as_str() {
            "GET" => client.get(&url),
            "POST" => client.post(&url),
            "PUT" => client.put(&url),
            "PATCH" => client.patch(&url),
            "DELETE" => client.delete(&url),
            _ => return Ok(ToolOutput::error(format!("Unsupported method: {method}"))),
        };

        // Add custom headers
        if let Some(headers) = arguments.get("headers").and_then(|v| v.as_object()) {
            for (key, value) in headers {
                if let Some(val_str) = value.as_str() {
                    if let (Ok(name), Ok(val)) = (
                        reqwest::header::HeaderName::from_bytes(key.as_bytes()),
                        reqwest::header::HeaderValue::from_str(val_str),
                    ) {
                        request = request.header(name, val);
                    } else {
                        warn!(header = %key, "Invalid header name or value, skipping");
                    }
                }
            }
        }

        // Add body for methods that support it
        if let Some(body) = arguments.get("body").and_then(|v| v.as_str()) {
            if matches!(method.as_str(), "POST" | "PUT" | "PATCH") {
                // Auto-detect JSON body
                if body.starts_with('{') || body.starts_with('[') {
                    request = request
                        .header("Content-Type", "application/json")
                        .body(body.to_string());
                } else {
                    request = request.body(body.to_string());
                }
            }
        }

        match request.send().await {
            Ok(resp) => {
                let status = resp.status();
                let status_code = status.as_u16();

                // Collect response headers
                let resp_headers: Vec<String> = resp
                    .headers()
                    .iter()
                    .take(20) // Limit header output
                    .map(|(k, v)| {
                        format!("{}: {}", k, v.to_str().unwrap_or("<binary>"))
                    })
                    .collect();

                let body = match resp.text().await {
                    Ok(text) => {
                        if text.len() > MAX_RESPONSE_BYTES {
                            format!(
                                "{}\n\n[Truncated: showing first {}KB of {} bytes]",
                                &text[..MAX_RESPONSE_BYTES],
                                MAX_RESPONSE_BYTES / 1024,
                                text.len()
                            )
                        } else {
                            text
                        }
                    }
                    Err(e) => format!("[Failed to read response body: {e}]"),
                };

                let mut output = format!("HTTP {status_code} {status}\n");
                if !resp_headers.is_empty() {
                    output.push_str("Headers:\n");
                    for h in &resp_headers {
                        output.push_str(&format!("  {h}\n"));
                    }
                }
                output.push_str(&format!("\nBody:\n{body}"));

                debug!(
                    url = %url,
                    method = %method,
                    status_code,
                    body_len = body.len(),
                    "HTTP request completed"
                );

                Ok(ToolOutput::success(output).with_metadata(json!({
                    "status_code": status_code,
                    "method": method,
                    "url": url,
                })))
            }
            Err(e) => {
                warn!(
                    url = %url,
                    method = %method,
                    error = %e,
                    "HTTP request failed"
                );

                if e.is_timeout() {
                    Ok(ToolOutput::error(format!(
                        "Request timed out after {timeout_secs}s"
                    )))
                } else if e.is_connect() {
                    Ok(ToolOutput::error(format!(
                        "Connection failed: {e}"
                    )))
                } else {
                    Ok(ToolOutput::error(format!("Request failed: {e}")))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definition_valid() {
        let tool = HttpRequestTool::new();
        assert_eq!(tool.name(), "http_request");
        let def = tool.definition();
        assert!(def.input_schema["required"]
            .as_array()
            .unwrap()
            .contains(&json!("url")));
    }

    #[test]
    fn supported_methods_in_schema() {
        let tool = HttpRequestTool::new();
        let def = tool.definition();
        let methods = def.input_schema["properties"]["method"]["enum"]
            .as_array()
            .unwrap();
        assert!(methods.contains(&json!("GET")));
        assert!(methods.contains(&json!("POST")));
        assert!(methods.contains(&json!("PUT")));
        assert!(methods.contains(&json!("PATCH")));
        assert!(methods.contains(&json!("DELETE")));
    }

    #[tokio::test]
    async fn empty_url_returns_error() {
        let tool = HttpRequestTool::new();
        let result = tool
            .execute("test", json!({"url": ""}), None)
            .await
            .unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn missing_url_returns_error() {
        let tool = HttpRequestTool::new();
        let result = tool.execute("test", json!({}), None).await.unwrap();
        assert!(result.is_error);
    }
}
