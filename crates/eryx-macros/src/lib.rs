//! Procedural macros for the eryx Python sandbox.
//!
//! Provides the `#[callback]` attribute macro for defining sandbox callbacks
//! with minimal boilerplate.
//!
//! # Example
//!
//! ```rust,ignore
//! use eryx::callback;
//! use eryx::CallbackError;
//! use serde_json::{json, Value};
//!
//! /// Returns the current Unix timestamp
//! #[callback]
//! async fn get_time() -> Result<Value, CallbackError> {
//!     let now = std::time::SystemTime::now()
//!         .duration_since(std::time::UNIX_EPOCH)
//!         .unwrap()
//!         .as_secs();
//!     Ok(json!({ "timestamp": now }))
//! }
//!
//! // Usage: the function name becomes a unit struct
//! let sandbox = Sandbox::embedded()
//!     .with_callback(get_time)
//!     .build()?;
//! ```

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{Attribute, FnArg, ItemFn, Pat, Type, parse_macro_input};

/// Attribute macro for defining sandbox callbacks with minimal boilerplate.
///
/// This macro transforms an async function into a type implementing `TypedCallback`,
/// allowing it to be used directly with `Sandbox::with_callback()`.
///
/// # Syntax
///
/// ```rust,ignore
/// /// Description from doc comment
/// #[callback]
/// async fn callback_name(param1: Type1, param2: Option<Type2>) -> Result<Value, CallbackError> {
///     // implementation
/// }
/// ```
///
/// # Generated Code
///
/// The macro generates:
/// 1. An args struct with the function parameters (with `Deserialize` and `JsonSchema` derives)
/// 2. A unit struct with the same name as the function
/// 3. A `TypedCallback` implementation for the unit struct
///
/// # Requirements
///
/// - Function must be `async`
/// - Return type must be `Result<Value, CallbackError>` or `Result<serde_json::Value, eryx::CallbackError>`
/// - Parameters must be deserializable types
/// - `Option<T>` parameters are treated as optional (with `#[serde(default)]`)
///
/// # Example
///
/// ```rust,ignore
/// use eryx::{callback, CallbackError, Sandbox};
/// use serde_json::{json, Value};
///
/// /// Echoes the message back
/// #[callback]
/// async fn echo(message: String, repeat: Option<u32>) -> Result<Value, CallbackError> {
///     let repeat = repeat.unwrap_or(1);
///     Ok(json!({ "echoed": message.repeat(repeat as usize) }))
/// }
///
/// // Use directly - `echo` is now a unit struct implementing TypedCallback
/// let sandbox = Sandbox::embedded()
///     .with_callback(echo)
///     .build()?;
/// ```
#[proc_macro_attribute]
pub fn callback(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as ItemFn);

    match generate_callback(attr.into(), input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn generate_callback(attr: TokenStream2, input: ItemFn) -> syn::Result<TokenStream2> {
    if !attr.is_empty() {
        return Err(syn::Error::new_spanned(
            attr,
            "#[callback] does not take any arguments",
        ));
    }

    // Validate: must be async
    if input.sig.asyncness.is_none() {
        return Err(syn::Error::new_spanned(
            input.sig.fn_token,
            "#[callback] functions must be async",
        ));
    }

    // Validate: no generics
    if !input.sig.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.sig.generics,
            "#[callback] functions cannot have generic parameters",
        ));
    }

    // Validate: no self parameter
    if let Some(FnArg::Receiver(receiver)) = input.sig.inputs.first() {
        return Err(syn::Error::new_spanned(
            receiver,
            "#[callback] functions cannot have a self parameter",
        ));
    }

    let fn_name = &input.sig.ident;
    let fn_name_str = fn_name.to_string();
    let impl_fn_name = format_ident!("__{}_impl", fn_name);
    let args_struct_name = format_ident!("__{}_Args", to_pascal_case(&fn_name_str));
    let visibility = &input.vis;

    // Extract doc comment for description
    let description = extract_doc_comment(&input.attrs).unwrap_or_else(|| fn_name_str.clone());

    // Extract parameters, each with the identifier it binds
    let params = input
        .sig
        .inputs
        .iter()
        .filter_map(|arg| {
            if let FnArg::Typed(pat_type) = arg {
                Some(pat_type)
            } else {
                None
            }
        })
        .map(|pat_type| Ok((param_name(&pat_type.pat)?, &pat_type.ty)))
        .collect::<syn::Result<Vec<_>>>()?;

    // Generate args struct fields and invoke arguments
    let (args_fields, invoke_args, has_params) = if params.is_empty() {
        // No parameters - use unit type
        (quote! {}, quote! {}, false)
    } else {
        let fields: Vec<_> = params
            .iter()
            .map(|(name, ty)| {
                let is_option = is_option_type(ty);

                if is_option {
                    quote! {
                        #[serde(default)]
                        #name: #ty
                    }
                } else {
                    quote! {
                        #name: #ty
                    }
                }
            })
            .collect();

        let args: Vec<_> = params
            .iter()
            .map(|(name, _)| quote! { __args.#name })
            .collect();

        (
            quote! {
                #(#fields),*
            },
            quote! { #(#args),* },
            true,
        )
    };

    // Generate the impl function (renamed original)
    let impl_fn = {
        let mut impl_fn = input.clone();
        impl_fn.sig.ident = impl_fn_name.clone();
        impl_fn.vis = syn::Visibility::Inherited; // Make private
        // Remove the #[callback] attribute and doc comments from impl fn
        impl_fn
            .attrs
            .retain(|attr| !attr.path().is_ident("callback") && !attr.path().is_ident("doc"));
        impl_fn
    };

    // Generate args struct (only if there are parameters). It is the unit
    // struct's `TypedCallback::Args`, so it needs at least the same visibility.
    let args_struct = if has_params {
        quote! {
            #[doc(hidden)]
            #[derive(::serde::Deserialize, ::schemars::JsonSchema)]
            #visibility struct #args_struct_name {
                #args_fields
            }
        }
    } else {
        quote! {}
    };

    // The Args type for TypedCallback
    let args_type = if has_params {
        quote! { #args_struct_name }
    } else {
        quote! { () }
    };

    // The invoke call
    let invoke_call = if has_params {
        quote! { #impl_fn_name(#invoke_args) }
    } else {
        quote! { #impl_fn_name() }
    };

    // Generate the unit struct and TypedCallback impl
    let output = quote! {
        #impl_fn

        #args_struct

        /// Callback struct generated by `#[callback]` macro.
        #[allow(non_camel_case_types)]
        #visibility struct #fn_name;

        impl ::eryx::TypedCallback for #fn_name {
            type Args = #args_type;

            fn name(&self) -> &str {
                #fn_name_str
            }

            fn description(&self) -> &str {
                #description
            }

            fn invoke_typed(
                &self,
                #[allow(unused_variables)]
                __args: Self::Args,
            ) -> ::std::pin::Pin<
                ::std::boxed::Box<
                    dyn ::std::future::Future<
                            Output = ::std::result::Result<
                                ::serde_json::Value,
                                ::eryx::CallbackError,
                            >,
                        > + ::std::marker::Send
                        + '_,
                >,
            > {
                ::std::boxed::Box::pin(#invoke_call)
            }
        }
    };

    Ok(output)
}

/// Extract the first doc comment as a description string.
fn extract_doc_comment(attrs: &[Attribute]) -> Option<String> {
    for attr in attrs {
        if attr.path().is_ident("doc")
            && let syn::Meta::NameValue(meta) = &attr.meta
            && let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(lit_str),
                ..
            }) = &meta.value
        {
            let value = lit_str.value();
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// Check if a type is Option<T>
fn is_option_type(ty: &Type) -> bool {
    if let Type::Path(type_path) = ty
        && let Some(segment) = type_path.path.segments.last()
    {
        return segment.ident == "Option";
    }
    false
}

/// Extract the identifier from a pattern (e.g., `message` from `message: String`
/// or `mut message: String`). The identifier doubles as the args struct field
/// and the JSON key, so anything else (`_`, tuples, `ref`, `x @ ..`) is rejected.
fn param_name(pat: &Pat) -> syn::Result<&syn::Ident> {
    match pat {
        Pat::Ident(pat_ident) if pat_ident.by_ref.is_none() && pat_ident.subpat.is_none() => {
            Ok(&pat_ident.ident)
        }
        _ => Err(syn::Error::new_spanned(
            pat,
            "#[callback] parameters must be plain identifiers, like `name: Type`",
        )),
    }
}

/// Convert snake_case to PascalCase
fn to_pascal_case(s: &str) -> String {
    s.split('_')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => first.to_uppercase().chain(chars).collect(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_quote;

    fn error(attr: TokenStream2, input: ItemFn) -> String {
        match generate_callback(attr, input) {
            Ok(_) => String::from("expanded without error"),
            Err(err) => err.to_string(),
        }
    }

    #[test]
    fn rejects_arguments() {
        let input = parse_quote!(
            async fn f() -> Result<Value, CallbackError> {}
        );
        assert_eq!(
            error(quote!(name = "g"), input),
            "#[callback] does not take any arguments"
        );
    }

    #[test]
    fn rejects_non_identifier_params() {
        let inputs: [ItemFn; 4] = [
            parse_quote!(
                async fn f(_: u32) -> Result<Value, CallbackError> {}
            ),
            parse_quote!(
                async fn f((a, b): (u32, u32)) -> Result<Value, CallbackError> {}
            ),
            parse_quote!(
                async fn f(ref a: u32) -> Result<Value, CallbackError> {}
            ),
            parse_quote!(
                async fn f(a @ 0..=9: u32) -> Result<Value, CallbackError> {}
            ),
        ];
        for input in inputs {
            assert_eq!(
                error(TokenStream2::new(), input),
                "#[callback] parameters must be plain identifiers, like `name: Type`"
            );
        }
    }

    #[test]
    fn mut_param_becomes_plain_field() {
        let input = parse_quote!(
            async fn f(mut a: u32) -> Result<Value, CallbackError> {}
        );
        let expanded = match generate_callback(TokenStream2::new(), input) {
            Ok(tokens) => tokens.to_string(),
            Err(err) => err.to_string(),
        };
        assert!(
            expanded.contains("struct __F_Args { a : u32 }"),
            "{expanded}"
        );
    }
}
