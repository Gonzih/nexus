use async_trait::async_trait;
use serde_json::json;
use soul_core::tool::{Tool, ToolOutput};
use soul_core::ToolDefinition;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// ArXiv paper search tool — queries the ArXiv Atom API (no key required).
///
/// Rate limit: ~1 request per 3 seconds (enforced by ArXiv ToS).
/// Returns structured results: title, authors, summary, link, published date.
pub struct ArxivSearchTool;

impl ArxivSearchTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ArxivSearchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ArxivSearchTool {
    fn name(&self) -> &str {
        "arxiv_search"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "arxiv_search".into(),
            description: "Search ArXiv for academic papers. No API key required. \
                          Returns title, authors, abstract, link, and publication date. \
                          Use for finding research papers on ML, AI, medical informatics, \
                          physics, math, cs, etc. Max 10 results per query. \
                          Note: respect ArXiv rate limits — avoid calling more than once every 3 seconds.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query. Supports ArXiv query syntax: \
                                        field prefixes (ti: title, au: author, abs: abstract, cat: category), \
                                        boolean operators (AND, OR, ANDNOT). \
                                        Examples: 'RAG medical LLM', 'ti:BioMistral', \
                                        'cat:cs.LG AND ti:transformer'"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Maximum number of results to return (1-10, default 5)",
                        "minimum": 1,
                        "maximum": 10
                    },
                    "sort_by": {
                        "type": "string",
                        "description": "Sort order: 'relevance' (default), 'lastUpdatedDate', 'submittedDate'",
                        "enum": ["relevance", "lastUpdatedDate", "submittedDate"]
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
            .trim()
            .to_string();

        if query.is_empty() {
            return Ok(ToolOutput::error("Missing required parameter: query"));
        }

        let max_results = arguments
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(5)
            .min(10)
            .max(1);

        let sort_by = arguments
            .get("sort_by")
            .and_then(|v| v.as_str())
            .unwrap_or("relevance");

        let sort_order = match sort_by {
            "lastUpdatedDate" | "submittedDate" => "descending",
            _ => "descending",
        };

        // URL-encode the query
        let encoded_query = url_encode(&query);
        let url = format!(
            "http://export.arxiv.org/api/query?search_query={}&max_results={}&sortBy={}&sortOrder={}",
            encoded_query, max_results, sort_by, sort_order
        );

        debug!(query = %query, max_results, "arxiv_search: querying");

        let client = reqwest::Client::builder()
            .user_agent("amai-agent/1.0 (research; mailto:research@amai.net)")
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| soul_core::error::SoulError::Provider(e.to_string()))?;

        match client.get(&url).send().await {
            Ok(resp) => {
                if !resp.status().is_success() {
                    let status = resp.status();
                    warn!(query = %query, status = %status, "arxiv_search: API error");
                    return Ok(ToolOutput::error(format!("ArXiv API returned status {status}")));
                }

                match resp.text().await {
                    Ok(body) => {
                        debug!(query = %query, body_len = body.len(), "arxiv_search: received response");
                        let papers = parse_arxiv_atom(&body);
                        if papers.is_empty() {
                            Ok(ToolOutput::success(format!(
                                "No results found for query: {query}"
                            )))
                        } else {
                            let count = papers.len();
                            let output = serde_json::to_string_pretty(&json!({
                                "query": query,
                                "count": count,
                                "papers": papers,
                            }))
                            .unwrap_or_else(|_| format!("Found {count} papers"));
                            Ok(ToolOutput::success(output))
                        }
                    }
                    Err(e) => {
                        warn!(query = %query, error = %e, "arxiv_search: body read failed");
                        Ok(ToolOutput::error(format!("Failed to read ArXiv response: {e}")))
                    }
                }
            }
            Err(e) => {
                warn!(query = %query, error = %e, "arxiv_search: request failed");
                Ok(ToolOutput::error(format!("ArXiv request failed: {e}")))
            }
        }
    }
}

