use async_trait::async_trait;
use serde_json::json;
use soul_core::tool::{Tool, ToolOutput};
use soul_core::ToolDefinition;
use std::sync::OnceLock;
use tokio::sync::{mpsc, Semaphore};
use tracing::{debug, warn};

/// Global semaphore limiting DDG fallback requests to 1 concurrent.
static DDG_SEMAPHORE: OnceLock<Semaphore> = OnceLock::new();

fn ddg_semaphore() -> &'static Semaphore {
    DDG_SEMAPHORE.get_or_init(|| Semaphore::new(1))
}

const MAX_RESULTS: usize = 10;
const MAX_BODY_BYTES: usize = 102_400;

/// Tool that searches the web.
///
/// Priority:
/// 1. Brave Search API (if BRAVE_SEARCH_API_KEY env var is set) — reliable, works on all IPs
/// 2. DuckDuckGo JSON instant answer API — works for famous entities, no API key
/// 3. DuckDuckGo HTML scraping via curl — fallback, blocked on some IPs
pub struct WebSearchTool {
    /// Optional proxy base URL for WASM environments (CORS bypass).
    proxy_base_url: Option<String>,
}

impl WebSearchTool {
    pub fn new() -> Self {
        Self { proxy_base_url: None }
    }

    pub fn with_proxy(proxy_base_url: impl Into<String>) -> Self {
        Self { proxy_base_url: Some(proxy_base_url.into()) }
    }
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "web_search".into(),
            description: "Search the web. Returns titles, URLs, and snippets for the top results.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Maximum number of results to return (default: 8, max: 10)"
                    }
                },
                "required": ["query"]
            }),
        }
    }

    async fn execute(
        &self,
        _call_id: &str,
        arguments: serde_json::Value,
        _partial_tx: Option<mpsc::UnboundedSender<String>>,
    ) -> soul_core::error::SoulResult<ToolOutput> {
        let query = arguments
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if query.is_empty() {
            return Ok(ToolOutput::error("No search query provided"));
        }

        let max_results = arguments
            .get("max_results")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).min(MAX_RESULTS))
            .unwrap_or(8);

        debug!(query = %query, max_results, "Executing web search");

        if self.proxy_base_url.is_none() {
            // Native mode: Brave → DDG JSON → DDG HTML

            // 1. Brave Search API
            if let Ok(api_key) = std::env::var("BRAVE_SEARCH_API_KEY") {
                if !api_key.is_empty() {
                    match brave_search(&query, max_results, &api_key).await {
                        Ok(results) if !results.is_empty() => {
                            return Ok(format_results(&query, &results, "brave"));
                        }
                        Ok(_) => debug!(query = %query, "Brave returned no results, falling back"),
                        Err(e) => warn!(query = %query, error = %e, "Brave search failed, falling back"),
                    }
                }
            }

            let _permit = ddg_semaphore().acquire().await.expect("semaphore not closed");

            // 2. DDG JSON instant answer API
            let json_url = format!(
                "https://api.duckduckgo.com/?q={}&format=json&no_html=1&skip_disambig=1",
                urlencoding(&query)
            );
            if let Ok(out) = tokio::process::Command::new("curl")
                .args(["-s", "--max-time", "10", &json_url])
                .output()
                .await
            {
                if out.status.success() && !out.stdout.is_empty() {
                    let text = String::from_utf8_lossy(&out.stdout);
                    let results = parse_ddg_json(&text, max_results);
                    if !results.is_empty() {
                        return Ok(format_results(&query, &results, "ddg_json"));
                    }
                }
            }

            // 3. DDG HTML scraping
            let ddg_url = format!("https://html.duckduckgo.com/html/?q={}", urlencoding(&query));
            match tokio::process::Command::new("curl")
                .args([
                    "-s", "--max-time", "15",
                    "-A", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
                    "-L", "--compressed",
                    &ddg_url,
                ])
                .output()
                .await
            {
                Ok(out) if out.status.success() && !out.stdout.is_empty() => {
                    let text = String::from_utf8_lossy(&out.stdout);
                    let body = if text.len() > MAX_BODY_BYTES { &text[..MAX_BODY_BYTES] } else { &text };
                    let results = parse_ddg_html(body, max_results);
                    if !results.is_empty() {
                        return Ok(format_results(&query, &results, "ddg_html"));
                    }
                    return Ok(ToolOutput::success(format!("No results found for: {query}")));
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    warn!(query = %query, stderr = %stderr, "web_search: all backends failed");
                    return Ok(ToolOutput::error(format!("Search failed: {stderr}")));
                }
                Err(e) => {
                    return Ok(ToolOutput::error(format!("Search failed: {e}")));
                }
            }
        } else {
            // Proxy mode (WASM): route through proxy to DDG HTML
            let proxy_url = self.proxy_base_url.as_ref().unwrap();
            let ddg_url = format!("https://html.duckduckgo.com/html/?q={}", urlencoding(&query));
            let fetch_url = format!("{}/proxy/fetch?url={}", proxy_url, urlencoding(&ddg_url));

            let client = reqwest::Client::builder()
                .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new());

            let body = match client.get(&fetch_url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    match resp.text().await {
                        Ok(text) => if text.len() > MAX_BODY_BYTES { text[..MAX_BODY_BYTES].to_string() } else { text },
                        Err(e) => return Ok(ToolOutput::error(format!("Failed to read response: {e}"))),
                    }
                }
                Ok(resp) => return Ok(ToolOutput::error(format!("Search failed: HTTP {}", resp.status()))),
                Err(e) => return Ok(ToolOutput::error(format!("Search failed: {e}"))),
            };

            let results = parse_ddg_html(&body, max_results);
            if results.is_empty() {
                return Ok(ToolOutput::success(format!("No results found for: {query}")));
            }
            return Ok(format_results(&query, &results, "ddg_html"));
        }
    }
}

