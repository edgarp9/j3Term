use std::io::{ErrorKind, Read, Write};
use std::sync::mpsc::{
    self, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError, TrySendError,
};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::domain::{TerminalEvent, TerminalFailure, TerminalInputBytes};
use crate::error::{AppError, AppResult};

use super::event_queue::PtyEventQueue;
use super::{
    READ_BUFFER_SIZE, READ_FAILURE_USER_MESSAGE, READER_JOIN_TIMEOUT,
    SHUTDOWN_FAILURE_USER_MESSAGE, WRITE_FAILURE_USER_MESSAGE, WRITER_JOIN_TIMEOUT,
};

pub(super) type PtyWriter = Box<dyn Write + Send>;

// sync_channel blocks senders at this depth instead of letting input Vecs grow unbounded.
const MAX_PENDING_WRITE_REQUESTS: usize = 256;
const MAX_USER_INPUT_BATCH_REQUESTS: usize = MAX_PENDING_WRITE_REQUESTS;
const WRITER_INPUT_QUEUE_FULL_MESSAGE: &str = "pty writer input queue is full";

pub(super) struct ReaderThread {
    handle: JoinHandle<()>,
    done_rx: Receiver<()>,
}

pub(super) struct WriterThread {
    input_tx: SyncSender<PtyWriteRequest>,
    handle: JoinHandle<AppResult<()>>,
    done_rx: Receiver<()>,
}

struct ReaderCompletion {
    done_tx: Option<Sender<()>>,
}

struct WriterCompletion {
    done_tx: Option<Sender<()>>,
}

pub(super) enum PtyWriteRequest {
    UserInput(PtyWriteBytes),
    GracefulExit(Vec<u8>),
}

pub(super) enum PtyWriteBytes {
    TerminalInput(TerminalInputBytes),
    Owned(Vec<u8>),
}

impl PtyWriteRequest {
    pub(super) fn user_input(bytes: Vec<u8>) -> Self {
        Self::UserInput(PtyWriteBytes::Owned(bytes))
    }

    fn terminal_input(bytes: TerminalInputBytes) -> Self {
        Self::UserInput(PtyWriteBytes::TerminalInput(bytes))
    }
}

impl PtyWriteBytes {
    fn as_slice(&self) -> &[u8] {
        match self {
            Self::TerminalInput(bytes) => bytes.as_slice(),
            Self::Owned(bytes) => bytes,
        }
    }
}

pub(super) struct PtyIoWorkers {
    input_tx: Option<SyncSender<PtyWriteRequest>>,
    reader_thread: Option<JoinHandle<()>>,
    reader_done_rx: Option<Receiver<()>>,
    writer_thread: Option<JoinHandle<AppResult<()>>>,
    writer_done_rx: Option<Receiver<()>>,
}

struct PtyIoWorkerReaper {
    reader_thread: Option<JoinHandle<()>>,
    reader_done_rx: Option<Receiver<()>>,
    writer_thread: Option<JoinHandle<AppResult<()>>>,
    writer_done_rx: Option<Receiver<()>>,
}

impl ReaderCompletion {
    fn new(done_tx: Sender<()>) -> Self {
        Self {
            done_tx: Some(done_tx),
        }
    }
}

impl Drop for ReaderCompletion {
    fn drop(&mut self) {
        if let Some(done_tx) = self.done_tx.take() {
            let _ = done_tx.send(());
        }
    }
}

impl WriterCompletion {
    fn new(done_tx: Sender<()>) -> Self {
        Self {
            done_tx: Some(done_tx),
        }
    }
}

impl Drop for WriterCompletion {
    fn drop(&mut self) {
        if let Some(done_tx) = self.done_tx.take() {
            let _ = done_tx.send(());
        }
    }
}

impl PtyIoWorkerReaper {
    fn from_workers(workers: &mut PtyIoWorkers) -> Option<Self> {
        if !workers.has_unjoined_threads() {
            return None;
        }

        Some(Self {
            reader_thread: workers.reader_thread.take(),
            reader_done_rx: workers.reader_done_rx.take(),
            writer_thread: workers.writer_thread.take(),
            writer_done_rx: workers.writer_done_rx.take(),
        })
    }

