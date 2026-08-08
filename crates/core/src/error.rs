//! Library error type (thiserror). Large underlying errors are boxed so the
//! enum stays small; `#[from]` is used only for small conversions.

/// Errors surfaced by the `procmon` core library.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// Configuration loading or parsing failed.
    #[error("config error: {0}")]
    Config(#[source] Box<figment::Error>),

    /// A runtime directory could not be resolved.
    #[error("directory resolution failed: {0}")]
    Dirs(String),

    /// A filesystem operation failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// The web server failed to bind or serve.
    #[error("web server error: {0}")]
    Web(String),
}

impl From<figment::Error> for CoreError {
    fn from(e: figment::Error) -> Self {
        Self::Config(Box::new(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_messages_are_prefixed() {
        let io = CoreError::Io(std::io::Error::other("boom"));
        assert!(io.to_string().starts_with("io error:"));
        let dirs = CoreError::Dirs("nope".into());
        assert!(dirs.to_string().contains("nope"));
        let web = CoreError::Web("bind failed".into());
        assert!(web.to_string().contains("bind failed"));
    }

    #[test]
    fn io_error_converts_via_from() {
        let e: CoreError = std::io::Error::other("x").into();
        assert!(matches!(e, CoreError::Io(_)));
    }
}
