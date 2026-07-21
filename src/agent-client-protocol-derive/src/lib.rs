//! Derive macros for Agent Client Protocol JSON-RPC traits.
//!
//! This crate provides derive macros to reduce boilerplate when implementing
//! custom JSON-RPC requests, notifications, and response types.
//!
//! # Example
//!
//! ```ignore
//! use agent_client_protocol::{JsonRpcRequest, JsonRpcNotification, JsonRpcResponse};
//!
//! #[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
//! #[request(method = "_hello", response = HelloResponse)]
//! struct HelloRequest {
//!     name: String,
//! }
//!
//! #[derive(Debug, Clone, Serialize, Deserialize, JsonRpcResponse)]
//! struct HelloResponse {
//!     greeting: String,
//! }
//!
//! #[derive(Debug, Clone, Serialize, Deserialize, JsonRpcNotification)]
//! #[notification(method = "_ping")]
//! struct PingNotification {
//!     timestamp: u64,
//! }
//! ```
//!
//! # Using within the `agent_client_protocol` crate
//!
//! When using these derives within the `agent_client_protocol` crate itself, add `crate = crate`:
//!
//! ```ignore
//! #[derive(JsonRpcRequest)]
//! #[request(method = "_foo", response = FooResponse, crate = crate)]
//! struct FooRequest { ... }
//! ```

use proc_macro::TokenStream;
use quote::quote;
use syn::{DeriveInput, LitStr, Path, Type, parse_macro_input};

/// Derive macro for implementing `JsonRpcRequest` and `JsonRpcMessage` traits.
///
/// # Attributes
///
/// - `#[request(method = "method_name", response = ResponseType)]`, where `ResponseType` may be
///   any Rust type, including generic types such as `Option<Response>`
/// - `#[request(method = "method_name", response = ResponseType, crate = crate)]` - for use within the `agent_client_protocol` crate
///
/// # Example
///
/// ```ignore
/// #[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
/// #[request(method = "_hello", response = HelloResponse)]
/// struct HelloRequest {
///     name: String,
/// }
/// ```
#[proc_macro_derive(JsonRpcRequest, attributes(request))]
pub fn derive_json_rpc_request(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;
    let (impl_generics, type_generics, where_clause) = input.generics.split_for_impl();

    // Parse attributes
    let (method, response_type, krate) = match parse_request_attrs(&input) {
        Ok(attrs) => attrs,
        Err(e) => return e.to_compile_error().into(),
    };

    let expanded = quote! {
        #[automatically_derived]
        impl #impl_generics #krate::JsonRpcMessage for #name #type_generics #where_clause {
            fn matches_method(method: &str) -> bool {
                method == #method
            }

            fn method(&self) -> &str {
                #method
            }

            fn to_untyped_message(&self) -> ::core::result::Result<#krate::UntypedMessage, #krate::Error> {
                #krate::UntypedMessage::new(#method, self)
            }

            fn parse_message(
                method: &str,
                params: &impl ::serde::Serialize,
            ) -> ::core::result::Result<Self, #krate::Error> {
                if method != #method {
                    return ::core::result::Result::Err(#krate::Error::method_not_found());
                }
                #krate::util::json_cast_params(params)
            }
        }

        #[automatically_derived]
        impl #impl_generics #krate::JsonRpcRequest for #name #type_generics #where_clause {
            type Response = #response_type;
        }
    };

    TokenStream::from(expanded)
}

