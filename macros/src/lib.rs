use proc_macro::TokenStream;

use quote::{format_ident, quote};
use syn::{
    braced, bracketed, parse::Parse, parse_macro_input, parse_quote, punctuated::Punctuated, Token,
};

enum FieldOrClosure {
    Field(syn::Ident),
    Closure(syn::Ident, syn::Expr),
}

impl Parse for FieldOrClosure {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        input.parse().map(Self::Field).or_else(|_| {
            input.parse().map(|mut closure: syn::ExprClosure| {
                assert_eq!(closure.inputs.len(), 1);
                let syn::Pat::Ident(arg) = closure.inputs.pop().unwrap().into_value() else {
                    panic!("expected ident for closure argument");
                };

                Self::Closure(arg.ident, *closure.body)
            })
        })
    }
}

struct EventVariant {
    name: syn::Ident,
    fields: Option<Punctuated<FieldOrClosure, Token![,]>>,
}

impl Parse for EventVariant {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let name = input.parse()?;
        let fields = if input.peek(syn::token::Brace) {
            let f;
            braced!(f in input);
            let f = Punctuated::parse_terminated(&f)?;
            Some(f)
        } else {
            None
        };

        Ok(Self { name, fields })
    }
}

struct Input {
    object: syn::Expr,
    event_object: syn::Ident,
    event_type: syn::Type,
    events: Punctuated<EventVariant, Token![,]>,
}

impl Parse for Input {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let object = input.parse()?;
        input.parse::<Token![,]>()?;
        let event_object = input.parse()?;
        input.parse::<Token![:]>()?;
        let event_type = input.parse()?;
        input.parse::<Token![=>]>()?;
        let events;
        bracketed!(events in input);
        let events = Punctuated::parse_terminated(&events)?;
        Ok(Self {
            object,
            event_object,
            event_type,
            events,
        })
    }
}

#[proc_macro]
pub fn simple_event_shunt(tokens: TokenStream) -> TokenStream {
    let Input {
        object,
        event_object,
        event_type,
        events,
    } = parse_macro_input!(tokens as Input);

    let match_arms = events.into_iter().map(|e| {
        let mut field_names = Punctuated::<_, Token![,]>::new();
        let mut fn_args = Punctuated::<syn::Expr, Token![,]>::new();
        if let Some(fields) = e.fields {
            for field in fields {
                match field {
                    FieldOrClosure::Field(name) => {
                        fn_args.push(parse_quote! { #name });
                        field_names.push(name);
                    }
                    FieldOrClosure::Closure(name, expr) => {
                        field_names.push(name);
                        fn_args.push(expr);
                    }
                }
            }
        }

        let name = e.name;
        let fn_name = String::from_utf8(
            name.to_string()
                .bytes()
                .enumerate()
                .flat_map(|(idx, c)| {
                    if idx != 0 && c.is_ascii_uppercase() {
                        vec![b'_', c.to_ascii_lowercase()]
                    } else {
                        vec![c.to_ascii_lowercase()]
                    }
                })
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let keyword_pfx = if fn_name == "type" { "_" } else { "" };
        let fn_name = format_ident!("{keyword_pfx}{fn_name}");

        quote! {
            #name { #field_names } => { #object.#fn_name(#fn_args); }
        }
    });
    quote! {{
        use #event_type::*;
        match #event_object {
            #(#match_arms)*
            _ => log::warn!(concat!("unhandled ", stringify!(#event_type), ": {:?}"), #event_object)
        }
    }}
    .into()
}