/// Parse ArXiv Atom XML response into structured paper list.
///
/// Extracts from each `<entry>`: title, authors, summary, id (link), published.
/// Uses simple tag extraction — robust enough for well-formed ArXiv Atom feeds.
fn parse_arxiv_atom(xml: &str) -> Vec<serde_json::Value> {
    let mut papers = Vec::new();

    // Split on entry tags
    let entries: Vec<&str> = xml.split("<entry>").skip(1).collect();

    for entry in entries {
        let end = entry.find("</entry>").unwrap_or(entry.len());
        let entry = &entry[..end];

        let title = extract_tag(entry, "title")
            .map(clean_whitespace)
            .unwrap_or_default();
        let summary = extract_tag(entry, "summary")
            .map(clean_whitespace)
            .unwrap_or_default();
        let published = extract_tag(entry, "published")
            .map(|s| s.trim().to_string())
            .unwrap_or_default();

        // ArXiv ID is in <id>http://arxiv.org/abs/XXXX</id>
        let arxiv_id = extract_tag(entry, "id")
            .map(|s| s.trim().to_string())
            .unwrap_or_default();

        // Authors: multiple <author><name>...</name></author>
        let authors = extract_all_tags(entry, "name");

        // Truncate summary to 500 chars for readability
        let summary_short = if summary.len() > 500 {
            format!("{}...", &summary[..500])
        } else {
            summary
        };

        if !title.is_empty() {
            papers.push(json!({
                "title": title,
                "authors": authors,
                "summary": summary_short,
                "link": arxiv_id,
                "published": published,
            }));
        }
    }

    papers
}

/// Extract text content of the first occurrence of `<tag>...</tag>`.
fn extract_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].to_string())
}

/// Extract text content of all occurrences of `<tag>...</tag>`.
fn extract_all_tags(xml: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut results = Vec::new();
    let mut remaining = xml;
    while let Some(start_pos) = remaining.find(&open) {
        let after_open = start_pos + open.len();
        if let Some(end_pos) = remaining[after_open..].find(&close) {
            let text = remaining[after_open..after_open + end_pos].to_string();
            results.push(text);
            remaining = &remaining[after_open + end_pos + close.len()..];
        } else {
            break;
        }
    }
    results
}

/// Collapse whitespace and trim.
fn clean_whitespace(s: String) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Simple URL percent-encoding for query parameters.
fn url_encode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            ' ' => "+".to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_name_and_schema() {
        let t = ArxivSearchTool::new();
        assert_eq!(t.name(), "arxiv_search");
        let def = t.definition();
        assert_eq!(def.name, "arxiv_search");
        let required = def.input_schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("query")));
    }

    #[tokio::test]
    async fn empty_query_returns_error() {
        let t = ArxivSearchTool::new();
        let out = t.execute("c1", json!({"query": ""}), None).await.unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn missing_query_returns_error() {
        let t = ArxivSearchTool::new();
        let out = t.execute("c2", json!({}), None).await.unwrap();
        assert!(out.is_error);
    }

    #[test]
    fn parse_arxiv_atom_sample() {
        let sample = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed>
  <entry>
    <id>http://arxiv.org/abs/2401.12345v1</id>
    <title>Sample Medical LLM Paper</title>
    <author><name>Alice Smith</name></author>
    <author><name>Bob Jones</name></author>
    <summary>This is the abstract of the paper about medical AI systems.</summary>
    <published>2024-01-15T00:00:00Z</published>
  </entry>
  <entry>
    <id>http://arxiv.org/abs/2401.99999v2</id>
    <title>Another Paper</title>
    <author><name>Carol White</name></author>
    <summary>Second paper abstract text here.</summary>
    <published>2024-01-20T00:00:00Z</published>
  </entry>
</feed>"#;

        let papers = parse_arxiv_atom(sample);
        assert_eq!(papers.len(), 2);

        let first = &papers[0];
        assert_eq!(first["title"].as_str().unwrap(), "Sample Medical LLM Paper");
        assert_eq!(first["authors"].as_array().unwrap().len(), 2);
        assert!(first["summary"].as_str().unwrap().contains("medical AI"));
        assert_eq!(first["link"].as_str().unwrap(), "http://arxiv.org/abs/2401.12345v1");

        let second = &papers[1];
        assert_eq!(second["title"].as_str().unwrap(), "Another Paper");
        assert_eq!(second["authors"].as_array().unwrap()[0].as_str().unwrap(), "Carol White");
    }

    #[test]
    fn parse_empty_feed() {
        let empty = r#"<?xml version="1.0"?><feed></feed>"#;
        let papers = parse_arxiv_atom(empty);
        assert!(papers.is_empty());
    }

    #[test]
    fn url_encode_basic() {
        assert_eq!(url_encode("hello world"), "hello+world");
        assert_eq!(url_encode("RAG & LLM"), "RAG+%26+LLM");
        assert_eq!(url_encode("cat:cs.LG"), "cat%3Acs.LG");
    }
}