/// Derive macro for implementing `JsonRpcNotification` and `JsonRpcMessage` traits.
///
/// # Attributes
///
/// - `#[notification(method = "method_name")]`
/// - `#[notification(method = "method_name", crate = crate)]` - for use within the `agent_client_protocol` crate
///
/// # Example
///
/// ```ignore
/// #[derive(Debug, Clone, Serialize, Deserialize, JsonRpcNotification)]
/// #[notification(method = "_ping")]
/// struct PingNotification {
///     timestamp: u64,
/// }
/// ```
#[proc_macro_derive(JsonRpcNotification, attributes(notification))]
pub fn derive_json_rpc_notification(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;
    let (impl_generics, type_generics, where_clause) = input.generics.split_for_impl();

    // Parse attributes
    let (method, krate) = match parse_notification_attrs(&input) {
        Ok(attrs) => attrs,
        Err(e) => return e.to_compile_error().into(),
    };

    let expanded = quote! {
        #[automatically_derived]
        impl #impl_generics #krate::JsonRpcMessage for #name #type_generics #where_clause {
            fn matches_method(method: &str) -> bool {
                method == #method
            }

            fn method(&self) -> &str {
                #method
            }

            fn to_untyped_message(&self) -> ::core::result::Result<#krate::UntypedMessage, #krate::Error> {
                #krate::UntypedMessage::new(#method, self)
            }

            fn parse_message(
                method: &str,
                params: &impl ::serde::Serialize,
            ) -> ::core::result::Result<Self, #krate::Error> {
                if method != #method {
                    return ::core::result::Result::Err(#krate::Error::method_not_found());
                }
                #krate::util::json_cast_params(params)
            }
        }

        #[automatically_derived]
        impl #impl_generics #krate::JsonRpcNotification for #name #type_generics #where_clause {}
    };

    TokenStream::from(expanded)
}

/// Derive macro for implementing `JsonRpcResponse` trait.
///
/// # Attributes
///
/// - `#[response(crate = crate)]` - for use within the `agent_client_protocol` crate
///
/// # Example
///
/// ```ignore
/// #[derive(Debug, Clone, Serialize, Deserialize, JsonRpcResponse)]
/// struct HelloResponse {
///     greeting: String,
/// }
/// ```
#[proc_macro_derive(JsonRpcResponse, attributes(response))]
pub fn derive_json_rpc_response_payload(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;
    let (impl_generics, type_generics, where_clause) = input.generics.split_for_impl();

    let krate = match parse_response_attrs(&input) {
        Ok(attrs) => attrs,
        Err(e) => return e.to_compile_error().into(),
    };

    let expanded = quote! {
        #[automatically_derived]
        impl #impl_generics #krate::JsonRpcResponse for #name #type_generics #where_clause {
            fn into_json(self, _method: &str) -> ::core::result::Result<::serde_json::Value, #krate::Error> {
                ::serde_json::to_value(self).map_err(#krate::Error::into_internal_error)
            }

            fn from_value(_method: &str, value: ::serde_json::Value) -> ::core::result::Result<Self, #krate::Error> {
                #krate::util::json_cast(value)
            }
        }
    };

    TokenStream::from(expanded)
}

fn default_crate_path() -> Path {
    syn::parse_quote!(::agent_client_protocol)
}

fn parse_request_attrs(input: &DeriveInput) -> syn::Result<(LitStr, Type, Path)> {
    let mut method: Option<LitStr> = None;
    let mut response_type: Option<Type> = None;
    let mut krate: Option<Path> = None;

    for attr in &input.attrs {
        if !attr.path().is_ident("request") {
            continue;
        }

        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("method") {
                if method.is_some() {
                    return Err(meta.error("duplicate `method` attribute"));
                }
                let value: LitStr = meta.value()?.parse()?;
                method = Some(value);
                return Ok(());
            }

            if meta.path.is_ident("response") {
                if response_type.is_some() {
                    return Err(meta.error("duplicate `response` attribute"));
                }
                response_type = Some(meta.value()?.parse()?);
                return Ok(());
            }

            if meta.path.is_ident("crate") {
                if krate.is_some() {
                    return Err(meta.error("duplicate `crate` attribute"));
                }
                krate = Some(meta.value()?.parse()?);
                return Ok(());
            }

            Err(meta.error("unknown attribute"))
        })?;
    }

    let method = method.ok_or_else(|| {
        syn::Error::new_spanned(
            &input.ident,
            "missing required attribute: #[request(method = \"...\")]",
        )
    })?;

    let response_type = response_type.ok_or_else(|| {
        syn::Error::new_spanned(
            &input.ident,
            "missing required attribute: #[request(response = ...)]",
        )
    })?;

    Ok((
        method,
        response_type,
        krate.unwrap_or_else(default_crate_path),
    ))
}

