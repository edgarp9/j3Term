use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll as AlacrittyScroll};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{self, Handler, Processor};

use crate::app::TerminalViewportPort;
use crate::domain::{
    CursorPosition, TerminalCell, TerminalScroll, TerminalScrollState, TerminalSize,
    TerminalViewport,
};
use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, Copy)]
struct GridSize {
    rows: usize,
    columns: usize,
}

impl From<TerminalSize> for GridSize {
    fn from(size: TerminalSize) -> Self {
        Self {
            rows: usize::from(size.rows),
            columns: usize::from(size.columns),
        }
    }
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DirtyRows {
    start: usize,
    end: usize,
}

impl DirtyRows {
    fn all(rows: usize) -> Option<Self> {
        if rows == 0 {
            None
        } else {
            Some(Self {
                start: 0,
                end: rows,
            })
        }
    }

    fn from_inclusive(rows: usize, start: usize, end: usize) -> Option<Self> {
        if rows == 0 {
            return None;
        }

        let last_row = rows.saturating_sub(1);
        Some(Self {
            start: start.min(last_row),
            end: end.min(last_row).saturating_add(1),
        })
    }

    fn merge(self, other: Self) -> Self {
        Self {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    fn range(self) -> Range<usize> {
        self.start..self.end
    }
}

#[derive(Clone, Default)]
struct TerminalEventSink {
    pending_pty_writes: Rc<RefCell<Vec<Vec<u8>>>>,
}

impl TerminalEventSink {
    fn take_pending_pty_writes(&self) -> Vec<Vec<u8>> {
        let mut pending = self.pending_pty_writes.borrow_mut();
        std::mem::take(&mut *pending)
    }
}

impl EventListener for TerminalEventSink {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(text) = event {
            self.pending_pty_writes.borrow_mut().push(text.into_bytes());
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OutputEffects {
    byte_len: usize,
    screen_cell_len: usize,
    requires_full_refresh: bool,
    may_change_visible_cells: bool,
    observed_parser_action: bool,
    needs_byte_len_refresh_estimate: bool,
}

impl OutputEffects {
    fn new(byte_len: usize) -> Self {
        Self {
            byte_len,
            screen_cell_len: 0,
            requires_full_refresh: false,
            may_change_visible_cells: false,
            observed_parser_action: false,
            needs_byte_len_refresh_estimate: false,
        }
    }

    #[cfg(test)]
    fn local(byte_len: usize) -> Self {
        Self {
            byte_len,
            screen_cell_len: byte_len,
            requires_full_refresh: false,
            may_change_visible_cells: true,
            observed_parser_action: true,
            needs_byte_len_refresh_estimate: false,
        }
    }

    fn non_local(byte_len: usize) -> Self {
        Self {
            byte_len,
            screen_cell_len: 0,
            requires_full_refresh: true,
            may_change_visible_cells: false,
            observed_parser_action: true,
            needs_byte_len_refresh_estimate: false,
        }
    }

    fn finish(self) -> Self {
        if self.byte_len == 0 || self.observed_parser_action {
            self
        } else {
            Self::non_local(self.byte_len)
        }
    }

    #[cfg(test)]
    fn cursor_only(byte_len: usize) -> Self {
        Self {
            byte_len,
            screen_cell_len: 0,
            requires_full_refresh: false,
            may_change_visible_cells: false,
            observed_parser_action: true,
            needs_byte_len_refresh_estimate: false,
        }
    }

    fn note_cell_change(&mut self) {
        self.observed_parser_action = true;
        self.may_change_visible_cells = true;
    }

    fn note_untracked_cell_change(&mut self) {
        self.note_cell_change();
        self.needs_byte_len_refresh_estimate = true;
    }

    fn note_input(&mut self, screen_cell_len: usize) {
        self.note_cell_change();
        self.screen_cell_len = self.screen_cell_len.saturating_add(screen_cell_len);
    }

    fn note_cursor_only_control(&mut self) {
        self.observed_parser_action = true;
    }

    fn note_render_ignored_control(&mut self) {
        self.observed_parser_action = true;
    }

    fn note_non_local_control(&mut self) {
        self.observed_parser_action = true;
        self.requires_full_refresh = true;
    }

    fn refresh_cell_len(self) -> usize {
        if self.needs_byte_len_refresh_estimate {
            self.byte_len
        } else {
            self.screen_cell_len
        }
    }
}

struct OutputTrackingHandler<'a> {
    term: &'a mut Term<TerminalEventSink>,
    output_effects: &'a mut OutputEffects,
}

impl OutputTrackingHandler<'_> {
    fn input_screen_cell_len(
        before: Point,
        before_needs_wrap: bool,
        after: Point,
        after_needs_wrap: bool,
        columns: usize,
    ) -> usize {
        if columns == 0 {
            return 0;
        }

        let last_column = columns.saturating_sub(1);
        let before_column = before.column.0.min(last_column);
        let after_column = after.column.0.min(last_column);
        if after.line.0 > before.line.0 {
            let line_delta = (after.line.0 - before.line.0) as usize;
            return line_delta
                .saturating_mul(columns)
                .saturating_add(after_column)
                .saturating_sub(before_column);
        }

        if after.line == before.line && after_needs_wrap && after_column >= before_column {
            return columns.saturating_sub(before_column);
        }

        if after.line == before.line && after_column > before_column {
            return after_column - before_column;
        }

        if before_needs_wrap {
            return columns
                .saturating_sub(before_column)
                .saturating_add(after_column);
        }

        usize::from(after.line == before.line)
    }
}

macro_rules! delegate_local {
    ($handler:ident, $method:ident($($arg:expr),* $(,)?)) => {{
        $handler.output_effects.note_untracked_cell_change();
        <Term<TerminalEventSink> as Handler>::$method($handler.term, $($arg),*);
    }};
}

macro_rules! delegate_cursor_only {
    ($handler:ident, $method:ident($($arg:expr),* $(,)?)) => {{
        $handler.output_effects.note_cursor_only_control();
        <Term<TerminalEventSink> as Handler>::$method($handler.term, $($arg),*);
    }};
}

macro_rules! delegate_render_ignored {
    ($handler:ident, $method:ident($($arg:expr),* $(,)?)) => {{
        $handler.output_effects.note_render_ignored_control();
        <Term<TerminalEventSink> as Handler>::$method($handler.term, $($arg),*);
    }};
}

macro_rules! delegate_non_local {
    ($handler:ident, $method:ident($($arg:expr),* $(,)?)) => {{
        $handler.output_effects.note_non_local_control();
        <Term<TerminalEventSink> as Handler>::$method($handler.term, $($arg),*);
    }};
}

impl Handler for OutputTrackingHandler<'_> {
    fn set_title(&mut self, title: Option<String>) {
        delegate_render_ignored!(self, set_title(title));
    }

    fn set_cursor_style(&mut self, style: Option<ansi::CursorStyle>) {
        delegate_render_ignored!(self, set_cursor_style(style));
    }

    fn set_cursor_shape(&mut self, shape: ansi::CursorShape) {
        delegate_render_ignored!(self, set_cursor_shape(shape));
    }

    fn input(&mut self, character: char) {
        let before = self.term.grid().cursor.point;
        let before_needs_wrap = self.term.grid().cursor.input_needs_wrap;
        <Term<TerminalEventSink> as Handler>::input(self.term, character);
        let after = self.term.grid().cursor.point;
        let after_needs_wrap = self.term.grid().cursor.input_needs_wrap;
        let screen_cell_len = Self::input_screen_cell_len(
            before,
            before_needs_wrap,
            after,
            after_needs_wrap,
            self.term.columns(),
        );
        self.output_effects.note_input(screen_cell_len);
    }

    fn goto(&mut self, line: i32, column: usize) {
        delegate_cursor_only!(self, goto(line, column));
    }

    fn goto_line(&mut self, line: i32) {
        delegate_cursor_only!(self, goto_line(line));
    }

    fn goto_col(&mut self, column: usize) {
        delegate_cursor_only!(self, goto_col(column));
    }

    fn insert_blank(&mut self, count: usize) {
        delegate_non_local!(self, insert_blank(count));
    }

    fn move_up(&mut self, rows: usize) {
        delegate_cursor_only!(self, move_up(rows));
    }

    fn move_down(&mut self, rows: usize) {
        delegate_cursor_only!(self, move_down(rows));
    }

    fn identify_terminal(&mut self, intermediate: Option<char>) {
        delegate_non_local!(self, identify_terminal(intermediate));
    }

    fn device_status(&mut self, status: usize) {
        delegate_non_local!(self, device_status(status));
    }

    fn move_forward(&mut self, columns: usize) {
        delegate_cursor_only!(self, move_forward(columns));
    }

    fn move_backward(&mut self, columns: usize) {
        delegate_cursor_only!(self, move_backward(columns));
    }

    fn move_down_and_cr(&mut self, rows: usize) {
        delegate_cursor_only!(self, move_down_and_cr(rows));
    }

    fn move_up_and_cr(&mut self, rows: usize) {
        delegate_cursor_only!(self, move_up_and_cr(rows));
    }

    fn put_tab(&mut self, count: u16) {
        delegate_local!(self, put_tab(count));
    }

    fn backspace(&mut self) {
        delegate_local!(self, backspace());
    }

    fn carriage_return(&mut self) {
        delegate_local!(self, carriage_return());
    }

    fn linefeed(&mut self) {
        delegate_local!(self, linefeed());
    }

    fn bell(&mut self) {
        delegate_non_local!(self, bell());
    }

    fn substitute(&mut self) {
        delegate_non_local!(self, substitute());
    }

    fn newline(&mut self) {
        delegate_non_local!(self, newline());
    }

    fn set_horizontal_tabstop(&mut self) {
        delegate_non_local!(self, set_horizontal_tabstop());
    }

    fn scroll_up(&mut self, rows: usize) {
        delegate_non_local!(self, scroll_up(rows));
    }

    fn scroll_down(&mut self, rows: usize) {
        delegate_non_local!(self, scroll_down(rows));
    }

    fn insert_blank_lines(&mut self, count: usize) {
        delegate_non_local!(self, insert_blank_lines(count));
    }

    fn delete_lines(&mut self, count: usize) {
        delegate_non_local!(self, delete_lines(count));
    }

    fn erase_chars(&mut self, count: usize) {
        delegate_non_local!(self, erase_chars(count));
    }

    fn delete_chars(&mut self, count: usize) {
        delegate_non_local!(self, delete_chars(count));
    }

    fn move_backward_tabs(&mut self, count: u16) {
        delegate_non_local!(self, move_backward_tabs(count));
    }

    fn move_forward_tabs(&mut self, count: u16) {
        delegate_non_local!(self, move_forward_tabs(count));
    }

    fn save_cursor_position(&mut self) {
        delegate_non_local!(self, save_cursor_position());
    }

    fn restore_cursor_position(&mut self) {
        delegate_non_local!(self, restore_cursor_position());
    }

    fn clear_line(&mut self, mode: ansi::LineClearMode) {
        delegate_non_local!(self, clear_line(mode));
    }

    fn clear_screen(&mut self, mode: ansi::ClearMode) {
        delegate_non_local!(self, clear_screen(mode));
    }

    fn clear_tabs(&mut self, mode: ansi::TabulationClearMode) {
        delegate_non_local!(self, clear_tabs(mode));
    }

    fn set_tabs(&mut self, interval: u16) {
        delegate_non_local!(self, set_tabs(interval));
    }

    fn reset_state(&mut self) {
        delegate_non_local!(self, reset_state());
    }

    fn reverse_index(&mut self) {
        delegate_non_local!(self, reverse_index());
    }

    fn terminal_attribute(&mut self, attr: ansi::Attr) {
        delegate_render_ignored!(self, terminal_attribute(attr));
    }

    fn set_mode(&mut self, mode: ansi::Mode) {
        delegate_non_local!(self, set_mode(mode));
    }

    fn unset_mode(&mut self, mode: ansi::Mode) {
        delegate_non_local!(self, unset_mode(mode));
    }

    fn report_mode(&mut self, mode: ansi::Mode) {
        delegate_non_local!(self, report_mode(mode));
    }

    fn set_private_mode(&mut self, mode: ansi::PrivateMode) {
        delegate_non_local!(self, set_private_mode(mode));
    }

    fn unset_private_mode(&mut self, mode: ansi::PrivateMode) {
        delegate_non_local!(self, unset_private_mode(mode));
    }

    fn report_private_mode(&mut self, mode: ansi::PrivateMode) {
        delegate_non_local!(self, report_private_mode(mode));
    }

    fn set_scrolling_region(&mut self, top: usize, bottom: Option<usize>) {
        delegate_non_local!(self, set_scrolling_region(top, bottom));
    }

    fn set_keypad_application_mode(&mut self) {
        delegate_non_local!(self, set_keypad_application_mode());
    }

    fn unset_keypad_application_mode(&mut self) {
        delegate_non_local!(self, unset_keypad_application_mode());
    }

    fn set_active_charset(&mut self, index: ansi::CharsetIndex) {
        delegate_non_local!(self, set_active_charset(index));
    }

    fn configure_charset(&mut self, index: ansi::CharsetIndex, charset: ansi::StandardCharset) {
        delegate_non_local!(self, configure_charset(index, charset));
    }

    fn set_color(&mut self, index: usize, color: ansi::Rgb) {
        delegate_render_ignored!(self, set_color(index, color));
    }

    fn dynamic_color_sequence(&mut self, prefix: String, index: usize, terminator: &str) {
        delegate_render_ignored!(self, dynamic_color_sequence(prefix, index, terminator));
    }

    fn reset_color(&mut self, index: usize) {
        delegate_render_ignored!(self, reset_color(index));
    }

    fn clipboard_store(&mut self, clipboard: u8, base64: &[u8]) {
        delegate_non_local!(self, clipboard_store(clipboard, base64));
    }

    fn clipboard_load(&mut self, clipboard: u8, terminator: &str) {
        delegate_non_local!(self, clipboard_load(clipboard, terminator));
    }

    fn decaln(&mut self) {
        delegate_non_local!(self, decaln());
    }

    fn push_title(&mut self) {
        delegate_non_local!(self, push_title());
    }

    fn pop_title(&mut self) {
        delegate_non_local!(self, pop_title());
    }

    fn text_area_size_pixels(&mut self) {
        delegate_non_local!(self, text_area_size_pixels());
    }

    fn text_area_size_chars(&mut self) {
        delegate_non_local!(self, text_area_size_chars());
    }

    fn set_hyperlink(&mut self, hyperlink: Option<ansi::Hyperlink>) {
        delegate_render_ignored!(self, set_hyperlink(hyperlink));
    }

    fn set_mouse_cursor_icon(&mut self, icon: ansi::cursor_icon::CursorIcon) {
        delegate_non_local!(self, set_mouse_cursor_icon(icon));
    }

    fn report_keyboard_mode(&mut self) {
        delegate_non_local!(self, report_keyboard_mode());
    }

    fn push_keyboard_mode(&mut self, mode: ansi::KeyboardModes) {
        delegate_non_local!(self, push_keyboard_mode(mode));
    }

    fn pop_keyboard_modes(&mut self, to_pop: u16) {
        delegate_non_local!(self, pop_keyboard_modes(to_pop));
    }

    fn set_keyboard_mode(
        &mut self,
        mode: ansi::KeyboardModes,
        behavior: ansi::KeyboardModesApplyBehavior,
    ) {
        delegate_non_local!(self, set_keyboard_mode(mode, behavior));
    }

    fn set_modify_other_keys(&mut self, mode: ansi::ModifyOtherKeys) {
        delegate_non_local!(self, set_modify_other_keys(mode));
    }

    fn report_modify_other_keys(&mut self) {
        delegate_non_local!(self, report_modify_other_keys());
    }

    fn set_scp(&mut self, char_path: ansi::ScpCharPath, update_mode: ansi::ScpUpdateMode) {
        delegate_non_local!(self, set_scp(char_path, update_mode));
    }
}

pub struct AlacrittyTerminalBuffer {
    term: Term<TerminalEventSink>,
    parser: Processor,
    event_sink: TerminalEventSink,
    dirty_rows: Option<DirtyRows>,
}

impl AlacrittyTerminalBuffer {
    pub fn new(size: TerminalSize) -> Self {
        let grid_size = GridSize::from(size);
        let event_sink = TerminalEventSink::default();
        Self {
            term: Term::new(Config::default(), &grid_size, event_sink.clone()),
            parser: Processor::new(),
            event_sink,
            dirty_rows: DirtyRows::all(grid_size.rows),
        }
    }

