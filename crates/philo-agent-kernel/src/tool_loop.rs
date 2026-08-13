use crate::{KernelToolCall, KernelToolResult};
use std::collections::HashSet;

pub(crate) fn valid_calls(calls: &[KernelToolCall]) -> bool {
    !calls.is_empty()
        && calls
            .iter()
            .all(|call| !call.id().as_str().is_empty() && !call.name().trim().is_empty())
        && calls
            .iter()
            .map(|call| call.id().as_str())
            .collect::<HashSet<_>>()
            .len()
            == calls.len()
}

pub(crate) fn matching_results(calls: &[KernelToolCall], results: &[KernelToolResult]) -> bool {
    calls.len() == results.len()
        && calls
            .iter()
            .zip(results)
            .all(|(call, result)| call.id() == result.call_id())
}