    fn spawn_or_join(self) {
        let shared_reaper = Arc::new(Mutex::new(Some(self)));
        let worker_reaper = Arc::clone(&shared_reaper);
        let spawn_result = thread::Builder::new()
            .name("j3term-pty-worker-reaper".to_owned())
            .spawn(move || {
                let reaper = match worker_reaper.lock() {
                    Ok(mut guard) => guard.take(),
                    Err(_) => None,
                };
                if let Some(reaper) = reaper {
                    reaper.join_blocking();
                }
            });

        if spawn_result.is_err() {
            let reaper = match shared_reaper.lock() {
                Ok(mut guard) => guard.take(),
                Err(_) => None,
            };
            if let Some(reaper) = reaper {
                reaper.join_blocking();
            }
        }
    }

    fn join_blocking(mut self) {
        if let Some(thread) = self.writer_thread.take() {
            join_finished_worker_thread(thread, self.writer_done_rx.take(), WRITER_JOIN_TIMEOUT);
        }
        if let Some(thread) = self.reader_thread.take() {
            join_finished_worker_thread(thread, self.reader_done_rx.take(), READER_JOIN_TIMEOUT);
        }
    }
}

impl Drop for PtyIoWorkers {
    fn drop(&mut self) {
        self.close_input();
        if let Some(reaper) = PtyIoWorkerReaper::from_workers(self) {
            reaper.spawn_or_join();
        }
    }
}

impl PtyIoWorkers {
    pub(super) fn new(writer_thread: WriterThread, reader_thread: ReaderThread) -> Self {
        Self {
            input_tx: Some(writer_thread.input_tx),
            reader_thread: Some(reader_thread.handle),
            reader_done_rx: Some(reader_thread.done_rx),
            writer_thread: Some(writer_thread.handle),
            writer_done_rx: Some(writer_thread.done_rx),
        }
    }

    pub(super) fn from_reader(reader_thread: ReaderThread) -> Self {
        Self {
            input_tx: None,
            reader_thread: Some(reader_thread.handle),
            reader_done_rx: Some(reader_thread.done_rx),
            writer_thread: None,
            writer_done_rx: None,
        }
    }

    pub(super) fn send_user_input(&self, bytes: Vec<u8>) -> AppResult<()> {
        let Some(input_tx) = self.input_tx.as_ref() else {
            return Err(AppError::InvalidState("pty writer worker is not running"));
        };

        try_send_write_request(input_tx, PtyWriteRequest::user_input(bytes))
    }

    pub(super) fn send_terminal_input(&self, bytes: TerminalInputBytes) -> AppResult<()> {
        let Some(input_tx) = self.input_tx.as_ref() else {
            return Err(AppError::InvalidState("pty writer worker is not running"));
        };

        try_send_write_request(input_tx, PtyWriteRequest::terminal_input(bytes))
    }

    pub(super) fn send_graceful_exit(&self, bytes: &[u8]) -> AppResult<()> {
        let Some(input_tx) = self.input_tx.as_ref() else {
            return Ok(());
        };

        try_send_write_request(input_tx, PtyWriteRequest::GracefulExit(bytes.to_vec()))
    }

    pub(super) fn close_input(&mut self) {
        self.input_tx = None;
    }

    pub(super) fn has_unjoined_threads(&self) -> bool {
        self.reader_thread.is_some() || self.writer_thread.is_some()
    }

    pub(super) fn join_reader(&mut self) -> AppResult<()> {
        join_reader_thread(&mut self.reader_thread, &mut self.reader_done_rx)
    }

    pub(super) fn join_writer(&mut self) -> AppResult<()> {
        join_writer_thread(&mut self.writer_thread, &mut self.writer_done_rx)
    }

    #[cfg(test)]
    pub(super) fn with_input_tx_for_test(input_tx: SyncSender<PtyWriteRequest>) -> Self {
        Self {
            input_tx: Some(input_tx),
            reader_thread: None,
            reader_done_rx: None,
            writer_thread: None,
            writer_done_rx: None,
        }
    }

