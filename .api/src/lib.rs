//! The vmm's request surface, declared once.
//!
//! One `define_schema!` list registers in-scope parameter types and grows the
//! request enum, [`OPERATIONS`] and, behind the `wire` feature, the serialized
//! shape and the adapter to a running request.

use std::fmt;

mod lifecycle;
mod metadata;

pub use lifecycle::{GetInstanceInfo, InstanceInfo, Shutdown, Status, VmmStatus};
pub use metadata::{Operation, Route};

/// Join one declaration's `#[doc]` lines with newlines.
macro_rules! doc_lines {
    () => {
        ""
    };
    ($first:literal $(, $rest:literal)*) => {
        concat!($first $(, "\n", $rest)*)
    };
}

/// Rebuild a declared route path from its tokens, without the spaces
/// `stringify!` would put between them. Anything the grammar does not allow
/// leaves no rule to match, which is the rejection.
macro_rules! route_path {
    ($($path:tt)+) => {
        route_path!(@pieces [] $($path)+)
    };
    (@pieces [$($piece:expr),*]) => {
        concat!($($piece),*)
    };
    (@pieces [$($piece:expr),*] / $($rest:tt)*) => {
        route_path!(@pieces [$($piece,)* "/"] $($rest)*)
    };
    (@pieces [$($piece:expr),*] - $($rest:tt)*) => {
        route_path!(@pieces [$($piece,)* "-"] $($rest)*)
    };
    (@pieces [$($piece:expr),*] $segment:ident $($rest:tt)*) => {
        route_path!(@pieces [$($piece,)* stringify!($segment)] $($rest)*)
    };
    (@pieces [$($piece:expr),*] $number:literal $($rest:tt)*) => {
        route_path!(@pieces [$($piece,)* stringify!($number)] $($rest)*)
    };
}

/// The readable form of a declared CLI module path. The module itself is
/// neither resolved nor called here.
macro_rules! cli_path {
    ($first:ident $(:: $rest:ident)*) => {
        concat!(stringify!($first) $(, "::", stringify!($rest))*)
    };
}

macro_rules! schema_config {
    ([]) => {
        ::core::option::Option::None
    };
    ([duplicate]) => {
        $crate::metadata::duplicate_config()
    };
    ([$path:literal]) => {
        $crate::metadata::checked_config($path)
    };
}

macro_rules! schema_route {
    ([]) => {
        ::core::option::Option::None
    };
    ([duplicate]) => {
        $crate::metadata::duplicate_route()
    };
    ([$method:ident $($path:tt)+]) => {
        $crate::metadata::checked_route(stringify!($method), route_path!($($path)+))
    };
}

macro_rules! schema_cli {
    ([]) => {
        ::core::option::Option::None
    };
    ([duplicate]) => {
        $crate::metadata::duplicate_cli()
    };
    ([$($path:tt)+]) => {
        ::core::option::Option::Some(cli_path!($($path)+))
    };
}

/// Register the api's operations, and everything that has to agree with them.
///
/// An entry names an in-scope parameter type and its reply type, and ends
/// either with `;` or with a property block holding `config: ".a.b[]"`,
/// `route: PUT /v1/disks` and `cli: disk::cli` -- each optional, at most once,
/// in any order. The list grows `$request<M>` -- runnable by default --
/// [`OPERATIONS`] and the `wire` shape. The parameter types keep the fields,
/// derives and Serde attributes their own module gave them.
macro_rules! define_schema {
    ($request:ident { $($entries:tt)* }) => {
        schema_parse!(@entry { $request } [] $($entries)*);
    };
}

