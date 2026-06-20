use std::fmt;
use std::io;

pub type AppResult<T> = Result<T, AppError>;

type ErrorSource = Box<dyn std::error::Error + Send + 'static>;

#[derive(Debug)]
struct DisplayErrorSource<E> {
    source: E,
}

impl<E> fmt::Display for DisplayErrorSource<E>
where
    E: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(formatter)
    }
}

impl<E> std::error::Error for DisplayErrorSource<E> where E: fmt::Debug + fmt::Display {}

#[derive(Debug)]
pub struct ErrorContext {
    operation: &'static str,
    user_message: String,
    cause: String,
    source: Option<ErrorSource>,
}

impl ErrorContext {
    fn from_message(
        operation: &'static str,
        user_message: impl Into<String>,
        cause: impl Into<String>,
    ) -> Self {
        Self {
            operation,
            user_message: user_message.into(),
            cause: cause.into(),
            source: None,
        }
    }

    fn from_source<E>(operation: &'static str, user_message: impl Into<String>, source: E) -> Self
    where
        E: fmt::Debug + fmt::Display + Send + 'static,
    {
        let cause = source.to_string();
        Self {
            operation,
            user_message: user_message.into(),
            cause,
            source: Some(Box::new(DisplayErrorSource { source })),
        }
    }

    pub fn operation(&self) -> &'static str {
        self.operation
    }

    pub fn user_message(&self) -> &str {
        &self.user_message
    }

    pub fn cause(&self) -> &str {
        &self.cause
    }

    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

#[derive(Debug)]
pub enum AppError {
    Io {
        operation: &'static str,
        source: io::Error,
    },
    InvalidInput(&'static str),
    InvalidState(&'static str),
    Pty(ErrorContext),
    Ui(ErrorContext),
    Win32 {
        operation: &'static str,
        source: io::Error,
    },
}

impl AppError {
    const PTY_USER_MESSAGE: &'static str = "The terminal backend reported an error.";
    const UI_USER_MESSAGE: &'static str = "The window could not complete the requested operation.";

    pub fn io(operation: &'static str, source: io::Error) -> Self {
        Self::Io { operation, source }
    }

    pub fn pty<E>(operation: &'static str, source: E) -> Self
    where
        E: fmt::Debug + fmt::Display + Send + 'static,
    {
        Self::Pty(ErrorContext::from_source(
            operation,
            Self::PTY_USER_MESSAGE,
            source,
        ))
    }

    pub fn pty_message(operation: &'static str, cause: impl Into<String>) -> Self {
        Self::Pty(ErrorContext::from_message(
            operation,
            Self::PTY_USER_MESSAGE,
            cause,
        ))
    }

    pub fn ui_message(operation: &'static str, cause: impl Into<String>) -> Self {
        Self::Ui(ErrorContext::from_message(
            operation,
            Self::UI_USER_MESSAGE,
            cause,
        ))
    }

    pub fn win32(operation: &'static str) -> Self {
        Self::Win32 {
            operation,
            source: io::Error::last_os_error(),
        }
    }

    pub fn user_message(&self) -> &str {
        match self {
            Self::Io { .. } => "A system I/O operation failed.",
            Self::InvalidInput(message) | Self::InvalidState(message) => message,
            Self::Pty(context) | Self::Ui(context) => context.user_message(),
            Self::Win32 { .. } => "A Windows API operation failed.",
        }
    }

    pub fn operation(&self) -> Option<&'static str> {
        match self {
            Self::Io { operation, .. } | Self::Win32 { operation, .. } => Some(*operation),
            Self::Pty(context) | Self::Ui(context) => Some(context.operation()),
            Self::InvalidInput(_) | Self::InvalidState(_) => None,
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { operation, source } => {
                write!(formatter, "{operation} failed: {source}")
            }
            Self::InvalidInput(message) => write!(formatter, "invalid input: {message}"),
            Self::InvalidState(message) => write!(formatter, "invalid state: {message}"),
            Self::Pty(context) => write!(formatter, "pty error: {}", context.cause()),
            Self::Ui(context) => write!(formatter, "ui error: {}", context.cause()),
            Self::Win32 { operation, source } => match source.raw_os_error() {
                Some(code) => write!(formatter, "{operation} failed with Win32 error {code}"),
                None => write!(formatter, "{operation} failed with unknown Win32 error"),
            },
        }
    }
}

impl std::error::Error for AppError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Pty(context) | Self::Ui(context) => context.source(),
            Self::Win32 { source, .. } => Some(source),
            Self::InvalidInput(_) | Self::InvalidState(_) => None,
        }
    }
}

impl From<io::Error> for AppError {
    fn from(source: io::Error) -> Self {
        Self::io("io operation", source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pty_error_preserves_context_and_source() {
        let error = AppError::pty("open pty", io::Error::other("pty unavailable"));

        assert_eq!(error.operation(), Some("open pty"));
        assert_eq!(
            error.user_message(),
            "The terminal backend reported an error."
        );

        let AppError::Pty(context) = &error else {
            panic!("expected pty error");
        };
        assert_eq!(context.cause(), "pty unavailable");

        let source = std::error::Error::source(&error);
        assert!(matches!(source, Some(source) if source.to_string() == "pty unavailable"));
    }

    #[test]
    fn message_only_pty_error_keeps_source_empty() {
        let error = AppError::pty_message("join pty reader thread", "pty reader thread panicked");

        assert_eq!(error.operation(), Some("join pty reader thread"));
        assert!(std::error::Error::source(&error).is_none());
    }
}
