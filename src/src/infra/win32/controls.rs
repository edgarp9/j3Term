use std::{cell::RefCell, mem, ptr};

use windows_sys::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows_sys::Win32::UI::Controls::SetScrollInfo;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BN_CLICKED, BS_PUSHBUTTON, CB_ADDSTRING, CB_ERR, CB_ERRSPACE, CB_GETCURSEL, CB_RESETCONTENT,
    CB_SETCURSEL, CBN_SELCHANGE, CBS_DROPDOWNLIST, CBS_HASSTRINGS, CreateWindowExW, DestroyWindow,
    GetDlgCtrlID, GetParent, GetScrollInfo, HMENU, MoveWindow, SB_BOTTOM, SB_CTL, SB_ENDSCROLL,
    SB_LINEDOWN, SB_LINEUP, SB_PAGEDOWN, SB_PAGEUP, SB_THUMBPOSITION, SB_THUMBTRACK, SB_TOP,
    SBS_VERT, SCROLLINFO, SIF_PAGE, SIF_POS, SIF_RANGE, SIF_TRACKPOS, SW_HIDE, SW_SHOW,
    SendMessageW, SetWindowTextW, ShowWindow, WS_CHILD, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
};

use crate::domain::{
    CommandButtonId, CommandPanel, TerminalScrollState, WindowLayout,
    layout::{COMMAND_CATEGORY_SELECTOR_DROPDOWN_HEIGHT, CommandButtonPlacement},
};
use crate::error::{AppError, AppResult};

use super::windowing::{current_instance, wide_null};

const CATEGORY_COMBO_CONTROL_ID: u16 = 1100;
const COMMAND_BUTTON_SCROLLBAR_CONTROL_ID: u16 = 1101;
const TERMINAL_SCROLLBAR_CONTROL_ID: u16 = 1102;
const COMMAND_BUTTON_CONTROL_ID_BASE: u16 = 1200;
const COMMAND_BUTTON_CONTROL_ID_LIMIT: u16 = 60_000;

#[derive(Default)]
pub(super) struct CommandPanelControls {
    category_combo: Option<HWND>,
    category_names: Vec<String>,
    selected_category_index: Option<usize>,
    button_scrollbar: Option<HWND>,
    button_parent: Option<HWND>,
    buttons: RefCell<Vec<ButtonControl>>,
    retired_buttons: RefCell<Vec<ButtonControl>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CommandButtonScrollRequest {
    LineUp,
    LineDown,
    PageUp,
    PageDown,
    Absolute(usize),
    Top,
    Bottom,
}

#[derive(Default)]
pub(super) struct TerminalScrollBarControl {
    scrollbar: Option<HWND>,
    scroll_state: RefCell<Option<TerminalScrollState>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TerminalScrollBarRequest {
    LineUp,
    LineDown,
    PageUp,
    PageDown,
    Absolute(usize),
    Top,
    Bottom,
}

struct ButtonControl {
    id: CommandButtonId,
    label: String,
    control_id: u16,
    hwnd: HWND,
}

impl TerminalScrollBarControl {
    pub(super) fn create(&mut self, parent: HWND) -> AppResult<()> {
        self.ensure_scrollbar(parent)
    }

    pub(super) fn layout(
        &self,
        layout: &WindowLayout,
        scroll: TerminalScrollState,
    ) -> AppResult<()> {
        let Some(scrollbar) = self.scrollbar else {
            return Ok(());
        };

        let Some(bounds) = layout.terminal_scrollbar else {
            // SAFETY: scrollbar hwnd is valid.
            unsafe {
                ShowWindow(scrollbar, SW_HIDE);
            }
            return Ok(());
        };

        // SAFETY: scrollbar hwnd belongs to this process and bounds are client coords.
        let moved = unsafe {
            MoveWindow(
                scrollbar,
                bounds.x,
                bounds.y,
                bounds.width,
                bounds.height,
                1,
            )
        };
        if moved == 0 {
            return Err(AppError::win32("MoveWindow terminal scrollbar"));
        }

        self.set_scroll_info(scroll)?;
        // SAFETY: scrollbar hwnd is valid.
        unsafe {
            ShowWindow(scrollbar, SW_SHOW);
        }

        Ok(())
    }

    pub(super) fn sync_scroll_state(&self, scroll: TerminalScrollState) -> AppResult<()> {
        if self.has_scroll_state(&scroll) {
            return Ok(());
        }

        self.set_scroll_info(scroll)
    }

    fn ensure_scrollbar(&mut self, parent: HWND) -> AppResult<()> {
        if self.scrollbar.is_some() {
            return Ok(());
        }

        let class_name = wide_null("SCROLLBAR");
        let instance = current_instance()?;

        // SAFETY: parent is a valid top-level window; class name lives for this call.
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                ptr::null(),
                WS_CHILD | WS_VISIBLE | (SBS_VERT as u32),
                0,
                0,
                0,
                0,
                parent,
                control_menu(TERMINAL_SCROLLBAR_CONTROL_ID),
                instance,
                ptr::null(),
            )
        };

        if hwnd.is_null() {
            return Err(AppError::win32("CreateWindowExW terminal scrollbar"));
        }

        self.scrollbar = Some(hwnd);
        Ok(())
    }

