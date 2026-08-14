//! Tool result mapping into kernel and session vocabularies.

use philo_agent_kernel as kernel;
use philo_session as session;
use philo_tools::ToolResult;

pub(crate) fn kernel_result(
    call: &kernel::KernelToolCall,
    result: &ToolResult,
) -> kernel::KernelToolResult {
    match result {
        ToolResult::Success { content } => {
            kernel::KernelToolResult::success(kernel::ToolCallId::new(call.id().as_str()), content)
        }
        ToolResult::Error { code, message } => kernel::KernelToolResult::error(
            kernel::ToolCallId::new(call.id().as_str()),
            code,
            message,
        ),
    }
}

pub(crate) fn session_result(
    call: &kernel::KernelToolCall,
    result: &ToolResult,
) -> session::SessionToolResult {
    match result {
        ToolResult::Success { content } => session::SessionToolResult::success(
            session::ToolCallId::new(call.id().as_str()),
            content,
        ),
        ToolResult::Error { code, message } => session::SessionToolResult::error(
            session::ToolCallId::new(call.id().as_str()),
            code,
            message,
        ),
    }
}
