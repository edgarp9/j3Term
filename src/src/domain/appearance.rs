use crate::error::{AppError, AppResult};

pub const DEFAULT_FONT_FAMILY: &str = "monospace";
pub const DEFAULT_FONT_SIZE_POINTS: u16 = 14;
pub const MIN_FONT_SIZE_POINTS: u16 = 6;
pub const MAX_FONT_SIZE_POINTS: u16 = 72;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalFont {
    family: String,
    size_points: u16,
}

impl TerminalFont {
    pub fn new(family: impl Into<String>, size_points: u16) -> AppResult<Self> {
        let family = family.into();
        validate_font_family(&family)?;
        validate_font_size(size_points)?;
        Ok(Self {
            family,
            size_points,
        })
    }

    pub fn family(&self) -> &str {
        &self.family
    }

    pub fn size_points(&self) -> u16 {
        self.size_points
    }
}

impl Default for TerminalFont {
    fn default() -> Self {
        Self {
            family: DEFAULT_FONT_FAMILY.to_owned(),
            size_points: DEFAULT_FONT_SIZE_POINTS,
        }
    }
}

fn validate_font_family(family: &str) -> AppResult<()> {
    if family.trim().is_empty() {
        return Err(AppError::InvalidInput("font family must not be empty"));
    }

    if family.chars().any(char::is_control) {
        return Err(AppError::InvalidInput(
            "font family must not contain control characters",
        ));
    }

    Ok(())
}

fn validate_font_size(size_points: u16) -> AppResult<()> {
    if !(MIN_FONT_SIZE_POINTS..=MAX_FONT_SIZE_POINTS).contains(&size_points) {
        return Err(AppError::InvalidInput(
            "font size must be between 6 and 72 points",
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_font_rejects_empty_family() {
        assert!(TerminalFont::new(" ", DEFAULT_FONT_SIZE_POINTS).is_err());
    }

    #[test]
    fn terminal_font_rejects_out_of_range_size() {
        assert!(TerminalFont::new(DEFAULT_FONT_FAMILY, MIN_FONT_SIZE_POINTS - 1).is_err());
        assert!(TerminalFont::new(DEFAULT_FONT_FAMILY, MAX_FONT_SIZE_POINTS + 1).is_err());
    }
}
