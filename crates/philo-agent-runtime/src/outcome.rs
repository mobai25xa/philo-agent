//! Phase, status, failure, and outcome vocabulary of one operation.

use crate::{AssistantMessage, ModelError, OperationId, SessionId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelCallPhase {
    Starting,
    WaitingForFirstOutput,
    Streaming,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunningToolBatchPhase {
    Preparing,
    /// Concurrent execution of the current batch. `in_flight` is the number
    /// of `ToolPort::invoke` calls that have started and not yet returned;
    /// `completed` is how many have already returned a model-facing result.
    Executing {
        in_flight: usize,
        completed: usize,
    },
    CommittingResults,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperationPhase {
    /// Waiting in the FIFO follow-up queue; no turn exists yet.
    Queued,
    PreparingTurn,
    RunningModelCall(ModelCallPhase),
    RunningToolBatch(RunningToolBatchPhase),
    Finalizing,
    Settled(OperationStatus),
}

/// Lets a by-value phase compare against `&OperationPhase` expectations.
impl PartialEq<&OperationPhase> for OperationPhase {
    fn eq(&self, other: &&OperationPhase) -> bool {
        self == *other
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationStatus {
    Succeeded,
    Failed,
    Cancelled,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettlementDurability {
    Confirmed,
    Unconfirmed,
}

/// Session-store revision carried by a terminal settlement. Never forged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettlementRevision {
    /// The settlement committed a Session transaction at this revision.
    Committed(philo_session::SessionRevision),
    /// No durable Session commit happened for this settlement.
    Unchanged,
}

/// Who is responsible for one failure. Orthogonal to [`FailureStage`]: a
/// protocol-decode failure is detected locally (`FailureStage::ModelPort`)
/// but caused by the remote provider emitting non-conforming data.
///
/// Frozen vocabulary; the code table lives in
/// `docs/philo-agent/error-codes.md`. Model-call failures pass the philo
/// SDK's `FaultDomain` through, with SDK `Sdk` absorbed into [`Self::Internal`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureDomain {
    /// The remote provider violated its wire contract, rejected the request,
    /// aborted the stream, or produced structurally invalid output.
    Provider,
    /// The local-to-remote delivery path failed or stalled.
    Network,
    /// Local configuration, request validity, or credentials: an identical
    /// retry cannot succeed.
    Caller,
    /// The local persistence layer (session store / disk) failed.
    Storage,
    /// An agent or SDK defect: invariant violation, port-contract breach,
    /// panic. Worth reporting upstream with the diagnostic attached.
    Internal,
}

/// Where in the agent stack one failure was detected. Replaces the former
/// five-value `AgentFailureKind`; durable facts keep kernel/session's
/// five-value `TurnFailureKind` via the code-table-pinned mapping in
/// `crate::mapping::failure`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureStage {
    /// ModelPort boundary: request assembly, stream setup, event normalization.
    ModelPort,
    /// Barrier commits, tool orchestration, settlement driving, driver invariants.
    TurnEngine,
    /// The kernel rejected a transition or an output.
    Kernel,
    /// Session transaction validation, projection advance, store actor interaction.
    SessionStore,
    /// Tool execution infrastructure (executor saturation, port protocol breach).
    ToolPort,
    /// Crash / shutdown forced finalization by the epoch supervisor.
    EpochSupervisor,
}

/// Recorded advice on whether re-issuing the same work could plausibly
/// succeed. This is the turn engine's recovery decision source: `Safe` and
/// `MayDuplicate` re-issue the identical call (failed attempts commit
/// nothing durable, so agent-level duplicates are impossible), `Never`
/// fails fast. `retry_after_ms` carries a provider-supplied pacing hint
/// when one is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryDisposition {
    Never,
    Safe { retry_after_ms: Option<u64> },
    MayDuplicate { retry_after_ms: Option<u64> },
}

impl RetryDisposition {
    /// Whether an identical re-issue is advised at all.
    pub fn is_retryable(self) -> bool {
        !matches!(self, Self::Never)
    }
}

/// One structured failure fact answering four questions: what failed
/// ([`Self::summary`]/[`Self::diagnostic`]), where it was detected
/// ([`Self::stage`]), who is responsible ([`Self::domain`]), and whether an
/// identical re-issue could succeed ([`Self::retry`]).
///
/// Durable facts keep kernel/session's coarse five-value `TurnFailureKind`;
/// this type never crosses into the Session format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentFailure {
    code: String,
    domain: FailureDomain,
    stage: FailureStage,
    retry: RetryDisposition,
    summary: String,
    diagnostic: String,
}

impl AgentFailure {
    /// Builds one failure from the frozen code table. Model-call failures
    /// should go through [`Self::from_model_error`] instead.
    pub fn new(
        code: impl Into<String>,
        domain: FailureDomain,
        stage: FailureStage,
        retry: RetryDisposition,
        summary: impl Into<String>,
        diagnostic: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            domain,
            stage,
            retry,
            summary: summary.into(),
            diagnostic: diagnostic.into(),
        }
    }

    pub(crate) fn from_model_error(error: &ModelError) -> Self {
        Self {
            code: error.code().to_owned(),
            domain: error.domain(),
            stage: FailureStage::ModelPort,
            retry: error.retry(),
            summary: error.summary().to_owned(),
            diagnostic: error.diagnostic().to_owned(),
        }
    }

    /// Same fact with appended diagnostic context; used when a failure
    /// settlement itself could not be confirmed durable.
    pub(crate) fn with_appended_diagnostic(&self, suffix: &str) -> Self {
        let mut degraded = self.clone();
        degraded
            .diagnostic
            .push_str("; failure settlement unconfirmed: ");
        degraded.diagnostic.push_str(suffix);
        degraded
    }

    pub fn code(&self) -> &str {
        &self.code
    }
    pub fn domain(&self) -> FailureDomain {
        self.domain
    }
    pub fn stage(&self) -> FailureStage {
        self.stage
    }
    pub fn retry(&self) -> RetryDisposition {
        self.retry
    }
    /// One bounded human-readable line: what happened and whose fault it is.
    pub fn summary(&self) -> &str {
        &self.summary
    }
    /// Bounded developer-facing detail (redacted); not for model context.
    pub fn diagnostic(&self) -> &str {
        &self.diagnostic
    }
    /// Legacy combined one-liner (`summary; diagnostic`) for logs and
    /// assertions; structured accessors are preferred.
    pub fn message(&self) -> String {
        format!("{}; {}", self.summary, self.diagnostic)
    }
    /// Durable five-value bucket this failure maps to, pinned row-by-row by
    /// the frozen code table (`docs/philo-agent/error-codes.md`). This is
    /// the single source for the kernel/session `TurnFailureKind` mapping.
    pub fn durable_kind(&self) -> DurableFailureKind {
        let code = &self.code;
        match self.stage {
            FailureStage::ToolPort => DurableFailureKind::ToolExecution,
            FailureStage::SessionStore => DurableFailureKind::Persistence,
            FailureStage::EpochSupervisor => DurableFailureKind::RuntimeDriver,
            FailureStage::Kernel => match code.as_str() {
                "kernel.transition_rejected" => DurableFailureKind::RuntimeDriver,
                // output_rejected / tool_results_rejected
                _ => DurableFailureKind::InvalidModelOutput,
            },
            FailureStage::TurnEngine => match code.as_str() {
                "engine.invariant_violation" => DurableFailureKind::RuntimeDriver,
                // barrier/settlement/cancel/seal commit failures
                _ => DurableFailureKind::Persistence,
            },
            FailureStage::ModelPort => {
                if code.starts_with("model.port.") || code.starts_with("model.output.") {
                    DurableFailureKind::InvalidModelOutput
                } else {
                    // SDK passthrough codes + model.assembly.*
                    DurableFailureKind::ModelCall
                }
            }
        }
    }
}

