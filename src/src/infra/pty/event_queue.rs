use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::domain::TerminalEvent;

use super::{MAX_PENDING_EVENT_COUNT, MAX_PENDING_OUTPUT_BYTES};

const MAX_OUTPUT_NODE_BYTES: usize = super::READ_BUFFER_SIZE * 4;

#[derive(Clone)]
pub(super) struct PtyEventQueue {
    inner: Arc<Mutex<PtyEventQueueState>>,
}

struct PtyEventQueueState {
    events: QueuedEventList,
    output_nodes: VecDeque<QueuedEventNodeId>,
    pending_output_bytes: usize,
    dropped_output_bytes: usize,
    closed: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct QueuedEventNodeId {
    index: usize,
    generation: u64,
}

struct QueuedEventList {
    slots: Vec<QueuedEventSlot>,
    free_slots: Vec<usize>,
    head: Option<usize>,
    tail: Option<usize>,
    len: usize,
}

struct QueuedEventSlot {
    generation: u64,
    node: Option<QueuedEventNode>,
}

struct QueuedEventNode {
    event: QueuedEvent,
    prev: Option<usize>,
    next: Option<usize>,
}

enum QueuedEvent {
    Control(TerminalEvent),
    Output(QueuedOutput),
}

struct QueuedOutput {
    bytes: Vec<u8>,
}

struct DrainedQueuedEvents {
    events: QueuedEventList,
    event_count: usize,
    dropped_output_bytes: usize,
}

#[derive(Clone, Copy)]
pub(super) struct PtyEventDrainBudget {
    max_events: usize,
    max_output_bytes: usize,
}

impl PtyEventDrainBudget {
    pub(super) fn new(max_events: usize, max_output_bytes: usize) -> Self {
        Self {
            max_events,
            max_output_bytes,
        }
    }

    pub(super) fn has_event_capacity(self) -> bool {
        self.max_events > 0
    }

