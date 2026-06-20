use std::io;
#[cfg(target_os = "windows")]
use std::os::windows::io::RawHandle;
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::Child;
#[cfg(target_os = "windows")]
use windows_sys::Win32::Foundation::HANDLE;
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Threading::TerminateProcess;

use crate::error::{AppError, AppResult};

#[cfg(target_os = "windows")]
use super::FORCED_CHILD_EXIT_CODE;
use super::shell::DefaultShell;
use super::worker::PtyIoWorkers;
use super::{
    CHILD_EXIT_POLL_INTERVAL, CHILD_FORCED_EXIT_TIMEOUT, CHILD_GRACEFUL_EXIT_TIMEOUT,
    DETACHED_CLEANUP_DRAIN_TIMEOUT, PtyMasterHandle,
};

pub(super) type PtyChildHandle = Box<dyn Child + Send + Sync>;

static DETACHED_CLEANUP_TASKS: OnceLock<Mutex<Vec<DetachedCleanupTask>>> = OnceLock::new();
static DETACHED_CLEANUP_FAILURES: OnceLock<Mutex<Vec<PtyCleanupFailure>>> = OnceLock::new();
const DETACHED_CLEANUP_DRAIN_TIMEOUT_OPERATION: &str = "join detached pty cleanup tasks";

pub(super) struct PtyCleanupResources {
    master: Option<PtyMasterHandle>,
    child: Option<PtyChildHandle>,
    workers: Option<PtyIoWorkers>,
}

pub(super) struct PtyCleanupFailure {
    error: AppError,
    resources: Option<Box<PtyCleanupResources>>,
}

pub(super) struct PtyCleanupTask {
    result_rx: Option<mpsc::Receiver<Result<(), PtyCleanupFailure>>>,
    thread: Option<thread::JoinHandle<()>>,
}

struct DetachedCleanupTask {
    result_rx: Option<mpsc::Receiver<Result<(), PtyCleanupFailure>>>,
    thread: Option<thread::JoinHandle<()>>,
    result: Option<Result<(), PtyCleanupFailure>>,
}

impl PtyCleanupFailure {
    fn with_resources(error: AppError, resources: PtyCleanupResources) -> Self {
        Self {
            error,
            resources: Some(Box::new(resources)),
        }
    }

    fn without_resources(error: AppError) -> Self {
        Self {
            error,
            resources: None,
        }
    }

    pub(super) fn into_parts(self) -> (Option<PtyCleanupResources>, AppError) {
        (self.resources.map(|resources| *resources), self.error)
    }
}

impl PtyCleanupTask {
    fn new(
        result_rx: mpsc::Receiver<Result<(), PtyCleanupFailure>>,
        thread: thread::JoinHandle<()>,
    ) -> Self {
        Self {
            result_rx: Some(result_rx),
            thread: Some(thread),
        }
    }

    pub(super) fn try_finish(&mut self) -> Option<Result<(), PtyCleanupFailure>> {
        let Some(result_rx) = self.result_rx.as_ref() else {
            return self
                .join_finished_thread()
                .map(|error| Err(PtyCleanupFailure::without_resources(error)));
        };

        match result_rx.try_recv() {
            Ok(result) => {
                self.result_rx = None;
                if let Some(error) = self.join_finished_thread() {
                    Some(Err(PtyCleanupFailure::without_resources(error)))
                } else {
                    Some(result)
                }
            }
            Err(TryRecvError::Empty) => {
                if let Some(error) = self.join_finished_thread() {
                    self.result_rx = None;
                    Some(Err(PtyCleanupFailure::without_resources(error)))
                } else {
                    None
                }
            }
            Err(TryRecvError::Disconnected) => {
                self.result_rx = None;
                match self.join_finished_thread() {
                    Some(error) => Some(Err(PtyCleanupFailure::without_resources(error))),
                    None if self.thread.is_none() => Some(Err(
                        PtyCleanupFailure::without_resources(AppError::InvalidState(
                            "pty cleanup task finished without reporting result",
                        )),
                    )),
                    None => None,
                }
            }
        }
    }

