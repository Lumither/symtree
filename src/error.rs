use std::{error::Error, io};

pub type AppResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

pub fn app_error(message: impl Into<String>) -> Box<dyn Error + Send + Sync> {
    Box::new(io::Error::other(message.into()))
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
        self.map_err(|error| app_error(format!("{}: {error}", message.into())))
    }

    fn with_context(self, message: impl FnOnce() -> String) -> AppResult<T> {
        self.map_err(|error| app_error(format!("{}: {error}", message())))
    }
}
