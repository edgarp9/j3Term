use std::mem::MaybeUninit;
use std::ops::Range;

use windows_sys::Win32::Foundation::{COLORREF, HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    BITSPIXEL, BeginPaint, BitBlt, CLIP_DEFAULT_PRECIS, CreateCompatibleBitmap, CreateCompatibleDC,
    CreateFontW, CreateSolidBrush, DEFAULT_CHARSET, DEFAULT_QUALITY, DeleteDC, DeleteObject,
    ETO_CLIPPED, EndPaint, ExtTextOutW, FF_MODERN, FIXED_PITCH, FW_NORMAL, FillRect, GetDC,
    GetDeviceCaps, GetStockObject, GetTextMetricsW, HBITMAP, HBRUSH, HDC, HFONT, HGDIOBJ,
    InvalidateRect, LOGPIXELSY, OUT_DEFAULT_PRECIS, PAINTSTRUCT, PLANES, ReleaseDC, SRCCOPY,
    SYSTEM_FIXED_FONT, SelectObject, SetBkColor, SetBkMode, SetTextColor, SetViewportOrgEx,
    TECHNOLOGY, TEXTMETRICW, TRANSPARENT, TextOutW,
};

use crate::domain::layout::terminal_content_area;
use crate::domain::terminal::TerminalChangedRows;
use crate::domain::{
    TerminalCell, TerminalFont, TerminalSelection, TerminalTabId, TerminalTabView,
    TerminalViewport, UiRect, WindowLayout,
};
use crate::error::{AppError, AppResult};

const CHROME_BACKGROUND: COLORREF = rgb(34, 38, 46);
const BACKGROUND: COLORREF = rgb(12, 14, 18);
const FOREGROUND: COLORREF = rgb(220, 226, 235);
const TAB_ACTIVE_BACKGROUND: COLORREF = rgb(12, 14, 18);
const TAB_INACTIVE_BACKGROUND: COLORREF = rgb(55, 61, 72);
const TAB_HOVER_BACKGROUND: COLORREF = rgb(70, 77, 90);
const TAB_MUTED_FOREGROUND: COLORREF = rgb(170, 180, 194);
const SPLITTER_BACKGROUND: COLORREF = rgb(42, 47, 56);
const SPLITTER_GRIP: COLORREF = rgb(90, 100, 116);
const SELECTION_BACKGROUND: COLORREF = rgb(52, 100, 156);
const LINE_CACHE_MAX_RETAINED_CAPACITY_FACTOR: usize = 4;
const PAINT_BUFFER_MAX_RETAINED_AREA_FACTOR: i64 = 4;

#[derive(Debug, Clone, Copy)]
pub struct CellMetrics {
    pub width: i32,
    pub height: i32,
}

impl Default for CellMetrics {
    fn default() -> Self {
        Self {
            width: 8,
            height: 16,
        }
    }
}

#[derive(Debug)]
pub struct GdiRenderer {
    metrics: CellMetrics,
    metrics_initialized: bool,
    font: TerminalFont,
    font_resources: Option<FontResources>,
    line_cache: Vec<EncodedLine>,
    ui_text: UiTextBuffer,
    brushes: SolidBrushCache,
    paint_buffer: PaintBuffer,
}

impl Default for GdiRenderer {
    fn default() -> Self {
        Self {
            metrics: CellMetrics::default(),
            metrics_initialized: false,
            font: TerminalFont::default(),
            font_resources: None,
            line_cache: Vec::new(),
            ui_text: UiTextBuffer::default(),
            brushes: SolidBrushCache::default(),
            paint_buffer: PaintBuffer::default(),
        }
    }
}

#[derive(Clone, Copy)]
struct PaintScene<'a> {
    viewport: &'a TerminalViewport,
    layout: &'a WindowLayout,
    tabs: &'a [TerminalTabView],
    selection: Option<TerminalSelection>,
}

impl GdiRenderer {
    pub fn refresh_metrics(&mut self, hwnd: HWND) -> AppResult<CellMetrics> {
        let dc = WindowDc::acquire(hwnd)?;
        let _font = self.select_font(dc.hdc())?;
        let metrics = measure_cell_metrics(dc.hdc());

        self.metrics = metrics;
        self.metrics_initialized = true;
        Ok(metrics)
    }

    pub fn ensure_metrics(&mut self, hwnd: HWND) -> AppResult<CellMetrics> {
        if self.metrics_initialized {
            return Ok(self.metrics);
        }

        self.refresh_metrics(hwnd)
    }

    pub fn cell_metrics(&self) -> CellMetrics {
        self.metrics
    }

    pub fn set_font(&mut self, font: TerminalFont) {
        if self.font == font {
            return;
        }

        self.font = font;
        self.font_resources = None;
        self.metrics_initialized = false;
        self.line_cache.clear();
        self.paint_buffer = PaintBuffer::default();
    }

    pub(crate) fn clear_terminal_line_cache(&mut self) {
        self.line_cache.clear();
    }

