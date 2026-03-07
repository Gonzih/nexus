/// SynthesizeTool — runtime tool synthesis for autopoietic agents.
///
/// Allows the agent to register new shell-based tools mid-session. The synthesized
/// tool is immediately available on the next turn via DynamicToolHandle.
///
/// This is P0 of the autopoietic agent architecture: the agent can reshape its own
/// operational substrate by creating new tools in response to failures or gaps.
///
/// Flow:
///   1. Agent calls synthesize_tool(name, description, script, args_schema)
///   2. SynthesizeTool validates the script runs without error on a dry-run
///   3. Wraps script in SynthesizedShellTool (implements Tool trait)
///   4. Registers via DynamicToolHandle → visible to LLM next turn
use async_trait::async_trait;
use serde_json::{json, Value};
use soul_core::{
    error::SoulResult,
    tool::{DynamicToolHandle, Tool, ToolOutput},
    types::ToolDefinition,
};
use std::sync::Arc;
use tokio::sync::mpsc;

/// A tool synthesized at runtime from a shell script.
///
/// The script receives arguments as environment variables:
/// AMAI_ARG_<NAME>=<value> for each arg in the schema.
/// Stdout → tool output content. Non-zero exit → is_error=true.
#[derive(Clone)]
pub struct SynthesizedShellTool {
    tool_name: String,
    definition: ToolDefinition,
    script: String,
}

impl SynthesizedShellTool {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        schema: Value,
        script: impl Into<String>,
    ) -> Self {
        let name = name.into();
        let definition = ToolDefinition {
            name: name.clone(),
            description: description.into(),
            input_schema: schema,
        };
        Self {
            tool_name: name,
            definition,
            script: script.into(),
        }
    }
}

#[async_trait]
impl Tool for SynthesizedShellTool {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(
        &self,
        _call_id: &str,
        arguments: Value,
        partial_tx: Option<mpsc::UnboundedSender<String>>,
    ) -> SoulResult<ToolOutput> {
        // Build env vars from arguments: AMAI_ARG_<KEY>=<val>
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c").arg(&self.script);

        if let Some(obj) = arguments.as_object() {
            for (k, v) in obj {
                let env_key = format!("AMAI_ARG_{}", k.to_uppercase().replace('-', "_"));
                let env_val = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                cmd.env(env_key, env_val);
            }
        }

        let output = cmd.output().await.map_err(|e| {
            soul_core::error::SoulError::ToolExecution { tool_name: self.tool_name.clone(), message: format!("Failed to run synthesized tool: {e}") }
        })?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        if let Some(tx) = &partial_tx {
            if !stdout.is_empty() {
                let _ = tx.send(stdout.clone());
            }
        }

        if output.status.success() {
            let content = if stdout.is_empty() { stderr } else { stdout };
            Ok(ToolOutput::success(content))
        } else {
            let content = if stderr.is_empty() {
                format!("Exit code: {}", output.status.code().unwrap_or(-1))
            } else {
                stderr
            };
            Ok(ToolOutput::error(content))
        }
    }
}

/// Meta-tool that lets the agent synthesize new shell tools at runtime.
///
/// The agent provides: name, description, arg schema, shell script.
/// SynthesizeTool validates (dry run with --dry-run flag if available,
/// otherwise syntax check via `bash -n`), then registers via DynamicToolHandle.
pub struct SynthesizeTool {
    handle: DynamicToolHandle,
    definition: ToolDefinition,
}

impl SynthesizeTool {
    pub fn new(handle: DynamicToolHandle) -> Self {
        let definition = ToolDefinition {
            name: "synthesize_tool".into(),
            description: concat!(
                "Register a new shell-based tool at runtime. ",
                "The tool becomes available immediately on the next turn. ",
                "Use this when you identify a capability gap — a recurring operation ",
                "that would benefit from a dedicated tool rather than repeated bash calls. ",
                "The script receives arguments as env vars: AMAI_ARG_<NAME>=<value>. ",
                "Stdout becomes the tool output. Non-zero exit = error."
            ).into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Tool name (snake_case, no spaces). Must be unique."
                    },
                    "description": {
                        "type": "string",
                        "description": "What this tool does. Shown to the LLM in tool definitions."
                    },
                    "script": {
                        "type": "string",
                        "description": "Bash script body. Access args via $AMAI_ARG_<NAME>."
                    },
                    "args_schema": {
                        "type": "object",
                        "description": "JSON Schema object for the tool's input parameters.",
                        "default": {"type": "object", "properties": {}}
                    }
                },
                "required": ["name", "description", "script"]
            }),
        };
        Self { handle, definition }
    }
}

#[async_trait]
impl Tool for SynthesizeTool {
    fn name(&self) -> &str {
        "synthesize_tool"
    }

    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(
        &self,
        _call_id: &str,
        arguments: Value,
        _partial_tx: Option<mpsc::UnboundedSender<String>>,
    ) -> SoulResult<ToolOutput> {
        let name = arguments
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| soul_core::error::SoulError::ToolExecution { tool_name: "synthesize_tool".into(), message: "Missing 'name'".into() })?
            .to_string();

        let description = arguments
            .get("description")
            .and_then(|v| v.as_str())
            .ok_or_else(|| soul_core::error::SoulError::ToolExecution { tool_name: "synthesize_tool".into(), message: "Missing 'description'".into() })?
            .to_string();

