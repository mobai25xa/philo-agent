//! Test-only host and read-only session fixtures.

mod fake_host;
mod fixtures;

pub(crate) use fake_host::FakeHost;
pub(crate) use fixtures::{empty_session_view, image_session_view, session_view, tool};

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use philo_agent_runtime::{SessionId, UserMessage};

    use super::*;
    use crate::api::host::TuiHost;

    #[tokio::test]
    async fn fake_host_is_object_safe_and_records_prompt_attempts() {
        let concrete = FakeHost::new();
        let host: Arc<dyn TuiHost> = concrete.clone();
        let result = host
            .prompt(SessionId::new("s"), UserMessage::new("hello"))
            .await;
        assert_eq!(
            result.expect_err("fake runtime rejects").message(),
            "fake host has no runtime"
        );
        assert_eq!(concrete.prompt_count(), 1);
        assert_eq!(host.list_sessions().expect("fake sessions").len(), 1);
        assert_eq!(host.new_session_id(), "fake-new-session");
    }

    #[tokio::test]
    async fn registered_views_read_back_and_count_calls() {
        let host = FakeHost::new();
        host.set_view("s-1", session_view("s-1"));
        let id = philo_session::SessionId::new("s-1");
        let view = host.context_view(&id).await.expect("registered");
        assert_eq!(view.messages().len(), 4);
        assert!(
            host.context_view(&philo_session::SessionId::new("s-2"))
                .await
                .is_err(),
            "unregistered sessions read as an error"
        );
        assert_eq!(host.view_calls(), ["s-1", "s-2"]);
    }
}