    #[cfg(test)]
    pub(crate) fn cache_terminal_line_for_test(
        &mut self,
        cells: &[TerminalCell],
        row_version: Option<u64>,
    ) -> AppResult<()> {
        let mut line = EncodedLine::default();
        line.refresh_utf16(cells, row_version, self.metrics.width)?;
        self.line_cache.push(line);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn terminal_line_cache_len_for_test(&self) -> usize {
        self.line_cache.len()
    }

    fn ensure_metrics_initialized(&mut self, hdc: HDC) {
        if self.metrics_initialized {
            return;
        }

        self.metrics = measure_cell_metrics(hdc);
        self.metrics_initialized = true;
    }

    fn select_font(&mut self, hdc: HDC) -> AppResult<SelectedObject> {
        let dpi_y = device_dpi_y(hdc);
        let should_recreate = self
            .font_resources
            .as_ref()
            .is_none_or(|resources| resources.dpi_y != dpi_y);
        if should_recreate {
            self.font_resources = Some(FontResources::new(hdc, &self.font)?);
            self.metrics_initialized = false;
            self.line_cache.clear();
        }

        Ok(SelectedObject::font(
            hdc,
            self.font_resources
                .as_ref()
                .map(|resources| resources.font.handle()),
        ))
    }

    pub fn invalidate(hwnd: HWND) {
        // SAFETY: hwnd is created and owned by this process; null rect invalidates the full client.
        unsafe {
            InvalidateRect(hwnd, std::ptr::null(), 0);
        }
    }

    pub fn invalidate_rect(hwnd: HWND, rect: UiRect) {
        if rect.width <= 0 || rect.height <= 0 {
            return;
        }

        let rect = rect_from_ui(rect);
        // SAFETY: hwnd is live and rect points to client coordinates for this call.
        unsafe {
            InvalidateRect(hwnd, &rect, 0);
        }
    }

    pub fn paint(
        &mut self,
        hwnd: HWND,
        viewport: &TerminalViewport,
        layout: &WindowLayout,
        tabs: &[TerminalTabView],
        selection: Option<TerminalSelection>,
        terminal_paint_rows: Option<&TerminalChangedRows>,
    ) -> AppResult<()> {
        let paint = PaintContext::begin(hwnd)?;
        let scene = PaintScene {
            viewport,
            layout,
            tabs,
            selection,
        };

        let mut paint_buffer = std::mem::take(&mut self.paint_buffer);
        let result = if let Some(rows) = terminal_paint_rows {
            self.paint_terminal_row_ranges(
                &mut paint_buffer,
                paint.hdc(),
                paint.paint_rect(),
                scene,
                rows,
            )
        } else {
            self.paint_buffered_rect(&mut paint_buffer, paint.hdc(), paint.paint_rect(), scene)
        };

        self.paint_buffer = paint_buffer;
        result
    }

    fn paint_buffered_rect(
        &mut self,
        paint_buffer: &mut PaintBuffer,
        target_hdc: HDC,
        paint_rect: &RECT,
        scene: PaintScene<'_>,
    ) -> AppResult<()> {
        match paint_buffer.prepare(target_hdc, paint_rect) {
            Ok(Some(buffer)) => {
                let render_result = self.paint_with_hdc(
                    buffer.hdc(),
                    paint_rect,
                    scene.viewport,
                    scene.layout,
                    scene.tabs,
                    scene.selection,
                );
                match render_result {
                    Ok(()) => buffer.flush_to(target_hdc, paint_rect),
                    Err(error) => Err(error),
                }
            }
            Ok(None) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn paint_terminal_row_ranges(
        &mut self,
        paint_buffer: &mut PaintBuffer,
        target_hdc: HDC,
        paint_rect: &RECT,
        scene: PaintScene<'_>,
        rows: &TerminalChangedRows,
    ) -> AppResult<()> {
        let terminal_content = terminal_content_area(scene.layout.terminal);
        let metrics = self.metrics;
        for row_range in rows.ranges() {
            let Some(row_rect) = terminal_row_range_rect(
                terminal_content,
                metrics,
                row_range.clone(),
                scene.viewport.rows,
            ) else {
                continue;
            };
            let Some(paint_rect) = intersect_rects(&rect_from_ui(row_rect), paint_rect) else {
                continue;
            };

            self.paint_buffered_rect(paint_buffer, target_hdc, &paint_rect, scene)?;
        }

        Ok(())
    }

    fn paint_with_hdc(
        &mut self,
        hdc: HDC,
        paint_rect: &RECT,
        viewport: &TerminalViewport,
        layout: &WindowLayout,
        tabs: &[TerminalTabView],
        selection: Option<TerminalSelection>,
    ) -> AppResult<()> {
        fill_solid_rect(
            hdc,
            paint_rect,
            CHROME_BACKGROUND,
            "FillRect chrome",
            &mut self.brushes,
        )?;

        let _font = self.select_font(hdc)?;
        self.ensure_metrics_initialized(hdc);

        // SAFETY: GDI calls operate on the HDC supplied by BeginPaint and valid constants.
        unsafe {
            SetBkMode(hdc, TRANSPARENT as i32);
            SetBkColor(hdc, BACKGROUND);
            SetTextColor(hdc, FOREGROUND);
        }

        self.paint_tab_bar(hdc, paint_rect, layout, tabs)?;
        fill_solid_rect_intersection(
            hdc,
            &rect_from_ui(layout.terminal),
            paint_rect,
            BACKGROUND,
            "FillRect terminal",
            &mut self.brushes,
        )?;

        let terminal_content = terminal_content_area(layout.terminal);
        let rows = terminal_paint_row_range(
            paint_rect,
            terminal_content,
            viewport.rows,
            self.metrics.height,
        );
        for row in rows {
            if let Some(range) =
                selection.and_then(|selection| selection.row_range(row, viewport.columns))
            {
                self.paint_selection_background(hdc, paint_rect, terminal_content, row, range)?;
            }

            if self.line_cache.len() <= row {
                self.line_cache
                    .resize_with(row.saturating_add(1), EncodedLine::default);
            }

            let cell_width = self.metrics.width.max(1);
            let row_version = viewport.row_version(row);
            let Some(cells) = viewport.line_cells(row) else {
                continue;
            };
            let fingerprint = terminal_cells_fingerprint(cells);
            if !self.line_cache[row].is_valid_for(
                viewport.columns,
                row_version,
                cell_width,
                fingerprint,
            ) {
                self.line_cache[row].refresh_utf16(cells, row_version, cell_width)?;
            }

            let line = &self.line_cache[row];
            let line_buffer = line.utf16();
            if line_buffer.is_empty() {
                continue;
            }

            let count = u32::try_from(line_buffer.len())
                .map_err(|_| AppError::InvalidInput("line is too long to render"))?;
            let y = i32::try_from(row)
                .map_err(|_| AppError::InvalidInput("row index is too large"))?
                .saturating_mul(self.metrics.height)
                .saturating_add(terminal_content.y);
            let Some(clip) = terminal_row_clip_rect(terminal_content, self.metrics, row) else {
                continue;
            };

            // SAFETY: line and its advance array point to buffers valid for the duration of the call.
            let ok = unsafe {
                ExtTextOutW(
                    hdc,
                    terminal_content.x,
                    y,
                    ETO_CLIPPED,
                    &clip,
                    line_buffer.as_ptr(),
                    count,
                    line.advances().as_ptr(),
                )
            };
            if ok == 0 {
                return Err(AppError::win32("ExtTextOutW terminal row"));
            }
        }

        self.paint_splitter(hdc, paint_rect, layout)?;
        self.paint_cursor(hdc, paint_rect, viewport, terminal_content)
    }

    fn paint_selection_background(
        &mut self,
        hdc: HDC,
        paint_rect: &RECT,
        terminal_area: UiRect,
        row: usize,
        columns: Range<usize>,
    ) -> AppResult<()> {
        let rect = selection_rect(terminal_area, self.metrics, row, columns)?;
        fill_solid_rect_intersection(
            hdc,
            &rect,
            paint_rect,
            SELECTION_BACKGROUND,
            "FillRect selection",
            &mut self.brushes,
        )
    }

    fn paint_tab_bar(
        &mut self,
        hdc: HDC,
        paint_rect: &RECT,
        layout: &WindowLayout,
        tabs: &[TerminalTabView],
    ) -> AppResult<()> {
        if !rects_intersect(&rect_from_ui(layout.tab_bar), paint_rect) {
            return Ok(());
        }

        fill_solid_rect_intersection(
            hdc,
            &rect_from_ui(layout.tab_bar),
            paint_rect,
            CHROME_BACKGROUND,
            "FillRect tab bar",
            &mut self.brushes,
        )?;

        for placement in &layout.tabs {
            let tab_rect = rect_from_ui(placement.bounds);
            if !rects_intersect(&tab_rect, paint_rect) {
                continue;
            }

            let background = if placement.active {
                TAB_ACTIVE_BACKGROUND
            } else {
                TAB_INACTIVE_BACKGROUND
            };
            fill_solid_rect_intersection(
                hdc,
                &tab_rect,
                paint_rect,
                background,
                "FillRect tab",
                &mut self.brushes,
            )?;

            let title = tab_title(tabs, placement.id);
            let close_padding = placement
                .close_bounds
                .map(|bounds| bounds.width.saturating_add(8))
                .unwrap_or(0);
            let label_bounds = UiRect {
                x: placement.bounds.x.saturating_add(10),
                y: placement.bounds.y,
                width: placement
                    .bounds
                    .width
                    .saturating_sub(14)
                    .saturating_sub(close_padding),
                height: placement.bounds.height,
            };
            draw_left_text(
                hdc,
                label_bounds,
                title,
                FOREGROUND,
                self.metrics,
                &mut self.ui_text,
            )?;

            if let Some(close_bounds) = placement.close_bounds
                && rects_intersect(&rect_from_ui(close_bounds), paint_rect)
            {
                draw_centered_text(
                    hdc,
                    close_bounds,
                    "x",
                    TAB_MUTED_FOREGROUND,
                    self.metrics,
                    &mut self.ui_text,
                )?;
            }
        }

        if let Some(new_tab_button) = layout.new_tab_button {
            let new_tab_rect = rect_from_ui(new_tab_button);
            if rects_intersect(&new_tab_rect, paint_rect) {
                fill_solid_rect_intersection(
                    hdc,
                    &new_tab_rect,
                    paint_rect,
                    TAB_HOVER_BACKGROUND,
                    "FillRect new tab",
                    &mut self.brushes,
                )?;
                draw_centered_text(
                    hdc,
                    new_tab_button,
                    "+",
                    FOREGROUND,
                    self.metrics,
                    &mut self.ui_text,
                )?;
            }
        }

        Ok(())
    }

    fn paint_splitter(
        &mut self,
        hdc: HDC,
        paint_rect: &RECT,
        layout: &WindowLayout,
    ) -> AppResult<()> {
        if layout.splitter.width <= 0 || layout.splitter.height <= 0 {
            return Ok(());
        }

        let splitter_rect = rect_from_ui(layout.splitter);
        if !rects_intersect(&splitter_rect, paint_rect) {
            return Ok(());
        }

        fill_solid_rect_intersection(
            hdc,
            &splitter_rect,
            paint_rect,
            SPLITTER_BACKGROUND,
            "FillRect splitter",
            &mut self.brushes,
        )?;

        let grip_width = 2.min(layout.splitter.width.max(1));
        let grip = UiRect {
            x: layout
                .splitter
                .x
                .saturating_add((layout.splitter.width.saturating_sub(grip_width)) / 2),
            y: layout.splitter.y,
            width: grip_width,
            height: layout.splitter.height,
        };
        fill_solid_rect_intersection(
            hdc,
            &rect_from_ui(grip),
            paint_rect,
            SPLITTER_GRIP,
            "FillRect splitter grip",
            &mut self.brushes,
        )
    }

    fn paint_cursor(
        &mut self,
        hdc: HDC,
        paint_rect: &RECT,
        viewport: &TerminalViewport,
        terminal_area: UiRect,
    ) -> AppResult<()> {
        if viewport.cursor.row >= viewport.rows || viewport.cursor.column >= viewport.columns {
            return Ok(());
        }

        let rect = cell_rect(
            terminal_area,
            self.metrics,
            viewport.cursor.row,
            viewport.cursor.column,
        )?;
        let Some(rect) = intersect_rects(&rect, &rect_from_ui(terminal_area)) else {
            return Ok(());
        };
        if !rects_intersect(&rect, paint_rect) {
            return Ok(());
        }

        fill_solid_rect(hdc, &rect, FOREGROUND, "FillRect cursor", &mut self.brushes)?;

        let Some(cell) = viewport.cell(viewport.cursor.row, viewport.cursor.column) else {
            return Ok(());
        };

        if cell.character == ' ' {
            return Ok(());
        }

        let mut wide = [0; 2];
        let wide = cell.character.encode_utf16(&mut wide);
        let count = u32::try_from(wide.len())
            .map_err(|_| AppError::InvalidInput("cursor cell is too long to render"))?;

        // SAFETY: wide points to UTF-16 text valid for the duration of the call.
        unsafe {
            SetTextColor(hdc, BACKGROUND);
            let ok = ExtTextOutW(
                hdc,
                rect.left,
                rect.top,
                ETO_CLIPPED,
                &rect,
                wide.as_ptr(),
                count,
                std::ptr::null(),
            );
            SetTextColor(hdc, FOREGROUND);
            if ok == 0 {
                return Err(AppError::win32("ExtTextOutW cursor"));
            }
        }

        Ok(())
    }
}

#[derive(Debug, Default)]
struct EncodedLine {
    wide: Vec<u16>,
    advances: Vec<i32>,
    columns: usize,
    cell_width: i32,
    is_valid: bool,
    row_version: Option<u64>,
    fingerprint: u64,
}

impl EncodedLine {
    fn is_valid_for(
        &self,
        columns: usize,
        row_version: Option<u64>,
        cell_width: i32,
        fingerprint: u64,
    ) -> bool {
        row_version.is_some()
            && self.is_valid
            && self.columns == columns
            && self.cell_width == cell_width
            && self.row_version == row_version
            && self.fingerprint == fingerprint
    }

    fn refresh_utf16(
        &mut self,
        cells: &[TerminalCell],
        row_version: Option<u64>,
        cell_width: i32,
    ) -> AppResult<()> {
        self.is_valid = false;
        self.columns = cells.len();
        self.cell_width = cell_width.max(1);
        self.row_version = row_version;
        self.fingerprint = terminal_cells_fingerprint(cells);
        encode_terminal_cells_utf16(&mut self.wide, &mut self.advances, cells, self.cell_width)?;
        self.is_valid = true;
        Ok(())
    }

    fn utf16(&self) -> &[u16] {
        &self.wide
    }

    fn advances(&self) -> &[i32] {
        &self.advances
    }
}

fn terminal_cells_fingerprint(cells: &[TerminalCell]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for cell in cells {
        hash ^= u64::from(cell.character as u32);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash ^= cells.len() as u64;
    hash
}

fn encode_terminal_cells_utf16(
    wide: &mut Vec<u16>,
    advances: &mut Vec<i32>,
    cells: &[TerminalCell],
    cell_width: i32,
) -> AppResult<()> {
    wide.clear();
    advances.clear();

    let Some(visible_cell_count) = cells
        .iter()
        .rposition(|cell| !cell.character.is_whitespace())
        .map(|index| index + 1)
    else {
        release_excess_line_cache_capacity(wide, 0);
        release_excess_line_cache_capacity(advances, 0);
        return Ok(());
    };

    let visible_cells = &cells[..visible_cell_count];
    let wide_capacity = visible_cells
        .len()
        .checked_mul(2)
        .ok_or(AppError::InvalidInput("line is too long to render"))?;
    release_excess_line_cache_capacity(wide, wide_capacity);
    release_excess_line_cache_capacity(advances, wide_capacity);
    ensure_vec_capacity(wide, wide_capacity, "line is too long to render")?;
    ensure_vec_capacity(advances, wide_capacity, "line is too long to render")?;

    for cell in visible_cells {
        let mut encoded = [0; 2];
        for (index, unit) in cell.character.encode_utf16(&mut encoded).iter().enumerate() {
            wide.push(*unit);
            advances.push(if index == 0 { cell_width.max(1) } else { 0 });
        }
    }

    Ok(())
}

fn release_excess_line_cache_capacity<T>(values: &mut Vec<T>, required: usize) {
    let max_retained = required.saturating_mul(LINE_CACHE_MAX_RETAINED_CAPACITY_FACTOR);
    if values.capacity() > max_retained {
        *values = Vec::new();
    }
}

#[derive(Debug, Default)]
struct UiTextBuffer {
    wide: Vec<u16>,
    truncated: String,
}

fn tab_title(tabs: &[TerminalTabView], id: TerminalTabId) -> &str {
    tabs.iter()
        .find(|tab| tab.id == id)
        .map(|tab| tab.title.as_str())
        .unwrap_or("")
}

fn draw_left_text(
    hdc: HDC,
    bounds: UiRect,
    text: &str,
    color: COLORREF,
    metrics: CellMetrics,
    buffer: &mut UiTextBuffer,
) -> AppResult<()> {
    if text.is_empty() || bounds.width <= 0 || bounds.height <= 0 {
        return Ok(());
    }

    let y = centered_text_y(bounds, metrics);
    let UiTextBuffer { wide, truncated } = buffer;
    let text = truncate_for_width(truncated, text, bounds.width, metrics)?;
    draw_text_at(hdc, bounds.x, y, text, color, wide)
}

fn draw_centered_text(
    hdc: HDC,
    bounds: UiRect,
    text: &str,
    color: COLORREF,
    metrics: CellMetrics,
    buffer: &mut UiTextBuffer,
) -> AppResult<()> {
    if text.is_empty() || bounds.width <= 0 || bounds.height <= 0 {
        return Ok(());
    }

    let text_width = approximate_text_width(text, metrics);
    let x = bounds
        .x
        .saturating_add((bounds.width.saturating_sub(text_width)) / 2);
    let y = centered_text_y(bounds, metrics);
    draw_text_at(hdc, x, y, text, color, &mut buffer.wide)
}

fn draw_text_at(
    hdc: HDC,
    x: i32,
    y: i32,
    text: &str,
    color: COLORREF,
    wide: &mut Vec<u16>,
) -> AppResult<()> {
    if text.is_empty() {
        return Ok(());
    }

    encode_str_utf16(wide, text, "text is too long to render")?;
    let count = i32::try_from(wide.len())
        .map_err(|_| AppError::InvalidInput("text is too long to render"))?;

    // SAFETY: wide points to UTF-16 text valid for the duration of the call.
    unsafe {
        SetTextColor(hdc, color);
        let ok = TextOutW(hdc, x, y, wide.as_ptr(), count);
        SetTextColor(hdc, FOREGROUND);
        if ok == 0 {
            return Err(AppError::win32("TextOutW ui text"));
        }
    }

    Ok(())
}

fn truncate_for_width<'a>(
    scratch: &'a mut String,
    text: &'a str,
    max_width: i32,
    metrics: CellMetrics,
) -> AppResult<&'a str> {
    if max_width <= 0 {
        scratch.clear();
        return Ok(scratch.as_str());
    }

    let max_chars = usize::try_from(max_width / metrics.width.max(1)).unwrap_or(0);
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return Ok(text);
    }

    scratch.clear();
    scratch
        .try_reserve(text.len())
        .map_err(|_| AppError::InvalidInput("text is too long to render"))?;
    if max_chars <= 3 {
        scratch.extend(text.chars().take(max_chars));
        return Ok(scratch.as_str());
    }

    scratch.extend(text.chars().take(max_chars.saturating_sub(3)));
    scratch.push_str("...");
    Ok(scratch.as_str())
}

fn approximate_text_width(text: &str, metrics: CellMetrics) -> i32 {
    i32::try_from(text.encode_utf16().count())
        .unwrap_or(i32::MAX)
        .saturating_mul(metrics.width.max(1))
}

fn centered_text_y(bounds: UiRect, metrics: CellMetrics) -> i32 {
    bounds
        .y
        .saturating_add((bounds.height.saturating_sub(metrics.height.max(1))) / 2)
}

fn fill_solid_rect(
    hdc: HDC,
    rect: &RECT,
    color: COLORREF,
    operation: &'static str,
    brushes: &mut SolidBrushCache,
) -> AppResult<()> {
    let brush = brushes.get(color)?;

    // SAFETY: rect points to valid RECT data and brush is a live GDI brush.
    let filled = unsafe { FillRect(hdc, rect, brush) };
    if filled == 0 {
        return Err(AppError::win32(operation));
    }

    Ok(())
}

fn fill_solid_rect_intersection(
    hdc: HDC,
    rect: &RECT,
    clip: &RECT,
    color: COLORREF,
    operation: &'static str,
    brushes: &mut SolidBrushCache,
) -> AppResult<()> {
    let Some(clipped) = intersect_rects(rect, clip) else {
        return Ok(());
    };

    fill_solid_rect(hdc, &clipped, color, operation, brushes)
}

fn encode_str_utf16(buffer: &mut Vec<u16>, text: &str, error: &'static str) -> AppResult<()> {
    buffer.clear();
    ensure_vec_capacity(buffer, text.len(), error)?;
    buffer.extend(text.encode_utf16());
    Ok(())
}

fn ensure_vec_capacity<T>(
    buffer: &mut Vec<T>,
    required: usize,
    error: &'static str,
) -> AppResult<()> {
    if buffer.capacity() < required {
        buffer
            .try_reserve(required - buffer.capacity())
            .map_err(|_| AppError::InvalidInput(error))?;
    }

    Ok(())
}

struct WindowDc {
    hwnd: HWND,
    hdc: HDC,
}

impl WindowDc {
    fn acquire(hwnd: HWND) -> AppResult<Self> {
        // SAFETY: hwnd is a live window on the UI thread; Drop releases the returned DC.
        let hdc = unsafe { GetDC(hwnd) };
        if hdc.is_null() {
            Err(AppError::win32("GetDC"))
        } else {
            Ok(Self { hwnd, hdc })
        }
    }

