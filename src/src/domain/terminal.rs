use std::ops::Range;

use crate::error::{AppError, AppResult};

pub const DEFAULT_COLUMNS: u16 = 80;
pub const DEFAULT_ROWS: u16 = 24;
pub const MIN_COLUMNS: u16 = 2;
pub const MIN_ROWS: u16 = 1;
pub const MAX_TERMINAL_TABS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorPosition {
    pub row: usize,
    pub column: usize,
}

impl CursorPosition {
    pub fn new(row: usize, column: usize) -> Self {
        Self { row, column }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalGridPoint {
    pub row: usize,
    pub column: usize,
}

impl TerminalGridPoint {
    pub fn new(row: usize, column: usize) -> Self {
        Self { row, column }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSelection {
    anchor: TerminalGridPoint,
    focus: TerminalGridPoint,
}

impl TerminalSelection {
    pub fn new(anchor: TerminalGridPoint, focus: TerminalGridPoint) -> Self {
        Self { anchor, focus }
    }

    pub fn is_empty(self) -> bool {
        self.anchor == self.focus
    }

    pub fn row_range(self, row: usize, columns: usize) -> Option<Range<usize>> {
        if self.is_empty() || columns == 0 {
            return None;
        }

        let (start, end) = self.ordered_points();
        if row < start.row || row > end.row {
            return None;
        }

        let start_column = if row == start.row {
            start.column.min(columns)
        } else {
            0
        };
        let end_column = if row == end.row {
            end.column.saturating_add(1).min(columns)
        } else {
            columns
        };

        non_empty_range(start_column, end_column)
    }

    fn ordered_points(self) -> (TerminalGridPoint, TerminalGridPoint) {
        if point_le(self.anchor, self.focus) {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalCell {
    pub character: char,
}

impl TerminalCell {
    pub fn new(character: char) -> Self {
        Self { character }
    }
}

impl Default for TerminalCell {
    fn default() -> Self {
        Self { character: ' ' }
    }
}

#[derive(Debug, Clone)]
pub struct TerminalViewport {
    pub rows: usize,
    pub columns: usize,
    pub cells: Vec<TerminalCell>,
    pub cursor: CursorPosition,
    pub scroll: TerminalScrollState,
    changed_rows: Option<Range<usize>>,
    row_versions: Vec<u64>,
}

impl PartialEq for TerminalViewport {
    fn eq(&self, other: &Self) -> bool {
        self.rows == other.rows
            && self.columns == other.columns
            && self.cells == other.cells
            && self.cursor == other.cursor
            && self.scroll == other.scroll
    }
}

impl Eq for TerminalViewport {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalViewportChangeBaseline {
    rows: usize,
    columns: usize,
    cell_count: usize,
    cursor: CursorPosition,
    scroll: TerminalScrollState,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalChangedRows {
    ranges: Vec<Range<usize>>,
}

impl TerminalChangedRows {
    pub fn ranges(&self) -> &[Range<usize>] {
        &self.ranges
    }

    pub fn merge(&mut self, other: &Self) {
        for rows in &other.ranges {
            self.include_clipped_range(rows.clone(), usize::MAX);
        }
    }

    fn from_range(rows: Range<usize>, visible_rows: usize) -> Option<Self> {
        let mut changed = Self::default();
        changed.include_range(Some(rows), visible_rows);
        changed.into_option()
    }

    fn include(&mut self, row: usize) {
        self.include_clipped_range(row..row.saturating_add(1), usize::MAX);
    }

    fn include_if_visible(&mut self, row: usize, rows: usize) {
        if row < rows {
            self.include(row);
        }
    }

    fn include_range(&mut self, rows: Option<Range<usize>>, visible_rows: usize) {
        let Some(rows) = rows else {
            return;
        };
        self.include_clipped_range(rows, visible_rows);
    }

    fn include_clipped_range(&mut self, rows: Range<usize>, visible_rows: usize) {
        let Some(mut rows) = clipped_non_empty_range(rows, visible_rows) else {
            return;
        };

        let mut index = 0;
        while index < self.ranges.len() {
            let current = &self.ranges[index];
            if rows.end < current.start {
                break;
            }
            if rows.start > current.end {
                index += 1;
                continue;
            }

            rows.start = rows.start.min(current.start);
            rows.end = rows.end.max(current.end);
            self.ranges.remove(index);
        }

        self.ranges.insert(index, rows);
    }

    fn into_option(self) -> Option<Self> {
        if self.ranges.is_empty() {
            None
        } else {
            Some(self)
        }
    }
}

impl TerminalViewport {
    #[cfg(test)]
    pub fn new(
        rows: usize,
        columns: usize,
        cells: Vec<TerminalCell>,
        cursor: CursorPosition,
    ) -> AppResult<Self> {
        Self::with_scroll(rows, columns, cells, cursor, TerminalScrollState::default())
    }

    pub fn with_scroll(
        rows: usize,
        columns: usize,
        cells: Vec<TerminalCell>,
        cursor: CursorPosition,
        scroll: TerminalScrollState,
    ) -> AppResult<Self> {
        let expected_cells = rows.checked_mul(columns).ok_or(AppError::InvalidInput(
            "terminal buffer dimensions are too large",
        ))?;

        if cells.len() != expected_cells {
            return Err(AppError::InvalidInput(
                "terminal buffer cell count does not match dimensions",
            ));
        }

        Ok(Self {
            rows,
            columns,
            cells,
            cursor,
            scroll,
            changed_rows: non_empty_range(0, rows),
            row_versions: vec![1; rows],
        })
    }

    pub fn change_baseline(&self) -> TerminalViewportChangeBaseline {
        TerminalViewportChangeBaseline {
            rows: self.rows,
            columns: self.columns,
            cell_count: self.cells.len(),
            cursor: self.cursor,
            scroll: self.scroll,
        }
    }

    pub fn set_changed_rows(&mut self, rows: Option<Range<usize>>) {
        let rows = rows.and_then(|rows| clipped_non_empty_range(rows, self.rows));
        if let Some(rows) = rows.clone() {
            self.bump_row_versions(rows);
        }
        self.changed_rows = rows;
    }

    pub fn row_version(&self, row: usize) -> Option<u64> {
        if row >= self.rows {
            return None;
        }

        self.row_versions.get(row).copied()
    }

    pub fn cell(&self, row: usize, column: usize) -> Option<&TerminalCell> {
        if row >= self.rows || column >= self.columns {
            return None;
        }

        let index = row.checked_mul(self.columns)?.checked_add(column)?;
        self.cells.get(index)
    }

    pub fn line_cells(&self, row: usize) -> Option<&[TerminalCell]> {
        if row >= self.rows {
            return None;
        }

        let start = row.checked_mul(self.columns)?;
        let end = start.checked_add(self.columns)?;
        self.cells.get(start..end)
    }

    #[cfg(test)]
    pub fn visible_line(&self, row: usize) -> Option<TerminalViewportLine<'_>> {
        let cells = self.line_cells(row)?;
        let end = cells
            .iter()
            .rposition(|cell| !cell.character.is_whitespace())
            .map(|index| index + 1)
            .unwrap_or(0);
        Some(TerminalViewportLine {
            cells: &cells[..end],
        })
    }

    fn bump_row_versions(&mut self, rows: Range<usize>) {
        let start = rows.start.min(self.row_versions.len());
        let end = rows.end.min(self.row_versions.len());
        for version in &mut self.row_versions[start..end] {
            *version = version.saturating_add(1);
        }
    }

    #[cfg(test)]
    pub fn changed_rows(&self, previous: &Self) -> Option<TerminalChangedRows> {
        if self.rows != previous.rows
            || self.columns != previous.columns
            || self.scroll.position != previous.scroll.position
        {
            return TerminalChangedRows::from_range(0..self.rows, self.rows);
        }

        let Some(expected_cells) = self.rows.checked_mul(self.columns) else {
            return TerminalChangedRows::from_range(0..self.rows, self.rows);
        };
        if self.cells.len() != expected_cells || previous.cells.len() != expected_cells {
            return TerminalChangedRows::from_range(0..self.rows, self.rows);
        }

        let mut changed = TerminalChangedRows::default();
        for row in 0..self.rows {
            if self.line_cells(row) != previous.line_cells(row) {
                changed.include(row);
            }
        }

        if self.cursor != previous.cursor {
            changed.include_if_visible(previous.cursor.row, self.rows);
            changed.include_if_visible(self.cursor.row, self.rows);
        }

        changed.into_option()
    }

    pub fn changed_rows_since_baseline(
        &self,
        previous: &TerminalViewportChangeBaseline,
    ) -> Option<TerminalChangedRows> {
        if self.rows != previous.rows
            || self.columns != previous.columns
            || self.scroll.position != previous.scroll.position
        {
            return TerminalChangedRows::from_range(0..self.rows, self.rows);
        }

        let Some(expected_cells) = self.rows.checked_mul(self.columns) else {
            return TerminalChangedRows::from_range(0..self.rows, self.rows);
        };
        if self.cells.len() != expected_cells || previous.cell_count != expected_cells {
            return TerminalChangedRows::from_range(0..self.rows, self.rows);
        }

        let mut changed = TerminalChangedRows::default();
        changed.include_range(self.changed_rows.clone(), self.rows);

        if self.cursor != previous.cursor {
            changed.include_if_visible(previous.cursor.row, self.rows);
            changed.include_if_visible(self.cursor.row, self.rows);
        }

        changed.into_option()
    }

    #[cfg(test)]
    pub fn visible_lines(&self) -> TerminalViewportLines<'_> {
        TerminalViewportLines {
            viewport: self,
            next_row: 0,
        }
    }

    #[cfg(test)]
    pub fn viewport_lines(&self) -> Vec<String> {
        self.visible_lines().map(|line| line.text()).collect()
    }

    pub fn selected_text(&self, selection: TerminalSelection) -> String {
        let mut text = String::new();
        if selection.is_empty() {
            return text;
        }

        let mut appended_line = false;
        for row in 0..self.rows {
            let Some(range) = selection.row_range(row, self.columns) else {
                continue;
            };
            if appended_line {
                text.push_str("\r\n");
            }
            appended_line = true;
            self.push_selected_row_text(row, range, &mut text);
        }

        text
    }

    fn push_selected_row_text(&self, row: usize, range: Range<usize>, text: &mut String) {
        let Some(cells) = self.line_cells(row) else {
            return;
        };
        let start = range.start.min(cells.len());
        let mut end = range.end.min(cells.len());
        while end > start && cells[end - 1].character.is_whitespace() {
            end -= 1;
        }

        for cell in &cells[start..end] {
            text.push(cell.character);
        }
    }
}

fn non_empty_range(start: usize, end: usize) -> Option<Range<usize>> {
    if start < end { Some(start..end) } else { None }
}

fn point_le(left: TerminalGridPoint, right: TerminalGridPoint) -> bool {
    left.row < right.row || (left.row == right.row && left.column <= right.column)
}

fn clipped_non_empty_range(rows: Range<usize>, visible_rows: usize) -> Option<Range<usize>> {
    non_empty_range(rows.start.min(visible_rows), rows.end.min(visible_rows))
}

#[cfg(test)]
#[derive(Debug, Clone, Copy)]
pub struct TerminalViewportLine<'a> {
    cells: &'a [TerminalCell],
}

#[cfg(test)]
impl<'a> TerminalViewportLine<'a> {
    pub fn text(&self) -> String {
        let mut line = String::with_capacity(self.cells.len());
        for cell in self.cells {
            line.push(cell.character);
        }
        line
    }
}

#[cfg(test)]
pub struct TerminalViewportLines<'a> {
    viewport: &'a TerminalViewport,
    next_row: usize,
}

#[cfg(test)]
impl<'a> Iterator for TerminalViewportLines<'a> {
    type Item = TerminalViewportLine<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next_row >= self.viewport.rows {
            return None;
        }

        let row = self.next_row;
        self.next_row += 1;
        self.viewport.visible_line(row)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSize {
    pub rows: u16,
    pub columns: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

impl TerminalSize {
    pub fn new(rows: u16, columns: u16) -> AppResult<Self> {
        Self::with_pixels(rows, columns, 0, 0)
    }

    pub fn with_pixels(
        rows: u16,
        columns: u16,
        pixel_width: u16,
        pixel_height: u16,
    ) -> AppResult<Self> {
        if rows < MIN_ROWS {
            return Err(AppError::InvalidInput("terminal rows must be at least 1"));
        }

        if columns < MIN_COLUMNS {
            return Err(AppError::InvalidInput(
                "terminal columns must be at least 2",
            ));
        }

        Ok(Self {
            rows,
            columns,
            pixel_width,
            pixel_height,
        })
    }
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self {
            rows: DEFAULT_ROWS,
            columns: DEFAULT_COLUMNS,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalCommand {
    Resize(TerminalSize),
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalScroll {
    Lines(i32),
    PageUp,
    PageDown,
    Absolute(usize),
    Top,
    Bottom,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TerminalScrollState {
    pub position: usize,
    pub max_position: usize,
    pub page_len: usize,
    pub total_len: usize,
}

impl TerminalScrollState {
    pub fn new(position: usize, max_position: usize, page_len: usize, total_len: usize) -> Self {
        Self {
            position: position.min(max_position),
            max_position,
            page_len,
            total_len,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TerminalTabId(u32);

impl TerminalTabId {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u32 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalTabView {
    pub id: TerminalTabId,
    pub title: String,
    pub active: bool,
}

impl TerminalTabView {
    pub fn new(id: TerminalTabId, title: impl Into<String>, active: bool) -> Self {
        Self {
            id,
            title: title.into(),
            active,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changed_row_ranges(rows: Option<TerminalChangedRows>) -> Option<Vec<(usize, usize)>> {
        rows.map(|rows| {
            rows.ranges()
                .iter()
                .map(|rows| (rows.start, rows.end))
                .collect()
        })
    }

    #[test]
    fn changed_rows_returns_only_rows_with_changed_cells() -> AppResult<()> {
        let previous = viewport(&["abc", "def", "ghi"], CursorPosition::new(2, 0))?;
        let current = viewport(&["abc", "dxf", "ghi"], CursorPosition::new(2, 0))?;

        assert_eq!(
            changed_row_ranges(current.changed_rows(&previous)),
            Some(vec![(1, 2)])
        );
        Ok(())
    }

    #[test]
    fn changed_rows_keeps_distant_cursor_rows_separate() -> AppResult<()> {
        let previous = viewport(&["abc", "def", "ghi"], CursorPosition::new(0, 1))?;
        let current = viewport(&["abc", "def", "ghi"], CursorPosition::new(2, 1))?;

        assert_eq!(
            changed_row_ranges(current.changed_rows(&previous)),
            Some(vec![(0, 1), (2, 3)])
        );
        Ok(())
    }

    #[test]
    fn changed_rows_ignores_scroll_extent_changes_without_visible_change() -> AppResult<()> {
        let previous = viewport_with_scroll(
            &["abc", "def", "ghi"],
            CursorPosition::new(2, 0),
            TerminalScrollState::new(0, 0, 3, 3),
        )?;
        let current = viewport_with_scroll(
            &["abc", "def", "ghi"],
            CursorPosition::new(2, 0),
            TerminalScrollState::new(0, 16, 3, 19),
        )?;

        assert_eq!(changed_row_ranges(current.changed_rows(&previous)), None);
        Ok(())
    }

    #[test]
    fn changed_rows_repaints_all_rows_when_scroll_position_changes() -> AppResult<()> {
        let previous = viewport_with_scroll(
            &["abc", "def", "ghi"],
            CursorPosition::new(2, 0),
            TerminalScrollState::new(0, 16, 3, 19),
        )?;
        let current = viewport_with_scroll(
            &["abc", "def", "ghi"],
            CursorPosition::new(2, 0),
            TerminalScrollState::new(2, 16, 3, 19),
        )?;

        assert_eq!(
            changed_row_ranges(current.changed_rows(&previous)),
            Some(vec![(0, 3)])
        );
        Ok(())
    }

    #[test]
    fn changed_rows_since_baseline_uses_recorded_changed_rows() -> AppResult<()> {
        let previous = viewport(&["abc", "def", "ghi"], CursorPosition::new(2, 0))?;
        let baseline = previous.change_baseline();
        let mut current = viewport(&["abc", "dxf", "ghi"], CursorPosition::new(2, 0))?;
        current.set_changed_rows(Some(1..2));

        assert_eq!(
            changed_row_ranges(current.changed_rows_since_baseline(&baseline)),
            Some(vec![(1, 2)])
        );
        Ok(())
    }

    #[test]
    fn row_version_updates_for_recorded_changed_rows() -> AppResult<()> {
        let mut current = viewport(&["abc", "def", "ghi"], CursorPosition::new(2, 0))?;
        assert_eq!(current.row_version(1), Some(1));

        current.set_changed_rows(Some(1..2));

        assert_eq!(current.row_version(1), Some(2));
        current.set_changed_rows(None);
        assert_eq!(current.row_version(1), Some(2));
        Ok(())
    }

    #[test]
    fn changed_rows_since_baseline_keeps_distant_cursor_rows_separate() -> AppResult<()> {
        let previous = viewport(&["abc", "def", "ghi"], CursorPosition::new(0, 1))?;
        let baseline = previous.change_baseline();
        let mut current = viewport(&["abc", "def", "ghi"], CursorPosition::new(2, 1))?;
        current.set_changed_rows(None);

        assert_eq!(
            changed_row_ranges(current.changed_rows_since_baseline(&baseline)),
            Some(vec![(0, 1), (2, 3)])
        );
        Ok(())
    }

    #[test]
    fn terminal_selection_returns_linear_row_ranges() {
        let selection =
            TerminalSelection::new(TerminalGridPoint::new(0, 2), TerminalGridPoint::new(2, 1));

        assert_eq!(selection.row_range(0, 5), Some(2..5));
        assert_eq!(selection.row_range(1, 5), Some(0..5));
        assert_eq!(selection.row_range(2, 5), Some(0..2));
        assert_eq!(selection.row_range(3, 5), None);
    }

    #[test]
    fn terminal_selection_supports_reverse_drag() {
        let selection =
            TerminalSelection::new(TerminalGridPoint::new(2, 1), TerminalGridPoint::new(0, 2));

        assert_eq!(selection.row_range(0, 5), Some(2..5));
        assert_eq!(selection.row_range(2, 5), Some(0..2));
    }

    #[test]
    fn terminal_selection_text_trims_trailing_spaces_per_row() -> AppResult<()> {
        let viewport = viewport(&["abc  ", "def  ", "ghi  "], CursorPosition::new(2, 0))?;
        let selection =
            TerminalSelection::new(TerminalGridPoint::new(0, 1), TerminalGridPoint::new(2, 2));

        assert_eq!(viewport.selected_text(selection), "bc\r\ndef\r\nghi");
        Ok(())
    }

    #[test]
    fn empty_terminal_selection_has_no_text() -> AppResult<()> {
        let viewport = viewport(&["abc"], CursorPosition::new(0, 0))?;
        let selection =
            TerminalSelection::new(TerminalGridPoint::new(0, 1), TerminalGridPoint::new(0, 1));

        assert_eq!(selection.row_range(0, viewport.columns), None);
        assert_eq!(viewport.selected_text(selection), "");
        Ok(())
    }

    #[test]
    fn changed_rows_since_baseline_ignores_scroll_extent_changes() -> AppResult<()> {
        let previous = viewport_with_scroll(
            &["abc", "def", "ghi"],
            CursorPosition::new(2, 0),
            TerminalScrollState::new(0, 0, 3, 3),
        )?;
        let baseline = previous.change_baseline();
        let mut current = viewport_with_scroll(
            &["abc", "def", "ghi"],
            CursorPosition::new(2, 0),
            TerminalScrollState::new(0, 16, 3, 19),
        )?;
        current.set_changed_rows(None);

        assert_eq!(
            changed_row_ranges(current.changed_rows_since_baseline(&baseline)),
            None
        );
        Ok(())
    }

    #[test]
    fn changed_rows_since_baseline_repaints_all_when_scroll_position_changes() -> AppResult<()> {
        let previous = viewport_with_scroll(
            &["abc", "def", "ghi"],
            CursorPosition::new(2, 0),
            TerminalScrollState::new(0, 16, 3, 19),
        )?;
        let baseline = previous.change_baseline();
        let mut current = viewport_with_scroll(
            &["abc", "def", "ghi"],
            CursorPosition::new(2, 0),
            TerminalScrollState::new(2, 16, 3, 19),
        )?;
        current.set_changed_rows(None);

        assert_eq!(
            changed_row_ranges(current.changed_rows_since_baseline(&baseline)),
            Some(vec![(0, 3)])
        );
        Ok(())
    }

    fn viewport(rows: &[&str], cursor: CursorPosition) -> AppResult<TerminalViewport> {
        viewport_with_scroll(rows, cursor, TerminalScrollState::default())
    }

    fn viewport_with_scroll(
        rows: &[&str],
        cursor: CursorPosition,
        scroll: TerminalScrollState,
    ) -> AppResult<TerminalViewport> {
        let columns = rows.first().map_or(0, |row| row.chars().count());
        if rows.iter().any(|row| row.chars().count() != columns) {
            return Err(AppError::InvalidInput(
                "test viewport rows must have matching widths",
            ));
        }

        let mut cells = Vec::with_capacity(rows.len().saturating_mul(columns));
        for row in rows {
            cells.extend(row.chars().map(TerminalCell::new));
        }

        TerminalViewport::with_scroll(rows.len(), columns, cells, cursor, scroll)
    }
}
