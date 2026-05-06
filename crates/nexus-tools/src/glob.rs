use async_trait::async_trait;
use serde_json::json;
use soul_core::tool::{Tool, ToolOutput};
use soul_core::ToolDefinition;
use tokio::sync::mpsc;
use tracing::debug;

/// Maximum number of matching files to return.
const MAX_RESULTS: usize = 1000;

/// Fast file pattern matching tool that works with any codebase size.
///
/// Supports glob patterns like `**/*.rs`, `src/**/*.ts`, `*.toml`.
/// Returns matching file paths sorted by modification time (most recent first).
/// Uses recursive directory walking with glob pattern matching.
///
/// This complements soul-coder's `find` tool by providing more expressive
/// glob patterns (double-star `**` for recursive matching) and sorting by
/// modification time rather than alphabetically.
pub struct GlobTool {
    cwd: String,
}

impl GlobTool {
    pub fn new(cwd: impl Into<String>) -> Self {
        Self { cwd: cwd.into() }
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "glob".into(),
            description: "Fast file pattern matching. Find files by glob patterns like '**/*.rs', 'src/**/*.ts', '*.toml'. Returns matching paths sorted by modification time (most recent first). Supports ** for recursive directory matching.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern to match files (e.g., '**/*.rs', 'src/**/*.ts', '*.toml')"
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory to search in (default: current working directory)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of results to return (default: 200, max: 1000)"
                    }
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn execute(
        &self,
        _call_id: &str,
        arguments: serde_json::Value,
        _partial_tx: Option<mpsc::UnboundedSender<String>>,
    ) -> soul_core::error::SoulResult<ToolOutput> {
        let pattern = arguments
            .get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if pattern.is_empty() {
            return Ok(ToolOutput::error("No glob pattern provided"));
        }

        let search_dir = arguments
            .get("path")
            .and_then(|v| v.as_str())
            .map(|p| resolve_path(&self.cwd, p))
            .unwrap_or_else(|| self.cwd.clone());

        let limit = arguments
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).min(MAX_RESULTS))
            .unwrap_or(200);

        debug!(
            pattern = %pattern,
            search_dir = %search_dir,
            limit,
            "Executing glob search"
        );

        let matches = glob_walk(&search_dir, &pattern, limit);

        if matches.is_empty() {
            return Ok(ToolOutput::success(format!(
                "No files matching '{pattern}' found in {search_dir}"
            )));
        }

        // Format relative paths
        let prefix = if search_dir.ends_with('/') {
            &search_dir
        } else {
            &search_dir
        };

        let mut output = String::new();
        for path in &matches {
            let relative = path
                .strip_prefix(prefix)
                .unwrap_or(path)
                .trim_start_matches('/');
            output.push_str(relative);
            output.push('\n');
        }

        debug!(
            pattern = %pattern,
            match_count = matches.len(),
            "Glob search completed"
        );

        Ok(ToolOutput::success(output).with_metadata(json!({
            "match_count": matches.len(),
            "pattern": pattern,
            "search_dir": search_dir,
            "truncated": matches.len() >= limit,
        })))
    }
}

/// Walk a directory recursively and match files against a glob pattern.
/// Returns paths sorted by modification time (most recent first).
fn glob_walk(root: &str, pattern: &str, limit: usize) -> Vec<String> {
    use std::fs;
    use std::path::Path;

    let root_path = Path::new(root);
    if !root_path.exists() || !root_path.is_dir() {
        return Vec::new();
    }

    let mut matches: Vec<(String, std::time::SystemTime)> = Vec::new();
    let mut stack: Vec<std::path::PathBuf> = vec![root_path.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = entry.file_name().to_string_lossy().to_string();

            // Skip hidden directories
            if file_name.starts_with('.') && path.is_dir() {
                continue;
            }

            if path.is_dir() {
                stack.push(path.clone());
            }

            if path.is_file() {
                let relative = path
                    .strip_prefix(root_path)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();

                if glob_match(pattern, &relative) {
                    let mtime = entry
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .unwrap_or(std::time::UNIX_EPOCH);

                    matches.push((path.to_string_lossy().to_string(), mtime));
                }
            }
        }
    }

    // Sort by modification time, most recent first
    matches.sort_by(|a, b| b.1.cmp(&a.1));

    matches
        .into_iter()
        .take(limit)
        .map(|(path, _)| path)
        .collect()
}