    fn hdc(&self) -> HDC {
        self.hdc
    }
}

impl Drop for WindowDc {
    fn drop(&mut self) {
        // SAFETY: hdc was returned by GetDC for this hwnd.
        unsafe {
            ReleaseDC(self.hwnd, self.hdc);
        }
    }
}

struct PaintContext {
    hwnd: HWND,
    paint: PAINTSTRUCT,
    hdc: HDC,
}

impl PaintContext {
    fn begin(hwnd: HWND) -> AppResult<Self> {
        let mut paint = MaybeUninit::<PAINTSTRUCT>::zeroed();

        // SAFETY: Win32 requires BeginPaint/EndPaint to be paired during WM_PAINT.
        let hdc = unsafe { BeginPaint(hwnd, paint.as_mut_ptr()) };
        if hdc.is_null() {
            return Err(AppError::win32("BeginPaint"));
        }

        // SAFETY: BeginPaint initialized PAINTSTRUCT when it returned a non-null HDC.
        let paint = unsafe { paint.assume_init() };
        Ok(Self { hwnd, paint, hdc })
    }

    fn hdc(&self) -> HDC {
        self.hdc
    }

    fn paint_rect(&self) -> &RECT {
        &self.paint.rcPaint
    }
}

impl Drop for PaintContext {
    fn drop(&mut self) {
        // SAFETY: hdc and paint are the values returned by BeginPaint for this hwnd.
        unsafe {
            EndPaint(self.hwnd, &self.paint);
        }
    }
}

#[derive(Debug, Default)]
struct PaintBuffer {
    resources: Option<PaintBufferResources>,
}

impl PaintBuffer {
    fn prepare(
        &mut self,
        source_hdc: HDC,
        rect: &RECT,
    ) -> AppResult<Option<PreparedPaintBuffer<'_>>> {
        let Some(size) = paint_buffer_size(rect) else {
            return Ok(None);
        };
        let compatibility = PaintBufferCompatibility::from_hdc(source_hdc);