    fn join_finished_thread(&mut self) -> Option<AppError> {
        let is_finished = self
            .thread
            .as_ref()
            .is_some_and(thread::JoinHandle::is_finished);
        if !is_finished {
            return None;
        }

        let thread = self.thread.take()?;
        match thread.join() {
            Ok(()) => None,
            Err(_) => Some(AppError::pty_message(
                "join pty shutdown thread",
                "pty shutdown thread panicked",
            )),
        }
    }
}

impl Drop for PtyCleanupTask {
    fn drop(&mut self) {
        match (self.result_rx.take(), self.thread.take()) {
            (Some(result_rx), thread) => {
                store_detached_cleanup_task(DetachedCleanupTask::new(Some(result_rx), thread))
            }
            (None, Some(thread)) => store_detached_cleanup_thread(thread),
            (None, None) => {}
        }
    }
}

impl DetachedCleanupTask {
    fn new(
        result_rx: Option<mpsc::Receiver<Result<(), PtyCleanupFailure>>>,
        thread: Option<thread::JoinHandle<()>>,
    ) -> Self {
        Self {
            result_rx,
            thread,
            result: None,
        }
    }

    fn from_thread(thread: thread::JoinHandle<()>) -> Self {
        Self::new(None, Some(thread))
    }

    fn try_finish(&mut self) -> Option<Result<(), PtyCleanupFailure>> {
        if self.result.is_none() {
            self.poll_result();
        }

        if let Some(error) = self.join_finished_thread() {
            self.result = None;
            return Some(Err(PtyCleanupFailure::without_resources(error)));
        }

        if self.result.is_none() {
            self.poll_result();
        }

        if self.thread.is_some() {
            return None;
        }

        if let Some(result) = self.result.take() {
            return Some(result);
        }

        if self.result_rx.is_none() {
            Some(Ok(()))
        } else {
            None
        }
    }

    fn poll_result(&mut self) {
        let Some(result_rx) = self.result_rx.as_ref() else {
            return;
        };

        let result = match result_rx.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(PtyCleanupFailure::without_resources(
                AppError::InvalidState("pty cleanup task finished without reporting result"),
            ))),
        };

        if let Some(result) = result {
            self.result_rx = None;
            self.result = Some(result);
        }
    }

    fn join_finished_thread(&mut self) -> Option<AppError> {
        let is_finished = self
            .thread
            .as_ref()
            .is_some_and(thread::JoinHandle::is_finished);
        if !is_finished {
            return None;
        }

        let thread = self.thread.take()?;
        match thread.join() {
            Ok(()) => None,
            Err(_) => Some(AppError::pty_message(
                "join pty shutdown thread",
                "pty shutdown thread panicked",
            )),
        }
    }
}

impl PtyCleanupResources {
    pub(super) fn new(
        master: Option<PtyMasterHandle>,
        child: Option<PtyChildHandle>,
        workers: Option<PtyIoWorkers>,
    ) -> Self {
        Self {
            master,
            child,
            workers,
        }
    }

    pub(super) fn into_recoverable_parts(
        mut self,
    ) -> (
        Option<PtyMasterHandle>,
        Option<PtyChildHandle>,
        Option<PtyIoWorkers>,
    ) {
        let workers = self
            .workers
            .take()
            .filter(PtyIoWorkers::has_unjoined_threads);
        (self.master.take(), self.child.take(), workers)
    }

    pub(super) fn shutdown(
        mut self,
        default_shell: DefaultShell,
        request_graceful_exit: bool,
    ) -> Result<(), PtyCleanupFailure> {
        let mut shutdown_result = Ok(());
        let should_request_graceful_exit = request_graceful_exit && self.child.is_some();
        let graceful_result = if should_request_graceful_exit {
            request_child_exit(self.workers.as_ref(), &default_shell)
        } else {
            Ok(())
        };

        if let Some(workers) = self.workers.as_mut() {
            workers.close_input();
        }

        let exited_gracefully = if should_request_graceful_exit {
            match graceful_result.and_then(|()| {
                wait_for_child_exit(
                    &mut self.child,
                    CHILD_GRACEFUL_EXIT_TIMEOUT,
                    "poll graceful pty child exit",
                )
            }) {
                Ok(exited) => exited,
                Err(error) => {
                    shutdown_result = Err(error);
                    false
                }
            }
        } else {
            self.child.is_none()
        };

        if !exited_gracefully
            && let Err(error) = terminate_child(&mut self.child)
            && shutdown_result.is_ok()
        {
            shutdown_result = Err(error);
        }

        self.master = None;
        if let Some(workers) = self.workers.as_mut()
            && let Err(error) = workers.join_writer()
            && shutdown_result.is_ok()
        {
            shutdown_result = Err(error);
        }
        if let Some(workers) = self.workers.as_mut()
            && let Err(error) = workers.join_reader()
            && shutdown_result.is_ok()
        {
            shutdown_result = Err(error);
        }

        match shutdown_result {
            Ok(()) => Ok(()),
            Err(error) => Err(PtyCleanupFailure::with_resources(error, self)),
        }
    }
}

