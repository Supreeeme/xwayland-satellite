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

enum ShuntObject {
    Ident(syn::Ident),
    KSelf(Token![self]),
}

impl Parse for ShuntObject {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        if input.peek(Token![self]) {
            Ok(Self::KSelf(input.parse()?))
        } else {
            Ok(Self::Ident(input.parse()?))
        }
    }
}

impl quote::ToTokens for ShuntObject {
    fn to_tokens(&self, tokens: &mut proc_macro2::TokenStream) {
        match self {
            Self::Ident(i) => i.to_tokens(tokens),
            Self::KSelf(s) => s.to_tokens(tokens),
        }
    }
}
struct Input {
    object: syn::Expr,
    event_object: ShuntObject,
    event_type: syn::Type,
    events: Punctuated<EventVariant, Token![,]>,
}

impl Parse for Input {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let object = input.parse()?;
        input.parse::<Token![,]>()?;
        let event_object = input.parse()?;
        let event_type = if matches!(event_object, ShuntObject::Ident(..)) {
            input.parse::<Token![:]>()?;
            input.parse()?
        } else {
            parse_quote!(Self)
        };
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
            #event_type::#name { #field_names } => { #object.#fn_name(#fn_args); }
        }
    });
    quote! {{
        match #event_object {
            #(#match_arms)*
            _ => log::warn!("unhandled {}: {:?}", std::any::type_name::<#event_type>(), #event_object)
        }
    }}
    .into()
}
