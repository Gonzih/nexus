//! InstallSkillTool — LLM-powered skill installation from any content.
//!
//! The agent calls `install_skill` with a URL or raw content. The tool:
//! 1. Fetches the URL (if provided) via HTTP GET
//! 2. If the content has YAML frontmatter (---), parses directly as a skill
//! 3. If not, calls the LLM to auto-generate Lua skill definitions from the docs
//! 4. Registers each generated skill as a new tool via `DynamicToolHandle`
//! 5. Persists skills to `.amai-skills/` for auto-reload next session

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::mpsc;

use soul_core::error::SoulResult;
use soul_core::provider::Provider;
use soul_core::skill::{
    parser::parse_skill, LuaSkillExecutor, ShellSkillExecutor, SkillExecution, SkillToolBridge,
};
use soul_core::tool::{DynamicToolHandle, Tool, ToolOutput};
use soul_core::types::{AuthProfile, Message, ModelInfo, ToolDefinition};
use soul_core::vexec::NativeExecutor;

/// Maximum response size when fetching a URL (100KB).
const MAX_FETCH_BYTES: usize = 100 * 1024;

/// HTTP timeout for fetching skill URLs.
const FETCH_TIMEOUT_SECS: u64 = 30;

/// Extract a namespace from a URL's hostname.
///
/// `https://id.amai.net/skill.md` → `"id_amai_net"`
/// `https://api.example.com/docs` → `"api_example_com"`
fn namespace_from_url(url: &str) -> Option<String> {
    // Simple URL parsing — extract host between :// and first /
    let after_scheme = url.strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let host = after_scheme.split('/').next()?;
    // Strip port if present
    let host = host.split(':').next()?;
    if host.is_empty() {
        return None;
    }
    // Replace dots with underscores
    Some(host.replace('.', "_"))
}

/// Apply a namespace prefix to a skill name.
///
/// `("id_amai_net", "register_identity")` → `"id_amai_net__register_identity"`
///
/// If the name already starts with the namespace prefix, returns it unchanged.
fn namespaced_name(namespace: &str, name: &str) -> String {
    let prefix = format!("{namespace}__");
    if name.starts_with(&prefix) {
        name.to_string()
    } else {
        format!("{namespace}__{name}")
    }
}

const SKILL_GEN_PROMPT: &str = r###"You are a skill generator. Given API documentation, generate one or more skill definitions as YAML frontmatter markdown blocks.

Each skill MUST use this exact format:

```
---
name: short_snake_case_name
description: One-line description of what this tool does
input_schema:
  type: object
  properties:
    param_name:
      type: string
      description: What this parameter is
  required:
    - param_name
execution:
  type: lua
  code: |
    local url = "https://api.example.com/" .. args.param_name
    local body = fetch(url)
    return body
  timeout_secs: 30
---
```

Rules:
- Use `type: lua` for any API with multiple endpoints, parameters, or JSON responses
- Available Lua functions: fetch(url), post(url, body, content_type), json_decode(s), json_encode(t), url_encode(s)
- Input args are available as `args.param_name`
- Use `type: shell` only for trivial single-command tools
- Create ONE skill per distinct API operation (e.g., register, get_identity, list_identities)
- Keep names short and descriptive (max 30 chars)
- Do NOT include authentication/signing — the agent handles that separately

Output ONLY the skill markdown blocks separated by blank lines. No explanations, no commentary."###;

/// Tool that installs skills at runtime from URLs or raw content.
///
/// LLM-powered: if content isn't a skill file, the LLM auto-generates
/// Lua skills from the documentation.
pub struct InstallSkillTool {
    dynamic_handle: DynamicToolHandle,
    persist_dir: Option<PathBuf>,
    provider: Arc<dyn Provider>,
    model: ModelInfo,
    auth: AuthProfile,
}

impl InstallSkillTool {
    pub fn new(
        dynamic_handle: DynamicToolHandle,
        provider: Arc<dyn Provider>,
        model: ModelInfo,
        auth: AuthProfile,
    ) -> Self {
        Self {
            dynamic_handle,
            persist_dir: None,
            provider,
            model,
            auth,
        }
    }