    fn cell_count(rows: usize, columns: usize) -> AppResult<usize> {
        rows.checked_mul(columns).ok_or(AppError::InvalidInput(
            "terminal buffer dimensions are too large",
        ))
    }

    fn snapshot_cells(&self, rows: usize, columns: usize) -> AppResult<Vec<TerminalCell>> {
        let cell_count = Self::cell_count(rows, columns)?;
        let mut cells = Vec::new();
        cells
            .try_reserve(cell_count)
            .map_err(|_| AppError::InvalidInput("terminal buffer snapshot is too large"))?;

        self.append_display_snapshot_rows(0, rows, columns, &mut cells)?;
        Ok(cells)
    }

    fn append_display_snapshot_rows(
        &self,
        start_row: usize,
        end_row: usize,
        columns: usize,
        cells: &mut Vec<TerminalCell>,
    ) -> AppResult<()> {
        let grid = self.term.grid();
        let display_offset = grid.display_offset();

        for row in start_row..end_row {
            let display_line = Self::display_line(row, display_offset)?;
            Self::validate_display_line(display_line, grid)?;

            for column in 0..columns {
                cells.push(Self::terminal_cell(&grid[display_line][Column(column)]));
            }
        }

        Ok(())
    }

    fn copy_snapshot_rows(
        &self,
        viewport: &mut TerminalViewport,
        dirty_rows: DirtyRows,
    ) -> AppResult<()> {
        let grid = self.term.grid();
        let display_offset = grid.display_offset();

        for row in dirty_rows.start..dirty_rows.end {
            let display_line = Self::display_line(row, display_offset)?;
            Self::validate_display_line(display_line, grid)?;

            let row_start = row
                .checked_mul(viewport.columns)
                .ok_or(AppError::InvalidInput("terminal row index is too large"))?;
            let row_end = row_start
                .checked_add(viewport.columns)
                .ok_or(AppError::InvalidInput("terminal cell index is too large"))?;
            let target_row = viewport
                .cells
                .get_mut(row_start..row_end)
                .ok_or(AppError::InvalidState("terminal viewport cache is invalid"))?;
            let source_row = &grid[display_line];

            for (column, target) in target_row.iter_mut().enumerate() {
                *target = Self::terminal_cell(&source_row[Column(column)]);
            }
        }

        Ok(())
    }

