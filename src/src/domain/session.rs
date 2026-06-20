#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvent {
    PtyOutput(Vec<u8>),
    PtyOutputDropped { byte_count: usize },
    PtyClosed,
    ChildExited { code: Option<u32> },
    StdinWriteFailed(TerminalFailure),
    Failure(TerminalFailure),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalFailure {
    pub user_message: String,
    pub cause: String,
    pub operation: Option<&'static str>,
}

impl TerminalFailure {
    pub fn new(user_message: impl Into<String>, cause: impl Into<String>) -> Self {
        Self::with_optional_operation(None, user_message, cause)
    }

    pub fn with_operation(
        operation: &'static str,
        user_message: impl Into<String>,
        cause: impl Into<String>,
    ) -> Self {
        Self::with_optional_operation(Some(operation), user_message, cause)
    }

    fn with_optional_operation(
        operation: Option<&'static str>,
        user_message: impl Into<String>,
        cause: impl Into<String>,
    ) -> Self {
        Self {
            user_message: user_message.into(),
            cause: cause.into(),
            operation,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Empty,
    Running,
    ShuttingDown,
    Exited,
    Failed,
}

pub fn session_status_after_event(current: SessionStatus, event: &TerminalEvent) -> SessionStatus {
    match event {
        TerminalEvent::PtyOutput(_) | TerminalEvent::PtyOutputDropped { .. } => current,
        TerminalEvent::PtyClosed => match current {
            SessionStatus::Running | SessionStatus::ShuttingDown => SessionStatus::Exited,
            SessionStatus::Empty | SessionStatus::Exited | SessionStatus::Failed => current,
        },
        TerminalEvent::ChildExited { .. } => match current {
            SessionStatus::Failed => SessionStatus::Failed,
            SessionStatus::Empty
            | SessionStatus::Running
            | SessionStatus::ShuttingDown
            | SessionStatus::Exited => SessionStatus::Exited,
        },
        TerminalEvent::StdinWriteFailed(_) | TerminalEvent::Failure(_) => SessionStatus::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_events_map_to_session_statuses() {
        let write_failure = TerminalFailure::new("write failed", "broken pipe");
        let backend_failure = TerminalFailure::new("backend failed", "reader failed");

        assert_eq!(
            session_status_after_event(
                SessionStatus::Running,
                &TerminalEvent::PtyOutput(b"hello".to_vec()),
            ),
            SessionStatus::Running
        );
        assert_eq!(
            session_status_after_event(
                SessionStatus::Running,
                &TerminalEvent::PtyOutputDropped { byte_count: 10 },
            ),
            SessionStatus::Running
        );
        assert_eq!(
            session_status_after_event(SessionStatus::Running, &TerminalEvent::PtyClosed),
            SessionStatus::Exited
        );
        assert_eq!(
            session_status_after_event(SessionStatus::Exited, &TerminalEvent::PtyClosed),
            SessionStatus::Exited
        );
        assert_eq!(
            session_status_after_event(SessionStatus::Failed, &TerminalEvent::PtyClosed),
            SessionStatus::Failed
        );
        assert_eq!(
            session_status_after_event(
                SessionStatus::Running,
                &TerminalEvent::ChildExited { code: Some(0) },
            ),
            SessionStatus::Exited
        );
        assert_eq!(
            session_status_after_event(
                SessionStatus::Failed,
                &TerminalEvent::ChildExited { code: Some(1) },
            ),
            SessionStatus::Failed
        );
        assert_eq!(
            session_status_after_event(
                SessionStatus::Running,
                &TerminalEvent::StdinWriteFailed(write_failure),
            ),
            SessionStatus::Failed
        );
        assert_eq!(
            session_status_after_event(
                SessionStatus::Running,
                &TerminalEvent::Failure(backend_failure),
            ),
            SessionStatus::Failed
        );
    }

    #[test]
    fn child_exit_does_not_overwrite_failure_from_same_event_batch() {
        let write_failure = TerminalFailure::new("write failed", "broken pipe");
        let backend_failure = TerminalFailure::new("backend failed", "reader failed");

        let status = [
            TerminalEvent::StdinWriteFailed(write_failure),
            TerminalEvent::ChildExited { code: Some(1) },
        ]
        .iter()
        .fold(SessionStatus::Running, session_status_after_event);

        assert_eq!(status, SessionStatus::Failed);

        let status = [
            TerminalEvent::Failure(backend_failure),
            TerminalEvent::ChildExited { code: Some(1) },
        ]
        .iter()
        .fold(SessionStatus::Running, session_status_after_event);

        assert_eq!(status, SessionStatus::Failed);
    }
}