pub(super) fn spawn_shutdown_resources(
    resources: PtyCleanupResources,
    default_shell: DefaultShell,
    request_graceful_exit: bool,
) -> Result<PtyCleanupTask, PtyCleanupFailure> {
    let shared_resources = Arc::new(Mutex::new(Some(resources)));
    let cleanup_resources = Arc::clone(&shared_resources);
    let (result_tx, result_rx) = mpsc::channel();

    let cleanup_thread = thread::Builder::new()
        .name("j3term-pty-shutdown".to_owned())
        .spawn(move || {
            let result = match take_shared_cleanup_resources(&cleanup_resources) {
                Some(resources) => resources.shutdown(default_shell, request_graceful_exit),
                None => Err(PtyCleanupFailure::without_resources(
                    AppError::InvalidState("pty cleanup resources are not available"),
                )),
            };
            let _ = result_tx.send(result);
        })
        .map_err(|source| cleanup_thread_spawn_failure(&shared_resources, source))?;

    Ok(PtyCleanupTask::new(result_rx, cleanup_thread))
}

pub(super) fn join_detached_cleanup_threads() -> Vec<AppError> {
    let mut errors = recover_detached_cleanup_failures(take_detached_cleanup_failures());
    let tasks = take_detached_cleanup_tasks();
    let (drain_errors, pending_tasks) =
        drain_detached_cleanup_tasks(tasks, DETACHED_CLEANUP_DRAIN_TIMEOUT);
    errors.extend(drain_errors);
    store_detached_cleanup_tasks(pending_tasks);
    errors.extend(recover_detached_cleanup_failures(
        take_detached_cleanup_failures(),
    ));
    errors
}

#[cfg(any(test, target_os = "linux"))]
pub(super) fn is_detached_cleanup_timeout_error(error: &AppError) -> bool {
    error.operation() == Some(DETACHED_CLEANUP_DRAIN_TIMEOUT_OPERATION)
}

fn drain_detached_cleanup_tasks(
    mut tasks: Vec<DetachedCleanupTask>,
    timeout: Duration,
) -> (Vec<AppError>, Vec<DetachedCleanupTask>) {
    let mut errors = Vec::new();
    if tasks.is_empty() {
        return (errors, tasks);
    }

    let deadline = Instant::now() + timeout;
    loop {
        errors.extend(recover_detached_cleanup_failures(
            collect_finished_detached_cleanup_tasks(&mut tasks),
        ));

        if tasks.is_empty() {
            return (errors, tasks);
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            errors.push(detached_cleanup_drain_timeout_error(tasks.len(), timeout));
            return (errors, tasks);
        }

        thread::sleep(remaining.min(CHILD_EXIT_POLL_INTERVAL));
    }
}

#[cfg(test)]
fn drain_detached_cleanup_tasks_until_finished(
    mut tasks: Vec<DetachedCleanupTask>,
) -> Vec<AppError> {
    let mut errors = Vec::new();
    while !tasks.is_empty() {
        errors.extend(recover_detached_cleanup_failures(
            collect_finished_detached_cleanup_tasks(&mut tasks),
        ));

        if !tasks.is_empty() {
            thread::sleep(CHILD_EXIT_POLL_INTERVAL);
        }
    }
    errors
}

fn detached_cleanup_drain_timeout_error(task_count: usize, timeout: Duration) -> AppError {
    AppError::pty_message(
        DETACHED_CLEANUP_DRAIN_TIMEOUT_OPERATION,
        format!(
            "timed out after {} ms waiting for {task_count} detached pty cleanup task(s) to finish; cleanup continues in background",
            timeout.as_millis()
        ),
    )
}

