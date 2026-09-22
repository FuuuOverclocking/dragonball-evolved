use std::error::Error as StdError;
use std::{fmt, ops};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub type ApiResult<T, E = ApiError> = std::result::Result<T, E>;

/// An API error containing the original error's display message, not its source chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct ApiError(Message);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
struct Message {
    error: String,
}

impl ApiError {
    pub fn msg(message: impl Into<String>) -> Self {
        Self(Message {
            error: message.into(),
        })
    }
}

impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.error)
    }
}

// The outer Error must not implement StdError: From<E> would overlap From<T> for T.
impl StdError for Message {}

impl AsRef<dyn StdError + Sync + Send> for ApiError {
    fn as_ref(&self) -> &(dyn StdError + Sync + Send + 'static) {
        &self.0
    }
}

impl AsRef<dyn StdError> for ApiError {
    fn as_ref(&self) -> &(dyn StdError + 'static) {
        &self.0
    }
}

impl ops::Deref for ApiError {
    type Target = dyn StdError + Sync + Send;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl ops::DerefMut for ApiError {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<E: StdError> From<E> for ApiError {
    fn from(error: E) -> Self {
        Self::msg(error.to_string())
    }
}

impl From<ApiError> for Box<dyn StdError + Send + Sync + 'static> {
    fn from(error: ApiError) -> Self {
        Box::new(error.0)
    }
}

impl From<ApiError> for Box<dyn StdError + Send + 'static> {
    fn from(error: ApiError) -> Self {
        Box::new(error.0)
    }
}

impl From<ApiError> for Box<dyn StdError + 'static> {
    fn from(error: ApiError) -> Self {
        Box::new(error.0)
    }
}
