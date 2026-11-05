use std::collections::HashSet;

use proc_macro2::TokenStream;
use quote::{ToTokens, quote};
use syn::{GenericArgument, Ident, PathArguments, Type};

use crate::parse::{Operation, Schema};

pub fn schema(schema: &Schema) -> TokenStream {
    let operations = operations(schema);
    let fds = fd_impls(schema);
    let replies = serialized_replies(schema);
    let serde = serde_impls(schema);
    let metadata = metadata(schema);
    quote! {
        #operations
        #fds
        #replies
        #serde
        #metadata
    }
}

fn operations(schema: &Schema) -> TokenStream {
    let name = &schema.name;
    let variants: Vec<_> = schema.operations.iter().map(|op| &op.variant).collect();
    let requests: Vec<_> = schema.operations.iter().map(|op| &op.request).collect();
    let responses: Vec<_> = schema.operations.iter().map(|op| &op.response).collect();
    quote! {
        pub enum #name<M: crate::op_mode::OpMode> {
            #(#variants(#requests, M::Reply<#responses>),)*
        }

        #(impl crate::RawVmmOp for #requests {
            type Response = #responses;

            #[cfg(feature = "request")]
            fn into_vmm_request(self) -> (
                #name<crate::op_mode::Request>,
                ::oneshot::Receiver<Self::Response>,
            ) {
                let (sender, receiver) = ::oneshot::channel();
                (#name::#variants(self, sender.into()), receiver)
            }

            fn into_vmm_command(self) -> #name<crate::op_mode::Command> {
                #name::#variants(self, ())
            }
        })*

        impl<M: crate::op_mode::OpMode> ::core::fmt::Debug for #name<M> {
            fn fmt(&self, formatter: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                match self {
                    #(Self::#variants(parameters, _) => formatter
                        .debug_tuple(stringify!(#variants)).field(parameters).finish(),)*
                }
            }
        }
    }
}

fn fd_impls(schema: &Schema) -> TokenStream {
    let name = &schema.name;
    let variants = schema.operations.iter().map(|op| &op.variant);
    let requests = schema
        .operations
        .iter()
        .filter(|op| !op.properties.request_fds)
        .map(|op| &op.request);
    let mut seen = HashSet::new();
    let responses = schema
        .operations
        .iter()
        .filter(|op| !op.properties.response_fds)
        .filter_map(|op| response_body(&op.response))
        .filter(|body| seen.insert(body.to_token_stream().to_string()));
    quote! {
        #(impl crate::helpers::fd::MergeFd for #requests {})*
        #(impl crate::helpers::fd::SplitFd for #responses {})*

        impl<M: crate::op_mode::OpMode> crate::helpers::fd::MergeFd for #name<M> {
            fn merge_fd(&mut self, fds: &mut [Option<::std::os::fd::OwnedFd>]) -> crate::ApiResult<()> {
                match self {
                    #(Self::#variants(parameters, _) => crate::helpers::fd::MergeFd::merge_fd(parameters, fds),)*
                }
            }
        }
    }
}

fn response_body(response: &Type) -> Option<&Type> {
    if let Type::Tuple(tuple) = response
        && tuple.elems.is_empty()
    {
        return None;
    }
    if let Type::Path(path) = response
        && path.qself.is_none()
        && let Some(segment) = path.path.segments.last()
        && (segment.ident == "ApiResult" || segment.ident == "Result")
        && let PathArguments::AngleBracketed(arguments) = &segment.arguments
        && let Some(GenericArgument::Type(body)) = arguments.args.first()
    {
        return response_body(body);
    }
    Some(response)
}

