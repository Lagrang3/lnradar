#[derive(thiserror::Error, core::fmt::Debug)]
pub enum Error {
    #[error("Could not find any routes: {0}")]
    NoRoutes(String),

    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn no_routes(e: impl std::fmt::Display) -> Error {
        Error::NoRoutes(e.to_string())
    }

    pub fn other(e: impl std::fmt::Display) -> Error {
        Error::Other(e.to_string())
    }
}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Error {
        Error::Other(e.to_string())
    }
}