/// Simple glob pattern matching.
///
/// Supports:
/// - `*` — match any characters except `/`
/// - `**` — match any characters including `/` (recursive)
/// - `?` — match any single character
/// - Character literals
///
/// Pattern examples:
/// - `*.rs` — all .rs files in current directory
/// - `**/*.rs` — all .rs files recursively
/// - `src/**/*.ts` — all .ts files under src/
/// - `Cargo.toml` — exact match
fn glob_match(pattern: &str, path: &str) -> bool {
    glob_match_inner(pattern.as_bytes(), path.as_bytes())
}

fn glob_match_inner(pattern: &[u8], path: &[u8]) -> bool {
    let mut pi = 0; // pattern index
    let mut si = 0; // string (path) index
    let mut star_pi = usize::MAX; // pattern position after last *
    let mut star_si = usize::MAX; // string position at last *
    let mut dstar_pi = usize::MAX; // pattern position after last **
    let mut dstar_si = usize::MAX; // string position at last **

    while si < path.len() || pi < pattern.len() {
        if pi < pattern.len() {
            // Check for **
            if pi + 1 < pattern.len() && pattern[pi] == b'*' && pattern[pi + 1] == b'*' {
                // ** matches everything including /
                dstar_pi = pi + 2;
                dstar_si = si;
                // Skip any trailing /
                if dstar_pi < pattern.len() && pattern[dstar_pi] == b'/' {
                    dstar_pi += 1;
                }
                pi = dstar_pi;
                continue;
            }

            match pattern[pi] {
                b'*' => {
                    // * matches everything except /
                    star_pi = pi + 1;
                    star_si = si;
                    pi += 1;
                    continue;
                }
                b'?' if si < path.len() && path[si] != b'/' => {
                    pi += 1;
                    si += 1;
                    continue;
                }
                c if si < path.len() && c == path[si] => {
                    pi += 1;
                    si += 1;
                    continue;
                }
                _ => {}
            }
        }

        // Backtrack to * (but not across /)
        if star_pi != usize::MAX && star_si < path.len() && path[star_si] != b'/' {
            star_si += 1;
            si = star_si;
            pi = star_pi;
            continue;
        }

        // Backtrack to ** (can cross /)
        if dstar_pi != usize::MAX && dstar_si < path.len() {
            dstar_si += 1;
            si = dstar_si;
            pi = dstar_pi;
            // Reset single star
            star_pi = usize::MAX;
            star_si = usize::MAX;
            continue;
        }

        return false;
    }

    true
}

fn resolve_path(cwd: &str, path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("{}/{}", cwd.trim_end_matches('/'), path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_match_exact() {
        assert!(glob_match("Cargo.toml", "Cargo.toml"));
        assert!(!glob_match("Cargo.toml", "Cargo.lock"));
    }

    #[test]
    fn glob_match_star() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(glob_match("*.rs", "lib.rs"));
        assert!(!glob_match("*.rs", "main.ts"));
        // * doesn't match /
        assert!(!glob_match("*.rs", "src/main.rs"));
    }

    #[test]
    fn glob_match_double_star() {
        assert!(glob_match("**/*.rs", "main.rs"));
        assert!(glob_match("**/*.rs", "src/main.rs"));
        assert!(glob_match("**/*.rs", "src/deep/nested/main.rs"));
        assert!(!glob_match("**/*.rs", "src/main.ts"));
    }

    #[test]
    fn glob_match_path_prefix() {
        assert!(glob_match("src/**/*.ts", "src/main.ts"));
        assert!(glob_match("src/**/*.ts", "src/components/App.ts"));
        assert!(!glob_match("src/**/*.ts", "lib/main.ts"));
    }

    #[test]
    fn glob_match_question_mark() {
        assert!(glob_match("?.rs", "a.rs"));
        assert!(!glob_match("?.rs", "ab.rs"));
        assert!(!glob_match("?.rs", ".rs"));
    }

    #[test]
    fn tool_definition_valid() {
        let tool = GlobTool::new("/tmp");
        assert_eq!(tool.name(), "glob");
        let def = tool.definition();
        assert!(def.input_schema["required"]
            .as_array()
            .unwrap()
            .contains(&json!("pattern")));
    }

    #[tokio::test]
    async fn empty_pattern_returns_error() {
        let tool = GlobTool::new("/tmp");
        let result = tool
            .execute("test", json!({"pattern": ""}), None)
            .await
            .unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn nonexistent_dir_returns_empty() {
        let tool = GlobTool::new("/nonexistent_dir_xyz");
        let result = tool
            .execute("test", json!({"pattern": "*.rs"}), None)
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("No files matching"));
    }
}
