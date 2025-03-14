#[derive(Debug, Clone)]
pub enum FbxLoadingError {
    IncorrectFileVersion,
    UnknownError,
    Other(String),
}

impl std::error::Error for FbxLoadingError {}

impl std::fmt::Display for FbxLoadingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IncorrectFileVersion => write!(f, "Incorrect file version"),
            Self::UnknownError => write!(f, "Unknown error"),
            Self::Other(s) => write!(f, "{}", s),
        }
    }
}

impl From<std::io::Error> for FbxLoadingError {
    fn from(err: std::io::Error) -> Self {
        Self::Other(err.to_string())
    }
}
