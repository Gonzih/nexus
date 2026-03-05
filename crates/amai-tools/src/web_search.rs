use async_trait::async_trait;
use serde_json::json;
use soul_core::tool::{Tool, ToolOutput};
use soul_core::ToolDefinition;
use std::sync::OnceLock;
use tokio::sync::{mpsc, Semaphore};
use tracing::{debug, warn};

/// Global semaphore limiting DuckDuckGo requests to 1 concurrent.
/// DuckDuckGo HTML endpoint blocks IPs that fire parallel requests.
static DDG_SEMAPHORE: OnceLock<Semaphore> = OnceLock::new();

fn ddg_semaphore() -> &'static Semaphore {
    DDG_SEMAPHORE.get_or_init(|| Semaphore::new(1))
}

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

        // In native mode (no proxy), try the DDG Instant Answer JSON API first.
        // This API is not IP-rate-limited and works on all machines. Falls back to
        // HTML scraping (via curl) if the JSON API returns no usable results.
        let body = if self.proxy_base_url.is_none() {
            // Acquire semaphore to serialize DDG requests
            let _permit = ddg_semaphore().acquire().await.expect("semaphore not closed");

            // Try DDG Instant Answer JSON API first — works on all IPs
            let json_url = format!(
                "https://api.duckduckgo.com/?q={}&format=json&no_html=1&skip_disambig=1",
                urlencoding(&query)
            );
            debug!(query = %query, "web_search: trying DDG JSON API");

            let json_result = tokio::process::Command::new("curl")
                .args(["-s", "--max-time", "10", &json_url])
                .output()
                .await;

            let json_output = match &json_result {
                Ok(out) if out.status.success() && !out.stdout.is_empty() => {
                    let text = String::from_utf8_lossy(&out.stdout);
                    parse_ddg_json(&text, max_results)
                }
                _ => vec![],
            };

            if !json_output.is_empty() {
                let mut out = format!("Search results for: {query}\n\n");
                for (i, r) in json_output.iter().enumerate() {
                    out.push_str(&format!("{}. {}\n   {}\n   {}\n\n", i + 1, r.title, r.url, r.snippet));
                }
                return Ok(ToolOutput::success(out).with_metadata(json!({
                    "result_count": json_output.len(),
                    "query": query,
                    "source": "ddg_json",
                })));
            }

            // Fallback: HTML scraping via curl (browser-compatible TLS fingerprint)
            let ddg_url = format!("https://html.duckduckgo.com/html/?q={}", urlencoding(&query));
            debug!(query = %query, "web_search: falling back to DDG HTML scraping");

            match tokio::process::Command::new("curl")
                .args([
                    "-s",
                    "--max-time", "15",
                    "-A", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
                    "-L",
                    "--compressed",
                    &ddg_url,
                ])
                .output()
                .await
            {
                Ok(out) if out.status.success() && !out.stdout.is_empty() => {
                    let text = String::from_utf8_lossy(&out.stdout);
                    if text.len() > MAX_BODY_BYTES {
                        text[..MAX_BODY_BYTES].to_string()
                    } else {
                        text.to_string()
                    }
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    warn!(query = %query, stderr = %stderr, "web_search: curl failed");
                    return Ok(ToolOutput::error(format!("Search failed: {stderr}")));
                }
                Err(e) => {
                    warn!(query = %query, error = %e, "web_search: curl exec failed");
                    return Ok(ToolOutput::error(format!("Search failed: {e}")));
                }
            }
        } else {
            // Proxy mode (WASM): use reqwest through the proxy
            let proxy_url = self.proxy_base_url.as_ref().unwrap();
            let ddg_url = format!("https://html.duckduckgo.com/html/?q={}", urlencoding(&query));
            let fetch_url = format!("{}/proxy/fetch?url={}", proxy_url, urlencoding(&ddg_url));

            let client = reqwest::Client::builder()
                .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new());

            match client.get(&fetch_url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    match resp.text().await {
                        Ok(text) => if text.len() > MAX_BODY_BYTES { text[..MAX_BODY_BYTES].to_string() } else { text },
                        Err(e) => return Ok(ToolOutput::error(format!("Failed to read response: {e}"))),
                    }
                }
                Ok(resp) => return Ok(ToolOutput::error(format!("Search failed: HTTP {}", resp.status()))),
                Err(e) => return Ok(ToolOutput::error(format!("Search failed: {e}"))),
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
/// Parse DuckDuckGo Instant Answer JSON API response into search results.
///
/// The JSON API returns an Abstract (Wikipedia summary), RelatedTopics, and Results.
/// This is not a full web search but works on IPs where the HTML endpoint is blocked.
fn parse_ddg_json(json_str: &str, max_results: usize) -> Vec<SearchResult> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json_str) else {
        return vec![];
    };

    let mut results = Vec::new();

    // Include the abstract (Wikipedia summary) if present
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

    // Include Results (official sites etc.)
    if let Some(res_arr) = v.get("Results").and_then(|v| v.as_array()) {
        for item in res_arr.iter().take(max_results.saturating_sub(results.len())) {
            let text = item.get("Text").and_then(|v| v.as_str()).unwrap_or("");
            let url = item.get("FirstURL").and_then(|v| v.as_str()).unwrap_or("");
            if !url.is_empty() {
                results.push(SearchResult {
                    title: text.into(),
                    url: url.into(),
                    snippet: String::new(),
                });
            }
        }
    }

    // Include RelatedTopics
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
                results.push(SearchResult {
                    title: title.into(),
                    url: url.into(),
                    snippet: snippet.into(),
                });
            }
        }
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