fn format_results(query: &str, results: &[SearchResult], source: &str) -> ToolOutput {
    let mut output = format!("Search results for: {query}\n\n");
    for (i, r) in results.iter().enumerate() {
        output.push_str(&format!("{}. {}\n   {}\n   {}\n\n", i + 1, r.title, r.url, r.snippet));
    }
    ToolOutput::success(output).with_metadata(json!({
        "result_count": results.len(),
        "query": query,
        "source": source,
    }))
}

/// Call the Brave Search API.
async fn brave_search(query: &str, max_results: usize, api_key: &str) -> Result<Vec<SearchResult>, String> {
    let url = format!(
        "https://api.search.brave.com/res/v1/web/search?q={}&count={}&text_decorations=false&search_lang=en",
        urlencoding(query),
        max_results.min(20),
    );

    let out = tokio::process::Command::new("curl")
        .args([
            "-s", "--max-time", "10",
            "-H", &format!("X-Subscription-Token: {api_key}"),
            "-H", "Accept: application/json",
            &url,
        ])
        .output()
        .await
        .map_err(|e| e.to_string())?;

    if !out.status.success() {
        return Err(format!("curl exit {}", out.status));
    }

    let text = String::from_utf8_lossy(&out.stdout);
    if text.is_empty() {
        return Err("empty response".into());
    }

    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;

    let mut results = Vec::new();
    if let Some(web) = v.get("web").and_then(|w| w.get("results")).and_then(|r| r.as_array()) {
        for item in web.iter().take(max_results) {
            let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let url = item.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let snippet = item.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if !url.is_empty() {
                results.push(SearchResult { title, url, snippet });
            }
        }
    }

    Ok(results)
}

struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

/// Parse DuckDuckGo Instant Answer JSON API response.
fn parse_ddg_json(json_str: &str, max_results: usize) -> Vec<SearchResult> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json_str) else {
        return vec![];
    };

    let mut results = Vec::new();

    let abstract_text = v.get("Abstract").and_then(|v| v.as_str()).unwrap_or("");
    let abstract_url = v.get("AbstractURL").and_then(|v| v.as_str()).unwrap_or("");
    let heading = v.get("Heading").and_then(|v| v.as_str()).unwrap_or("");

    if !abstract_text.is_empty() && !abstract_url.is_empty() {
        results.push(SearchResult {
            title: if heading.is_empty() { "Wikipedia".into() } else { heading.into() },
            url: abstract_url.into(),
            snippet: abstract_text.into(),
        });
    }

    if let Some(res_arr) = v.get("Results").and_then(|v| v.as_array()) {
        for item in res_arr.iter().take(max_results.saturating_sub(results.len())) {
            let text = item.get("Text").and_then(|v| v.as_str()).unwrap_or("");
            let url = item.get("FirstURL").and_then(|v| v.as_str()).unwrap_or("");
            if !url.is_empty() {
                results.push(SearchResult { title: text.into(), url: url.into(), snippet: String::new() });
            }
        }
    }

    if let Some(topics) = v.get("RelatedTopics").and_then(|v| v.as_array()) {
        for item in topics.iter().take(max_results.saturating_sub(results.len())) {
            let text = item.get("Text").and_then(|v| v.as_str()).unwrap_or("");
            let url = item.get("FirstURL").and_then(|v| v.as_str()).unwrap_or("");
            if !text.is_empty() && !url.is_empty() {
                let (title, snippet) = if let Some(dot) = text.find(" - ") {
                    (&text[..dot], &text[dot + 3..])
                } else {
                    (text, "")
                };
                results.push(SearchResult { title: title.into(), url: url.into(), snippet: snippet.into() });
            }
        }
    }

    results
}