fn parse_notification_attrs(input: &DeriveInput) -> syn::Result<(LitStr, Path)> {
    let mut method: Option<LitStr> = None;
    let mut krate: Option<Path> = None;

    for attr in &input.attrs {
        if !attr.path().is_ident("notification") {
            continue;
        }

        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("method") {
                if method.is_some() {
                    return Err(meta.error("duplicate `method` attribute"));
                }
                let value: LitStr = meta.value()?.parse()?;
                method = Some(value);
                return Ok(());
            }

            if meta.path.is_ident("crate") {
                if krate.is_some() {
                    return Err(meta.error("duplicate `crate` attribute"));
                }
                krate = Some(meta.value()?.parse()?);
                return Ok(());
            }

            Err(meta.error("unknown attribute"))
        })?;
    }

    let method = method.ok_or_else(|| {
        syn::Error::new_spanned(
            &input.ident,
            "missing required attribute: #[notification(method = \"...\")]",
        )
    })?;

    Ok((method, krate.unwrap_or_else(default_crate_path)))
}

fn parse_response_attrs(input: &DeriveInput) -> syn::Result<Path> {
    let mut krate: Option<Path> = None;

    for attr in &input.attrs {
        if !attr.path().is_ident("response") {
            continue;
        }

        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("crate") {
                if krate.is_some() {
                    return Err(meta.error("duplicate `crate` attribute"));
                }
                krate = Some(meta.value()?.parse()?);
                return Ok(());
            }

            Err(meta.error("unknown attribute"))
        })?;
    }

    Ok(krate.unwrap_or_else(default_crate_path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::quote;
    use syn::parse_quote;

    fn expect_error<T>(result: syn::Result<T>) -> syn::Error {
        match result {
            Ok(_) => panic!("expected attribute parsing to fail"),
            Err(error) => error,
        }
    }

    #[test]
    fn request_attributes_accept_rust_types() {
        let input = parse_quote! {
            #[request(
                method = "test/method",
                response = Result<Option<Response>, Error>,
                crate = crate::protocol
            )]
            struct Request;
        };

        let (method, response, krate) = parse_request_attrs(&input).unwrap();

        assert_eq!(method.value(), "test/method");
        assert_eq!(
            quote!(#response).to_string(),
            "Result < Option < Response > , Error >"
        );
        assert_eq!(quote!(#krate).to_string(), "crate :: protocol");
    }

    #[test]
    fn request_attributes_reject_duplicate_keys() {
        let input = parse_quote! {
            #[request(method = "first", method = "second", response = Response)]
            struct Request;
        };

        let error = expect_error(parse_request_attrs(&input));

        assert_eq!(error.to_string(), "duplicate `method` attribute");
    }

    #[test]
    fn notification_attributes_reject_duplicate_keys_across_attributes() {
        let input = parse_quote! {
            #[notification(method = "test/method")]
            #[notification(method = "test/other")]
            struct Notification;
        };

        let error = expect_error(parse_notification_attrs(&input));

        assert_eq!(error.to_string(), "duplicate `method` attribute");
    }

    #[test]
    fn response_attributes_reject_duplicate_crate_paths() {
        let input = parse_quote! {
            #[response(crate = crate, crate = agent_client_protocol)]
            struct Response;
        };

        let error = expect_error(parse_response_attrs(&input));

        assert_eq!(error.to_string(), "duplicate `crate` attribute");
    }
}
