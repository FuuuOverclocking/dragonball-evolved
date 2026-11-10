use proc_macro2::TokenStream;
use quote::quote;
use syn::parse::Parser;
use syn::{Attribute, Data, DeriveInput, LitBool, parse_quote};

pub fn expand(args: TokenStream, input: TokenStream) -> syn::Result<TokenStream> {
    let mut serialize = None;
    let mut deserialize = None;
    let mut schema = None;
    syn::meta::parser(|meta| {
        let option = if meta.path.is_ident("serialize") {
            &mut serialize
        } else if meta.path.is_ident("deserialize") {
            &mut deserialize
        } else if meta.path.is_ident("schema") {
            &mut schema
        } else {
            return Err(meta.error("expected `serialize`, `deserialize` or `schema`"));
        };
        if option.is_some() {
            return Err(meta.error("duplicate option"));
        }
        *option = Some(meta.value()?.parse::<LitBool>()?.value);
        Ok(())
    })
    .parse2(args)?;

    let mut item: DeriveInput = syn::parse2(input)?;
    match &mut item.data {
        Data::Struct(data) => {
            for field in &mut data.fields {
                gate_attributes(&mut field.attrs);
            }
        }
        Data::Enum(data) => {
            for variant in &mut data.variants {
                gate_attributes(&mut variant.attrs);
                for field in &mut variant.fields {
                    gate_attributes(&mut field.attrs);
                }
            }
        }
        Data::Union(_) => {
            return Err(syn::Error::new_spanned(
                &item.ident,
                "api_type only supports structs and enums",
            ));
        }
    }
    gate_attributes(&mut item.attrs);

    let mut derives = Vec::new();
    if serialize.unwrap_or(true) {
        derives.push(quote! {
            #[cfg_attr(feature = "serde", derive(serde::Serialize))]
        });
    }
    if deserialize.unwrap_or(true) {
        derives.push(quote! {
            #[cfg_attr(feature = "serde", derive(serde::Deserialize))]
        });
    }
    if schema.unwrap_or(true) {
        derives.push(quote! {
            #[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
        });
    }
    Ok(quote! {
        #(#derives)*
        #item
    })
}

fn gate_attributes(attrs: &mut [Attribute]) {
    for attr in attrs {
        let feature = if attr.path().is_ident("serde") {
            "serde"
        } else if attr.path().is_ident("schemars") {
            "schema"
        } else {
            continue;
        };
        let meta = &attr.meta;
        attr.meta = parse_quote!(cfg_attr(feature = #feature, #meta));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_expansion(args: TokenStream, input: TokenStream, expected: TokenStream) {
        let actual = expand(args, input).unwrap();
        syn::parse2::<DeriveInput>(actual.clone()).unwrap();
        assert_eq!(actual.to_string(), expected.to_string());
    }

    #[test]
    fn gates_type_and_named_field_attributes_without_changing_generics() {
        assert_expansion(
            quote! {},
            quote! {
                #[derive(Debug)]
                #[serde(default, deny_unknown_fields)]
                #[schemars(rename = "Config")]
                pub struct Config<T> where T: Default {
                    #[serde(skip)]
                    #[schemars(skip)]
                    pub value: T,
                }
            },
            quote! {
                #[cfg_attr(feature = "serde", derive(serde::Serialize))]
                #[cfg_attr(feature = "serde", derive(serde::Deserialize))]
                #[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
                #[derive(Debug)]
                #[cfg_attr(feature = "serde", serde(default, deny_unknown_fields))]
                #[cfg_attr(feature = "schema", schemars(rename = "Config"))]
                pub struct Config<T> where T: Default {
                    #[cfg_attr(feature = "serde", serde(skip))]
                    #[cfg_attr(feature = "schema", schemars(skip))]
                    pub value: T,
                }
            },
        );
    }

    #[test]
    fn gates_enum_variants_and_tuple_fields() {
        assert_expansion(
            quote! { serialize = false, schema = false },
            quote! {
                #[serde(try_from = "String")]
                enum Target {
                    #[serde(rename = "file")]
                    #[schemars(rename = "file")]
                    File(#[serde(skip)] String),
                }
            },
            quote! {
                #[cfg_attr(feature = "serde", derive(serde::Deserialize))]
                #[cfg_attr(feature = "serde", serde(try_from = "String"))]
                enum Target {
                    #[cfg_attr(feature = "serde", serde(rename = "file"))]
                    #[cfg_attr(feature = "schema", schemars(rename = "file"))]
                    File(#[cfg_attr(feature = "serde", serde(skip))] String),
                }
            },
        );
    }

    #[test]
    fn supports_serialize_only_and_explicit_defaults() {
        assert_expansion(
            quote! { serialize = true, deserialize = false, schema = false },
            quote! { struct Config; },
            quote! {
                #[cfg_attr(feature = "serde", derive(serde::Serialize))]
                struct Config;
            },
        );
    }

    #[test]
    fn preserves_existing_conditional_attributes() {
        assert_expansion(
            quote! { serialize = false, deserialize = false, schema = false },
            quote! {
                struct Wrapper(#[cfg_attr(feature = "serde", serde(skip))] u8);
            },
            quote! {
                struct Wrapper(#[cfg_attr(feature = "serde", serde(skip))] u8);
            },
        );
    }

    #[test]
    fn rejects_invalid_options_and_items() {
        for args in [
            quote! { unknown = false },
            quote! { serialize },
            quote! { serialize = "false" },
            quote! { serialize = false, serialize = true },
        ] {
            assert!(expand(args, quote! { struct Config; }).is_err());
        }
        assert!(expand(quote! {}, quote! { union Value { byte: u8 } }).is_err());
        assert!(expand(quote! {}, quote! { fn function() {} }).is_err());
    }
}