    fn display_line(row: usize, display_offset: usize) -> AppResult<Line> {
        let row = i32::try_from(row)
            .map_err(|_| AppError::InvalidInput("terminal row index is too large"))?;
        let display_offset = i32::try_from(display_offset)
            .map_err(|_| AppError::InvalidInput("terminal display offset is too large"))?;
        let line = row
            .checked_sub(display_offset)
            .ok_or(AppError::InvalidInput("terminal display line is too large"))?;
        Ok(Line(line))
    }

    fn validate_display_line(line: Line, grid: &impl Dimensions) -> AppResult<()> {
        if line < grid.topmost_line() || line > grid.bottommost_line() {
            return Err(AppError::InvalidState(
                "terminal display row is outside grid",
            ));
        }

        Ok(())
    }

    fn terminal_cell(cell: &Cell) -> TerminalCell {
        let character = if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER | Flags::HIDDEN)
        {
            ' '
        } else {
            cell.c
        };
        TerminalCell::new(character)
    }

    fn cursor_position(&self) -> AppResult<CursorPosition> {
        if self.term.grid().display_offset() != 0 {
            return Ok(CursorPosition::new(usize::MAX, usize::MAX));
        }

        let cursor = self.term.grid().cursor.point;
        let row = usize::try_from(cursor.line.0)
            .map_err(|_| AppError::InvalidState("terminal cursor row is negative"))?;
        Ok(CursorPosition::new(row, cursor.column.0))
    }

