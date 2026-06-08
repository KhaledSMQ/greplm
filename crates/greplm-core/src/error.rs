use std::path::PathBuf;

/// Result type used across greplm-core.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors produced by the greplm core engine.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("io error: {0}")]
    PlainIo(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("cache serialize error: {0}")]
    Postcard(#[from] postcard::Error),

    #[error("toml deserialize error: {0}")]
    TomlDe(#[from] toml::de::Error),

    #[error("toml serialize error: {0}")]
    TomlSer(#[from] toml::ser::Error),

    #[error("fst error: {0}")]
    Fst(#[from] fst::Error),

    #[error("redb database error: {0}")]
    Db(String),

    #[error("invalid regex: {0}")]
    Regex(#[from] regex::Error),

    #[error("index not found at {0}; run `greplm index` first")]
    IndexMissing(PathBuf),

    #[error("corrupt index: {0}")]
    Corrupt(String),

    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }

    pub fn other(msg: impl Into<String>) -> Self {
        Error::Other(msg.into())
    }
}

macro_rules! from_redb {
    ($($t:ty),* $(,)?) => {
        $(
            impl From<$t> for Error {
                fn from(e: $t) -> Self {
                    Error::Db(e.to_string())
                }
            }
        )*
    };
}

from_redb!(
    redb::Error,
    redb::DatabaseError,
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError,
);