    pub(super) fn remaining_after(self, events: &[TerminalEvent]) -> Self {
        let output_bytes = events
            .iter()
            .map(|event| match event {
                TerminalEvent::PtyOutput(bytes) => bytes.len(),
                _ => 0,
            })
            .fold(0usize, usize::saturating_add);

        Self {
            max_events: self.max_events.saturating_sub(events.len()),
            max_output_bytes: self.max_output_bytes.saturating_sub(output_bytes),
        }
    }
}

impl PtyEventQueue {
    pub(super) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(PtyEventQueueState {
                events: QueuedEventList::new(),
                output_nodes: VecDeque::new(),
                pending_output_bytes: 0,
                dropped_output_bytes: 0,
                closed: false,
            })),
        }
    }

    pub(super) fn push(&self, event: TerminalEvent) -> bool {
        self.lock_state().push(event)
    }

    pub(super) fn push_output(&self, bytes: &[u8]) -> bool {
        if bytes.len() >= MAX_OUTPUT_NODE_BYTES {
            return self.push(TerminalEvent::PtyOutput(bytes.to_vec()));
        }

        self.lock_state().push_reader_output(bytes)
    }

    pub(super) fn drain(&self) -> Vec<TerminalEvent> {
        let drained = {
            let mut state = self.lock_state();
            state.drain()
        };
        drained.into_terminal_events()
    }

    pub(super) fn drain_with_budget(&self, budget: PtyEventDrainBudget) -> Vec<TerminalEvent> {
        let drained = {
            let mut state = self.lock_state();
            state.drain_with_budget(budget)
        };
        drained.into_terminal_events()
    }

    pub(super) fn clear(&self) {
        let mut state = self.lock_state();
        state.clear_events();
    }

    pub(super) fn close(&self) {
        let mut state = self.lock_state();
        state.clear_events();
        state.closed = true;
    }

    pub(super) fn close_and_drain(&self) -> Vec<TerminalEvent> {
        let drained = {
            let mut state = self.lock_state();
            state.closed = true;
            state.drain()
        };
        drained.into_terminal_events()
    }

    fn lock_state(&self) -> MutexGuard<'_, PtyEventQueueState> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl PtyEventQueueState {
    fn drain(&mut self) -> DrainedQueuedEvents {
        let event_count = self.events.len();
        let events = if event_count == 0 {
            QueuedEventList::new()
        } else {
            std::mem::replace(&mut self.events, QueuedEventList::new())
        };
        let dropped_output_bytes = self.dropped_output_bytes;
        self.dropped_output_bytes = 0;
        self.output_nodes.clear();
        self.pending_output_bytes = 0;
        DrainedQueuedEvents {
            events,
            event_count,
            dropped_output_bytes,
        }
    }

    fn clear_events(&mut self) {
        self.events.clear();
        self.output_nodes.clear();
        self.pending_output_bytes = 0;
        self.dropped_output_bytes = 0;
    }

    fn push(&mut self, event: TerminalEvent) -> bool {
        if self.closed {
            return false;
        }

        match event {
            TerminalEvent::PtyOutput(bytes) => self.push_output_vec(bytes),
            event => self.push_control(event),
        }
    }

    fn drain_with_budget(&mut self, budget: PtyEventDrainBudget) -> DrainedQueuedEvents {
        let dropped_output_bytes = if budget.max_events > 0 {
            let dropped = self.dropped_output_bytes;
            self.dropped_output_bytes = 0;
            dropped
        } else {
            0
        };
        let event_budget = budget
            .max_events
            .saturating_sub(usize::from(dropped_output_bytes > 0));
        let mut drained = QueuedEventList::new();
        let mut event_count = 0usize;
        let mut output_bytes = 0usize;

        while event_count < event_budget {
            let Some((_, event)) = self.events.front() else {
                break;
            };
            let next_output_bytes = match event {
                QueuedEvent::Output(output) => output.len(),
                QueuedEvent::Control(_) => 0,
            };
            if next_output_bytes > 0
                && output_bytes.saturating_add(next_output_bytes) > budget.max_output_bytes
                && (event_count > 0 || budget.max_output_bytes == 0)
            {
                break;
            }

            let Some((node, event)) = self.events.pop_front() else {
                break;
            };
            if let QueuedEvent::Output(output) = &event {
                self.pending_output_bytes = self.pending_output_bytes.saturating_sub(output.len());
                output_bytes = output_bytes.saturating_add(output.len());
                self.remove_drained_output_node(node);
            }
            drained.push_back(event);
            event_count = event_count.saturating_add(1);
        }

        DrainedQueuedEvents {
            events: drained,
            event_count,
            dropped_output_bytes,
        }
    }

    fn push_reader_output(&mut self, bytes: &[u8]) -> bool {
        if self.closed {
            return false;
        }

        self.merge_reader_output(bytes)
    }

    fn push_output_vec(&mut self, bytes: Vec<u8>) -> bool {
        let byte_count = bytes.len();
        if !self.prepare_output_node_push(byte_count) {
            self.record_dropped_output_bytes(byte_count);
            return false;
        }

        self.push_output_node(bytes);
        true
    }

    fn merge_reader_output(&mut self, bytes: &[u8]) -> bool {
        let byte_count = bytes.len();
        if !self.prepare_output_merge_push(byte_count) {
            self.record_dropped_output_bytes(byte_count);
            return false;
        }

        if self.append_to_tail_output(bytes) {
            return true;
        }
        self.push_output_node(bytes.to_vec());
        true
    }

    fn prepare_output_merge_push(&mut self, byte_count: usize) -> bool {
        while self.pending_output_bytes.saturating_add(byte_count) > MAX_PENDING_OUTPUT_BYTES {
            if !self.drop_oldest_output() {
                return false;
            }
        }

        if self.events.tail_output_has_capacity(byte_count) {
            return true;
        }

        if self.events.len() >= MAX_PENDING_EVENT_COUNT && !self.drop_oldest_output() {
            return false;
        }

        true
    }

    fn prepare_output_node_push(&mut self, byte_count: usize) -> bool {
        while self.pending_output_bytes.saturating_add(byte_count) > MAX_PENDING_OUTPUT_BYTES {
            if !self.drop_oldest_output() {
                return false;
            }
        }

        if self.events.len() >= MAX_PENDING_EVENT_COUNT && !self.drop_oldest_output() {
            return false;
        }

        true
    }

    fn append_to_tail_output(&mut self, bytes: &[u8]) -> bool {
        let byte_count = bytes.len();
        if self.events.push_tail_output_chunk(bytes) {
            self.pending_output_bytes = self.pending_output_bytes.saturating_add(byte_count);
            true
        } else {
            false
        }
    }

    fn push_output_node(&mut self, bytes: Vec<u8>) {
        let output = QueuedOutput::from_vec(bytes);
        self.pending_output_bytes = self.pending_output_bytes.saturating_add(output.len());
        let node = self.events.push_back(QueuedEvent::Output(output));
        self.output_nodes.push_back(node);
    }

    fn push_control(&mut self, event: TerminalEvent) -> bool {
        if self.events.len() >= MAX_PENDING_EVENT_COUNT {
            // Lifecycle and failure events are more important than stale terminal output.
            if !self.drop_oldest_output() {
                return false;
            }
        }

        self.events.push_back(QueuedEvent::Control(event));
        true
    }

    fn drop_oldest_output(&mut self) -> bool {
        while let Some(node) = self.output_nodes.pop_front() {
            let Some(bytes) = self.events.remove_output(node) else {
                continue;
            };

            self.pending_output_bytes = self.pending_output_bytes.saturating_sub(bytes.len());
            self.record_dropped_output_bytes(bytes.len());
            return true;
        }

        false
    }

    fn remove_drained_output_node(&mut self, node: QueuedEventNodeId) {
        while let Some(front) = self.output_nodes.front().copied() {
            if front == node {
                self.output_nodes.pop_front();
                return;
            }
            if matches!(self.events.event(front), Some(QueuedEvent::Output(_))) {
                return;
            }
            self.output_nodes.pop_front();
        }
    }

    fn record_dropped_output_bytes(&mut self, byte_count: usize) {
        self.dropped_output_bytes = self.dropped_output_bytes.saturating_add(byte_count);
    }
}