    fn scroll_state(&self) -> TerminalScrollState {
        let history_size = self.term.grid().history_size();
        let display_offset = self.term.grid().display_offset().min(history_size);
        let rows = self.term.screen_lines();
        TerminalScrollState::new(
            history_size.saturating_sub(display_offset),
            history_size,
            rows,
            history_size.saturating_add(rows),
        )
    }

    fn mark_all_dirty(&mut self) {
        self.dirty_rows = DirtyRows::all(self.term.screen_lines());
    }

    fn mark_dirty_range(&mut self, start: usize, end: usize) {
        let Some(rows) = DirtyRows::from_inclusive(self.term.screen_lines(), start, end) else {
            return;
        };
        self.dirty_rows = Some(match self.dirty_rows {
            Some(existing) => existing.merge(rows),
            None => rows,
        });
    }

    fn mark_dirty_after_output(
        &mut self,
        output_effects: OutputEffects,
        before: CursorPosition,
        after: CursorPosition,
        history_size_changed: bool,
    ) {
        if output_effects.byte_len == 0 {
            return;
        }

        let rows = self.term.screen_lines();
        let columns = self.term.columns();
        if history_size_changed {
            self.mark_all_dirty();
            return;
        }

        if Self::output_requires_full_refresh(output_effects, before, after, rows, columns) {
            self.mark_all_dirty();
            return;
        }

        if !output_effects.may_change_visible_cells {
            return;
        }

        if self.term.grid().display_offset() != 0 {
            self.mark_all_dirty();
            return;
        }

        self.mark_dirty_range(before.row.min(after.row), before.row.max(after.row));
    }

    fn cursor_requires_full_refresh(
        before: CursorPosition,
        after: CursorPosition,
        rows: usize,
    ) -> bool {
        after.row < before.row || before.row >= rows
    }

    fn output_cell_len_requires_full_refresh(
        cell_len: usize,
        before: CursorPosition,
        rows: usize,
        columns: usize,
    ) -> bool {
        if rows == 0 || columns == 0 || before.row >= rows {
            return false;
        }

        let column = before.column.min(columns.saturating_sub(1));
        let cells_until_bottom = rows
            .saturating_sub(before.row)
            .saturating_mul(columns)
            .saturating_sub(column);
        cell_len >= cells_until_bottom
    }

