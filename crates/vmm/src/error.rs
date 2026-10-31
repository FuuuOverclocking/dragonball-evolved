use std::error::Error;
use std::fmt::Write;

use api::ApiError;

/// Renders `error` and its source chain as `outer: inner: innermost`, for
/// `ApiError` replies and `VmmExitStatus::Error`.
pub(crate) fn render_chain(error: &dyn Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let _ = write!(message, ": {cause}");
        source = cause.source();
    }
    message
}

/// The reply for a request that failed with `error`, its source chain included.
pub(crate) fn api_error(error: &dyn Error) -> ApiError {
    ApiError::msg(render_chain(error))
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    /// outer
    #[derive(Debug, displaydoc::Display, thiserror::Error)]
    struct Outer(#[source] Inner);

    /// inner
    #[derive(Debug, displaydoc::Display, thiserror::Error)]
    struct Inner(#[source] io::Error);

    #[test]
    fn renders_every_source_in_order() {
        let error = Outer(Inner(io::Error::other("innermost")));
        assert_eq!(render_chain(&error), "outer: inner: innermost");
        assert_eq!(render_chain(&io::Error::other("alone")), "alone");
    }
}