fn recover_detached_cleanup_failure(failure: PtyCleanupFailure) -> AppError {
    let (resources, error) = failure.into_parts();
    let Some(resources) = resources else {
        return error;
    };

    match resources.shutdown(DefaultShell::detect(), false) {
        Ok(()) => error,
        Err(retry_failure) => {
            let (_, retry_error) = retry_failure.into_parts();
            AppError::pty_message(
                "cleanup detached pty resources",
                format!("{error}; retry cleanup failed: {retry_error}"),
            )
        }
    }
}

fn recover_detached_cleanup_failures(failures: Vec<PtyCleanupFailure>) -> Vec<AppError> {
    failures
        .into_iter()
        .map(recover_detached_cleanup_failure)
        .collect()
}

fn collect_finished_detached_cleanup_tasks(
    tasks: &mut Vec<DetachedCleanupTask>,
) -> Vec<PtyCleanupFailure> {
    let mut failures = Vec::new();
    let mut index = 0;
    while index < tasks.len() {
        let Some(result) = tasks[index].try_finish() else {
            index += 1;
            continue;
        };

        tasks.swap_remove(index);
        if let Err(failure) = result {
            failures.push(failure);
        }
    }
    failures
}

fn store_detached_cleanup_thread(thread: thread::JoinHandle<()>) {
    store_detached_cleanup_task(DetachedCleanupTask::from_thread(thread));
}

fn store_detached_cleanup_task(task: DetachedCleanupTask) {
    let tasks = DETACHED_CLEANUP_TASKS.get_or_init(|| Mutex::new(Vec::new()));
    let failures = match tasks.lock() {
        Ok(mut tasks) => {
            tasks.push(task);
            collect_finished_detached_cleanup_tasks(&mut tasks)
        }
        Err(poisoned) => {
            let mut tasks = poisoned.into_inner();
            tasks.push(task);
            collect_finished_detached_cleanup_tasks(&mut tasks)
        }
    };
    store_detached_cleanup_failures(failures);
}

fn store_detached_cleanup_tasks(mut pending_tasks: Vec<DetachedCleanupTask>) {
    if pending_tasks.is_empty() {
        return;
    }

    let tasks = DETACHED_CLEANUP_TASKS.get_or_init(|| Mutex::new(Vec::new()));
    let failures = match tasks.lock() {
        Ok(mut tasks) => {
            tasks.append(&mut pending_tasks);
            collect_finished_detached_cleanup_tasks(&mut tasks)
        }
        Err(poisoned) => {
            let mut tasks = poisoned.into_inner();
            tasks.append(&mut pending_tasks);
            collect_finished_detached_cleanup_tasks(&mut tasks)
        }
    };
    store_detached_cleanup_failures(failures);
}

fn store_detached_cleanup_failures(mut failures: Vec<PtyCleanupFailure>) {
    if failures.is_empty() {
        return;
    }

    let stored_failures = DETACHED_CLEANUP_FAILURES.get_or_init(|| Mutex::new(Vec::new()));
    match stored_failures.lock() {
        Ok(mut stored_failures) => stored_failures.append(&mut failures),
        Err(poisoned) => {
            let mut stored_failures = poisoned.into_inner();
            stored_failures.append(&mut failures);
        }
    }
}

fn take_detached_cleanup_tasks() -> Vec<DetachedCleanupTask> {
    let Some(tasks) = DETACHED_CLEANUP_TASKS.get() else {
        return Vec::new();
    };

    match tasks.lock() {
        Ok(mut tasks) => std::mem::take(&mut *tasks),
        Err(poisoned) => {
            let mut tasks = poisoned.into_inner();
            std::mem::take(&mut *tasks)
        }
    }
}

fn take_detached_cleanup_failures() -> Vec<PtyCleanupFailure> {
    let Some(failures) = DETACHED_CLEANUP_FAILURES.get() else {
        return Vec::new();
    };

    match failures.lock() {
        Ok(mut failures) => std::mem::take(&mut *failures),
        Err(poisoned) => {
            let mut failures = poisoned.into_inner();
            std::mem::take(&mut *failures)
        }
    }
}

