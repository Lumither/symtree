use std::{error::Error, fmt};

pub type AppResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

/// An owned error that is honestly itself rather than a mislabeled `io::Error`.
/// When built through [`AppContext`], it keeps the underlying error as its
/// [`Error::source`] instead of flattening the chain into one string, while its
/// `Display` still renders the full `context: cause` text so user-facing output
/// is unchanged.
#[derive(Debug)]
pub struct AppError {
    message: String,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)?;
        if let Some(source) = &self.source {
            write!(f, ": {source}")?;
        }
        Ok(())
    }
}

impl Error for AppError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_ref()
            .map(|source| source.as_ref() as &(dyn Error + 'static))
    }
}

pub fn app_error(message: impl Into<String>) -> Box<dyn Error + Send + Sync> {
    Box::new(AppError {
        message: message.into(),
        source: None,
    })
}

pub trait AppContext<T> {
    fn context(self, message: impl Into<String>) -> AppResult<T>;
    fn with_context(self, message: impl FnOnce() -> String) -> AppResult<T>;
}

impl<T, E> AppContext<T> for Result<T, E>
where
    E: Error + Send + Sync + 'static,
{
    fn context(self, message: impl Into<String>) -> AppResult<T> {
        self.map_err(|error| wrap(message.into(), error))
    }

    fn with_context(self, message: impl FnOnce() -> String) -> AppResult<T> {
        self.map_err(|error| wrap(message(), error))
    }
}

fn wrap<E: Error + Send + Sync + 'static>(
    message: String,
    error: E,
) -> Box<dyn Error + Send + Sync> {
    Box::new(AppError {
        message,
        source: Some(Box::new(error)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn context_preserves_source_chain() {
        let io_err = io::Error::new(io::ErrorKind::PermissionDenied, "denied");
        let result: AppResult<()> = Err(io_err).context("failed to read config");
        let err = result.unwrap_err();

        // Display renders the full chain, unchanged from the old behavior.
        assert_eq!(err.to_string(), "failed to read config: denied");
        // ...but the source is now a real chain link, not flattened away.
        let source = err.source().expect("source preserved");
        assert_eq!(source.to_string(), "denied");
    }

    #[test]
    fn app_error_has_no_source() {
        let err = app_error("standalone");
        assert_eq!(err.to_string(), "standalone");
        assert!(err.source().is_none());
    }
}
