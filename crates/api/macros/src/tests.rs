use proc_macro2::TokenStream;
use quote::{ToTokens, quote};
use syn::{Item, ItemImpl};

use crate::{expand, parse};

fn expand(input: TokenStream) -> syn::File {
    let schema = syn::parse2::<parse::Schema>(input).unwrap();
    syn::parse2(expand::schema(&schema)).unwrap()
}

fn trait_impls(file: &syn::File, name: &str) -> Vec<String> {
    file.items
        .iter()
        .filter_map(|item| {
            let Item::Impl(ItemImpl {
                trait_: Some((_, path, _)),
                self_ty,
                ..
            }) = item
            else {
                return None;
            };
            (path.segments.last().unwrap().ident == name)
                .then(|| self_ty.to_token_stream().to_string())
        })
        .collect()
}

#[test]
fn parses_qualified_requests_and_both_entry_forms() {
    let schema: parse::Schema = syn::parse2(quote! {
        VmmOp {
            crate::instance_info::GetInstanceInfo -> crate::instance_info::InstanceInfo;
            logger::UpdateLogger -> ApiResult<()> {}
        }
    })
    .unwrap();
    assert_eq!(schema.name, "VmmOp");
    assert_eq!(schema.operations[0].variant, "GetInstanceInfo");
    assert_eq!(schema.operations[1].variant, "UpdateLogger");
    assert_eq!(
        schema.operations[0].request.to_token_stream().to_string(),
        quote!(crate::instance_info::GetInstanceInfo).to_string()
    );
    assert!(schema.operations[0].properties.config.is_none());
    assert!(schema.operations[1].properties.route.is_none());
}

#[test]
fn parses_properties_and_accepts_the_cli_placeholder() {
    let schema: parse::Schema = syn::parse2(quote! {
        VmmOp {
            block::AddDisk -> ApiResult<()> {
                config: ".machine.disks[]";
                route: PUT /v1/disks;
                cli: crate::cli::add_disk;
                fd_in: req, resp;
            }
        }
    })
    .unwrap();
    let properties = &schema.operations[0].properties;
    assert_eq!(
        properties.config.as_ref().unwrap().value(),
        ".machine.disks[]"
    );
    assert_eq!(properties.route.as_ref().unwrap().method, "PUT");
    assert_eq!(properties.route.as_ref().unwrap().path.value(), "/v1/disks");
    assert!(properties.request_fds);
    assert!(properties.response_fds);
    let expanded = expand::schema(&schema).to_string();
    assert!(expanded.contains("\".machine.disks[]\""));
    assert!(expanded.contains("\"/v1/disks\""));
    assert!(!expanded.contains("add_disk"));
}