        let should_recreate = match self.resources.as_ref() {
            Some(resources) => !resources.is_compatible(size, compatibility),
            None => true,
        };
        if should_recreate {
            self.resources = None;
            self.resources = Some(PaintBufferResources::new(source_hdc, size, compatibility)?);
        }

        let Some(resources) = self.resources.as_ref() else {
            return Err(AppError::InvalidInput("paint buffer was not initialized"));
        };
        resources.set_viewport_for(rect)?;

        Ok(Some(PreparedPaintBuffer { resources }))
    }
}

#[derive(Debug)]
struct PreparedPaintBuffer<'a> {
    resources: &'a PaintBufferResources,
}

impl PreparedPaintBuffer<'_> {
    fn hdc(&self) -> HDC {
        self.resources.hdc
    }

    fn flush_to(&self, target_hdc: HDC, rect: &RECT) -> AppResult<()> {
        let Some(size) = paint_buffer_size(rect) else {
            return Ok(());
        };

        self.resources.reset_viewport()?;

        // SAFETY: both HDCs are valid; source bitmap covers width x height.
        let copied = unsafe {
            BitBlt(
                target_hdc,
                rect.left,
                rect.top,
                size.width,
                size.height,
                self.resources.hdc,
                0,
                0,
                SRCCOPY,
            )
        };
        if copied == 0 {
            Err(AppError::win32("BitBlt"))
        } else {
            Ok(())
        }
    }
}