pub(super) fn shutdown_resources(
    resources: PtyCleanupResources,
    default_shell: DefaultShell,
    request_graceful_exit: bool,
) -> Result<(), PtyCleanupFailure> {
    let shared_resources = Arc::new(Mutex::new(Some(resources)));
    let cleanup_resources = Arc::clone(&shared_resources);

    // Join the cleanup thread so shutdown failures reach the caller before the
    // session is marked exited.
    let cleanup_thread = thread::Builder::new()
        .name("j3term-pty-shutdown".to_owned())
        .spawn(move || {
            let Some(resources) = take_shared_cleanup_resources(&cleanup_resources) else {
                return Err(PtyCleanupFailure::without_resources(
                    AppError::InvalidState("pty cleanup resources are not available"),
                ));
            };
            resources.shutdown(default_shell, request_graceful_exit)
        })
        .map_err(|source| cleanup_thread_spawn_failure(&shared_resources, source))?;

    cleanup_thread.join().map_err(|_| {
        PtyCleanupFailure::without_resources(AppError::pty_message(
            "join pty shutdown thread",
            "pty shutdown thread panicked",
        ))
    })?
}

fn cleanup_thread_spawn_failure(
    shared_resources: &Arc<Mutex<Option<PtyCleanupResources>>>,
    source: io::Error,
) -> PtyCleanupFailure {
    let error = AppError::io("spawn pty shutdown thread", source);
    match take_shared_cleanup_resources(shared_resources) {
        Some(resources) => PtyCleanupFailure::with_resources(error, resources),
        None => PtyCleanupFailure::without_resources(error),
    }
}

fn take_shared_cleanup_resources(
    shared_resources: &Arc<Mutex<Option<PtyCleanupResources>>>,
) -> Option<PtyCleanupResources> {
    match shared_resources.lock() {
        Ok(mut resources) => resources.take(),
        Err(poisoned) => {
            let mut resources = poisoned.into_inner();
            resources.take()
        }
    }
}

pub(super) fn take_child_exit_code(
    child: &mut Option<PtyChildHandle>,
    operation: &'static str,
) -> AppResult<Option<u32>> {
    let exit_code = match child.as_mut() {
        Some(child) => poll_child_exit_code(child.as_mut(), operation)?,
        None => None,
    };

    if exit_code.is_some() {
        *child = None;
    }

    Ok(exit_code)
}

fn request_child_exit(
    workers: Option<&PtyIoWorkers>,
    default_shell: &DefaultShell,
) -> AppResult<()> {
    let Some(workers) = workers else {
        return Ok(());
    };

    workers.send_graceful_exit(default_shell.graceful_exit_sequence())
}

fn wait_for_child_exit(
    child: &mut Option<PtyChildHandle>,
    timeout: Duration,
    operation: &'static str,
) -> AppResult<bool> {
    let deadline = Instant::now() + timeout;

    loop {
        if child.is_none() || take_child_exit_code(child, operation)?.is_some() {
            return Ok(true);
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }

        thread::sleep(remaining.min(CHILD_EXIT_POLL_INTERVAL));
    }
}

fn terminate_child(child: &mut Option<PtyChildHandle>) -> AppResult<()> {
    if child.is_none() || take_child_exit_code(child, "poll pty child before terminate")?.is_some()
    {
        return Ok(());
    }

    let Some(active_child) = child.as_mut() else {
        return Ok(());
    };
    if let Err(error) = terminate_child_process(active_child.as_mut()) {
        if take_child_exit_code(child, "poll pty child after terminate failure")?.is_some() {
            return Ok(());
        }
        return Err(error);
    }

    if wait_for_child_exit(
        child,
        CHILD_FORCED_EXIT_TIMEOUT,
        "poll terminated pty child",
    )? {
        Ok(())
    } else {
        Err(AppError::pty_message(
            "wait for pty child after termination",
            "timed out waiting for pty child to exit after termination",
        ))
    }
}

pub(super) fn terminate_child_process(child: &mut (dyn Child + Send + Sync)) -> AppResult<()> {
    #[cfg(target_os = "windows")]
    match child.as_raw_handle() {
        Some(handle) => terminate_raw_process(handle),
        None => child
            .kill()
            .map_err(|source| AppError::io("terminate pty child", source)),
    }

    #[cfg(not(target_os = "windows"))]
    {
        child
            .kill()
            .map_err(|source| AppError::io("terminate pty child", source))
    }
}

