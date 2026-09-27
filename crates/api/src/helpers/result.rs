use std::error::Error as StdError;
use std::{fmt, ops};

pub type ApiResult<T, E = ApiError> = std::result::Result<T, E>;

/// An API error containing the original error's display message, not its source chain.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
#[cfg_attr(feature = "metadata", derive(schemars::JsonSchema))]
pub struct ApiError(Message);

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "metadata", derive(schemars::JsonSchema))]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_conversion_preserves_the_message() {
        let error = ApiError::from(std::io::Error::other("failure"));
        assert_eq!(error, ApiError::msg("failure"));
        assert_eq!(error.to_string(), "failure");
        let boxed: Box<dyn StdError + Send + Sync> = error.into();
        assert_eq!(boxed.to_string(), "failure");
    }

    #[cfg(feature = "serde")]
    #[test]
    fn error_wire_format_is_transparent() {
        let error = ApiError::msg("failure");
        let value = serde_json::json!({ "error": "failure" });
        assert_eq!(serde_json::to_value(&error).unwrap(), value);
        assert_eq!(serde_json::from_value::<ApiError>(value).unwrap(), error);
        assert!(serde_json::from_value::<ApiError>(serde_json::json!({})).is_err());
        assert!(serde_json::from_value::<ApiError>(serde_json::json!({ "error": 1 })).is_err());
    }

    #[cfg(feature = "metadata")]
    #[test]
    fn error_schema_preserves_the_message_shape() {
        let schema = schemars::schema_for!(ApiError).to_value();
        let reference = schema["$ref"].as_str().unwrap();
        let message = schema
            .pointer(reference.strip_prefix('#').unwrap())
            .unwrap();
        assert_eq!(message["type"], "object");
        assert_eq!(message["properties"]["error"]["type"], "string");
        assert_eq!(message["required"], serde_json::json!(["error"]));
    }
}