/// Walk the declaration, one entry and one property at a time, until
/// `schema_emit!` receives the whole list in a fixed shape. An entry
/// accumulates as `(docs op reply config route cli)`.
macro_rules! schema_parse {
    // The list is exhausted.
    (@entry { $request:ident } [$($entries:tt)*]) => {
        schema_emit! { $request $($entries)* }
    };

    // An entry starts with its optional doc comment. Its reply type is
    // gathered token by token, since only `;` or `{` ends it.
    (@entry { $request:ident } [$($entries:tt)*]
        $(#[doc = $doc:literal])* $op:ident -> $($tail:tt)+) => {
        schema_parse!(@reply { $request } [$($entries)*] ([$($doc)*] [$op]) [] $($tail)+);
    };

    // A final entry may also end where the declaration ends.
    (@reply { $request:ident } [$($entries:tt)*] $head:tt [$($reply:tt)+]) => {
        schema_parse!(@entry { $request }
            [$($entries)* ($head [$($reply)+] [] [] [])]);
    };
    (@reply { $request:ident } [$($entries:tt)*] $head:tt [$($reply:tt)*] ; $($rest:tt)*) => {
        schema_parse!(@entry { $request }
            [$($entries)* ($head [$($reply)*] [] [] [])] $($rest)*);
    };
    (@reply { $request:ident } [$($entries:tt)*] $head:tt [$($reply:tt)*]
        { $($properties:tt)* } $($rest:tt)*) => {
        schema_parse!(@props { $request } [$($entries)*] $head [$($reply)*] [] [] []
            { $($properties)* } $($rest)*);
    };
    (@reply { $request:ident } [$($entries:tt)*] $head:tt [$($reply:tt)*]
        $token:tt $($rest:tt)*) => {
        schema_parse!(@reply { $request } [$($entries)*] $head [$($reply)* $token] $($rest)*);
    };

    // The property block is exhausted; the entry is complete.
    (@props { $request:ident } [$($entries:tt)*] $head:tt [$($reply:tt)*]
        [$($config:tt)*] [$($route:tt)*] [$($cli:tt)*]
        { } $($rest:tt)*) => {
        schema_parse!(@entry { $request }
            [$($entries)* ($head [$($reply)*] [$($config)*] [$($route)*] [$($cli)*])]
            $($rest)*);
    };

    // A second `config`, `route` or `cli` poisons its slot; `schema_emit!`
    // turns that into a const evaluation failure.
    (@props { $request:ident } [$($entries:tt)*] $head:tt [$($reply:tt)*]
        [] [$($route:tt)*] [$($cli:tt)*]
        { config: $path:literal ; $($properties:tt)* } $($rest:tt)*) => {
        schema_parse!(@props { $request } [$($entries)*] $head [$($reply)*] [$path] [$($route)*] [$($cli)*]
            { $($properties)* } $($rest)*);
    };
    (@props { $request:ident } [$($entries:tt)*] $head:tt [$($reply:tt)*]
        [$($config:tt)+] [$($route:tt)*] [$($cli:tt)*]
        { config: $path:literal ; $($properties:tt)* } $($rest:tt)*) => {
        schema_parse!(@props { $request } [$($entries)*] $head [$($reply)*] [duplicate] [$($route)*] [$($cli)*]
            { $($properties)* } $($rest)*);
    };
    (@props { $request:ident } [$($entries:tt)*] $head:tt [$($reply:tt)*]
        [$($config:tt)*] [] [$($cli:tt)*]
        { route: $method:ident $($tail:tt)+ } $($rest:tt)*) => {
        schema_parse!(@route { $request } [$($entries)*] $head [$($reply)*] [$($config)*] [$method] [$($cli)*]
            { $($tail)* } $($rest)*);
    };
    (@props { $request:ident } [$($entries:tt)*] $head:tt [$($reply:tt)*]
        [$($config:tt)*] [$($route:tt)+] [$($cli:tt)*]
        { route: $method:ident $($tail:tt)+ } $($rest:tt)*) => {
        schema_parse!(@props { $request } [$($entries)*] $head [$($reply)*] [$($config)*] [duplicate] [$($cli)*]
            { $($method)* } $($rest)*);
    };
    (@props { $request:ident } [$($entries:tt)*] $head:tt [$($reply:tt)*]
        [$($config:tt)*] [$($route:tt)*] []
        { cli: $($tail:tt)+ } $($rest:tt)*) => {
        schema_parse!(@cli { $request } [$($entries)*] $head [$($reply)*] [$($config)*] [$($route)*] []
            { $($tail)* } $($rest)*);
    };
    (@props { $request:ident } [$($entries:tt)*] $head:tt [$($reply:tt)*]
        [$($config:tt)*] [$($route:tt)*] [$($cli:tt)+]
        { cli: $($tail:tt)+ } $($rest:tt)*) => {
        schema_parse!(@props { $request } [$($entries)*] $head [$($reply)*] [$($config)*] [$($route)*] [duplicate]
            { $($tail)* } $($rest)*);
    };
    (@props { $request:ident } [$($entries:tt)*] $head:tt [$($reply:tt)*]
        [$($config:tt)*] [$($route:tt)*] [$($cli:tt)*]
        { $unknown:ident : $($properties:tt)* } $($rest:tt)*) => {
        compile_error!(concat!("unknown operation property `", stringify!($unknown), "`"));
    };
    (@props { $request:ident } [$($entries:tt)*] $head:tt [$($reply:tt)*]
        [$($config:tt)*] [$($route:tt)*] [$($cli:tt)*]
        { $garbage:tt $($properties:tt)* } $($rest:tt)*) => {
        compile_error!(concat!(
            "operation properties are `config:`, `route:` and `cli:`, not `",
            stringify!($garbage), "`"
        ));
    };

    // A route's path is gathered token by token up to its `;`.
    (@route { $request:ident } [$($entries:tt)*] $head:tt [$($reply:tt)*]
        [$($config:tt)*] [$route:ident] [$($cli:tt)*]
        { ; $($properties:tt)* } $($rest:tt)*) => {
        compile_error!(concat!("`route` needs a path after `", stringify!($route), "`"));
    };
    (@route { $request:ident } [$($entries:tt)*] $head:tt [$($reply:tt)*]
        [$($config:tt)*] [$($route:tt)*] [$($cli:tt)*]
        { ; $($properties:tt)* } $($rest:tt)*) => {
        schema_parse!(@props { $request } [$($entries)*] $head [$($reply)*] [$($config)*] [$($route)*] [$($cli)*]
            { $($properties)* } $($rest)*);
    };
    (@route { $request:ident } [$($entries:tt)*] $head:tt [$($reply:tt)*]
        [$($config:tt)*] [$($route:tt)*] [$($cli:tt)*]
        { $token:tt $($tail:tt)* } $($rest:tt)*) => {
        schema_parse!(@route { $request } [$($entries)*] $head [$($reply)*] [$($config)*] [$($route)* $token] [$($cli)*]
            { $($tail)* } $($rest)*);
    };

    // The same for a CLI module path.
    (@cli { $request:ident } [$($entries:tt)*] $head:tt [$($reply:tt)*]
        [$($config:tt)*] [$($route:tt)*] []
        { ; $($properties:tt)* } $($rest:tt)*) => {
        compile_error!("`cli` needs a module path");
    };
    (@cli { $request:ident } [$($entries:tt)*] $head:tt [$($reply:tt)*]
        [$($config:tt)*] [$($route:tt)*] [$($cli:tt)*]
        { ; $($properties:tt)* } $($rest:tt)*) => {
        schema_parse!(@props { $request } [$($entries)*] $head [$($reply)*] [$($config)*] [$($route)*] [$($cli)*]
            { $($properties)* } $($rest)*);
    };
    (@cli { $request:ident } [$($entries:tt)*] $head:tt [$($reply:tt)*]
        [$($config:tt)*] [$($route:tt)*] [$($cli:tt)*]
        { $token:tt $($tail:tt)* } $($rest:tt)*) => {
        schema_parse!(@cli { $request } [$($entries)*] $head [$($reply)*] [$($config)*] [$($route)*] [$($cli)* $token]
            { $($tail)* } $($rest)*);
    };
}

/// Grow everything from the parsed list.
macro_rules! schema_emit {
    ($request:ident $((
        ([$($doc:literal)*] [$op:ident]) [$($reply:tt)*]
        [$($config:tt)*] [$($route:tt)*] [$($cli:tt)*]
    ))*) => {
        /// A request for the vmm: one operation's parameters, plus the slot its
        /// answer travels in.
        ///
        /// `M` decides the slot: empty for plain data, a channel when runnable.
        pub enum $request<M: $crate::RequestMode = $crate::mode::Execute> {
            $(
                $(#[doc = $doc])*
                $op($op, M::Reply<$reply>),
            )*
        }

        impl<M: $crate::RequestMode> $request<M> {
            /// Which operation this request performs.
            pub fn name(&self) -> &'static str {
                match self {
                    $(Self::$op(..) => stringify!($op),)*
                }
            }
        }

        // Print the parameters only: the reply slot is plumbing.
        impl<M: $crate::RequestMode> ::core::fmt::Debug for $request<M> {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                match self {
                    $(
                        Self::$op(params, _) => {
                            f.debug_tuple(stringify!($op)).field(params).finish()
                        }
                    )*
                }
            }
        }

        /// Every operation, in declaration order.
        pub const OPERATIONS: &[$crate::Operation] = &[
            $(
                $crate::Operation {
                    name: stringify!($op),
                    doc: doc_lines!($($doc),*),
                },
            )*
        ];

        #[cfg(feature = "wire")]
        impl $request<$crate::mode::Command> {
            /// Attach a reply channel to a command.
            ///
            /// Returns the request the vmm runs and a future for its answer,
            /// erased to JSON: the one place the api boxes.
            pub fn prepare(self) -> ($request<$crate::mode::Execute>, $crate::wire::ReplyWaiter) {
                match self {
                    $(
                        Self::$op(params, ()) => {
                            let (reply, answer) = $crate::reply_channel();
                            ($request::$op(params, reply), $crate::wire::waiter(answer))
                        }
                    )*
                }
            }
        }

        #[cfg(feature = "wire")]
        impl ::serde::Serialize for $request<$crate::mode::Command> {
            fn serialize<S: ::serde::Serializer>(
                &self,
                serializer: S,
            ) -> ::core::result::Result<S::Ok, S::Error> {
                use ::serde::ser::SerializeMap as _;

                let mut map = serializer.serialize_map(Some(1))?;
                match self {
                    $(Self::$op(params, ()) => map.serialize_entry(stringify!($op), params)?,)*
                }
                map.end()
            }
        }

        #[cfg(feature = "wire")]
        impl<'de> ::serde::Deserialize<'de> for $request<$crate::mode::Command> {
            fn deserialize<D: ::serde::Deserializer<'de>>(
                deserializer: D,
            ) -> ::core::result::Result<Self, D::Error> {
                struct OneOperation;

                impl<'de> ::serde::de::Visitor<'de> for OneOperation {
                    type Value = $request<$crate::mode::Command>;

                    fn expecting(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                        f.write_str("a map holding exactly one operation")
                    }

                    fn visit_map<A: ::serde::de::MapAccess<'de>>(
                        self,
                        mut map: A,
                    ) -> ::core::result::Result<Self::Value, A::Error> {
                        let Some(name) = map.next_key::<String>()? else {
                            return Err(::serde::de::Error::invalid_value(
                                ::serde::de::Unexpected::Map,
                                &self,
                            ));
                        };
                        let request = match name.as_str() {
                            $(stringify!($op) => $request::$op(map.next_value::<$op>()?, ()),)*
                            other => {
                                return Err(::serde::de::Error::unknown_variant(
                                    other,
                                    &[$(stringify!($op)),*],
                                ));
                            }
                        };
                        match map.next_key::<String>()? {
                            None => Ok(request),
                            Some(_) => Err(::serde::de::Error::custom(
                                "a request holds exactly one operation",
                            )),
                        }
                    }
                }

                deserializer.deserialize_map(OneOperation)
            }
        }
    };
}

