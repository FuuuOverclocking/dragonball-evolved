#[cfg(feature = "serde")]
use std::io::Read;
#[cfg(any(feature = "schema", not(feature = "serde")))]
use std::marker::PhantomData;

#[cfg(feature = "schema")]
use schemars::{JsonSchema, Schema, SchemaGenerator};
#[cfg(feature = "serde")]
use serde::de::DeserializeOwned;
#[cfg(feature = "serde")]
use serde_json::Value;

#[cfg(feature = "schema")]
pub(crate) trait ResponseSchema: Sized {
    type Body: JsonSchema;

    fn response_schema(self, generator: &mut SchemaGenerator) -> Schema {
        generator.subschema_for::<Self::Body>()
    }
}

// Autoref provides the ordinary-type fallback without overlapping the Result implementation.
#[cfg(feature = "schema")]
impl<T: JsonSchema> ResponseSchema for &PhantomData<T> {
    type Body = T;
}

#[cfg(feature = "schema")]
impl<T: JsonSchema> ResponseSchema for PhantomData<crate::ApiResult<T>> {
    type Body = T;
}

#[derive(Debug, Clone)]
pub struct Op<C> {
    pub name: &'static str,
    pub config: Option<&'static str>,
    pub route: Option<Route>,
    #[cfg(feature = "serde")]
    pub parse: for<'a> fn(ParseInput<'a>) -> Result<C, serde::de::value::Error>,
    #[cfg(not(feature = "serde"))]
    pub(crate) _command: PhantomData<fn() -> C>,
    #[cfg(feature = "schema")]
    pub request: fn(&mut SchemaGenerator) -> Schema,
    #[cfg(feature = "schema")]
    pub response: fn(&mut SchemaGenerator) -> Schema,
}

#[cfg(feature = "serde")]
pub enum ParseInput<'a> {
    Reader(Box<dyn Read + 'a>),
    Bytes(&'a [u8]),
    Str(&'a str),
    Value(Value),
}

#[cfg(feature = "serde")]
impl ParseInput<'_> {
    pub fn deserialize<T: DeserializeOwned>(self) -> serde_json::Result<T> {
        match self {
            Self::Reader(reader) => serde_json::from_reader(reader),
            Self::Bytes(bytes) => serde_json::from_slice(bytes),
            Self::Str(string) => serde_json::from_str(string),
            Self::Value(value) => serde_json::from_value(value),
        }
    }
}

#[cfg(feature = "serde")]
impl<'a> From<Box<dyn Read + 'a>> for ParseInput<'a> {
    fn from(reader: Box<dyn Read + 'a>) -> Self {
        Self::Reader(reader)
    }
}

#[cfg(feature = "serde")]
impl<'a, R: Read + 'a> From<Box<R>> for ParseInput<'a> {
    fn from(reader: Box<R>) -> Self {
        Self::Reader(reader)
    }
}

#[cfg(feature = "serde")]
impl<'a> From<&'a [u8]> for ParseInput<'a> {
    fn from(bytes: &'a [u8]) -> Self {
        Self::Bytes(bytes)
    }
}

#[cfg(feature = "serde")]
impl<'a> From<&'a str> for ParseInput<'a> {
    fn from(string: &'a str) -> Self {
        Self::Str(string)
    }
}

#[cfg(feature = "serde")]
impl From<Value> for ParseInput<'_> {
    fn from(value: Value) -> Self {
        Self::Value(value)
    }
}

/// One operation's HTTP binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    /// The method as declared, e.g. `PUT`.
    pub method: &'static str,
    /// The path rebuilt from its tokens without spacing artifacts, e.g.
    /// `/v1/block-devices`.
    pub path: &'static str,
}

impl From<(&'static str, &'static str)> for Route {
    fn from(value: (&'static str, &'static str)) -> Self {
        Self {
            method: value.0,
            path: value.1,
        }
    }
}

impl From<Route> for (&'static str, &'static str) {
    fn from(value: Route) -> Self {
        (value.method, value.path)
    }
}

pub(crate) struct ConfigPath {
    pub path: &'static str,
    #[cfg(feature = "serde")]
    pub many: bool,
}

impl ConfigPath {
    pub const fn new(path: &'static str) -> Self {
        let bytes = path.as_bytes();
        assert!(
            !bytes.is_empty() && bytes[0] == b'.',
            "config binding must start with '.'"
        );
        let many =
            bytes.len() >= 2 && bytes[bytes.len() - 2] == b'[' && bytes[bytes.len() - 1] == b']';
        let end = bytes.len() - if many { 2 } else { 0 };
        let path = path.split_at(end).0;
        assert!(
            !path_prefix(".dragonball", path)
                && !path_prefix(".extends", path)
                && !path_prefix(".$schema", path),
            "config binding conflicts with a reserved root field"
        );
        validate_segments(path, b'.');
        Self {
            path,
            #[cfg(feature = "serde")]
            many,
        }
    }
}

pub(crate) const fn str_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    let mut index = 0;
    while index < left.len() {
        if left[index] != right[index] {
            return false;
        }
        index += 1;
    }
    true
}

pub(crate) const fn path_prefix(prefix: &str, path: &str) -> bool {
    if prefix.len() > path.len() {
        return false;
    }
    str_eq(prefix, path.split_at(prefix.len()).0)
        && (prefix.len() == path.len() || path.as_bytes()[prefix.len()] == b'.')
}

const fn validate_segments(path: &str, separator: u8) {
    let bytes = path.as_bytes();
    assert!(
        bytes.len() > 1 && bytes[0] == separator,
        "invalid binding path"
    );
    let mut empty = true;
    let mut index = 1;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == separator {
            assert!(!empty, "empty binding path segment");
            empty = true;
        } else {
            assert!(
                byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-',
                "invalid binding path character"
            );
            empty = false;
        }
        index += 1;
    }
    assert!(!empty, "empty binding path segment");
}

pub(crate) const fn validate<C>(operations: &[Op<C>]) {
    let mut index = 0;
    while index < operations.len() {
        let op = &operations[index];
        if let Some(path) = op.config {
            let path = ConfigPath::new(path);
            let mut previous = 0;
            while previous < index {
                if let Some(other) = operations[previous].config {
                    let other = ConfigPath::new(other);
                    assert!(
                        !path_prefix(path.path, other.path) && !path_prefix(other.path, path.path),
                        "duplicate or overlapping config binding"
                    );
                }
                previous += 1;
            }
        }
        if let Some(route) = op.route {
            assert!(
                str_eq(route.method, "GET")
                    || str_eq(route.method, "PUT")
                    || str_eq(route.method, "POST")
                    || str_eq(route.method, "DELETE")
                    || str_eq(route.method, "OPTIONS")
                    || str_eq(route.method, "HEAD")
                    || str_eq(route.method, "PATCH")
                    || str_eq(route.method, "TRACE"),
                "unsupported HTTP method; use an uppercase standard method"
            );
            validate_segments(route.path, b'/');
            let mut previous = 0;
            while previous < index {
                if let Some(other) = operations[previous].route {
                    assert!(
                        !str_eq(route.method, other.method) || !str_eq(route.path, other.path),
                        "duplicate HTTP route"
                    );
                }
                previous += 1;
            }
        }
        index += 1;
    }
}
