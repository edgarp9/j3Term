mod event_queue;
mod process;
mod shell;
mod worker;

use portable_pty::{MasterPty, PtySize, native_pty_system};

use crate::app::PtyBackend;
use crate::domain::{
    ShellCommandDialect, StartupDirectory, TerminalEvent, TerminalFailure, TerminalInputBytes,
    TerminalSize,
};
use crate::error::{AppError, AppResult};

use self::event_queue::{PtyEventDrainBudget, PtyEventQueue};
use self::process::{
    PtyChildHandle, PtyCleanupResources, PtyCleanupTask, shutdown_resources,
    spawn_shutdown_resources, take_child_exit_code,
};
use self::shell::DefaultShell;
#[cfg(all(test, target_os = "windows"))]
use self::shell::windows_powershell_path;
use self::worker::{PtyIoWorkers, is_writer_input_queue_full_error, spawn_reader, spawn_writer};

const READ_BUFFER_SIZE: usize = 8192;
const MAX_PENDING_EVENT_COUNT: usize = 512;
const MAX_PENDING_OUTPUT_BYTES: usize = READ_BUFFER_SIZE * MAX_PENDING_EVENT_COUNT;
const MAX_DRAIN_EVENT_COUNT: usize = 64;
const MAX_DRAIN_OUTPUT_BYTES: usize = READ_BUFFER_SIZE * 16;
const READ_FAILURE_USER_MESSAGE: &str = "terminal output stream failed";
const WRITE_FAILURE_USER_MESSAGE: &str = "terminal input stream failed";
const SHUTDOWN_FAILURE_USER_MESSAGE: &str = "terminal shutdown failed";
const READER_JOIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const WRITER_JOIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const CHILD_GRACEFUL_EXIT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);
const CHILD_FORCED_EXIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
// Covers graceful child exit, forced child exit, and bounded worker joins
// performed by the detached shutdown thread before application exit returns.
const DETACHED_CLEANUP_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(7);
const CHILD_EXIT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(25);
const CHILD_EXIT_BACKGROUND_POLL_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(250);
const FORCED_CHILD_EXIT_CODE: u32 = 1;

type PtyMasterHandle = Box<dyn MasterPty + Send>;

pub(crate) fn join_detached_cleanup_tasks() -> Vec<AppError> {
    process::join_detached_cleanup_threads()
}

pub(crate) fn finish_detached_cleanup_tasks() -> Vec<AppError> {
    process::finish_detached_cleanup_threads()
}

pub(crate) fn is_detached_cleanup_timeout_error(error: &AppError) -> bool {
    process::is_detached_cleanup_timeout_error(error)
}

pub struct PortablePtySession {
    default_shell: DefaultShell,
    master: Option<PtyMasterHandle>,
    child: Option<PtyChildHandle>,
    events: PtyEventQueue,
    workers: Option<PtyIoWorkers>,
    cleanup_tasks: Vec<PtyCleanupTask>,
    next_child_exit_poll: Option<std::time::Instant>,
}

impl PortablePtySession {
    pub fn new() -> Self {
        Self {
            default_shell: DefaultShell::detect(),
            master: None,
            child: None,
            events: PtyEventQueue::new(),
            workers: None,
            cleanup_tasks: Vec::new(),
            next_child_exit_poll: None,
        }
    }

    #[cfg(test)]
    fn with_default_shell_for_test(default_shell: DefaultShell) -> Self {
        let mut session = Self::new();
        session.default_shell = default_shell;
        session
    }

    fn pty_size(size: TerminalSize) -> PtySize {
        PtySize {
            rows: size.rows,
            cols: size.columns,
            pixel_width: size.pixel_width,
            pixel_height: size.pixel_height,
        }
    }

    fn has_live_session_resources(&self) -> bool {
        self.master.is_some() || self.child.is_some() || self.workers.is_some()
    }

    fn drain_event_channel(&mut self) -> Vec<TerminalEvent> {
        self.events.drain()
    }

    fn drain_event_channel_with_budget(
        &mut self,
        budget: PtyEventDrainBudget,
    ) -> Vec<TerminalEvent> {
        self.events.drain_with_budget(budget)
    }

    fn clear_pending_events(&mut self) {
        self.events.clear();
    }

    fn send_event(&self, event: TerminalEvent) {
        let _ = self.events.push(event);
    }

    fn send_stdin_write_failure(&self, error: &AppError) {
        self.send_event(TerminalEvent::StdinWriteFailed(
            terminal_failure_from_app_error(WRITE_FAILURE_USER_MESSAGE, error),
        ));
    }

    fn write_worker_input(
        &mut self,
        send_input: impl FnOnce(&PtyIoWorkers) -> AppResult<()>,
    ) -> AppResult<()> {
        let Some(workers) = self.workers.as_ref() else {
            let error = AppError::InvalidState("pty writer worker is not available");
            self.send_stdin_write_failure(&error);
            return Err(error);
        };

        if let Err(error) = send_input(workers) {
            if !is_writer_input_queue_full_error(&error) {
                self.send_stdin_write_failure(&error);
            }
            return Err(error);
        }

        Ok(())
    }

    fn has_pending_failure_event(&mut self) -> bool {
        let events = self.drain_event_channel();
        let has_failure = events
            .iter()
            .any(|event| matches!(event, TerminalEvent::Failure(_)));

        for event in events {
            self.send_event(event);
        }

        has_failure
    }

    fn poll_child_exit(&mut self, force: bool) -> AppResult<Option<u32>> {
        if self.child.is_none() {
            self.next_child_exit_poll = None;
            return Ok(None);
        }

        if !force && !self.child_exit_poll_due() {
            return Ok(None);
        }

        self.next_child_exit_poll =
            Some(std::time::Instant::now() + CHILD_EXIT_BACKGROUND_POLL_INTERVAL);
        let Some(exit_code) = take_child_exit_code(&mut self.child, "poll pty child")? else {
            return Ok(None);
        };

        self.spawn_cleanup(false)?;

        Ok(Some(exit_code))
    }

    fn child_exit_poll_due(&mut self) -> bool {
        let now = std::time::Instant::now();
        match self.next_child_exit_poll {
            Some(next_poll) if now < next_poll => false,
            _ => {
                self.next_child_exit_poll = Some(now + CHILD_EXIT_BACKGROUND_POLL_INTERVAL);
                true
            }
        }
    }

    fn poll_cleanup_tasks(&mut self) -> bool {
        let mut reported_failure = false;
        let mut index = 0;
        while index < self.cleanup_tasks.len() {
            let result = self.cleanup_tasks[index].try_finish();
            let Some(result) = result else {
                index += 1;
                continue;
            };

            self.cleanup_tasks.remove(index);
            if let Err(failure) = result {
                reported_failure = true;
                let (resources, error) = failure.into_parts();
                if let Some(resources) = resources {
                    self.restore_failed_cleanup_resources(resources);
                }
                self.send_event(TerminalEvent::Failure(terminal_failure_from_app_error(
                    SHUTDOWN_FAILURE_USER_MESSAGE,
                    &error,
                )));
            }
        }

        reported_failure
    }

