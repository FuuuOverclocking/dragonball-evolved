macro_rules! define_schema {
    ($name:ident { $($entries:tt)* }) => {
        $crate::helpers::macros::define_schema!(@entries $name [] $($entries)*);
    };
    (@entries $name:ident [$($parsed:tt)*] $head:ident $($rest:tt)*) => {
        $crate::helpers::macros::define_schema!(
            @request $name [$($parsed)*] [] $head $($rest)*
        );
    };
    (@request $name:ident [$($parsed:tt)*]
        [$($prefix:tt)*] $head:ident :: $next:ident $($rest:tt)*) => {
        $crate::helpers::macros::define_schema!(
            @request $name [$($parsed)*] [$($prefix)* $head ::] $next $($rest)*
        );
    };
    (@request $name:ident [$($parsed:tt)*]
        [$($prefix:tt)*] $op:ident -> $reply:ty; $($rest:tt)*) => {
        $crate::helpers::macros::define_schema!(@entries $name
            [$($parsed)* ($op [$($prefix)* $op] -> $reply {})] $($rest)*);
    };
    (@request $name:ident [$($parsed:tt)*]
        [$($prefix:tt)*] $op:ident -> $reply:ty { $($properties:tt)* } $($rest:tt)*) => {
        $crate::helpers::macros::define_schema!(@entries $name
            [$($parsed)* ($op [$($prefix)* $op] -> $reply { $($properties)* })] $($rest)*);
    };
    (@entries $name:ident [$(($op:ident [$request:path] -> $reply:ty { $($properties:tt)* }))*]) => {
        pub enum $name<M: $crate::op_mode::OpMode> {
            $($op($request, M::Reply<$reply>),)*
        }

        impl ::core::clone::Clone for $name<$crate::op_mode::Command> {
            fn clone(&self) -> Self {
                match self {
                    $(Self::$op(parameters, ()) => Self::$op(parameters.clone(), ()),)*
                }
            }
        }

        impl<M: $crate::op_mode::OpMode> ::core::fmt::Debug for $name<M> {
            fn fmt(&self, formatter: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                match self {
                    $(Self::$op(parameters, _) => formatter.debug_tuple(stringify!($op))
                        .field(parameters).finish(),)*
                }
            }
        }

        #[cfg(feature = "serde")]
        impl ::serde::Serialize for $name<$crate::op_mode::Command> {
            fn serialize<S: ::serde::Serializer>(
                &self,
                serializer: S,
            ) -> ::core::result::Result<S::Ok, S::Error> {
                #[derive(::serde::Serialize)]
                #[serde(rename_all = "kebab-case")]
                enum Parameters<'a> {
                    $($op(&'a $request),)*
                }

                let parameters = match self {
                    $(Self::$op(parameters, ()) => Parameters::$op(parameters),)*
                };
                ::serde::Serialize::serialize(&parameters, serializer)
            }
        }

        #[cfg(feature = "serde")]
        impl<'de> ::serde::Deserialize<'de> for $name<$crate::op_mode::Command> {
            fn deserialize<D: ::serde::Deserializer<'de>>(
                deserializer: D,
            ) -> ::core::result::Result<Self, D::Error> {
                #[derive(::serde::Deserialize)]
                #[serde(rename_all = "kebab-case")]
                enum Parameters {
                    $($op($request),)*
                }

                let parameters = <Parameters as ::serde::Deserialize>::deserialize(deserializer)?;
                Ok(match parameters {
                    $(Parameters::$op(parameters) => Self::$op(parameters, ()),)*
                })
            }
        }

        pub mod metadata {
            #[cfg(feature = "serde")]
            use super::*;

            #[cfg(feature = "schema")]
            #[allow(unused)]
            use $crate::helpers::metadata::ResponseSchema as _;
            pub use $crate::helpers::metadata::{Op, Route};

            #[cfg(feature = "serde")]
            pub(crate) static CONFIG_BINDINGS: ::std::sync::LazyLock<
                $crate::helpers::config::ConfigBindings<$name<$crate::op_mode::Command>>,
            > = ::std::sync::LazyLock::new(|| {
                $crate::helpers::config::ConfigBindings::new([
                    $($crate::helpers::macros::define_schema!(
                        @binding $name $op [$request] { $($properties)* }
                    )),*
                ].into_iter().flatten()).expect("Failed making bindings for api config")
            });

            pub static OPERATIONS: ::std::sync::LazyLock<Vec<Op>> =
                ::std::sync::LazyLock::new(ops);

            fn ops() -> Vec<Op> {
                vec![$($crate::helpers::macros::define_schema!(
                    @metadata $op [$request] -> $reply { $($properties)* }
                )),*]
            }
        }
    };
    (@binding $name:ident $op:ident [$request:path] { config: $path:literal; $($rest:tt)* }) => {
        Some($crate::helpers::config::Binding {
            path: $path,
            parse: |value| ::serde_json::from_value::<$request>(value)
                .map(|parameters| $name::<$crate::op_mode::Command>::$op(parameters, ()))
                .map_err(::serde::de::Error::custom),
            #[cfg(feature = "schema")]
            schema: |generator| generator.subschema_for::<$request>(),
        })
    };
    (@binding $name:ident $op:ident [$request:path] { $($properties:tt)* }) => {
        None
    };
    (@metadata $op:ident [$request:path] -> $reply:ty {
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
            #[cfg(feature = "schema")]
            request: |generator| generator.subschema_for::<$request>(),
            #[cfg(feature = "schema")]
            response: |generator| ::core::marker::PhantomData::<$reply>.response_schema(generator),
        }
    };
}

