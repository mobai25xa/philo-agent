//! Host double used by state-machine and cross-layer tests.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use philo_agent_runtime::{
    AgentError, OperationHandle, ReasoningEffort, RuntimeFuture, SessionId, ToolDefinition,
    UserMessage,
};
use philo_session::SessionContextView;

use crate::api::confirmation::ConfirmationChannel;
use crate::api::host::{ConfigEntry, HostError, TuiHost};

pub(crate) struct FakeHost {
    prompts: Mutex<Vec<(SessionId, UserMessage)>>,
    confirmations: ConfirmationChannel,
    sessions: Mutex<Result<Vec<philo_session::SessionId>, String>>,
    views: Mutex<HashMap<String, SessionContextView>>,
    view_calls: Mutex<Vec<String>>,
    model_error: Mutex<Option<String>>,
    reasoning: Mutex<Vec<ReasoningEffort>>,
    reasoning_error: Mutex<Option<String>>,
    config: Mutex<Vec<ConfigEntry>>,
    tools: Mutex<Vec<ToolDefinition>>,
    next_session_id: Mutex<String>,
}

impl Default for FakeHost {
    fn default() -> Self {
        Self {
            prompts: Mutex::default(),
            confirmations: ConfirmationChannel::default(),
            sessions: Mutex::new(Ok(vec![philo_session::SessionId::new("fake-session")])),
            views: Mutex::default(),
            view_calls: Mutex::default(),
            model_error: Mutex::default(),
            reasoning: Mutex::default(),
            reasoning_error: Mutex::default(),
            config: Mutex::new(vec![ConfigEntry {
                key: "model".to_owned(),
                value: "fake".to_owned(),
                source: "default".to_owned(),
            }]),
            tools: Mutex::default(),
            next_session_id: Mutex::new("fake-new-session".to_owned()),
        }
    }
}

impl FakeHost {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(crate) fn prompt_count(&self) -> usize {
        self.prompts.lock().expect("fake host mutex").len()
    }

    pub(crate) fn confirmations(&self) -> ConfirmationChannel {
        self.confirmations.clone()
    }

    pub(crate) fn set_sessions(&self, ids: &[&str]) {
        *self.sessions.lock().expect("fake host mutex") = Ok(ids
            .iter()
            .map(|id| philo_session::SessionId::new(*id))
            .collect());
    }

    pub(crate) fn set_view(&self, id: &str, view: SessionContextView) {
        self.views
            .lock()
            .expect("fake host mutex")
            .insert(id.to_owned(), view);
    }

    pub(crate) fn view_calls(&self) -> Vec<String> {
        self.view_calls.lock().expect("fake host mutex").clone()
    }

    pub(crate) fn fail_model(&self, message: &str) {
        *self.model_error.lock().expect("fake host mutex") = Some(message.to_owned());
    }

    pub(crate) fn reasoning_calls(&self) -> Vec<ReasoningEffort> {
        self.reasoning.lock().expect("fake host mutex").clone()
    }

    pub(crate) fn set_config(&self, entries: Vec<ConfigEntry>) {
        *self.config.lock().expect("fake host mutex") = entries;
    }

    pub(crate) fn set_tools(&self, tools: Vec<ToolDefinition>) {
        *self.tools.lock().expect("fake host mutex") = tools;
    }

    pub(crate) fn set_next_session_id(&self, id: &str) {
        *self.next_session_id.lock().expect("fake host mutex") = id.to_owned();
    }
}

impl TuiHost for FakeHost {
    fn prompt<'a>(
        &'a self,
        session_id: SessionId,
        message: UserMessage,
    ) -> RuntimeFuture<'a, Result<OperationHandle, AgentError>> {
        self.prompts
            .lock()
            .expect("fake host mutex")
            .push((session_id, message));
        Box::pin(async { Err(AgentError::new("fake host has no runtime")) })
    }

    fn list_sessions(&self) -> Result<Vec<philo_session::SessionId>, HostError> {
        self.sessions
            .lock()
            .expect("fake host mutex")
            .clone()
            .map_err(HostError::new)
    }

    fn context_view<'a>(
        &'a self,
        session_id: &'a philo_session::SessionId,
    ) -> RuntimeFuture<'a, Result<SessionContextView, HostError>> {
        self.view_calls
            .lock()
            .expect("fake host mutex")
            .push(session_id.as_str().to_owned());
        let view = self
            .views
            .lock()
            .expect("fake host mutex")
            .get(session_id.as_str())
            .cloned();
        Box::pin(async move {
            view.ok_or_else(|| HostError::new("fake host has no history for this session"))
        })
    }

    fn rebuild_model(&self, _name: &str) -> Result<(), HostError> {
        match self.model_error.lock().expect("fake host mutex").clone() {
            Some(message) => Err(HostError::new(message)),
            None => Ok(()),
        }
    }

    fn set_reasoning(&self, effort: ReasoningEffort) -> Result<(), HostError> {
        match self
            .reasoning_error
            .lock()
            .expect("fake host mutex")
            .clone()
        {
            Some(message) => Err(HostError::new(message)),
            None => {
                self.reasoning.lock().expect("fake host mutex").push(effort);
                Ok(())
            }
        }
    }

    fn config_view(&self) -> Vec<ConfigEntry> {
        self.config.lock().expect("fake host mutex").clone()
    }

    fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.tools.lock().expect("fake host mutex").clone()
    }

    fn new_session_id(&self) -> String {
        self.next_session_id
            .lock()
            .expect("fake host mutex")
            .clone()
    }

    fn confirmations(&self) -> ConfirmationChannel {
        self.confirmations.clone()
    }
}