    fn spawn_cleanup(&mut self, request_graceful_exit: bool) -> AppResult<()> {
        let cleanup_failed = self.poll_cleanup_tasks();

        if !self.has_live_session_resources() {
            if !self.cleanup_tasks.is_empty() {
                return Ok(());
            }
            if !cleanup_failed {
                self.clear_pending_events();
            }
            self.next_child_exit_poll = None;
            return Ok(());
        }

        let resources =
            PtyCleanupResources::new(self.master.take(), self.child.take(), self.workers.take());
        let default_shell = self.default_shell.clone();
        self.next_child_exit_poll = None;

        match spawn_shutdown_resources(resources, default_shell, request_graceful_exit) {
            Ok(task) => {
                let old_events = std::mem::replace(&mut self.events, PtyEventQueue::new());
                for event in old_events.close_and_drain() {
                    self.send_event(event);
                }
                self.cleanup_tasks.push(task);
                Ok(())
            }
            Err(failure) => {
                let (resources, error) = failure.into_parts();
                if let Some(resources) = resources {
                    self.restore_failed_cleanup_resources(resources);
                }
                Err(error)
            }
        }
    }

    fn cleanup_after_start_failure(
        &mut self,
        resources: PtyCleanupResources,
        start_error: AppError,
        cleanup_operation: &'static str,
    ) -> AppResult<()> {
        self.next_child_exit_poll = None;
        let default_shell = self.default_shell.clone();

        match shutdown_resources(resources, default_shell, false) {
            Ok(()) => Err(start_error),
            Err(failure) => {
                let (resources, cleanup_error) = failure.into_parts();
                if let Some(resources) = resources {
                    self.restore_failed_cleanup_resources(resources);
                }
                Err(AppError::pty_message(
                    cleanup_operation,
                    format!("{start_error}; cleanup failed: {cleanup_error}"),
                ))
            }
        }
    }

    fn restore_failed_cleanup_resources(&mut self, resources: PtyCleanupResources) {
        let (master, child, workers) = resources.into_recoverable_parts();
        self.master = master;
        self.child = child;
        self.workers = workers;
    }
}

impl PtyBackend for PortablePtySession {
    fn start(
        &mut self,
        size: TerminalSize,
        startup_directory: Option<&StartupDirectory>,
    ) -> AppResult<()> {
        self.poll_cleanup_tasks();
        if self.has_pending_failure_event() {
            return Err(AppError::InvalidState("pty cleanup failure is pending"));
        }
        if !self.cleanup_tasks.is_empty() {
            return Err(AppError::InvalidState("pty session is shutting down"));
        }

        if self.has_live_session_resources() {
            self.shutdown()?;
            return Err(AppError::InvalidState("pty session is shutting down"));
        } else {
            self.clear_pending_events();
        }

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(Self::pty_size(size))
            .map_err(|source| AppError::pty("open pty", source))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|source| AppError::pty("take pty writer", source))?;
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|source| AppError::pty("clone pty reader", source))?;
        let child = pair
            .slave
            .spawn_command(self.default_shell.command_builder(startup_directory))
            .map_err(|source| AppError::pty("spawn pty command", source))?;
        let reader_thread = match spawn_reader(reader, self.events.clone()) {
            Ok(thread) => thread,
            Err(error) => {
                let resources = PtyCleanupResources::new(Some(pair.master), Some(child), None);
                return self.cleanup_after_start_failure(
                    resources,
                    error,
                    "cleanup pty after reader thread spawn failure",
                );
            }
        };
        let writer_thread = match spawn_writer(writer, self.events.clone()) {
            Ok(writer_thread) => writer_thread,
            Err(error) => {
                let resources = PtyCleanupResources::new(
                    Some(pair.master),
                    Some(child),
                    Some(PtyIoWorkers::from_reader(reader_thread)),
                );
                return self.cleanup_after_start_failure(
                    resources,
                    error,
                    "cleanup pty after writer thread spawn failure",
                );
            }
        };

        self.master = Some(pair.master);
        self.child = Some(child);
        self.workers = Some(PtyIoWorkers::new(writer_thread, reader_thread));
        self.next_child_exit_poll = None;
        Ok(())
    }

    fn write_input(&mut self, bytes: Vec<u8>) -> AppResult<()> {
        if bytes.is_empty() {
            return Ok(());
        }

        self.write_worker_input(|workers| workers.send_user_input(bytes))
    }

    fn write_terminal_input(&mut self, bytes: TerminalInputBytes) -> AppResult<()> {
        if bytes.as_slice().is_empty() {
            return Ok(());
        }

        self.write_worker_input(|workers| workers.send_terminal_input(bytes))
    }

    fn resize(&mut self, size: TerminalSize) -> AppResult<()> {
        let master = self
            .master
            .as_ref()
            .ok_or(AppError::InvalidState("pty master is not available"))?;
        master
            .resize(Self::pty_size(size))
            .map_err(|source| AppError::pty("resize pty", source))
    }

    fn drain_events(&mut self) -> AppResult<Vec<TerminalEvent>> {
        self.poll_cleanup_tasks();
        let drain_budget = PtyEventDrainBudget::new(MAX_DRAIN_EVENT_COUNT, MAX_DRAIN_OUTPUT_BYTES);
        let mut events = self.drain_event_channel_with_budget(drain_budget);
        let has_failure = events
            .iter()
            .any(|event| matches!(event, TerminalEvent::Failure(_)));

        if has_failure {
            if let Err(error) = self.shutdown() {
                events.push(TerminalEvent::Failure(terminal_failure_from_app_error(
                    SHUTDOWN_FAILURE_USER_MESSAGE,
                    &error,
                )));
            }
            return Ok(events);
        }

        let should_poll_child_now = events.iter().any(should_poll_child_exit_after_event);
        if let Some(exit_code) = self.poll_child_exit(should_poll_child_now)? {
            self.send_event(TerminalEvent::ChildExited {
                code: Some(exit_code),
            });
            let remaining_budget = drain_budget.remaining_after(&events);
            if remaining_budget.has_event_capacity() {
                events.extend(self.drain_event_channel_with_budget(remaining_budget));
            }
        }

        Ok(events)
    }

    fn shutdown(&mut self) -> AppResult<()> {
        self.spawn_cleanup(true)
    }

    fn is_shutdown_pending(&self) -> bool {
        !self.cleanup_tasks.is_empty()
    }

    fn shell_command_dialect(&self) -> ShellCommandDialect {
        self.default_shell.command_dialect()
    }
}

impl Drop for PortablePtySession {
    fn drop(&mut self) {
        let _ = self.shutdown();
        self.events.close();
    }
}

fn terminal_failure_from_app_error(user_message: &str, error: &AppError) -> TerminalFailure {
    match error.operation() {
        Some(operation) => {
            TerminalFailure::with_operation(operation, user_message, error.to_string())
        }
        None => TerminalFailure::new(user_message, error.to_string()),
    }
}

