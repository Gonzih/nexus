mod web_fetch;
mod web_search;
mod http_request;
mod glob;
mod link;
mod ask_user;

pub use web_fetch::WebFetchTool;
pub use web_search::WebSearchTool;
pub use http_request::HttpRequestTool;
pub use glob::GlobTool;
pub use link::OpenLinkTool;
pub use ask_user::AskUserTool;

use soul_core::tool::ToolRegistry;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Channel for communicating tool requests to the UI layer.
/// The UI renders prompts/links and sends back user responses.
#[derive(Clone)]
pub struct UiBridge {
    pub request_tx: mpsc::UnboundedSender<UiRequest>,
    pub response_rx: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<UiResponse>>>,
}

#[derive(Debug, Clone)]
pub enum UiRequest {
    /// Ask the user a question, optionally with choices
    AskUser {
        id: String,
        question: String,
        options: Vec<String>,
    },
    /// Request to open a link in the browser
    OpenLink {
        id: String,
        url: String,
        instructions: String,
    },
}

#[derive(Debug, Clone)]
pub enum UiResponse {
    /// User answered a question
    Answer { id: String, text: String },
    /// Link was opened
    LinkOpened { id: String },
}

impl UiBridge {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<UiRequest>, mpsc::UnboundedSender<UiResponse>) {
        let (req_tx, req_rx) = mpsc::unbounded_channel();
        let (resp_tx, resp_rx) = mpsc::unbounded_channel();
        let bridge = Self {
            request_tx: req_tx,
            response_rx: Arc::new(tokio::sync::Mutex::new(resp_rx)),
        };
        (bridge, req_rx, resp_tx)
    }
}

/// Create a ToolRegistry with all AMAI-specific tools (browser/UI context).
pub fn amai_tools(
    bridge: UiBridge,
    proxy_base_url: String,
) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(AskUserTool::new(bridge.clone())));
    registry.register(Box::new(OpenLinkTool::new(bridge.clone())));
    registry.register(Box::new(WebFetchTool::new(proxy_base_url.clone())));
    registry.register(Box::new(WebSearchTool::with_proxy(proxy_base_url)));
    registry.register(Box::new(HttpRequestTool::new()));
    registry
}

/// Create a ToolRegistry with agent tools for native (non-WASM) environments.
///
/// Includes web search, HTTP requests, and glob — tools that an autonomous
/// agent uses when it has direct system access (no proxy needed).
pub fn agent_tools(cwd: impl Into<String>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(WebSearchTool::new()));
    registry.register(Box::new(HttpRequestTool::new()));
    registry.register(Box::new(GlobTool::new(cwd)));
    registry
}