    /// Enable disk persistence — skills will be saved to this directory.
    pub fn with_persist_dir(mut self, dir: PathBuf) -> Self {
        self.persist_dir = Some(dir);
        self
    }

    /// Use the LLM to generate skill definitions from documentation content.
    async fn generate_skills_from_docs(&self, docs: &str, namespace: Option<&str>) -> SoulResult<Vec<String>> {
        let ns_instruction = if let Some(ns) = namespace {
            format!(
                "\n\nIMPORTANT: Prefix ALL skill names with \"{ns}__\" to namespace them. \
                 For example, if the skill would be named \"register\", name it \"{ns}__register\" instead."
            )
        } else {
            String::new()
        };
        let user_msg = format!(
            "Generate Lua skill definitions from this API documentation:{ns_instruction}\n\n{docs}"
        );
        let messages = vec![Message::user(user_msg)];

        let (tx, _rx) = mpsc::unbounded_channel();
        let response = self
            .provider
            .stream(
                &messages,
                SKILL_GEN_PROMPT,
                &[],
                &self.model,
                &self.auth,
                tx,
            )
            .await?;

        let text = response.text_content();

        // Extract skill blocks — each starts with "---" and ends with "---"
        let mut skills = Vec::new();
        let mut current = String::new();
        let mut in_frontmatter = false;
        let mut found_first_delimiter = false;

        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed == "---" {
                if !found_first_delimiter {
                    // Start of first skill
                    found_first_delimiter = true;
                    in_frontmatter = true;
                    current.push_str("---\n");
                } else if in_frontmatter {
                    // End of frontmatter
                    current.push_str("---\n");
                    in_frontmatter = false;
                } else {
                    // Start of next skill — save previous
                    if !current.is_empty() {
                        skills.push(current.clone());
                    }
                    current = String::from("---\n");
                    in_frontmatter = true;
                    found_first_delimiter = true;
                }
            } else if found_first_delimiter {
                current.push_str(line);
                current.push('\n');
            }
        }

        // Save the last skill
        if !current.is_empty() && current.contains("---") {
            skills.push(current);
        }