    fn set_scroll_info(&self, scroll: TerminalScrollState) -> AppResult<()> {
        let Some(scrollbar) = self.scrollbar else {
            return Ok(());
        };

        let info = SCROLLINFO {
            cbSize: mem::size_of::<SCROLLINFO>() as u32,
            fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
            nMin: 0,
            nMax: scroll_info_i32(scroll.total_len.saturating_sub(1))?,
            nPage: scroll_info_u32(scroll.page_len.max(1))?,
            nPos: scroll_info_i32(scroll.position)?,
            nTrackPos: 0,
        };

        // SAFETY: scrollbar is a live scroll bar control; info points to initialized data.
        unsafe {
            SetScrollInfo(scrollbar, SB_CTL, &info, 1);
        }

        self.remember_scroll_state(scroll);
        Ok(())
    }

    fn has_scroll_state(&self, scroll: &TerminalScrollState) -> bool {
        let current = self.scroll_state.borrow();
        match current.as_ref() {
            Some(current) => terminal_scroll_states_equal(current, scroll),
            None => false,
        }
    }

    fn remember_scroll_state(&self, scroll: TerminalScrollState) {
        *self.scroll_state.borrow_mut() = Some(scroll);
    }

    #[cfg(test)]
    fn remember_scroll_state_for_test(&self, scroll: TerminalScrollState) {
        self.remember_scroll_state(scroll);
    }
}

impl CommandPanelControls {
    pub(super) fn create(&mut self, parent: HWND, panel: &CommandPanel) -> AppResult<()> {
        self.ensure_category_combo(parent)?;
        self.ensure_button_scrollbar(parent)?;
        self.sync(parent, panel)
    }

    pub(super) fn sync(&mut self, parent: HWND, panel: &CommandPanel) -> AppResult<()> {
        self.ensure_category_combo(parent)?;
        self.ensure_button_scrollbar(parent)?;
        self.button_parent = Some(parent);
        self.destroy_retired_button_controls()?;
        self.sync_category_combo(panel)
    }

    pub(super) fn layout(&self, layout: &WindowLayout) -> AppResult<()> {
        if let Some(combo) = self.category_combo {
            match layout.command_category_selector {
                Some(bounds) => {
                    // SAFETY: combo hwnd belongs to this process and bounds are client coords.
                    let moved = unsafe {
                        MoveWindow(
                            combo,
                            bounds.x,
                            bounds.y,
                            bounds.width,
                            COMMAND_CATEGORY_SELECTOR_DROPDOWN_HEIGHT,
                            1,
                        )
                    };
                    if moved == 0 {
                        return Err(AppError::win32("MoveWindow category combo"));
                    }
                    // SAFETY: combo hwnd is valid.
                    unsafe {
                        ShowWindow(combo, SW_SHOW);
                    }
                }
                None => {
                    // SAFETY: combo hwnd is valid.
                    unsafe {
                        ShowWindow(combo, SW_HIDE);
                    }
                }
            }
        }

        self.layout_button_scrollbar(layout)?;
        self.sync_button_controls(layout)?;

        let buttons = self.buttons.borrow();
        for (control, placement) in buttons.iter().zip(&layout.buttons) {
            // SAFETY: control hwnd belongs to this process and placement contains coords.
            let moved = unsafe {
                MoveWindow(
                    control.hwnd,
                    placement.bounds.x,
                    placement.bounds.y,
                    placement.bounds.width,
                    placement.bounds.height,
                    1,
                )
            };
            if moved == 0 {
                return Err(AppError::win32("MoveWindow command button"));
            }
            // SAFETY: control hwnd is valid.
            unsafe {
                ShowWindow(control.hwnd, SW_SHOW);
            }
        }

        Ok(())
    }

    pub(super) fn selected_category_index(&self) -> Option<usize> {
        let combo = self.category_combo?;
        // SAFETY: combo is a live combobox child window.
        let index = unsafe { SendMessageW(combo, CB_GETCURSEL, 0, 0) };
        if index == combo_error() {
            None
        } else {
            usize::try_from(index).ok()
        }
    }

    pub(super) fn command_button_id_from_wparam(&self, wparam: WPARAM) -> Option<CommandButtonId> {
        let notification_code = hiword(wparam);
        if notification_code != BN_CLICKED as u16 {
            return None;
        }

        let control_id = loword(wparam);
        self.command_button_id_from_control_id(control_id)
    }

    pub(super) fn command_button_id_from_hwnd(&self, hwnd: HWND) -> Option<CommandButtonId> {
        control_id_from_hwnd(hwnd)
            .and_then(|control_id| self.command_button_id_from_control_id(control_id))
    }

    fn layout_button_scrollbar(&self, layout: &WindowLayout) -> AppResult<()> {
        let Some(scrollbar) = self.button_scrollbar else {
            return Ok(());
        };

        let Some(scroll) = layout.command_button_scroll else {
            // SAFETY: scrollbar hwnd is valid.
            unsafe {
                ShowWindow(scrollbar, SW_HIDE);
            }
            return Ok(());
        };

        // SAFETY: scrollbar hwnd belongs to this process and bounds are client coords.
        let moved = unsafe {
            MoveWindow(
                scrollbar,
                scroll.bounds.x,
                scroll.bounds.y,
                scroll.bounds.width,
                scroll.bounds.height,
                1,
            )
        };
        if moved == 0 {
            return Err(AppError::win32("MoveWindow command button scrollbar"));
        }

        let info = SCROLLINFO {
            cbSize: mem::size_of::<SCROLLINFO>() as u32,
            fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
            nMin: 0,
            nMax: scroll_info_i32(scroll.total_len.saturating_sub(1))?,
            nPage: scroll_info_u32(scroll.page_len)?,
            nPos: scroll_info_i32(scroll.position)?,
            nTrackPos: 0,
        };

        // SAFETY: scrollbar is a live scroll bar control; info points to initialized data.
        unsafe {
            SetScrollInfo(scrollbar, SB_CTL, &info, 1);
            ShowWindow(scrollbar, SW_SHOW);
        }

        Ok(())
    }