pub(crate) use define_schema;

#[cfg(test)]
mod tests {
    #[cfg(feature = "schema")]
    use schemars::SchemaGenerator;
    #[cfg(feature = "serde")]
    use serde::{Deserialize, Serialize};
    #[cfg(feature = "serde")]
    use serde_json::json;

    use crate::op_mode;

    #[derive(Debug, Clone)]
    #[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
    #[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
    pub struct Plain {}

    mod requests {
        pub mod nested {
            pub use crate::block::AddDisk;
        }
    }

    define_schema! {
        TestOp {
            Plain -> ();
            requests::nested::AddDisk -> Result<(), String> {
                config: ".machine.disks[]";
                route: PUT /v1/disks;
            }
            crate::logger::UpdateLogger -> ();
        }
    }

    #[cfg(feature = "serde")]
    #[test]
    fn request_paths_preserve_variant_names_and_wire_format() {
        for (key, name) in [
            ("plain", "Plain"),
            ("add-disk", "AddDisk"),
            ("update-logger", "UpdateLogger"),
        ] {
            let command: TestOp<op_mode::Command> =
                serde_json::from_value(json!({ (key): {} })).unwrap();
            assert!(format!("{command:?}").starts_with(&format!("{name}(")));
            let serialized = serde_json::to_value(&command).unwrap();
            let roundtrip: TestOp<op_mode::Command> =
                serde_json::from_value(serialized.clone()).unwrap();
            assert_eq!(serde_json::to_value(roundtrip).unwrap(), serialized);
            assert_eq!(serialized.as_object().unwrap().len(), 1);
            assert!(serialized.get(key).is_some());
            assert_eq!(serde_json::to_value(command.clone()).unwrap(), serialized);
        }
    }

    #[cfg(feature = "serde")]
    #[test]
    fn config_bindings_are_reused_without_retaining_parse_state() {
        let first = ::std::sync::LazyLock::force(&metadata::CONFIG_BINDINGS);
        let second = ::std::sync::LazyLock::force(&metadata::CONFIG_BINDINGS);
        assert!(::std::ptr::eq(first, second));
        assert_eq!(
            first
                .parse(json!({ "machine": { "disks": [{}, {}] } }))
                .unwrap()
                .len(),
            2
        );
        assert!(second.parse(json!({})).unwrap().is_empty());
        assert_eq!(
            second
                .parse(json!({ "machine": { "disks": [{}] } }))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn request_paths_preserve_metadata_and_config_bindings() {
        let operations = &metadata::OPERATIONS;
        assert_eq!(
            operations.iter().map(|op| op.name).collect::<Vec<_>>(),
            ["Plain", "AddDisk", "UpdateLogger"]
        );
        let commands = [
            TestOp::<op_mode::Command>::Plain(Plain {}, ()),
            TestOp::AddDisk(Default::default(), ()),
            TestOp::UpdateLogger(Default::default(), ()),
        ];
        for (command, op) in commands.iter().zip(operations.iter()) {
            assert!(format!("{:?}", command.clone()).starts_with(op.name));
        }
        assert_eq!(operations[0].config, None);
        assert_eq!(operations[0].route, None);
        assert_eq!(operations[2].config, None);
        assert_eq!(operations[2].route, None);
        let disk: metadata::Op = operations[1].clone();
        assert_eq!(disk.config, Some(".machine.disks[]"));
        let route: metadata::Route = disk.route.unwrap();
        assert_eq!((route.method, route.path), ("PUT", "/v1/disks"));
        assert_eq!(metadata::Route::from(("PUT", "/v1/disks")), route);
        assert_eq!(<(&str, &str)>::from(route), ("PUT", "/v1/disks"));

        #[cfg(feature = "schema")]
        {
            let mut generator = SchemaGenerator::default();
            assert_eq!(
                (disk.request)(&mut generator),
                generator.subschema_for::<crate::block::AddDisk>()
            );
            assert_eq!(
                (disk.response)(&mut generator),
                generator.subschema_for::<()>()
            );

            let bindings = &metadata::CONFIG_BINDINGS;
            let commands = bindings
                .parse(json!({ "machine": { "disks": [{}] } }))
                .unwrap();
            assert!(matches!(commands.as_slice(), [TestOp::AddDisk(_, ())]));
            let schema = bindings.schema(&mut generator);
            assert_eq!(
                schema.as_value()["properties"]["machine"]["properties"]["disks"]["type"],
                "array"
            );
        }
    }
}