fn poll_child_exit_code(
    child: &mut (dyn Child + Send + Sync),
    operation: &'static str,
) -> AppResult<Option<u32>> {
    child
        .try_wait()
        .map_err(|source| AppError::io(operation, source))
        .map(|status| status.map(|status| status.exit_code()))
}

#[cfg(target_os = "windows")]
fn terminate_raw_process(handle: RawHandle) -> AppResult<()> {
    // SAFETY: the handle comes from portable-pty's live child process handle.
    let result = unsafe { TerminateProcess(handle as HANDLE, FORCED_CHILD_EXIT_CODE) };
    if result == 0 {
        Err(AppError::win32("TerminateProcess pty child"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    #[cfg(target_os = "windows")]
    use std::os::windows::io::RawHandle;
    use std::path::PathBuf;
    use std::sync::{Arc, Condvar, Mutex};

    use portable_pty::{Child, ChildKiller, ExitStatus};

    use super::*;

    #[test]
    fn cleanup_thread_spawn_failure_preserves_resources() -> AppResult<()> {
        let resources =
            PtyCleanupResources::new(None, Some(Box::new(ExitReadyChild) as PtyChildHandle), None);
        let shared_resources = Arc::new(Mutex::new(Some(resources)));

        let failure = cleanup_thread_spawn_failure(
            &shared_resources,
            io::Error::other("thread spawn refused"),
        );
        let (resources, error) = failure.into_parts();

        assert!(matches!(
            error,
            AppError::Io {
                operation: "spawn pty shutdown thread",
                ..
            }
        ));
        let Some(resources) = resources else {
            return Err(AppError::pty_message(
                "assert cleanup thread spawn failure resources",
                "cleanup thread spawn failure dropped resources",
            ));
        };
        assert!(resources.child.is_some());
        assert!(take_shared_cleanup_resources(&shared_resources).is_none());
        Ok(())
    }

    #[test]
    fn dropped_pending_cleanup_task_preserves_failure_for_detached_join() -> AppResult<()> {
        let child = BlockingKillFailureChild::new();
        let state = Arc::clone(&child.state);
        let task = match spawn_shutdown_resources(
            PtyCleanupResources::new(None, Some(Box::new(child)), None),
            DefaultShell::CommandPrompt(PathBuf::from("cmd.exe")),
            false,
        ) {
            Ok(task) => task,
            Err(failure) => {
                let (_, error) = failure.into_parts();
                return Err(error);
            }
        };

        wait_for_first_kill(&state)?;
        drop(task);
        release_first_kill(&state)?;

        let errors = join_detached_cleanup_threads();

        assert!(errors.iter().any(|error| {
            error.operation() == Some("cleanup detached pty resources")
                && error.to_string().contains("terminate pty child")
        }));
        assert_eq!(kill_calls(&state)?, 2);
        Ok(())
    }

    #[test]
    fn detached_cleanup_drain_times_out_pending_task() -> AppResult<()> {
        let child = BlockingKillFailureChild::new();
        let state = Arc::clone(&child.state);
        let mut task = match spawn_shutdown_resources(
            PtyCleanupResources::new(None, Some(Box::new(child)), None),
            DefaultShell::CommandPrompt(PathBuf::from("cmd.exe")),
            false,
        ) {
            Ok(task) => task,
            Err(failure) => {
                let (_, error) = failure.into_parts();
                return Err(error);
            }
        };

        wait_for_first_kill(&state)?;
        let detached_task = DetachedCleanupTask::new(task.result_rx.take(), task.thread.take());
        drop(task);

        let started = Instant::now();
        let (errors, pending_tasks) =
            drain_detached_cleanup_tasks(vec![detached_task], Duration::from_millis(10));
        let elapsed = started.elapsed();
        let timeout_reported = errors
            .iter()
            .any(|error| error.operation() == Some("join detached pty cleanup tasks"));
        let pending_count = pending_tasks.len();
        let kill_calls_before_release = kill_calls(&state);

        release_first_kill(&state)?;
        let retry_errors = drain_detached_cleanup_tasks_until_finished(pending_tasks);

        assert!(elapsed < Duration::from_millis(500));
        assert!(timeout_reported);
        assert_eq!(pending_count, 1);
        assert_eq!(kill_calls_before_release?, 1);
        assert!(!retry_errors.iter().any(is_detached_cleanup_timeout_error));
        assert!(retry_errors.iter().any(|error| {
            error.operation() == Some("cleanup detached pty resources")
                && error.to_string().contains("terminate pty child")
        }));
        assert_eq!(kill_calls(&state)?, 2);
        Ok(())
    }

    #[test]
    fn finished_detached_cleanup_task_is_collected_without_shutdown_drain() -> AppResult<()> {
        let mut task = match spawn_shutdown_resources(
            PtyCleanupResources::new(None, Some(Box::new(ExitReadyChild)), None),
            DefaultShell::CommandPrompt(PathBuf::from("cmd.exe")),
            false,
        ) {
            Ok(task) => task,
            Err(failure) => {
                let (_, error) = failure.into_parts();
                return Err(error);
            }
        };
        let detached_task = DetachedCleanupTask::new(task.result_rx.take(), task.thread.take());
        drop(task);

        let mut tasks = vec![detached_task];
        let failures = wait_for_detached_cleanup_collection(&mut tasks)?;

        assert!(tasks.is_empty());
        assert!(failures.is_empty());
        Ok(())
    }

    #[test]
    fn finished_detached_cleanup_failure_is_collected_for_shutdown_report() -> AppResult<()> {
        let child = BlockingKillFailureChild::new();
        let state = Arc::clone(&child.state);
        let mut task = match spawn_shutdown_resources(
            PtyCleanupResources::new(None, Some(Box::new(child)), None),
            DefaultShell::CommandPrompt(PathBuf::from("cmd.exe")),
            false,
        ) {
            Ok(task) => task,
            Err(failure) => {
                let (_, error) = failure.into_parts();
                return Err(error);
            }
        };

        wait_for_first_kill(&state)?;
        let detached_task = DetachedCleanupTask::new(task.result_rx.take(), task.thread.take());
        drop(task);
        release_first_kill(&state)?;

        let mut tasks = vec![detached_task];
        let mut failures = wait_for_detached_cleanup_collection(&mut tasks)?;

        assert!(tasks.is_empty());
        assert_eq!(failures.len(), 1);
        let error = recover_detached_cleanup_failure(failures.remove(0));
        assert_eq!(error.operation(), Some("cleanup detached pty resources"));
        assert!(error.to_string().contains("terminate pty child"));
        assert_eq!(kill_calls(&state)?, 2);
        Ok(())
    }

    #[test]
    fn detached_cleanup_drain_waits_for_bounded_shutdown_work() -> AppResult<()> {
        let child = BlockingKillFailureChild::new();
        let state = Arc::clone(&child.state);
        let mut task = match spawn_shutdown_resources(
            PtyCleanupResources::new(None, Some(Box::new(child)), None),
            DefaultShell::CommandPrompt(PathBuf::from("cmd.exe")),
            false,
        ) {
            Ok(task) => task,
            Err(failure) => {
                let (_, error) = failure.into_parts();
                return Err(error);
            }
        };

        wait_for_first_kill(&state)?;
        let detached_task = DetachedCleanupTask::new(task.result_rx.take(), task.thread.take());
        drop(task);

        let release_state = Arc::clone(&state);
        let release_delay = CHILD_GRACEFUL_EXIT_TIMEOUT + Duration::from_millis(150);
        let release_thread = std::thread::spawn(move || {
            std::thread::sleep(release_delay);
            release_first_kill(&release_state)
        });

        let (mut errors, pending_tasks) =
            drain_detached_cleanup_tasks(vec![detached_task], DETACHED_CLEANUP_DRAIN_TIMEOUT);

        release_thread.join().map_err(|_| {
            AppError::pty_message(
                "release detached pty cleanup test",
                "release thread panicked",
            )
        })??;

        let pending_count = pending_tasks.len();
        if pending_count > 0 {
            let (retry_errors, retry_pending_tasks) =
                drain_detached_cleanup_tasks(pending_tasks, Duration::from_secs(1));
            errors.extend(retry_errors);
            assert!(retry_pending_tasks.is_empty());
        }

        assert_eq!(
            pending_count, 0,
            "detached cleanup should finish within the shutdown drain timeout"
        );
        assert!(
            !errors
                .iter()
                .any(|error| error.operation() == Some("join detached pty cleanup tasks"))
        );
        assert!(errors.iter().any(|error| {
            error.operation() == Some("cleanup detached pty resources")
                && error.to_string().contains("terminate pty child")
        }));
        assert_eq!(kill_calls(&state)?, 2);
        Ok(())
    }

    #[derive(Debug)]
    struct ExitReadyChild;

    impl ChildKiller for ExitReadyChild {
        fn kill(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(Self)
        }
    }

    impl Child for ExitReadyChild {
        fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            Ok(Some(ExitStatus::with_exit_code(0)))
        }

        fn wait(&mut self) -> io::Result<ExitStatus> {
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

    type BlockingKillState = Arc<(Mutex<BlockingKillStateInner>, Condvar)>;

    #[derive(Debug)]
    struct BlockingKillFailureChild {
        state: BlockingKillState,
    }

    #[derive(Debug)]
    struct BlockingKillStateInner {
        kill_calls: usize,
        first_kill_entered: bool,
        release_first_kill: bool,
    }

    impl BlockingKillFailureChild {
        fn new() -> Self {
            Self {
                state: Arc::new((
                    Mutex::new(BlockingKillStateInner {
                        kill_calls: 0,
                        first_kill_entered: false,
                        release_first_kill: false,
                    }),
                    Condvar::new(),
                )),
            }
        }
    }

    impl ChildKiller for BlockingKillFailureChild {
        fn kill(&mut self) -> io::Result<()> {
            let (lock, cvar) = &*self.state;
            let mut state = lock
                .lock()
                .map_err(|_| io::Error::other("blocking kill mutex poisoned"))?;
            state.kill_calls = state.kill_calls.saturating_add(1);

            if state.kill_calls == 1 {
                state.first_kill_entered = true;
                cvar.notify_all();
                while !state.release_first_kill {
                    state = cvar
                        .wait(state)
                        .map_err(|_| io::Error::other("blocking kill mutex poisoned"))?;
                }
            }

            Err(io::Error::other("test child kill failure"))
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(Self {
                state: Arc::clone(&self.state),
            })
        }
    }

    impl Child for BlockingKillFailureChild {
        fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            Ok(None)
        }

        fn wait(&mut self) -> io::Result<ExitStatus> {
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

    fn wait_for_first_kill(state: &BlockingKillState) -> AppResult<()> {
        let (lock, cvar) = &**state;
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut guard = lock
            .lock()
            .map_err(|_| AppError::InvalidState("blocking kill mutex poisoned"))?;

        while !guard.first_kill_entered {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(AppError::pty_message(
                    "wait for detached pty cleanup test",
                    "timed out waiting for first child kill",
                ));
            }

            let (next_guard, wait_result) = cvar
                .wait_timeout(guard, remaining)
                .map_err(|_| AppError::InvalidState("blocking kill mutex poisoned"))?;
            guard = next_guard;
            if wait_result.timed_out() && !guard.first_kill_entered {
                return Err(AppError::pty_message(
                    "wait for detached pty cleanup test",
                    "timed out waiting for first child kill",
                ));
            }
        }

        Ok(())
    }

    fn release_first_kill(state: &BlockingKillState) -> AppResult<()> {
        let (lock, cvar) = &**state;
        let mut guard = lock
            .lock()
            .map_err(|_| AppError::InvalidState("blocking kill mutex poisoned"))?;
        guard.release_first_kill = true;
        cvar.notify_all();
        Ok(())
    }

    fn kill_calls(state: &BlockingKillState) -> AppResult<usize> {
        let (lock, _) = &**state;
        let guard = lock
            .lock()
            .map_err(|_| AppError::InvalidState("blocking kill mutex poisoned"))?;
        Ok(guard.kill_calls)
    }

    fn wait_for_detached_cleanup_collection(
        tasks: &mut Vec<DetachedCleanupTask>,
    ) -> AppResult<Vec<PtyCleanupFailure>> {
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut failures = Vec::new();

        loop {
            failures.extend(collect_finished_detached_cleanup_tasks(tasks));
            if tasks.is_empty() {
                return Ok(failures);
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(AppError::pty_message(
                    "wait for detached pty cleanup collection test",
                    "timed out waiting for detached cleanup task collection",
                ));
            }

            std::thread::sleep(remaining.min(Duration::from_millis(10)));
        }
    }
}
