//! Core and MCP errors as data (T-006, TZ §103, D46, D60).

use std::fmt;

/// Machine-readable codes from TZ §103. Not free-form strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    AccountNotFound,
    MessageNotFound,
    PermissionDenied,
    NetworkUnavailable,
    AuthRequired,
    Conflict,
    InvalidArgument,
    OperationNotSupported,
    AttachmentTooLarge,
}

impl ErrorCode {
    pub const ALL: [Self; 9] = [
        Self::AccountNotFound,
        Self::MessageNotFound,
        Self::PermissionDenied,
        Self::NetworkUnavailable,
        Self::AuthRequired,
        Self::Conflict,
        Self::InvalidArgument,
        Self::OperationNotSupported,
        Self::AttachmentTooLarge,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::AccountNotFound => "ACCOUNT_NOT_FOUND",
            Self::MessageNotFound => "MESSAGE_NOT_FOUND",
            Self::PermissionDenied => "PERMISSION_DENIED",
            Self::NetworkUnavailable => "NETWORK_UNAVAILABLE",
            Self::AuthRequired => "AUTH_REQUIRED",
            Self::Conflict => "CONFLICT",
            Self::InvalidArgument => "INVALID_ARGUMENT",
            Self::OperationNotSupported => "OPERATION_NOT_SUPPORTED",
            Self::AttachmentTooLarge => "ATTACHMENT_TOO_LARGE",
        }
    }

    /// Human line for the UI (D46). Technical text goes in [`CoreError::details`].
    pub fn default_message(self) -> &'static str {
        match self {
            Self::AccountNotFound => "That account isn't in this mailbox.",
            Self::MessageNotFound => "That message isn't here anymore.",
            Self::PermissionDenied => "This action needs your approval.",
            Self::NetworkUnavailable => "Couldn't reach the server.",
            Self::AuthRequired => "Sign in again to continue.",
            Self::Conflict => "This change conflicted with the server.",
            Self::InvalidArgument => "That request isn't valid.",
            Self::OperationNotSupported => "This action isn't available.",
            Self::AttachmentTooLarge => "That attachment is too large.",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ErrorCode {
    type Err = ParseErrorCodeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|c| c.as_str() == s)
            .ok_or(ParseErrorCodeError)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseErrorCodeError;

impl fmt::Display for ParseErrorCodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unknown error code")
    }
}

impl std::error::Error for ParseErrorCodeError {}

/// Structured failure. `code` is the contract; `message` is the toast.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoreError {
    pub code: ErrorCode,
    pub message: String,
    pub details: Option<String>,
}

impl CoreError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
        }
    }

    pub fn from_code(code: ErrorCode) -> Self {
        Self::new(code, code.default_message())
    }

    pub fn with_details(mut self, details: impl Into<String>) -> Self {
        self.details = Some(details.into());
        self
    }
}

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CoreError {}

impl From<ErrorCode> for CoreError {
    fn from(code: ErrorCode) -> Self {
        Self::from_code(code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_103_codes_are_enum_not_strings() {
        let parsed: Vec<&str> = ErrorCode::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(
            parsed,
            [
                "ACCOUNT_NOT_FOUND",
                "MESSAGE_NOT_FOUND",
                "PERMISSION_DENIED",
                "NETWORK_UNAVAILABLE",
                "AUTH_REQUIRED",
                "CONFLICT",
                "INVALID_ARGUMENT",
                "OPERATION_NOT_SUPPORTED",
                "ATTACHMENT_TOO_LARGE",
            ]
        );
        let e = CoreError::from_code(ErrorCode::PermissionDenied)
            .with_details("tool send_message; grant missing");
        assert_eq!(e.code, ErrorCode::PermissionDenied);
        assert_eq!(e.code.as_str(), "PERMISSION_DENIED");
        assert!(!e.message.contains("IMAP"));
        assert_eq!("PERMISSION_DENIED".parse::<ErrorCode>(), Ok(e.code));
        assert!("not-a-code".parse::<ErrorCode>().is_err());
    }
}
