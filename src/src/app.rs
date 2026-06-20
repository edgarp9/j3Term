use std::collections::VecDeque;

use crate::domain::{
    CommandText, MAX_TERMINAL_TABS, SessionStatus, ShellCommandDialect, StartupCommand,
    StartupDirectory, TerminalCommand, TerminalEvent, TerminalFailure, TerminalInput,
    TerminalInputBytes, TerminalScroll, TerminalSize, TerminalTabId, TerminalTabView,
    TerminalViewport, session_status_after_event, terminal_paste_text_to_pty_bytes,
};
use crate::error::{AppError, AppResult};

// Bounds synchronous terminal parsing done from a single timer drain.
const MAX_TERMINAL_INGEST_BYTES_PER_DRAIN: usize = 32 * 1024;
const MAX_BACKGROUND_TABS_PER_TIMER_DRAIN: usize = 3;

pub trait PtyBackend {
    fn start(
        &mut self,
        size: TerminalSize,
        startup_directory: Option<&StartupDirectory>,
    ) -> AppResult<()>;
    fn write_input(&mut self, bytes: Vec<u8>) -> AppResult<()>;
    fn write_terminal_input(&mut self, bytes: TerminalInputBytes) -> AppResult<()> {
        self.write_input(bytes.to_vec())
    }
    fn resize(&mut self, size: TerminalSize) -> AppResult<()>;
    fn drain_events(&mut self) -> AppResult<Vec<TerminalEvent>>;
    fn shutdown(&mut self) -> AppResult<()>;
    fn is_shutdown_pending(&self) -> bool {
        false
    }
    fn shell_command_dialect(&self) -> ShellCommandDialect {
        ShellCommandDialect::CommandPrompt
    }
}

