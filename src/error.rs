use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::io;
use std::path::Path;

pub type Result<T> = std::result::Result<T, RomeroError>;

#[derive(Debug)]
pub enum RomeroError {
    InvalidRoot(String),
    Config(String),
    Dat(String),
    Cache(String),
    Operational(String),
    Io { action: String, source: io::Error },
}

impl RomeroError {
    pub(crate) fn io(action: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            action: action.into(),
            source,
        }
    }

    pub(crate) fn io_path(action: &str, path: &Path, source: io::Error) -> Self {
        Self::io(format!("{action} {}", path.display()), source)
    }
}

impl Display for RomeroError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRoot(message)
            | Self::Config(message)
            | Self::Dat(message)
            | Self::Cache(message)
            | Self::Operational(message) => formatter.write_str(message),
            Self::Io { action, source } => write!(formatter, "{action}: {source}"),
        }
    }
}

impl Error for RomeroError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}
