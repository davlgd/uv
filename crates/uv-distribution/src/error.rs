use std::path::PathBuf;
use tokio::task::JoinError;
use zip::result::ZipError;

use distribution_filename::WheelFilenameError;
use uv_client::BetterReqwestError;
use uv_normalize::PackageName;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Building source distributions is disabled")]
    NoBuild,
    #[error("Using pre-built wheels is disabled")]
    NoBinary,

    // Network error
    #[error("Failed to parse URL: `{0}`")]
    Url(String, #[source] url::ParseError),
    #[error(transparent)]
    JoinRelativeUrl(#[from] pypi_types::JoinRelativeError),
    #[error("Git operation failed")]
    Git(#[source] anyhow::Error),
    #[error(transparent)]
    Reqwest(#[from] BetterReqwestError),
    #[error(transparent)]
    Client(#[from] uv_client::Error),

    // Cache writing error
    #[error("Failed to read from the distribution cache")]
    CacheRead(#[source] std::io::Error),
    #[error("Failed to write to the distribution cache")]
    CacheWrite(#[source] std::io::Error),
    #[error("Failed to deserialize cache entry")]
    CacheDecode(#[from] rmp_serde::decode::Error),
    #[error("Failed to serialize cache entry")]
    CacheEncode(#[from] rmp_serde::encode::Error),

    // Build error
    #[error("Failed to build: {0}")]
    Build(String, #[source] anyhow::Error),
    #[error("Failed to build editable: {0}")]
    BuildEditable(String, #[source] anyhow::Error),
    #[error("Built wheel has an invalid filename")]
    WheelFilename(#[from] WheelFilenameError),
    #[error("Package metadata name `{metadata}` does not match given name `{given}`")]
    NameMismatch {
        given: PackageName,
        metadata: PackageName,
    },
    #[error("Failed to parse metadata from built wheel")]
    Metadata(#[from] pypi_types::MetadataError),
    #[error("Failed to read `dist-info` metadata from built wheel")]
    DistInfo(#[from] install_wheel_rs::Error),
    #[error("Failed to read zip archive from built wheel")]
    Zip(#[from] ZipError),
    #[error("Source distribution directory contains neither readable pyproject.toml nor setup.py")]
    DirWithoutEntrypoint,
    #[error("Failed to extract archive")]
    Extract(#[from] uv_extract::Error),
    #[error("Source distribution not found at: {0}")]
    NotFound(PathBuf),
    #[error("The source distribution is missing a `PKG-INFO` file")]
    MissingPkgInfo,
    #[error("The source distribution does not support static metadata in `PKG-INFO`")]
    DynamicPkgInfo(#[source] pypi_types::MetadataError),
    #[error("The source distribution is missing a `pyproject.toml` file")]
    MissingPyprojectToml,
    #[error("The source distribution does not support static metadata in `pyproject.toml`")]
    DynamicPyprojectToml(#[source] pypi_types::MetadataError),
    #[error("Unsupported scheme in URL: {0}")]
    UnsupportedScheme(String),

    /// A generic request middleware error happened while making a request.
    /// Refer to the error message for more details.
    #[error(transparent)]
    ReqwestMiddlewareError(#[from] anyhow::Error),

    /// Should not occur; only seen when another task panicked.
    #[error("The task executor is broken, did some other task panic?")]
    Join(#[from] JoinError),
}

impl From<reqwest::Error> for Error {
    fn from(error: reqwest::Error) -> Self {
        Self::Reqwest(BetterReqwestError::from(error))
    }
}

impl From<reqwest_middleware::Error> for Error {
    fn from(error: reqwest_middleware::Error) -> Self {
        match error {
            reqwest_middleware::Error::Middleware(error) => Self::ReqwestMiddlewareError(error),
            reqwest_middleware::Error::Reqwest(error) => {
                Self::Reqwest(BetterReqwestError::from(error))
            }
        }
    }
}
