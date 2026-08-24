use philo::api::stable as sdk;
use philo_agent_runtime::{ModelError, ToolChoice, ToolDefinition};

use crate::error::caller_error;

pub(super) fn map_tools(
    request: &mut sdk::ModelRequest,
    choice: &ToolChoice,
    tools: &[ToolDefinition],
    max_parallel_tool_calls: u32,
) -> Result<(), ModelError> {
    if tools.is_empty() {
        // Tool disabling wins: the frozen configuration has no effect on a
        // tools_allowed = false call (pre-M10 behavior unchanged).
        request.tool_choice = sdk::ToolChoice::None;
    } else {
        request.tool_choice = map_tool_choice(choice, tools)?;
        for tool in tools {
            request.tools.push(map_tool(tool)?);
        }
    }
    request.parallel_tool_calls = if max_parallel_tool_calls > 1 {
        sdk::ParallelToolCalls::Allow
    } else {
        sdk::ParallelToolCalls::Forbid
    };
    Ok(())
}

/// Direct mapping of the frozen runtime tool choice onto the SDK vocabulary.
/// `Specific` is validated against the frozen tool definitions before any
/// transport call: an unknown name is a configuration error (M4 decision 6
/// precedent, established failure path).
fn map_tool_choice(
    choice: &ToolChoice,
    tools: &[ToolDefinition],
) -> Result<sdk::ToolChoice, ModelError> {
    Ok(match choice {
        ToolChoice::Auto => sdk::ToolChoice::Auto,
        ToolChoice::None => sdk::ToolChoice::None,
        ToolChoice::Required => sdk::ToolChoice::Required,
        ToolChoice::Specific { name } => {
            if !tools.iter().any(|tool| tool.name() == name) {
                return Err(caller_error(
                    "model.assembly.invalid_tool_choice",
                    format!(
                        "tool_choice requires '{name}', which is not among the frozen tool \
                         definitions"
                    ),
                ));
            }
            sdk::ToolChoice::Specific(sdk::ToolName::new(name.as_str()).map_err(|_| {
                caller_error(
                    "model.assembly.invalid_tool_choice",
                    format!("tool_choice name '{name}' is not a valid tool name"),
                )
            })?)
        }
    })
}

fn map_tool(tool: &ToolDefinition) -> Result<sdk::ToolDefinition, ModelError> {
    let name = sdk::ToolName::new(tool.name())
        .map_err(|_| caller_error("model.assembly.request_build", "frozen tool definition has an invalid name"))?;
    let schema: serde_json::Value = serde_json::from_str(tool.parameters().as_str())
        .map_err(|_| caller_error("model.assembly.request_build", "frozen tool definition has an invalid parameter schema"))?;
    let parameters = sdk::JsonSchema::new(schema)
        .map_err(|_| caller_error("model.assembly.request_build", "frozen tool definition schema root must be an object"))?;
    Ok(sdk::ToolDefinition {
        name,
        description: (!tool.description().is_empty()).then(|| tool.description().to_owned()),
        parameters,
        strictness: sdk::ToolStrictness::BestEffort,
    })
}
