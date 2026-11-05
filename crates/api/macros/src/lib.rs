use proc_macro::TokenStream;
use syn::parse_macro_input;

mod expand;
mod parse;

#[proc_macro]
pub fn define_schema(input: TokenStream) -> TokenStream {
    expand::schema(&parse_macro_input!(input as parse::Schema)).into()
}

#[cfg(test)]
mod tests;
