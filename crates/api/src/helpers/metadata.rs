use std::marker::PhantomData;

use schemars::{JsonSchema, Schema, SchemaGenerator};

pub(crate) trait ResponseSchema: Sized {
    type Body: JsonSchema;

    fn response_schema(self, generator: &mut SchemaGenerator) -> Schema {
        generator.subschema_for::<Self::Body>()
    }
}

// Autoref provides the ordinary-type fallback without overlapping the Result implementation.
impl<T: JsonSchema> ResponseSchema for &PhantomData<T> {
    type Body = T;
}

impl<T: JsonSchema, E> ResponseSchema for PhantomData<Result<T, E>> {
    type Body = T;
}

#[derive(Debug, Clone)]
pub struct Op {
    pub name: &'static str,
    pub config: Option<&'static str>,
    pub route: Option<Route>,
    pub request: fn(&mut SchemaGenerator) -> Schema,
    pub response: fn(&mut SchemaGenerator) -> Schema,
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
