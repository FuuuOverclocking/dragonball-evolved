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
        [$($prefix:tt)*] $op:ident -> $($rest:tt)*) => {
        $crate::helpers::macros::define_schema!(
            @reply $name [$($parsed)*] $op [$($prefix)* $op] [] $($rest)*
        );
    };
    (@reply $name:ident [$($parsed:tt)*] $op:ident [$request:path]
        [$($reply:tt)+] ; $($rest:tt)*) => {
        $crate::helpers::macros::define_schema!(@entries $name
            [$($parsed)* ($op [$request] -> [$($reply)+] {})] $($rest)*);
    };
    (@reply $name:ident [$($parsed:tt)*] $op:ident [$request:path]
        [$($reply:tt)+] { $($properties:tt)* } $($rest:tt)*) => {
        $crate::helpers::macros::define_schema!(@entries $name
            [$($parsed)* ($op [$request] -> [$($reply)+] { $($properties)* })] $($rest)*);
    };
    (@reply $name:ident [$($parsed:tt)*] $op:ident [$request:path]
        [$($reply:tt)*] $next:tt $($rest:tt)*) => {
        $crate::helpers::macros::define_schema!(
            @reply $name [$($parsed)*] $op [$request] [$($reply)* $next] $($rest)*
        );
    };
    (@entries $name:ident [$(($op:ident [$request:path] -> [$($reply:tt)+] { $($properties:tt)* }))*]) => {
        pub enum $name<M: $crate::op_mode::OpMode> {
            $($op($request, M::Reply<$($reply)+>),)*
        }

        $(impl $crate::RawVmmOp for $request {
            type Response = $($reply)+;

            #[cfg(feature = "request")]
            fn into_vmm_request(self) -> (
                $name<$crate::op_mode::Request>,
                ::oneshot::Receiver<Self::Response>,
            ) {
                let (sender, receiver) = ::oneshot::channel();
                ($name::$op(self, sender.into()), receiver)
            }

            fn into_vmm_command(self) -> $name<$crate::op_mode::Command> {
                $name::$op(self, ())
            }
        })*

        $($crate::helpers::macros::define_schema!(
            @request_fds [$request] ; $($properties)*
        );)*

        $crate::helpers::macros::define_schema!(
            @response_fds [] $(([$($reply)+] { $($properties)* }))*
        );

        impl<M: $crate::op_mode::OpMode> $crate::helpers::fd::MergeFd for $name<M> {
            fn merge_fd(&mut self, fds: &mut [Option<::std::os::fd::OwnedFd>]) -> $crate::ApiResult<()> {
                match self {
                    $(Self::$op(parameters, _) => $crate::helpers::fd::MergeFd::merge_fd(parameters, fds),)*
                }
            }
        }

        #[cfg(all(feature = "request", feature = "serde"))]
        impl $name<$crate::op_mode::Command> {
            pub fn into_request(self) -> ($name<$crate::op_mode::Request>, $crate::helpers::SerializedReplyWaiter) {
                use core::marker::PhantomData;
                use $crate::helpers::reply::{SerializeReply as _, into_request};

                match self {
                    $(Self::$op(parameters, ()) => into_request(
                        parameters,
                        $name::$op,
                        |reply| PhantomData::<$($reply)+>.serialize_reply(reply),
                    ),)*
                }
            }
        }

        // impl ::core::clone::Clone for $name<$crate::op_mode::Command> {
        //     fn clone(&self) -> Self {
        //         match self {
        //             $(Self::$op(parameters, ()) => Self::$op(parameters.clone(), ()),)*
        //         }
        //     }
        // }

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
            #![allow(unused)]

            use core::marker::PhantomData;

            use super::*;
            #[cfg(feature = "serde")]
            use crate::helpers::config;
            #[cfg(feature = "schema")]
            use crate::helpers::metadata::ResponseSchema as _;
            pub use crate::helpers::metadata::Route;
            use crate::helpers::{metadata as meta, op_mode};

            pub type Op = meta::Op<super::$name<$crate::op_mode::Command>>;

            pub static OPERATIONS: &[Op] = &[
                $($crate::helpers::macros::define_schema!(
                    @metadata $name $op [$request] -> $($reply)+ { $($properties)* }
                )),*
            ];

            const _: () = meta::validate(OPERATIONS);

            #[cfg(feature = "serde")]
            pub(crate) static CONFIG_BINDINGS: config::ConfigBindings<
                $name<op_mode::Command>,
                { config::node_count(OPERATIONS) },
            > = config::ConfigBindings::new(OPERATIONS);
        }
    };
    (@metadata $name:ident $op:ident [$request:path] -> $reply:ty {
        $(config: $config:literal;)?
        $(route: $method:ident $(/ $($segment:ident $(- $suffix:ident)*)?)+;)?
        $(cli: $cli:path;)?
        $(fd_in: $($fd_side:ident),+;)?
    }) => {{
        $($($crate::helpers::macros::define_schema!(@fd_side $fd_side);)+)?
        Op {
            name: stringify!($op),
            config: $crate::helpers::macros::define_schema!(@option $($config)?),
            route: $crate::helpers::macros::define_schema!(@option $(
                Route {
                    method: stringify!($method),
                    path: concat!($("/", $(stringify!($segment), $("-", stringify!($suffix),)*)?)+),
                }
            )?),
            #[cfg(feature = "serde")]
            parse: |input| input.deserialize::<$request>()
                .map(|parameters| $name::<op_mode::Command>::$op(parameters, ()))
                .map_err(serde::de::Error::custom),
            #[cfg(not(feature = "serde"))]
            _command: PhantomData,
            #[cfg(feature = "schema")]
            request: |generator| generator.subschema_for::<$request>(),
            #[cfg(feature = "schema")]
            response: |generator| PhantomData::<$reply>.response_schema(generator),
        }
    }};
    (@fd_side req) => {};
    (@fd_side resp) => {};
    (@fd_side $other:ident) => {
        compile_error!("fd_in accepts req and/or resp");
    };
    (@request_fds [$request:path] ; fd_in: $($side:ident),+; $($rest:tt)*) => {
        $crate::helpers::macros::define_schema!(@request_fd_sides [$request] ; $($side),+);
    };
    (@request_fds [$request:path] ; $head:tt $($rest:tt)*) => {
        $crate::helpers::macros::define_schema!(@request_fds [$request] ; $($rest)*);
    };
    (@request_fds [$request:path] ;) => {
        $crate::helpers::macros::define_schema!(@request_fd_sides [$request] ;);
    };
    (@request_fd_sides [$request:path] ; req $(, $rest:ident)*) => {};
    (@request_fd_sides [$request:path] ; resp $(, $rest:ident)*) => {
        $crate::helpers::macros::define_schema!(@request_fd_sides [$request] ; $($rest),*);
    };
    (@request_fd_sides [$request:path] ;) => {
        impl $crate::helpers::fd::MergeFd for $request {}
    };
    (@response_fds [$($seen:tt)*]) => {};
    (@response_fds [$($seen:tt)*] ([$($reply:tt)+] { $($properties:tt)* }) $($rest:tt)*) => {
        $crate::helpers::macros::define_schema!(
            @response_properties [$($seen)*] [$($rest)*] [$($reply)+] ; $($properties)*
        );
    };
    (@response_properties $seen:tt $rest:tt $reply:tt ; fd_in: $($side:ident),+; $($properties:tt)*) => {
        $crate::helpers::macros::define_schema!(@response_sides $seen $rest $reply ; $($side),+);
    };
    (@response_properties $seen:tt $rest:tt $reply:tt ; $head:tt $($properties:tt)*) => {
        $crate::helpers::macros::define_schema!(@response_properties $seen $rest $reply ; $($properties)*);
    };
    (@response_properties $seen:tt $rest:tt $reply:tt ;) => {
        $crate::helpers::macros::define_schema!(@response_sides $seen $rest $reply ;);
    };
    (@response_sides $seen:tt [$($rest:tt)*] $reply:tt ; resp $(, $side:ident)*) => {
        $crate::helpers::macros::define_schema!(@response_fds $seen $($rest)*);
    };
    (@response_sides $seen:tt $rest:tt $reply:tt ; req $(, $side:ident)*) => {
        $crate::helpers::macros::define_schema!(@response_sides $seen $rest $reply ; $($side),*);
    };
    (@response_sides $seen:tt $rest:tt [$($reply:tt)+] ;) => {
        $crate::helpers::macros::define_schema!(@response_body $seen $rest [] $($reply)+);
    };
    (@response_body $seen:tt [$($rest:tt)*] [] ()) => {
        $crate::helpers::macros::define_schema!(@response_fds $seen $($rest)*);
    };
    (@response_body $seen:tt $rest:tt [$($prefix:tt)*] $head:ident :: $($tail:tt)+) => {
        $crate::helpers::macros::define_schema!(@response_body $seen $rest [$($prefix)* $head ::] $($tail)+);
    };
    (@response_body $seen:tt $rest:tt $prefix:tt ApiResult < $($tail:tt)+) => {
        $crate::helpers::macros::define_schema!(@response_inner $seen $rest [] [] $($tail)+);
    };
    (@response_body $seen:tt $rest:tt $prefix:tt Result < $($tail:tt)+) => {
        $crate::helpers::macros::define_schema!(@response_inner $seen $rest [] [] $($tail)+);
    };
    (@response_body $seen:tt $rest:tt [$($prefix:tt)*] $($reply:tt)+) => {
        $crate::helpers::macros::define_schema!(@response_unique $seen $rest [$($prefix)* $($reply)+] $seen);
    };
    (@response_inner $seen:tt $rest:tt [$($body:tt)+] [] >) => {
        $crate::helpers::macros::define_schema!(@response_body $seen $rest [] $($body)+);
    };
    (@response_inner $seen:tt $rest:tt [$($body:tt)+] [] , $error:ty >) => {
        $crate::helpers::macros::define_schema!(@response_body $seen $rest [] $($body)+);
    };
    (@response_inner $seen:tt $rest:tt [$($body:tt)*] [$($depth:tt)*] < $($tail:tt)+) => {
        $crate::helpers::macros::define_schema!(@response_inner $seen $rest [$($body)* <] [() $($depth)*] $($tail)+);
    };
    (@response_inner $seen:tt $rest:tt $body:tt [() $($depth:tt)*] >> $($tail:tt)*) => {
        $crate::helpers::macros::define_schema!(@response_inner $seen $rest $body [() $($depth)*] > > $($tail)*);
    };
    (@response_inner $seen:tt $rest:tt [$($body:tt)*] [() $($depth:tt)*] > $($tail:tt)+) => {
        $crate::helpers::macros::define_schema!(@response_inner $seen $rest [$($body)* >] [$($depth)*] $($tail)+);
    };
    (@response_inner $seen:tt $rest:tt [$($body:tt)*] $depth:tt $head:tt $($tail:tt)+) => {
        $crate::helpers::macros::define_schema!(@response_inner $seen $rest [$($body)* $head] $depth $($tail)+);
    };
    (@response_unique [$($seen:tt)*] [$($rest:tt)*] [$($reply:tt)+] []) => {
        impl $crate::helpers::fd::SplitFd for $($reply)+ {}
        $crate::helpers::macros::define_schema!(@response_fds [$($seen)* [$($reply)+]] $($rest)*);
    };
    (@response_unique $seen:tt [$($rest:tt)*] [$($reply:tt)+] [$first:tt $($remaining:tt)*]) => {
        const _: () = {
            macro_rules! response_seen {
                ([$($reply)+]) => {
                    $crate::helpers::macros::define_schema!(@response_fds $seen $($rest)*);
                };
                ($other:tt) => {
                    $crate::helpers::macros::define_schema!(
                        @response_unique $seen [$($rest)*] [$($reply)+] [$($remaining)*]
                    );
                };
            }
            response_seen!($first);
        };
    };
    (@option) => { None };
    (@option $value:expr) => { Some($value) };
}

