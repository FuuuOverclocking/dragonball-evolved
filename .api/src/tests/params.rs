//! The parameter types of the test registry, handwritten here the way the
//! production ones live in modules of their own.

use serde::{Deserialize, Serialize};

/// Two numbers to add.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Add {
    /// The left-hand operand.
    pub lhs: i64,
    /// The right-hand operand; a negative one is refused.
    pub rhs: i64,
}

/// Text to hand back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Echo {
    /// The text itself.
    pub text: String,
}

/// An operation whose reply JSON cannot hold takes no parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Opaque {}

/// An operation with nothing to say takes no parameters either.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Noop {}

/// Whom to greet, on the wire's own terms: the registry has to carry this type
/// through with its Serde rename and default intact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Greet {
    /// `name` in Rust, `who` on the wire, and `world` when the wire omits it.
    #[serde(rename = "who", default = "world")]
    pub name: String,
}

fn world() -> String {
    "world".into()
}