impl QueuedOutput {
    fn from_vec(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn push_chunk(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    fn into_vec(self) -> Vec<u8> {
        self.bytes
    }
}

impl DrainedQueuedEvents {
    fn into_terminal_events(mut self) -> Vec<TerminalEvent> {
        let mut drained = Vec::with_capacity(
            self.event_count
                .saturating_add(usize::from(self.dropped_output_bytes > 0)),
        );
        if self.dropped_output_bytes > 0 {
            drained.push(TerminalEvent::PtyOutputDropped {
                byte_count: self.dropped_output_bytes,
            });
        }
        while let Some((_, event)) = self.events.pop_front() {
            match event {
                QueuedEvent::Control(event) => drained.push(event),
                QueuedEvent::Output(output) => {
                    drained.push(TerminalEvent::PtyOutput(output.into_vec()));
                }
            }
        }
        drained
    }
}

impl QueuedEventList {
    fn new() -> Self {
        Self {
            slots: Vec::new(),
            free_slots: Vec::new(),
            head: None,
            tail: None,
            len: 0,
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn tail_output_has_capacity(&self, additional_bytes: usize) -> bool {
        let Some(tail) = self.tail else {
            return false;
        };

        let Some(slot) = self.slots.get(tail) else {
            return false;
        };

        let Some(node) = slot.node.as_ref() else {
            return false;
        };

        let QueuedEvent::Output(output) = &node.event else {
            return false;
        };

        output.len().saturating_add(additional_bytes) <= MAX_OUTPUT_NODE_BYTES
    }

    fn push_tail_output_chunk(&mut self, bytes: &[u8]) -> bool {
        let Some(tail) = self.tail else {
            return false;
        };

        let Some(slot) = self.slots.get_mut(tail) else {
            return false;
        };

        let Some(node) = slot.node.as_mut() else {
            return false;
        };

        let QueuedEvent::Output(output) = &mut node.event else {
            return false;
        };

        if output.len().saturating_add(bytes.len()) > MAX_OUTPUT_NODE_BYTES {
            return false;
        }

        output.push_chunk(bytes);
        true
    }

    fn push_back(&mut self, event: QueuedEvent) -> QueuedEventNodeId {
        let index = self.next_slot_index();
        let generation = self.next_generation(index);
        let node = QueuedEventNode {
            event,
            prev: self.tail,
            next: None,
        };

        if let Some(slot) = self.slots.get_mut(index) {
            slot.node = Some(node);
        }

        match self.tail {
            Some(tail) => {
                if let Some(tail_slot) = self.slots.get_mut(tail)
                    && let Some(tail_node) = tail_slot.node.as_mut()
                {
                    tail_node.next = Some(index);
                }
            }
            None => {
                self.head = Some(index);
            }
        }

        self.tail = Some(index);
        self.len = self.len.saturating_add(1);
        QueuedEventNodeId { index, generation }
    }

    fn pop_front(&mut self) -> Option<(QueuedEventNodeId, QueuedEvent)> {
        let index = self.head?;
        let generation = self.slots.get(index)?.generation;
        let node = QueuedEventNodeId { index, generation };
        self.remove(node).map(|event| (node, event))
    }

    fn front(&self) -> Option<(QueuedEventNodeId, &QueuedEvent)> {
        let index = self.head?;
        let generation = self.slots.get(index)?.generation;
        let node = QueuedEventNodeId { index, generation };
        self.event(node).map(|event| (node, event))
    }

    fn remove_output(&mut self, node: QueuedEventNodeId) -> Option<QueuedOutput> {
        if !matches!(self.event(node), Some(QueuedEvent::Output(_))) {
            return None;
        }

        match self.remove(node)? {
            QueuedEvent::Output(bytes) => Some(bytes),
            QueuedEvent::Control(_) => None,
        }
    }

    fn clear(&mut self) {
        self.slots.clear();
        self.free_slots.clear();
        self.head = None;
        self.tail = None;
        self.len = 0;
    }

    fn event(&self, node: QueuedEventNodeId) -> Option<&QueuedEvent> {
        let slot = self.slots.get(node.index)?;
        if slot.generation != node.generation {
            return None;
        }

        slot.node.as_ref().map(|queued| &queued.event)
    }

    fn remove(&mut self, node: QueuedEventNodeId) -> Option<QueuedEvent> {
        let removed = {
            let slot = self.slots.get_mut(node.index)?;
            if slot.generation != node.generation {
                return None;
            }

            slot.node.take()?
        };

        match removed.prev {
            Some(prev) => {
                if let Some(prev_slot) = self.slots.get_mut(prev)
                    && let Some(prev_node) = prev_slot.node.as_mut()
                {
                    prev_node.next = removed.next;
                }
            }
            None => {
                self.head = removed.next;
            }
        }

        match removed.next {
            Some(next) => {
                if let Some(next_slot) = self.slots.get_mut(next)
                    && let Some(next_node) = next_slot.node.as_mut()
                {
                    next_node.prev = removed.prev;
                }
            }
            None => {
                self.tail = removed.prev;
            }
        }

        self.free_slots.push(node.index);
        self.len = self.len.saturating_sub(1);
        Some(removed.event)
    }

    fn next_slot_index(&mut self) -> usize {
        match self.free_slots.pop() {
            Some(index) if index < self.slots.len() => index,
            _ => {
                self.slots.push(QueuedEventSlot {
                    generation: 0,
                    node: None,
                });
                self.slots.len().saturating_sub(1)
            }
        }
    }

    fn next_generation(&mut self, index: usize) -> u64 {
        let Some(slot) = self.slots.get_mut(index) else {
            return 0;
        };

        slot.generation = slot.generation.wrapping_add(1);
        if slot.generation == 0 {
            slot.generation = 1;
        }
        slot.generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::domain::TerminalFailure;

    fn fill_control_queue(queue: &PtyEventQueue) {
        for index in 0..MAX_PENDING_EVENT_COUNT {
            assert!(queue.push(TerminalEvent::ChildExited {
                code: Some(index as u32),
            }));
        }
    }

    #[test]
    fn drain_merges_consecutive_output_chunks() {
        let queue = PtyEventQueue::new();

        assert!(queue.push_output(b"hello "));
        assert!(queue.push_output(b"world"));

        let events = queue.drain();

        assert_eq!(events.len(), 1);
        assert!(matches!(
            events.as_slice(),
            [TerminalEvent::PtyOutput(bytes)] if bytes.as_slice() == b"hello world"
        ));
    }

    #[test]
    fn drain_merges_read_sized_output_chunks_to_node_limit() {
        let queue = PtyEventQueue::new();
        let chunk = vec![b'x'; super::super::READ_BUFFER_SIZE];

        for _ in 0..(MAX_OUTPUT_NODE_BYTES / super::super::READ_BUFFER_SIZE) {
            assert!(queue.push_output(&chunk));
        }

        let events = queue.drain();

        assert_eq!(events.len(), 1);
        assert!(matches!(
            events.as_slice(),
            [TerminalEvent::PtyOutput(bytes)]
                if bytes.len() == MAX_OUTPUT_NODE_BYTES
                    && bytes.iter().all(|byte| *byte == b'x')
        ));
    }

    #[test]
    fn drain_splits_reader_output_at_node_byte_limit() {
        let queue = PtyEventQueue::new();
        let first_chunk = vec![b'a'; MAX_OUTPUT_NODE_BYTES - 1];

        assert!(queue.push_output(&first_chunk));
        assert!(queue.push_output(b"bc"));

        let events = queue.drain();

        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0],
            TerminalEvent::PtyOutput(bytes)
                if bytes.len() == MAX_OUTPUT_NODE_BYTES - 1
                    && bytes.iter().all(|byte| *byte == b'a')
        ));
        assert!(matches!(
            &events[1],
            TerminalEvent::PtyOutput(bytes) if bytes.as_slice() == b"bc"
        ));
    }

    #[test]
    fn reader_output_chunks_preserve_pending_byte_drop_accounting() {
        const EXTRA_OUTPUTS: usize = 10;
        let queue = PtyEventQueue::new();
        let retained_output_count = MAX_PENDING_OUTPUT_BYTES / MAX_OUTPUT_NODE_BYTES;

        for index in 0..(retained_output_count + EXTRA_OUTPUTS) {
            let mut bytes = vec![0; MAX_OUTPUT_NODE_BYTES];
            bytes[0] = (index % 256) as u8;
            assert!(queue.push_output(&bytes));
        }

        let events = queue.drain();

        assert_eq!(events.len(), retained_output_count + 1);
        assert!(matches!(
            events.first(),
            Some(TerminalEvent::PtyOutputDropped { byte_count })
                if *byte_count == EXTRA_OUTPUTS * MAX_OUTPUT_NODE_BYTES
        ));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, TerminalEvent::PtyOutput(_)))
                .count(),
            retained_output_count
        );
        assert!(matches!(
            events.get(1),
            Some(TerminalEvent::PtyOutput(bytes))
                if bytes.first().copied() == Some((EXTRA_OUTPUTS % 256) as u8)
        ));
    }

    #[test]
    fn budgeted_drain_keeps_undrained_output_pending() {
        let queue = PtyEventQueue::new();

        assert!(queue.push_output(&vec![b'a'; MAX_OUTPUT_NODE_BYTES]));
        assert!(queue.push_output(&vec![b'b'; MAX_OUTPUT_NODE_BYTES]));
        assert!(queue.push_output(&vec![b'c'; MAX_OUTPUT_NODE_BYTES]));

        let first = queue.drain_with_budget(PtyEventDrainBudget::new(
            MAX_PENDING_EVENT_COUNT,
            MAX_OUTPUT_NODE_BYTES * 2,
        ));
        let second = queue.drain();

        assert_eq!(first.len(), 2);
        assert!(matches!(
            &first[0],
            TerminalEvent::PtyOutput(bytes)
                if bytes.len() == MAX_OUTPUT_NODE_BYTES
                    && bytes.iter().all(|byte| *byte == b'a')
        ));
        assert!(matches!(
            &first[1],
            TerminalEvent::PtyOutput(bytes)
                if bytes.len() == MAX_OUTPUT_NODE_BYTES
                    && bytes.iter().all(|byte| *byte == b'b')
        ));
        assert_eq!(second.len(), 1);
        assert!(matches!(
            &second[0],
            TerminalEvent::PtyOutput(bytes)
                if bytes.len() == MAX_OUTPUT_NODE_BYTES
                    && bytes.iter().all(|byte| *byte == b'c')
        ));
    }

    #[test]
    fn budgeted_drain_counts_output_drop_marker_against_event_budget() {
        let queue = PtyEventQueue::new();
        let retained_output_count = MAX_PENDING_OUTPUT_BYTES / MAX_OUTPUT_NODE_BYTES;

        for _ in 0..(retained_output_count + 1) {
            assert!(queue.push_output(&vec![b'x'; MAX_OUTPUT_NODE_BYTES]));
        }

        let first = queue.drain_with_budget(PtyEventDrainBudget::new(1, MAX_PENDING_OUTPUT_BYTES));
        let second = queue.drain();

        assert!(matches!(
            first.as_slice(),
            [TerminalEvent::PtyOutputDropped { byte_count }]
                if *byte_count == MAX_OUTPUT_NODE_BYTES
        ));
        assert_eq!(
            second
                .iter()
                .filter(|event| matches!(event, TerminalEvent::PtyOutput(_)))
                .count(),
            retained_output_count
        );
    }

    #[test]
    fn drain_keeps_control_event_boundaries_between_output_chunks() {
        let queue = PtyEventQueue::new();

        assert!(queue.push_output(b"before"));
        assert!(queue.push(TerminalEvent::PtyClosed));
        assert!(queue.push_output(b"after"));

        let events = queue.drain();

        assert_eq!(events.len(), 3);
        assert!(matches!(
            &events[0],
            TerminalEvent::PtyOutput(bytes) if bytes.as_slice() == b"before"
        ));
        assert!(matches!(events[1], TerminalEvent::PtyClosed));
        assert!(matches!(
            &events[2],
            TerminalEvent::PtyOutput(bytes) if bytes.as_slice() == b"after"
        ));
    }

    #[test]
    fn pty_output_event_reports_drop_when_control_only_queue_is_full() {
        let queue = PtyEventQueue::new();
        let bytes = vec![b'x'; MAX_OUTPUT_NODE_BYTES];
        fill_control_queue(&queue);

        assert!(!queue.push(TerminalEvent::PtyOutput(bytes)));

        let events = queue.drain();

        assert_eq!(events.len(), MAX_PENDING_EVENT_COUNT + 1);
        assert!(matches!(
            events.first(),
            Some(TerminalEvent::PtyOutputDropped { byte_count })
                if *byte_count == MAX_OUTPUT_NODE_BYTES
        ));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, TerminalEvent::PtyOutput(_)))
        );
        assert!(matches!(
            events.get(1),
            Some(TerminalEvent::ChildExited { code: Some(0) })
        ));
    }

    #[test]
    fn reader_output_reports_drop_when_control_only_queue_is_full() {
        let queue = PtyEventQueue::new();
        let bytes = b"lost output";
        fill_control_queue(&queue);

        assert!(!queue.push_output(bytes));

        let events = queue.drain();

        assert_eq!(events.len(), MAX_PENDING_EVENT_COUNT + 1);
        assert!(matches!(
            events.first(),
            Some(TerminalEvent::PtyOutputDropped { byte_count })
                if *byte_count == bytes.len()
        ));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, TerminalEvent::PtyOutput(_)))
        );
    }

    #[test]
    fn control_events_are_rejected_when_control_only_queue_is_full() {
        let queue = PtyEventQueue::new();
        let failure = TerminalFailure::new("read failed", "reader error");
        fill_control_queue(&queue);

        assert!(!queue.push(TerminalEvent::PtyClosed));
        assert!(!queue.push(TerminalEvent::Failure(failure.clone())));
        for _ in 0..MAX_PENDING_EVENT_COUNT {
            assert!(!queue.push(TerminalEvent::Failure(failure.clone())));
        }

        let events = queue.drain();

        assert_eq!(events.len(), MAX_PENDING_EVENT_COUNT);
        assert!(matches!(
            events.first(),
            Some(TerminalEvent::ChildExited { code: Some(0) })
        ));
        assert!(matches!(
            events.last(),
            Some(TerminalEvent::ChildExited { code: Some(code) })
                if *code == (MAX_PENDING_EVENT_COUNT - 1) as u32
        ));
    }
}