#[derive(Debug)]
struct PaintBufferResources {
    hdc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    size: PaintBufferSize,
    compatibility: PaintBufferCompatibility,
}

impl PaintBufferResources {
    fn new(
        source_hdc: HDC,
        size: PaintBufferSize,
        compatibility: PaintBufferCompatibility,
    ) -> AppResult<Self> {
        // SAFETY: source_hdc is the paint HDC and can create compatible resources.
        let hdc = unsafe { CreateCompatibleDC(source_hdc) };
        if hdc.is_null() {
            return Err(AppError::win32("CreateCompatibleDC"));
        }

        // SAFETY: source_hdc is valid and size was checked positive.
        let bitmap = unsafe { CreateCompatibleBitmap(source_hdc, size.width, size.height) };
        if bitmap.is_null() {
            // SAFETY: hdc was created above and is owned here.
            unsafe {
                DeleteDC(hdc);
            }
            return Err(AppError::win32("CreateCompatibleBitmap"));
        }

        // SAFETY: bitmap is compatible with hdc and selected for offscreen drawing.
        let previous = unsafe { SelectObject(hdc, bitmap) };
        if previous.is_null() {
            // SAFETY: bitmap was not selected into hdc, and both owned resources must be released.
            unsafe {
                DeleteObject(bitmap);
                DeleteDC(hdc);
            }
            return Err(AppError::win32("SelectObject bitmap"));
        }

        Ok(Self {
            hdc,
            bitmap,
            previous,
            size,
            compatibility,
        })
    }

    fn is_compatible(
        &self,
        size: PaintBufferSize,
        compatibility: PaintBufferCompatibility,
    ) -> bool {
        self.compatibility == compatibility && self.size.reusable_for(size)
    }

    fn set_viewport_for(&self, rect: &RECT) -> AppResult<()> {
        self.set_viewport_origin(
            rect.left.saturating_neg(),
            rect.top.saturating_neg(),
            "SetViewportOrgEx paint buffer",
        )
    }

    fn reset_viewport(&self) -> AppResult<()> {
        self.set_viewport_origin(0, 0, "SetViewportOrgEx paint buffer reset")
    }

    fn set_viewport_origin(&self, x: i32, y: i32, operation: &'static str) -> AppResult<()> {
        // SAFETY: hdc is an owned memory DC and the previous viewport origin is not needed.
        let ok = unsafe { SetViewportOrgEx(self.hdc, x, y, std::ptr::null_mut()) };
        if ok == 0 {
            Err(AppError::win32(operation))
        } else {
            Ok(())
        }
    }
}

