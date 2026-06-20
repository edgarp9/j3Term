use super::command::{CommandButton, CommandButtonId};
use super::terminal::{TerminalTabId, TerminalTabView};

pub const COMMAND_PANEL_WIDTH: i32 = 128;
pub const COMMAND_PANEL_MIN_WIDTH: i32 = 64;
pub const TERMINAL_MIN_WIDTH: i32 = 80;
pub const COMMAND_BUTTON_WIDTH: i32 = 104;
pub const COMMAND_BUTTON_HEIGHT: i32 = 28;
pub const COMMAND_BUTTON_GAP: i32 = 8;
pub const COMMAND_BUTTON_SCROLLBAR_WIDTH: i32 = 17;
pub const COMMAND_BUTTON_SCROLLBAR_GAP: i32 = 4;
pub const TERMINAL_SCROLLBAR_WIDTH: i32 = 17;
pub const TERMINAL_SCROLLBAR_GAP: i32 = 4;
pub const COMMAND_CATEGORY_SELECTOR_HEIGHT: i32 = 40;
pub const COMMAND_CATEGORY_SELECTOR_DROPDOWN_HEIGHT: i32 = 160;
pub const COMMAND_PANEL_HEADER_GAP: i32 = 8;
pub const SPLITTER_WIDTH: i32 = 6;
pub const TAB_BAR_HEIGHT: i32 = 28;
pub const TAB_MAX_WIDTH: i32 = 104;
pub const TAB_CLOSE_BUTTON_SIZE: i32 = 16;
pub const NEW_TAB_BUTTON_WIDTH: i32 = 26;
pub const TAB_BUTTON_GAP: i32 = 4;
pub const UI_PADDING: i32 = 8;
pub const TERMINAL_PADDING: i32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiPoint {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl UiRect {
    pub fn contains(&self, point: UiPoint) -> bool {
        point.x >= self.x
            && point.y >= self.y
            && point.x < self.x.saturating_add(self.width)
            && point.y < self.y.saturating_add(self.height)
    }

    pub fn inset(&self, padding: i32) -> Self {
        let padding = padding.max(0);
        let inset_x = padding.min(self.width.max(0) / 2);
        let inset_y = padding.min(self.height.max(0) / 2);

        Self {
            x: self.x.saturating_add(inset_x),
            y: self.y.saturating_add(inset_y),
            width: self.width.saturating_sub(inset_x.saturating_mul(2)),
            height: self.height.saturating_sub(inset_y.saturating_mul(2)),
        }
    }
}

pub fn terminal_content_area(terminal_area: UiRect) -> UiRect {
    let mut area = terminal_area.inset(TERMINAL_PADDING);
    area.width = area
        .width
        .saturating_sub(terminal_scrollbar_reserved_width(terminal_area));
    area
}

pub fn terminal_scrollbar_bounds(terminal_area: UiRect) -> Option<UiRect> {
    if terminal_area.width
        <= TERMINAL_PADDING
            .saturating_mul(2)
            .saturating_add(TERMINAL_SCROLLBAR_WIDTH)
            .saturating_add(TERMINAL_SCROLLBAR_GAP)
        || terminal_area.height <= 0
    {
        return None;
    }

    let width = TERMINAL_SCROLLBAR_WIDTH.min(terminal_area.width.max(0));
    Some(UiRect {
        x: terminal_area
            .x
            .saturating_add(terminal_area.width.saturating_sub(width)),
        y: terminal_area.y,
        width,
        height: terminal_area.height,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandButtonPlacement {
    pub id: CommandButtonId,
    pub label: String,
    pub bounds: UiRect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandButtonScroll {
    pub bounds: UiRect,
    pub position: usize,
    pub max_position: usize,
    pub page_len: usize,
    pub total_len: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabPlacement {
    pub id: TerminalTabId,
    pub bounds: UiRect,
    pub close_bounds: Option<UiRect>,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowLayout {
    pub tab_bar: UiRect,
    pub command_panel: UiRect,
    pub command_category_selector: Option<UiRect>,
    pub command_button_viewport: UiRect,
    pub command_button_scroll: Option<CommandButtonScroll>,
    pub splitter: UiRect,
    pub terminal: UiRect,
    pub terminal_scrollbar: Option<UiRect>,
    pub buttons: Vec<CommandButtonPlacement>,
    pub tabs: Vec<TabPlacement>,
    pub new_tab_button: Option<UiRect>,
}

impl WindowLayout {
    pub fn for_client_with_command_panel_width(
        client_width: i32,
        client_height: i32,
        command_panel_width: i32,
        buttons: &[CommandButton],
        tabs: &[TerminalTabView],
    ) -> Self {
        Self::for_client_with_command_panel_width_and_button_scroll(
            client_width,
            client_height,
            command_panel_width,
            buttons,
            tabs,
            0,
        )
    }

    pub fn for_client_with_command_panel_width_and_button_scroll(
        client_width: i32,
        client_height: i32,
        command_panel_width: i32,
        buttons: &[CommandButton],
        tabs: &[TerminalTabView],
        command_button_scroll_position: usize,
    ) -> Self {
        let width = client_width.max(1);
        let height = client_height.max(1);
        let tab_bar_height = tab_bar_height_for_client(height);
        let (terminal_width, splitter_width, command_panel_width) =
            split_widths_for_client(width, command_panel_width);
        let content_height = height.saturating_sub(tab_bar_height).max(1);
        let tab_bar = UiRect {
            x: 0,
            y: 0,
            width,
            height: tab_bar_height,
        };
        let splitter = UiRect {
            x: terminal_width,
            y: tab_bar_height,
            width: splitter_width,
            height: content_height,
        };
        let command_panel = UiRect {
            x: terminal_width.saturating_add(splitter_width),
            y: tab_bar_height,
            width: command_panel_width,
            height: content_height,
        };
        let terminal = UiRect {
            x: 0,
            y: tab_bar_height,
            width: terminal_width,
            height: content_height,
        };
        let terminal_scrollbar = terminal_scrollbar_bounds(terminal);

        let command_button_viewport = if command_panel.width > 0 {
            place_command_button_viewport(command_panel)
        } else {
            UiRect {
                x: command_panel.x,
                y: command_panel.y,
                width: 0,
                height: 0,
            }
        };
        let (placements, command_button_scroll) = if command_panel.width > 0 {
            place_command_buttons(
                command_button_viewport,
                buttons,
                command_button_scroll_position,
            )
        } else {
            (Vec::new(), None)
        };
        let command_category_selector = if command_panel.width > 0 {
            place_command_category_selector(command_panel)
        } else {
            None
        };
        let (tab_placements, new_tab_button) = place_tabs(tab_bar, tabs);

        Self {
            tab_bar,
            command_panel,
            command_category_selector,
            command_button_viewport,
            command_button_scroll,
            splitter,
            terminal,
            terminal_scrollbar,
            buttons: placements,
            tabs: tab_placements,
            new_tab_button,
        }
    }

    pub fn tab_at(&self, point: UiPoint) -> Option<TerminalTabId> {
        self.tabs
            .iter()
            .find(|placement| placement.bounds.contains(point))
            .map(|placement| placement.id)
    }

    pub fn tab_close_at(&self, point: UiPoint) -> Option<TerminalTabId> {
        self.tabs
            .iter()
            .filter_map(|placement| placement.close_bounds.map(|bounds| (placement.id, bounds)))
            .find(|(_, bounds)| bounds.contains(point))
            .map(|(id, _)| id)
    }

    pub fn new_tab_at(&self, point: UiPoint) -> bool {
        self.new_tab_button
            .is_some_and(|bounds| bounds.contains(point))
    }

    #[cfg(any(target_os = "linux", test))]
    pub fn command_button_at(&self, point: UiPoint) -> Option<CommandButtonId> {
        self.buttons
            .iter()
            .find(|placement| placement.bounds.contains(point))
            .map(|placement| placement.id)
    }

    pub fn splitter_at(&self, point: UiPoint) -> bool {
        self.splitter.contains(point)
    }

    pub fn command_button_scroll_position(&self) -> usize {
        self.command_button_scroll
            .as_ref()
            .map_or(0, |scroll| scroll.position)
    }

    pub fn command_panel_width_from_splitter_x(client_width: i32, splitter_x: i32) -> i32 {
        let width = client_width.max(1);
        let splitter_width = splitter_width_for_client(width);
        if splitter_width <= 0 {
            return 0;
        }

        let command_panel_width = width
            .saturating_sub(splitter_width)
            .saturating_sub(splitter_x);
        command_panel_width_for_client(width, command_panel_width)
    }

    /// Updates only the geometry affected by a command panel width change.
    ///
    /// Returns `false` when the current button placements cannot be reused and
    /// the caller should rebuild the full layout.
    pub fn try_resize_command_panel_width(
        &mut self,
        client_width: i32,
        client_height: i32,
        command_panel_width: i32,
        buttons: &[CommandButton],
        requested_scroll_position: usize,
    ) -> bool {
        let width = client_width.max(1);
        let height = client_height.max(1);
        let tab_bar_height = tab_bar_height_for_client(height);
        let content_height = height.saturating_sub(tab_bar_height).max(1);
        if self.tab_bar.width != width
            || self.tab_bar.height != tab_bar_height
            || self.terminal.y != tab_bar_height
            || self.terminal.height != content_height
        {
            return false;
        }

        let (terminal_width, splitter_width, command_panel_width) =
            split_widths_for_client(width, command_panel_width);
        let splitter = UiRect {
            x: terminal_width,
            y: tab_bar_height,
            width: splitter_width,
            height: content_height,
        };
        let command_panel = UiRect {
            x: terminal_width.saturating_add(splitter_width),
            y: tab_bar_height,
            width: command_panel_width,
            height: content_height,
        };
        let terminal = UiRect {
            x: 0,
            y: tab_bar_height,
            width: terminal_width,
            height: content_height,
        };
        let command_button_viewport = if command_panel.width > 0 {
            place_command_button_viewport(command_panel)
        } else {
            UiRect {
                x: command_panel.x,
                y: command_panel.y,
                width: 0,
                height: 0,
            }
        };
        let command_category_selector = if command_panel.width > 0 {
            place_command_category_selector(command_panel)
        } else {
            None
        };
        let visible_slots = command_button_visible_slots(command_button_viewport.height);
        let mut command_button_scroll = None;
        let mut position = 0;
        let mut lane = None;

        if command_panel.width > 0
            && command_button_viewport.width > 0
            && !buttons.is_empty()
            && visible_slots > 0
        {
            let total_len = buttons.len();
            let max_position = total_len.saturating_sub(visible_slots);
            position = requested_scroll_position.min(max_position);
            command_button_scroll = if total_len > visible_slots {
                Some(CommandButtonScroll {
                    bounds: command_button_scrollbar_bounds(command_button_viewport),
                    position,
                    max_position,
                    page_len: visible_slots,
                    total_len,
                })
            } else {
                None
            };

            let lane_bounds = command_button_lane_bounds(
                command_button_viewport,
                command_button_scroll.is_some(),
            );
            if lane_bounds.width > 0 {
                lane = Some(lane_bounds);
            }
        }

        if !self.command_button_placements_match(buttons, position, visible_slots, lane) {
            return false;
        }

        self.splitter = splitter;
        self.command_panel = command_panel;
        self.command_category_selector = command_category_selector;
        self.command_button_viewport = command_button_viewport;
        self.command_button_scroll = command_button_scroll;
        self.terminal = terminal;
        self.terminal_scrollbar = terminal_scrollbar_bounds(terminal);
        self.update_command_button_bounds(visible_slots, lane);
        true
    }

    fn command_button_placements_match(
        &self,
        buttons: &[CommandButton],
        position: usize,
        visible_slots: usize,
        lane: Option<UiRect>,
    ) -> bool {
        let Some(lane) = lane else {
            return self.buttons.is_empty();
        };

        let expected_candidates = buttons.len().saturating_sub(position).min(visible_slots);
        let expected_len = command_button_placement_count(lane, expected_candidates);
        if self.buttons.len() != expected_len {
            return false;
        }

        self.buttons
            .iter()
            .zip(buttons.iter().skip(position).take(visible_slots))
            .take(expected_len)
            .all(|(placement, button)| placement.id == button.id)
    }

    fn update_command_button_bounds(&mut self, visible_slots: usize, lane: Option<UiRect>) {
        let Some(lane) = lane else {
            self.buttons.clear();
            return;
        };

        let width = COMMAND_BUTTON_WIDTH.min(lane.width);
        let x = lane
            .x
            .saturating_add((lane.width.saturating_sub(width)) / 2);
        let mut y = lane.y;
        let bottom = lane.y.saturating_add(lane.height);

        for placement in self.buttons.iter_mut().take(visible_slots) {
            if y.saturating_add(COMMAND_BUTTON_HEIGHT) > bottom {
                break;
            }

            placement.bounds = UiRect {
                x,
                y,
                width,
                height: COMMAND_BUTTON_HEIGHT,
            };
            y = y
                .saturating_add(COMMAND_BUTTON_HEIGHT)
                .saturating_add(COMMAND_BUTTON_GAP);
        }
    }
}

fn tab_bar_height_for_client(client_height: i32) -> i32 {
    if client_height <= 1 {
        0
    } else {
        TAB_BAR_HEIGHT.min(client_height.saturating_sub(1))
    }
}

fn split_widths_for_client(client_width: i32, command_panel_width: i32) -> (i32, i32, i32) {
    let width = client_width.max(1);
    let splitter_width = splitter_width_for_client(width);
    let command_panel_width = command_panel_width_for_client(width, command_panel_width);
    let terminal_width = width
        .saturating_sub(splitter_width)
        .saturating_sub(command_panel_width)
        .max(1);

    (terminal_width, splitter_width, command_panel_width)
}

fn splitter_width_for_client(client_width: i32) -> i32 {
    if client_width <= 2 {
        0
    } else {
        SPLITTER_WIDTH.min(client_width.saturating_sub(2))
    }
}

fn command_panel_width_for_client(client_width: i32, command_panel_width: i32) -> i32 {
    if client_width <= 1 {
        return 0;
    }

    let available_width = client_width.saturating_sub(splitter_width_for_client(client_width));
    let (min_width, max_width) = command_panel_width_bounds(available_width);
    command_panel_width.clamp(min_width, max_width)
}

fn command_panel_width_bounds(available_width: i32) -> (i32, i32) {
    if available_width <= 1 {
        return (0, 0);
    }

    if available_width < COMMAND_PANEL_MIN_WIDTH.saturating_add(TERMINAL_MIN_WIDTH) {
        let half_width = (available_width / 2).max(1);
        return (half_width, half_width);
    }

    (
        COMMAND_PANEL_MIN_WIDTH,
        available_width.saturating_sub(TERMINAL_MIN_WIDTH),
    )
}

fn place_command_buttons(
    command_button_viewport: UiRect,
    buttons: &[CommandButton],
    requested_scroll_position: usize,
) -> (Vec<CommandButtonPlacement>, Option<CommandButtonScroll>) {
    let visible_slots = command_button_visible_slots(command_button_viewport.height);
    if command_button_viewport.width <= 0 || buttons.is_empty() || visible_slots == 0 {
        return (Vec::new(), None);
    }

    let total_len = buttons.len();
    let max_position = total_len.saturating_sub(visible_slots);
    let position = requested_scroll_position.min(max_position);
    let scroll = if total_len > visible_slots {
        Some(CommandButtonScroll {
            bounds: command_button_scrollbar_bounds(command_button_viewport),
            position,
            max_position,
            page_len: visible_slots,
            total_len,
        })
    } else {
        None
    };
    let lane = command_button_lane_bounds(command_button_viewport, scroll.is_some());
    if lane.width <= 0 {
        return (Vec::new(), scroll);
    }

    let mut placements = Vec::with_capacity(visible_slots.min(total_len));
    let width = COMMAND_BUTTON_WIDTH.min(lane.width);
    let x = lane
        .x
        .saturating_add((lane.width.saturating_sub(width)) / 2);
    let mut y = command_button_viewport.y;

    for button in buttons.iter().skip(position).take(visible_slots) {
        if y.saturating_add(COMMAND_BUTTON_HEIGHT)
            > command_button_viewport
                .y
                .saturating_add(command_button_viewport.height)
        {
            break;
        }

        placements.push(CommandButtonPlacement {
            id: button.id,
            label: button.label.clone(),
            bounds: UiRect {
                x,
                y,
                width,
                height: COMMAND_BUTTON_HEIGHT,
            },
        });
        y = y
            .saturating_add(COMMAND_BUTTON_HEIGHT)
            .saturating_add(COMMAND_BUTTON_GAP);
    }

    (placements, scroll)
}

fn place_command_button_viewport(command_panel: UiRect) -> UiRect {
    let top = command_panel
        .y
        .saturating_add(UI_PADDING)
        .saturating_add(COMMAND_CATEGORY_SELECTOR_HEIGHT)
        .saturating_add(COMMAND_PANEL_HEADER_GAP);
    let bottom = command_panel
        .y
        .saturating_add(command_panel.height)
        .saturating_sub(UI_PADDING);

    UiRect {
        x: command_panel.x.saturating_add(UI_PADDING),
        y: top,
        width: command_panel
            .width
            .saturating_sub(UI_PADDING.saturating_mul(2)),
        height: bottom.saturating_sub(top),
    }
}

fn command_button_visible_slots(height: i32) -> usize {
    if height < COMMAND_BUTTON_HEIGHT {
        return 0;
    }

    let slots = height.saturating_add(COMMAND_BUTTON_GAP)
        / COMMAND_BUTTON_HEIGHT.saturating_add(COMMAND_BUTTON_GAP);
    match usize::try_from(slots) {
        Ok(slots) => slots,
        Err(_) => usize::MAX,
    }
}

fn command_button_placement_count(lane: UiRect, visible_slots: usize) -> usize {
    let bottom = lane.y.saturating_add(lane.height);
    let mut y = lane.y;
    let mut count = 0;

    for _ in 0..visible_slots {
        if y.saturating_add(COMMAND_BUTTON_HEIGHT) > bottom {
            break;
        }

        count += 1;
        y = y
            .saturating_add(COMMAND_BUTTON_HEIGHT)
            .saturating_add(COMMAND_BUTTON_GAP);
    }

    count
}

fn command_button_lane_bounds(viewport: UiRect, has_scrollbar: bool) -> UiRect {
    if !has_scrollbar {
        return viewport;
    }

    let scrollbar_width = command_button_scrollbar_width(viewport);
    UiRect {
        x: viewport.x,
        y: viewport.y,
        width: viewport
            .width
            .saturating_sub(scrollbar_width)
            .saturating_sub(COMMAND_BUTTON_SCROLLBAR_GAP),
        height: viewport.height,
    }
}

fn command_button_scrollbar_bounds(viewport: UiRect) -> UiRect {
    let width = command_button_scrollbar_width(viewport);
    UiRect {
        x: viewport
            .x
            .saturating_add(viewport.width.saturating_sub(width)),
        y: viewport.y,
        width,
        height: viewport.height,
    }
}

fn command_button_scrollbar_width(viewport: UiRect) -> i32 {
    COMMAND_BUTTON_SCROLLBAR_WIDTH.min(viewport.width.max(0))
}

fn terminal_scrollbar_reserved_width(terminal_area: UiRect) -> i32 {
    terminal_scrollbar_bounds(terminal_area).map_or(0, |bounds| {
        bounds
            .width
            .saturating_add(TERMINAL_SCROLLBAR_GAP)
            .min(terminal_area.width.max(0))
    })
}

fn place_command_category_selector(command_panel: UiRect) -> Option<UiRect> {
    let available_width = command_panel
        .width
        .saturating_sub(UI_PADDING.saturating_mul(2));
    if available_width <= 0 {
        return None;
    }

    Some(UiRect {
        x: command_panel.x.saturating_add(UI_PADDING),
        y: command_panel.y.saturating_add(UI_PADDING),
        width: available_width,
        height: COMMAND_CATEGORY_SELECTOR_HEIGHT,
    })
}

fn place_tabs(tab_bar: UiRect, tabs: &[TerminalTabView]) -> (Vec<TabPlacement>, Option<UiRect>) {
    if tab_bar.width <= UI_PADDING.saturating_mul(2) || tab_bar.height <= 0 {
        return (Vec::new(), None);
    }

    let available_width = tab_bar.width.saturating_sub(UI_PADDING.saturating_mul(2));
    let reserve_new_tab = NEW_TAB_BUTTON_WIDTH
        .saturating_add(TAB_BUTTON_GAP)
        .min(available_width);
    let available_for_tabs = available_width.saturating_sub(reserve_new_tab);
    let y = tab_bar.y.saturating_add(3);
    let height = tab_bar.height.saturating_sub(5).max(1);
    let mut placements = Vec::with_capacity(tabs.len());

    if !tabs.is_empty() && available_for_tabs > 0 {
        let gap_count = tabs.len().saturating_sub(1);
        let gap_width = i32::try_from(gap_count)
            .unwrap_or(i32::MAX)
            .saturating_mul(TAB_BUTTON_GAP)
            .min(available_for_tabs);
        let tab_count = i32::try_from(tabs.len()).unwrap_or(i32::MAX).max(1);
        let tab_width = available_for_tabs
            .saturating_sub(gap_width)
            .checked_div(tab_count)
            .unwrap_or(1)
            .clamp(1, TAB_MAX_WIDTH);
        let mut x = tab_bar.x.saturating_add(UI_PADDING);
        let tab_limit = tab_bar
            .x
            .saturating_add(UI_PADDING)
            .saturating_add(available_for_tabs);

        for tab in tabs {
            if x.saturating_add(tab_width) > tab_limit {
                break;
            }

            let bounds = UiRect {
                x,
                y,
                width: tab_width,
                height,
            };
            placements.push(TabPlacement {
                id: tab.id,
                bounds,
                close_bounds: tab_close_bounds(bounds),
                active: tab.active,
            });
            x = x.saturating_add(tab_width).saturating_add(TAB_BUTTON_GAP);
        }
    }

    let used_width = placements
        .last()
        .map(|placement| {
            placement
                .bounds
                .x
                .saturating_add(placement.bounds.width)
                .saturating_sub(tab_bar.x.saturating_add(UI_PADDING))
        })
        .unwrap_or(0);
    let new_tab_x = tab_bar
        .x
        .saturating_add(UI_PADDING)
        .saturating_add(used_width)
        .saturating_add(if placements.is_empty() {
            0
        } else {
            TAB_BUTTON_GAP
        });
    let right_limit = tab_bar
        .x
        .saturating_add(tab_bar.width)
        .saturating_sub(UI_PADDING);
    let remaining_width = right_limit.saturating_sub(new_tab_x);
    let new_tab_button = if remaining_width >= 20 {
        Some(UiRect {
            x: new_tab_x,
            y,
            width: NEW_TAB_BUTTON_WIDTH.min(remaining_width),
            height,
        })
    } else {
        None
    };

    (placements, new_tab_button)
}

fn tab_close_bounds(tab_bounds: UiRect) -> Option<UiRect> {
    if tab_bounds.width < 56 || tab_bounds.height < TAB_CLOSE_BUTTON_SIZE {
        return None;
    }

    let size = TAB_CLOSE_BUTTON_SIZE.min(tab_bounds.height);
    Some(UiRect {
        x: tab_bounds
            .x
            .saturating_add(tab_bounds.width)
            .saturating_sub(size)
            .saturating_sub(4),
        y: tab_bounds
            .y
            .saturating_add((tab_bounds.height.saturating_sub(size)) / 2),
        width: size,
        height: size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::command::predefined_command_buttons;
    use crate::domain::terminal::{TerminalTabId, TerminalTabView};

    #[test]
    fn window_layout_splits_terminal_left_and_command_panel_right() {
        let layout = default_window_layout(900, 520, &single_tab_views());

        assert_eq!(
            layout.terminal,
            UiRect {
                x: 0,
                y: 28,
                width: 766,
                height: 492,
            }
        );
        assert_eq!(
            layout.splitter,
            UiRect {
                x: 766,
                y: 28,
                width: 6,
                height: 492,
            }
        );
        assert_eq!(
            layout.command_panel,
            UiRect {
                x: 772,
                y: 28,
                width: 128,
                height: 492,
            }
        );
        assert_eq!(
            layout.terminal_scrollbar,
            Some(UiRect {
                x: 749,
                y: 28,
                width: 17,
                height: 492,
            })
        );
        assert_eq!(
            layout.command_category_selector,
            Some(UiRect {
                x: 780,
                y: 36,
                width: 112,
                height: 40,
            })
        );
        assert_eq!(
            layout.tab_bar,
            UiRect {
                x: 0,
                y: 0,
                width: 900,
                height: 28,
            }
        );
        assert_eq!(layout.tabs.len(), 1);
        assert_eq!(layout.tabs[0].id, TerminalTabId::new(1));
        assert!(layout.new_tab_button.is_some());
        assert_eq!(layout.buttons.len(), predefined_command_buttons().len());
        assert_eq!(
            layout.buttons[0].bounds,
            UiRect {
                x: 784,
                y: 84,
                width: 104,
                height: 28,
            }
        );
        assert_eq!(layout.buttons[1].bounds.y, 120);
        assert_command_category_and_buttons_do_not_overlap(&layout);
    }

    #[test]
    fn terminal_content_area_insets_terminal_by_padding() {
        let content = terminal_content_area(UiRect {
            x: 0,
            y: 28,
            width: 766,
            height: 492,
        });

        assert_eq!(
            content,
            UiRect {
                x: 8,
                y: 36,
                width: 729,
                height: 476,
            }
        );
    }

    #[test]
    fn terminal_content_area_stays_inside_tiny_terminal_area() {
        let content = terminal_content_area(UiRect {
            x: 4,
            y: 8,
            width: 10,
            height: 6,
        });

        assert_eq!(
            content,
            UiRect {
                x: 9,
                y: 11,
                width: 0,
                height: 0,
            }
        );
    }

    #[test]
    fn window_layout_keeps_terminal_visible_on_narrow_clients() {
        let layout = default_window_layout(100, 100, &single_tab_views());

        assert_eq!(layout.terminal.width, 47);
        assert_eq!(layout.terminal.height, 72);
        assert_eq!(layout.splitter.width, 6);
        assert_eq!(layout.command_panel.width, 47);
        assert!(
            layout
                .buttons
                .iter()
                .all(|button| layout.command_panel.contains(UiPoint {
                    x: button.bounds.x,
                    y: button.bounds.y,
                }))
        );
    }

    #[test]
    fn window_layout_hides_buttons_that_do_not_fit_vertically() {
        let layout = default_window_layout(900, 80, &single_tab_views());

        assert!(layout.buttons.is_empty());
        assert!(layout.buttons.iter().all(|button| {
            button.bounds.y.saturating_add(button.bounds.height)
                <= layout
                    .command_panel
                    .y
                    .saturating_add(layout.command_panel.height)
                    .saturating_sub(UI_PADDING)
        }));
    }

    #[test]
    fn window_layout_scrolls_overflowing_command_buttons() -> crate::error::AppResult<()> {
        let buttons = test_buttons(20)?;
        let layout = WindowLayout::for_client_with_command_panel_width_and_button_scroll(
            900,
            200,
            COMMAND_PANEL_WIDTH,
            &buttons,
            &single_tab_views(),
            3,
        );

        assert_eq!(
            layout.buttons.first().map(|button| button.id),
            Some(buttons[3].id)
        );
        assert_eq!(
            layout.buttons.first().map(|button| button.label.as_str()),
            Some(buttons[3].label.as_str())
        );
        assert!(layout.buttons.iter().all(|button| {
            layout.command_button_viewport.contains(UiPoint {
                x: button.bounds.x,
                y: button.bounds.y,
            }) && layout.command_button_viewport.contains(UiPoint {
                x: button.bounds.x.saturating_add(button.bounds.width - 1),
                y: button.bounds.y.saturating_add(button.bounds.height - 1),
            })
        }));

        let scroll = layout
            .command_button_scroll
            .ok_or(crate::error::AppError::InvalidState(
                "overflowing button layout should include scrollbar",
            ))?;
        assert_eq!(scroll.position, 3);
        assert_eq!(scroll.total_len, 20);
        assert_eq!(scroll.page_len, layout.buttons.len());
        assert_eq!(layout.command_button_scroll_position(), 3);
        Ok(())
    }

    #[test]
    fn window_layout_clamps_command_button_scroll_position() -> crate::error::AppResult<()> {
        let buttons = test_buttons(20)?;
        let layout = WindowLayout::for_client_with_command_panel_width_and_button_scroll(
            900,
            200,
            COMMAND_PANEL_WIDTH,
            &buttons,
            &single_tab_views(),
            usize::MAX,
        );

        let scroll = layout
            .command_button_scroll
            .ok_or(crate::error::AppError::InvalidState(
                "overflowing button layout should include scrollbar",
            ))?;
        assert_eq!(scroll.position, scroll.max_position);
        assert_eq!(
            layout.buttons.first().map(|button| button.id),
            Some(buttons[scroll.max_position].id)
        );
        Ok(())
    }

    #[test]
    fn window_layout_places_multiple_tabs_and_hit_targets() {
        let tabs = vec![
            TerminalTabView::new(TerminalTabId::new(1), "Tab 1", true),
            TerminalTabView::new(TerminalTabId::new(2), "Tab 2", false),
        ];
        let layout = default_window_layout(900, 520, &tabs);

        assert_eq!(layout.tabs.len(), 2);
        assert_eq!(layout.tabs[0].id, TerminalTabId::new(1));
        assert_eq!(layout.tabs[1].id, TerminalTabId::new(2));
        assert!(layout.tabs[0].active);
        assert!(!layout.tabs[1].active);
        assert_eq!(
            layout.tab_at(UiPoint {
                x: layout.tabs[1].bounds.x,
                y: layout.tabs[1].bounds.y,
            }),
            Some(TerminalTabId::new(2))
        );
        assert!(layout.new_tab_at(UiPoint {
            x: layout.new_tab_button.map_or(0, |bounds| bounds.x),
            y: layout.new_tab_button.map_or(0, |bounds| bounds.y),
        }));
    }

    #[test]
    fn window_layout_keeps_command_buttons_below_category_selector() {
        let layout = default_window_layout(900, 520, &single_tab_views());

        assert_command_category_and_buttons_do_not_overlap(&layout);
    }

    #[test]
    fn window_layout_hit_tests_visible_command_buttons() {
        let layout = default_window_layout(900, 520, &single_tab_views());
        let first = &layout.buttons[0];

        assert_eq!(
            layout.command_button_at(UiPoint {
                x: first.bounds.x,
                y: first.bounds.y,
            }),
            Some(first.id)
        );
        assert_eq!(
            layout.command_button_at(UiPoint {
                x: first.bounds.x.saturating_sub(1),
                y: first.bounds.y,
            }),
            None
        );
    }

    #[test]
    fn window_layout_uses_requested_command_panel_width() {
        let buttons = predefined_command_buttons();
        let layout = WindowLayout::for_client_with_command_panel_width(
            900,
            520,
            220,
            &buttons,
            &single_tab_views(),
        );

        assert_eq!(layout.terminal.width, 674);
        assert_eq!(layout.splitter.x, 674);
        assert_eq!(layout.command_panel.x, 680);
        assert_eq!(layout.command_panel.width, 220);
        assert!(layout.splitter_at(UiPoint {
            x: layout.splitter.x,
            y: layout.splitter.y,
        }));
    }

    #[test]
    fn window_layout_resizes_command_panel_width_without_reallocating_button_labels() {
        let buttons = predefined_command_buttons();
        let mut layout = WindowLayout::for_client_with_command_panel_width(
            900,
            520,
            COMMAND_PANEL_WIDTH,
            &buttons,
            &single_tab_views(),
        );
        let label_ptrs = layout
            .buttons
            .iter()
            .map(|button| button.label.as_ptr())
            .collect::<Vec<_>>();

        assert!(layout.try_resize_command_panel_width(
            900,
            520,
            COMMAND_PANEL_WIDTH.saturating_add(48),
            &buttons,
            0,
        ));

        let expected = WindowLayout::for_client_with_command_panel_width(
            900,
            520,
            COMMAND_PANEL_WIDTH.saturating_add(48),
            &buttons,
            &single_tab_views(),
        );
        assert_eq!(layout, expected);
        assert_eq!(
            layout
                .buttons
                .iter()
                .map(|button| button.label.as_ptr())
                .collect::<Vec<_>>(),
            label_ptrs
        );
    }

    #[test]
    fn window_layout_clamps_requested_command_panel_width() {
        let buttons = predefined_command_buttons();
        let narrow = WindowLayout::for_client_with_command_panel_width(
            900,
            520,
            20,
            &buttons,
            &single_tab_views(),
        );
        let wide = WindowLayout::for_client_with_command_panel_width(
            900,
            520,
            900,
            &buttons,
            &single_tab_views(),
        );

        assert_eq!(narrow.command_panel.width, COMMAND_PANEL_MIN_WIDTH);
        assert_eq!(wide.terminal.width, TERMINAL_MIN_WIDTH);
        assert_eq!(
            wide.command_panel.width,
            900 - SPLITTER_WIDTH - TERMINAL_MIN_WIDTH
        );
    }

    #[test]
    fn window_layout_maps_splitter_x_to_command_panel_width() {
        let width = WindowLayout::command_panel_width_from_splitter_x(900, 600);
        let buttons = predefined_command_buttons();
        let layout = WindowLayout::for_client_with_command_panel_width(
            900,
            520,
            width,
            &buttons,
            &single_tab_views(),
        );

        assert_eq!(width, 294);
        assert_eq!(layout.terminal.width, 600);
        assert_eq!(layout.splitter.x, 600);
    }

    fn single_tab_views() -> Vec<TerminalTabView> {
        vec![TerminalTabView::new(TerminalTabId::new(1), "Tab 1", true)]
    }

    fn test_buttons(count: usize) -> crate::error::AppResult<Vec<CommandButton>> {
        let mut buttons = Vec::with_capacity(count);
        for index in 0..count {
            let id = u32::try_from(index)
                .map_err(|_| crate::error::AppError::InvalidInput("too many test buttons"))?
                .saturating_add(100);
            buttons.push(CommandButton::new(
                CommandButtonId::new(id),
                format!("button {index}"),
                "echo",
                crate::domain::CommandArguments::empty(),
            )?);
        }

        Ok(buttons)
    }

    fn default_window_layout(
        client_width: i32,
        client_height: i32,
        tabs: &[TerminalTabView],
    ) -> WindowLayout {
        let buttons = predefined_command_buttons();
        WindowLayout::for_client_with_command_panel_width(
            client_width,
            client_height,
            COMMAND_PANEL_WIDTH,
            &buttons,
            tabs,
        )
    }

    fn assert_command_category_and_buttons_do_not_overlap(layout: &WindowLayout) {
        let Some(selector) = layout.command_category_selector else {
            assert!(layout.buttons.is_empty());
            return;
        };
        let selector_bottom = selector.y.saturating_add(selector.height);
        for button in &layout.buttons {
            assert!(
                button.bounds.y >= selector_bottom.saturating_add(COMMAND_PANEL_HEADER_GAP),
                "button {:?} starts before category selector spacing ends",
                button.id
            );
        }
    }
}
