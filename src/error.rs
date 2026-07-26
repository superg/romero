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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_errors_display_their_messages_without_sources() {
        for error in [
            RomeroError::InvalidRoot("root".into()),
            RomeroError::Config("config".into()),
            RomeroError::Dat("dat".into()),
            RomeroError::Cache("cache".into()),
            RomeroError::Operational("operation".into()),
        ] {
            assert!(error.source().is_none());
            assert!(!error.to_string().is_empty());
        }
    }

    #[test]
    fn io_errors_include_the_action_path_and_source() {
        let error = RomeroError::io_path(
            "cannot read",
            Path::new("/root/file"),
            io::Error::new(io::ErrorKind::PermissionDenied, "denied"),
        );

        assert_eq!(error.to_string(), "cannot read /root/file: denied");
        assert_eq!(
            error
                .source()
                .and_then(|source| source.downcast_ref::<io::Error>())
                .map(io::Error::kind),
            Some(io::ErrorKind::PermissionDenied)
        );
    }
}