fn should_poll_child_exit_after_event(event: &TerminalEvent) -> bool {
    matches!(
        event,
        TerminalEvent::PtyClosed | TerminalEvent::StdinWriteFailed(_) | TerminalEvent::Failure(_)
    )
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    #[cfg(target_os = "windows")]
    use std::os::windows::io::RawHandle;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    use portable_pty::{Child, ChildKiller, ExitStatus};

    use crate::app::TerminalSession;
    use crate::domain::{
        CommandText, TerminalKey, TerminalKeyModifiers, TerminalViewport, terminal_input_from_char,
        terminal_input_from_key,
    };
    use crate::infra::terminal::AlacrittyTerminalBuffer;

    use super::*;

    #[test]
    #[ignore = "starts a real shell through Windows ConPTY"]
    fn starts_shell_and_emits_initial_output() -> AppResult<()> {
        let size = TerminalSize::new(24, 80)?;
        let mut session = TerminalSession::new(
            PortablePtySession::new(),
            AlacrittyTerminalBuffer::new(size),
            size,
        );
        session.start()?;

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut received_output = false;
        let mut failure = None;
        let mut visible_output = false;

        while Instant::now() < deadline {
            for event in session.drain_events()? {
                match event {
                    TerminalEvent::PtyOutput(_) => {
                        received_output = true;
                    }
                    TerminalEvent::PtyOutputDropped { .. } => {}
                    TerminalEvent::PtyClosed => {}
                    TerminalEvent::ChildExited { code } => {
                        failure = Some(AppError::pty_message(
                            "run pty smoke test",
                            format!("shell exited during smoke test: {code:?}"),
                        ));
                    }
                    TerminalEvent::StdinWriteFailed(failure_event) => {
                        failure = Some(AppError::pty_message(
                            failure_event
                                .operation
                                .unwrap_or("process pty stdin failure"),
                            format!("{}: {}", failure_event.user_message, failure_event.cause),
                        ));
                    }
                    TerminalEvent::Failure(failure_event) => {
                        failure = Some(AppError::pty_message(
                            failure_event.operation.unwrap_or("process pty failure"),
                            format!("{}: {}", failure_event.user_message, failure_event.cause),
                        ));
                    }
                }
            }

            let viewport = session.terminal_viewport()?;
            visible_output = viewport
                .viewport_lines()
                .iter()
                .any(|line| !line.trim().is_empty());

            if (received_output && visible_output) || failure.is_some() {
                break;
            }

            thread::sleep(Duration::from_millis(50));
        }

        let shutdown_result = session.shutdown();
        if let Some(error) = failure {
            let _ = shutdown_result;
            return Err(error);
        }
        shutdown_result?;

        assert!(
            received_output,
            "default shell did not produce initial PTY output"
        );
        assert!(
            visible_output,
            "default shell output did not reach terminal viewport"
        );
        Ok(())
    }

    #[test]
    fn stdin_write_failure_event_does_not_include_input_bytes() -> AppResult<()> {
        let mut backend = PortablePtySession::new();
        let input = b"typed-value";

        let result = <PortablePtySession as PtyBackend>::write_input(&mut backend, input.to_vec());

        assert!(result.is_err());
        let events = <PortablePtySession as PtyBackend>::drain_events(&mut backend)?;
        let Some(TerminalEvent::StdinWriteFailed(failure)) = events.first() else {
            return Err(AppError::pty_message(
                "assert stdin write failure event",
                "stdin write failure event was not emitted",
            ));
        };
        assert!(!failure.user_message.contains("typed-value"));
        assert!(!failure.cause.contains("typed-value"));
        Ok(())
    }

    #[test]
    fn writer_input_queue_backpressure_does_not_emit_stdin_failure_event() -> AppResult<()> {
        let (input_tx, _input_rx) = mpsc::sync_channel(1);
        input_tx
            .try_send(worker::PtyWriteRequest::user_input(b"queued".to_vec()))
            .map_err(|_| AppError::InvalidState("failed to fill pty writer input queue"))?;
        let mut backend = PortablePtySession::new();
        backend.workers = Some(PtyIoWorkers::with_input_tx_for_test(input_tx));

        let result =
            <PortablePtySession as PtyBackend>::write_input(&mut backend, b"overflow".to_vec());

        assert!(matches!(
            result,
            Err(ref error) if is_writer_input_queue_full_error(error)
        ));
        let events = <PortablePtySession as PtyBackend>::drain_events(&mut backend)?;
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, TerminalEvent::StdinWriteFailed(_)))
        );
        Ok(())
    }

    #[test]
    fn drain_events_throttles_child_exit_polling_without_lifecycle_events() -> AppResult<()> {
        let child = MockChild::exits_after_running_polls(usize::MAX);
        let state = child.state.clone();
        let mut backend = PortablePtySession::new();
        backend.child = Some(Box::new(child));

        <PortablePtySession as PtyBackend>::drain_events(&mut backend)?;
        backend
            .events
            .push(TerminalEvent::PtyOutput(b"output".to_vec()));
        let events = <PortablePtySession as PtyBackend>::drain_events(&mut backend)?;

        assert!(matches!(events.first(), Some(TerminalEvent::PtyOutput(_))));
        let state = state
            .lock()
            .map_err(|_| AppError::InvalidState("mock child mutex poisoned"))?;
        assert_eq!(state.try_wait_calls, 1);
        Ok(())
    }

    #[test]
    fn drain_events_preserves_output_queued_during_child_exit_cleanup() -> AppResult<()> {
        let mut backend = PortablePtySession::new();
        let worker_events = backend.events.clone();
        let child =
            MockChild::exits_with_output_on_exit_poll(worker_events, b"final-output".to_vec());
        let (done_tx, done_rx) = mpsc::channel();
        let reader_thread = thread::spawn(move || {
            let _ = done_tx.send(());
        });
        backend.workers = Some(PtyIoWorkers::with_reader_completion_for_test(
            reader_thread,
            done_rx,
        ));
        backend.child = Some(Box::new(child));
        backend.events.push(TerminalEvent::PtyClosed);

        let events = <PortablePtySession as PtyBackend>::drain_events(&mut backend)?;

        assert!(events.iter().any(|event| {
            matches!(event, TerminalEvent::PtyOutput(bytes) if bytes.as_slice() == b"final-output")
        }));
        assert!(matches!(
            events.last(),
            Some(TerminalEvent::ChildExited { code: Some(0) })
        ));
        Ok(())
    }

    #[test]
    fn drain_events_limits_output_bytes_per_call() -> AppResult<()> {
        let mut backend = PortablePtySession::new();
        let output_event_count = (MAX_DRAIN_OUTPUT_BYTES / READ_BUFFER_SIZE) + 2;

        for index in 0..output_event_count {
            let mut bytes = vec![0; READ_BUFFER_SIZE];
            bytes[0] = (index % 256) as u8;
            backend.events.push(TerminalEvent::PtyOutput(bytes));
        }

        let first_events = <PortablePtySession as PtyBackend>::drain_events(&mut backend)?;
        let second_events = <PortablePtySession as PtyBackend>::drain_events(&mut backend)?;

        assert_eq!(
            first_events
                .iter()
                .filter(|event| matches!(event, TerminalEvent::PtyOutput(_)))
                .count(),
            MAX_DRAIN_OUTPUT_BYTES / READ_BUFFER_SIZE
        );
        assert_eq!(
            second_events
                .iter()
                .filter(|event| matches!(event, TerminalEvent::PtyOutput(_)))
                .count(),
            2
        );
        assert!(matches!(
            second_events.first(),
            Some(TerminalEvent::PtyOutput(bytes))
                if bytes.first().copied()
                    == Some(((MAX_DRAIN_OUTPUT_BYTES / READ_BUFFER_SIZE) % 256) as u8)
        ));
        Ok(())
    }

    #[test]
    fn drain_events_limits_control_events_per_call() -> AppResult<()> {
        let mut backend = PortablePtySession::new();

        for index in 0..(MAX_DRAIN_EVENT_COUNT + 2) {
            backend.events.push(TerminalEvent::ChildExited {
                code: Some(index as u32),
            });
        }

        let first_events = <PortablePtySession as PtyBackend>::drain_events(&mut backend)?;
        let second_events = <PortablePtySession as PtyBackend>::drain_events(&mut backend)?;

        assert_eq!(first_events.len(), MAX_DRAIN_EVENT_COUNT);
        assert_eq!(second_events.len(), 2);
        assert!(matches!(
            second_events.as_slice(),
            [
                TerminalEvent::ChildExited {
                    code: Some(first_code)
                },
                TerminalEvent::ChildExited {
                    code: Some(second_code)
                }
            ] if *first_code == MAX_DRAIN_EVENT_COUNT as u32
                && *second_code == (MAX_DRAIN_EVENT_COUNT + 1) as u32
        ));
        Ok(())
    }

    #[test]
    fn pty_event_queue_bounds_pending_output() -> AppResult<()> {
        const EXTRA_OUTPUTS: usize = 10;
        let queue = PtyEventQueue::new();

        for index in 0..(MAX_PENDING_EVENT_COUNT + EXTRA_OUTPUTS) {
            let mut bytes = vec![0; READ_BUFFER_SIZE];
            bytes[0] = (index % 256) as u8;
            queue.push(TerminalEvent::PtyOutput(bytes));
        }

        let events = queue.drain();
        assert_eq!(events.len(), MAX_PENDING_EVENT_COUNT + 1);
        assert!(matches!(
            events.first(),
            Some(TerminalEvent::PtyOutputDropped { byte_count })
                if *byte_count == EXTRA_OUTPUTS * READ_BUFFER_SIZE
        ));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, TerminalEvent::PtyOutput(_)))
                .count(),
            MAX_PENDING_EVENT_COUNT
        );

        let Some(TerminalEvent::PtyOutput(bytes)) = events.get(1) else {
            return Err(AppError::pty_message(
                "assert bounded pty event queue",
                "bounded event queue did not retain pty output",
            ));
        };
        assert_eq!(bytes.first().copied(), Some((EXTRA_OUTPUTS % 256) as u8));
        Ok(())
    }

    #[test]
    fn pty_event_queue_prefers_control_events_when_full() {
        let queue = PtyEventQueue::new();

        for _ in 0..MAX_PENDING_EVENT_COUNT {
            queue.push(TerminalEvent::PtyOutput(vec![b'x'; READ_BUFFER_SIZE]));
        }
        queue.push(TerminalEvent::PtyClosed);

        let events = queue.drain();
        assert_eq!(events.len(), MAX_PENDING_EVENT_COUNT + 1);
        assert!(matches!(
            events.first(),
            Some(TerminalEvent::PtyOutputDropped { byte_count })
                if *byte_count == READ_BUFFER_SIZE
        ));
        assert!(matches!(events.last(), Some(TerminalEvent::PtyClosed)));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, TerminalEvent::PtyOutput(_)))
                .count(),
            MAX_PENDING_EVENT_COUNT - 1
        );
    }

    #[test]
    fn pty_event_queue_rejects_events_after_close() {
        let queue = PtyEventQueue::new();

        assert!(queue.push(TerminalEvent::PtyOutput(vec![b'a'])));
        queue.close();

        assert!(!queue.push(TerminalEvent::PtyOutput(vec![b'b'])));
        assert!(queue.drain().is_empty());
    }

    #[test]
    #[ignore = "starts a real shell through Windows ConPTY"]
    fn accepts_command_input_and_backspace_through_pty() -> AppResult<()> {
        let size = TerminalSize::new(24, 80)?;
        let mut session = TerminalSession::new(
            PortablePtySession::new(),
            AlacrittyTerminalBuffer::new(size),
            size,
        );
        session.start()?;

        let smoke_result = (|| -> AppResult<()> {
            send_text_input(&mut session, "echo hello")?;
            send_key_input(&mut session, TerminalKey::Enter)?;
            wait_for_viewport(&mut session, Duration::from_secs(5), |viewport| {
                viewport_has_trimmed_line(viewport, "hello")
            })?;

            send_text_input(&mut session, "echo backspace-oX")?;
            send_key_input(&mut session, TerminalKey::Backspace)?;
            send_text_input(&mut session, "k")?;
            send_key_input(&mut session, TerminalKey::Enter)?;
            wait_for_viewport(&mut session, Duration::from_secs(5), |viewport| {
                viewport_has_trimmed_line(viewport, "backspace-ok")
            })
        })();

        let shutdown_result = session.shutdown();
        smoke_result?;
        shutdown_result
    }

    #[test]
    #[ignore = "starts a real shell through Windows ConPTY"]
    fn accepts_arrow_key_line_editing_through_pty() -> AppResult<()> {
        let size = TerminalSize::new(24, 80)?;
        let mut session = TerminalSession::new(
            PortablePtySession::new(),
            AlacrittyTerminalBuffer::new(size),
            size,
        );
        session.start()?;

        let smoke_result = (|| -> AppResult<()> {
            send_text_input(&mut session, "echo ab")?;
            send_key_input(&mut session, TerminalKey::ArrowLeft)?;
            send_text_input(&mut session, "X")?;
            send_key_input(&mut session, TerminalKey::Enter)?;
            wait_for_viewport(&mut session, Duration::from_secs(5), |viewport| {
                viewport_has_trimmed_line(viewport, "aXb")
            })
        })();

        let shutdown_result = session.shutdown();
        smoke_result?;
        shutdown_result
    }

    #[test]
    #[ignore = "starts a real shell through Windows ConPTY"]
    fn ctrl_c_interrupts_running_command_through_pty() -> AppResult<()> {
        let size = TerminalSize::new(24, 80)?;
        let mut session = TerminalSession::new(
            PortablePtySession::new(),
            AlacrittyTerminalBuffer::new(size),
            size,
        );
        session.start()?;

        let smoke_result = (|| -> AppResult<()> {
            send_text_input(&mut session, "ping 127.0.0.1 -n 30")?;
            send_key_input(&mut session, TerminalKey::Enter)?;
            wait_for_viewport(&mut session, Duration::from_secs(5), |viewport| {
                viewport_contains_text(viewport, "127.0.0.1")
            })?;

            session.handle_input(terminal_input_from_char('\u{03}'))?;
            wait_for_viewport(&mut session, Duration::from_secs(5), |viewport| {
                viewport_looks_ready_for_command(viewport)
            })?;
            send_text_input(&mut session, "echo ctrl-c-after")?;
            send_key_input(&mut session, TerminalKey::Enter)?;
            wait_for_viewport(&mut session, Duration::from_secs(5), |viewport| {
                viewport_has_trimmed_line(viewport, "ctrl-c-after")
            })
        })();

        let shutdown_result = session.shutdown();
        smoke_result?;
        shutdown_result
    }

    #[test]
    #[ignore = "starts a real shell through Windows ConPTY"]
    fn runs_button_command_and_accepts_direct_input_afterward() -> AppResult<()> {
        let size = TerminalSize::new(24, 80)?;
        let mut session = TerminalSession::new(
            PortablePtySession::with_default_shell_for_test(DefaultShell::CommandPrompt(
                PathBuf::from("cmd.exe"),
            )),
            AlacrittyTerminalBuffer::new(size),
            size,
        );
        session.start()?;

        let smoke_result = (|| -> AppResult<()> {
            session.run_command_text(&CommandText::from_static("cd"))?;
            wait_for_viewport(&mut session, Duration::from_secs(5), |viewport| {
                viewport_has_windows_path_line(viewport)
            })?;

            session.run_command_text(&CommandText::from_static("dir"))?;
            wait_for_viewport(&mut session, Duration::from_secs(5), |viewport| {
                viewport_looks_like_dir_output(viewport)
            })?;

            session.run_command_text(&CommandText::from_static("echo hello"))?;
            wait_for_viewport(&mut session, Duration::from_secs(5), |viewport| {
                viewport_has_trimmed_line(viewport, "hello")
            })?;

            session.run_command_text(&CommandText::from_static("echo before-cls-marker"))?;
            wait_for_viewport(&mut session, Duration::from_secs(5), |viewport| {
                viewport_has_trimmed_line(viewport, "before-cls-marker")
            })?;

            session.run_command_text(&CommandText::from_static("cls"))?;
            wait_for_viewport(&mut session, Duration::from_secs(5), |viewport| {
                !viewport_contains_text(viewport, "before-cls-marker")
                    && viewport_looks_ready_for_command(viewport)
            })?;

            send_text_input(&mut session, "echo manual")?;
            send_key_input(&mut session, TerminalKey::Enter)?;
            wait_for_viewport(&mut session, Duration::from_secs(5), |viewport| {
                viewport_has_trimmed_line(viewport, "manual")
            })
        })();

        let shutdown_result = session.shutdown();
        smoke_result?;
        shutdown_result
    }

    #[test]
    #[ignore = "starts a real shell through Windows ConPTY"]
    fn accepts_set_p_interactive_input_through_pty() -> AppResult<()> {
        let size = TerminalSize::new(24, 80)?;
        let mut session = TerminalSession::new(
            PortablePtySession::with_default_shell_for_test(DefaultShell::CommandPrompt(
                PathBuf::from("cmd.exe"),
            )),
            AlacrittyTerminalBuffer::new(size),
            size,
        );
        session.start()?;

        let smoke_result = (|| -> AppResult<()> {
            send_text_input(&mut session, "set /p NAME=Name:")?;
            send_key_input(&mut session, TerminalKey::Enter)?;
            wait_for_viewport(&mut session, Duration::from_secs(5), |viewport| {
                viewport_contains_text(viewport, "Name:")
            })?;

            send_text_input(&mut session, "smoke-value")?;
            send_key_input(&mut session, TerminalKey::Enter)?;
            send_text_input(&mut session, "echo typed=%NAME%")?;
            send_key_input(&mut session, TerminalKey::Enter)?;
            wait_for_viewport(&mut session, Duration::from_secs(5), |viewport| {
                viewport_contains_text(viewport, "typed=smoke-value")
            })
        })();

        let shutdown_result = session.shutdown();
        smoke_result?;
        shutdown_result
    }

    #[cfg(target_os = "windows")]
    #[test]
    #[ignore = "starts a real shell through Windows ConPTY"]
    fn accepts_read_host_interactive_input_through_pty() -> AppResult<()> {
        let size = TerminalSize::new(24, 80)?;
        let mut session = TerminalSession::new(
            PortablePtySession::with_default_shell_for_test(DefaultShell::PowerShell(
                powershell_path_for_test(),
            )),
            AlacrittyTerminalBuffer::new(size),
            size,
        );
        session.start()?;

        let smoke_result = (|| -> AppResult<()> {
            send_text_input(
                &mut session,
                "$name = Read-Host \"Name\"; Write-Output \"typed=$name\"",
            )?;
            send_key_input(&mut session, TerminalKey::Enter)?;
            wait_for_viewport(&mut session, Duration::from_secs(5), |viewport| {
                viewport_contains_text(viewport, "Name")
            })?;

            send_text_input(&mut session, "smoke-value")?;
            send_key_input(&mut session, TerminalKey::Enter)?;
            wait_for_viewport(&mut session, Duration::from_secs(5), |viewport| {
                viewport_contains_text(viewport, "typed=smoke-value")
            })
        })();

        let shutdown_result = session.shutdown();
        smoke_result?;
        shutdown_result
    }

    #[cfg(target_os = "windows")]
    fn powershell_path_for_test() -> PathBuf {
        windows_powershell_path().unwrap_or_else(|| PathBuf::from("powershell.exe"))
    }

    #[test]
    fn shutdown_requests_graceful_exit_before_forced_termination() -> AppResult<()> {
        let writer = RecordingWriter::default();
        let written = writer.bytes.clone();
        let child = MockChild::exits_after_running_polls(1);
        let state = child.state.clone();
        let mut backend = PortablePtySession::new();
        let expected_exit = backend.default_shell.graceful_exit_sequence().to_vec();
        install_writer_for_test(&mut backend, writer)?;
        backend.child = Some(Box::new(child));

        backend.shutdown()?;

        wait_until(Duration::from_secs(1), || {
            let written = written
                .lock()
                .map_err(|_| AppError::InvalidState("recording writer mutex poisoned"))?;
            Ok(written.as_slice() == expected_exit.as_slice())
        })?;
        wait_until(Duration::from_secs(1), || {
            let state = state
                .lock()
                .map_err(|_| AppError::InvalidState("mock child mutex poisoned"))?;
            Ok(!state.killed && state.running_polls_before_exit == 0)
        })?;

        let written = written
            .lock()
            .map_err(|_| AppError::InvalidState("recording writer mutex poisoned"))?;
        assert_eq!(&*written, expected_exit.as_slice());
        let state = state
            .lock()
            .map_err(|_| AppError::InvalidState("mock child mutex poisoned"))?;
        assert!(!state.killed);
        assert!(backend.child.is_none());
        Ok(())
    }

    #[test]
    fn shutdown_falls_back_to_termination_when_graceful_exit_times_out() -> AppResult<()> {
        let child = MockChild::exits_only_after_kill();
        let state = child.state.clone();
        let mut backend = PortablePtySession::new();
        install_writer_for_test(&mut backend, RecordingWriter::default())?;
        backend.child = Some(Box::new(child));

        backend.shutdown()?;

        wait_until(Duration::from_secs(1), || {
            let state = state
                .lock()
                .map_err(|_| AppError::InvalidState("mock child mutex poisoned"))?;
            Ok(state.killed)
        })?;

        let state = state
            .lock()
            .map_err(|_| AppError::InvalidState("mock child mutex poisoned"))?;
        assert!(state.killed);
        assert_eq!(state.wait_calls, 0);
        assert!(backend.child.is_none());
        Ok(())
    }

    #[test]
    fn shutdown_returns_before_reader_join_timeout() -> AppResult<()> {
        let mut backend = PortablePtySession::new();
        let (release_tx, release_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let reader_thread = thread::spawn(move || {
            let _ = release_rx.recv();
            let _ = done_tx.send(());
        });
        backend.workers = Some(PtyIoWorkers::with_reader_completion_for_test(
            reader_thread,
            done_rx,
        ));

        let started = Instant::now();
        backend.shutdown()?;

        assert!(
            started.elapsed() < Duration::from_millis(500),
            "shutdown waited for the reader join timeout"
        );
        assert!(backend.workers.is_none());

        release_tx
            .send(())
            .map_err(|_| AppError::InvalidState("release blocked reader thread"))?;
        wait_until(Duration::from_secs(1), || {
            let _ = <PortablePtySession as PtyBackend>::drain_events(&mut backend)?;
            Ok(backend.cleanup_tasks.is_empty())
        })?;
        Ok(())
    }

    #[test]
    fn start_rejects_pending_shutdown_cleanup() -> AppResult<()> {
        let mut backend = PortablePtySession::new();
        let (release_tx, release_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let reader_thread = thread::spawn(move || {
            let _ = release_rx.recv();
            let _ = done_tx.send(());
        });
        backend.workers = Some(PtyIoWorkers::with_reader_completion_for_test(
            reader_thread,
            done_rx,
        ));

        backend.shutdown()?;
        let result = backend.start(TerminalSize::new(24, 80)?, None);

        assert!(matches!(
            result,
            Err(AppError::InvalidState("pty session is shutting down"))
        ));
        assert!(!backend.cleanup_tasks.is_empty());

        release_tx
            .send(())
            .map_err(|_| AppError::InvalidState("release blocked reader thread"))?;
        wait_until(Duration::from_secs(1), || {
            let _ = <PortablePtySession as PtyBackend>::drain_events(&mut backend)?;
            Ok(backend.cleanup_tasks.is_empty())
        })?;
        Ok(())
    }

    #[test]
    fn start_preserves_pending_cleanup_failure() -> AppResult<()> {
        let child = MockChild::exits_only_after_kill();
        let mut backend = PortablePtySession::new();
        install_failing_writer_for_test(&mut backend)?;
        backend.child = Some(Box::new(child));

        backend.shutdown()?;
        wait_until(Duration::from_secs(5), || Ok(backend.poll_cleanup_tasks()))?;
        let result = backend.start(TerminalSize::new(24, 80)?, None);

        assert!(matches!(
            result,
            Err(AppError::InvalidState("pty cleanup failure is pending"))
        ));
        let events = <PortablePtySession as PtyBackend>::drain_events(&mut backend)?;
        assert!(events.iter().any(|event| {
            matches!(
                event,
                TerminalEvent::Failure(failure)
                    if failure.user_message == SHUTDOWN_FAILURE_USER_MESSAGE
                        && failure.operation == Some("write graceful pty shutdown")
            )
        }));
        Ok(())
    }

    #[test]
    fn cleanup_reports_graceful_exit_request_failure() -> AppResult<()> {
        let child = MockChild::exits_only_after_kill();
        let state = child.state.clone();
        let (input_tx, input_rx) = mpsc::sync_channel(1);
        drop(input_rx);
        let resources = PtyCleanupResources::new(
            None,
            Some(Box::new(child)),
            Some(PtyIoWorkers::with_input_tx_for_test(input_tx)),
        );

        let error =
            match resources.shutdown(DefaultShell::CommandPrompt(PathBuf::from("cmd.exe")), true) {
                Ok(()) => {
                    return Err(AppError::pty_message(
                        "assert pty shutdown failure",
                        "shutdown unexpectedly hid graceful exit request failure",
                    ));
                }
                Err(failure) => {
                    let (_, error) = failure.into_parts();
                    error
                }
            };

        assert!(matches!(
            error,
            AppError::InvalidState("pty writer worker is not running")
        ));
        let state = state
            .lock()
            .map_err(|_| AppError::InvalidState("mock child mutex poisoned"))?;
        assert!(state.killed);
        Ok(())
    }

    #[test]
    fn shutdown_reports_graceful_exit_write_failure_after_event_queue_swap() -> AppResult<()> {
        let child = MockChild::exits_only_after_kill();
        let state = child.state.clone();
        let mut backend = PortablePtySession::new();
        install_failing_writer_for_test(&mut backend)?;
        backend.child = Some(Box::new(child));

        backend.shutdown()?;

        wait_for_shutdown_failure(&mut backend, "write graceful pty shutdown")?;
        let state = state
            .lock()
            .map_err(|_| AppError::InvalidState("mock child mutex poisoned"))?;
        assert!(state.killed);
        Ok(())
    }

    #[test]
    fn shutdown_resources_reports_cleanup_failure() -> AppResult<()> {
        let child = MockChild::exits_only_after_kill();
        let state = child.state.clone();
        let (input_tx, input_rx) = mpsc::sync_channel(1);
        drop(input_rx);
        let resources = PtyCleanupResources::new(
            None,
            Some(Box::new(child)),
            Some(PtyIoWorkers::with_input_tx_for_test(input_tx)),
        );

        let error = match shutdown_resources(
            resources,
            DefaultShell::CommandPrompt(PathBuf::from("cmd.exe")),
            true,
        ) {
            Ok(()) => {
                return Err(AppError::pty_message(
                    "assert async pty shutdown failure",
                    "shutdown resources unexpectedly hid cleanup failure",
                ));
            }
            Err(failure) => {
                let (_, error) = failure.into_parts();
                error
            }
        };

        assert!(matches!(
            error,
            AppError::InvalidState("pty writer worker is not running")
        ));
        let state = state
            .lock()
            .map_err(|_| AppError::InvalidState("mock child mutex poisoned"))?;
        assert!(state.killed);
        Ok(())
    }

    #[test]
    fn start_failure_reports_cleanup_failure_and_restores_child() -> AppResult<()> {
        let child = MockChild::fails_to_kill();
        let state = child.state.clone();
        let resources = PtyCleanupResources::new(None, Some(Box::new(child)), None);
        let mut backend = PortablePtySession::new();

        let error = match backend.cleanup_after_start_failure(
            resources,
            AppError::io(
                "spawn pty reader thread",
                io::Error::other("reader spawn failed"),
            ),
            "cleanup pty after reader thread spawn failure",
        ) {
            Ok(()) => {
                return Err(AppError::pty_message(
                    "assert pty start cleanup failure",
                    "start failure cleanup unexpectedly succeeded",
                ));
            }
            Err(error) => error,
        };

        assert_eq!(
            error.operation(),
            Some("cleanup pty after reader thread spawn failure")
        );
        let AppError::Pty(context) = &error else {
            return Err(AppError::pty_message(
                "assert pty start cleanup failure",
                "start failure cleanup did not return a pty error",
            ));
        };
        assert!(context.cause().contains("spawn pty reader thread"));
        assert!(context.cause().contains("terminate pty child"));
        assert!(backend.child.is_some());

        let state = state
            .lock()
            .map_err(|_| AppError::InvalidState("mock child mutex poisoned"))?;
        assert_eq!(state.kill_calls, 1);
        drop(state);

        backend.child = None;
        Ok(())
    }

    #[test]
    fn background_cleanup_failure_restores_child() -> AppResult<()> {
        let child = MockChild::fails_to_kill();
        let state = child.state.clone();
        let mut backend = PortablePtySession::new();
        backend.child = Some(Box::new(child));

        backend.spawn_cleanup(false)?;

        wait_until(Duration::from_secs(5), || Ok(backend.poll_cleanup_tasks()))?;
        assert!(backend.child.is_some());
        let events = backend.drain_event_channel();
        assert!(events.iter().any(|event| {
            matches!(
                event,
                TerminalEvent::Failure(failure)
                    if failure.user_message == SHUTDOWN_FAILURE_USER_MESSAGE
                        && failure.operation == Some("terminate pty child")
            )
        }));

        let state = state
            .lock()
            .map_err(|_| AppError::InvalidState("mock child mutex poisoned"))?;
        assert_eq!(state.kill_calls, 1);
        drop(state);

        backend.child = None;
        Ok(())
    }

    #[test]
    fn shutdown_reports_reader_thread_join_failure() -> AppResult<()> {
        let resources = PtyCleanupResources::new(
            None,
            None,
            Some(PtyIoWorkers::with_reader_for_test(thread::spawn(|| {
                panic!("reader thread test panic")
            }))),
        );

        let error = match resources
            .shutdown(DefaultShell::CommandPrompt(PathBuf::from("cmd.exe")), false)
        {
            Ok(()) => {
                return Err(AppError::pty_message(
                    "assert pty reader join failure",
                    "shutdown unexpectedly hid reader thread join failure",
                ));
            }
            Err(failure) => {
                let (_, error) = failure.into_parts();
                error
            }
        };

        assert!(matches!(
            error,
            AppError::Pty(context) if context.cause() == "pty reader thread panicked"
        ));
        Ok(())
    }

    #[test]
    fn shutdown_reports_reader_join_timeout_as_async_failure() -> AppResult<()> {
        let mut backend = PortablePtySession::new();
        let (release_tx, release_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let reader_thread = thread::spawn(move || {
            let _ = release_rx.recv();
            let _ = done_tx.send(());
        });
        backend.workers = Some(PtyIoWorkers::with_reader_completion_for_test(
            reader_thread,
            done_rx,
        ));

        backend.shutdown()?;

        wait_until(Duration::from_secs(5), || Ok(backend.poll_cleanup_tasks()))?;
        assert!(backend.workers.is_some());
        let events = backend.drain_event_channel();
        assert!(events.iter().any(|event| {
            matches!(
                event,
                TerminalEvent::Failure(failure)
                    if failure.user_message == SHUTDOWN_FAILURE_USER_MESSAGE
                        && failure.operation == Some("join pty reader thread")
            )
        }));

        release_tx
            .send(())
            .map_err(|_| AppError::InvalidState("release blocked reader thread"))?;
        backend.shutdown()?;
        wait_until(Duration::from_secs(1), || {
            let _ = <PortablePtySession as PtyBackend>::drain_events(&mut backend)?;
            Ok(backend.cleanup_tasks.is_empty() && backend.workers.is_none())
        })?;
        Ok(())
    }

    #[test]
    fn shutdown_closes_old_event_queue_for_background_cleanup() -> AppResult<()> {
        let mut backend = PortablePtySession::new();
        let worker_events = backend.events.clone();
        let (release_tx, release_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let reader_thread = thread::spawn(move || {
            let _ = release_rx.recv();
            let _ = worker_events.push(TerminalEvent::PtyOutput(b"after-timeout".to_vec()));
            let _ = done_tx.send(());
        });
        backend.workers = Some(PtyIoWorkers::with_reader_completion_for_test(
            reader_thread,
            done_rx,
        ));

        backend.shutdown()?;

        release_tx
            .send(())
            .map_err(|_| AppError::InvalidState("release blocked reader thread"))?;
        wait_until(Duration::from_secs(1), || {
            let _ = <PortablePtySession as PtyBackend>::drain_events(&mut backend)?;
            Ok(backend.cleanup_tasks.is_empty())
        })?;
        let events = <PortablePtySession as PtyBackend>::drain_events(&mut backend)?;

        assert!(!events.iter().any(|event| {
            matches!(event, TerminalEvent::PtyOutput(bytes) if bytes.as_slice() == b"after-timeout")
        }));
        Ok(())
    }

    fn install_writer_for_test(
        backend: &mut PortablePtySession,
        writer: RecordingWriter,
    ) -> AppResult<()> {
        backend.workers = Some(PtyIoWorkers::with_writer_for_test(spawn_writer(
            Box::new(writer),
            backend.events.clone(),
        )?));
        Ok(())
    }

    fn install_failing_writer_for_test(backend: &mut PortablePtySession) -> AppResult<()> {
        backend.workers = Some(PtyIoWorkers::with_writer_for_test(spawn_writer(
            Box::new(FailingWriter),
            backend.events.clone(),
        )?));
        Ok(())
    }

    fn wait_until(
        timeout: Duration,
        mut condition: impl FnMut() -> AppResult<bool>,
    ) -> AppResult<()> {
        let deadline = Instant::now() + timeout;
        loop {
            if condition()? {
                return Ok(());
            }

            if Instant::now() >= deadline {
                return Err(AppError::pty_message(
                    "wait for async pty cleanup",
                    "timed out waiting for async pty cleanup",
                ));
            }

            thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_for_shutdown_failure(
        backend: &mut PortablePtySession,
        operation: &'static str,
    ) -> AppResult<()> {
        wait_until(Duration::from_secs(5), || {
            let events = <PortablePtySession as PtyBackend>::drain_events(backend)?;
            Ok(events.iter().any(|event| {
                matches!(
                    event,
                    TerminalEvent::Failure(failure)
                        if failure.user_message == SHUTDOWN_FAILURE_USER_MESSAGE
                            && failure.operation == Some(operation)
                )
            }))
        })
    }

    fn send_text_input(
        session: &mut TerminalSession<PortablePtySession, AlacrittyTerminalBuffer>,
        text: &str,
    ) -> AppResult<()> {
        for character in text.chars() {
            session.handle_input(terminal_input_from_char(character))?;
        }
        Ok(())
    }

    fn send_key_input(
        session: &mut TerminalSession<PortablePtySession, AlacrittyTerminalBuffer>,
        key: TerminalKey,
    ) -> AppResult<()> {
        session.handle_input(terminal_input_from_key(
            key,
            TerminalKeyModifiers::default(),
        ))
    }

    fn wait_for_viewport<F>(
        session: &mut TerminalSession<PortablePtySession, AlacrittyTerminalBuffer>,
        timeout: Duration,
        condition: F,
    ) -> AppResult<()>
    where
        F: Fn(&TerminalViewport) -> bool,
    {
        let deadline = Instant::now() + timeout;
        loop {
            for event in session.drain_events()? {
                match event {
                    TerminalEvent::PtyOutput(_) => {}
                    TerminalEvent::PtyOutputDropped { .. } => {}
                    TerminalEvent::PtyClosed => {}
                    TerminalEvent::ChildExited { code } => {
                        return Err(AppError::pty_message(
                            "run pty input smoke test",
                            format!("shell exited during input smoke test: {code:?}"),
                        ));
                    }
                    TerminalEvent::StdinWriteFailed(failure) => {
                        return Err(AppError::pty_message(
                            failure.operation.unwrap_or("process pty stdin failure"),
                            format!("{}: {}", failure.user_message, failure.cause),
                        ));
                    }
                    TerminalEvent::Failure(failure) => {
                        return Err(AppError::pty_message(
                            failure.operation.unwrap_or("process pty failure"),
                            format!("{}: {}", failure.user_message, failure.cause),
                        ));
                    }
                }
            }

            let viewport = session.terminal_viewport()?;
            if condition(&viewport) {
                return Ok(());
            }

            if Instant::now() >= deadline {
                let visible_lines = viewport
                    .viewport_lines()
                    .into_iter()
                    .filter(|line| !line.trim().is_empty())
                    .collect::<Vec<_>>()
                    .join(" | ");
                return Err(AppError::pty_message(
                    "wait for terminal smoke output",
                    format!(
                        "timed out waiting for terminal smoke output; visible lines: {visible_lines}"
                    ),
                ));
            }

            thread::sleep(Duration::from_millis(50));
        }
    }

    fn viewport_has_trimmed_line(viewport: &TerminalViewport, expected: &str) -> bool {
        viewport
            .viewport_lines()
            .iter()
            .any(|line| line.trim() == expected)
    }

    fn viewport_contains_text(viewport: &TerminalViewport, expected: &str) -> bool {
        viewport
            .viewport_lines()
            .iter()
            .any(|line| line.contains(expected))
    }

    fn viewport_has_windows_path_line(viewport: &TerminalViewport) -> bool {
        viewport.viewport_lines().iter().any(|line| {
            let trimmed = line.trim();
            let mut chars = trimmed.chars();
            matches!(
                (chars.next(), chars.next(), chars.next()),
                (Some(drive), Some(':'), Some('\\')) if drive.is_ascii_alphabetic()
            ) && !trimmed.contains('>')
        })
    }

    fn viewport_looks_like_dir_output(viewport: &TerminalViewport) -> bool {
        let lines = viewport.viewport_lines();
        lines.iter().any(|line| {
            line.contains("<DIR>")
                || line.contains("Directory of")
                || line.contains("디 렉 터 리")
                || line.contains("bytes")
                || line.contains("바 이 트")
                || line.contains("LastWriteTime")
        })
    }

    fn viewport_looks_ready_for_command(viewport: &TerminalViewport) -> bool {
        viewport
            .viewport_lines()
            .iter()
            .any(|line| line.trim_end().ends_with('>'))
    }

    #[derive(Clone, Default)]
    struct RecordingWriter {
        bytes: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for RecordingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let mut written = self
                .bytes
                .lock()
                .map_err(|_| io::Error::other("recording writer mutex poisoned"))?;
            written.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("test writer failure"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Clone)]
    struct MockChild {
        state: Arc<Mutex<MockChildState>>,
        output_on_exit_poll: Option<(PtyEventQueue, Vec<u8>)>,
    }

    impl std::fmt::Debug for MockChild {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("MockChild")
                .field("state", &self.state)
                .field(
                    "output_on_exit_poll",
                    &self
                        .output_on_exit_poll
                        .as_ref()
                        .map(|(_, bytes)| bytes.as_slice()),
                )
                .finish()
        }
    }

    #[derive(Debug)]
    struct MockChildState {
        running_polls_before_exit: usize,
        killed: bool,
        kill_calls: usize,
        kill_error: Option<&'static str>,
        exits_after_kill: bool,
        wait_calls: usize,
        try_wait_calls: usize,
    }

    impl MockChild {
        fn exits_after_running_polls(running_polls_before_exit: usize) -> Self {
            Self {
                state: Arc::new(Mutex::new(MockChildState {
                    running_polls_before_exit,
                    killed: false,
                    kill_calls: 0,
                    kill_error: None,
                    exits_after_kill: false,
                    wait_calls: 0,
                    try_wait_calls: 0,
                })),
                output_on_exit_poll: None,
            }
        }

        fn exits_with_output_on_exit_poll(events: PtyEventQueue, output: Vec<u8>) -> Self {
            let mut child = Self::exits_after_running_polls(0);
            child.output_on_exit_poll = Some((events, output));
            child
        }

        fn exits_only_after_kill() -> Self {
            Self {
                state: Arc::new(Mutex::new(MockChildState {
                    running_polls_before_exit: usize::MAX,
                    killed: false,
                    kill_calls: 0,
                    kill_error: None,
                    exits_after_kill: true,
                    wait_calls: 0,
                    try_wait_calls: 0,
                })),
                output_on_exit_poll: None,
            }
        }

        fn fails_to_kill() -> Self {
            Self {
                state: Arc::new(Mutex::new(MockChildState {
                    running_polls_before_exit: usize::MAX,
                    killed: false,
                    kill_calls: 0,
                    kill_error: Some("test child kill failure"),
                    exits_after_kill: false,
                    wait_calls: 0,
                    try_wait_calls: 0,
                })),
                output_on_exit_poll: None,
            }
        }
    }

    impl ChildKiller for MockChild {
        fn kill(&mut self) -> io::Result<()> {
            let mut state = self
                .state
                .lock()
                .map_err(|_| io::Error::other("mock child mutex poisoned"))?;
            state.kill_calls = state.kill_calls.saturating_add(1);
            if let Some(error) = state.kill_error {
                return Err(io::Error::other(error));
            }
            state.killed = true;
            if state.exits_after_kill {
                state.running_polls_before_exit = 0;
            }
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(self.clone())
        }
    }

    impl Child for MockChild {
        fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            let exited = {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| io::Error::other("mock child mutex poisoned"))?;
                state.try_wait_calls = state.try_wait_calls.saturating_add(1);
                if state.running_polls_before_exit == 0 {
                    true
                } else {
                    state.running_polls_before_exit =
                        state.running_polls_before_exit.saturating_sub(1);
                    false
                }
            };

            if exited {
                if let Some((events, output)) = self.output_on_exit_poll.take() {
                    let _ = events.push(TerminalEvent::PtyOutput(output));
                }
                return Ok(Some(ExitStatus::with_exit_code(0)));
            }

            Ok(None)
        }

        fn wait(&mut self) -> io::Result<ExitStatus> {
            let mut state = self
                .state
                .lock()
                .map_err(|_| io::Error::other("mock child mutex poisoned"))?;
            state.wait_calls = state.wait_calls.saturating_add(1);
            Ok(ExitStatus::with_exit_code(0))
        }

        fn process_id(&self) -> Option<u32> {
            None
        }

        #[cfg(target_os = "windows")]
        fn as_raw_handle(&self) -> Option<RawHandle> {
            None
        }
    }
}