    #[cfg(test)]
    pub(super) fn with_reader_for_test(reader_thread: JoinHandle<()>) -> Self {
        Self {
            input_tx: None,
            reader_thread: Some(reader_thread),
            reader_done_rx: None,
            writer_thread: None,
            writer_done_rx: None,
        }
    }

    #[cfg(test)]
    pub(super) fn with_reader_completion_for_test(
        reader_thread: JoinHandle<()>,
        reader_done_rx: Receiver<()>,
    ) -> Self {
        Self {
            input_tx: None,
            reader_thread: Some(reader_thread),
            reader_done_rx: Some(reader_done_rx),
            writer_thread: None,
            writer_done_rx: None,
        }
    }

    #[cfg(test)]
    pub(super) fn with_writer_for_test(writer_thread: WriterThread) -> Self {
        Self {
            input_tx: Some(writer_thread.input_tx),
            reader_thread: None,
            reader_done_rx: None,
            writer_thread: Some(writer_thread.handle),
            writer_done_rx: Some(writer_thread.done_rx),
        }
    }
}

fn try_send_write_request(
    input_tx: &SyncSender<PtyWriteRequest>,
    request: PtyWriteRequest,
) -> AppResult<()> {
    input_tx.try_send(request).map_err(|error| match error {
        TrySendError::Full(_) => AppError::InvalidState(WRITER_INPUT_QUEUE_FULL_MESSAGE),
        TrySendError::Disconnected(_) => AppError::InvalidState("pty writer worker is not running"),
    })
}

pub(super) fn is_writer_input_queue_full_error(error: &AppError) -> bool {
    matches!(
        error,
        AppError::InvalidState(message) if *message == WRITER_INPUT_QUEUE_FULL_MESSAGE
    )
}

pub(super) fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    events: PtyEventQueue,
) -> AppResult<ReaderThread> {
    let (done_tx, done_rx) = mpsc::channel();
    let handle = thread::Builder::new()
        .name("j3term-pty-reader".to_owned())
        .spawn(move || {
            let _completion = ReaderCompletion::new(done_tx);
            let mut buffer = [0_u8; READ_BUFFER_SIZE];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => {
                        let _ = events.push(TerminalEvent::PtyClosed);
                        break;
                    }
                    Ok(read) => {
                        if !events.push_output(&buffer[..read]) {
                            break;
                        }
                    }
                    Err(source) if source.kind() == ErrorKind::Interrupted => continue,
                    Err(source) => {
                        let error = AppError::io("read from pty stdout", source);
                        let failure =
                            terminal_failure_from_app_error(READ_FAILURE_USER_MESSAGE, &error);
                        let _ = events.push(TerminalEvent::Failure(failure));
                        break;
                    }
                }
            }
        })
        .map_err(|source| AppError::io("spawn pty reader thread", source))?;

    Ok(ReaderThread { handle, done_rx })
}

pub(super) fn spawn_writer(
    mut writer: PtyWriter,
    events: PtyEventQueue,
) -> AppResult<WriterThread> {
    let (input_tx, input_rx) = mpsc::sync_channel(MAX_PENDING_WRITE_REQUESTS);
    let (done_tx, done_rx) = mpsc::channel();
    let handle = thread::Builder::new()
        .name("j3term-pty-writer".to_owned())
        .spawn(move || {
            let _completion = WriterCompletion::new(done_tx);
            let mut pending_request: Option<PtyWriteRequest> = None;
            loop {
                let request = match pending_request.take() {
                    Some(request) => request,
                    None => match input_rx.recv() {
                        Ok(request) => request,
                        Err(_) => break,
                    },
                };

                match request {
                    PtyWriteRequest::UserInput(bytes) => {
                        match write_user_input_batch(writer.as_mut(), bytes, &input_rx) {
                            Ok(next_request) => {
                                pending_request = next_request;
                            }
                            Err(error) => {
                                let failure = terminal_failure_from_app_error(
                                    WRITE_FAILURE_USER_MESSAGE,
                                    &error,
                                );
                                let _ = events.push(TerminalEvent::StdinWriteFailed(failure));
                                return Err(error);
                            }
                        }
                    }
                    PtyWriteRequest::GracefulExit(bytes) => {
                        if let Err(error) = write_pty_bytes(
                            writer.as_mut(),
                            &bytes,
                            "write graceful pty shutdown",
                            "flush graceful pty shutdown",
                        ) {
                            let failure = terminal_failure_from_app_error(
                                SHUTDOWN_FAILURE_USER_MESSAGE,
                                &error,
                            );
                            let _ = events.push(TerminalEvent::Failure(failure));
                            return Err(error);
                        }
                    }
                }
            }
            Ok(())
        })
        .map_err(|source| AppError::io("spawn pty writer thread", source))?;

    Ok(WriterThread {
        input_tx,
        handle,
        done_rx,
    })
}