/// Coarse durable failure buckets owned by kernel/session
/// (`TurnFailureKind`). The [`AgentFailure::durable_kind`] mapping is
/// pinned by the frozen code table; the JSONL format never changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DurableFailureKind {
    ModelCall,
    InvalidModelOutput,
    ToolExecution,
    Persistence,
    RuntimeDriver,
}

/// Every agent-owned row of the frozen code table
/// (`docs/philo-agent/error-codes.md`) as `(code, domain, stage, retry)`.
/// SDK-passthrough `model.<sdk>` codes are not listed here — the philo
/// code table is their single source. This list is drift-guarded by the
/// `agent_code_table_is_frozen` test; any change here must land in the
/// same change as the doc table.
pub const AGENT_OWNED_CODES: &[(&str, FailureDomain, FailureStage, RetryDisposition)] = &[
    (
        "model.port.stream_after_completed",
        FailureDomain::Internal,
        FailureStage::ModelPort,
        RetryDisposition::Never,
    ),
    (
        "model.port.response_started_twice",
        FailureDomain::Internal,
        FailureStage::ModelPort,
        RetryDisposition::Never,
    ),
    (
        "model.port.stream_closed_early",
        FailureDomain::Internal,
        FailureStage::ModelPort,
        RetryDisposition::Never,
    ),
    (
        "model.output.invalid_block",
        FailureDomain::Provider,
        FailureStage::ModelPort,
        RetryDisposition::MayDuplicate { retry_after_ms: None },
    ),
    (
        "model.output.delta_before_start",
        FailureDomain::Provider,
        FailureStage::ModelPort,
        RetryDisposition::MayDuplicate { retry_after_ms: None },
    ),
    (
        "model.replay.corrupted",
        FailureDomain::Caller,
        FailureStage::ModelPort,
        RetryDisposition::Never,
    ),
    (
        "model.replay.unavailable",
        FailureDomain::Caller,
        FailureStage::ModelPort,
        RetryDisposition::Never,
    ),
    (
        "model.replay.persist_failed",
        FailureDomain::Caller,
        FailureStage::ModelPort,
        RetryDisposition::Never,
    ),
    (
        "model.replay.unsupported",
        FailureDomain::Caller,
        FailureStage::ModelPort,
        RetryDisposition::Never,
    ),
    (
        "model.assembly.invalid_tool_choice",
        FailureDomain::Caller,
        FailureStage::ModelPort,
        RetryDisposition::Never,
    ),
    (
        "model.assembly.max_output_tokens",
        FailureDomain::Caller,
        FailureStage::ModelPort,
        RetryDisposition::Never,
    ),
    (
        "model.assembly.image_invalid",
        FailureDomain::Caller,
        FailureStage::ModelPort,
        RetryDisposition::Never,
    ),
    (
        "model.assembly.effort_unsupported",
        FailureDomain::Caller,
        FailureStage::ModelPort,
        RetryDisposition::Never,
    ),
    (
        "model.assembly.request_build",
        FailureDomain::Caller,
        FailureStage::ModelPort,
        RetryDisposition::Never,
    ),
    (
        "kernel.output_rejected",
        FailureDomain::Provider,
        FailureStage::Kernel,
        RetryDisposition::Never,
    ),
    (
        "kernel.tool_results_rejected",
        FailureDomain::Internal,
        FailureStage::Kernel,
        RetryDisposition::Never,
    ),
    (
        "kernel.transition_rejected",
        FailureDomain::Internal,
        FailureStage::Kernel,
        RetryDisposition::Never,
    ),
    (
        "engine.barrier_a_commit_failed",
        FailureDomain::Storage,
        FailureStage::TurnEngine,
        RetryDisposition::Safe { retry_after_ms: None },
    ),
    (
        "engine.barrier_b_commit_failed",
        FailureDomain::Storage,
        FailureStage::TurnEngine,
        RetryDisposition::Safe { retry_after_ms: None },
    ),
    (
        "engine.barrier_c_commit_failed",
        FailureDomain::Storage,
        FailureStage::TurnEngine,
        RetryDisposition::MayDuplicate { retry_after_ms: None },
    ),
    (
        "engine.settlement_commit_failed",
        FailureDomain::Storage,
        FailureStage::TurnEngine,
        RetryDisposition::MayDuplicate { retry_after_ms: None },
    ),
    (
        "engine.cancel_commit_failed",
        FailureDomain::Storage,
        FailureStage::TurnEngine,
        RetryDisposition::MayDuplicate { retry_after_ms: None },
    ),
    (
        "engine.seal_commit_failed",
        FailureDomain::Storage,
        FailureStage::TurnEngine,
        RetryDisposition::Safe { retry_after_ms: None },
    ),
    (
        "engine.invariant_violation",
        FailureDomain::Internal,
        FailureStage::TurnEngine,
        RetryDisposition::Never,
    ),
    (
        "session.store_busy",
        FailureDomain::Storage,
        FailureStage::SessionStore,
        RetryDisposition::Safe { retry_after_ms: None },
    ),
    (
        "session.store_unavailable",
        FailureDomain::Storage,
        FailureStage::SessionStore,
        RetryDisposition::Never,
    ),
    (
        "session.revision_conflict",
        FailureDomain::Internal,
        FailureStage::SessionStore,
        RetryDisposition::Never,
    ),
    (
        "session.validation_rejected",
        FailureDomain::Internal,
        FailureStage::SessionStore,
        RetryDisposition::Never,
    ),
    (
        "tool.port_failed",
        FailureDomain::Internal,
        FailureStage::ToolPort,
        RetryDisposition::MayDuplicate { retry_after_ms: None },
    ),
    (
        "tool.stopped_without_cancel",
        FailureDomain::Internal,
        FailureStage::ToolPort,
        RetryDisposition::Never,
    ),
    (
        "epoch.forced_settlement",
        FailureDomain::Internal,
        FailureStage::EpochSupervisor,
        RetryDisposition::Never,
    ),
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperationOutcome {
    Succeeded {
        assistant: AssistantMessage,
    },
    Failed {
        failure: AgentFailure,
        durability: SettlementDurability,
    },
    /// The operation ended by user request; a normal terminal outcome.
    Cancelled,
}

/// Read-only observation of whether the runtime is driving an operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentAvailability {
    Idle,
    Busy { operation_id: OperationId },
    Compacting { session_id: SessionId },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentError {
    message: String,
}
impl AgentError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
    pub fn message(&self) -> &str {
        &self.message
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drift guard for `docs/philo-agent/error-codes.md`: every agent-owned
    /// row keeps its domain / stage / retry classification, and the durable
    /// bucket mapping agrees with the doc's per-row pinning.
    #[test]
    fn agent_code_table_is_frozen() {
        let expected: Vec<(&str, FailureDomain, FailureStage, RetryDisposition)> = vec![
            ("model.port.stream_after_completed", FailureDomain::Internal, FailureStage::ModelPort, RetryDisposition::Never),
            ("model.port.response_started_twice", FailureDomain::Internal, FailureStage::ModelPort, RetryDisposition::Never),
            ("model.port.stream_closed_early", FailureDomain::Internal, FailureStage::ModelPort, RetryDisposition::Never),
            ("model.output.invalid_block", FailureDomain::Provider, FailureStage::ModelPort, RetryDisposition::MayDuplicate { retry_after_ms: None }),
            ("model.output.delta_before_start", FailureDomain::Provider, FailureStage::ModelPort, RetryDisposition::MayDuplicate { retry_after_ms: None }),
            ("model.replay.corrupted", FailureDomain::Caller, FailureStage::ModelPort, RetryDisposition::Never),
            ("model.replay.unavailable", FailureDomain::Caller, FailureStage::ModelPort, RetryDisposition::Never),
            ("model.replay.persist_failed", FailureDomain::Caller, FailureStage::ModelPort, RetryDisposition::Never),
            ("model.replay.unsupported", FailureDomain::Caller, FailureStage::ModelPort, RetryDisposition::Never),
            ("model.assembly.invalid_tool_choice", FailureDomain::Caller, FailureStage::ModelPort, RetryDisposition::Never),
            ("model.assembly.max_output_tokens", FailureDomain::Caller, FailureStage::ModelPort, RetryDisposition::Never),
            ("model.assembly.image_invalid", FailureDomain::Caller, FailureStage::ModelPort, RetryDisposition::Never),
            ("model.assembly.effort_unsupported", FailureDomain::Caller, FailureStage::ModelPort, RetryDisposition::Never),
            ("model.assembly.request_build", FailureDomain::Caller, FailureStage::ModelPort, RetryDisposition::Never),
            ("kernel.output_rejected", FailureDomain::Provider, FailureStage::Kernel, RetryDisposition::Never),
            ("kernel.tool_results_rejected", FailureDomain::Internal, FailureStage::Kernel, RetryDisposition::Never),
            ("kernel.transition_rejected", FailureDomain::Internal, FailureStage::Kernel, RetryDisposition::Never),
            ("engine.barrier_a_commit_failed", FailureDomain::Storage, FailureStage::TurnEngine, RetryDisposition::Safe { retry_after_ms: None }),
            ("engine.barrier_b_commit_failed", FailureDomain::Storage, FailureStage::TurnEngine, RetryDisposition::Safe { retry_after_ms: None }),
            ("engine.barrier_c_commit_failed", FailureDomain::Storage, FailureStage::TurnEngine, RetryDisposition::MayDuplicate { retry_after_ms: None }),
            ("engine.settlement_commit_failed", FailureDomain::Storage, FailureStage::TurnEngine, RetryDisposition::MayDuplicate { retry_after_ms: None }),
            ("engine.cancel_commit_failed", FailureDomain::Storage, FailureStage::TurnEngine, RetryDisposition::MayDuplicate { retry_after_ms: None }),
            ("engine.seal_commit_failed", FailureDomain::Storage, FailureStage::TurnEngine, RetryDisposition::Safe { retry_after_ms: None }),
            ("engine.invariant_violation", FailureDomain::Internal, FailureStage::TurnEngine, RetryDisposition::Never),
            ("session.store_busy", FailureDomain::Storage, FailureStage::SessionStore, RetryDisposition::Safe { retry_after_ms: None }),
            ("session.store_unavailable", FailureDomain::Storage, FailureStage::SessionStore, RetryDisposition::Never),
            ("session.revision_conflict", FailureDomain::Internal, FailureStage::SessionStore, RetryDisposition::Never),
            ("session.validation_rejected", FailureDomain::Internal, FailureStage::SessionStore, RetryDisposition::Never),
            ("tool.port_failed", FailureDomain::Internal, FailureStage::ToolPort, RetryDisposition::MayDuplicate { retry_after_ms: None }),
            ("tool.stopped_without_cancel", FailureDomain::Internal, FailureStage::ToolPort, RetryDisposition::Never),
            ("epoch.forced_settlement", FailureDomain::Internal, FailureStage::EpochSupervisor, RetryDisposition::Never),
        ];
        assert_eq!(AGENT_OWNED_CODES, expected);

        // The durable bucket mapping must classify every row consistently
        // with the doc table's pinning section.
        let durable_expectations: &[(&str, DurableFailureKind)] = &[
            ("model.port.stream_after_completed", DurableFailureKind::InvalidModelOutput),
            ("model.output.invalid_block", DurableFailureKind::InvalidModelOutput),
            ("model.assembly.request_build", DurableFailureKind::ModelCall),
            ("model.invalid_sequence", DurableFailureKind::ModelCall),
            ("kernel.output_rejected", DurableFailureKind::InvalidModelOutput),
            ("kernel.tool_results_rejected", DurableFailureKind::InvalidModelOutput),
            ("kernel.transition_rejected", DurableFailureKind::RuntimeDriver),
            ("engine.barrier_a_commit_failed", DurableFailureKind::Persistence),
            ("engine.invariant_violation", DurableFailureKind::RuntimeDriver),
            ("session.store_busy", DurableFailureKind::Persistence),
            ("tool.port_failed", DurableFailureKind::ToolExecution),
            ("tool.stopped_without_cancel", DurableFailureKind::ToolExecution),
            ("epoch.forced_settlement", DurableFailureKind::RuntimeDriver),
        ];
        for (code, domain, stage, retry) in AGENT_OWNED_CODES {
            let failure = AgentFailure::new(*code, *domain, *stage, *retry, "s", "d");
            for (probe, bucket) in durable_expectations {
                if code == probe {
                    assert_eq!(
                        failure.durable_kind(),
                        *bucket,
                        "durable bucket drifted for {code}"
                    );
                }
            }
        }
    }
}