pub trait TerminalViewportPort {
    fn ingest_output(&mut self, bytes: &[u8]) -> AppResult<()>;
    fn take_pending_pty_writes(&mut self) -> AppResult<Vec<Vec<u8>>>;
    fn resize(&mut self, size: TerminalSize) -> AppResult<()>;
    fn scroll_display(&mut self, _scroll: TerminalScroll) -> AppResult<bool> {
        Ok(false)
    }
    fn snapshot(&mut self) -> AppResult<TerminalViewport>;
    fn snapshot_into(&mut self, viewport: &mut TerminalViewport) -> AppResult<()> {
        *viewport = self.snapshot()?;
        Ok(())
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TerminalTimerDrain {
    pub had_events: bool,
    pub active_tab_dirty: bool,
    pub needs_active_poll: bool,
    pub failure_cause: Option<String>,
}

impl TerminalTimerDrain {
    fn record_tab_events(&mut self, active_tab: bool, events: Vec<TerminalEvent>) {
        if events.is_empty() {
            return;
        }

        self.had_events = true;
        if active_tab {
            self.active_tab_dirty = true;
        }

        for event in events {
            if let TerminalEvent::Failure(failure) = event {
                self.failure_cause = Some(failure.cause);
            }
        }
    }

    fn record_active_poll_needed(&mut self) {
        self.needs_active_poll = true;
    }
}

pub struct TerminalTabs<B, T>
where
    B: PtyBackend,
    T: TerminalViewportPort,
{
    tabs: Vec<TerminalTab<B, T>>,
    active_id: TerminalTabId,
    next_id: u32,
    size: TerminalSize,
    background_drain_cursor: usize,
    backend_factory: fn() -> B,
    terminal_factory: fn(TerminalSize) -> T,
}

struct TerminalTab<B, T>
where
    B: PtyBackend,
    T: TerminalViewportPort,
{
    id: TerminalTabId,
    title: String,
    session: TerminalSession<B, T>,
}

impl<B, T> TerminalTabs<B, T>
where
    B: PtyBackend,
    T: TerminalViewportPort,
{
    pub fn new(
        size: TerminalSize,
        backend_factory: fn() -> B,
        terminal_factory: fn(TerminalSize) -> T,
    ) -> Self {
        let first_id = TerminalTabId::new(1);
        let first_tab = make_terminal_tab(first_id, size, backend_factory, terminal_factory);

        Self {
            tabs: vec![first_tab],
            active_id: first_id,
            next_id: 2,
            size,
            background_drain_cursor: 0,
            backend_factory,
            terminal_factory,
        }
    }

    pub fn start(&mut self) -> AppResult<()> {
        self.start_with_startup_directory(None)
    }

    pub fn start_with_startup_directory(
        &mut self,
        startup_directory: Option<&StartupDirectory>,
    ) -> AppResult<()> {
        self.active_session_mut()?
            .start_with_startup_directory(startup_directory)
    }

    pub fn execute(&mut self, command: TerminalCommand) -> AppResult<()> {
        match command {
            TerminalCommand::Resize(size) => self.resize(size),
            TerminalCommand::Shutdown => self.shutdown(),
        }
    }

    pub fn open_tab(&mut self) -> AppResult<TerminalTabId> {
        if self.tabs.len() >= MAX_TERMINAL_TABS {
            return Err(AppError::InvalidState("maximum terminal tab count reached"));
        }

        let id = self.allocate_tab_id()?;
        let mut tab = make_terminal_tab(id, self.size, self.backend_factory, self.terminal_factory);
        tab.session.start()?;
        self.tabs.push(tab);
        self.active_id = id;
        Ok(id)
    }

    pub fn close_tab(&mut self, id: TerminalTabId) -> AppResult<()> {
        if self.tabs.len() <= 1 {
            return Err(AppError::InvalidState(
                "at least one terminal tab must stay open",
            ));
        }

        let was_active = self.active_id == id;
        let index = self
            .tabs
            .iter()
            .position(|tab| tab.id == id)
            .ok_or(AppError::InvalidInput("unknown terminal tab"))?;
        self.tabs[index].session.shutdown()?;
        self.tabs.remove(index);

        if was_active {
            let new_index = index
                .saturating_sub(1)
                .min(self.tabs.len().saturating_sub(1));
            self.active_id = self.tabs[new_index].id;
            self.sync_session_size(new_index)?;
        }

        Ok(())
    }

    pub fn switch_to_tab(&mut self, id: TerminalTabId) -> AppResult<()> {
        let index = self
            .tabs
            .iter()
            .position(|tab| tab.id == id)
            .ok_or(AppError::InvalidInput("unknown terminal tab"))?;

        self.sync_session_size(index)?;
        self.active_id = id;
        Ok(())
    }

    pub fn run_command_text(&mut self, command_text: &CommandText) -> AppResult<()> {
        self.active_session_mut()?.run_command_text(command_text)
    }

    pub fn active_shell_command_dialect(&mut self) -> AppResult<ShellCommandDialect> {
        self.active_session_mut()
            .map(|session| session.shell_command_dialect())
    }

    pub fn run_startup_command(&mut self, command: &StartupCommand) -> AppResult<()> {
        self.active_session_mut()?.run_startup_command(command)
    }

    pub fn handle_input(&mut self, input: TerminalInput) -> AppResult<()> {
        self.active_session_mut()?.handle_input(input)
    }

    pub fn paste_text(&mut self, text: &str) -> AppResult<()> {
        self.active_session_mut()?.paste_text(text)
    }

    pub fn drain_timer_events(&mut self) -> AppResult<TerminalTimerDrain> {
        let active_index = self.active_tab_index()?;
        let mut drain = TerminalTimerDrain::default();
        self.drain_tab_events_into(active_index, active_index, &mut drain)?;
        self.drain_background_tabs_into(active_index, &mut drain)?;

        Ok(drain)
    }

    pub fn terminal_viewport(&mut self) -> AppResult<TerminalViewport> {
        self.active_session_mut()?.terminal_viewport()
    }

    pub fn refresh_terminal_viewport(&mut self, viewport: &mut TerminalViewport) -> AppResult<()> {
        self.active_session_mut()?
            .refresh_terminal_viewport(viewport)
    }

    pub fn scroll_terminal_display(&mut self, scroll: TerminalScroll) -> AppResult<bool> {
        self.active_session_mut()?.scroll_display(scroll)
    }

    pub fn display_recoverable_error(&mut self, user_message: &str) -> AppResult<()> {
        self.active_session_mut()?
            .display_recoverable_error(user_message)
    }

    pub fn display_status_message(&mut self, user_message: &str) -> AppResult<()> {
        self.active_session_mut()?
            .display_status_message(user_message)
    }

    pub fn tab_views(&self) -> Vec<TerminalTabView> {
        self.tabs
            .iter()
            .map(|tab| TerminalTabView::new(tab.id, tab.title.clone(), tab.id == self.active_id))
            .collect()
    }

    #[cfg(test)]
    pub fn is_active_tab(&self, id: TerminalTabId) -> bool {
        self.active_id == id
    }

    fn resize(&mut self, size: TerminalSize) -> AppResult<()> {
        if self.size == size {
            return Ok(());
        }

        let active_index = self.active_tab_index()?;
        self.resize_session_with_rollback(active_index, size)?;
        self.size = size;
        Ok(())
    }

    fn resize_session_with_rollback(&mut self, index: usize, size: TerminalSize) -> AppResult<()> {
        let previous_size = self
            .tabs
            .get(index)
            .ok_or(AppError::InvalidState(
                "terminal tab resize index is invalid",
            ))?
            .session
            .size;

        match self.tabs[index].session.resize(size) {
            Ok(()) => Ok(()),
            Err(error) => {
                if self.tabs[index].session.size != previous_size
                    && let Some(rollback_error) =
                        self.rollback_resized_tabs(&[index], previous_size)
                {
                    return Err(rollback_error);
                }
                Err(error)
            }
        }
    }

    fn rollback_resized_tabs(
        &mut self,
        tab_indices: &[usize],
        size: TerminalSize,
    ) -> Option<AppError> {
        let mut first_error = None;

        for index in tab_indices.iter().rev() {
            if let Err(error) = self.tabs[*index].session.resize(size)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }

        first_error
    }

    fn shutdown(&mut self) -> AppResult<()> {
        let mut first_error = None;

        for tab in &mut self.tabs {
            if let Err(error) = tab.session.shutdown()
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn active_session_mut(&mut self) -> AppResult<&mut TerminalSession<B, T>> {
        let active_index = self.active_tab_index()?;
        self.sync_session_size(active_index)?;
        self.tabs
            .get_mut(active_index)
            .map(|tab| &mut tab.session)
            .ok_or(AppError::InvalidState(
                "active terminal tab index is invalid",
            ))
    }

    fn sync_session_size(&mut self, index: usize) -> AppResult<()> {
        let size = self.size;
        let tab = self.tabs.get_mut(index).ok_or(AppError::InvalidState(
            "terminal tab size sync index is invalid",
        ))?;
        if tab.session.size == size {
            return Ok(());
        }

        self.resize_session_with_rollback(index, size)
    }

    fn allocate_tab_id(&mut self) -> AppResult<TerminalTabId> {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or(AppError::InvalidState("terminal tab id space is exhausted"))?;
        Ok(TerminalTabId::new(id))
    }

    fn active_tab_index(&self) -> AppResult<usize> {
        self.tabs
            .iter()
            .position(|tab| tab.id == self.active_id)
            .ok_or(AppError::InvalidState("active terminal tab is missing"))
    }

    fn drain_tab_events_into(
        &mut self,
        index: usize,
        active_index: usize,
        drain: &mut TerminalTimerDrain,
    ) -> AppResult<()> {
        self.sync_session_size(index)?;
        let tab = self.tabs.get_mut(index).ok_or(AppError::InvalidState(
            "terminal tab drain index is invalid",
        ))?;
        let events = tab.session.drain_events()?;

        drain.record_tab_events(index == active_index, events);
        if tab.session.needs_timer_drain() {
            drain.record_active_poll_needed();
        }
        Ok(())
    }

    fn drain_background_tabs_into(
        &mut self,
        active_index: usize,
        drain: &mut TerminalTimerDrain,
    ) -> AppResult<()> {
        let background_count = self.tabs.len().saturating_sub(1);
        let drain_count = background_count.min(MAX_BACKGROUND_TABS_PER_TIMER_DRAIN);

        for _ in 0..drain_count {
            let Some(background_index) = self.next_background_drain_index(active_index) else {
                break;
            };
            self.drain_tab_events_into(background_index, active_index, drain)?;
        }

        Ok(())
    }

    fn next_background_drain_index(&mut self, active_index: usize) -> Option<usize> {
        let tab_count = self.tabs.len();
        if tab_count <= 1 {
            self.background_drain_cursor = 0;
            return None;
        }

        let mut index = self.background_drain_cursor % tab_count;
        for _ in 0..tab_count {
            if index != active_index {
                self.background_drain_cursor = (index + 1) % tab_count;
                return Some(index);
            }
            index = (index + 1) % tab_count;
        }

        None
    }
}

fn make_terminal_tab<B, T>(
    id: TerminalTabId,
    size: TerminalSize,
    backend_factory: fn() -> B,
    terminal_factory: fn(TerminalSize) -> T,
) -> TerminalTab<B, T>
where
    B: PtyBackend,
    T: TerminalViewportPort,
{
    TerminalTab {
        id,
        title: format!("Tab {}", id.value()),
        session: TerminalSession::new(backend_factory(), terminal_factory(size), size),
    }
}

pub struct TerminalSession<B, T>
where
    B: PtyBackend,
    T: TerminalViewportPort,
{
    backend: B,
    terminal: T,
    pending_events: VecDeque<PendingTerminalEvent>,
    size: TerminalSize,
    status: SessionStatus,
}

enum PendingTerminalEvent {
    Event(TerminalEvent),
    PtyOutput(PendingPtyOutput),
}

#[derive(Default)]
struct PendingPtyOutput {
    bytes: Vec<u8>,
    start: usize,
}

impl PendingPtyOutput {
    fn new(bytes: Vec<u8>) -> Self {
        Self { bytes, start: 0 }
    }

    fn remaining_len(&self) -> usize {
        self.bytes.len().saturating_sub(self.start)
    }
}

impl From<TerminalEvent> for PendingTerminalEvent {
    fn from(event: TerminalEvent) -> Self {
        match event {
            TerminalEvent::PtyOutput(bytes) => Self::PtyOutput(PendingPtyOutput::new(bytes)),
            event => Self::Event(event),
        }
    }
}

impl PendingTerminalEvent {
    fn into_terminal_event(self) -> TerminalEvent {
        match self {
            Self::Event(event) => event,
            Self::PtyOutput(_) => TerminalEvent::PtyOutput(Vec::new()),
        }
    }
}

impl<B, T> TerminalSession<B, T>
where
    B: PtyBackend,
    T: TerminalViewportPort,
{
    pub fn new(backend: B, terminal: T, size: TerminalSize) -> Self {
        Self {
            backend,
            terminal,
            pending_events: VecDeque::new(),
            size,
            status: SessionStatus::Empty,
        }
    }

    pub fn start(&mut self) -> AppResult<()> {
        self.start_with_startup_directory(None)
    }

    pub fn start_with_startup_directory(
        &mut self,
        startup_directory: Option<&StartupDirectory>,
    ) -> AppResult<()> {
        if self.status == SessionStatus::ShuttingDown {
            return Err(AppError::InvalidState("terminal session is shutting down"));
        }

        if matches!(self.status, SessionStatus::Running | SessionStatus::Failed) {
            self.shutdown()?;
            if self.status == SessionStatus::ShuttingDown {
                return Err(AppError::InvalidState("terminal session is shutting down"));
            }
        }

        self.backend.start(self.size, startup_directory)?;
        self.pending_events.clear();
        self.status = SessionStatus::Running;
        Ok(())
    }

    pub fn run_command_text(&mut self, command_text: &CommandText) -> AppResult<()> {
        let bytes = command_text.to_pty_bytes();
        self.write_input(bytes)
    }

    pub fn shell_command_dialect(&self) -> ShellCommandDialect {
        self.backend.shell_command_dialect()
    }

    pub fn run_startup_command(&mut self, command: &StartupCommand) -> AppResult<()> {
        let bytes = command.to_pty_bytes(self.shell_command_dialect());

        self.write_input(bytes)
    }

    pub fn handle_input(&mut self, input: TerminalInput) -> AppResult<()> {
        let bytes = input.to_pty_bytes();
        self.write_terminal_input(bytes)
    }

    pub fn paste_text(&mut self, text: &str) -> AppResult<()> {
        let bytes = terminal_paste_text_to_pty_bytes(text);
        self.write_input(bytes)
    }

    pub fn write_input(&mut self, bytes: Vec<u8>) -> AppResult<()> {
        if self.status != SessionStatus::Running {
            return Err(AppError::InvalidState("terminal session is not running"));
        }
        self.backend.write_input(bytes)
    }

    pub fn write_terminal_input(&mut self, bytes: TerminalInputBytes) -> AppResult<()> {
        if self.status != SessionStatus::Running {
            return Err(AppError::InvalidState("terminal session is not running"));
        }
        self.backend.write_terminal_input(bytes)
    }

    pub fn resize(&mut self, size: TerminalSize) -> AppResult<()> {
        if self.size == size {
            return Ok(());
        }

        if self.status == SessionStatus::Running
            && let Err(error) = self.backend.resize(size)
        {
            return Err(error);
        }

        if let Err(error) = self.terminal.resize(size) {
            self.size = size;
            return Err(error);
        }

        self.size = size;
        Ok(())
    }

    pub fn drain_events(&mut self) -> AppResult<Vec<TerminalEvent>> {
        let was_shutting_down = self.status == SessionStatus::ShuttingDown;
        if self.pending_events.is_empty() {
            self.pending_events
                .extend(self.backend.drain_events()?.into_iter().map(Into::into));
        }
        let mut events = Vec::new();
        let mut processing_errors = EventProcessingErrors::default();
        let mut ingest_budget = TerminalIngestBudget::new(MAX_TERMINAL_INGEST_BYTES_PER_DRAIN);

        while let Some(mut event) = self.pending_events.pop_front() {
            let continue_drain =
                self.apply_pending_terminal_event_with_budget(&mut event, &mut ingest_budget);
            processing_errors.record(continue_drain.result);
            events.push(event.into_terminal_event());
            if !continue_drain.should_continue {
                break;
            }
        }

        self.reconcile_shutdown_status_after_drain(was_shutting_down, &events);
        processing_errors.finish()?;
        Ok(events)
    }

    fn needs_timer_drain(&self) -> bool {
        !self.pending_events.is_empty() || self.backend.is_shutdown_pending()
    }

    fn apply_pending_terminal_event_with_budget(
        &mut self,
        event: &mut PendingTerminalEvent,
        ingest_budget: &mut TerminalIngestBudget,
    ) -> EventProcessingStep {
        match event {
            PendingTerminalEvent::PtyOutput(output) => {
                self.ingest_pty_output_with_budget(output, ingest_budget)
            }
            PendingTerminalEvent::Event(event) => {
                EventProcessingStep::continue_after(self.apply_terminal_event(event))
            }
        }
    }

    fn apply_terminal_event(&mut self, event: &mut TerminalEvent) -> AppResult<()> {
        self.apply_event_status_transition(event);
        self.apply_terminal_event_output(event)
    }

    fn apply_event_status_transition(&mut self, event: &TerminalEvent) {
        self.status = session_status_after_event(self.status, event);
    }

    fn reconcile_shutdown_status_after_drain(
        &mut self,
        was_shutting_down: bool,
        events: &[TerminalEvent],
    ) {
        if was_shutting_down
            && events
                .iter()
                .any(|event| matches!(event, TerminalEvent::Failure(_)))
        {
            self.status = SessionStatus::Failed;
            return;
        }

        if self.backend.is_shutdown_pending() {
            if was_shutting_down || self.status == SessionStatus::Exited {
                self.status = SessionStatus::ShuttingDown;
            }
            return;
        }

        if was_shutting_down || self.status == SessionStatus::ShuttingDown {
            self.status = SessionStatus::Exited;
        }
    }

    fn apply_terminal_event_output(&mut self, event: &mut TerminalEvent) -> AppResult<()> {
        match event {
            TerminalEvent::PtyOutput(bytes) => self.ingest_pty_output(bytes),
            _ => self.ingest_terminal_event_message(event),
        }
    }

    fn ingest_pty_output(&mut self, bytes: &mut Vec<u8>) -> AppResult<()> {
        self.ingest_pty_output_slice(bytes.as_slice())?;
        *bytes = Vec::new();
        Ok(())
    }

    fn ingest_pty_output_slice(&mut self, bytes: &[u8]) -> AppResult<()> {
        self.terminal.ingest_output(bytes)?;
        self.flush_terminal_pty_writes()
    }

    fn ingest_pty_output_with_budget(
        &mut self,
        output: &mut PendingPtyOutput,
        ingest_budget: &mut TerminalIngestBudget,
    ) -> EventProcessingStep {
        let byte_count = output.remaining_len();
        if byte_count == 0 {
            *output = PendingPtyOutput::default();
            return EventProcessingStep::continue_after(self.ingest_pty_output_slice(&[]));
        }

        let ingest_len = ingest_budget.consume_output_bytes(byte_count);
        if ingest_len == 0 {
            let retained = std::mem::take(output);
            self.pending_events
                .push_front(PendingTerminalEvent::PtyOutput(retained));
            return EventProcessingStep::stop_after(Ok(()));
        }

        let ingest_start = output.start;
        let ingest_end = ingest_start.saturating_add(ingest_len);
        let result = self.ingest_pty_output_slice(&output.bytes[ingest_start..ingest_end]);
        if ingest_len < byte_count {
            output.start = ingest_end;
            let retained = std::mem::take(output);
            self.pending_events
                .push_front(PendingTerminalEvent::PtyOutput(retained));
            return EventProcessingStep::stop_after(result);
        }

        *output = PendingPtyOutput::default();
        if ingest_budget.is_exhausted() {
            EventProcessingStep::stop_after(result)
        } else {
            EventProcessingStep::continue_after(result)
        }
    }

    fn ingest_terminal_event_message(&mut self, event: &TerminalEvent) -> AppResult<()> {
        if let Some(message) = terminal_event_message(event) {
            self.terminal.ingest_output(message.as_bytes())
        } else {
            Ok(())
        }
    }

    fn flush_terminal_pty_writes(&mut self) -> AppResult<()> {
        let writes = self.terminal.take_pending_pty_writes()?;
        if self.status != SessionStatus::Running {
            return Ok(());
        }

        for bytes in writes {
            self.backend.write_input(bytes)?;
        }

        Ok(())
    }

    pub fn terminal_viewport(&mut self) -> AppResult<TerminalViewport> {
        self.terminal.snapshot()
    }

    pub fn refresh_terminal_viewport(&mut self, viewport: &mut TerminalViewport) -> AppResult<()> {
        self.terminal.snapshot_into(viewport)
    }

    pub fn scroll_display(&mut self, scroll: TerminalScroll) -> AppResult<bool> {
        self.terminal.scroll_display(scroll)
    }

    pub fn display_recoverable_error(&mut self, user_message: &str) -> AppResult<()> {
        let message = app_error_message(user_message);
        self.terminal.ingest_output(message.as_bytes())
    }

    pub fn display_status_message(&mut self, user_message: &str) -> AppResult<()> {
        let message = app_status_message(user_message);
        self.terminal.ingest_output(message.as_bytes())
    }

    pub fn shutdown(&mut self) -> AppResult<()> {
        match self.status {
            SessionStatus::Empty | SessionStatus::Exited => {
                self.status = if self.backend.is_shutdown_pending() {
                    SessionStatus::ShuttingDown
                } else {
                    SessionStatus::Exited
                };
                Ok(())
            }
            SessionStatus::ShuttingDown => Ok(()),
            SessionStatus::Running | SessionStatus::Failed => {
                self.status = SessionStatus::ShuttingDown;
                match self.backend.shutdown() {
                    Ok(()) => {
                        if !self.backend.is_shutdown_pending() {
                            self.status = SessionStatus::Exited;
                        }
                        Ok(())
                    }
                    Err(error) => {
                        self.status = SessionStatus::Failed;
                        Err(error)
                    }
                }
            }
        }
    }
}

struct TerminalIngestBudget {
    remaining_output_bytes: usize,
}

impl TerminalIngestBudget {
    fn new(max_output_bytes: usize) -> Self {
        Self {
            remaining_output_bytes: max_output_bytes,
        }
    }

    fn consume_output_bytes(&mut self, byte_count: usize) -> usize {
        let consumed = byte_count.min(self.remaining_output_bytes);
        self.remaining_output_bytes = self.remaining_output_bytes.saturating_sub(consumed);
        consumed
    }

    fn is_exhausted(&self) -> bool {
        self.remaining_output_bytes == 0
    }
}

struct EventProcessingStep {
    result: AppResult<()>,
    should_continue: bool,
}

impl EventProcessingStep {
    fn continue_after(result: AppResult<()>) -> Self {
        Self {
            result,
            should_continue: true,
        }
    }

    fn stop_after(result: AppResult<()>) -> Self {
        Self {
            result,
            should_continue: false,
        }
    }
}

#[derive(Default)]
struct EventProcessingErrors {
    first_error: Option<AppError>,
}

impl EventProcessingErrors {
    fn record(&mut self, result: AppResult<()>) {
        if self.first_error.is_some() {
            return;
        }

        if let Err(error) = result {
            self.first_error = Some(error);
        }
    }

    fn finish(self) -> AppResult<()> {
        match self.first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

fn terminal_event_message(event: &TerminalEvent) -> Option<std::borrow::Cow<'_, str>> {
    match event {
        TerminalEvent::PtyOutput(_) => None,
        TerminalEvent::PtyOutputDropped { byte_count } => Some(std::borrow::Cow::Owned(
            pty_output_dropped_message(*byte_count),
        )),
        TerminalEvent::PtyClosed => Some(std::borrow::Cow::Borrowed(pty_closed_message())),
        TerminalEvent::ChildExited { code } => Some(std::borrow::Cow::Owned(exit_message(*code))),
        TerminalEvent::StdinWriteFailed(failure) => Some(std::borrow::Cow::Owned(
            stdin_write_failure_message(failure),
        )),
        TerminalEvent::Failure(failure) => Some(std::borrow::Cow::Owned(failure_message(failure))),
    }
}

fn pty_closed_message() -> &'static str {
    "\r\n[pty closed]\r\n"
}

fn pty_output_dropped_message(byte_count: usize) -> String {
    // CAN aborts a partial ANSI escape/control sequence before the visible marker.
    format!("\x18\r\n[pty output dropped: {byte_count} bytes]\r\n")
}

fn exit_message(code: Option<u32>) -> String {
    match code {
        Some(code) => format!("\r\n[process exited with code {code}]\r\n"),
        None => "\r\n[process exited]\r\n".to_owned(),
    }
}

fn stdin_write_failure_message(failure: &TerminalFailure) -> String {
    format!("\r\n[pty stdin error: {}]\r\n", failure.user_message)
}

fn failure_message(failure: &TerminalFailure) -> String {
    format!("\r\n[pty error: {}]\r\n", failure.user_message)
}

fn app_error_message(user_message: &str) -> String {
    format!("\r\n[app error: {user_message}]\r\n")
}

fn app_status_message(user_message: &str) -> String {
    format!("\r\n[app: {user_message}]\r\n")
}

pub(crate) fn startup_window_size_message(width: i32, height: i32) -> String {
    let width = width.max(1);
    let height = height.max(1);
    format!("Startup window client size: {width}x{height} px")
}

#[cfg(test)]
mod tests {
    use crate::domain::{
        CommandText, CursorPosition, TerminalCell, TerminalKey, TerminalKeyModifiers,
        TerminalScroll, TerminalTabId, TerminalViewport, terminal_input_from_char,
        terminal_input_from_key,
    };

    use super::*;

    #[derive(Default)]
    struct RecordingBackend {
        started_with: Vec<TerminalSize>,
        started_directories: Vec<Option<StartupDirectory>>,
        resizes: Vec<TerminalSize>,
        resize_error: Option<&'static str>,
        write_error: Option<&'static str>,
        events: Vec<TerminalEvent>,
        shutdowns: usize,
        shutdown_pending: bool,
        writes: Vec<Vec<u8>>,
        drain_calls: usize,
    }

    impl PtyBackend for RecordingBackend {
        fn start(
            &mut self,
            size: TerminalSize,
            startup_directory: Option<&StartupDirectory>,
        ) -> AppResult<()> {
            self.started_with.push(size);
            self.started_directories.push(startup_directory.cloned());
            Ok(())
        }

        fn write_input(&mut self, bytes: Vec<u8>) -> AppResult<()> {
            if let Some(message) = self.write_error {
                return Err(AppError::InvalidState(message));
            }

            self.writes.push(bytes);
            Ok(())
        }

        fn resize(&mut self, size: TerminalSize) -> AppResult<()> {
            self.resizes.push(size);
            match self.resize_error {
                Some(message) => Err(AppError::pty_message("resize recording backend", message)),
                None => Ok(()),
            }
        }

        fn drain_events(&mut self) -> AppResult<Vec<TerminalEvent>> {
            self.drain_calls = self.drain_calls.saturating_add(1);
            Ok(std::mem::take(&mut self.events))
        }

        fn shutdown(&mut self) -> AppResult<()> {
            self.shutdowns = self.shutdowns.saturating_add(1);
            Ok(())
        }

        fn is_shutdown_pending(&self) -> bool {
            self.shutdown_pending
        }
    }

    #[derive(Default)]
    struct PowerShellRecordingBackend {
        inner: RecordingBackend,
    }

    impl PtyBackend for PowerShellRecordingBackend {
        fn start(
            &mut self,
            size: TerminalSize,
            startup_directory: Option<&StartupDirectory>,
        ) -> AppResult<()> {
            self.inner.start(size, startup_directory)
        }

        fn write_input(&mut self, bytes: Vec<u8>) -> AppResult<()> {
            self.inner.write_input(bytes)
        }

        fn resize(&mut self, size: TerminalSize) -> AppResult<()> {
            self.inner.resize(size)
        }

        fn drain_events(&mut self) -> AppResult<Vec<TerminalEvent>> {
            self.inner.drain_events()
        }

        fn shutdown(&mut self) -> AppResult<()> {
            self.inner.shutdown()
        }

        fn is_shutdown_pending(&self) -> bool {
            self.inner.is_shutdown_pending()
        }

        fn shell_command_dialect(&self) -> ShellCommandDialect {
            ShellCommandDialect::PowerShell
        }
    }

    struct RecordingTerminal {
        viewport: TerminalViewport,
        resizes: Vec<TerminalSize>,
        resize_error: Option<&'static str>,
        output: Vec<u8>,
        pending_pty_writes: Vec<Vec<u8>>,
        scrolls: Vec<TerminalScroll>,
    }

    impl RecordingTerminal {
        fn new(size: TerminalSize) -> AppResult<Self> {
            Ok(Self {
                viewport: empty_viewport(size)?,
                resizes: Vec::new(),
                resize_error: None,
                output: Vec::new(),
                pending_pty_writes: Vec::new(),
                scrolls: Vec::new(),
            })
        }
    }

    impl TerminalViewportPort for RecordingTerminal {
        fn ingest_output(&mut self, bytes: &[u8]) -> AppResult<()> {
            self.output.extend_from_slice(bytes);
            Ok(())
        }

        fn take_pending_pty_writes(&mut self) -> AppResult<Vec<Vec<u8>>> {
            Ok(std::mem::take(&mut self.pending_pty_writes))
        }

        fn resize(&mut self, size: TerminalSize) -> AppResult<()> {
            self.resizes.push(size);
            if let Some(message) = self.resize_error {
                return Err(AppError::InvalidState(message));
            }

            self.viewport = empty_viewport(size)?;
            Ok(())
        }

        fn scroll_display(&mut self, scroll: TerminalScroll) -> AppResult<bool> {
            self.scrolls.push(scroll);
            Ok(!matches!(scroll, TerminalScroll::Lines(0)))
        }

        fn snapshot(&mut self) -> AppResult<TerminalViewport> {
            Ok(self.viewport.clone())
        }
    }

    fn recording_backend() -> RecordingBackend {
        RecordingBackend::default()
    }

    fn recording_terminal(size: TerminalSize) -> RecordingTerminal {
        match RecordingTerminal::new(size) {
            Ok(terminal) => terminal,
            Err(error) => panic!("recording terminal setup failed: {error}"),
        }
    }

    fn handle_tab_text_input<B, T>(tabs: &mut TerminalTabs<B, T>, text: &str) -> AppResult<()>
    where
        B: PtyBackend,
        T: TerminalViewportPort,
    {
        for character in text.chars() {
            tabs.handle_input(terminal_input_from_char(character))?;
        }
        Ok(())
    }

    fn handle_session_text_input<B, T>(
        session: &mut TerminalSession<B, T>,
        text: &str,
    ) -> AppResult<()>
    where
        B: PtyBackend,
        T: TerminalViewportPort,
    {
        for character in text.chars() {
            session.handle_input(terminal_input_from_char(character))?;
        }
        Ok(())
    }

    #[test]
    fn terminal_tabs_start_with_one_active_tab() -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let mut tabs = TerminalTabs::new(initial_size, recording_backend, recording_terminal);

        tabs.start()?;

        assert_eq!(tabs.tabs.len(), 1);
        assert_eq!(tabs.tabs[0].id, TerminalTabId::new(1));
        assert_eq!(
            tabs.tabs[0].session.backend.started_with,
            vec![initial_size]
        );
        assert_eq!(
            tabs.tab_views(),
            vec![crate::domain::TerminalTabView::new(
                TerminalTabId::new(1),
                "Tab 1",
                true,
            )]
        );
        Ok(())
    }

    #[test]
    fn terminal_tabs_start_passes_startup_working_directory_to_active_backend() -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let mut tabs = TerminalTabs::new(initial_size, recording_backend, recording_terminal);
        let startup_directory = StartupDirectory::new(std::path::PathBuf::from(r"C:\Windows"))?;

        tabs.start_with_startup_directory(Some(&startup_directory))?;

        assert_eq!(
            tabs.tabs[0].session.backend.started_directories,
            vec![Some(startup_directory)]
        );
        Ok(())
    }

    #[test]
    fn startup_window_size_message_reports_positive_client_size() {
        assert_eq!(
            startup_window_size_message(750, 520),
            "Startup window client size: 750x520 px"
        );
        assert_eq!(
            startup_window_size_message(0, -2),
            "Startup window client size: 1x1 px"
        );
    }

    #[test]
    fn terminal_tabs_display_status_message_writes_to_active_terminal() -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let mut tabs = TerminalTabs::new(initial_size, recording_backend, recording_terminal);
        tabs.start()?;

        tabs.display_status_message("Startup window client size: 750x520 px")?;

        assert_eq!(
            tabs.tabs[0].session.terminal.output,
            b"\r\n[app: Startup window client size: 750x520 px]\r\n"
        );
        Ok(())
    }

    #[test]
    fn open_tab_starts_new_session_and_switches_active_tab() -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let mut tabs = TerminalTabs::new(initial_size, recording_backend, recording_terminal);
        tabs.start()?;

        let opened = tabs.open_tab()?;

        assert_eq!(opened, TerminalTabId::new(2));
        assert_eq!(tabs.tabs.len(), 2);
        assert_eq!(
            tabs.tabs[1].session.backend.started_with,
            vec![initial_size]
        );
        assert!(tabs.is_active_tab(TerminalTabId::new(2)));
        Ok(())
    }

