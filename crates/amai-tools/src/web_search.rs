use async_trait::async_trait;
use serde_json::json;
use soul_core::tool::{Tool, ToolOutput};
use soul_core::ToolDefinition;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Maximum number of search results to return.
const MAX_RESULTS: usize = 10;

/// Maximum response body size before truncation (100KB).
const MAX_BODY_BYTES: usize = 102_400;

/// Tool that searches the web using DuckDuckGo HTML (no API key required).
///
/// Uses the DuckDuckGo HTML endpoint which returns search results without
/// JavaScript rendering. Results are parsed from the HTML response and
/// returned as structured text with titles, URLs, and snippets.
///
/// This is designed for free-tier agent operation — no API keys, no rate
/// limit tokens, no billing. DuckDuckGo's HTML endpoint is stable and
/// doesn't require authentication.
pub struct WebSearchTool {
    /// Optional proxy base URL for WASM environments (CORS bypass).
    /// When None, requests go directly (native mode).
    proxy_base_url: Option<String>,
}

impl WebSearchTool {
    /// Create a new WebSearchTool for native environments (direct HTTP).
    pub fn new() -> Self {
        Self {
            proxy_base_url: None,
        }
    }

    /// Create a new WebSearchTool that routes through a proxy (for WASM/CORS).
    pub fn with_proxy(proxy_base_url: impl Into<String>) -> Self {
        Self {
            proxy_base_url: Some(proxy_base_url.into()),
        }
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
            description: "Search the web using DuckDuckGo. Returns titles, URLs, and snippets for the top results. Use this to find current information, documentation, or answers to questions.".into(),
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

        let url = format!(
            "https://html.duckduckgo.com/html/?q={}",
            urlencoding(&query)
        );

        let fetch_url = if let Some(ref proxy) = self.proxy_base_url {
            format!("{}/proxy/fetch?url={}", proxy, urlencoding(&url))
        } else {
            url
        };

        let client = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (compatible; AMAI-Agent/1.0)")
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        let body = match client.get(&fetch_url).send().await {
            Ok(resp) => {
                if !resp.status().is_success() {
                    let status = resp.status();
                    warn!(status = %status, "Web search HTTP error");
                    return Ok(ToolOutput::error(format!(
                        "Search request failed with status {status}"
                    )));
                }
                match resp.text().await {
                    Ok(text) => {
                        if text.len() > MAX_BODY_BYTES {
                            text[..MAX_BODY_BYTES].to_string()
                        } else {
                            text
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "Failed to read search response body");
                        return Ok(ToolOutput::error(format!(
                            "Failed to read search response: {e}"
                        )));
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "Web search request failed");
                return Ok(ToolOutput::error(format!("Search request failed: {e}")));
            }
        };

        let results = parse_ddg_html(&body, max_results);

        if results.is_empty() {
            debug!(query = %query, "No search results found");
            return Ok(ToolOutput::success(format!(
                "No results found for: {query}"
            )));
        }

        let mut output = format!("Search results for: {query}\n\n");
        for (i, result) in results.iter().enumerate() {
            output.push_str(&format!(
                "{}. {}\n   {}\n   {}\n\n",
                i + 1,
                result.title,
                result.url,
                result.snippet
            ));
        }

        debug!(
            query = %query,
            result_count = results.len(),
            "Web search completed"
        );

        Ok(ToolOutput::success(output).with_metadata(json!({
            "result_count": results.len(),
            "query": query,
        })))
    }
}

struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

/// Parse DuckDuckGo HTML search results.
///
/// DDG HTML returns results in a structure like:
/// ```html
/// <div class="result results_links results_links_deep web-result">
///   <h2 class="result__title">
///     <a class="result__a" href="https://...">Title</a>
///   </h2>
///   <a class="result__snippet">Snippet text...</a>
/// </div>
/// ```
///
/// We parse this with simple string scanning — no HTML parser dependency needed.
fn parse_ddg_html(html: &str, max_results: usize) -> Vec<SearchResult> {
    let mut results = Vec::new();

    // Find all result__a links (these are the main result links)
    let mut search_pos = 0;
    while results.len() < max_results {
        // Find the next result link
        let link_marker = "class=\"result__a\"";
        let link_pos = match html[search_pos..].find(link_marker) {
            Some(pos) => search_pos + pos,
            None => break,
        };

        // Extract href from the link
        let href_start = match html[search_pos..link_pos].rfind("href=\"") {
            Some(pos) => search_pos + pos + 6,
            None => {
                // Try forward from the marker
                match html[link_pos..].find("href=\"") {
                    Some(pos) => link_pos + pos + 6,
                    None => {
                        search_pos = link_pos + link_marker.len();
                        continue;
                    }
                }
            }
        };

        let href_end = match html[href_start..].find('"') {
            Some(pos) => href_start + pos,
            None => {
                search_pos = link_pos + link_marker.len();
                continue;
            }
        };

        let raw_url = html[href_start..href_end].to_string();
        let url = decode_ddg_url(&raw_url);

        // Extract title (text between > and </a> after the class marker)
        let title_start = match html[link_pos..].find('>') {
            Some(pos) => link_pos + pos + 1,
            None => {
                search_pos = href_end;
                continue;
            }
        };
        let title_end = match html[title_start..].find("</a>") {
            Some(pos) => title_start + pos,
            None => {
                search_pos = href_end;
                continue;
            }
        };
        let title = strip_html_tags(&html[title_start..title_end]);

        // Find snippet near this result
        let snippet_search_start = title_end;
        let snippet_search_end = (snippet_search_start + 2000).min(html.len());
        let snippet_region = &html[snippet_search_start..snippet_search_end];

        let snippet = if let Some(snip_pos) = snippet_region.find("class=\"result__snippet\"") {
            let snip_start = match snippet_region[snip_pos..].find('>') {
                Some(pos) => snip_pos + pos + 1,
                None => {
                    search_pos = title_end;
                    continue;
                }
            };
            let snip_end_tag = snippet_region[snip_start..]
                .find("</a>")
                .or_else(|| snippet_region[snip_start..].find("</span>"))
                .unwrap_or(snippet_region.len() - snip_start);
            strip_html_tags(&snippet_region[snip_start..snip_start + snip_end_tag])
        } else {
            String::new()
        };

        if !title.is_empty() && !url.is_empty() {
            results.push(SearchResult {
                title: html_entities_decode(&title),
                url,
                snippet: html_entities_decode(&snippet),
            });
        }

        search_pos = title_end;
    }

    results
}

/// Decode DuckDuckGo redirect URLs.
/// DDG wraps results in redirect URLs like:
/// `//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com&rut=...`
fn decode_ddg_url(raw: &str) -> String {
    if raw.contains("uddg=") {
        if let Some(start) = raw.find("uddg=") {
            let encoded = &raw[start + 5..];
            let end = encoded.find('&').unwrap_or(encoded.len());
            return urldecoding(&encoded[..end]);
        }
    }
    // If it's already a direct URL
    if raw.starts_with("http") {
        return raw.to_string();
    }
    if raw.starts_with("//") {
        return format!("https:{raw}");
    }
    raw.to_string()
}

/// Strip HTML tags from a string, preserving text content.
fn strip_html_tags(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        if c == '<' {
            in_tag = true;
        } else if c == '>' {
            in_tag = false;
        } else if !in_tag {
            result.push(c);
        }
    }
    result.trim().to_string()
}