define_schema! {
    VmmRequest {
        /// Stop the vmm. Answered once every device has quiesced, so the reply
        /// means shutdown finished rather than shutdown started.
        Shutdown -> ();

        /// What the vmm is currently doing.
        Status -> VmmStatus;

        /// Which instance this vmm is.
        GetInstanceInfo -> InstanceInfo;
    }
}

/// A request as plain data: parameters only, no channel to allocate.
pub type Command = VmmRequest<mode::Command>;

/// One operation, as seen from outside Rust's type system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Operation {
    /// The operation's name, stable across builds.
    pub name: &'static str,
    /// Its doc comment: one line per `///`, joined by newlines, empty if it has
    /// none.
    pub doc: &'static str,
}

/// The channel a request is answered on.
///
/// `Send`, so a handler can move it into a task and answer later. The reply type
/// belongs to the operation, so a `Status` is only answered with a `VmmStatus`:
///
/// ```
/// use api::{Reply, Status, VmmRequest, VmmStatus, reply_channel};
///
/// let (reply, answer): (Reply<VmmStatus>, _) = reply_channel();
/// let request: VmmRequest = VmmRequest::Status(Status {}, reply);
///
/// // The vmm's side of the same request.
/// match request {
///     VmmRequest::Status(_, reply) => reply.send(VmmStatus { devices_running: 3 }),
///     other => panic!("unexpected request: {other:?}"),
/// }
///
/// assert_eq!(answer.recv().expect("answered").devices_running, 3);
/// ```
///
/// Answering it with a `u8` instead does not compile:
///
/// ```compile_fail
/// use api::{Reply, Status, VmmRequest, VmmStatus, reply_channel};
///
/// let (reply, answer): (Reply<u8>, _) = reply_channel();
/// let request: VmmRequest = VmmRequest::Status(Status {}, reply);
/// ```
pub struct Reply<T>(oneshot::Sender<T>);