pub(crate) use define_schema;

#[cfg(test)]
mod tests {
    #[cfg(feature = "schema")]
    use schemars::SchemaGenerator;
    #[cfg(feature = "serde")]
    use serde_json::json;

    use crate::{RawVmmOp, VmmCommand, metadata};

    fn operation(name: &str) -> &'static metadata::Op {
        metadata::OPERATIONS
            .iter()
            .find(|op| op.name == name)
            .unwrap_or_else(|| panic!("missing operation {name}"))
    }

    #[test]
    fn route_paths_preserve_segments_and_trailing_slashes() {
        #[cfg(any(feature = "schema", not(feature = "serde")))]
        use core::marker::PhantomData;

        #[cfg(feature = "schema")]
        use crate::helpers::metadata::ResponseSchema as _;
        use crate::metadata::{Op, Route};
        #[cfg(feature = "serde")]
        use crate::{VmmOp, op_mode};

        const OPERATIONS: &[Op] = &[
            define_schema!(@metadata VmmOp AddDisk [crate::block::AddDisk] -> crate::ApiResult<()> {
                route: GET /;
            }),
            define_schema!(@metadata VmmOp AddDisk [crate::block::AddDisk] -> crate::ApiResult<()> {
                route: GET /v1;
            }),
            define_schema!(@metadata VmmOp AddDisk [crate::block::AddDisk] -> crate::ApiResult<()> {
                route: GET /v1/;
            }),
            define_schema!(@metadata VmmOp AddDisk [crate::block::AddDisk] -> crate::ApiResult<()> {
                route: GET /v1/block-devices;
            }),
            define_schema!(@metadata VmmOp AddDisk [crate::block::AddDisk] -> crate::ApiResult<()> {
                route: GET /v1/block-devices/;
            }),
        ];
        const _: () = crate::helpers::metadata::validate(OPERATIONS);

        for (op, path) in OPERATIONS.iter().zip([
            "/",
            "/v1",
            "/v1/",
            "/v1/block-devices",
            "/v1/block-devices/",
        ]) {
            assert_eq!(op.route, Some(("GET", path).into()));
        }
        for path in ["", "v1", "//", "/v1//", "/v1//disks", "/v1/disks//"] {
            let mut op = define_schema!(@metadata VmmOp AddDisk [crate::block::AddDisk] -> crate::ApiResult<()> {
                route: GET /;
            });
            op.route = Some(("GET", path).into());
            assert!(
                std::panic::catch_unwind(|| crate::helpers::metadata::validate(&[op])).is_err()
            );
        }
    }

    #[test]
    fn instance_info_preserves_unit_request_and_plain_response() {
        let op = operation("GetInstanceInfo");
        assert_eq!(op.route, Some(("GET", "/v1/").into()));
        assert_eq!(op.config, None);
        assert!(matches!(
            crate::instance_info::GetInstanceInfo.into_vmm_command(),
            VmmCommand::GetInstanceInfo(_, ())
        ));

        #[cfg(feature = "serde")]
        {
            let command = (op.parse)(json!(null).into()).unwrap();
            assert!(matches!(command, VmmCommand::GetInstanceInfo(_, ())));
            let serialized = serde_json::to_value(command).unwrap();
            assert_eq!(serialized, json!({ "get-instance-info": null }));
            assert!(matches!(
                serde_json::from_value::<VmmCommand>(serialized).unwrap(),
                VmmCommand::GetInstanceInfo(_, ())
            ));
            assert!((op.parse)(json!({ "unknown": true }).into()).is_err());
        }
        #[cfg(feature = "request")]
        {
            let (request, receiver) = crate::instance_info::GetInstanceInfo.into_vmm_request();
            let crate::VmmRequest::GetInstanceInfo(_, reply) = request else {
                panic!("expected GetInstanceInfo");
            };
            let info = crate::instance_info::InstanceInfo {
                id: "test-vm".into(),
                version: "1.0".into(),
                state: crate::instance_info::VmState::Running,
            };
            reply.send(info.clone());
            assert_eq!(receiver.recv().unwrap(), info);
        }
        #[cfg(feature = "schema")]
        {
            let mut generator = SchemaGenerator::default();
            assert_eq!(
                (op.request)(&mut generator),
                generator.subschema_for::<crate::instance_info::GetInstanceInfo>()
            );
            assert_eq!(
                (op.response)(&mut generator),
                generator.subschema_for::<crate::instance_info::InstanceInfo>()
            );
        }
    }

    #[test]
    fn request_fd_declarations_generate_only_merge_and_preserve_custom_impls() {
        use std::os::fd::{AsRawFd, OwnedFd};
        use std::os::unix::net::UnixStream;

        use crate::helpers::fd::{Fds, MergeFd, SplitFd};
        use crate::{ApiError, ApiResult};

        struct Plain;
        define_schema!(@request_fds [Plain] ; fd_in: resp;);
        impl SplitFd for Plain {}

        struct Custom(Option<OwnedFd>);
        impl MergeFd for Custom {
            fn merge_fd(&mut self, fds: &mut [Option<OwnedFd>]) -> ApiResult<()> {
                self.0 = Some(
                    fds.first_mut()
                        .and_then(Option::take)
                        .ok_or_else(|| ApiError::msg("missing request fd"))?,
                );
                Ok(())
            }
        }
        define_schema!(@request_fds [Custom] ; fd_in: req;);
        define_schema!(@request_fds [Custom] ; fd_in: req, resp;);
        define_schema!(@request_fds [Custom] ; fd_in: resp, req;);

        let (socket, _peer) = UnixStream::pair().unwrap();
        let raw_fd = socket.as_raw_fd();
        let mut slots = vec![Some(socket.into())];
        Plain.merge_fd(&mut slots).unwrap();
        let mut command = crate::live_upgrade::HandoverState {}.into_vmm_command();
        command.merge_fd(&mut slots).unwrap();
        assert!(slots[0].is_some());

        let mut custom = Custom(None);
        assert!(custom.merge_fd(&mut []).is_err());
        custom.merge_fd(&mut slots).unwrap();
        assert_eq!(custom.0.as_ref().unwrap().as_raw_fd(), raw_fd);
        assert!(slots[0].is_none());
        let fds: Fds = Plain.split_fd().unwrap();
        assert!(fds.is_empty());
    }

    #[test]
    fn response_fd_declarations_generate_split_once_for_shared_bodies() {
        use core::marker::PhantomData;

        use crate::helpers::fd::{MergeFd, SplitFd};

        struct Shared;
        struct Other;
        struct Generic<T>(PhantomData<T>);
        define_schema!(@response_fds []
            ([Shared] {})
            ([Other] { fd_in: req; })
            ([Shared] {})
            ([crate::ApiResult<Shared>] {})
            ([std::result::Result<Other, String>] {})
            ([()] {})
            ([crate::ApiResult<()>] {})
            ([ApiResult<()>] {})
            ([crate::ApiResult<Generic<(u8, u16)>, String>] {})
            ([Generic<(u8, u16)>] {})
            ([ApiResult<Result<Shared, String>>] {})
        );
        impl MergeFd for Shared {}
        assert!(Shared.split_fd().unwrap().is_empty());
        assert!(Other.split_fd().unwrap().is_empty());
        assert!(
            Generic::<(u8, u16)>(PhantomData)
                .split_fd()
                .unwrap()
                .is_empty()
        );
        let mut wrapped: crate::ApiResult<Shared> = Ok(Shared);
        assert!(wrapped.split_fd().unwrap().is_empty());
        assert!(
            crate::instance_info::InstanceInfo::default()
                .split_fd()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn response_fd_declarations_preserve_custom_split_impls() {
        use std::os::fd::{AsRawFd, OwnedFd};
        use std::os::unix::net::UnixStream;

        use crate::helpers::fd::{Fds, SplitFd};
        use crate::{ApiError, ApiResult};

        struct Custom(Option<OwnedFd>);
        impl SplitFd for Custom {
            fn split_fd(&mut self) -> ApiResult<Fds> {
                Ok(vec![
                    self.0
                        .take()
                        .ok_or_else(|| ApiError::msg("missing response fd"))?,
                ])
            }
        }
        define_schema!(@response_fds []
            ([Custom] { fd_in: resp; })
            ([ApiResult<Custom>] { fd_in: req, resp; })
            ([Custom] { fd_in: resp, req; })
        );
        let (socket, _peer) = UnixStream::pair().unwrap();
        let raw_fd = socket.as_raw_fd();
        let mut response: ApiResult<Custom> = Ok(Custom(Some(socket.into())));
        let fds = response.split_fd().unwrap();
        assert_eq!(fds.len(), 1);
        assert_eq!(fds[0].as_raw_fd(), raw_fd);
        assert_eq!(
            response.split_fd().unwrap_err(),
            ApiError::msg("missing response fd")
        );
    }

    #[test]
    fn raw_ops_preserve_command_parameters() {
        let disk = crate::block::AddDisk {};
        let command: VmmCommand = disk.clone().into_vmm_command();
        assert!(matches!(command, VmmCommand::AddDisk(parameters, ()) if parameters == disk));

        let logger = crate::logger::UpdateLogger {
            level: Some(crate::logger::LevelFilter::Warn),
            show_tid: Some(false),
            ..Default::default()
        };
        let command: VmmCommand = logger.clone().into_vmm_command();
        assert!(matches!(
            command,
            VmmCommand::UpdateLogger(parameters, ()) if parameters == logger
        ));
    }

    #[cfg(feature = "request")]
    #[test]
    fn raw_ops_deliver_typed_replies() {
        let disk = crate::block::AddDisk {};
        let (request, receiver) = disk.clone().into_vmm_request();
        let crate::VmmRequest::AddDisk(parameters, reply) = request else {
            panic!("expected AddDisk");
        };
        assert_eq!(parameters, disk);
        reply.send(Ok(()));
        assert_eq!(receiver.recv().unwrap(), Ok(()));

        let logger = crate::logger::UpdateLogger {
            level: Some(crate::logger::LevelFilter::Debug),
            ..Default::default()
        };
        let (request, receiver) = logger.clone().into_vmm_request();
        let crate::VmmRequest::UpdateLogger(parameters, reply) = request else {
            panic!("expected UpdateLogger");
        };
        assert_eq!(parameters, logger);
        let error = crate::ApiError::msg("logger failed");
        reply.send(Err(error.clone()));
        assert_eq!(receiver.recv().unwrap(), Err(error));
    }

    #[cfg(feature = "request")]
    #[test]
    fn raw_request_tolerates_a_dropped_receiver() {
        let (request, receiver) = crate::block::AddDisk {}.into_vmm_request();
        drop(receiver);
        let crate::VmmRequest::AddDisk(_, reply) = request else {
            panic!("expected AddDisk");
        };
        reply.send(Ok(()));
    }

    #[cfg(feature = "serde")]
    #[test]
    fn request_paths_preserve_variant_names_and_wire_format() {
        for (key, name) in [("update-logger", "UpdateLogger"), ("add-disk", "AddDisk")] {
            let command: VmmCommand = serde_json::from_value(json!({ (key): {} })).unwrap();
            assert!(format!("{command:?}").starts_with(&format!("{name}(")));
            let serialized = serde_json::to_value(&command).unwrap();
            let roundtrip: VmmCommand = serde_json::from_value(serialized.clone()).unwrap();
            assert_eq!(serde_json::to_value(roundtrip).unwrap(), serialized);
            assert_eq!(serialized.as_object().unwrap().len(), 1);
            assert!(serialized.get(key).is_some());
            assert_eq!(serde_json::to_value(command).unwrap(), serialized);
        }
    }

    #[cfg(feature = "serde")]
    #[test]
    fn config_bindings_are_reused_without_retaining_parse_state() {
        let first = &metadata::CONFIG_BINDINGS;
        let second = &metadata::CONFIG_BINDINGS;
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
        let commands = [
            (
                "UpdateLogger",
                VmmCommand::UpdateLogger(Default::default(), ()),
            ),
            ("AddDisk", VmmCommand::AddDisk(Default::default(), ())),
        ];
        for (name, command) in commands {
            let op = operation(name);
            assert!(format!("{command:?}").starts_with(op.name));
            #[cfg(feature = "serde")]
            {
                let parsed = (op.parse)(json!({}).into()).unwrap();
                assert_eq!(
                    serde_json::to_value(parsed).unwrap(),
                    serde_json::to_value(&command).unwrap()
                );
                assert!((op.parse)(json!(null).into()).is_err());
            }
            #[cfg(all(feature = "request", feature = "serde"))]
            {
                let (request, _waiter) = command.into_request();
                assert!(format!("{request:?}").starts_with(op.name));
            }
        }
        let logger = operation("UpdateLogger");
        #[cfg(feature = "serde")]
        {
            let parsed =
                (logger.parse)(json!({ "level": "warn", "show_tid": false }).into()).unwrap();
            let VmmCommand::UpdateLogger(parameters, ()) = parsed else {
                panic!("expected UpdateLogger");
            };
            assert_eq!(parameters.level, Some(crate::logger::LevelFilter::Warn));
            assert_eq!(parameters.show_tid, Some(false));
            assert!((logger.parse)(json!({ "unknown": true }).into()).is_err());
        }
        assert_eq!(logger.config, None);
        assert_eq!(logger.route, Some(("PATCH", "/v1/logger").into()));
        let disk = operation("AddDisk");
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
            assert!(matches!(commands.as_slice(), [VmmCommand::AddDisk(_, ())]));
            let schema = bindings.schema(&mut generator);
            assert_eq!(
                schema.as_value()["properties"]["machine"]["properties"]["disks"]["type"],
                "array"
            );
        }
    }
}