        Ok(skills)
    }

    /// Register a single parsed skill, returning its name on success.
    ///
    /// If `namespace` is provided, the skill name is prefixed: `namespace__original_name`.
    fn register_skill(&self, markdown: &str, namespace: Option<&str>) -> Result<String, String> {
        let mut skill = parse_skill(markdown).map_err(|e| format!("Parse error: {e}"))?;

        // Apply namespace prefix to avoid collisions between services
        if let Some(ns) = namespace {
            skill.name = namespaced_name(ns, &skill.name);
        }

        let skill_name = skill.name.clone();

        let executor: Arc<dyn soul_core::skill::SkillExecutor> = match &skill.execution {
            SkillExecution::Lua { .. } => Arc::new(LuaSkillExecutor::new()),
            SkillExecution::Shell { .. } => {
                Arc::new(ShellSkillExecutor::new(Arc::new(NativeExecutor)))
            }
            _ => Arc::new(ShellSkillExecutor::new(Arc::new(NativeExecutor))),
        };

        let bridge = SkillToolBridge::new(skill, executor);
        self.dynamic_handle.register(Arc::new(bridge));

        // Persist to disk
        if let Some(ref dir) = self.persist_dir {
            std::fs::create_dir_all(dir).ok();
            let path = dir.join(format!("{skill_name}.skill"));
            if let Err(e) = std::fs::write(&path, markdown) {
                tracing::warn!(skill = %skill_name, error = %e, "Failed to persist skill");
            } else {
                tracing::info!(skill = %skill_name, path = %path.display(), "Skill persisted");
            }
        }

        Ok(skill_name)
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl Tool for InstallSkillTool {
    fn name(&self) -> &str {
        "install_skill"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "install_skill".into(),
            description: "Install skills from a URL or raw content. Accepts BOTH: \
                (1) skill files with YAML frontmatter (installed directly), or \
                (2) API documentation / markdown (LLM auto-generates Lua skills from it). \
                Skills persist to .amai-skills/ and auto-load next session."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "URL to fetch content from — can be a skill file OR API documentation. Provide either url or content."
                    },
                    "content": {
                        "type": "string",
                        "description": "Raw content — can be skill markdown with YAML frontmatter OR API documentation. Provide either url or content."
                    }
                }
            }),
        }
    }

    async fn execute(
        &self,
        _call_id: &str,
        arguments: serde_json::Value,
        _partial_tx: Option<mpsc::UnboundedSender<String>>,
    ) -> SoulResult<ToolOutput> {
        let url = arguments.get("url").and_then(|v| v.as_str());
        let content = arguments.get("content").and_then(|v| v.as_str());

        // Extract namespace from URL hostname (e.g. "id_amai_net" from "https://id.amai.net/...")
        let namespace = url.and_then(namespace_from_url);
        let ns = namespace.as_deref();

        if let Some(ref ns_str) = namespace {
            tracing::info!(namespace = %ns_str, "Skill namespace derived from URL");
        }

        // Get the content — either from URL or direct
        let raw_content = match (url, content) {
            (Some(url), _) => match fetch_url(url).await {
                Ok(body) => body,
                Err(e) => return Ok(ToolOutput::error(format!("Failed to fetch {url}: {e}"))),
            },
            (_, Some(content)) => content.to_string(),
            (None, None) => {
                return Ok(ToolOutput::error(
                    "install_skill: either 'url' or 'content' is required",
                ));
            }
        };

        // Try direct parse first — if it has valid YAML frontmatter, use it
        if raw_content.trim_start().starts_with("---") {
            if let Ok(_) = parse_skill(&raw_content) {
                return match self.register_skill(&raw_content, ns) {
                    Ok(name) => {
                        let count = self.dynamic_handle.len();
                        Ok(ToolOutput::success(format!(
                            "Skill '{name}' installed (persisted to .amai-skills/).\n\
                             Total dynamic tools: {count}\n\
                             Call it by name on your next turn."
                        )))
                    }
                    Err(e) => Ok(ToolOutput::error(e)),
                };
            }
        }

        // Not a skill file — treat as documentation, use LLM to generate skills
        tracing::info!("Content is not a skill file, using LLM to generate skills from docs");

        let skill_blocks = match self.generate_skills_from_docs(&raw_content, ns).await {
            Ok(blocks) => blocks,
            Err(e) => {
                return Ok(ToolOutput::error(format!(
                    "Failed to generate skills from documentation: {e}"
                )));
            }
        };

        if skill_blocks.is_empty() {
            return Ok(ToolOutput::error(
                "LLM did not generate any valid skill definitions from the documentation. \
                 The content may not contain API endpoints.",
            ));
        }

        let mut installed = Vec::new();
        let mut errors = Vec::new();

        for block in &skill_blocks {
            match self.register_skill(block, ns) {
                Ok(name) => installed.push(name),
                Err(e) => errors.push(e),
            }
        }

        let count = self.dynamic_handle.len();
        let mut result = format!(
            "Auto-generated and installed {} skills from documentation (persisted to .amai-skills/):\n",
            installed.len()
        );
        for name in &installed {
            result.push_str(&format!("  - {name}\n"));
        }
        if !errors.is_empty() {
            result.push_str(&format!("\n{} skills failed to parse:\n", errors.len()));
            for e in &errors {
                result.push_str(&format!("  - {e}\n"));
            }
        }
        result.push_str(&format!("\nTotal dynamic tools: {count}\n"));
        result.push_str("Call any of these by name on your next turn.");

        Ok(ToolOutput::success(result))
    }
}