impl Drop for PaintBufferResources {
    fn drop(&mut self) {
        // SAFETY: previous was returned by SelectObject for this memory HDC.
        unsafe {
            SelectObject(self.hdc, self.previous);
        }

        // SAFETY: bitmap and memory HDC are owned by this buffer.
        unsafe {
            DeleteObject(self.bitmap);
            DeleteDC(self.hdc);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PaintBufferSize {
    width: i32,
    height: i32,
}

impl PaintBufferSize {
    fn covers(self, required: Self) -> bool {
        self.width >= required.width && self.height >= required.height
    }

    fn reusable_for(self, required: Self) -> bool {
        if !self.covers(required) {
            return false;
        }

        self.area()
            <= required
                .area()
                .saturating_mul(PAINT_BUFFER_MAX_RETAINED_AREA_FACTOR)
    }

    fn area(self) -> i64 {
        i64::from(self.width.max(0)) * i64::from(self.height.max(0))
    }
}

fn paint_buffer_size(rect: &RECT) -> Option<PaintBufferSize> {
    let width = rect.right.saturating_sub(rect.left);
    let height = rect.bottom.saturating_sub(rect.top);
    if width <= 0 || height <= 0 {
        None
    } else {
        Some(PaintBufferSize { width, height })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PaintBufferCompatibility {
    technology: i32,
    bits_pixel: i32,
    planes: i32,
}

impl PaintBufferCompatibility {
    fn from_hdc(hdc: HDC) -> Self {
        // SAFETY: hdc is a live paint HDC for querying immutable device capabilities.
        unsafe {
            Self {
                technology: GetDeviceCaps(hdc, TECHNOLOGY as i32),
                bits_pixel: GetDeviceCaps(hdc, BITSPIXEL as i32),
                planes: GetDeviceCaps(hdc, PLANES as i32),
            }
        }
    }
}

struct SelectedObject {
    hdc: HDC,
    previous: Option<HGDIOBJ>,
}

impl SelectedObject {
    fn font(hdc: HDC, font: Option<HFONT>) -> Self {
        let previous = match font {
            Some(font) => select_object(hdc, font as HGDIOBJ),
            None => select_fixed_font(hdc),
        };
        Self { hdc, previous }
    }
}

impl Drop for SelectedObject {
    fn drop(&mut self) {
        if let Some(previous) = self.previous {
            // SAFETY: previous was returned by SelectObject for this HDC.
            unsafe {
                SelectObject(self.hdc, previous);
            }
        }
    }
}

#[derive(Debug)]
struct FontResources {
    font: OwnedFont,
    dpi_y: i32,
}

impl FontResources {
    fn new(hdc: HDC, font: &TerminalFont) -> AppResult<Self> {
        let dpi_y = device_dpi_y(hdc);
        Ok(Self {
            font: OwnedFont::new(font, dpi_y)?,
            dpi_y,
        })
    }
}

#[derive(Debug)]
struct OwnedFont {
    handle: HFONT,
}

impl OwnedFont {
    fn new(font: &TerminalFont, dpi_y: i32) -> AppResult<Self> {
        let family = wide_null(font.family());
        let height = point_size_to_logical_height(font.size_points(), dpi_y);

        // SAFETY: family is a valid null-terminated UTF-16 face name for the duration of the call.
        let handle = unsafe {
            CreateFontW(
                height,
                0,
                0,
                0,
                FW_NORMAL as i32,
                0,
                0,
                0,
                u32::from(DEFAULT_CHARSET),
                u32::from(OUT_DEFAULT_PRECIS),
                u32::from(CLIP_DEFAULT_PRECIS),
                u32::from(DEFAULT_QUALITY),
                u32::from(FIXED_PITCH | FF_MODERN),
                family.as_ptr(),
            )
        };
        if handle.is_null() {
            Err(AppError::win32("CreateFontW terminal font"))
        } else {
            Ok(Self { handle })
        }
    }

    fn handle(&self) -> HFONT {
        self.handle
    }
}

impl Drop for OwnedFont {
    fn drop(&mut self) {
        if self.handle.is_null() {
            return;
        }

        // SAFETY: handle is an owned font created by CreateFontW.
        unsafe {
            DeleteObject(self.handle);
        }
    }
}

#[derive(Debug, Default)]
struct SolidBrushCache {
    brushes: Vec<CachedBrush>,
}

impl SolidBrushCache {
    fn get(&mut self, color: COLORREF) -> AppResult<HBRUSH> {
        if let Some(cached) = self.brushes.iter().find(|cached| cached.color == color) {
            return Ok(cached.brush.handle());
        }

        self.brushes
            .try_reserve(1)
            .map_err(|_| AppError::InvalidInput("too many brushes to cache"))?;
        let brush = OwnedBrush::solid(color)?;
        let handle = brush.handle();
        self.brushes.push(CachedBrush { color, brush });
        Ok(handle)
    }
}

#[derive(Debug)]
struct CachedBrush {
    color: COLORREF,
    brush: OwnedBrush,
}

#[derive(Debug)]
struct OwnedBrush {
    handle: HBRUSH,
}

impl OwnedBrush {
    fn solid(color: COLORREF) -> AppResult<Self> {
        // SAFETY: CreateSolidBrush returns an owned brush handle that Drop deletes.
        let handle = unsafe { CreateSolidBrush(color) };
        if handle.is_null() {
            Err(AppError::win32("CreateSolidBrush"))
        } else {
            Ok(Self { handle })
        }
    }

    fn handle(&self) -> HBRUSH {
        self.handle
    }
}

impl Drop for OwnedBrush {
    fn drop(&mut self) {
        // SAFETY: handle is an owned GDI brush created by CreateSolidBrush.
        unsafe {
            DeleteObject(self.handle);
        }
    }
}

fn select_fixed_font(hdc: HDC) -> Option<HGDIOBJ> {
    // SAFETY: SYSTEM_FIXED_FONT is a stock object managed by GDI and valid for selection.
    unsafe {
        let font = GetStockObject(SYSTEM_FIXED_FONT);
        if font.is_null() {
            return None;
        }

        select_object(hdc, font)
    }
}

fn select_object(hdc: HDC, object: HGDIOBJ) -> Option<HGDIOBJ> {
    // SAFETY: object is a live GDI object compatible with the target HDC.
    let previous = unsafe { SelectObject(hdc, object) };
    if previous.is_null() {
        None
    } else {
        Some(previous)
    }
}

fn device_dpi_y(hdc: HDC) -> i32 {
    // SAFETY: hdc is a live device context and LOGPIXELSY is an immutable capability query.
    let dpi_y = unsafe { GetDeviceCaps(hdc, LOGPIXELSY as i32) };
    dpi_y.max(96)
}

fn point_size_to_logical_height(size_points: u16, dpi_y: i32) -> i32 {
    let pixels = i32::from(size_points)
        .saturating_mul(dpi_y.max(1))
        .saturating_add(36)
        / 72;
    pixels.max(1).saturating_neg()
}

fn wide_null(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn measure_cell_metrics(hdc: HDC) -> CellMetrics {
    let mut metrics = MaybeUninit::<TEXTMETRICW>::zeroed();

    // SAFETY: metrics points to valid writable memory for GetTextMetricsW.
    let ok = unsafe { GetTextMetricsW(hdc, metrics.as_mut_ptr()) };
    if ok == 0 {
        return CellMetrics::default();
    }

    // SAFETY: GetTextMetricsW succeeded and initialized metrics.
    let metrics = unsafe { metrics.assume_init() };
    CellMetrics {
        width: metrics.tmAveCharWidth.max(1),
        height: metrics.tmHeight.max(1),
    }
}

fn rect_from_ui(rect: UiRect) -> RECT {
    RECT {
        left: rect.x,
        top: rect.y,
        right: rect.x.saturating_add(rect.width),
        bottom: rect.y.saturating_add(rect.height),
    }
}

fn rects_intersect(left: &RECT, right: &RECT) -> bool {
    intersect_rects(left, right).is_some()
}

fn intersect_rects(left: &RECT, right: &RECT) -> Option<RECT> {
    let rect = RECT {
        left: left.left.max(right.left),
        top: left.top.max(right.top),
        right: left.right.min(right.right),
        bottom: left.bottom.min(right.bottom),
    };

    if rect.right <= rect.left || rect.bottom <= rect.top {
        None
    } else {
        Some(rect)
    }
}

fn terminal_paint_row_range(
    paint_rect: &RECT,
    terminal_content: UiRect,
    rows: usize,
    cell_height: i32,
) -> Range<usize> {
    if rows == 0 {
        return 0..0;
    }

    let content_top = terminal_content.y;
    let content_bottom = terminal_content.y.saturating_add(terminal_content.height);
    let paint_top = paint_rect.top.max(content_top);
    let paint_bottom = paint_rect.bottom.min(content_bottom);
    if paint_bottom <= paint_top {
        return 0..0;
    }

    let cell_height = cell_height.max(1);
    let start_offset = paint_top.saturating_sub(content_top);
    let end_offset = paint_bottom.saturating_sub(content_top);
    let start = usize_from_i32_saturating(start_offset / cell_height).min(rows);
    let end = usize_from_i32_saturating(
        end_offset.saturating_add(cell_height.saturating_sub(1)) / cell_height,
    )
    .min(rows);

    start..end
}

fn terminal_row_range_rect(
    content_area: UiRect,
    metrics: CellMetrics,
    rows: Range<usize>,
    visible_rows: usize,
) -> Option<UiRect> {
    let start = rows.start.min(visible_rows);
    let end = rows.end.min(visible_rows);
    if start >= end || content_area.width <= 0 || content_area.height <= 0 {
        return None;
    }

    let row_height = metrics.height.max(1);
    let top_offset = i32_from_usize_saturating(start).saturating_mul(row_height);
    if top_offset >= content_area.height {
        return None;
    }

    let bottom_offset = if end >= visible_rows {
        content_area.height
    } else {
        i32_from_usize_saturating(end)
            .saturating_mul(row_height)
            .min(content_area.height)
    };
    let height = bottom_offset.saturating_sub(top_offset);
    if height <= 0 {
        return None;
    }

    Some(UiRect {
        x: content_area.x,
        y: content_area.y.saturating_add(top_offset),
        width: content_area.width,
        height,
    })
}

fn terminal_row_clip_rect(content_area: UiRect, metrics: CellMetrics, row: usize) -> Option<RECT> {
    if content_area.width <= 0 || content_area.height <= 0 {
        return None;
    }

    let row_height = metrics.height.max(1);
    let top_offset = i32_from_usize_saturating(row).saturating_mul(row_height);
    if top_offset >= content_area.height {
        return None;
    }

    Some(RECT {
        left: content_area.x,
        top: content_area.y.saturating_add(top_offset),
        right: content_area.x.saturating_add(content_area.width),
        bottom: content_area.y.saturating_add(
            top_offset
                .saturating_add(row_height)
                .min(content_area.height),
        ),
    })
}

fn usize_from_i32_saturating(value: i32) -> usize {
    if value <= 0 {
        0
    } else {
        match usize::try_from(value) {
            Ok(value) => value,
            Err(_) => usize::MAX,
        }
    }
}

fn i32_from_usize_saturating(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

fn cell_rect(area: UiRect, metrics: CellMetrics, row: usize, column: usize) -> AppResult<RECT> {
    let row = i32::try_from(row).map_err(|_| AppError::InvalidInput("row index is too large"))?;
    let column =
        i32::try_from(column).map_err(|_| AppError::InvalidInput("column index is too large"))?;
    let left = area
        .x
        .saturating_add(column.saturating_mul(metrics.width.max(1)));
    let top = area
        .y
        .saturating_add(row.saturating_mul(metrics.height.max(1)));

    Ok(RECT {
        left,
        top,
        right: left.saturating_add(metrics.width.max(1)),
        bottom: top.saturating_add(metrics.height.max(1)),
    })
}

fn selection_rect(
    area: UiRect,
    metrics: CellMetrics,
    row: usize,
    columns: Range<usize>,
) -> AppResult<RECT> {
    let row = i32::try_from(row).map_err(|_| AppError::InvalidInput("row index is too large"))?;
    let start = i32::try_from(columns.start)
        .map_err(|_| AppError::InvalidInput("column index is too large"))?;
    let end = i32::try_from(columns.end)
        .map_err(|_| AppError::InvalidInput("column index is too large"))?;
    let cell_width = metrics.width.max(1);
    let cell_height = metrics.height.max(1);
    let left = area.x.saturating_add(start.saturating_mul(cell_width));
    let top = area.y.saturating_add(row.saturating_mul(cell_height));
    let right = area.x.saturating_add(end.saturating_mul(cell_width));

    Ok(RECT {
        left,
        top,
        right,
        bottom: top.saturating_add(cell_height),
    })
}

const fn rgb(red: u8, green: u8, blue: u8) -> COLORREF {
    red as COLORREF | ((green as COLORREF) << 8) | ((blue as COLORREF) << 16)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terminal_cells(text: &str) -> Vec<TerminalCell> {
        text.chars().map(TerminalCell::new).collect()
    }

    #[test]
    fn terminal_paint_row_range_limits_rows_to_paint_rect() {
        let terminal_content = UiRect {
            x: 8,
            y: 20,
            width: 320,
            height: 80,
        };
        let paint_rect = RECT {
            left: 0,
            top: 36,
            right: 360,
            bottom: 52,
        };

        assert_eq!(
            terminal_paint_row_range(&paint_rect, terminal_content, 5, 16),
            1..2
        );
    }

    #[test]
    fn terminal_paint_row_range_includes_partially_painted_rows() {
        let terminal_content = UiRect {
            x: 8,
            y: 20,
            width: 320,
            height: 80,
        };
        let paint_rect = RECT {
            left: 0,
            top: 35,
            right: 360,
            bottom: 53,
        };

        assert_eq!(
            terminal_paint_row_range(&paint_rect, terminal_content, 5, 16),
            0..3
        );
    }

    #[test]
    fn terminal_paint_row_range_is_empty_outside_terminal_content() {
        let terminal_content = UiRect {
            x: 8,
            y: 20,
            width: 320,
            height: 80,
        };
        let paint_rect = RECT {
            left: 0,
            top: 0,
            right: 360,
            bottom: 20,
        };

        assert_eq!(
            terminal_paint_row_range(&paint_rect, terminal_content, 5, 16),
            0..0
        );
    }

    #[test]
    fn intersect_rects_returns_overlapping_band() {
        let left = RECT {
            left: 8,
            top: 20,
            right: 108,
            bottom: 68,
        };
        let right = RECT {
            left: 0,
            top: 36,
            right: 360,
            bottom: 52,
        };

        let Some(rect) = intersect_rects(&left, &right) else {
            panic!("overlapping rects should produce an intersection");
        };

        assert_eq!(rect.left, 8);
        assert_eq!(rect.top, 36);
        assert_eq!(rect.right, 108);
        assert_eq!(rect.bottom, 52);
    }

    #[test]
    fn intersect_rects_rejects_non_overlapping_edges() {
        let left = RECT {
            left: 8,
            top: 20,
            right: 108,
            bottom: 36,
        };
        let right = RECT {
            left: 0,
            top: 36,
            right: 360,
            bottom: 52,
        };

        assert!(intersect_rects(&left, &right).is_none());
        assert!(!rects_intersect(&left, &right));
    }

    #[test]
    fn terminal_row_range_rect_extends_last_row_to_content_bottom() {
        let content_area = UiRect {
            x: 8,
            y: 20,
            width: 320,
            height: 38,
        };
        let metrics = CellMetrics {
            width: 8,
            height: 16,
        };

        let Some(rect) = terminal_row_range_rect(content_area, metrics, 1..2, 2) else {
            panic!("last row should include trailing terminal content");
        };

        assert_eq!(
            rect,
            UiRect {
                x: 8,
                y: 36,
                width: 320,
                height: 22,
            }
        );
    }

    #[test]
    fn terminal_row_clip_rect_stays_inside_content_area() {
        let content_area = UiRect {
            x: 8,
            y: 20,
            width: 320,
            height: 38,
        };
        let metrics = CellMetrics {
            width: 8,
            height: 16,
        };

        let Some(rect) = terminal_row_clip_rect(content_area, metrics, 2) else {
            panic!("partial last row should produce a clip rect");
        };

        assert_eq!(rect.left, 8);
        assert_eq!(rect.top, 52);
        assert_eq!(rect.right, 328);
        assert_eq!(rect.bottom, 58);
    }

    #[test]
    fn paint_buffer_size_rejects_empty_rects() {
        let rect = RECT {
            left: 10,
            top: 10,
            right: 10,
            bottom: 24,
        };

        assert_eq!(paint_buffer_size(&rect), None);
    }

    #[test]
    fn paint_buffer_size_uses_rect_extent() {
        let rect = RECT {
            left: 8,
            top: 20,
            right: 108,
            bottom: 68,
        };

        assert_eq!(
            paint_buffer_size(&rect),
            Some(PaintBufferSize {
                width: 100,
                height: 48,
            })
        );
    }

    #[test]
    fn paint_buffer_size_covers_smaller_required_extent() {
        let cached = PaintBufferSize {
            width: 320,
            height: 200,
        };

        assert!(cached.covers(PaintBufferSize {
            width: 240,
            height: 120,
        }));
        assert!(!cached.covers(PaintBufferSize {
            width: 480,
            height: 120,
        }));
    }

    #[test]
    fn paint_buffer_size_reuses_cached_extent_within_area_budget() {
        let cached = PaintBufferSize {
            width: 400,
            height: 200,
        };

        assert!(cached.reusable_for(PaintBufferSize {
            width: 200,
            height: 100,
        }));
    }

    #[test]
    fn paint_buffer_size_rejects_cached_extent_above_area_budget() {
        let cached = PaintBufferSize {
            width: 1600,
            height: 900,
        };

        assert!(!cached.reusable_for(PaintBufferSize {
            width: 400,
            height: 225,
        }));
    }

    #[test]
    fn paint_buffer_size_reuse_requires_covering_required_extent() {
        let cached = PaintBufferSize {
            width: 320,
            height: 200,
        };

        assert!(!cached.reusable_for(PaintBufferSize {
            width: 480,
            height: 120,
        }));
    }

    #[test]
    fn encoded_line_refresh_trims_trailing_whitespace() -> AppResult<()> {
        let cells = terminal_cells("ab  ");
        let mut line = EncodedLine::default();

        line.refresh_utf16(&cells, Some(1), 8)?;
        let wide = line.utf16().to_vec();
        let fingerprint = terminal_cells_fingerprint(&cells);

        assert_eq!(wide, vec![b'a' as u16, b'b' as u16]);
        assert_eq!(line.utf16(), wide.as_slice());
        assert_eq!(line.advances(), &[8, 8]);
        assert!(line.is_valid_for(cells.len(), Some(1), 8, fingerprint));
        assert!(!line.is_valid_for(cells.len(), Some(2), 8, fingerprint));
        assert!(!line.is_valid_for(cells.len(), Some(1), 9, fingerprint));
        Ok(())
    }

    #[test]
    fn encoded_line_rejects_same_version_with_changed_cells() -> AppResult<()> {
        let cells = terminal_cells("stale");
        let changed = terminal_cells("     ");
        let mut line = EncodedLine::default();

        line.refresh_utf16(&cells, Some(1), 8)?;

        assert!(!line.is_valid_for(
            changed.len(),
            Some(1),
            8,
            terminal_cells_fingerprint(&changed)
        ));
        Ok(())
    }

    #[test]
    fn encoded_line_refresh_reserves_only_visible_prefix() -> AppResult<()> {
        let mut cells = terminal_cells("ab");
        cells.extend((0..512).map(|_| TerminalCell::new(' ')));
        let mut line = EncodedLine::default();

        line.refresh_utf16(&cells, Some(1), 8)?;
        let wide = line.utf16().to_vec();

        assert_eq!(wide, vec![b'a' as u16, b'b' as u16]);
        assert!(line.wide.capacity() < cells.len());
        Ok(())
    }

    #[test]
    fn encoded_line_refresh_preserves_non_bmp_character() -> AppResult<()> {
        let cells = vec![
            TerminalCell::new('a'),
            TerminalCell::new('\u{1F642}'),
            TerminalCell::new(' '),
            TerminalCell::new(' '),
        ];
        let mut line = EncodedLine::default();

        line.refresh_utf16(&cells, Some(1), 8)?;
        let wide = line.utf16().to_vec();
        let expected = "a\u{1F642}".encode_utf16().collect::<Vec<_>>();

        assert_eq!(wide, expected);
        assert_eq!(line.utf16(), expected.as_slice());
        assert_eq!(line.advances(), &[8, 8, 0]);
        Ok(())
    }

    #[test]
    fn encoded_line_refresh_uses_cell_advances_for_wide_character_spacers() -> AppResult<()> {
        let cells = vec![
            TerminalCell::new('오'),
            TerminalCell::new(' '),
            TerminalCell::new('후'),
            TerminalCell::new(' '),
            TerminalCell::new('P'),
        ];
        let mut line = EncodedLine::default();

        line.refresh_utf16(&cells, Some(1), 9)?;

        assert_eq!(line.utf16(), "오 후 P".encode_utf16().collect::<Vec<_>>());
        assert_eq!(line.advances(), &[9, 9, 9, 9, 9]);
        Ok(())
    }

    #[test]
    fn encoded_line_refresh_releases_oversized_blank_row_cache() -> AppResult<()> {
        let cells = terminal_cells("    ");
        let mut line = EncodedLine::default();
        line.wide
            .try_reserve(4096)
            .map_err(|_| AppError::InvalidInput("test allocation failed"))?;

        assert!(line.wide.capacity() > 0);
        line.refresh_utf16(&cells, Some(7), 8)?;

        assert!(line.utf16().is_empty());
        assert_eq!(line.wide.capacity(), 0);
        assert_eq!(line.advances.capacity(), 0);
        Ok(())
    }

    #[test]
    fn encoded_line_refresh_caches_blank_rows_as_valid() -> AppResult<()> {
        let cells = terminal_cells("    ");
        let mut line = EncodedLine::default();

        line.refresh_utf16(&cells, Some(7), 8)?;

        assert!(line.utf16().is_empty());
        assert!(line.advances().is_empty());
        let fingerprint = terminal_cells_fingerprint(&cells);
        assert!(line.is_valid_for(cells.len(), Some(7), 8, fingerprint));
        assert!(!line.is_valid_for(cells.len().saturating_add(1), Some(7), 8, fingerprint));
        assert!(!line.is_valid_for(cells.len(), None, 8, fingerprint));
        Ok(())
    }
}
