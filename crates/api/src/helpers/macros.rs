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

        pub mod metadata {
            use super::*;

            pub use $crate::helpers::metadata::{Op, Route};

            pub fn ops() -> Vec<Op> {
                vec![$($crate::helpers::macros::define_schema!(
                    @metadata $op -> $reply { $($properties)* }
                )),*]
            }
        }
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
            request: ::schemars::schema_for!($op),
            response: {
                use $crate::helpers::metadata::ResponseSchema as _;
                ::core::marker::PhantomData::<$reply>.response_schema()
            },
        }
    };
}

pub(crate) use define_schema;