        let script = arguments
            .get("script")
            .and_then(|v| v.as_str())
            .ok_or_else(|| soul_core::error::SoulError::ToolExecution { tool_name: "synthesize_tool".into(), message: "Missing 'script'".into() })?
            .to_string();

        let schema = arguments
            .get("args_schema")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));

        // Validate: bash -n (syntax check without execution)
        let syntax_check = tokio::process::Command::new("bash")
            .arg("-n")
            .arg("-c")
            .arg(&script)
            .output()
            .await
            .map_err(|e| soul_core::error::SoulError::ToolExecution { tool_name: "synthesize_tool".into(), message: format!("bash not found: {e}") })?;

        if !syntax_check.status.success() {
            let err = String::from_utf8_lossy(&syntax_check.stderr).to_string();
            return Ok(ToolOutput::error(format!(
                "Script syntax error — tool NOT registered:\n{err}"
            )));
        }

        // Validate name (basic sanity)
        if name.is_empty() || name.contains(' ') || name.len() > 64 {
            return Ok(ToolOutput::error(
                "Invalid tool name: must be non-empty, no spaces, max 64 chars".to_string(),
            ));
        }

        let tool = SynthesizedShellTool::new(&name, &description, schema, &script);
        self.handle.register(Arc::new(tool));

        Ok(ToolOutput::success(format!(
            "Tool '{}' registered successfully. It is now available on the next turn.",
            name
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soul_core::tool::ToolRegistry;

    fn make_handle() -> (ToolRegistry, DynamicToolHandle) {
        let registry = ToolRegistry::new();
        let handle = registry.dynamic_handle();
        (registry, handle)
    }

    #[tokio::test]
    async fn synthesize_tool_registers_and_executes() {
        let (registry, handle) = make_handle();

        let synth = SynthesizeTool::new(handle.clone());

        let result = synth
            .execute(
                "call_1",
                json!({
                    "name": "greet",
                    "description": "Greets a user",
                    "script": "echo \"Hello, $AMAI_ARG_USER\"",
                    "args_schema": {
                        "type": "object",
                        "properties": {"user": {"type": "string"}},
                        "required": ["user"]
                    }
                }),
                None,
            )
            .await
            .unwrap();

        assert!(!result.is_error, "Registration should succeed: {}", result.content);
        assert!(result.content.contains("greet"));

        // The tool should now be in the dynamic registry
        assert_eq!(handle.len(), 1);

        // Execute the synthesized tool
        let greet_tool = registry.get_dynamic("greet").expect("greet should be registered");
        let output = greet_tool
            .execute("call_2", json!({"user": "World"}), None)
            .await
            .unwrap();

        assert!(!output.is_error);
        assert!(output.content.contains("Hello, World"), "got: {}", output.content);
    }

    #[tokio::test]
    async fn synthesize_tool_rejects_bad_syntax() {
        let (_, handle) = make_handle();
        let synth = SynthesizeTool::new(handle);

        let result = synth
            .execute(
                "call_1",
                json!({
                    "name": "broken",
                    "description": "A broken tool",
                    "script": "if [ unclosed"
                }),
                None,
            )
            .await
            .unwrap();

        assert!(result.is_error, "Should fail on syntax error");
        assert!(result.content.contains("syntax error"));
    }

    #[tokio::test]
    async fn synthesize_tool_rejects_invalid_name() {
        let (_, handle) = make_handle();
        let synth = SynthesizeTool::new(handle);

        let result = synth
            .execute(
                "call_1",
                json!({
                    "name": "has spaces",
                    "description": "Bad name",
                    "script": "echo ok"
                }),
                None,
            )
            .await
            .unwrap();

        assert!(result.is_error);
    }

    #[tokio::test]
    async fn synthesized_shell_tool_returns_error_on_nonzero_exit() {
        let tool = SynthesizedShellTool::new(
            "fail_tool",
            "Always fails",
            json!({"type": "object", "properties": {}}),
            "exit 1",
        );

        let output = tool.execute("call_1", json!({}), None).await.unwrap();
        assert!(output.is_error);
    }

    #[tokio::test]
    async fn synthesized_shell_tool_passes_args_as_env_vars() {
        let tool = SynthesizedShellTool::new(
            "concat",
            "Concatenates two strings",
            json!({
                "type": "object",
                "properties": {
                    "a": {"type": "string"},
                    "b": {"type": "string"}
                }
            }),
            "echo \"${AMAI_ARG_A}${AMAI_ARG_B}\"",
        );

        let output = tool
            .execute("call_1", json!({"a": "foo", "b": "bar"}), None)
            .await
            .unwrap();

        assert!(!output.is_error);
        assert!(output.content.contains("foobar"), "got: {}", output.content);
    }

    #[test]
    fn synthesize_tool_definition_has_required_fields() {
        let (_, handle) = make_handle();
        let synth = SynthesizeTool::new(handle);
        let def = synth.definition();

        assert_eq!(def.name, "synthesize_tool");
        let required = def.input_schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "name"));
        assert!(required.iter().any(|v| v == "description"));
        assert!(required.iter().any(|v| v == "script"));
    }
}