fn serialized_replies(schema: &Schema) -> TokenStream {
    let name = &schema.name;
    let variants = schema.operations.iter().map(|op| &op.variant);
    let responses = schema.operations.iter().map(|op| &op.response);
    quote! {
        #[cfg(all(feature = "request", feature = "serde"))]
        impl #name<crate::op_mode::Command> {
            pub fn into_request(self) -> (
                #name<crate::op_mode::Request>,
                crate::helpers::SerializedReplyWaiter,
            ) {
                use core::marker::PhantomData;
                use crate::helpers::reply::{SerializeReply as _, into_request};

                match self {
                    #(Self::#variants(parameters, ()) => into_request(
                        parameters,
                        #name::#variants,
                        |reply| PhantomData::<#responses>.serialize_reply(reply),
                    ),)*
                }
            }
        }
    }
}

fn serde_impls(schema: &Schema) -> TokenStream {
    let name = &schema.name;
    let variants: Vec<_> = schema.operations.iter().map(|op| &op.variant).collect();
    let requests: Vec<_> = schema.operations.iter().map(|op| &op.request).collect();
    quote! {
        #[cfg(feature = "serde")]
        impl ::serde::Serialize for #name<crate::op_mode::Command> {
            fn serialize<S: ::serde::Serializer>(
                &self,
                serializer: S,
            ) -> ::core::result::Result<S::Ok, S::Error> {
                #[derive(::serde::Serialize)]
                #[serde(rename_all = "kebab-case")]
                enum Parameters<'a> {
                    #(#variants(&'a #requests),)*
                }

                let parameters = match self {
                    #(Self::#variants(parameters, ()) => Parameters::#variants(parameters),)*
                };
                ::serde::Serialize::serialize(&parameters, serializer)
            }
        }

        #[cfg(feature = "serde")]
        impl<'de> ::serde::Deserialize<'de> for #name<crate::op_mode::Command> {
            fn deserialize<D: ::serde::Deserializer<'de>>(
                deserializer: D,
            ) -> ::core::result::Result<Self, D::Error> {
                #[derive(::serde::Deserialize)]
                #[serde(rename_all = "kebab-case")]
                enum Parameters {
                    #(#variants(#requests),)*
                }

                let parameters = <Parameters as ::serde::Deserialize>::deserialize(deserializer)?;
                Ok(match parameters {
                    #(Parameters::#variants(parameters) => Self::#variants(parameters, ()),)*
                })
            }
        }
    }
}

fn metadata(schema: &Schema) -> TokenStream {
    let name = &schema.name;
    let entries = schema.operations.iter().map(|op| metadata_entry(name, op));
    quote! {
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

            pub type Op = meta::Op<super::#name<crate::op_mode::Command>>;

            pub static OPERATIONS: &[Op] = &[#(#entries,)*];

            const _: () = meta::validate(OPERATIONS);

            #[cfg(feature = "serde")]
            pub(crate) static CONFIG_BINDINGS: config::ConfigBindings<
                #name<op_mode::Command>,
                { config::node_count(OPERATIONS) },
            > = config::ConfigBindings::new(OPERATIONS);
        }
    }
}

fn metadata_entry(name: &Ident, operation: &Operation) -> TokenStream {
    let Operation {
        variant,
        request,
        response,
        properties,
    } = operation;
    let config = match &properties.config {
        Some(config) => quote!(Some(#config)),
        None => quote!(None),
    };
    let route = match &properties.route {
        Some(route) => {
            let method = &route.method;
            let path = &route.path;
            quote!(Some(Route { method: stringify!(#method), path: #path }))
        }
        None => quote!(None),
    };
    quote! {
        Op {
            name: stringify!(#variant),
            config: #config,
            route: #route,
            #[cfg(feature = "serde")]
            parse: |input| input.deserialize::<#request>()
                .map(|parameters| #name::<op_mode::Command>::#variant(parameters, ()))
                .map_err(serde::de::Error::custom),
            #[cfg(not(feature = "serde"))]
            _command: PhantomData,
            #[cfg(feature = "schema")]
            request: |generator| generator.subschema_for::<#request>(),
            #[cfg(feature = "schema")]
            response: |generator| PhantomData::<#response>.response_schema(generator),
        }
    }
}