fn write_user_input_batch(
    writer: &mut (dyn Write + Send),
    first: PtyWriteBytes,
    input_rx: &Receiver<PtyWriteRequest>,
) -> AppResult<Option<PtyWriteRequest>> {
    let mut wrote_bytes =
        write_pty_bytes_without_flush(writer, first.as_slice(), "write to pty stdin")?;
    let mut batched_requests = 1;

    while batched_requests < MAX_USER_INPUT_BATCH_REQUESTS {
        match input_rx.try_recv() {
            Ok(PtyWriteRequest::UserInput(bytes)) => {
                if write_pty_bytes_without_flush(writer, bytes.as_slice(), "write to pty stdin")? {
                    wrote_bytes = true;
                }
                batched_requests += 1;
            }
            Ok(request) => {
                if wrote_bytes {
                    flush_pty_writer(writer, "flush pty stdin")?;
                }
                return Ok(Some(request));
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
        }
    }

    if wrote_bytes {
        flush_pty_writer(writer, "flush pty stdin")?;
    }

    Ok(None)
}

fn write_pty_bytes(
    writer: &mut (dyn Write + Send),
    bytes: &[u8],
    write_operation: &'static str,
    flush_operation: &'static str,
) -> AppResult<()> {
    if write_pty_bytes_without_flush(writer, bytes, write_operation)? {
        flush_pty_writer(writer, flush_operation)?;
    }

    Ok(())
}

fn write_pty_bytes_without_flush(
    writer: &mut (dyn Write + Send),
    bytes: &[u8],
    write_operation: &'static str,
) -> AppResult<bool> {
    if bytes.is_empty() {
        return Ok(false);
    }

    writer
        .write_all(bytes)
        .map_err(|source| AppError::io(write_operation, source))?;

    Ok(true)
}

fn flush_pty_writer(
    writer: &mut (dyn Write + Send),
    flush_operation: &'static str,
) -> AppResult<()> {
    writer
        .flush()
        .map_err(|source| AppError::io(flush_operation, source))
}

fn join_reader_thread(
    reader_thread: &mut Option<JoinHandle<()>>,
    reader_done_rx: &mut Option<Receiver<()>>,
) -> AppResult<()> {
    let Some(thread) = reader_thread.take() else {
        *reader_done_rx = None;
        return Ok(());
    };

    if let Some(done_rx) = reader_done_rx.take() {
        match done_rx.recv_timeout(READER_JOIN_TIMEOUT) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {}
            Err(RecvTimeoutError::Timeout) => {
                *reader_thread = Some(thread);
                *reader_done_rx = Some(done_rx);
                return Err(AppError::pty_message(
                    "join pty reader thread",
                    "timed out waiting for pty reader thread to exit",
                ));
            }
        }
    }

    thread
        .join()
        .map_err(|_| AppError::pty_message("join pty reader thread", "pty reader thread panicked"))
}

fn join_writer_thread(
    writer_thread: &mut Option<JoinHandle<AppResult<()>>>,
    writer_done_rx: &mut Option<Receiver<()>>,
) -> AppResult<()> {
    let Some(thread) = writer_thread.take() else {
        *writer_done_rx = None;
        return Ok(());
    };

    if let Some(done_rx) = writer_done_rx.take() {
        match done_rx.recv_timeout(WRITER_JOIN_TIMEOUT) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {}
            Err(RecvTimeoutError::Timeout) => {
                *writer_thread = Some(thread);
                *writer_done_rx = Some(done_rx);
                return Err(AppError::pty_message(
                    "join pty writer thread",
                    "timed out waiting for pty writer thread to exit",
                ));
            }
        }
    }

    thread.join().map_err(|_| {
        AppError::pty_message("join pty writer thread", "pty writer thread panicked")
    })?
}