    fn ensure_category_combo(&mut self, parent: HWND) -> AppResult<()> {
        if self.category_combo.is_some() {
            return Ok(());
        }

        let class_name = wide_null("COMBOBOX");
        let instance = current_instance()?;

        // SAFETY: parent is a valid top-level window; class name lives for this call.
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                ptr::null(),
                WS_CHILD
                    | WS_VISIBLE
                    | WS_TABSTOP
                    | WS_VSCROLL
                    | (CBS_DROPDOWNLIST as u32)
                    | (CBS_HASSTRINGS as u32),
                0,
                0,
                0,
                0,
                parent,
                control_menu(CATEGORY_COMBO_CONTROL_ID),
                instance,
                ptr::null(),
            )
        };

        if hwnd.is_null() {
            return Err(AppError::win32("CreateWindowExW category combo"));
        }

        self.category_combo = Some(hwnd);
        Ok(())
    }

    fn ensure_button_scrollbar(&mut self, parent: HWND) -> AppResult<()> {
        if self.button_scrollbar.is_some() {
            return Ok(());
        }

        let class_name = wide_null("SCROLLBAR");
        let instance = current_instance()?;

        // SAFETY: parent is a valid top-level window; class name lives for this call.
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                ptr::null(),
                WS_CHILD | WS_VISIBLE | (SBS_VERT as u32),
                0,
                0,
                0,
                0,
                parent,
                control_menu(COMMAND_BUTTON_SCROLLBAR_CONTROL_ID),
                instance,
                ptr::null(),
            )
        };

        if hwnd.is_null() {
            return Err(AppError::win32("CreateWindowExW command button scrollbar"));
        }

        self.button_scrollbar = Some(hwnd);
        Ok(())
    }

    fn sync_category_combo(&mut self, panel: &CommandPanel) -> AppResult<()> {
        let combo = self
            .category_combo
            .ok_or(AppError::InvalidState("category combo is not created"))?;

        if !self.category_names_match(panel) {
            self.rebuild_category_combo(combo, panel)?;
        }

        self.sync_category_combo_selection(combo, panel)
    }

    fn category_names_match(&self, panel: &CommandPanel) -> bool {
        self.category_names.len() == panel.categories().len()
            && self
                .category_names
                .iter()
                .zip(panel.categories())
                .all(|(name, category)| name == &category.name)
    }

    fn rebuild_category_combo(&mut self, combo: HWND, panel: &CommandPanel) -> AppResult<()> {
        // SAFETY: combo is a live combobox child window.
        unsafe {
            SendMessageW(combo, CB_RESETCONTENT, 0, 0);
        }
        self.category_names.clear();
        self.selected_category_index = None;

        for category in panel.categories() {
            let name = wide_null(&category.name);
            // SAFETY: name points to a null-terminated UTF-16 string for the duration of the call.
            let added = unsafe { SendMessageW(combo, CB_ADDSTRING, 0, name.as_ptr() as LPARAM) };
            if is_combo_add_string_failure(added) {
                return Err(AppError::win32("CB_ADDSTRING category combo"));
            }
            self.category_names.push(category.name.clone());
        }

        Ok(())
    }

    fn sync_category_combo_selection(
        &mut self,
        combo: HWND,
        panel: &CommandPanel,
    ) -> AppResult<()> {
        let selected = if panel.categories().is_empty() {
            None
        } else {
            Some(panel.selected_category_index().unwrap_or(0))
        };
        if self.selected_category_index == selected {
            return Ok(());
        }

        let selected = selected.unwrap_or(0);
        // SAFETY: combo is live; out-of-range selection is rejected by the control.
        let selected = unsafe { SendMessageW(combo, CB_SETCURSEL, selected, 0) };
        if selected == combo_error() && !panel.categories().is_empty() {
            self.selected_category_index = None;
            return Err(AppError::win32("CB_SETCURSEL category combo"));
        }

        self.selected_category_index = if panel.categories().is_empty() {
            None
        } else {
            usize::try_from(selected).ok()
        };
        Ok(())
    }

    fn sync_button_controls(&self, layout: &WindowLayout) -> AppResult<()> {
        self.destroy_retired_button_controls()?;

        let mut controls = self.buttons.borrow_mut();
        let desired_buttons = layout.buttons.as_slice();
        let reused_len = controls.len().min(desired_buttons.len());
        for (index, placement) in desired_buttons.iter().take(reused_len).enumerate() {
            Self::update_button_control(&mut controls[index], index, placement)?;
        }

        if desired_buttons.len() > controls.len() {
            let parent = self.button_parent.ok_or(AppError::InvalidState(
                "command button parent is not created",
            ))?;
            let start_index = controls.len();
            Self::append_button_controls(
                &self.retired_buttons,
                parent,
                &mut controls,
                &desired_buttons[start_index..],
            )?;
        } else if desired_buttons.len() < controls.len() {
            Self::destroy_surplus_button_controls(
                &self.retired_buttons,
                &mut controls,
                desired_buttons.len(),
            )?;
        }

        Ok(())
    }

    fn update_button_control(
        control: &mut ButtonControl,
        index: usize,
        placement: &CommandButtonPlacement,
    ) -> AppResult<()> {
        if control.label != placement.label {
            let label = wide_null(&placement.label);
            // SAFETY: control hwnd belongs to this process and label lives for this call.
            let updated = unsafe { SetWindowTextW(control.hwnd, label.as_ptr()) };
            if updated == 0 {
                return Err(AppError::win32("SetWindowTextW command button"));
            }
            control.label.clone_from(&placement.label);
        }

        Self::set_button_control_state(control, index, placement)
    }

    fn set_button_control_state(
        control: &mut ButtonControl,
        index: usize,
        placement: &CommandButtonPlacement,
    ) -> AppResult<()> {
        control.id = placement.id;
        control.control_id = button_control_id_for_index(index)?;
        Ok(())
    }

    fn append_button_controls(
        retired_buttons: &RefCell<Vec<ButtonControl>>,
        parent: HWND,
        controls: &mut Vec<ButtonControl>,
        buttons: &[CommandButtonPlacement],
    ) -> AppResult<()> {
        let start_index = controls.len();
        validate_button_control_id_range(start_index, buttons.len())?;

        let mut next_buttons = Vec::new();
        next_buttons
            .try_reserve(buttons.len())
            .map_err(|_| AppError::InvalidInput("too many command buttons"))?;

        let class_name = wide_null("BUTTON");
        let instance = current_instance()?;

        for (offset, button) in buttons.iter().enumerate() {
            let label = wide_null(&button.label);
            let index = start_index + offset;
            let control_id = button_control_id_for_index(index)?;

            // SAFETY: parent is valid; class name and label live for this call.
            let hwnd = unsafe {
                CreateWindowExW(
                    0,
                    class_name.as_ptr(),
                    label.as_ptr(),
                    WS_CHILD | WS_TABSTOP | (BS_PUSHBUTTON as u32),
                    0,
                    0,
                    0,
                    0,
                    parent,
                    control_menu(control_id),
                    instance,
                    ptr::null(),
                )
            };

            if hwnd.is_null() {
                let error = AppError::win32("CreateWindowExW command button");
                if let Err(cleanup_error) = Self::destroy_button_controls(&mut next_buttons) {
                    retired_buttons.borrow_mut().append(&mut next_buttons);
                    return Err(AppError::ui_message(
                        "cleanup partial command button controls",
                        format!(
                            "{error}; additionally failed to cleanup partial command button controls: {cleanup_error}"
                        ),
                    ));
                }
                return Err(error);
            }

            next_buttons.push(ButtonControl {
                id: button.id,
                label: button.label.clone(),
                control_id,
                hwnd,
            });
        }

        controls.append(&mut next_buttons);
        Ok(())
    }

    fn destroy_surplus_button_controls(
        retired_buttons: &RefCell<Vec<ButtonControl>>,
        controls: &mut Vec<ButtonControl>,
        desired_len: usize,
    ) -> AppResult<()> {
        let mut surplus_buttons = controls.split_off(desired_len);
        Self::hide_button_controls(&surplus_buttons);
        if let Err(error) = Self::destroy_button_controls(&mut surplus_buttons) {
            retired_buttons.borrow_mut().append(&mut surplus_buttons);
            return Err(error);
        }

        Ok(())
    }

    fn destroy_retired_button_controls(&self) -> AppResult<()> {
        let mut retired_buttons = self.retired_buttons.borrow_mut();
        Self::destroy_button_controls(&mut retired_buttons)
    }

    fn hide_button_controls(buttons: &[ButtonControl]) {
        for control in buttons {
            if control.hwnd.is_null() {
                continue;
            }

            // SAFETY: control hwnd is a child control created by this adapter.
            unsafe {
                ShowWindow(control.hwnd, SW_HIDE);
            }
        }
    }

    fn destroy_button_controls(buttons: &mut Vec<ButtonControl>) -> AppResult<()> {
        Self::destroy_button_controls_with(buttons, |hwnd| {
            // SAFETY: the HWND is a child control created by this adapter.
            let destroyed = unsafe { DestroyWindow(hwnd) };
            if destroyed == 0 {
                return Err(AppError::win32("DestroyWindow command button"));
            }
            Ok(())
        })
    }

    fn destroy_button_controls_with(
        buttons: &mut Vec<ButtonControl>,
        mut destroy: impl FnMut(HWND) -> AppResult<()>,
    ) -> AppResult<()> {
        while let Some(control) = buttons.pop() {
            let hwnd = control.hwnd;
            if hwnd.is_null() {
                continue;
            }

            if let Err(error) = destroy(hwnd) {
                buttons.push(control);
                return Err(error);
            }
        }

        Ok(())
    }

    #[cfg(test)]
    pub(super) fn sync_buttons_for_test(&mut self, panel: &CommandPanel) -> AppResult<()> {
        let mut placements = Vec::new();
        placements
            .try_reserve(panel.selected_buttons().len())
            .map_err(|_| AppError::InvalidInput("too many command buttons"))?;
        for (index, button) in panel.selected_buttons().iter().enumerate() {
            let y = i32::try_from(index)
                .map_err(|_| AppError::InvalidInput("too many command buttons"))?;
            placements.push(CommandButtonPlacement {
                id: button.id,
                label: button.label.clone(),
                bounds: crate::domain::layout::UiRect {
                    x: 0,
                    y,
                    width: 1,
                    height: 1,
                },
            });
        }

        self.sync_button_placements_for_test(&placements)
    }

    #[cfg(test)]
    pub(super) fn sync_button_placements_for_test(
        &mut self,
        placements: &[CommandButtonPlacement],
    ) -> AppResult<()> {
        self.retired_buttons.borrow_mut().clear();
        let mut buttons = self.buttons.borrow_mut();
        let reused_len = buttons.len().min(placements.len());
        for (index, placement) in placements.iter().take(reused_len).enumerate() {
            let control = &mut buttons[index];
            control.label.clone_from(&placement.label);
            Self::set_button_control_state(control, index, placement)?;
        }

        if placements.len() < buttons.len() {
            buttons.truncate(placements.len());
            return Ok(());
        }

        let start_index = buttons.len();
        buttons
            .try_reserve(placements.len().saturating_sub(start_index))
            .map_err(|_| AppError::InvalidInput("too many command buttons"))?;
        for (offset, placement) in placements[start_index..].iter().enumerate() {
            let index = start_index + offset;
            buttons.push(ButtonControl {
                id: placement.id,
                label: placement.label.clone(),
                control_id: button_control_id_for_index(index)?,
                hwnd: ptr::null_mut(),
            });
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn button_ids_for_test(&self) -> Vec<CommandButtonId> {
        self.buttons
            .borrow()
            .iter()
            .map(|button| button.id)
            .collect()
    }

    #[cfg(test)]
    fn button_labels_for_test(&self) -> Vec<String> {
        self.buttons
            .borrow()
            .iter()
            .map(|button| button.label.clone())
            .collect()
    }

    #[cfg(test)]
    fn button_control_ids_for_test(&self) -> Vec<u16> {
        self.buttons
            .borrow()
            .iter()
            .map(|button| button.control_id)
            .collect()
    }

    #[cfg(test)]
    fn button_hwnds_for_test(&self) -> Vec<HWND> {
        self.buttons
            .borrow()
            .iter()
            .map(|button| button.hwnd)
            .collect()
    }

    #[cfg(test)]
    fn set_button_hwnds_for_test(&mut self, hwnds: &[HWND]) {
        for (control, hwnd) in self.buttons.borrow_mut().iter_mut().zip(hwnds) {
            control.hwnd = *hwnd;
        }
    }

    fn command_button_id_from_control_id(&self, control_id: u16) -> Option<CommandButtonId> {
        self.buttons
            .borrow()
            .iter()
            .find(|button| button.control_id == control_id)
            .map(|button| button.id)
    }
}

pub(super) fn category_combo_selection_changed(wparam: WPARAM, hwnd: HWND) -> bool {
    if hwnd.is_null() || hiword(wparam) != CBN_SELCHANGE as u16 {
        return false;
    }

    loword(wparam) == CATEGORY_COMBO_CONTROL_ID
}

pub(super) fn is_category_combo_child(parent: HWND, hwnd: HWND) -> bool {
    child_control_id(parent, hwnd) == Some(CATEGORY_COMBO_CONTROL_ID)
}

pub(super) fn is_command_button_child(parent: HWND, hwnd: HWND) -> bool {
    child_control_id(parent, hwnd).is_some_and(is_command_button_control_id)
}

pub(super) fn command_button_scroll_request(
    parent: HWND,
    source: HWND,
    wparam: WPARAM,
) -> Option<CommandButtonScrollRequest> {
    if child_control_id(parent, source) != Some(COMMAND_BUTTON_SCROLLBAR_CONTROL_ID) {
        return None;
    }

    let request = match u32::from(loword(wparam)) {
        code if code == SB_LINEUP as u32 => CommandButtonScrollRequest::LineUp,
        code if code == SB_LINEDOWN as u32 => CommandButtonScrollRequest::LineDown,
        code if code == SB_PAGEUP as u32 => CommandButtonScrollRequest::PageUp,
        code if code == SB_PAGEDOWN as u32 => CommandButtonScrollRequest::PageDown,
        code if code == SB_THUMBPOSITION as u32 || code == SB_THUMBTRACK as u32 => {
            CommandButtonScrollRequest::Absolute(usize::from(hiword(wparam)))
        }
        code if code == SB_TOP as u32 => CommandButtonScrollRequest::Top,
        code if code == SB_BOTTOM as u32 => CommandButtonScrollRequest::Bottom,
        code if code == SB_ENDSCROLL as u32 => return None,
        _ => return None,
    };

    Some(request)
}

pub(super) fn terminal_scrollbar_request(
    parent: HWND,
    source: HWND,
    wparam: WPARAM,
) -> Option<TerminalScrollBarRequest> {
    if child_control_id(parent, source) != Some(TERMINAL_SCROLLBAR_CONTROL_ID) {
        return None;
    }

    let request = match u32::from(loword(wparam)) {
        code if code == SB_LINEUP as u32 => TerminalScrollBarRequest::LineUp,
        code if code == SB_LINEDOWN as u32 => TerminalScrollBarRequest::LineDown,
        code if code == SB_PAGEUP as u32 => TerminalScrollBarRequest::PageUp,
        code if code == SB_PAGEDOWN as u32 => TerminalScrollBarRequest::PageDown,
        code if code == SB_THUMBPOSITION as u32 || code == SB_THUMBTRACK as u32 => {
            TerminalScrollBarRequest::Absolute(terminal_scrollbar_track_position(source)?)
        }
        code if code == SB_TOP as u32 => TerminalScrollBarRequest::Top,
        code if code == SB_BOTTOM as u32 => TerminalScrollBarRequest::Bottom,
        code if code == SB_ENDSCROLL as u32 => return None,
        _ => return None,
    };

    Some(request)
}

fn terminal_scrollbar_track_position(scrollbar: HWND) -> Option<usize> {
    let mut info = SCROLLINFO {
        cbSize: mem::size_of::<SCROLLINFO>() as u32,
        fMask: SIF_TRACKPOS,
        nMin: 0,
        nMax: 0,
        nPage: 0,
        nPos: 0,
        nTrackPos: 0,
    };

    // SAFETY: scrollbar is the source HWND from a scrollbar notification; info is initialized.
    let ok = unsafe { GetScrollInfo(scrollbar, SB_CTL, &mut info) };
    if ok == 0 {
        return None;
    }

    usize::try_from(info.nTrackPos).ok()
}

fn child_control_id(parent: HWND, hwnd: HWND) -> Option<u16> {
    if hwnd.is_null() {
        return None;
    }

    // SAFETY: hwnd is from the current thread's message and can be inspected.
    let actual_parent = unsafe { GetParent(hwnd) };
    if actual_parent != parent {
        return None;
    }

    control_id_from_hwnd(hwnd)
}

fn control_id_from_hwnd(hwnd: HWND) -> Option<u16> {
    if hwnd.is_null() {
        return None;
    }

    // SAFETY: hwnd is a child window; GetDlgCtrlID returns its control id.
    let control_id = unsafe { GetDlgCtrlID(hwnd) };
    u16::try_from(control_id).ok()
}

fn button_control_id_for_index(index: usize) -> AppResult<u16> {
    let index =
        u16::try_from(index).map_err(|_| AppError::InvalidInput("too many command buttons"))?;
    let control_id = COMMAND_BUTTON_CONTROL_ID_BASE
        .checked_add(index)
        .ok_or(AppError::InvalidInput("too many command buttons"))?;
    if control_id >= COMMAND_BUTTON_CONTROL_ID_LIMIT {
        return Err(AppError::InvalidInput("too many command buttons"));
    }

    Ok(control_id)
}

fn validate_button_control_id_range(start_index: usize, len: usize) -> AppResult<()> {
    if len == 0 {
        return Ok(());
    }

    let last_index = start_index
        .checked_add(len - 1)
        .ok_or(AppError::InvalidInput("too many command buttons"))?;
    button_control_id_for_index(start_index)?;
    button_control_id_for_index(last_index)?;
    Ok(())
}

fn is_command_button_control_id(control_id: u16) -> bool {
    (COMMAND_BUTTON_CONTROL_ID_BASE..COMMAND_BUTTON_CONTROL_ID_LIMIT).contains(&control_id)
}

fn loword(value: WPARAM) -> u16 {
    (value & 0xffff) as u16
}

fn hiword(value: WPARAM) -> u16 {
    ((value >> 16) & 0xffff) as u16
}

fn control_menu(control_id: u16) -> HMENU {
    usize::from(control_id) as HMENU
}

fn combo_error() -> isize {
    CB_ERR as isize
}

fn combo_error_space() -> isize {
    CB_ERRSPACE as isize
}

fn is_combo_add_string_failure(result: isize) -> bool {
    result == combo_error() || result == combo_error_space()
}

fn scroll_info_i32(value: usize) -> AppResult<i32> {
    i32::try_from(value).map_err(|_| AppError::InvalidInput("scroll range is too large"))
}

fn scroll_info_u32(value: usize) -> AppResult<u32> {
    u32::try_from(value).map_err(|_| AppError::InvalidInput("scroll range is too large"))
}

fn terminal_scroll_states_equal(left: &TerminalScrollState, right: &TerminalScrollState) -> bool {
    left.total_len == right.total_len
        && left.page_len == right.page_len
        && left.max_position == right.max_position
        && left.position == right.position
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        CommandArguments, CommandButtonDefinition, CommandCategoryDefinition, UiRect,
        layout::COMMAND_PANEL_WIDTH,
    };

    #[test]
    fn dynamic_button_control_ids_use_reserved_range() -> AppResult<()> {
        assert_eq!(
            button_control_id_for_index(0)?,
            COMMAND_BUTTON_CONTROL_ID_BASE
        );
        assert!(is_command_button_control_id(COMMAND_BUTTON_CONTROL_ID_BASE));
        assert!(!is_command_button_control_id(CATEGORY_COMBO_CONTROL_ID));
        assert!(!is_command_button_control_id(
            COMMAND_BUTTON_SCROLLBAR_CONTROL_ID
        ));
        assert!(!is_command_button_control_id(TERMINAL_SCROLLBAR_CONTROL_ID));
        Ok(())
    }

    #[test]
    fn too_many_button_controls_are_rejected() {
        let index = usize::from(COMMAND_BUTTON_CONTROL_ID_LIMIT - COMMAND_BUTTON_CONTROL_ID_BASE);

        assert!(matches!(
            button_control_id_for_index(index),
            Err(AppError::InvalidInput("too many command buttons"))
        ));
    }

    #[test]
    fn combo_add_string_failure_includes_space_errors() {
        assert!(is_combo_add_string_failure(combo_error()));
        assert!(is_combo_add_string_failure(combo_error_space()));
        assert!(!is_combo_add_string_failure(0));
        assert!(!is_combo_add_string_failure(42));
    }

    #[test]
    fn category_combo_selection_change_requires_combo_id_and_notification() {
        let combo = CATEGORY_COMBO_CONTROL_ID as WPARAM;
        let selection_change = combo | ((CBN_SELCHANGE as WPARAM) << 16);
        let wrong_notification = combo | ((BN_CLICKED as WPARAM) << 16);
        let wrong_control =
            (CATEGORY_COMBO_CONTROL_ID + 1) as WPARAM | ((CBN_SELCHANGE as WPARAM) << 16);
        let hwnd = 1usize as HWND;

        assert!(category_combo_selection_changed(selection_change, hwnd));
        assert!(!category_combo_selection_changed(
            selection_change,
            ptr::null_mut()
        ));
        assert!(!category_combo_selection_changed(wrong_notification, hwnd));
        assert!(!category_combo_selection_changed(wrong_control, hwnd));
    }

    #[test]
    fn command_button_wparam_maps_only_clicked_button_controls() -> AppResult<()> {
        let placements = command_button_placements_for_test(&["one", "two"])?;
        let mut controls = CommandPanelControls::default();
        controls.sync_button_placements_for_test(&placements)?;

        let first_clicked =
            COMMAND_BUTTON_CONTROL_ID_BASE as WPARAM | ((BN_CLICKED as WPARAM) << 16);
        let first_non_click =
            COMMAND_BUTTON_CONTROL_ID_BASE as WPARAM | ((CBN_SELCHANGE as WPARAM) << 16);
        let unknown_clicked =
            (COMMAND_BUTTON_CONTROL_ID_BASE + 42) as WPARAM | ((BN_CLICKED as WPARAM) << 16);

        assert_eq!(
            controls.command_button_id_from_wparam(first_clicked),
            Some(placements[0].id)
        );
        assert_eq!(
            controls.command_button_id_from_wparam(first_non_click),
            None
        );
        assert_eq!(
            controls.command_button_id_from_wparam(unknown_clicked),
            None
        );
        Ok(())
    }

    #[test]
    fn terminal_scrollbar_cache_detects_unchanged_scroll_state() {
        let controls = TerminalScrollBarControl::default();
        let scroll = terminal_scroll_state_for_test(120, 30, 90, 4);

        assert!(!controls.has_scroll_state(&scroll));

        controls.remember_scroll_state_for_test(scroll);

        assert!(controls.has_scroll_state(&terminal_scroll_state_for_test(120, 30, 90, 4)));
        assert!(!controls.has_scroll_state(&terminal_scroll_state_for_test(121, 30, 90, 4)));
        assert!(!controls.has_scroll_state(&terminal_scroll_state_for_test(120, 31, 90, 4)));
        assert!(!controls.has_scroll_state(&terminal_scroll_state_for_test(120, 30, 91, 4)));
        assert!(!controls.has_scroll_state(&terminal_scroll_state_for_test(120, 30, 90, 5)));
    }

    #[test]
    fn button_append_rejects_overflowing_control_ids_before_creating_controls() -> AppResult<()> {
        let existing_len =
            usize::from(COMMAND_BUTTON_CONTROL_ID_LIMIT - COMMAND_BUTTON_CONTROL_ID_BASE) - 1;
        let id = command_button_id_for_test()?;
        let controls = CommandPanelControls::default();
        {
            let mut buttons = controls.buttons.borrow_mut();
            buttons
                .try_reserve(existing_len)
                .map_err(|_| AppError::InvalidInput("too many command buttons"))?;
            for index in 0..existing_len {
                buttons.push(ButtonControl {
                    id,
                    label: String::from("existing"),
                    control_id: button_control_id_for_index(index)?,
                    hwnd: (index + 1) as HWND,
                });
            }
        }

        let placements = command_button_placements_for_test(&["next", "overflow"])?;
        let result = CommandPanelControls::append_button_controls(
            &controls.retired_buttons,
            ptr::null_mut(),
            &mut controls.buttons.borrow_mut(),
            &placements,
        );

        assert!(matches!(
            result,
            Err(AppError::InvalidInput("too many command buttons"))
        ));
        assert_eq!(controls.buttons.borrow().len(), existing_len);
        assert!(controls.retired_buttons.borrow().is_empty());
        Ok(())
    }

    #[test]
    fn failed_button_destroy_keeps_hwnd_tracked() -> AppResult<()> {
        let id = command_button_id_for_test()?;
        let hwnd = 1usize as HWND;
        let mut buttons = vec![ButtonControl {
            id,
            label: String::from("run"),
            control_id: COMMAND_BUTTON_CONTROL_ID_BASE,
            hwnd,
        }];

        let result = CommandPanelControls::destroy_button_controls_with(&mut buttons, |_| {
            Err(AppError::InvalidState("forced destroy failure"))
        });

        assert!(result.is_err());
        assert_eq!(buttons.len(), 1);
        assert_eq!(buttons[0].id, id);
        assert_eq!(buttons[0].control_id, COMMAND_BUTTON_CONTROL_ID_BASE);
        assert_eq!(buttons[0].hwnd, hwnd);
        Ok(())
    }

    #[test]
    fn test_button_sync_reuses_existing_control_slots() -> AppResult<()> {
        let initial_placements = command_button_placements_for_test(&["one", "two"])?;
        let mut controls = CommandPanelControls::default();
        controls.sync_button_placements_for_test(&initial_placements)?;
        let existing_hwnds = vec![11usize as HWND, 22usize as HWND];
        controls.set_button_hwnds_for_test(&existing_hwnds);

        let next_placements = command_button_placements_for_test(&["renamed", "two", "three"])?;
        controls.sync_button_placements_for_test(&next_placements)?;

        assert_eq!(
            &controls.button_hwnds_for_test()[..existing_hwnds.len()],
            existing_hwnds.as_slice()
        );
        assert_eq!(
            controls.button_labels_for_test(),
            vec![
                String::from("renamed"),
                String::from("two"),
                String::from("three")
            ]
        );
        assert_eq!(
            controls.button_control_ids_for_test(),
            vec![
                COMMAND_BUTTON_CONTROL_ID_BASE,
                COMMAND_BUTTON_CONTROL_ID_BASE + 1,
                COMMAND_BUTTON_CONTROL_ID_BASE + 2
            ]
        );
        Ok(())
    }

    #[test]
    fn test_button_sync_removes_only_surplus_control_slots() -> AppResult<()> {
        let initial_placements = command_button_placements_for_test(&["one", "two", "three"])?;
        let mut controls = CommandPanelControls::default();
        controls.sync_button_placements_for_test(&initial_placements)?;
        controls.set_button_hwnds_for_test(&[11usize as HWND, 22usize as HWND, 33usize as HWND]);

        let next_placements = command_button_placements_for_test(&["one"])?;
        controls.sync_button_placements_for_test(&next_placements)?;

        assert_eq!(controls.button_hwnds_for_test(), vec![11usize as HWND]);
        assert_eq!(
            controls.button_control_ids_for_test(),
            vec![COMMAND_BUTTON_CONTROL_ID_BASE]
        );
        Ok(())
    }

    #[test]
    fn test_button_sync_limits_controls_to_layout_placements() -> AppResult<()> {
        let panel = command_panel_for_test(&[
            "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
            "eleven",
        ])?;
        let layout = WindowLayout::for_client_with_command_panel_width_and_button_scroll(
            900,
            200,
            COMMAND_PANEL_WIDTH,
            panel.selected_buttons(),
            &[],
            4,
        );
        let mut controls = CommandPanelControls::default();
        controls.sync_button_placements_for_test(&layout.buttons)?;

        let layout_ids: Vec<_> = layout.buttons.iter().map(|button| button.id).collect();
        let layout_labels: Vec<_> = layout
            .buttons
            .iter()
            .map(|button| button.label.clone())
            .collect();

        assert_eq!(controls.button_ids_for_test(), layout_ids);
        assert_eq!(controls.button_labels_for_test(), layout_labels);
        assert_eq!(controls.button_ids_for_test().len(), layout.buttons.len());
        assert!(controls.button_ids_for_test().len() < panel.selected_buttons().len());
        Ok(())
    }

    fn command_button_id_for_test() -> AppResult<CommandButtonId> {
        let panel = command_panel_for_test(&["run"])?;
        panel
            .selected_buttons()
            .first()
            .map(|button| button.id)
            .ok_or(AppError::InvalidState("missing command button"))
    }

    fn command_panel_for_test(labels: &[&str]) -> AppResult<CommandPanel> {
        let mut buttons = Vec::new();
        buttons
            .try_reserve(labels.len())
            .map_err(|_| AppError::InvalidInput("too many command buttons"))?;
        for label in labels {
            buttons.push(CommandButtonDefinition::new(
                *label,
                "echo",
                CommandArguments::new(*label)?,
            )?);
        }

        CommandPanel::from_definitions(vec![CommandCategoryDefinition::new("Default", buttons)?], 0)
    }

    fn command_button_placements_for_test(
        labels: &[&str],
    ) -> AppResult<Vec<CommandButtonPlacement>> {
        let panel = command_panel_for_test(labels)?;
        let mut placements = Vec::new();
        placements
            .try_reserve(panel.selected_buttons().len())
            .map_err(|_| AppError::InvalidInput("too many command buttons"))?;
        for (index, button) in panel.selected_buttons().iter().enumerate() {
            let y = i32::try_from(index)
                .map_err(|_| AppError::InvalidInput("too many command buttons"))?;
            placements.push(CommandButtonPlacement {
                id: button.id,
                label: button.label.clone(),
                bounds: UiRect {
                    x: 0,
                    y,
                    width: 1,
                    height: 1,
                },
            });
        }

        Ok(placements)
    }

    fn terminal_scroll_state_for_test(
        total_len: usize,
        page_len: usize,
        max_position: usize,
        position: usize,
    ) -> TerminalScrollState {
        TerminalScrollState {
            total_len,
            page_len,
            max_position,
            position,
        }
    }
}