#[test]
fn route_paths_preserve_segments_and_trailing_slashes() {
    for (tokens, expected) in [
        (quote!(/), "/"),
        (quote!(/v1), "/v1"),
        (quote!(/v1/), "/v1/"),
        (quote!(/v1/block-devices), "/v1/block-devices"),
        (quote!(/v1/block-devices/), "/v1/block-devices/"),
        (
            quote!(/v1/live-upgrade/handover-state),
            "/v1/live-upgrade/handover-state",
        ),
        (quote!(/v1/type/async_op), "/v1/type/async_op"),
    ] {
        let schema: parse::Schema = syn::parse2(quote! {
            VmmOp { Op -> () { route: GET #tokens; } }
        })
        .unwrap();
        assert_eq!(
            schema.operations[0]
                .properties
                .route
                .as_ref()
                .unwrap()
                .path
                .value(),
            expected
        );
        assert!(
            expand::schema(&schema)
                .to_string()
                .contains(&format!("\"{expected}\""))
        );
    }
}

#[test]
fn request_fd_declarations_generate_only_merge_and_preserve_custom_impls() {
    let file = expand(quote! {
        VmmOp {
            Plain -> ();
            ResponseOnly -> Custom { fd_in: resp; }
            Custom -> () { fd_in: req; }
            Both -> Custom { fd_in: req, resp; }
            Reversed -> Custom { fd_in: resp, req; }
        }
    });
    assert_eq!(
        trait_impls(&file, "MergeFd"),
        ["Plain", "ResponseOnly", "VmmOp < M >"]
    );
    assert!(trait_impls(&file, "SplitFd").is_empty());
    assert_eq!(
        trait_impls(&file, "RawVmmOp"),
        ["Plain", "ResponseOnly", "Custom", "Both", "Reversed"]
    );
}

#[test]
fn response_fd_declarations_generate_split_once_for_shared_bodies() {
    let file = expand(quote! {
        VmmOp {
            First -> Shared;
            Second -> Other { fd_in: req; }
            Third -> Shared;
            Fourth -> crate::ApiResult<Shared>;
            Fifth -> std::result::Result<Other, String>;
            Sixth -> ();
            Seventh -> crate::ApiResult<()>;
            Eighth -> ApiResult<()>;
            Ninth -> crate::ApiResult<Generic<(u8, u16)>, String>;
            Tenth -> Generic<(u8, u16)>;
            Eleventh -> ApiResult<Result<Shared, String>>;
            Twelfth -> Result<Result<(), Error>, Error>;
            Thirteenth -> Result<Generic<Result<Shared, Error>>, Error>;
            Fourteenth -> Generic<Result<Shared, Error> >;
        }
    });
    assert_eq!(
        trait_impls(&file, "SplitFd"),
        [
            "Shared",
            "Other",
            "Generic < (u8 , u16) >",
            "Generic < Result < Shared , Error > >",
        ]
    );
}

#[test]
fn response_fd_declarations_preserve_custom_split_impls() {
    let file = expand(quote! {
        VmmOp {
            First -> Custom { fd_in: resp; }
            Second -> ApiResult<Custom> { fd_in: req, resp; }
            Third -> Custom { fd_in: resp, req; }
        }
    });
    assert!(trait_impls(&file, "SplitFd").is_empty());
}

#[test]
fn fd_types_keep_qualified_paths_and_skip_only_marked_responses() {
    let file = expand(quote! {
        VmmOp {
            first::Read -> first::Body;
            second::ReadOther -> second::Body;
            Marked -> Shared { fd_in: resp; }
            Unmarked -> Shared;
        }
    });
    assert_eq!(
        trait_impls(&file, "SplitFd"),
        ["first :: Body", "second :: Body", "Shared"]
    );
    assert_eq!(
        trait_impls(&file, "RawVmmOp"),
        ["first :: Read", "second :: ReadOther", "Marked", "Unmarked"]
    );
}

#[test]
fn invalid_properties_report_specific_errors() {
    for (properties, message) in [
        (quote!(unknown: true;), "unknown schema property"),
        (quote!(fd_in: request;), "fd_in accepts req and/or resp"),
        (quote!(fd_in: req, other;), "fd_in accepts req and/or resp"),
        (
            quote!(config: ".first"; config: ".second";),
            "duplicate schema property",
        ),
        (
            quote!(route: GET /; route: POST /;),
            "duplicate schema property",
        ),
        (
            quote!(fd_in: req; fd_in: resp;),
            "duplicate schema property",
        ),
        (
            quote!(cli: first; cli: second;),
            "duplicate schema property",
        ),
    ] {
        let result = syn::parse2::<parse::Schema>(quote! {
            VmmOp { Op -> () { #properties } }
        });
        assert_eq!(
            result.err().expect("expected an error").to_string(),
            message
        );
    }
}

#[test]
fn rejects_incomplete_or_malformed_declarations() {
    for input in [
        quote!(VmmOp { Op -> }),
        quote!(VmmOp { Op -> () }),
        quote!(VmmOp { Op -> () { config: 42; } }),
        quote!(VmmOp { Op -> () { route: GET v1; } }),
        quote!(VmmOp { Op -> () { route: GET /v1/-; } }),
        quote!(VmmOp { Op -> () { fd_in:; } }),
        quote!(VmmOp { Op -> () { fd_in: req,; } }),
        quote!(VmmOp { Op -> () { config: ".disk" } }),
        quote!(VmmOp { Op -> (); } trailing),
    ] {
        assert!(
            syn::parse2::<parse::Schema>(input.clone()).is_err(),
            "{input}"
        );
    }
}