    fn output_requires_full_refresh(
        output_effects: OutputEffects,
        before: CursorPosition,
        after: CursorPosition,
        rows: usize,
        columns: usize,
    ) -> bool {
        if rows == 0 || columns == 0 {
            return false;
        }
        if output_effects.requires_full_refresh {
            return true;
        }
        if !output_effects.may_change_visible_cells {
            return false;
        }
        if Self::cursor_requires_full_refresh(before, after, rows) {
            return true;
        }
        if Self::output_cell_len_requires_full_refresh(
            output_effects.refresh_cell_len(),
            before,
            rows,
            columns,
        ) {
            return true;
        }
        false
    }
}

impl TerminalViewportPort for AlacrittyTerminalBuffer {
    fn ingest_output(&mut self, bytes: &[u8]) -> AppResult<()> {
        let before = self.cursor_position()?;
        let before_history_size = self.term.grid().history_size();
        let mut output_effects = OutputEffects::new(bytes.len());
        {
            let mut handler = OutputTrackingHandler {
                term: &mut self.term,
                output_effects: &mut output_effects,
            };
            self.parser.advance(&mut handler, bytes);
        }
        let output_effects = output_effects.finish();
        let after = self.cursor_position()?;
        let history_size_changed = before_history_size != self.term.grid().history_size();
        self.mark_dirty_after_output(output_effects, before, after, history_size_changed);
        Ok(())
    }

    fn take_pending_pty_writes(&mut self) -> AppResult<Vec<Vec<u8>>> {
        Ok(self.event_sink.take_pending_pty_writes())
    }

    fn resize(&mut self, size: TerminalSize) -> AppResult<()> {
        let grid_size = GridSize::from(size);
        if self.term.screen_lines() == grid_size.rows && self.term.columns() == grid_size.columns {
            return Ok(());
        }

        self.term.resize(grid_size);
        self.mark_all_dirty();
        Ok(())
    }

    fn scroll_display(&mut self, scroll: TerminalScroll) -> AppResult<bool> {
        let before = self.term.grid().display_offset();
        self.term.scroll_display(match scroll {
            TerminalScroll::Lines(lines) => AlacrittyScroll::Delta(lines),
            TerminalScroll::PageUp => AlacrittyScroll::PageUp,
            TerminalScroll::PageDown => AlacrittyScroll::PageDown,
            TerminalScroll::Absolute(position) => {
                let current = self.term.grid().display_offset();
                AlacrittyScroll::Delta(scroll_delta(current, position))
            }
            TerminalScroll::Top => AlacrittyScroll::Top,
            TerminalScroll::Bottom => AlacrittyScroll::Bottom,
        });
        let changed = before != self.term.grid().display_offset();
        if changed {
            self.mark_all_dirty();
        }
        Ok(changed)
    }

    fn snapshot(&mut self) -> AppResult<TerminalViewport> {
        let rows = self.term.screen_lines();
        let columns = self.term.columns();
        let cells = self.snapshot_cells(rows, columns)?;
        self.dirty_rows = None;
        TerminalViewport::with_scroll(
            rows,
            columns,
            cells,
            self.cursor_position()?,
            self.scroll_state(),
        )
    }

