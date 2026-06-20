use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::Rc;

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

use crate::error::{AppError, AppResult};

const DEFAULT_CATEGORY_ID: u32 = 1;
const FIRST_DYNAMIC_CATEGORY_ID: u32 = 2;
const FIRST_DYNAMIC_BUTTON_ID: u32 = 5;

const TOKEN_PATH: &str = "{path}";
const TOKEN_NAME: &str = "{name}";
const TOKEN_SELECT_FILE: &str = "{selectfile}";
const TOKEN_SELECT_DIR: &str = "{selectdir}";
const TOKEN_INPUT_TEXT: &str = "{inputtext}";
const CMD_LITERAL_UNSUPPORTED_MESSAGE: &str = "command prompt literal values must not contain quote, percent, exclamation, or caret characters";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CommandCategoryId(u32);

impl CommandCategoryId {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CommandButtonId(u32);

impl CommandButtonId {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandText {
    value: Vec<u8>,
}

impl CommandText {
    #[cfg(test)]
    pub fn from_static(value: &'static str) -> Self {
        Self {
            value: value.as_bytes().to_vec(),
        }
    }

    fn from_bytes(value: Vec<u8>) -> AppResult<Self> {
        validate_command_bytes(&value)?;
        Ok(Self { value })
    }

    pub fn to_pty_bytes(&self) -> Vec<u8> {
        let mut bytes = self.value.clone();
        bytes.push(b'\r');
        bytes
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellCommandDialect {
    CommandPrompt,
    PowerShell,
    #[cfg(any(not(target_os = "windows"), test))]
    Posix,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandArguments {
    value: String,
}

impl CommandArguments {
    pub fn new(value: impl Into<String>) -> AppResult<Self> {
        let value = value.into();
        validate_command_arguments(&value)?;
        Ok(Self { value })
    }

    #[cfg(test)]
    pub fn empty() -> Self {
        Self {
            value: String::new(),
        }
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn required_inputs(&self) -> ButtonArgumentInputs {
        CommandArgumentTemplate::new(self.value()).required_inputs()
    }

    fn contains_current_name(&self) -> bool {
        CommandArgumentTemplate::new(self.value()).contains_token(CommandArgumentToken::CurrentName)
    }
}

#[derive(Debug, Clone, Copy)]
struct CommandArgumentTemplate<'a> {
    value: &'a str,
}

impl<'a> CommandArgumentTemplate<'a> {
    fn new(value: &'a str) -> Self {
        Self { value }
    }

    fn parts(self) -> CommandArgumentParts<'a> {
        CommandArgumentParts {
            remaining: self.value,
        }
    }

    fn required_inputs(self) -> ButtonArgumentInputs {
        let mut inputs = ButtonArgumentInputs::default();
        for part in self.parts() {
            if let CommandArgumentPart::Token(token) = part {
                inputs.include_token(token);
            }
        }
        inputs
    }

    fn contains_token(self, expected: CommandArgumentToken) -> bool {
        self.parts()
            .any(|part| matches!(part, CommandArgumentPart::Token(token) if token == expected))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandArgumentToken {
    CurrentPath,
    CurrentName,
    SelectedFile,
    SelectedDir,
    InputText,
}

impl CommandArgumentToken {
    const ALL: [Self; 5] = [
        Self::CurrentPath,
        Self::CurrentName,
        Self::SelectedFile,
        Self::SelectedDir,
        Self::InputText,
    ];

    fn at_start(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|token| value.starts_with(token.source()))
    }

    fn source(self) -> &'static str {
        match self {
            Self::CurrentPath => TOKEN_PATH,
            Self::CurrentName => TOKEN_NAME,
            Self::SelectedFile => TOKEN_SELECT_FILE,
            Self::SelectedDir => TOKEN_SELECT_DIR,
            Self::InputText => TOKEN_INPUT_TEXT,
        }
    }

    fn render_into(
        self,
        rendered: &mut Vec<u8>,
        values: &CommandArgumentFragments,
        dialect: ShellCommandDialect,
    ) {
        match self {
            Self::CurrentPath => rendered.extend(shell_current_path_fragment(dialect).as_bytes()),
            Self::CurrentName => rendered.extend(shell_current_name_fragment(dialect).as_bytes()),
            Self::SelectedFile => {
                rendered.extend(
                    values
                        .selected_file
                        .as_deref()
                        .unwrap_or_else(|| self.source().as_bytes()),
                );
            }
            Self::SelectedDir => {
                rendered.extend(
                    values
                        .selected_dir
                        .as_deref()
                        .unwrap_or_else(|| self.source().as_bytes()),
                );
            }
            Self::InputText => {
                rendered.extend(
                    values
                        .input_text
                        .as_deref()
                        .unwrap_or_else(|| self.source().as_bytes()),
                );
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum CommandArgumentPart<'a> {
    Literal(&'a str),
    Token(CommandArgumentToken),
}

#[derive(Debug, Clone, Copy)]
struct CommandArgumentParts<'a> {
    remaining: &'a str,
}

impl<'a> Iterator for CommandArgumentParts<'a> {
    type Item = CommandArgumentPart<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }

        if let Some(token) = CommandArgumentToken::at_start(self.remaining) {
            self.remaining = &self.remaining[token.source().len()..];
            return Some(CommandArgumentPart::Token(token));
        }

        let mut characters = self.remaining.char_indices();
        characters.next();
        let literal_end = characters
            .find_map(|(index, _)| {
                CommandArgumentToken::at_start(&self.remaining[index..]).map(|_| index)
            })
            .unwrap_or(self.remaining.len());
        let literal = &self.remaining[..literal_end];
        self.remaining = &self.remaining[literal_end..];
        Some(CommandArgumentPart::Literal(literal))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandButtonDefinition {
    pub label: String,
    pub executable_path: String,
    pub arguments: CommandArguments,
}

impl CommandButtonDefinition {
    pub fn new(
        label: impl Into<String>,
        executable_path: impl Into<String>,
        arguments: CommandArguments,
    ) -> AppResult<Self> {
        let label = label.into();
        let executable_path = executable_path.into();
        validate_label(&label, "button label must not be empty")?;
        validate_executable_path(&executable_path)?;
        Ok(Self {
            label,
            executable_path,
            arguments,
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ButtonArgumentInputs {
    pub select_file: bool,
    pub select_dir: bool,
    pub input_text: bool,
}

impl ButtonArgumentInputs {
    pub fn any(self) -> bool {
        self.select_file || self.select_dir || self.input_text
    }

    fn include_token(&mut self, token: CommandArgumentToken) {
        match token {
            CommandArgumentToken::SelectedFile => self.select_file = true,
            CommandArgumentToken::SelectedDir => self.select_dir = true,
            CommandArgumentToken::InputText => self.input_text = true,
            CommandArgumentToken::CurrentPath | CommandArgumentToken::CurrentName => {}
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ButtonArgumentValues {
    pub selected_file: Option<String>,
    pub selected_dir: Option<String>,
    pub input_text: Option<String>,
    #[cfg(unix)]
    selected_file_posix_bytes: Option<Vec<u8>>,
    #[cfg(unix)]
    selected_dir_posix_bytes: Option<Vec<u8>>,
}

impl ButtonArgumentValues {
    #[cfg(unix)]
    pub fn set_selected_file_path(&mut self, path: PathBuf) {
        self.selected_file_posix_bytes = Some(path.as_os_str().as_bytes().to_vec());
        self.selected_file = Some(path.to_string_lossy().into_owned());
    }

    #[cfg(unix)]
    pub fn set_selected_dir_path(&mut self, path: PathBuf) {
        self.selected_dir_posix_bytes = Some(path.as_os_str().as_bytes().to_vec());
        self.selected_dir = Some(path.to_string_lossy().into_owned());
    }

    pub fn validate_for(&self, inputs: ButtonArgumentInputs) -> AppResult<()> {
        if inputs.select_file && self.selected_file.is_none() {
            return Err(AppError::InvalidInput("file selection is required"));
        }

        if inputs.select_dir && self.selected_dir.is_none() {
            return Err(AppError::InvalidInput("folder selection is required"));
        }

        if inputs.input_text && self.input_text.is_none() {
            return Err(AppError::InvalidInput("text input is required"));
        }

        for value in [
            self.selected_file.as_deref(),
            self.selected_dir.as_deref(),
            self.input_text.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            validate_runtime_argument_value(value)?;
        }

        #[cfg(unix)]
        {
            for value in [
                self.selected_file_posix_bytes.as_deref(),
                self.selected_dir_posix_bytes.as_deref(),
            ]
            .into_iter()
            .flatten()
            {
                validate_runtime_argument_bytes(value)?;
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandButton {
    pub id: CommandButtonId,
    pub label: String,
    pub executable_path: String,
    pub arguments: CommandArguments,
}

impl CommandButton {
    pub fn new(
        id: CommandButtonId,
        label: impl Into<String>,
        executable_path: impl Into<String>,
        arguments: CommandArguments,
    ) -> AppResult<Self> {
        let definition = CommandButtonDefinition::new(label, executable_path, arguments)?;
        Ok(Self {
            id,
            label: definition.label,
            executable_path: definition.executable_path,
            arguments: definition.arguments,
        })
    }

    pub fn definition(&self) -> CommandButtonDefinition {
        CommandButtonDefinition {
            label: self.label.clone(),
            executable_path: self.executable_path.clone(),
            arguments: self.arguments.clone(),
        }
    }

    pub fn required_argument_inputs(&self) -> ButtonArgumentInputs {
        self.arguments.required_inputs()
    }

    pub fn to_command_text(
        &self,
        values: &ButtonArgumentValues,
        dialect: ShellCommandDialect,
    ) -> AppResult<CommandText> {
        values.validate_for(self.required_argument_inputs())?;
        let executable = shell_executable_fragment(&self.executable_path, dialect)?;
        let arguments = render_command_arguments(&self.arguments, values, dialect)?;
        let command_line = if self.arguments.contains_current_name()
            && dialect == ShellCommandDialect::CommandPrompt
        {
            let mut command_line = b"for %J in (\"%CD%\") do ".to_vec();
            command_line.extend(join_command_parts(&executable, &arguments));
            command_line
        } else {
            join_command_parts(&executable, &arguments)
        };

        CommandText::from_bytes(command_line)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandCategory {
    pub id: CommandCategoryId,
    pub name: String,
    pub buttons: Rc<Vec<CommandButton>>,
}

impl CommandCategory {
    pub fn new(
        id: CommandCategoryId,
        name: impl Into<String>,
        buttons: Vec<CommandButton>,
    ) -> AppResult<Self> {
        let name = name.into();
        validate_category_name(&name)?;
        Ok(Self {
            id,
            name,
            buttons: Rc::new(buttons),
        })
    }

    pub fn rename(&mut self, name: impl Into<String>) -> AppResult<()> {
        let name = name.into();
        validate_category_name(&name)?;
        self.name = name;
        Ok(())
    }

    fn buttons_mut(&mut self) -> &mut Vec<CommandButton> {
        Rc::make_mut(&mut self.buttons)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandCategoryDefinition {
    pub name: String,
    pub buttons: Vec<CommandButtonDefinition>,
}

impl CommandCategoryDefinition {
    pub fn new(name: impl Into<String>, buttons: Vec<CommandButtonDefinition>) -> AppResult<Self> {
        let name = name.into();
        validate_category_name(&name)?;
        Ok(Self { name, buttons })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandPanel {
    categories: Rc<Vec<CommandCategory>>,
    selected_category_id: CommandCategoryId,
    next_category_id: u32,
    next_button_id: u32,
}

impl CommandPanel {
    pub fn default_commands() -> Self {
        default_command_panel_from_buttons(vec![
            predefined_button(1, "cd", "cd", ""),
            predefined_button(2, "dir", "dir", ""),
            predefined_button(3, "echo hello", "echo", "hello"),
            predefined_button(4, "cls", "cls", ""),
        ])
    }

    #[cfg(any(target_os = "linux", test))]
    pub fn posix_default_commands() -> Self {
        default_command_panel_from_buttons(vec![
            predefined_button(1, "cd", "pwd", ""),
            predefined_button(2, "dir", "ls", ""),
            predefined_button(3, "echo hello", "echo", "hello"),
            predefined_button(4, "cls", "clear", ""),
        ])
    }
}

fn default_command_panel_from_buttons(buttons: Vec<CommandButton>) -> CommandPanel {
    let default_category_id = CommandCategoryId::new(DEFAULT_CATEGORY_ID);
    CommandPanel {
        categories: Rc::new(vec![CommandCategory {
            id: default_category_id,
            name: "Default".to_owned(),
            buttons: Rc::new(buttons),
        }]),
        selected_category_id: default_category_id,
        next_category_id: FIRST_DYNAMIC_CATEGORY_ID,
        next_button_id: FIRST_DYNAMIC_BUTTON_ID,
    }
}

impl CommandPanel {
    pub fn from_definitions(
        categories: Vec<CommandCategoryDefinition>,
        selected_category_index: usize,
    ) -> AppResult<Self> {
        if categories.is_empty() {
            return Err(AppError::InvalidInput(
                "at least one command category is required",
            ));
        }

        if selected_category_index >= categories.len() {
            return Err(AppError::InvalidInput(
                "selected command category index is out of range",
            ));
        }

        let mut next_button_id = 1_u32;
        let mut panel_categories = Vec::with_capacity(categories.len());
        for (index, definition) in categories.into_iter().enumerate() {
            let category_id = CommandCategoryId::new(command_category_id_value(index)?);
            let mut buttons = Vec::with_capacity(definition.buttons.len());
            for button_definition in definition.buttons {
                let button_id = CommandButtonId::new(next_button_id);
                next_button_id = next_button_id.checked_add(1).ok_or(AppError::InvalidState(
                    "command button id space is exhausted",
                ))?;
                buttons.push(CommandButton::new(
                    button_id,
                    button_definition.label,
                    button_definition.executable_path,
                    button_definition.arguments,
                )?);
            }
            panel_categories.push(CommandCategory::new(category_id, definition.name, buttons)?);
        }

        let selected_category_id = panel_categories[selected_category_index].id;
        let next_category_id = command_category_id_value(panel_categories.len())?;

        Ok(Self {
            categories: Rc::new(panel_categories),
            selected_category_id,
            next_category_id,
            next_button_id,
        })
    }

    pub fn categories(&self) -> &[CommandCategory] {
        self.categories.as_slice()
    }

    pub fn selected_category(&self) -> Option<&CommandCategory> {
        self.categories
            .iter()
            .find(|category| category.id == self.selected_category_id)
    }

    pub fn selected_buttons(&self) -> &[CommandButton] {
        self.selected_category()
            .map(|category| category.buttons.as_slice())
            .unwrap_or(&[])
    }

    pub fn selected_category_index(&self) -> Option<usize> {
        self.category_index(self.selected_category_id)
    }

    pub fn category_id_at_index(&self, index: usize) -> Option<CommandCategoryId> {
        self.categories.get(index).map(|category| category.id)
    }

    pub fn select_category_by_index(&mut self, index: usize) -> AppResult<()> {
        let id = self
            .category_id_at_index(index)
            .ok_or(AppError::InvalidInput("unknown command category"))?;
        self.selected_category_id = id;
        Ok(())
    }

    pub fn button_by_id(&self, id: CommandButtonId) -> Option<&CommandButton> {
        self.categories
            .iter()
            .flat_map(|category| category.buttons.iter())
            .find(|button| button.id == id)
    }

    pub fn suggested_new_category_name(&self) -> String {
        self.unique_category_name("New Category")
    }

    #[cfg(test)]
    pub fn add_category(&mut self) -> AppResult<CommandCategoryId> {
        let name = self.suggested_new_category_name();
        self.add_category_named(name)
    }

    pub fn add_category_named(&mut self, name: impl Into<String>) -> AppResult<CommandCategoryId> {
        let name = name.into();
        validate_category_name(&name)?;
        let id = self.allocate_category_id()?;
        Rc::make_mut(&mut self.categories).push(CommandCategory::new(id, name, Vec::new())?);
        self.selected_category_id = id;
        Ok(id)
    }

    pub fn rename_selected_category(&mut self, name: impl Into<String>) -> AppResult<()> {
        self.rename_category(self.selected_category_id, name)
    }

    pub fn rename_category(
        &mut self,
        id: CommandCategoryId,
        name: impl Into<String>,
    ) -> AppResult<()> {
        let name = name.into();
        validate_category_name(&name)?;
        let index = self
            .category_index(id)
            .ok_or(AppError::InvalidInput("unknown command category"))?;
        Rc::make_mut(&mut self.categories)[index].rename(name)
    }

    pub fn delete_selected_category(&mut self) -> AppResult<()> {
        self.delete_category(self.selected_category_id)
    }

    pub fn delete_category(&mut self, id: CommandCategoryId) -> AppResult<()> {
        if self.categories.len() <= 1 {
            return Err(AppError::InvalidState(
                "at least one command category must stay available",
            ));
        }

        let index = self
            .category_index(id)
            .ok_or(AppError::InvalidInput("unknown command category"))?;
        let categories = Rc::make_mut(&mut self.categories);
        categories.remove(index);

        if self.selected_category_id == id {
            let new_index = index
                .saturating_sub(1)
                .min(categories.len().saturating_sub(1));
            if let Some(category) = categories.get(new_index) {
                self.selected_category_id = category.id;
            }
        }

        Ok(())
    }

    pub fn move_selected_category_up(&mut self) -> AppResult<()> {
        self.move_category(self.selected_category_id, MoveDirection::Up)
    }

    pub fn move_selected_category_down(&mut self) -> AppResult<()> {
        self.move_category(self.selected_category_id, MoveDirection::Down)
    }

    pub fn can_move_selected_category_up(&self) -> bool {
        self.selected_category_index()
            .is_some_and(|index| index > 0)
    }

    pub fn can_move_selected_category_down(&self) -> bool {
        self.selected_category_index()
            .is_some_and(|index| index + 1 < self.categories.len())
    }

    pub fn add_button_to_selected_category(
        &mut self,
        definition: CommandButtonDefinition,
    ) -> AppResult<CommandButtonId> {
        let category_index = self
            .selected_category_index()
            .ok_or(AppError::InvalidState(
                "selected command category is missing",
            ))?;
        let id = self.allocate_button_id()?;
        let button = CommandButton::new(
            id,
            definition.label,
            definition.executable_path,
            definition.arguments,
        )?;
        Rc::make_mut(&mut self.categories)[category_index]
            .buttons_mut()
            .push(button);
        Ok(id)
    }

    pub fn update_button(
        &mut self,
        id: CommandButtonId,
        definition: CommandButtonDefinition,
    ) -> AppResult<()> {
        let (category_index, button_index) = self
            .button_position(id)
            .ok_or(AppError::InvalidInput("unknown command button"))?;
        Rc::make_mut(&mut self.categories)[category_index].buttons_mut()[button_index] =
            CommandButton::new(
                id,
                definition.label,
                definition.executable_path,
                definition.arguments,
            )?;
        Ok(())
    }

    pub fn delete_button(&mut self, id: CommandButtonId) -> AppResult<()> {
        let (category_index, button_index) = self
            .button_position(id)
            .ok_or(AppError::InvalidInput("unknown command button"))?;
        Rc::make_mut(&mut self.categories)[category_index]
            .buttons_mut()
            .remove(button_index);
        Ok(())
    }

    pub fn move_button_up(&mut self, id: CommandButtonId) -> AppResult<()> {
        self.move_button(id, MoveDirection::Up)
    }

    pub fn move_button_down(&mut self, id: CommandButtonId) -> AppResult<()> {
        self.move_button(id, MoveDirection::Down)
    }

    pub fn can_move_button_up(&self, id: CommandButtonId) -> bool {
        self.button_position(id)
            .is_some_and(|(_, button_index)| button_index > 0)
    }

    pub fn can_move_button_down(&self, id: CommandButtonId) -> bool {
        self.button_position(id)
            .is_some_and(|(category_index, button_index)| {
                button_index + 1 < self.categories[category_index].buttons.len()
            })
    }

    fn move_category(&mut self, id: CommandCategoryId, direction: MoveDirection) -> AppResult<()> {
        let index = self
            .category_index(id)
            .ok_or(AppError::InvalidInput("unknown command category"))?;
        let Some(target_index) = moved_index(index, self.categories.len(), direction) else {
            return Ok(());
        };
        Rc::make_mut(&mut self.categories).swap(index, target_index);
        Ok(())
    }

    fn move_button(&mut self, id: CommandButtonId, direction: MoveDirection) -> AppResult<()> {
        let (category_index, button_index) = self
            .button_position(id)
            .ok_or(AppError::InvalidInput("unknown command button"))?;
        let button_count = self.categories[category_index].buttons.len();
        let Some(target_index) = moved_index(button_index, button_count, direction) else {
            return Ok(());
        };
        Rc::make_mut(&mut self.categories)[category_index]
            .buttons_mut()
            .swap(button_index, target_index);
        Ok(())
    }

    fn category_index(&self, id: CommandCategoryId) -> Option<usize> {
        self.categories
            .iter()
            .position(|category| category.id == id)
    }

    fn button_position(&self, id: CommandButtonId) -> Option<(usize, usize)> {
        self.categories
            .iter()
            .enumerate()
            .find_map(|(category_index, category)| {
                category
                    .buttons
                    .iter()
                    .position(|button| button.id == id)
                    .map(|button_index| (category_index, button_index))
            })
    }

    fn allocate_category_id(&mut self) -> AppResult<CommandCategoryId> {
        let id = self.next_category_id;
        self.next_category_id =
            self.next_category_id
                .checked_add(1)
                .ok_or(AppError::InvalidState(
                    "command category id space is exhausted",
                ))?;
        Ok(CommandCategoryId::new(id))
    }

    fn allocate_button_id(&mut self) -> AppResult<CommandButtonId> {
        let id = self.next_button_id;
        self.next_button_id = self
            .next_button_id
            .checked_add(1)
            .ok_or(AppError::InvalidState(
                "command button id space is exhausted",
            ))?;
        Ok(CommandButtonId::new(id))
    }

    fn unique_category_name(&self, base: &str) -> String {
        unique_name(base, |candidate| {
            self.categories
                .iter()
                .any(|category| category.name == candidate)
        })
    }
}

#[derive(Debug, Clone, Copy)]
enum MoveDirection {
    Up,
    Down,
}

#[cfg(test)]
pub fn default_command_panel() -> CommandPanel {
    CommandPanel::default_commands()
}

pub fn default_platform_command_panel() -> CommandPanel {
    #[cfg(target_os = "linux")]
    {
        CommandPanel::posix_default_commands()
    }
    #[cfg(not(target_os = "linux"))]
    {
        CommandPanel::default_commands()
    }
}

#[cfg(test)]
pub fn predefined_command_buttons() -> Vec<CommandButton> {
    default_command_panel().selected_buttons().to_vec()
}

fn predefined_button(
    id: u32,
    label: &'static str,
    executable_path: &'static str,
    arguments: &'static str,
) -> CommandButton {
    CommandButton {
        id: CommandButtonId::new(id),
        label: label.to_owned(),
        executable_path: executable_path.to_owned(),
        arguments: CommandArguments {
            value: arguments.to_owned(),
        },
    }
}

fn moved_index(index: usize, len: usize, direction: MoveDirection) -> Option<usize> {
    match direction {
        MoveDirection::Up if index > 0 => Some(index - 1),
        MoveDirection::Down if index + 1 < len => Some(index + 1),
        _ => None,
    }
}

fn command_category_id_value(index: usize) -> AppResult<u32> {
    let offset = u32::try_from(index)
        .map_err(|_| AppError::InvalidState("command category id space is exhausted"))?;
    offset.checked_add(1).ok_or(AppError::InvalidState(
        "command category id space is exhausted",
    ))
}

fn unique_name(base: &str, exists: impl Fn(&str) -> bool) -> String {
    if !exists(base) {
        return base.to_owned();
    }

    let mut suffix = 2_u32;
    loop {
        let candidate = format!("{base} {suffix}");
        if !exists(&candidate) {
            return candidate;
        }
        suffix = suffix.saturating_add(1);
    }
}

fn validate_category_name(name: &str) -> AppResult<()> {
    validate_label(name, "category name must not be empty")
}

fn validate_label(value: &str, empty_message: &'static str) -> AppResult<()> {
    if value.trim().is_empty() {
        return Err(AppError::InvalidInput(empty_message));
    }

    if value.chars().any(char::is_control) {
        return Err(AppError::InvalidInput(
            "labels must not contain control characters",
        ));
    }

    Ok(())
}

fn validate_command_bytes(value: &[u8]) -> AppResult<()> {
    if value.is_empty() || value.iter().all(u8::is_ascii_whitespace) {
        return Err(AppError::InvalidInput("command text must not be empty"));
    }

    if value.iter().any(|byte| matches!(byte, b'\r' | b'\n')) {
        return Err(AppError::InvalidInput(
            "command text must not contain line breaks",
        ));
    }

    Ok(())
}

fn validate_command_arguments(value: &str) -> AppResult<()> {
    if value
        .chars()
        .any(|character| matches!(character, '\r' | '\n'))
    {
        return Err(AppError::InvalidInput(
            "command arguments must not contain line breaks",
        ));
    }

    if value.chars().any(is_disallowed_control_character) {
        return Err(AppError::InvalidInput(
            "command arguments must not contain control characters",
        ));
    }

    Ok(())
}

fn validate_executable_path(value: &str) -> AppResult<()> {
    if value.trim().is_empty() {
        return Err(AppError::InvalidInput("executable path must not be empty"));
    }

    if value
        .chars()
        .any(|character| matches!(character, '\r' | '\n'))
    {
        return Err(AppError::InvalidInput(
            "executable path must not contain line breaks",
        ));
    }

    if value.chars().any(is_disallowed_control_character) {
        return Err(AppError::InvalidInput(
            "executable path must not contain control characters",
        ));
    }

    Ok(())
}

fn validate_runtime_argument_value(value: &str) -> AppResult<()> {
    if value
        .chars()
        .any(|character| matches!(character, '\r' | '\n'))
    {
        return Err(AppError::InvalidInput(
            "runtime argument values must not contain line breaks",
        ));
    }

    if value.chars().any(is_disallowed_control_character) {
        return Err(AppError::InvalidInput(
            "runtime argument values must not contain control characters",
        ));
    }
    Ok(())
}

fn validate_runtime_argument_bytes(value: &[u8]) -> AppResult<()> {
    if value.iter().any(|byte| matches!(byte, b'\r' | b'\n')) {
        return Err(AppError::InvalidInput(
            "runtime argument values must not contain line breaks",
        ));
    }

    if value
        .iter()
        .any(|byte| byte.is_ascii_control() && !matches!(byte, b'\t'))
    {
        return Err(AppError::InvalidInput(
            "runtime argument values must not contain control characters",
        ));
    }

    Ok(())
}

fn is_disallowed_control_character(character: char) -> bool {
    character.is_control() && !matches!(character, '\t')
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandArgumentFragments {
    selected_file: Option<Vec<u8>>,
    selected_dir: Option<Vec<u8>>,
    input_text: Option<Vec<u8>>,
}

impl CommandArgumentFragments {
    fn new(values: &ButtonArgumentValues, dialect: ShellCommandDialect) -> AppResult<Self> {
        Ok(Self {
            selected_file: values
                .selected_file
                .as_deref()
                .map(|value| {
                    shell_literal_argument_fragment(
                        value,
                        selected_file_posix_bytes(values),
                        dialect,
                    )
                })
                .transpose()?,
            selected_dir: values
                .selected_dir
                .as_deref()
                .map(|value| {
                    shell_literal_argument_fragment(
                        value,
                        selected_dir_posix_bytes(values),
                        dialect,
                    )
                })
                .transpose()?,
            input_text: values
                .input_text
                .as_deref()
                .map(|value| shell_literal_argument_fragment(value, None, dialect))
                .transpose()?,
        })
    }
}

#[cfg(unix)]
fn selected_file_posix_bytes(values: &ButtonArgumentValues) -> Option<&[u8]> {
    values.selected_file_posix_bytes.as_deref()
}

#[cfg(not(unix))]
fn selected_file_posix_bytes(_values: &ButtonArgumentValues) -> Option<&[u8]> {
    None
}

#[cfg(unix)]
fn selected_dir_posix_bytes(values: &ButtonArgumentValues) -> Option<&[u8]> {
    values.selected_dir_posix_bytes.as_deref()
}

#[cfg(not(unix))]
fn selected_dir_posix_bytes(_values: &ButtonArgumentValues) -> Option<&[u8]> {
    None
}

fn render_command_arguments(
    arguments: &CommandArguments,
    values: &ButtonArgumentValues,
    dialect: ShellCommandDialect,
) -> AppResult<Vec<u8>> {
    let fragments = CommandArgumentFragments::new(values, dialect)?;
    let mut rendered = Vec::with_capacity(arguments.value().len());
    for part in CommandArgumentTemplate::new(arguments.value()).parts() {
        match part {
            CommandArgumentPart::Literal(literal) => rendered.extend(literal.as_bytes()),
            CommandArgumentPart::Token(token) => {
                token.render_into(&mut rendered, &fragments, dialect);
            }
        }
    }

    Ok(rendered)
}

fn shell_executable_fragment(
    executable_path: &str,
    dialect: ShellCommandDialect,
) -> AppResult<Vec<u8>> {
    validate_executable_path(executable_path)?;
    match dialect {
        ShellCommandDialect::CommandPrompt => {
            quote_cmd_executable(executable_path).map(|fragment| fragment.into_bytes())
        }
        ShellCommandDialect::PowerShell => {
            Ok(format!("& {}", quote_powershell_literal(executable_path)).into_bytes())
        }
        #[cfg(any(not(target_os = "windows"), test))]
        ShellCommandDialect::Posix => Ok(quote_posix_executable(executable_path)),
    }
}

fn shell_current_path_fragment(dialect: ShellCommandDialect) -> &'static str {
    match dialect {
        ShellCommandDialect::CommandPrompt => "\"%CD%\"",
        ShellCommandDialect::PowerShell => "$PWD.Path",
        #[cfg(any(not(target_os = "windows"), test))]
        ShellCommandDialect::Posix => "\"$PWD\"",
    }
}

fn shell_current_name_fragment(dialect: ShellCommandDialect) -> &'static str {
    match dialect {
        ShellCommandDialect::CommandPrompt => "\"%~nxJ\"",
        ShellCommandDialect::PowerShell => "(Split-Path -Leaf $PWD.Path)",
        #[cfg(any(not(target_os = "windows"), test))]
        ShellCommandDialect::Posix => "\"${PWD##*/}\"",
    }
}

fn shell_literal_argument_fragment(
    value: &str,
    posix_bytes: Option<&[u8]>,
    dialect: ShellCommandDialect,
) -> AppResult<Vec<u8>> {
    validate_runtime_argument_value(value)?;
    if let Some(bytes) = posix_bytes {
        validate_runtime_argument_bytes(bytes)?;
    }

    match dialect {
        ShellCommandDialect::CommandPrompt => {
            quote_cmd_literal(value).map(|fragment| fragment.into_bytes())
        }
        ShellCommandDialect::PowerShell => Ok(quote_powershell_literal(value).into_bytes()),
        #[cfg(any(not(target_os = "windows"), test))]
        ShellCommandDialect::Posix => Ok(quote_posix_literal_bytes(
            posix_bytes.unwrap_or(value.as_bytes()),
        )),
    }
}

fn quote_cmd_executable(value: &str) -> AppResult<String> {
    validate_cmd_literal_value(value)?;
    if value.is_empty() || !argument_needs_quotes(value) {
        return Ok(value.to_owned());
    }

    quote_cmd_literal(value)
}

fn quote_cmd_literal(value: &str) -> AppResult<String> {
    validate_cmd_literal_value(value)?;
    let mut quoted = String::with_capacity(value.len().saturating_add(2));
    quoted.push('"');
    for character in value.chars() {
        quoted.push(character);
    }
    quoted.push('"');
    Ok(quoted)
}

fn quote_powershell_literal(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len().saturating_add(2));
    quoted.push('\'');
    for character in value.chars() {
        if character == '\'' {
            quoted.push('\'');
        }
        quoted.push(character);
    }
    quoted.push('\'');
    quoted
}

#[cfg(any(not(target_os = "windows"), test))]
fn quote_posix_executable(value: &str) -> Vec<u8> {
    let bytes = value.as_bytes();
    if !bytes.is_empty() && bytes.iter().copied().all(is_safe_posix_executable_byte) {
        bytes.to_vec()
    } else {
        quote_posix_literal_bytes(bytes)
    }
}

#[cfg(any(not(target_os = "windows"), test))]
fn is_safe_posix_executable_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-' | b'+')
}

#[cfg(any(not(target_os = "windows"), test))]
fn quote_posix_literal_bytes(value: &[u8]) -> Vec<u8> {
    let mut quoted = Vec::with_capacity(value.len().saturating_add(2));
    quoted.push(b'\'');
    for byte in value {
        if *byte == b'\'' {
            quoted.extend(b"'\\''");
        } else {
            quoted.push(*byte);
        }
    }
    quoted.push(b'\'');
    quoted
}

fn validate_cmd_literal_value(value: &str) -> AppResult<()> {
    if value.chars().any(is_unsupported_cmd_literal_character) {
        return Err(AppError::InvalidInput(CMD_LITERAL_UNSUPPORTED_MESSAGE));
    }

    Ok(())
}

fn is_unsupported_cmd_literal_character(character: char) -> bool {
    matches!(character, '"' | '%' | '!' | '^')
}

fn join_command_parts(executable: &[u8], arguments: &[u8]) -> Vec<u8> {
    if arguments.iter().all(u8::is_ascii_whitespace) {
        executable.to_vec()
    } else {
        let mut command = Vec::with_capacity(
            executable
                .len()
                .saturating_add(1)
                .saturating_add(arguments.len()),
        );
        command.extend(executable);
        command.push(b' ');
        command.extend(arguments);
        command
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupDirectory {
    path: PathBuf,
}

impl StartupDirectory {
    pub fn new(path: impl Into<PathBuf>) -> AppResult<Self> {
        let path = path.into();
        if path.as_os_str().is_empty() {
            return Err(AppError::InvalidInput(
                "startup directory path must not be empty",
            ));
        }

        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartupInvocation {
    working_directory: Option<StartupDirectory>,
    command: Option<StartupCommand>,
}

impl StartupInvocation {
    pub fn new(
        working_directory: Option<StartupDirectory>,
        command: Option<StartupCommand>,
    ) -> Self {
        Self {
            working_directory,
            command,
        }
    }

    pub fn working_directory(&self) -> Option<&StartupDirectory> {
        self.working_directory.as_ref()
    }

    pub fn command(&self) -> Option<&StartupCommand> {
        self.command.as_ref()
    }

    pub fn clear_command(&mut self) {
        self.command = None;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupCommand {
    arguments: Vec<StartupArgument>,
}

impl StartupCommand {
    #[cfg(test)]
    pub fn from_arguments(arguments: Vec<String>) -> AppResult<Option<Self>> {
        if arguments.is_empty() {
            return Ok(None);
        }

        let arguments = arguments
            .into_iter()
            .map(StartupArgument::from_string)
            .collect::<AppResult<Vec<_>>>()?;

        Ok(Some(Self { arguments }))
    }

    pub fn from_os_arguments(arguments: Vec<OsString>) -> AppResult<Option<Self>> {
        if arguments.is_empty() {
            return Ok(None);
        }

        let arguments = arguments
            .into_iter()
            .map(StartupArgument::from_os)
            .collect::<AppResult<Vec<_>>>()?;

        Ok(Some(Self { arguments }))
    }

    pub fn to_pty_bytes(&self, dialect: ShellCommandDialect) -> Vec<u8> {
        let mut bytes = match dialect {
            ShellCommandDialect::CommandPrompt => {
                startup_command_prompt_line(&self.arguments).into_bytes()
            }
            ShellCommandDialect::PowerShell => {
                startup_powershell_line(&self.arguments).into_bytes()
            }
            #[cfg(any(not(target_os = "windows"), test))]
            ShellCommandDialect::Posix => startup_posix_line(&self.arguments),
        };
        bytes.push(b'\r');
        bytes
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StartupArgument {
    value: String,
    #[cfg(unix)]
    posix_bytes: Option<Vec<u8>>,
}

impl StartupArgument {
    fn from_string(value: String) -> AppResult<Self> {
        validate_startup_argument(&value)?;
        Ok(Self {
            value,
            #[cfg(unix)]
            posix_bytes: None,
        })
    }

    fn from_os(argument: OsString) -> AppResult<Self> {
        startup_argument_from_os(argument)
    }

    fn value(&self) -> &str {
        &self.value
    }

    #[cfg(any(not(target_os = "windows"), test))]
    fn posix_bytes(&self) -> &[u8] {
        #[cfg(unix)]
        {
            self.posix_bytes.as_deref().unwrap_or(self.value.as_bytes())
        }

        #[cfg(not(unix))]
        {
            self.value.as_bytes()
        }
    }
}

#[cfg(unix)]
fn startup_argument_from_os(argument: OsString) -> AppResult<StartupArgument> {
    let posix_bytes = argument.as_bytes().to_vec();
    let value = argument.to_string_lossy().into_owned();
    validate_startup_argument(&value)?;
    validate_startup_argument_bytes(&posix_bytes)?;
    Ok(StartupArgument {
        value,
        posix_bytes: Some(posix_bytes),
    })
}

#[cfg(not(unix))]
fn startup_argument_from_os(argument: OsString) -> AppResult<StartupArgument> {
    argument
        .into_string()
        .map_err(|_| AppError::InvalidInput("command line arguments must be valid Unicode text"))
        .and_then(StartupArgument::from_string)
}

fn startup_command_prompt_line(arguments: &[StartupArgument]) -> String {
    arguments
        .iter()
        .map(|argument| quote_startup_argument(argument.value()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn startup_powershell_line(arguments: &[StartupArgument]) -> String {
    let mut parts = arguments.iter();
    let Some(executable) = parts.next() else {
        return String::new();
    };

    let mut command_line = format!("& {}", quote_powershell_literal(executable.value()));
    for argument in parts {
        command_line.push(' ');
        command_line.push_str(&quote_powershell_literal(argument.value()));
    }
    command_line
}

#[cfg(any(not(target_os = "windows"), test))]
fn startup_posix_line(arguments: &[StartupArgument]) -> Vec<u8> {
    let mut command_line = Vec::new();
    for argument in arguments {
        if !command_line.is_empty() {
            command_line.push(b' ');
        }
        command_line.extend(quote_posix_literal_bytes(argument.posix_bytes()));
    }
    command_line
}

fn quote_startup_argument(argument: &str) -> String {
    if argument.is_empty() {
        return "\"\"".to_owned();
    }

    if !argument_needs_quotes(argument) {
        return argument.to_owned();
    }

    let mut quoted = String::with_capacity(argument.len().saturating_add(2));
    quoted.push('"');
    quoted.push_str(argument);
    quoted.push('"');
    quoted
}

fn validate_startup_argument(argument: &str) -> AppResult<()> {
    if argument.chars().any(char::is_control) {
        return Err(AppError::InvalidInput(
            "startup command arguments must not contain control characters",
        ));
    }

    if argument.chars().any(is_unsupported_shell_character) {
        return Err(AppError::InvalidInput(
            "startup command arguments contain unsupported shell syntax characters",
        ));
    }

    Ok(())
}

#[cfg(unix)]
fn validate_startup_argument_bytes(argument: &[u8]) -> AppResult<()> {
    if argument.iter().any(u8::is_ascii_control) {
        return Err(AppError::InvalidInput(
            "startup command arguments must not contain control characters",
        ));
    }

    if argument
        .iter()
        .copied()
        .any(is_unsupported_shell_character_byte)
    {
        return Err(AppError::InvalidInput(
            "startup command arguments contain unsupported shell syntax characters",
        ));
    }

    Ok(())
}

fn is_unsupported_shell_character(character: char) -> bool {
    matches!(character, '"' | '%' | '!' | '$' | '`')
}

#[cfg(unix)]
fn is_unsupported_shell_character_byte(byte: u8) -> bool {
    matches!(byte, b'"' | b'%' | b'!' | b'$' | b'`')
}

fn argument_needs_quotes(argument: &str) -> bool {
    argument.chars().any(|character| {
        character.is_whitespace()
            || matches!(
                character,
                '&' | '|' | '<' | '>' | '(' | ')' | '^' | '\'' | ';'
            )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_text_adds_enter_for_pty_execution() {
        let command = CommandText::from_static("echo hello");

        assert_eq!(command.to_pty_bytes(), b"echo hello\r".to_vec());
    }

    #[test]
    fn predefined_command_buttons_are_shell_commands() -> AppResult<()> {
        let commands = predefined_command_buttons()
            .iter()
            .map(|button| {
                button
                    .to_command_text(
                        &ButtonArgumentValues::default(),
                        ShellCommandDialect::CommandPrompt,
                    )
                    .map(|command| command.to_pty_bytes())
            })
            .collect::<AppResult<Vec<_>>>()?;

        assert_eq!(
            commands,
            vec![
                b"cd\r".to_vec(),
                b"dir\r".to_vec(),
                b"echo hello\r".to_vec(),
                b"cls\r".to_vec()
            ]
        );
        Ok(())
    }

    #[test]
    fn command_panel_starts_with_default_category() {
        let panel = default_command_panel();

        assert_eq!(panel.categories().len(), 1);
        assert_eq!(
            panel
                .selected_category()
                .map(|category| category.name.as_str()),
            Some("Default")
        );
        assert_eq!(panel.selected_buttons().len(), 4);
    }

    #[test]
    fn command_panel_loads_from_definitions_with_fresh_runtime_ids() -> AppResult<()> {
        let panel = CommandPanel::from_definitions(
            vec![
                CommandCategoryDefinition::new(
                    "Build",
                    vec![CommandButtonDefinition::new(
                        "check",
                        "cargo",
                        CommandArguments::new("check")?,
                    )?],
                )?,
                CommandCategoryDefinition::new("Deploy", Vec::new())?,
            ],
            1,
        )?;

        assert_eq!(panel.categories().len(), 2);
        assert_eq!(
            panel
                .selected_category()
                .map(|category| category.name.as_str()),
            Some("Deploy")
        );
        assert_eq!(panel.categories()[0].buttons[0].label, "check");

        let added_category = panel.next_category_id;
        let added_button = panel.next_button_id;

        assert_eq!(added_category, 3);
        assert_eq!(added_button, 2);
        Ok(())
    }

    #[test]
    fn command_panel_selection_clone_keeps_categories_shared() -> AppResult<()> {
        let original = CommandPanel::from_definitions(
            vec![
                CommandCategoryDefinition::new("Default", Vec::new())?,
                CommandCategoryDefinition::new("Tools", Vec::new())?,
            ],
            0,
        )?;
        let mut selected = original.clone();

        assert!(Rc::ptr_eq(&original.categories, &selected.categories));

        selected.select_category_by_index(1)?;

        assert_eq!(selected.selected_category_index(), Some(1));
        assert_eq!(original.selected_category_index(), Some(0));
        assert!(Rc::ptr_eq(&original.categories, &selected.categories));

        selected.add_category()?;

        assert!(!Rc::ptr_eq(&original.categories, &selected.categories));
        assert_eq!(original.categories().len(), 2);
        assert_eq!(selected.categories().len(), 3);
        Ok(())
    }

    #[test]
    fn command_panel_adds_selects_and_deletes_category() -> AppResult<()> {
        let mut panel = default_command_panel();
        let added = panel.add_category()?;

        assert_eq!(
            panel.selected_category().map(|category| category.id),
            Some(added)
        );
        assert_eq!(
            panel
                .selected_category()
                .map(|category| category.name.as_str()),
            Some("New Category")
        );

        panel.delete_selected_category()?;

        assert_eq!(panel.categories().len(), 1);
        assert_eq!(
            panel
                .selected_category()
                .map(|category| category.name.as_str()),
            Some("Default")
        );
        Ok(())
    }

    #[test]
    fn command_panel_adds_category_with_requested_name() -> AppResult<()> {
        let mut panel = default_command_panel();
        let added = panel.add_category_named("Projects")?;

        assert_eq!(
            panel.selected_category().map(|category| category.id),
            Some(added)
        );
        assert_eq!(
            panel
                .selected_category()
                .map(|category| category.name.as_str()),
            Some("Projects")
        );
        Ok(())
    }

    #[test]
    fn command_panel_invalid_category_name_does_not_allocate_id() -> AppResult<()> {
        let mut panel = default_command_panel();
        let next_category_id = panel.next_category_id;

        assert!(matches!(
            panel.add_category_named(""),
            Err(AppError::InvalidInput("category name must not be empty"))
        ));

        assert_eq!(panel.next_category_id, next_category_id);
        assert_eq!(panel.categories().len(), 1);
        Ok(())
    }

    #[test]
    fn command_panel_renames_selected_category() -> AppResult<()> {
        let mut panel = default_command_panel();
        let renamed = panel.add_category_named("Tools")?;

        panel.rename_selected_category("Operations")?;

        assert_eq!(
            panel.selected_category().map(|category| category.id),
            Some(renamed)
        );
        assert_eq!(
            panel
                .selected_category()
                .map(|category| category.name.as_str()),
            Some("Operations")
        );
        Ok(())
    }

    #[test]
    fn command_panel_invalid_rename_keeps_category_name() -> AppResult<()> {
        let mut panel = default_command_panel();
        panel.add_category_named("Tools")?;

        assert!(matches!(
            panel.rename_selected_category(""),
            Err(AppError::InvalidInput("category name must not be empty"))
        ));

        assert_eq!(
            panel
                .selected_category()
                .map(|category| category.name.as_str()),
            Some("Tools")
        );
        Ok(())
    }

    #[test]
    fn command_panel_keeps_one_category() {
        let mut panel = default_command_panel();

        assert!(matches!(
            panel.delete_selected_category(),
            Err(AppError::InvalidState(
                "at least one command category must stay available"
            ))
        ));
    }

    #[test]
    fn command_panel_moves_categories() -> AppResult<()> {
        let mut panel = default_command_panel();
        let first = panel
            .selected_category()
            .map(|category| category.id)
            .ok_or(AppError::InvalidState("selected category should exist"))?;
        let second = panel.add_category()?;

        panel.move_selected_category_up()?;

        assert_eq!(panel.categories()[0].id, second);
        assert_eq!(panel.categories()[1].id, first);
        Ok(())
    }

    #[test]
    fn command_panel_adds_deletes_and_moves_button() -> AppResult<()> {
        let mut panel = default_command_panel();
        let added = panel.add_button_to_selected_category(CommandButtonDefinition::new(
            "new command",
            "echo",
            CommandArguments::new("new command")?,
        )?)?;

        assert_eq!(
            panel.selected_buttons().last().map(|button| button.id),
            Some(added)
        );

        panel.move_button_up(added)?;
        assert_eq!(panel.selected_buttons()[3].id, added);

        panel.delete_button(added)?;
        assert!(panel.button_by_id(added).is_none());
        Ok(())
    }

    #[test]
    fn command_panel_updates_button_definition() -> AppResult<()> {
        let mut panel = default_command_panel();
        let button_id = panel.selected_buttons()[0].id;

        panel.update_button(
            button_id,
            CommandButtonDefinition::new("open editor", "notepad.exe", CommandArguments::empty())?,
        )?;

        let button = panel
            .button_by_id(button_id)
            .ok_or(AppError::InvalidInput("updated button should exist"))?;
        assert_eq!(button.label, "open editor");
        assert_eq!(button.executable_path, "notepad.exe");
        Ok(())
    }

    #[test]
    fn command_panel_button_edits_clone_only_edited_category_buttons() -> AppResult<()> {
        let panel = command_panel_with_button_categories(64)?;
        let edited_category_id = panel
            .category_id_at_index(0)
            .ok_or(AppError::InvalidState("edited category should exist"))?;
        let untouched_category_id = panel
            .category_id_at_index(1)
            .ok_or(AppError::InvalidState("untouched category should exist"))?;
        let edited_buttons = Rc::clone(&panel.categories()[0].buttons);
        let untouched_buttons = Rc::clone(&panel.categories()[1].buttons);

        let mut updated = panel.clone();
        let update_id = updated.categories()[0].buttons[0].id;
        updated.update_button(
            update_id,
            CommandButtonDefinition::new(
                "updated command",
                "echo",
                CommandArguments::new("updated")?,
            )?,
        )?;
        assert_category_buttons_replaced(&updated, edited_category_id, &edited_buttons)?;
        assert_category_buttons_shared(&updated, untouched_category_id, &untouched_buttons)?;

        let mut added = panel.clone();
        added.add_button_to_selected_category(CommandButtonDefinition::new(
            "added command",
            "echo",
            CommandArguments::new("added")?,
        )?)?;
        assert_category_buttons_replaced(&added, edited_category_id, &edited_buttons)?;
        assert_category_buttons_shared(&added, untouched_category_id, &untouched_buttons)?;

        let mut deleted = panel.clone();
        let delete_id = deleted.categories()[0].buttons[1].id;
        deleted.delete_button(delete_id)?;
        assert_category_buttons_replaced(&deleted, edited_category_id, &edited_buttons)?;
        assert_category_buttons_shared(&deleted, untouched_category_id, &untouched_buttons)?;

        let mut moved = panel.clone();
        let move_id = moved.categories()[0].buttons[1].id;
        moved.move_button_up(move_id)?;
        assert_category_buttons_replaced(&moved, edited_category_id, &edited_buttons)?;
        assert_category_buttons_shared(&moved, untouched_category_id, &untouched_buttons)?;
        Ok(())
    }

    #[test]
    fn command_arguments_detect_required_inputs_from_template_tokens() -> AppResult<()> {
        let arguments = CommandArguments::new("{selectfile}{selectdir}{inputtext}")?;

        assert_eq!(
            arguments.required_inputs(),
            ButtonArgumentInputs {
                select_file: true,
                select_dir: true,
                input_text: true,
            }
        );

        let arguments = CommandArguments::new("--cwd {path} --name {name}")?;

        assert_eq!(arguments.required_inputs(), ButtonArgumentInputs::default());
        Ok(())
    }

    #[test]
    fn button_command_replaces_runtime_tokens_for_cmd() -> AppResult<()> {
        let button = CommandButton::new(
            CommandButtonId::new(7),
            "tool",
            r"C:\Tools\tool.exe",
            CommandArguments::new(
                "--cwd {path} --dir {name} --file {selectfile} --text {inputtext}",
            )?,
        )?;
        let values = ButtonArgumentValues {
            selected_file: Some(r"C:\Users\me\file name.txt".to_owned()),
            input_text: Some("hello world".to_owned()),
            ..ButtonArgumentValues::default()
        };

        let command = button.to_command_text(&values, ShellCommandDialect::CommandPrompt)?;

        assert_eq!(
            command.to_pty_bytes(),
            b"for %J in (\"%CD%\") do C:\\Tools\\tool.exe --cwd \"%CD%\" --dir \"%~nxJ\" --file \"C:\\Users\\me\\file name.txt\" --text \"hello world\"\r".to_vec()
        );
        Ok(())
    }

    #[test]
    fn button_command_rejects_cmd_literal_metacharacters_in_executable_path() -> AppResult<()> {
        for executable_path in [
            r"C:\Tools\%TEMP%\tool.exe",
            r"C:\Tools\!TEMP!\tool.exe",
            r"C:\Tools\^tool.exe",
            r#"C:\Tools\bad"tool.exe"#,
        ] {
            let button = CommandButton::new(
                CommandButtonId::new(8),
                "tool",
                executable_path,
                CommandArguments::empty(),
            )?;

            assert_cmd_literal_rejected(button.to_command_text(
                &ButtonArgumentValues::default(),
                ShellCommandDialect::CommandPrompt,
            ));
        }

        Ok(())
    }

    #[test]
    fn button_command_rejects_cmd_literal_metacharacters_in_runtime_values() -> AppResult<()> {
        let cases = [
            (
                CommandArguments::new("--file {selectfile}")?,
                ButtonArgumentValues {
                    selected_file: Some(r"C:\Temp\%USERNAME%.txt".to_owned()),
                    ..ButtonArgumentValues::default()
                },
            ),
            (
                CommandArguments::new("--folder {selectdir}")?,
                ButtonArgumentValues {
                    selected_dir: Some(r"C:\Temp\!USERNAME!".to_owned()),
                    ..ButtonArgumentValues::default()
                },
            ),
            (
                CommandArguments::new("--text {inputtext}")?,
                ButtonArgumentValues {
                    input_text: Some("caret ^ value".to_owned()),
                    ..ButtonArgumentValues::default()
                },
            ),
            (
                CommandArguments::new("--text {inputtext}")?,
                ButtonArgumentValues {
                    input_text: Some("say \"hello\"".to_owned()),
                    ..ButtonArgumentValues::default()
                },
            ),
        ];

        for (arguments, values) in cases {
            let button =
                CommandButton::new(CommandButtonId::new(9), "tool", "tool.exe", arguments)?;

            assert_cmd_literal_rejected(
                button.to_command_text(&values, ShellCommandDialect::CommandPrompt),
            );
        }

        Ok(())
    }

    #[test]
    fn button_command_replaces_runtime_tokens_for_powershell() -> AppResult<()> {
        let button = CommandButton::new(
            CommandButtonId::new(8),
            "tool",
            r"C:\Tools\tool.exe",
            CommandArguments::new("--cwd {path} --dir {name} --folder {selectdir}")?,
        )?;
        let values = ButtonArgumentValues {
            selected_dir: Some(r"C:\Users\me\Folder One".to_owned()),
            ..ButtonArgumentValues::default()
        };

        let command = button.to_command_text(&values, ShellCommandDialect::PowerShell)?;

        assert_eq!(
            command.to_pty_bytes(),
            b"& 'C:\\Tools\\tool.exe' --cwd $PWD.Path --dir (Split-Path -Leaf $PWD.Path) --folder 'C:\\Users\\me\\Folder One'\r".to_vec()
        );
        Ok(())
    }

    #[test]
    fn button_command_preserves_token_literals_inside_runtime_values() -> AppResult<()> {
        let button = CommandButton::new(
            CommandButtonId::new(9),
            "tool",
            r"C:\Tools\tool.exe",
            CommandArguments::new(
                "--file {selectfile} --folder {selectdir} --text {inputtext} --cwd {path} --name {name}",
            )?,
        )?;
        let values = ButtonArgumentValues {
            selected_file: Some(r"C:\Temp\{selectdir}\{inputtext}.txt".to_owned()),
            selected_dir: Some(r"C:\Temp\{selectfile}\{inputtext}".to_owned()),
            input_text: Some("keep {selectfile} {selectdir} {path} {name} {inputtext}".to_owned()),
            ..ButtonArgumentValues::default()
        };

        let command = button.to_command_text(&values, ShellCommandDialect::PowerShell)?;

        assert_eq!(
            command.to_pty_bytes(),
            b"& 'C:\\Tools\\tool.exe' --file 'C:\\Temp\\{selectdir}\\{inputtext}.txt' --folder 'C:\\Temp\\{selectfile}\\{inputtext}' --text 'keep {selectfile} {selectdir} {path} {name} {inputtext}' --cwd $PWD.Path --name (Split-Path -Leaf $PWD.Path)\r".to_vec()
        );
        Ok(())
    }

    #[test]
    fn button_command_accepts_cmd_metacharacters_for_posix() -> AppResult<()> {
        let button = CommandButton::new(
            CommandButtonId::new(10),
            "tool",
            r#"/opt/tools/100%!"^/tool"#,
            CommandArguments::new("--file {selectfile} --text {inputtext}")?,
        )?;
        let values = ButtonArgumentValues {
            selected_file: Some(r#"/tmp/100%!"^ file.txt"#.to_owned()),
            input_text: Some(r#"say "100%!"^"#.to_owned()),
            ..ButtonArgumentValues::default()
        };

        let command = button.to_command_text(&values, ShellCommandDialect::Posix)?;

        assert_eq!(
            command.to_pty_bytes(),
            b"'/opt/tools/100%!\"^/tool' --file '/tmp/100%!\"^ file.txt' --text 'say \"100%!\"^'\r"
                .to_vec()
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn button_command_preserves_non_utf8_selected_path_bytes_for_posix() -> AppResult<()> {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let button = CommandButton::new(
            CommandButtonId::new(11),
            "tool",
            "/bin/cat",
            CommandArguments::new("--file {selectfile}")?,
        )?;
        let path_bytes = vec![
            b'/', b't', b'm', b'p', b'/', b'n', b'o', b'n', b'u', b't', b'f', b'-', 0xff, b'-',
            b'\'', b'e', b'n', b'd',
        ];
        let mut values = ButtonArgumentValues::default();
        values.set_selected_file_path(PathBuf::from(OsString::from_vec(path_bytes.clone())));

        let command = button.to_command_text(&values, ShellCommandDialect::Posix)?;

        let mut expected = b"/bin/cat --file '".as_slice().to_vec();
        for byte in path_bytes {
            if byte == b'\'' {
                expected.extend(b"'\\''");
            } else {
                expected.push(byte);
            }
        }
        expected.extend(b"'\r");
        assert_eq!(command.to_pty_bytes(), expected);
        Ok(())
    }

    #[test]
    fn button_command_replaces_runtime_tokens_for_posix() -> AppResult<()> {
        let button = CommandButton::new(
            CommandButtonId::new(12),
            "tool",
            "/opt/tools/tool",
            CommandArguments::new("--cwd {path} --dir {name} --file {selectfile}")?,
        )?;
        let values = ButtonArgumentValues {
            selected_file: Some("/tmp/file name.txt".to_owned()),
            ..ButtonArgumentValues::default()
        };

        let command = button.to_command_text(&values, ShellCommandDialect::Posix)?;

        assert_eq!(
            command.to_pty_bytes(),
            b"/opt/tools/tool --cwd \"$PWD\" --dir \"${PWD##*/}\" --file '/tmp/file name.txt'\r"
                .to_vec()
        );
        Ok(())
    }

    #[test]
    fn posix_default_command_buttons_are_shell_commands() -> AppResult<()> {
        let panel = CommandPanel::posix_default_commands();
        let buttons = panel.selected_buttons();
        let labels = buttons
            .iter()
            .map(|button| button.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(labels, vec!["cd", "dir", "echo hello", "cls"]);

        let commands = buttons
            .iter()
            .map(|button| {
                button
                    .to_command_text(&ButtonArgumentValues::default(), ShellCommandDialect::Posix)
                    .map(|command| command.to_pty_bytes())
            })
            .collect::<AppResult<Vec<_>>>()?;

        assert_eq!(
            commands,
            vec![
                b"pwd\r".to_vec(),
                b"ls\r".to_vec(),
                b"echo hello\r".to_vec(),
                b"clear\r".to_vec()
            ]
        );
        Ok(())
    }

    #[test]
    fn startup_command_is_absent_without_arguments() -> AppResult<()> {
        assert_eq!(StartupCommand::from_arguments(Vec::new())?, None);
        Ok(())
    }

    #[test]
    fn startup_directory_rejects_empty_path() {
        assert!(matches!(
            StartupDirectory::new(PathBuf::new()),
            Err(AppError::InvalidInput(
                "startup directory path must not be empty"
            ))
        ));
    }

    #[test]
    fn startup_invocation_groups_directory_and_command() -> AppResult<()> {
        let directory = StartupDirectory::new(PathBuf::from(r"C:\Windows"))?;
        let command = StartupCommand::from_arguments(vec!["cargo".to_owned(), "test".to_owned()])?
            .ok_or(AppError::InvalidInput("startup command should exist"))?;
        let mut invocation = StartupInvocation::new(Some(directory.clone()), Some(command));

        assert_eq!(invocation.working_directory(), Some(&directory));
        assert!(invocation.command().is_some());

        invocation.clear_command();

        assert!(invocation.command().is_none());
        Ok(())
    }

    #[test]
    fn startup_command_writes_single_argument_with_enter() -> AppResult<()> {
        let command = StartupCommand::from_arguments(vec!["script.bat".to_owned()])?
            .ok_or(AppError::InvalidInput("startup command should exist"))?;

        assert_eq!(
            command.to_pty_bytes(ShellCommandDialect::CommandPrompt),
            b"script.bat\r".to_vec()
        );
        Ok(())
    }

    #[test]
    fn startup_command_quotes_notepad_style_path_argument() -> AppResult<()> {
        let command =
            StartupCommand::from_arguments(vec![r"C:\Users\me\My Script.bat".to_owned()])?
                .ok_or(AppError::InvalidInput("startup command should exist"))?;

        assert_eq!(
            command.to_pty_bytes(ShellCommandDialect::CommandPrompt),
            b"\"C:\\Users\\me\\My Script.bat\"\r".to_vec()
        );
        Ok(())
    }

    #[test]
    fn startup_command_preserves_multiple_arguments() -> AppResult<()> {
        let command = StartupCommand::from_arguments(vec![
            "cargo".to_owned(),
            "test".to_owned(),
            "--all-targets".to_owned(),
        ])?
        .ok_or(AppError::InvalidInput("startup command should exist"))?;

        assert_eq!(
            command.to_pty_bytes(ShellCommandDialect::CommandPrompt),
            b"cargo test --all-targets\r".to_vec()
        );
        Ok(())
    }

    #[test]
    fn startup_command_uses_powershell_call_operator_for_path_argument() -> AppResult<()> {
        let command = StartupCommand::from_arguments(vec![
            r"C:\Users\me\My Script.ps1".to_owned(),
            "hello world".to_owned(),
        ])?
        .ok_or(AppError::InvalidInput("startup command should exist"))?;

        assert_eq!(
            command.to_pty_bytes(ShellCommandDialect::PowerShell),
            b"& 'C:\\Users\\me\\My Script.ps1' 'hello world'\r".to_vec()
        );
        Ok(())
    }

    #[test]
    fn startup_command_quotes_posix_arguments() -> AppResult<()> {
        let command = StartupCommand::from_arguments(vec![
            "/tmp/my script.sh".to_owned(),
            "hello world".to_owned(),
        ])?
        .ok_or(AppError::InvalidInput("startup command should exist"))?;

        assert_eq!(
            command.to_pty_bytes(ShellCommandDialect::Posix),
            b"'/tmp/my script.sh' 'hello world'\r".to_vec()
        );
        Ok(())
    }

    #[test]
    fn startup_command_rejects_quote_argument() {
        assert!(matches!(
            StartupCommand::from_arguments(vec!["say \"hello\"".to_owned()]),
            Err(AppError::InvalidInput(
                "startup command arguments contain unsupported shell syntax characters"
            ))
        ));
    }

    #[test]
    fn startup_command_rejects_control_argument() {
        assert!(matches!(
            StartupCommand::from_arguments(vec!["script.bat\rwhoami".to_owned()]),
            Err(AppError::InvalidInput(
                "startup command arguments must not contain control characters"
            ))
        ));

        assert!(matches!(
            StartupCommand::from_arguments(vec!["script.bat\nwhoami".to_owned()]),
            Err(AppError::InvalidInput(
                "startup command arguments must not contain control characters"
            ))
        ));
    }

    fn assert_cmd_literal_rejected<T>(result: AppResult<T>) {
        assert!(matches!(
            result,
            Err(AppError::InvalidInput(CMD_LITERAL_UNSUPPORTED_MESSAGE))
        ));
    }

    fn command_panel_with_button_categories(button_count: usize) -> AppResult<CommandPanel> {
        CommandPanel::from_definitions(
            vec![
                CommandCategoryDefinition::new(
                    "Default",
                    command_button_definitions("default", button_count)?,
                )?,
                CommandCategoryDefinition::new(
                    "Tools",
                    command_button_definitions("tools", button_count)?,
                )?,
            ],
            0,
        )
    }

    fn command_button_definitions(
        prefix: &str,
        button_count: usize,
    ) -> AppResult<Vec<CommandButtonDefinition>> {
        let mut definitions = Vec::with_capacity(button_count);
        for index in 0..button_count {
            definitions.push(CommandButtonDefinition::new(
                format!("{prefix} {index}"),
                "echo",
                CommandArguments::new(format!("{prefix}-{index}"))?,
            )?);
        }
        Ok(definitions)
    }

    fn assert_category_buttons_shared(
        panel: &CommandPanel,
        category_id: CommandCategoryId,
        expected: &Rc<Vec<CommandButton>>,
    ) -> AppResult<()> {
        let actual = category_buttons(panel, category_id)?;
        assert!(Rc::ptr_eq(actual, expected));
        Ok(())
    }

    fn assert_category_buttons_replaced(
        panel: &CommandPanel,
        category_id: CommandCategoryId,
        previous: &Rc<Vec<CommandButton>>,
    ) -> AppResult<()> {
        let actual = category_buttons(panel, category_id)?;
        assert!(!Rc::ptr_eq(actual, previous));
        Ok(())
    }

    fn category_buttons(
        panel: &CommandPanel,
        category_id: CommandCategoryId,
    ) -> AppResult<&Rc<Vec<CommandButton>>> {
        panel
            .categories()
            .iter()
            .find(|category| category.id == category_id)
            .map(|category| &category.buttons)
            .ok_or(AppError::InvalidState("command category should exist"))
    }
}