/// Parse DuckDuckGo HTML search results.
fn parse_ddg_html(html: &str, max_results: usize) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let mut search_pos = 0;

    while results.len() < max_results {
        let link_marker = "class=\"result__a\"";
        let link_pos = match html[search_pos..].find(link_marker) {
            Some(pos) => search_pos + pos,
            None => break,
        };

        let href_start = match html[search_pos..link_pos].rfind("href=\"") {
            Some(pos) => search_pos + pos + 6,
            None => match html[link_pos..].find("href=\"") {
                Some(pos) => link_pos + pos + 6,
                None => { search_pos = link_pos + link_marker.len(); continue; }
            },
        };

        let href_end = match html[href_start..].find('"') {
            Some(pos) => href_start + pos,
            None => { search_pos = link_pos + link_marker.len(); continue; }
        };

        let raw_url = html[href_start..href_end].to_string();
        let url = decode_ddg_url(&raw_url);

        let title_start = match html[link_pos..].find('>') {
            Some(pos) => link_pos + pos + 1,
            None => { search_pos = href_end; continue; }
        };
        let title_end = match html[title_start..].find("</a>") {
            Some(pos) => title_start + pos,
            None => { search_pos = href_end; continue; }
        };

        let title = strip_html_tags(&html[title_start..title_end]).trim().to_string();

        let snippet_marker = "class=\"result__snippet\"";
        let snippet = if let Some(snip_pos) = html[title_end..].find(snippet_marker) {
            let snip_start_pos = title_end + snip_pos;
            if let Some(snip_text_start) = html[snip_start_pos..].find('>') {
                let snip_text_pos = snip_start_pos + snip_text_start + 1;
                if let Some(snip_text_end) = html[snip_text_pos..].find("</a>") {
                    strip_html_tags(&html[snip_text_pos..snip_text_pos + snip_text_end])
                        .trim()
                        .to_string()
                } else {
                    String::new()
                }
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        if !url.is_empty() && !title.is_empty() && url.starts_with("http") {
            results.push(SearchResult { title, url, snippet });
        }

        search_pos = title_end;
    }

    results
}

fn decode_ddg_url(raw: &str) -> String {
    if raw.contains("uddg=") {
        if let Some(start) = raw.find("uddg=") {
            let encoded = &raw[start + 5..];
            let end = encoded.find('&').unwrap_or(encoded.len());
            return urldecoding(&encoded[..end]);
        }
    }
    if raw.starts_with("http") {
        return raw.to_string();
    }
    raw.to_string()
}

fn strip_html_tags(s: &str) -> String {
    let mut result = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(c),
            _ => {}
        }
    }
    result
}

fn urlencoding(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            ' ' => "+".to_string(),
            c => {
                let mut buf = [0u8; 4];
                let bytes = c.encode_utf8(&mut buf);
                bytes.bytes().map(|b| format!("%{b:02X}")).collect()
            }
        })
        .collect()
}

fn urldecoding(s: &str) -> String {
    let mut result = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            let h1 = chars.next().unwrap_or('0');
            let h2 = chars.next().unwrap_or('0');
            if let Ok(byte) = u8::from_str_radix(&format!("{h1}{h2}"), 16) {
                result.push(byte as char);
            }
        } else if c == '+' {
            result.push(' ');
        } else {
            result.push(c);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_urlencoding_spaces() {
        assert_eq!(urlencoding("hello world"), "hello+world");
    }

    #[test]
    fn test_urlencoding_special() {
        assert_eq!(urlencoding("foo&bar=baz"), "foo%26bar%3Dbaz");
    }

    #[test]
    fn test_strip_html_tags() {
        assert_eq!(strip_html_tags("<b>hello</b> world"), "hello world");
    }

    #[test]
    fn test_parse_ddg_json_empty() {
        let results = parse_ddg_json("{}", 5);
        assert!(results.is_empty());
    }

    #[test]
    fn test_parse_ddg_json_with_abstract() {
        let json = r#"{"Abstract":"Test abstract","AbstractURL":"https://example.com","Heading":"Test","Results":[],"RelatedTopics":[]}"#;
        let results = parse_ddg_json(json, 5);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Test");
        assert_eq!(results[0].url, "https://example.com");
    }

    #[test]
    fn test_web_search_tool_name() {
        let tool = WebSearchTool::new();
        assert_eq!(tool.name(), "web_search");
    }
}