impl<T> Reply<T> {
    /// Answer the request; a dropped receiver is ignored rather than reported.
    pub fn send(self, value: T) {
        let _ = self.0.send(value);
    }
}

impl<T> fmt::Debug for Reply<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Reply")
    }
}

/// The receiving half of a reply channel: blocking, timed and awaited receives.
pub type Answer<T> = oneshot::Receiver<T>;

/// Create the two halves of a request's reply channel.
pub fn reply_channel<T>() -> (Reply<T>, Answer<T>) {
    let (tx, rx) = oneshot::channel();
    (Reply(tx), rx)
}

/// How a request carries its operation's answer: an empty slot for plain data,
/// a channel for a runnable request.
pub trait RequestMode {
    /// The slot an operation's reply travels in.
    type Reply<T>;
}

pub mod mode {
    //! The two slots a request can carry.

    use super::{Reply, RequestMode};

    /// Ready to run: the answer arrives on a channel this request owns.
    pub struct Execute;

    impl RequestMode for Execute {
        type Reply<T> = Reply<T>;
    }

    /// Plain data: nothing to allocate, and nothing to disconnect.
    pub struct Command;

    impl RequestMode for Command {
        type Reply<T> = ();
    }
}

#[cfg(feature = "wire")]
pub mod wire;

#[cfg(test)]
mod tests;
