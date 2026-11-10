use proc_macro::TokenStream;
use syn::parse_macro_input;

mod api_type;
mod expand;
mod parse;

#[proc_macro_attribute]
pub fn api_type(args: TokenStream, input: TokenStream) -> TokenStream {
    api_type::expand(args.into(), input.into())
        .unwrap_or_else(|error| error.into_compile_error())
        .into()
}

#[proc_macro]
pub fn define_schema(input: TokenStream) -> TokenStream {
    expand::schema(&parse_macro_input!(input as parse::Schema)).into()
}

#[cfg(test)]
mod tests;