fn join_finished_worker_thread<T>(
    thread: JoinHandle<T>,
    done_rx: Option<Receiver<()>>,
    timeout: Duration,
) {
    let worker_done = wait_for_worker_done(done_rx, timeout);
    if worker_done && !thread.is_finished() {
        thread::yield_now();
    }
    // JoinHandle::join has no timeout, so the reaper only joins threads that
    // are already known to have finished. Otherwise dropping the handle detaches it.
    if thread.is_finished() {
        let _ = thread.join();
    }
}

fn wait_for_worker_done(done_rx: Option<Receiver<()>>, timeout: Duration) -> bool {
    let Some(done_rx) = done_rx else {
        return false;
    };

    match done_rx.recv_timeout(timeout) {
        Ok(()) | Err(RecvTimeoutError::Disconnected) => true,
        Err(RecvTimeoutError::Timeout) => false,
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

#[cfg(test)]
mod tests {
    use super::*;

    struct ScriptedReader {
        reads: Vec<std::io::Result<Vec<u8>>>,
    }

    impl ScriptedReader {
        fn new(reads: Vec<std::io::Result<Vec<u8>>>) -> Self {
            Self { reads }
        }
    }

    impl Read for ScriptedReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let read = if self.reads.is_empty() {
                Ok(Vec::new())
            } else {
                self.reads.remove(0)
            };

            match read {
                Ok(bytes) => {
                    let byte_count = bytes.len().min(buf.len());
                    buf[..byte_count].copy_from_slice(&bytes[..byte_count]);
                    Ok(byte_count)
                }
                Err(error) => Err(error),
            }
        }
    }

    #[derive(Default)]
    struct RecordingWriter {
        bytes: Vec<u8>,
        flushes: usize,
    }

    impl RecordingWriter {
        fn bytes(&self) -> &[u8] {
            &self.bytes
        }

        fn flushes(&self) -> usize {
            self.flushes
        }
    }

    impl Write for RecordingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    fn full_writer_queue() -> AppResult<(SyncSender<PtyWriteRequest>, Receiver<PtyWriteRequest>)> {
        let (input_tx, input_rx) = mpsc::sync_channel(1);
        input_tx
            .try_send(PtyWriteRequest::user_input(b"queued".to_vec()))
            .map_err(|_| AppError::InvalidState("failed to fill pty writer input queue"))?;

        Ok((input_tx, input_rx))
    }

    fn queue_write_request(
        input_tx: &SyncSender<PtyWriteRequest>,
        request: PtyWriteRequest,
    ) -> AppResult<()> {
        input_tx
            .try_send(request)
            .map_err(|_| AppError::InvalidState("failed to queue pty writer input"))
    }

    #[test]
    fn reader_retries_interrupted_read_and_continues_output() -> AppResult<()> {
        let reader = ScriptedReader::new(vec![
            Err(std::io::Error::from(ErrorKind::Interrupted)),
            Ok(b"ready".to_vec()),
            Ok(Vec::new()),
        ]);
        let events = PtyEventQueue::new();

        let ReaderThread { handle, done_rx } = spawn_reader(Box::new(reader), events.clone())?;
        let mut reader_thread = Some(handle);
        let mut reader_done_rx = Some(done_rx);

        join_reader_thread(&mut reader_thread, &mut reader_done_rx)?;

        let drained = events.drain();
        assert!(matches!(
            drained.as_slice(),
            [TerminalEvent::PtyOutput(bytes), TerminalEvent::PtyClosed]
                if bytes.as_slice() == b"ready"
        ));
        Ok(())
    }

    #[test]
    fn user_input_batch_flushes_once_for_contiguous_inputs() -> AppResult<()> {
        let (input_tx, input_rx) = mpsc::sync_channel(4);
        queue_write_request(&input_tx, PtyWriteRequest::user_input(b"b".to_vec()))?;
        queue_write_request(&input_tx, PtyWriteRequest::user_input(b"c".to_vec()))?;
        let mut writer = RecordingWriter::default();

        let pending_request =
            write_user_input_batch(&mut writer, PtyWriteBytes::Owned(b"a".to_vec()), &input_rx)?;

        assert!(pending_request.is_none());
        assert_eq!(writer.bytes(), b"abc");
        assert_eq!(writer.flushes(), 1);
        Ok(())
    }

    #[test]
    fn user_input_batch_preserves_graceful_exit_order() -> AppResult<()> {
        let (input_tx, input_rx) = mpsc::sync_channel(4);
        queue_write_request(&input_tx, PtyWriteRequest::user_input(b"b".to_vec()))?;
        queue_write_request(&input_tx, PtyWriteRequest::GracefulExit(b"exit".to_vec()))?;
        queue_write_request(&input_tx, PtyWriteRequest::user_input(b"after".to_vec()))?;
        let mut writer = RecordingWriter::default();

        let pending_request =
            write_user_input_batch(&mut writer, PtyWriteBytes::Owned(b"a".to_vec()), &input_rx)?;

        let Some(PtyWriteRequest::GracefulExit(bytes)) = pending_request else {
            return Err(AppError::InvalidState(
                "graceful exit request was not preserved",
            ));
        };
        assert_eq!(writer.bytes(), b"ab");
        assert_eq!(writer.flushes(), 1);
        assert_eq!(bytes.as_slice(), b"exit");

        match input_rx.try_recv() {
            Ok(PtyWriteRequest::UserInput(bytes)) => {
                assert_eq!(bytes.as_slice(), b"after");
            }
            Ok(PtyWriteRequest::GracefulExit(_)) => {
                return Err(AppError::InvalidState(
                    "unexpected graceful exit request after pending request",
                ));
            }
            Err(_) => {
                return Err(AppError::InvalidState(
                    "remaining input request was not left queued",
                ));
            }
        }
        Ok(())
    }

    #[test]
    fn send_user_input_fails_immediately_when_writer_queue_is_full() -> AppResult<()> {
        let (input_tx, _input_rx) = full_writer_queue()?;
        let workers = PtyIoWorkers::with_input_tx_for_test(input_tx);

        let result = workers.send_user_input(b"blocked input".to_vec());

        assert!(matches!(
            result,
            Err(AppError::InvalidState(message)) if message == "pty writer input queue is full"
        ));
        Ok(())
    }

    #[test]
    fn send_graceful_exit_fails_immediately_when_writer_queue_is_full() -> AppResult<()> {
        let (input_tx, _input_rx) = full_writer_queue()?;
        let workers = PtyIoWorkers::with_input_tx_for_test(input_tx);

        let result = workers.send_graceful_exit(b"exit");

        assert!(matches!(
            result,
            Err(AppError::InvalidState(message)) if message == "pty writer input queue is full"
        ));
        Ok(())
    }

    #[test]
    fn reaper_returns_without_joining_unfinished_reader_without_done_signal() -> AppResult<()> {
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let reader_thread = thread::spawn(move || {
            let _ = release_rx.recv();
        });
        let reaper = PtyIoWorkerReaper {
            reader_thread: Some(reader_thread),
            reader_done_rx: None,
            writer_thread: None,
            writer_done_rx: None,
        };
        let (done_tx, done_rx) = mpsc::channel();

        let reaper_thread = thread::spawn(move || {
            reaper.join_blocking();
            let _ = done_tx.send(());
        });

        let result = done_rx.recv_timeout(Duration::from_secs(1));
        drop(release_tx);
        let _ = reaper_thread.join();

        assert!(matches!(result, Ok(())));
        Ok(())
    }

    #[test]
    fn worker_done_wait_returns_on_timeout() {
        let (_done_tx, done_rx) = mpsc::channel();

        let result = wait_for_worker_done(Some(done_rx), Duration::from_millis(1));

        assert!(!result);
    }
}
