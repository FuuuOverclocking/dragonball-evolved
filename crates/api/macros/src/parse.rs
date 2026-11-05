use std::collections::HashSet;

use syn::ext::IdentExt;
use syn::parse::{Parse, ParseStream};
use syn::{Ident, LitStr, Path, Result, Token, Type, braced};

pub struct Schema {
    pub name: Ident,
    pub operations: Vec<Operation>,
}

pub struct Operation {
    pub variant: Ident,
    pub request: Path,
    pub response: Type,
    pub properties: Properties,
}

#[derive(Default)]
pub struct Properties {
    pub config: Option<LitStr>,
    pub route: Option<Route>,
    pub request_fds: bool,
    pub response_fds: bool,
}

pub struct Route {
    pub method: Ident,
    pub path: LitStr,
}

impl Parse for Schema {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let name = input.parse()?;
        let content;
        braced!(content in input);
        let mut operations = Vec::new();
        while !content.is_empty() {
            operations.push(content.parse()?);
        }
        Ok(Self { name, operations })
    }
}

impl Parse for Operation {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let request = input.call(Path::parse_mod_style)?;
        let variant = request.segments.last().unwrap().ident.clone();
        input.parse::<Token![->]>()?;
        let response = input.parse()?;
        let properties = if input.peek(Token![;]) {
            input.parse::<Token![;]>()?;
            Properties::default()
        } else {
            let content;
            braced!(content in input);
            content.parse()?
        };
        Ok(Self {
            variant,
            request,
            response,
            properties,
        })
    }
}

impl Parse for Properties {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut properties = Self::default();
        let mut seen = HashSet::new();
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            if !seen.insert(key.to_string()) {
                return Err(syn::Error::new(key.span(), "duplicate schema property"));
            }
            input.parse::<Token![:]>()?;
            match key.to_string().as_str() {
                "config" => properties.config = Some(input.parse()?),
                "route" => properties.route = Some(input.parse()?),
                "cli" => {
                    input.parse::<Path>()?;
                }
                "fd_in" => loop {
                    let side: Ident = input.parse()?;
                    match side.to_string().as_str() {
                        "req" => properties.request_fds = true,
                        "resp" => properties.response_fds = true,
                        _ => {
                            return Err(syn::Error::new(
                                side.span(),
                                "fd_in accepts req and/or resp",
                            ));
                        }
                    }
                    if !input.peek(Token![,]) {
                        break;
                    }
                    input.parse::<Token![,]>()?;
                },
                _ => return Err(syn::Error::new(key.span(), "unknown schema property")),
            }
            input.parse::<Token![;]>()?;
        }
        Ok(properties)
    }
}

impl Parse for Route {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let method = input.parse()?;
        let span = input.span();
        let mut path = String::new();
        loop {
            input.parse::<Token![/]>()?;
            path.push('/');
            if input.peek(Ident::peek_any) {
                path.push_str(&input.call(Ident::parse_any)?.to_string());
                while input.peek(Token![-]) {
                    input.parse::<Token![-]>()?;
                    path.push('-');
                    path.push_str(&input.call(Ident::parse_any)?.to_string());
                }
            }
            if !input.peek(Token![/]) {
                break;
            }
        }
        Ok(Self {
            method,
            path: LitStr::new(&path, span),
        })
    }
}
