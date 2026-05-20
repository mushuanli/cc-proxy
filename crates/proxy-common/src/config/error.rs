/// Error type for config operations.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("toml deserialize error: {0}")]
    TomlDeserialize(#[from] toml::de::Error),

    #[error("toml serialize error: {0}")]
    TomlSerialize(#[from] toml::ser::Error),

    #[error("toml_edit error: {0}")]
    TomlEdit(#[from] toml_edit::TomlError),

    #[error("config validation failed: {0}")]
    Validation(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("duplicate entry: {0}")]
    Duplicate(String),
}

pub type ConfigResult<T> = Result<T, ConfigError>;