    #[test]
    fn terminal_tabs_route_input_to_active_tab() -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let mut tabs = TerminalTabs::new(initial_size, recording_backend, recording_terminal);
        tabs.start()?;
        tabs.open_tab()?;

        handle_tab_text_input(&mut tabs, "active")?;
        tabs.switch_to_tab(TerminalTabId::new(1))?;
        handle_tab_text_input(&mut tabs, "first")?;

        assert_eq!(tabs.tabs[0].session.backend.writes.concat(), b"first");
        assert_eq!(tabs.tabs[1].session.backend.writes.concat(), b"active");
        Ok(())
    }

    #[test]
    fn terminal_tabs_scroll_active_terminal_display() -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let mut tabs = TerminalTabs::new(initial_size, recording_backend, recording_terminal);
        tabs.start()?;
        tabs.open_tab()?;

        assert!(tabs.scroll_terminal_display(TerminalScroll::Lines(3))?);

        assert!(tabs.tabs[0].session.terminal.scrolls.is_empty());
        assert_eq!(
            tabs.tabs[1].session.terminal.scrolls,
            vec![TerminalScroll::Lines(3)]
        );
        Ok(())
    }

    #[test]
    fn closing_active_tab_shutdowns_session_and_activates_neighbor() -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let mut tabs = TerminalTabs::new(initial_size, recording_backend, recording_terminal);
        tabs.start()?;
        tabs.open_tab()?;

        tabs.close_tab(TerminalTabId::new(2))?;

        assert_eq!(tabs.tabs.len(), 1);
        assert_eq!(tabs.tabs[0].id, TerminalTabId::new(1));
        assert!(tabs.is_active_tab(TerminalTabId::new(1)));
        Ok(())
    }

    #[test]
    fn closing_tab_removes_tab_while_shutdown_cleanup_is_pending() -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let mut tabs = TerminalTabs::new(initial_size, recording_backend, recording_terminal);
        tabs.start()?;
        let closing_id = tabs.open_tab()?;
        tabs.tabs[1].session.backend.shutdown_pending = true;

        tabs.close_tab(closing_id)?;

        assert_eq!(tabs.tabs.len(), 1);
        assert_eq!(tabs.tabs[0].id, TerminalTabId::new(1));
        assert!(tabs.is_active_tab(TerminalTabId::new(1)));
        Ok(())
    }

    #[test]
    fn closing_active_tab_exposes_neighbor_terminal_viewport() -> AppResult<()> {
        let initial_size = TerminalSize::new(2, 8)?;
        let mut tabs = TerminalTabs::new(initial_size, recording_backend, recording_terminal);
        tabs.start()?;
        tabs.tabs[0].session.terminal.viewport = viewport_with_text(initial_size, "one")?;
        let closing_id = tabs.open_tab()?;
        tabs.tabs[1].session.terminal.viewport = viewport_with_text(initial_size, "two")?;

        tabs.close_tab(closing_id)?;

        let viewport = tabs.terminal_viewport()?;
        let line = viewport.visible_line(0).ok_or(AppError::InvalidState(
            "active terminal viewport row is missing",
        ))?;
        assert_eq!(line.text(), "one");
        Ok(())
    }

    #[test]
    fn terminal_tabs_resize_updates_active_session_and_defers_inactive_until_switch()
    -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let mut tabs = TerminalTabs::new(initial_size, recording_backend, recording_terminal);
        tabs.start()?;
        tabs.open_tab()?;

        let resized = TerminalSize::with_pixels(30, 100, 800, 480)?;
        tabs.execute(TerminalCommand::Resize(resized))?;

        assert!(tabs.tabs[0].session.backend.resizes.is_empty());
        assert!(tabs.tabs[0].session.terminal.resizes.is_empty());
        assert_eq!(tabs.tabs[0].session.size, initial_size);
        assert_eq!(tabs.tabs[1].session.backend.resizes, vec![resized]);
        assert_eq!(tabs.tabs[1].session.terminal.resizes, vec![resized]);
        assert_eq!(tabs.size, resized);

        tabs.switch_to_tab(TerminalTabId::new(1))?;

        assert_eq!(tabs.tabs[0].session.backend.resizes, vec![resized]);
        assert_eq!(tabs.tabs[0].session.terminal.resizes, vec![resized]);
        assert_eq!(tabs.tabs[0].session.size, resized);
        assert!(tabs.is_active_tab(TerminalTabId::new(1)));
        Ok(())
    }

    #[test]
    fn terminal_tabs_resize_coalesces_deferred_inactive_session_resize() -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let mut tabs = TerminalTabs::new(initial_size, recording_backend, recording_terminal);
        tabs.start()?;
        tabs.open_tab()?;

        let first_resize = TerminalSize::with_pixels(30, 100, 800, 480)?;
        let second_resize = TerminalSize::with_pixels(36, 120, 960, 576)?;
        tabs.execute(TerminalCommand::Resize(first_resize))?;
        tabs.execute(TerminalCommand::Resize(second_resize))?;

        assert!(tabs.tabs[0].session.backend.resizes.is_empty());
        assert!(tabs.tabs[0].session.terminal.resizes.is_empty());
        assert_eq!(tabs.tabs[0].session.size, initial_size);
        assert_eq!(
            tabs.tabs[1].session.backend.resizes,
            vec![first_resize, second_resize]
        );
        assert_eq!(
            tabs.tabs[1].session.terminal.resizes,
            vec![first_resize, second_resize]
        );

        tabs.switch_to_tab(TerminalTabId::new(1))?;

        assert_eq!(tabs.tabs[0].session.backend.resizes, vec![second_resize]);
        assert_eq!(tabs.tabs[0].session.terminal.resizes, vec![second_resize]);
        assert_eq!(tabs.tabs[0].session.size, second_resize);
        assert_eq!(tabs.size, second_resize);
        Ok(())
    }

    #[test]
    fn terminal_tabs_resize_same_grid_pixel_change_updates_active_session() -> AppResult<()> {
        let initial_size = TerminalSize::with_pixels(24, 80, 640, 384)?;
        let mut tabs = TerminalTabs::new(initial_size, recording_backend, recording_terminal);
        tabs.start()?;
        tabs.open_tab()?;

        let resized = TerminalSize::with_pixels(24, 80, 800, 384)?;
        tabs.execute(TerminalCommand::Resize(resized))?;

        assert!(tabs.tabs[0].session.backend.resizes.is_empty());
        assert!(tabs.tabs[0].session.terminal.resizes.is_empty());
        assert_eq!(tabs.tabs[0].session.size, initial_size);
        assert_eq!(tabs.tabs[1].session.backend.resizes, vec![resized]);
        assert_eq!(tabs.tabs[1].session.terminal.resizes, vec![resized]);
        assert_eq!(tabs.tabs[1].session.size, resized);
        assert_eq!(tabs.size, resized);

        tabs.switch_to_tab(TerminalTabId::new(1))?;
        tabs.terminal_viewport()?;

        assert_eq!(tabs.tabs[0].session.size, resized);
        assert_eq!(tabs.tabs[0].session.backend.resizes, vec![resized]);
        assert_eq!(tabs.tabs[0].session.terminal.resizes, vec![resized]);
        Ok(())
    }

    #[test]
    fn terminal_tabs_resize_active_failure_keeps_shared_size_and_inactive_tabs_untouched()
    -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let mut tabs = TerminalTabs::new(initial_size, recording_backend, recording_terminal);
        tabs.start()?;
        tabs.open_tab()?;
        tabs.open_tab()?;
        tabs.tabs[2].session.backend.resize_error = Some("backend resize failed");

        let resized = TerminalSize::with_pixels(30, 100, 800, 480)?;
        let result = tabs.execute(TerminalCommand::Resize(resized));

        assert!(matches!(
            result,
            Err(AppError::Pty(context)) if context.cause() == "backend resize failed"
        ));
        assert_eq!(tabs.size, initial_size);
        assert_eq!(tabs.tabs[0].session.size, initial_size);
        assert!(tabs.tabs[0].session.backend.resizes.is_empty());
        assert!(tabs.tabs[0].session.terminal.resizes.is_empty());
        assert_eq!(tabs.tabs[1].session.size, initial_size);
        assert!(tabs.tabs[1].session.backend.resizes.is_empty());
        assert!(tabs.tabs[1].session.terminal.resizes.is_empty());
        assert_eq!(tabs.tabs[2].session.size, initial_size);
        assert_eq!(tabs.tabs[2].session.backend.resizes, vec![resized]);
        assert!(tabs.tabs[2].session.terminal.resizes.is_empty());

        let opened = tabs.open_tab()?;

        assert_eq!(opened, TerminalTabId::new(4));
        assert_eq!(tabs.tabs[3].session.size, initial_size);
        assert_eq!(
            tabs.tabs[3].session.backend.started_with,
            vec![initial_size]
        );
        Ok(())
    }

    #[test]
    fn terminal_tabs_timer_drain_prioritizes_active_and_drains_multiple_background_tabs()
    -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let mut tabs = TerminalTabs::new(initial_size, recording_backend, recording_terminal);
        tabs.start()?;
        tabs.open_tab()?;
        tabs.open_tab()?;
        tabs.tabs[0]
            .session
            .backend
            .events
            .push(TerminalEvent::PtyOutput(b"one".to_vec()));
        tabs.tabs[1]
            .session
            .backend
            .events
            .push(TerminalEvent::PtyOutput(b"two".to_vec()));
        tabs.tabs[2]
            .session
            .backend
            .events
            .push(TerminalEvent::PtyOutput(b"three".to_vec()));

        let first_drain = tabs.drain_timer_events()?;

        assert!(first_drain.had_events);
        assert!(first_drain.active_tab_dirty);
        assert_eq!(first_drain.failure_cause, None);
        assert_eq!(tabs.tabs[0].session.backend.drain_calls, 1);
        assert_eq!(tabs.tabs[1].session.backend.drain_calls, 1);
        assert_eq!(tabs.tabs[2].session.backend.drain_calls, 1);

        let second_drain = tabs.drain_timer_events()?;

        assert!(!second_drain.had_events);
        assert!(!second_drain.active_tab_dirty);
        assert_eq!(second_drain.failure_cause, None);
        assert_eq!(tabs.tabs[0].session.backend.drain_calls, 2);
        assert_eq!(tabs.tabs[1].session.backend.drain_calls, 2);
        assert_eq!(tabs.tabs[2].session.backend.drain_calls, 2);
        Ok(())
    }

    #[test]
    fn terminal_tabs_timer_drain_summarizes_pty_output_without_returning_bytes() -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let mut tabs = TerminalTabs::new(initial_size, recording_backend, recording_terminal);
        tabs.start()?;
        tabs.tabs[0]
            .session
            .backend
            .events
            .push(TerminalEvent::PtyOutput(b"large-output".to_vec()));

        let drain = tabs.drain_timer_events()?;

        assert_eq!(
            drain,
            TerminalTimerDrain {
                had_events: true,
                active_tab_dirty: true,
                needs_active_poll: false,
                failure_cause: None,
            }
        );
        assert_eq!(
            tabs.tabs[0].session.terminal.output.as_slice(),
            b"large-output"
        );
        Ok(())
    }

    #[test]
    fn terminal_tabs_timer_drain_reports_failure_cause_and_active_dirty() -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let mut tabs = TerminalTabs::new(initial_size, recording_backend, recording_terminal);
        tabs.start()?;
        tabs.open_tab()?;
        tabs.tabs[0]
            .session
            .backend
            .events
            .push(TerminalEvent::Failure(TerminalFailure::new(
                "background failed",
                "background cause",
            )));

        let background_drain = tabs.drain_timer_events()?;

        assert!(background_drain.had_events);
        assert!(!background_drain.active_tab_dirty);
        assert_eq!(
            background_drain.failure_cause.as_deref(),
            Some("background cause")
        );

        tabs.tabs[1]
            .session
            .backend
            .events
            .push(TerminalEvent::Failure(TerminalFailure::new(
                "active failed",
                "active cause",
            )));

        let active_drain = tabs.drain_timer_events()?;

        assert!(active_drain.had_events);
        assert!(active_drain.active_tab_dirty);
        assert_eq!(active_drain.failure_cause.as_deref(), Some("active cause"));
        Ok(())
    }

    #[test]
    fn resize_running_session_updates_backend_and_terminal_viewport() -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let mut session = TerminalSession::new(
            RecordingBackend::default(),
            RecordingTerminal::new(initial_size)?,
            initial_size,
        );
        session.start()?;

        let resized = TerminalSize::with_pixels(30, 100, 800, 480)?;
        session.resize(resized)?;

        assert_eq!(session.backend.resizes, vec![resized]);
        assert_eq!(session.terminal.resizes, vec![resized]);
        assert_eq!(session.size, resized);
        Ok(())
    }

    #[test]
    fn resize_running_session_same_grid_pixel_change_updates_backend_and_terminal() -> AppResult<()>
    {
        let initial_size = TerminalSize::with_pixels(24, 80, 640, 384)?;
        let mut session = TerminalSession::new(
            RecordingBackend::default(),
            RecordingTerminal::new(initial_size)?,
            initial_size,
        );
        session.start()?;

        let resized = TerminalSize::with_pixels(24, 80, 800, 384)?;
        session.resize(resized)?;

        assert_eq!(session.backend.resizes, vec![resized]);
        assert_eq!(session.terminal.resizes, vec![resized]);
        assert_eq!(session.size, resized);
        Ok(())
    }

    #[test]
    fn resize_running_session_backend_failure_keeps_terminal_and_size_unchanged() -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let backend = RecordingBackend {
            resize_error: Some("backend resize failed"),
            ..RecordingBackend::default()
        };
        let mut session =
            TerminalSession::new(backend, RecordingTerminal::new(initial_size)?, initial_size);
        session.start()?;

        let resized = TerminalSize::with_pixels(30, 100, 800, 480)?;
        let result = session.resize(resized);

        assert!(matches!(
            result,
            Err(AppError::Pty(context)) if context.cause() == "backend resize failed"
        ));
        assert_eq!(session.backend.resizes, vec![resized]);
        assert!(session.terminal.resizes.is_empty());
        assert_eq!(session.size, initial_size);
        Ok(())
    }

    #[test]
    fn resize_terminal_failure_keeps_session_size_in_sync_with_backend() -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let mut terminal = RecordingTerminal::new(initial_size)?;
        terminal.resize_error = Some("terminal resize failed");
        let mut session = TerminalSession::new(RecordingBackend::default(), terminal, initial_size);
        session.start()?;

        let resized = TerminalSize::with_pixels(30, 100, 800, 480)?;
        let result = session.resize(resized);

        assert!(matches!(
            result,
            Err(AppError::InvalidState(message)) if message == "terminal resize failed"
        ));
        assert_eq!(session.backend.resizes, vec![resized]);
        assert_eq!(session.terminal.resizes, vec![resized]);
        assert_eq!(session.size, resized);
        Ok(())
    }

    #[test]
    fn resize_before_start_updates_terminal_viewport_only() -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let mut session = TerminalSession::new(
            RecordingBackend::default(),
            RecordingTerminal::new(initial_size)?,
            initial_size,
        );

        let resized = TerminalSize::with_pixels(12, 40, 320, 192)?;
        session.resize(resized)?;

        assert!(session.backend.resizes.is_empty());
        assert_eq!(session.terminal.resizes, vec![resized]);
        assert_eq!(session.size, resized);
        Ok(())
    }

    #[test]
    fn drain_events_writes_terminal_responses_back_to_pty() -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let mut terminal = RecordingTerminal::new(initial_size)?;
        terminal.pending_pty_writes.push(b"\x1b[1;1R".to_vec());
        let mut backend = RecordingBackend::default();
        backend
            .events
            .push(TerminalEvent::PtyOutput(b"\x1b[6n".to_vec()));
        let mut session = TerminalSession::new(backend, terminal, initial_size);
        session.start()?;

        session.drain_events()?;

        assert_eq!(session.backend.writes, vec![b"\x1b[1;1R".to_vec()]);
        Ok(())
    }

    #[test]
    fn drain_events_marks_dropped_pty_output_before_retained_bytes() -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let mut backend = RecordingBackend::default();
        backend.events.extend([
            TerminalEvent::PtyOutputDropped { byte_count: 42 },
            TerminalEvent::PtyOutput(b"tail".to_vec()),
        ]);
        let mut session =
            TerminalSession::new(backend, RecordingTerminal::new(initial_size)?, initial_size);
        session.start()?;

        let events = session.drain_events()?;

        assert!(matches!(
            events.first(),
            Some(TerminalEvent::PtyOutputDropped { byte_count: 42 })
        ));
        assert!(
            session
                .terminal
                .output
                .starts_with(b"\x18\r\n[pty output dropped: 42 bytes]\r\n")
        );
        assert!(session.terminal.output.ends_with(b"tail"));
        assert_eq!(session.status, SessionStatus::Running);
        Ok(())
    }

    #[test]
    fn drain_events_limits_pty_output_ingest_per_call_and_preserves_event_order() -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let large_output = vec![b'x'; MAX_TERMINAL_INGEST_BYTES_PER_DRAIN + 3];
        let mut backend = RecordingBackend::default();
        backend.events.extend([
            TerminalEvent::PtyOutput(large_output.clone()),
            TerminalEvent::PtyClosed,
        ]);
        let mut session =
            TerminalSession::new(backend, RecordingTerminal::new(initial_size)?, initial_size);
        session.start()?;

        let first_events = session.drain_events()?;

        assert!(matches!(
            first_events.as_slice(),
            [TerminalEvent::PtyOutput(_)]
        ));
        assert_eq!(
            session.terminal.output.len(),
            MAX_TERMINAL_INGEST_BYTES_PER_DRAIN
        );
        assert_eq!(session.status, SessionStatus::Running);
        assert_eq!(session.backend.drain_calls, 1);

        let second_events = session.drain_events()?;

        assert!(matches!(
            second_events.as_slice(),
            [TerminalEvent::PtyOutput(_), TerminalEvent::PtyClosed]
        ));
        assert_eq!(session.backend.drain_calls, 1);
        assert!(session.terminal.output.starts_with(large_output.as_slice()));
        assert!(String::from_utf8_lossy(&session.terminal.output).ends_with("[pty closed]\r\n"));
        assert_eq!(session.status, SessionStatus::Exited);
        Ok(())
    }

    #[test]
    fn drain_events_retains_large_pty_output_buffer_when_budget_splits() -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let large_output = vec![b'x'; MAX_TERMINAL_INGEST_BYTES_PER_DRAIN + 3];
        let output_ptr = large_output.as_ptr();
        let mut backend = RecordingBackend::default();
        backend.events.push(TerminalEvent::PtyOutput(large_output));
        let mut session =
            TerminalSession::new(backend, RecordingTerminal::new(initial_size)?, initial_size);
        session.start()?;

        let first_events = session.drain_events()?;

        assert!(matches!(
            first_events.as_slice(),
            [TerminalEvent::PtyOutput(_)]
        ));
        assert!(matches!(
            session.pending_events.front(),
            Some(PendingTerminalEvent::PtyOutput(output))
                if output.bytes.as_ptr() == output_ptr
                    && output.start == MAX_TERMINAL_INGEST_BYTES_PER_DRAIN
                    && output.remaining_len() == 3
        ));

        let second_events = session.drain_events()?;

        assert!(matches!(
            second_events.as_slice(),
            [TerminalEvent::PtyOutput(_)]
        ));
        assert!(session.pending_events.is_empty());
        assert_eq!(
            session.terminal.output.len(),
            MAX_TERMINAL_INGEST_BYTES_PER_DRAIN + 3
        );
        Ok(())
    }

    #[test]
    fn drain_events_keeps_lifecycle_events_after_terminal_response_write_error() -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let mut terminal = RecordingTerminal::new(initial_size)?;
        terminal.pending_pty_writes.push(b"\x1b[1;1R".to_vec());
        let mut backend = RecordingBackend {
            write_error: Some("pty writer worker is not available"),
            ..RecordingBackend::default()
        };
        backend.events.extend([
            TerminalEvent::PtyOutput(b"\x1b[6n".to_vec()),
            TerminalEvent::PtyClosed,
            TerminalEvent::ChildExited { code: Some(0) },
        ]);
        let mut session = TerminalSession::new(backend, terminal, initial_size);
        session.start()?;

        let result = session.drain_events();

        assert!(matches!(
            result,
            Err(AppError::InvalidState(message))
                if message == "pty writer worker is not available"
        ));
        let output = String::from_utf8_lossy(&session.terminal.output);
        assert!(output.contains("[pty closed]"));
        assert!(output.contains("[process exited with code 0]"));
        assert_eq!(session.status, SessionStatus::Exited);
        assert!(session.backend.writes.is_empty());
        Ok(())
    }

    #[test]
    fn run_button_command_writes_predefined_command_to_pty() -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let mut session = TerminalSession::new(
            RecordingBackend::default(),
            RecordingTerminal::new(initial_size)?,
            initial_size,
        );
        session.start()?;

        session.run_command_text(&CommandText::from_static("echo hello"))?;

        assert_eq!(session.backend.writes, vec![b"echo hello\r".to_vec()]);
        assert_eq!(session.status, SessionStatus::Running);
        Ok(())
    }

    #[test]
    fn run_dir_button_command_writes_dir_to_pty() -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let mut session = TerminalSession::new(
            RecordingBackend::default(),
            RecordingTerminal::new(initial_size)?,
            initial_size,
        );
        session.start()?;

        session.run_command_text(&CommandText::from_static("dir"))?;

        assert_eq!(session.backend.writes, vec![b"dir\r".to_vec()]);
        assert_eq!(session.status, SessionStatus::Running);
        Ok(())
    }

    #[test]
    fn run_startup_command_writes_argument_command_to_pty() -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let mut session = TerminalSession::new(
            RecordingBackend::default(),
            RecordingTerminal::new(initial_size)?,
            initial_size,
        );
        let command = StartupCommand::from_arguments(vec![
            r"C:\Tools\hello script.bat".to_owned(),
            "world".to_owned(),
        ])?
        .ok_or(AppError::InvalidInput("startup command should exist"))?;
        session.start()?;

        session.run_startup_command(&command)?;

        assert_eq!(
            session.backend.writes,
            vec![b"\"C:\\Tools\\hello script.bat\" world\r".to_vec()]
        );
        assert_eq!(session.status, SessionStatus::Running);
        Ok(())
    }

    #[test]
    fn run_startup_command_uses_backend_shell_dialect() -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let mut session = TerminalSession::new(
            PowerShellRecordingBackend::default(),
            RecordingTerminal::new(initial_size)?,
            initial_size,
        );
        let command = StartupCommand::from_arguments(vec![
            r"C:\Tools\hello script.ps1".to_owned(),
            "world".to_owned(),
        ])?
        .ok_or(AppError::InvalidInput("startup command should exist"))?;
        session.start()?;

        session.run_startup_command(&command)?;

        assert_eq!(
            session.backend.inner.writes,
            vec![b"& 'C:\\Tools\\hello script.ps1' 'world'\r".to_vec()]
        );
        assert_eq!(session.status, SessionStatus::Running);
        Ok(())
    }

    #[test]
    fn button_command_keeps_direct_terminal_input_available() -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let mut session = TerminalSession::new(
            RecordingBackend::default(),
            RecordingTerminal::new(initial_size)?,
            initial_size,
        );
        session.start()?;

        session.run_command_text(&CommandText::from_static("echo hello"))?;
        handle_session_text_input(&mut session, "echo manual")?;
        session.handle_input(terminal_input_from_key(
            TerminalKey::Enter,
            TerminalKeyModifiers::default(),
        ))?;

        assert_eq!(session.backend.writes[0], b"echo hello\r");
        assert_eq!(session.backend.writes[1..].concat(), b"echo manual\r");
        assert_eq!(session.status, SessionStatus::Running);
        Ok(())
    }

    #[test]
    fn handle_input_writes_terminal_input_bytes_to_pty() -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let mut session = TerminalSession::new(
            RecordingBackend::default(),
            RecordingTerminal::new(initial_size)?,
            initial_size,
        );
        session.start()?;

        handle_session_text_input(&mut session, "echo hello")?;
        session.handle_input(terminal_input_from_key(
            TerminalKey::Tab,
            TerminalKeyModifiers::default(),
        ))?;
        session.handle_input(terminal_input_from_key(
            TerminalKey::ArrowLeft,
            TerminalKeyModifiers::default(),
        ))?;
        session.handle_input(terminal_input_from_char('\u{03}'))?;

        assert_eq!(session.backend.writes.concat(), b"echo hello\t\x1b[D\x03");
        Ok(())
    }

    #[test]
    fn paste_text_writes_normalized_clipboard_text_to_pty() -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let mut session = TerminalSession::new(
            RecordingBackend::default(),
            RecordingTerminal::new(initial_size)?,
            initial_size,
        );
        session.start()?;

        session.paste_text("echo one\r\necho two\n")?;

        assert_eq!(
            session.backend.writes,
            vec![b"echo one\recho two\r".to_vec()]
        );
        Ok(())
    }

    #[test]
    fn drain_events_distinguishes_stdin_pty_and_child_lifecycle() -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let mut backend = RecordingBackend::default();
        backend.events.extend([
            TerminalEvent::StdinWriteFailed(TerminalFailure::new(
                "terminal input stream failed",
                "write to pty stdin failed: broken pipe",
            )),
            TerminalEvent::PtyClosed,
            TerminalEvent::ChildExited { code: Some(0) },
        ]);
        let mut session =
            TerminalSession::new(backend, RecordingTerminal::new(initial_size)?, initial_size);
        session.start()?;

        let events = session.drain_events()?;

        assert!(matches!(events[0], TerminalEvent::StdinWriteFailed(_)));
        assert!(matches!(events[1], TerminalEvent::PtyClosed));
        assert!(matches!(
            events[2],
            TerminalEvent::ChildExited { code: Some(0) }
        ));
        let output = String::from_utf8_lossy(&session.terminal.output);
        assert!(output.contains("[pty stdin error: terminal input stream failed]"));
        assert!(output.contains("[pty closed]"));
        assert!(output.contains("[process exited with code 0]"));
        assert_eq!(session.status, SessionStatus::Failed);
        Ok(())
    }

    #[test]
    fn pty_closed_without_child_exit_stops_session_input() -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let mut backend = RecordingBackend::default();
        backend.events.push(TerminalEvent::PtyClosed);
        let mut session =
            TerminalSession::new(backend, RecordingTerminal::new(initial_size)?, initial_size);
        session.start()?;

        let events = session.drain_events()?;

        assert!(matches!(events.as_slice(), [TerminalEvent::PtyClosed]));
        assert_eq!(session.status, SessionStatus::Exited);
        let output = String::from_utf8_lossy(&session.terminal.output);
        assert!(output.contains("[pty closed]"));

        let result = handle_session_text_input(&mut session, "still-running");

        assert!(matches!(
            result,
            Err(AppError::InvalidState(message)) if message == "terminal session is not running"
        ));
        assert!(session.backend.writes.is_empty());
        Ok(())
    }

    #[test]
    fn shutdown_is_idempotent_and_marks_session_exited() -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let mut session = TerminalSession::new(
            RecordingBackend::default(),
            RecordingTerminal::new(initial_size)?,
            initial_size,
        );
        session.start()?;

        session.shutdown()?;
        session.shutdown()?;

        assert_eq!(session.status, SessionStatus::Exited);
        assert_eq!(session.backend.shutdowns, 1);
        Ok(())
    }

    #[test]
    fn shutdown_keeps_session_pending_until_backend_cleanup_finishes() -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let backend = RecordingBackend {
            shutdown_pending: true,
            ..RecordingBackend::default()
        };
        let mut session =
            TerminalSession::new(backend, RecordingTerminal::new(initial_size)?, initial_size);
        session.start()?;

        session.shutdown()?;
        session.shutdown()?;

        assert_eq!(session.status, SessionStatus::ShuttingDown);
        assert_eq!(session.backend.shutdowns, 1);

        session.backend.shutdown_pending = false;
        let events = session.drain_events()?;

        assert!(events.is_empty());
        assert_eq!(session.status, SessionStatus::Exited);
        Ok(())
    }

    #[test]
    fn shutdown_cleanup_failure_keeps_session_failed() -> AppResult<()> {
        let initial_size = TerminalSize::new(24, 80)?;
        let backend = RecordingBackend {
            shutdown_pending: true,
            ..RecordingBackend::default()
        };
        let mut session =
            TerminalSession::new(backend, RecordingTerminal::new(initial_size)?, initial_size);
        session.start()?;

        session.shutdown()?;
        session
            .backend
            .events
            .push(TerminalEvent::Failure(TerminalFailure::new(
                "terminal shutdown failed",
                "cleanup failed",
            )));
        session.backend.shutdown_pending = false;
        let events = session.drain_events()?;

        assert!(matches!(events.as_slice(), [TerminalEvent::Failure(_)]));
        assert_eq!(session.status, SessionStatus::Failed);
        Ok(())
    }

    fn empty_viewport(size: TerminalSize) -> AppResult<TerminalViewport> {
        let rows = usize::from(size.rows);
        let columns = usize::from(size.columns);
        let cells = vec![TerminalCell::default(); rows.saturating_mul(columns)];
        TerminalViewport::new(rows, columns, cells, CursorPosition::new(0, 0))
    }

    fn viewport_with_text(size: TerminalSize, text: &str) -> AppResult<TerminalViewport> {
        let rows = usize::from(size.rows);
        let columns = usize::from(size.columns);
        let mut cells = vec![TerminalCell::default(); rows.saturating_mul(columns)];
        for (index, character) in text.chars().take(columns).enumerate() {
            cells[index] = TerminalCell::new(character);
        }
        TerminalViewport::new(rows, columns, cells, CursorPosition::new(0, 0))
    }
}