/// Decode basic HTML entities.
fn html_entities_decode(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ")
}

fn urlencoding(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect()
}

fn urldecoding(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                result.push(byte as char);
            } else {
                result.push('%');
                result.push_str(&hex);
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
    fn strip_tags_basic() {
        assert_eq!(strip_html_tags("<b>hello</b> world"), "hello world");
        assert_eq!(strip_html_tags("no tags"), "no tags");
        assert_eq!(strip_html_tags("<a href=\"x\">link</a>"), "link");
    }

    #[test]
    fn html_entities() {
        assert_eq!(html_entities_decode("a &amp; b"), "a & b");
        assert_eq!(html_entities_decode("&lt;tag&gt;"), "<tag>");
        assert_eq!(html_entities_decode("it&#39;s"), "it's");
    }

    #[test]
    fn url_encoding_roundtrip() {
        let original = "rust lang tutorial";
        let encoded = urlencoding(original);
        let decoded = urldecoding(&encoded);
        assert_eq!(decoded, original);
    }

    #[test]
    fn decode_ddg_redirect_url() {
        let raw = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&rut=abc";
        assert_eq!(decode_ddg_url(raw), "https://example.com/page");
    }

    #[test]
    fn decode_direct_url() {
        assert_eq!(
            decode_ddg_url("https://example.com"),
            "https://example.com"
        );
    }

    #[test]
    fn parse_empty_html() {
        let results = parse_ddg_html("", 10);
        assert!(results.is_empty());
    }

    #[test]
    fn parse_ddg_result_block() {
        let html = r#"
        <div class="result">
            <h2><a class="result__a" href="https://example.com">Example Title</a></h2>
            <a class="result__snippet">This is the snippet text.</a>
        </div>
        "#;
        let results = parse_ddg_html(html, 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Example Title");
        assert_eq!(results[0].url, "https://example.com");
        assert_eq!(results[0].snippet, "This is the snippet text.");
    }

    #[test]
    fn parse_respects_max_results() {
        let html = r#"
        <a class="result__a" href="https://a.com">A</a><a class="result__snippet">snip a</a>
        <a class="result__a" href="https://b.com">B</a><a class="result__snippet">snip b</a>
        <a class="result__a" href="https://c.com">C</a><a class="result__snippet">snip c</a>
        "#;
        let results = parse_ddg_html(html, 2);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn tool_definition_valid() {
        let tool = WebSearchTool::new();
        assert_eq!(tool.name(), "web_search");
        let def = tool.definition();
        assert!(def.input_schema["required"]
            .as_array()
            .unwrap()
            .contains(&json!("query")));
    }
}
