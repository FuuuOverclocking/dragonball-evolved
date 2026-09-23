macro_rules! define_schema {
    ($name:ident { $($entries:tt)* }) => {
        $crate::helpers::macros::define_schema!(@entries $name [] $($entries)*);
    };
    (@entries $name:ident [$($parsed:tt)*] $op:ident -> $reply:ty; $($rest:tt)*) => {
        $crate::helpers::macros::define_schema!(@entries $name
            [$($parsed)* ($op -> $reply {})] $($rest)*);
    };
    (@entries $name:ident [$($parsed:tt)*]
        $op:ident -> $reply:ty { $($properties:tt)* } $($rest:tt)*) => {
        $crate::helpers::macros::define_schema!(@entries $name
            [$($parsed)* ($op -> $reply { $($properties)* })] $($rest)*);
    };
    (@entries $name:ident [$(($op:ident -> $reply:ty { $($properties:tt)* }))*]) => {
        pub enum $name<M: $crate::op_mode::OpMode> {
            $($op($op, M::Reply<$reply>),)*
        }

        impl<M: $crate::op_mode::OpMode> ::core::fmt::Debug for $name<M> {
            fn fmt(&self, formatter: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                match self {
                    $(Self::$op(parameters, _) => formatter.debug_tuple(stringify!($op))
                        .field(parameters).finish(),)*
                }
            }
        }

        impl ::serde::Serialize for $name<$crate::op_mode::Command> {
            fn serialize<S: ::serde::Serializer>(
                &self,
                serializer: S,
            ) -> ::core::result::Result<S::Ok, S::Error> {
                #[derive(::serde::Serialize)]
                #[serde(rename_all = "kebab-case")]
                enum Parameters<'a> {
                    $($op(&'a $op),)*
                }

                let parameters = match self {
                    $(Self::$op(parameters, ()) => Parameters::$op(parameters),)*
                };
                ::serde::Serialize::serialize(&parameters, serializer)
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name<$crate::op_mode::Command> {
            fn deserialize<D: ::serde::Deserializer<'de>>(
                deserializer: D,
            ) -> ::core::result::Result<Self, D::Error> {
                #[derive(::serde::Deserialize)]
                #[serde(rename_all = "kebab-case")]
                enum Parameters {
                    $($op($op),)*
                }

                let parameters = <Parameters as ::serde::Deserialize>::deserialize(deserializer)?;
                Ok(match parameters {
                    $(Parameters::$op(parameters) => Self::$op(parameters, ()),)*
                })
            }
        }

        pub mod config {
            use super::*;

            static BINDINGS: ::std::sync::LazyLock<
                $crate::helpers::config::Config<$name<$crate::op_mode::Command>>,
            > = ::std::sync::LazyLock::new(|| bindings().expect("Failed making bindings for api config"));

            fn bindings() -> ::anyhow::Result<
                $crate::helpers::config::Config<$name<$crate::op_mode::Command>>,
            > {
                $crate::helpers::config::Config::new([
                    $($crate::helpers::macros::define_schema!(
                        @binding $name $op { $($properties)* }
                    )),*
                ].into_iter().flatten())
            }

            /// Parse merged operation configuration, with process settings already removed.
            pub fn parse(value: ::serde_json::Value) -> ::anyhow::Result<
                Vec<$name<$crate::op_mode::Command>>,
            > {
                BINDINGS.parse(value)
            }

            pub fn schema(generator: &mut ::schemars::SchemaGenerator) -> ::schemars::Schema {
                BINDINGS.schema(generator)
            }
        }

        pub mod metadata {
            use super::*;

            #[allow(unused)]
            use $crate::helpers::metadata::ResponseSchema as _;
            pub use $crate::helpers::metadata::{Op, Route};

            pub static METADATA: ::std::sync::LazyLock<Vec<Op>> =
                ::std::sync::LazyLock::new(ops);

            fn ops() -> Vec<Op> {
                vec![$($crate::helpers::macros::define_schema!(
                    @metadata $op -> $reply { $($properties)* }
                )),*]
            }
        }
    };
    (@binding $name:ident $op:ident { config: $path:literal; $($rest:tt)* }) => {
        Some($crate::helpers::config::Binding {
            path: $path,
            parse: |value| ::serde_json::from_value::<$op>(value)
                .map(|parameters| $name::<$crate::op_mode::Command>::$op(parameters, ()))
                .map_err(::anyhow::Error::from),
            schema: |generator| generator.subschema_for::<$op>(),
        })
    };
    (@binding $name:ident $op:ident { $($properties:tt)* }) => {
        None
    };
    (@metadata $op:ident -> $reply:ty {
        $(config: $config:literal;)?
        $(route: $method:ident / $($segment:ident $(- $suffix:ident)*)/+;)?
        $(cli: $cli:path;)?
    }) => {
        $crate::helpers::metadata::Op {
            name: stringify!($op),
            config: None $(.or(Some($config)))?,
            route: None $(.or(Some($crate::helpers::metadata::Route {
                method: stringify!($method),
                path: concat!($("/", stringify!($segment), $("-", stringify!($suffix),)*)+),
            })))?,
            request: |generator| generator.subschema_for::<$op>(),
            response: |generator| ::core::marker::PhantomData::<$reply>.response_schema(generator),
        }
    };
}

pub(crate) use define_schema;