/// Fetch a URL and return the body as a string.
async fn fetch_url(url: &str) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(FETCH_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| format!("HTTP client error: {e}"))?;

    let response = client
        .get(url)
        .header("User-Agent", "amai-agent/0.1")
        .send()
        .await
        .map_err(|e| format!("HTTP request failed: {e}"))?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {status} from {url}"));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("Failed to read response body: {e}"))?;

    if bytes.len() > MAX_FETCH_BYTES {
        return Err(format!(
            "Response too large: {} bytes (max {})",
            bytes.len(),
            MAX_FETCH_BYTES
        ));
    }

    String::from_utf8(bytes.to_vec())
        .map_err(|e| format!("Response is not valid UTF-8: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use soul_core::tool::ToolRegistry;
    use soul_core::types::ProviderKind;

    use soul_core::provider::ProbeResult;

    // Mock provider for tests — returns empty responses
    struct MockProvider;

    #[async_trait]
    impl Provider for MockProvider {
        fn kind(&self) -> ProviderKind {
            ProviderKind::Custom("mock".into())
        }

        async fn stream(
            &self,
            _messages: &[Message],
            _system: &str,
            _tools: &[ToolDefinition],
            _model: &ModelInfo,
            _auth: &AuthProfile,
            _stream_tx: mpsc::UnboundedSender<soul_core::types::StreamDelta>,
        ) -> SoulResult<Message> {
            Ok(Message::assistant(""))
        }

        async fn count_tokens(
            &self,
            _messages: &[Message],
            _system: &str,
            _tools: &[ToolDefinition],
            _model: &ModelInfo,
            _auth: &AuthProfile,
        ) -> SoulResult<usize> {
            Ok(0)
        }

        async fn probe(
            &self,
            _model: &ModelInfo,
            _auth: &AuthProfile,
        ) -> SoulResult<ProbeResult> {
            Ok(ProbeResult {
                healthy: true,
                rate_limit_remaining: None,
                rate_limit_utilization: None,
            })
        }
    }

    fn test_model() -> ModelInfo {
        ModelInfo {
            id: "test".into(),
            provider: ProviderKind::Custom("test".into()),
            context_window: 8192,
            max_output_tokens: 4096,
            supports_thinking: false,
            supports_tools: false,
            supports_images: false,
            cost_per_input_token: 0.0,
            cost_per_output_token: 0.0,
        }
    }

    fn make_install_tool() -> (InstallSkillTool, DynamicToolHandle) {
        let registry = ToolRegistry::new();
        let handle = registry.dynamic_handle();
        let provider: Arc<dyn Provider> = Arc::new(MockProvider);
        let tool = InstallSkillTool::new(
            handle.clone(),
            provider,
            test_model(),
            AuthProfile::new(ProviderKind::Custom("test".into()), ""),
        );
        (tool, handle)
    }

    #[test]
    fn install_skill_definition() {
        let (tool, _) = make_install_tool();
        let def = tool.definition();
        assert_eq!(def.name, "install_skill");
        assert!(def.description.contains("skill"));
        assert!(def.input_schema["properties"]["url"].is_object());
        assert!(def.input_schema["properties"]["content"].is_object());
    }

    #[test]
    fn install_skill_name() {
        let (tool, _) = make_install_tool();
        assert_eq!(tool.name(), "install_skill");
    }

    #[tokio::test]
    async fn install_from_raw_skill_content() {
        let (tool, handle) = make_install_tool();

        let skill_md = r#"---
name: greet
description: Greet someone
input_schema:
  type: object
  properties:
    name:
      type: string
  required:
    - name
execution:
  type: shell
  command_template: "echo Hello, {{name}}!"
---
A simple greeting skill.
"#;

        let result = tool
            .execute("c1", json!({"content": skill_md}), None)
            .await
            .unwrap();

        assert!(!result.is_error, "Error: {}", result.content);
        assert!(result.content.contains("greet"));
        assert!(result.content.contains("installed"));
        assert_eq!(handle.len(), 1);
    }

    #[tokio::test]
    async fn install_lua_skill() {
        let (tool, handle) = make_install_tool();

        let skill_md = r#"---
name: api_health
description: Check API health
execution:
  type: lua
  code: |
    return fetch("https://api.example.com/health")
  timeout_secs: 10
---
"#;

        let result = tool
            .execute("c1", json!({"content": skill_md}), None)
            .await
            .unwrap();

        assert!(!result.is_error, "Error: {}", result.content);
        assert!(result.content.contains("api_health"));
        assert_eq!(handle.len(), 1);
    }

    #[tokio::test]
    async fn install_docs_with_no_skills_generated() {
        let (tool, handle) = make_install_tool();

        // Mock provider returns empty response, so no skills generated
        let result = tool
            .execute("c1", json!({"content": "# API Docs\nSome documentation here"}), None)
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("did not generate"));
        assert_eq!(handle.len(), 0);
    }

    #[tokio::test]
    async fn install_missing_params_returns_error() {
        let (tool, _) = make_install_tool();

        let result = tool.execute("c1", json!({}), None).await.unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("required"));
    }

    #[tokio::test]
    async fn install_multiple_skills() {
        let (tool, handle) = make_install_tool();

        let skill_a = r#"---
name: skill_a
description: First skill
execution:
  type: shell
  command_template: "echo a"
---
"#;
        let skill_b = r#"---
name: skill_b
description: Second skill
execution:
  type: shell
  command_template: "echo b"
---
"#;

        tool.execute("c1", json!({"content": skill_a}), None)
            .await
            .unwrap();
        tool.execute("c2", json!({"content": skill_b}), None)
            .await
            .unwrap();

        assert_eq!(handle.len(), 2);
    }

    #[tokio::test]
    async fn install_llm_delegate_skill() {
        let (tool, handle) = make_install_tool();

        let skill_md = r#"---
name: summarize
description: Summarize text
execution:
  type: llm_delegate
  system_prompt: You are a summarizer.
---
"#;

        let result = tool
            .execute("c1", json!({"content": skill_md}), None)
            .await
            .unwrap();

        assert!(!result.is_error, "Error: {}", result.content);
        assert!(result.content.contains("summarize"));
        assert_eq!(handle.len(), 1);
    }

    #[tokio::test]
    async fn dynamic_handle_shared_across_instances() {
        let registry = ToolRegistry::new();
        let handle1 = registry.dynamic_handle();
        let handle2 = registry.dynamic_handle();

        let provider: Arc<dyn Provider> = Arc::new(MockProvider);
        let tool1 = InstallSkillTool::new(
            handle1.clone(),
            provider,
            test_model(),
            AuthProfile::new(ProviderKind::Custom("test".into()), ""),
        );

        let skill_md = r#"---
name: shared_test
description: Test sharing
execution:
  type: shell
  command_template: "echo test"
---
"#;

        tool1
            .execute("c1", json!({"content": skill_md}), None)
            .await
            .unwrap();

        assert_eq!(handle1.len(), 1);
        assert_eq!(handle2.len(), 1);

        let defs = registry.definitions();
        assert!(defs.iter().any(|d| d.name == "shared_test"));
    }

    // ─── Namespace Tests ────────────────────────────────────────────

    #[test]
    fn namespace_from_url_extracts_host() {
        assert_eq!(
            namespace_from_url("https://id.amai.net/skill.md"),
            Some("id_amai_net".into())
        );
        assert_eq!(
            namespace_from_url("https://api.example.com/docs/v1"),
            Some("api_example_com".into())
        );
        assert_eq!(
            namespace_from_url("http://localhost:8080/health"),
            Some("localhost".into())
        );
    }

    #[test]
    fn namespace_from_url_returns_none_for_invalid() {
        assert_eq!(namespace_from_url("not-a-url"), None);
        assert_eq!(namespace_from_url(""), None);
    }

    #[test]
    fn namespaced_name_prefixes_correctly() {
        assert_eq!(
            namespaced_name("id_amai_net", "register_identity"),
            "id_amai_net__register_identity"
        );
    }

    #[test]
    fn namespaced_name_avoids_double_prefix() {
        assert_eq!(
            namespaced_name("id_amai_net", "id_amai_net__register_identity"),
            "id_amai_net__register_identity"
        );
    }

    #[test]
    fn register_skill_with_namespace() {
        let (tool, handle) = make_install_tool();

        let skill_md = r#"---
name: health_check
description: Check health
execution:
  type: shell
  command_template: "echo ok"
---
"#;
        let name = tool.register_skill(skill_md, Some("id_amai_net")).unwrap();
        assert_eq!(name, "id_amai_net__health_check");
        assert_eq!(handle.len(), 1);
    }

    #[test]
    fn register_skill_without_namespace() {
        let (tool, handle) = make_install_tool();

        let skill_md = r#"---
name: health_check
description: Check health
execution:
  type: shell
  command_template: "echo ok"
---
"#;
        let name = tool.register_skill(skill_md, None).unwrap();
        assert_eq!(name, "health_check");
        assert_eq!(handle.len(), 1);
    }
}