    fn snapshot_into(&mut self, viewport: &mut TerminalViewport) -> AppResult<()> {
        let rows = self.term.screen_lines();
        let columns = self.term.columns();
        let cell_count = Self::cell_count(rows, columns)?;
        if viewport.rows != rows
            || viewport.columns != columns
            || viewport.cells.len() != cell_count
        {
            *viewport = self.snapshot()?;
            viewport.set_changed_rows(DirtyRows::all(rows).map(DirtyRows::range));
            return Ok(());
        }

        let changed_rows = if let Some(dirty_rows) = self.dirty_rows {
            self.copy_snapshot_rows(viewport, dirty_rows)?;
            self.dirty_rows = None;
            Some(dirty_rows.range())
        } else {
            None
        };
        viewport.cursor = self.cursor_position()?;
        viewport.scroll = self.scroll_state();
        viewport.set_changed_rows(changed_rows);
        Ok(())
    }
}

fn scroll_delta(current: usize, target: usize) -> i32 {
    match i32::try_from(target.saturating_sub(current)) {
        Ok(delta) if target >= current => delta,
        Err(_) if target >= current => i32::MAX,
        _ => match i32::try_from(current.saturating_sub(target)) {
            Ok(delta) => delta.saturating_neg(),
            Err(_) => i32::MIN,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer_after(input: &[u8]) -> AppResult<TerminalViewport> {
        let size = TerminalSize::new(4, 16)?;
        let mut terminal = AlacrittyTerminalBuffer::new(size);
        terminal.ingest_output(input)?;
        terminal.snapshot()
    }

    fn line_text(viewport: &TerminalViewport, row: usize) -> Option<String> {
        let mut line = String::with_capacity(viewport.columns);
        for cell in viewport.line_cells(row)? {
            line.push(cell.character);
        }
        Some(line)
    }

    fn changed_row_ranges(
        rows: Option<crate::domain::terminal::TerminalChangedRows>,
    ) -> Option<Vec<(usize, usize)>> {
        rows.map(|rows| {
            rows.ranges()
                .iter()
                .map(|rows| (rows.start, rows.end))
                .collect()
        })
    }

    #[test]
    fn ingests_text_and_newlines_into_grid() -> AppResult<()> {
        let buffer = buffer_after(b"hello\r\nworld")?;

        assert_eq!(line_text(&buffer, 0).as_deref(), Some("hello           "));
        assert_eq!(line_text(&buffer, 1).as_deref(), Some("world           "));
        assert_eq!(buffer.cursor, CursorPosition::new(1, 5));
        Ok(())
    }

    #[test]
    fn applies_ansi_clear_and_cursor_home() -> AppResult<()> {
        let buffer = buffer_after(b"hello\x1b[2J\x1b[Hworld")?;

        assert_eq!(line_text(&buffer, 0).as_deref(), Some("world           "));
        assert_eq!(line_text(&buffer, 1).as_deref(), Some("                "));
        assert_eq!(buffer.cursor, CursorPosition::new(0, 5));
        Ok(())
    }

    #[test]
    fn tracks_cursor_position_escape_sequence() -> AppResult<()> {
        let buffer = buffer_after(b"\x1b[3;4H")?;

        assert_eq!(buffer.cursor, CursorPosition::new(2, 3));
        Ok(())
    }

    #[test]
    fn resize_updates_snapshot_dimensions_and_bounds_cursor() -> AppResult<()> {
        let initial_size = TerminalSize::new(2, 8)?;
        let mut terminal = AlacrittyTerminalBuffer::new(initial_size);
        terminal.ingest_output(b"hello")?;

        terminal.resize(TerminalSize::with_pixels(3, 12, 96, 48)?)?;

        let viewport = terminal.snapshot()?;
        assert_eq!(viewport.rows, 3);
        assert_eq!(viewport.columns, 12);
        assert!(viewport.cursor.row < viewport.rows);
        assert!(viewport.cursor.column < viewport.columns);
        Ok(())
    }

    #[test]
    fn resize_same_grid_keeps_dirty_rows_unchanged() -> AppResult<()> {
        let initial_size = TerminalSize::with_pixels(4, 16, 128, 64)?;
        let mut terminal = AlacrittyTerminalBuffer::new(initial_size);
        terminal.ingest_output(b"hello")?;
        terminal.snapshot()?;

        terminal.resize(TerminalSize::with_pixels(4, 16, 160, 64)?)?;

        assert_eq!(terminal.dirty_rows, None);
        let viewport = terminal.snapshot()?;
        assert_eq!(viewport.rows, 4);
        assert_eq!(viewport.columns, 16);
        Ok(())
    }

    #[test]
    fn snapshot_does_not_change_grid_or_cursor() -> AppResult<()> {
        let size = TerminalSize::new(4, 16)?;
        let mut terminal = AlacrittyTerminalBuffer::new(size);
        terminal.ingest_output(b"hello\r\nworld")?;

        let first = terminal.snapshot()?;
        let second = terminal.snapshot()?;

        assert_eq!(second, first);
        Ok(())
    }

    #[test]
    fn scroll_display_reads_from_scrollback_history() -> AppResult<()> {
        let size = TerminalSize::new(3, 16)?;
        let mut terminal = AlacrittyTerminalBuffer::new(size);
        terminal.ingest_output(b"line1\r\nline2\r\nline3\r\nline4\r\nline5")?;
        let bottom = terminal.snapshot()?;

        assert!(terminal.scroll_display(TerminalScroll::Lines(2))?);
        let scrolled = terminal.snapshot()?;

        assert_ne!(scrolled, bottom);
        assert!(scrolled.cursor.row >= scrolled.rows);
        assert!(terminal.scroll_display(TerminalScroll::Lines(-100))?);
        assert_eq!(terminal.snapshot()?, bottom);
        Ok(())
    }

    #[test]
    fn snapshot_into_reuses_viewport_cells_for_plain_output() -> AppResult<()> {
        let size = TerminalSize::new(4, 16)?;
        let mut terminal = AlacrittyTerminalBuffer::new(size);
        terminal.ingest_output(b"hello\r\nworld")?;
        let mut viewport = terminal.snapshot()?;
        let cells_ptr = viewport.cells.as_ptr();
        let cells_capacity = viewport.cells.capacity();

        terminal.ingest_output(b"!")?;
        terminal.snapshot_into(&mut viewport)?;

        assert_eq!(viewport.cells.as_ptr(), cells_ptr);
        assert_eq!(viewport.cells.capacity(), cells_capacity);
        assert_eq!(line_text(&viewport, 1).as_deref(), Some("world!          "));
        Ok(())
    }

    #[test]
    fn snapshot_into_records_changed_rows_for_plain_output() -> AppResult<()> {
        let size = TerminalSize::new(4, 16)?;
        let mut terminal = AlacrittyTerminalBuffer::new(size);
        terminal.ingest_output(b"hello\r\nworld")?;
        let mut viewport = terminal.snapshot()?;
        let baseline = viewport.change_baseline();

        terminal.ingest_output(b"!")?;
        terminal.snapshot_into(&mut viewport)?;

        assert_eq!(
            changed_row_ranges(viewport.changed_rows_since_baseline(&baseline)),
            Some(vec![(1, 2)])
        );
        Ok(())
    }

    #[test]
    fn snapshot_into_records_changed_rows_for_sgr_output() -> AppResult<()> {
        let size = TerminalSize::new(4, 16)?;
        let mut terminal = AlacrittyTerminalBuffer::new(size);
        terminal.ingest_output(b"hello\r\nworld")?;
        let mut viewport = terminal.snapshot()?;
        let baseline = viewport.change_baseline();

        terminal.ingest_output(b"\x1b[31m!\x1b[0m")?;
        terminal.snapshot_into(&mut viewport)?;

        assert_eq!(line_text(&viewport, 1).as_deref(), Some("world!          "));
        assert_eq!(
            changed_row_ranges(viewport.changed_rows_since_baseline(&baseline)),
            Some(vec![(1, 2)])
        );
        Ok(())
    }

    #[test]
    fn snapshot_into_ignores_pure_sgr_output_for_dirty_rows() -> AppResult<()> {
        let size = TerminalSize::new(4, 16)?;
        let mut terminal = AlacrittyTerminalBuffer::new(size);
        terminal.ingest_output(b"hello\r\nworld")?;
        let mut viewport = terminal.snapshot()?;
        let baseline = viewport.change_baseline();
        let row_versions = [
            viewport.row_version(0),
            viewport.row_version(1),
            viewport.row_version(2),
            viewport.row_version(3),
        ];

        terminal.ingest_output(b"\x1b[31m\x1b[1m\x1b[0m")?;
        assert_eq!(terminal.dirty_rows, None);
        terminal.snapshot_into(&mut viewport)?;

        assert_eq!(line_text(&viewport, 0).as_deref(), Some("hello           "));
        assert_eq!(line_text(&viewport, 1).as_deref(), Some("world           "));
        assert_eq!(viewport.changed_rows_since_baseline(&baseline), None);
        assert_eq!(
            [
                viewport.row_version(0),
                viewport.row_version(1),
                viewport.row_version(2),
                viewport.row_version(3),
            ],
            row_versions
        );
        Ok(())
    }

    #[test]
    fn snapshot_into_ignores_renderer_metadata_output_for_dirty_rows() -> AppResult<()> {
        let size = TerminalSize::new(4, 16)?;
        let mut terminal = AlacrittyTerminalBuffer::new(size);
        terminal.ingest_output(b"hello\r\nworld")?;
        let mut viewport = terminal.snapshot()?;
        let baseline = viewport.change_baseline();
        let row_versions = [
            viewport.row_version(0),
            viewport.row_version(1),
            viewport.row_version(2),
            viewport.row_version(3),
        ];

        terminal.ingest_output(b"\x1b]2;title\x07\x1b[5 q\x1b]4;1;rgb:ff/00/00\x07")?;
        assert_eq!(terminal.dirty_rows, None);
        terminal.snapshot_into(&mut viewport)?;

        assert_eq!(line_text(&viewport, 0).as_deref(), Some("hello           "));
        assert_eq!(line_text(&viewport, 1).as_deref(), Some("world           "));
        assert_eq!(viewport.changed_rows_since_baseline(&baseline), None);
        assert_eq!(
            [
                viewport.row_version(0),
                viewport.row_version(1),
                viewport.row_version(2),
                viewport.row_version(3),
            ],
            row_versions
        );
        Ok(())
    }

    #[test]
    fn snapshot_into_ignores_pure_sgr_output_while_scrolled_back() -> AppResult<()> {
        let size = TerminalSize::new(3, 16)?;
        let mut terminal = AlacrittyTerminalBuffer::new(size);
        terminal.ingest_output(b"line1\r\nline2\r\nline3\r\nline4\r\nline5")?;
        terminal.snapshot()?;
        assert!(terminal.scroll_display(TerminalScroll::Lines(2))?);
        let mut viewport = terminal.snapshot()?;
        let baseline = viewport.change_baseline();
        let row_versions = [
            viewport.row_version(0),
            viewport.row_version(1),
            viewport.row_version(2),
        ];

        terminal.ingest_output(b"\x1b[31m\x1b[0m")?;
        assert_eq!(terminal.dirty_rows, None);
        terminal.snapshot_into(&mut viewport)?;

        assert_eq!(line_text(&viewport, 0).as_deref(), Some("line1           "));
        assert_eq!(line_text(&viewport, 1).as_deref(), Some("line2           "));
        assert_eq!(viewport.changed_rows_since_baseline(&baseline), None);
        assert_eq!(
            [
                viewport.row_version(0),
                viewport.row_version(1),
                viewport.row_version(2),
            ],
            row_versions
        );
        Ok(())
    }

    #[test]
    fn output_cell_len_requires_full_refresh_at_visible_bottom_boundary() {
        let before = CursorPosition::new(1, 5);

        assert!(!AlacrittyTerminalBuffer::output_cell_len_requires_full_refresh(42, before, 4, 16));
        assert!(AlacrittyTerminalBuffer::output_cell_len_requires_full_refresh(43, before, 4, 16));
    }

    #[test]
    fn output_requires_full_refresh_accepts_plain_printable_fast_path() {
        let before = CursorPosition::new(1, 5);
        let after = CursorPosition::new(1, 10);

        assert!(!AlacrittyTerminalBuffer::output_requires_full_refresh(
            OutputEffects::local(b"hello".len()),
            before,
            after,
            4,
            16
        ));
        assert!(AlacrittyTerminalBuffer::output_requires_full_refresh(
            OutputEffects::local(1),
            CursorPosition::new(3, 15),
            CursorPosition::new(3, 15),
            4,
            16
        ));
    }

    #[test]
    fn output_requires_full_refresh_keeps_large_plain_output_local() {
        let before = CursorPosition::new(0, 0);
        let after = CursorPosition::new(32, 1);
        let output = vec![b'x'; (16 * 1024) + 1];

        assert!(
            !AlacrittyTerminalBuffer::output_cell_len_requires_full_refresh(
                output.len(),
                before,
                512,
                512
            )
        );
        assert!(!AlacrittyTerminalBuffer::output_requires_full_refresh(
            OutputEffects::local(output.len()),
            before,
            after,
            512,
            512
        ));
    }

    #[test]
    fn snapshot_into_records_changed_rows_for_large_plain_output_without_full_refresh()
    -> AppResult<()> {
        let size = TerminalSize::new(40, 512)?;
        let mut terminal = AlacrittyTerminalBuffer::new(size);
        let mut viewport = terminal.snapshot()?;
        let baseline = viewport.change_baseline();
        let output = vec![b'x'; (16 * 1024) + 1];

        terminal.ingest_output(&output)?;
        terminal.snapshot_into(&mut viewport)?;

        assert_eq!(
            changed_row_ranges(viewport.changed_rows_since_baseline(&baseline)),
            Some(vec![(0, 33)])
        );
        Ok(())
    }

    #[test]
    fn snapshot_into_records_changed_rows_for_large_multibyte_output_without_full_refresh()
    -> AppResult<()> {
        let size = TerminalSize::new(24, 80)?;
        let mut terminal = AlacrittyTerminalBuffer::new(size);
        let mut viewport = terminal.snapshot()?;
        let baseline = viewport.change_baseline();
        let output = "\u{d55c}".repeat(640);

        terminal.ingest_output(output.as_bytes())?;
        assert_eq!(terminal.dirty_rows, Some(DirtyRows { start: 0, end: 16 }));
        terminal.snapshot_into(&mut viewport)?;

        assert_eq!(
            changed_row_ranges(viewport.changed_rows_since_baseline(&baseline)),
            Some(vec![(0, 16)])
        );
        Ok(())
    }

    #[test]
    fn snapshot_into_records_changed_rows_for_large_plain_output() -> AppResult<()> {
        let size = TerminalSize::new(4, 512)?;
        let mut terminal = AlacrittyTerminalBuffer::new(size);
        let mut viewport = terminal.snapshot()?;
        let baseline = viewport.change_baseline();
        let output = vec![b'x'; 1100];

        terminal.ingest_output(&output)?;
        terminal.snapshot_into(&mut viewport)?;

        assert_eq!(
            changed_row_ranges(viewport.changed_rows_since_baseline(&baseline)),
            Some(vec![(0, 3)])
        );
        Ok(())
    }

    #[test]
    fn output_requires_full_refresh_keeps_non_sgr_escape_conservative() {
        let before = CursorPosition::new(0, 0);
        let after = CursorPosition::new(0, 8);

        assert!(AlacrittyTerminalBuffer::output_requires_full_refresh(
            OutputEffects::non_local(b"\x1b[2J\x1b[8C".len()),
            before,
            after,
            4,
            16
        ));
    }

    #[test]
    fn output_requires_full_refresh_skips_cursor_only_escape() {
        let before = CursorPosition::new(0, 5);
        let after = CursorPosition::new(0, 13);

        assert!(!AlacrittyTerminalBuffer::output_requires_full_refresh(
            OutputEffects::cursor_only(b"\x1b[8C".len()),
            before,
            after,
            4,
            16
        ));
    }

    #[test]
    fn snapshot_into_skips_row_copy_for_cursor_only_escape() -> AppResult<()> {
        let size = TerminalSize::new(4, 16)?;
        let mut terminal = AlacrittyTerminalBuffer::new(size);
        terminal.ingest_output(b"hello")?;
        let mut viewport = terminal.snapshot()?;
        let baseline = viewport.change_baseline();
        let row_versions: Vec<_> = (0..viewport.rows)
            .map(|row| viewport.row_version(row))
            .collect();

        terminal.ingest_output(b"\x1b[8C")?;
        terminal.snapshot_into(&mut viewport)?;

        assert_eq!(viewport.cursor, CursorPosition::new(0, 13));
        assert_eq!(
            changed_row_ranges(viewport.changed_rows_since_baseline(&baseline)),
            Some(vec![(0, 1)])
        );
        assert_eq!(
            (0..viewport.rows)
                .map(|row| viewport.row_version(row))
                .collect::<Vec<_>>(),
            row_versions
        );
        Ok(())
    }

    #[test]
    fn snapshot_into_copies_visible_scrollback_rows() -> AppResult<()> {
        let size = TerminalSize::new(3, 16)?;
        let mut terminal = AlacrittyTerminalBuffer::new(size);
        terminal.ingest_output(b"line1\r\nline2\r\nline3\r\nline4\r\nline5")?;
        let mut viewport = terminal.snapshot()?;
        let bottom = viewport.clone();

        assert!(terminal.scroll_display(TerminalScroll::Lines(2))?);
        terminal.snapshot_into(&mut viewport)?;
        let expected = terminal.snapshot()?;

        assert_ne!(viewport, bottom);
        assert_eq!(viewport, expected);
        Ok(())
    }

    #[test]
    fn snapshot_into_records_no_changed_rows_without_output() -> AppResult<()> {
        let size = TerminalSize::new(4, 16)?;
        let mut terminal = AlacrittyTerminalBuffer::new(size);
        terminal.ingest_output(b"hello\r\nworld")?;
        let mut viewport = terminal.snapshot()?;
        let baseline = viewport.change_baseline();

        terminal.snapshot_into(&mut viewport)?;

        assert_eq!(viewport.changed_rows_since_baseline(&baseline), None);
        Ok(())
    }

    #[test]
    fn snapshot_into_refreshes_full_viewport_for_escape_output() -> AppResult<()> {
        let size = TerminalSize::new(4, 16)?;
        let mut terminal = AlacrittyTerminalBuffer::new(size);
        terminal.ingest_output(b"hello\r\nworld")?;
        let mut viewport = terminal.snapshot()?;
        let baseline = viewport.change_baseline();

        terminal.ingest_output(b"\x1b[2J\x1b[Hdone")?;
        terminal.snapshot_into(&mut viewport)?;

        assert_eq!(line_text(&viewport, 0).as_deref(), Some("done            "));
        assert_eq!(line_text(&viewport, 1).as_deref(), Some("                "));
        assert_eq!(viewport.cursor, CursorPosition::new(0, 4));
        assert_eq!(
            changed_row_ranges(viewport.changed_rows_since_baseline(&baseline)),
            Some(vec![(0, 4)])
        );
        Ok(())
    }

    #[test]
    fn emits_terminal_response_for_cursor_position_request() -> AppResult<()> {
        let size = TerminalSize::new(4, 16)?;
        let mut terminal = AlacrittyTerminalBuffer::new(size);

        terminal.ingest_output(b"\x1b[6n")?;

        assert_eq!(
            terminal.take_pending_pty_writes()?,
            vec![b"\x1b[1;1R".to_vec()]
        );
        Ok(())
    }
}
