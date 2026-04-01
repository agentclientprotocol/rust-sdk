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
//! #[derive(Debug, Serialize, Deserialize, JsonRpcResponse)]
//! #[response(method = "_hello")]
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
use syn::{DeriveInput, Expr, Lit, Path, Type, parse_macro_input};

/// Derive macro for implementing `JsonRpcRequest` and `JsonRpcMessage` traits.
///
/// # Attributes
///
/// - `#[request(method = "method_name", response = ResponseType)]`
/// - `#[request(method = "method_name", response = ResponseType, crate = crate)]` - for use within sacp
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

    // Parse attributes
    let (method, response_type, krate) = match parse_request_attrs(&input) {
        Ok(attrs) => attrs,
        Err(e) => return e.to_compile_error().into(),
    };

    let expanded = quote! {
        impl #krate::JsonRpcMessage for #name {
            fn matches_method(method: &str) -> bool {
                method == #method
            }

            fn method(&self) -> &str {
                #method
            }

            fn to_untyped_message(&self) -> Result<#krate::UntypedMessage, #krate::Error> {
                #krate::UntypedMessage::new(#method, self)
            }

            fn parse_message(
                method: &str,
                params: &impl serde::Serialize,
            ) -> Result<Self, #krate::Error> {
                if method != #method {
                    return Err(#krate::Error::method_not_found());
                }
                #krate::util::json_cast(params)
            }
        }

        impl #krate::JsonRpcRequest for #name {
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
/// - `#[notification(method = "method_name", crate = crate)]` - for use within sacp
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

    // Parse attributes
    let (method, krate) = match parse_notification_attrs(&input) {
        Ok(attrs) => attrs,
        Err(e) => return e.to_compile_error().into(),
    };

    let expanded = quote! {
        impl #krate::JsonRpcMessage for #name {
            fn matches_method(method: &str) -> bool {
                method == #method
            }

            fn method(&self) -> &str {
                #method
            }

            fn to_untyped_message(&self) -> Result<#krate::UntypedMessage, #krate::Error> {
                #krate::UntypedMessage::new(#method, self)
            }

            fn parse_message(
                method: &str,
                params: &impl serde::Serialize,
            ) -> Result<Self, #krate::Error> {
                if method != #method {
                    return Err(#krate::Error::method_not_found());
                }
                #krate::util::json_cast(params)
            }
        }

        impl #krate::JsonRpcNotification for #name {}
    };

    TokenStream::from(expanded)
}

/// Derive macro for implementing `JsonRpcResponse` trait.
///
/// # Attributes
///
/// - `#[response(crate = crate)]` - for use within sacp crate
///
/// # Example
///
/// ```ignore
/// #[derive(Debug, Serialize, Deserialize, JsonRpcResponse)]
/// struct HelloResponse {
///     greeting: String,
/// }
/// ```
#[proc_macro_derive(JsonRpcResponse, attributes(response))]
pub fn derive_json_rpc_response_payload(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    let krate = match parse_response_attrs(&input) {
        Ok(attrs) => attrs,
        Err(e) => return e.to_compile_error().into(),
    };

    let expanded = quote! {
        impl #krate::JsonRpcResponse for #name {
            fn into_json(self, _method: &str) -> Result<serde_json::Value, #krate::Error> {
                serde_json::to_value(self).map_err(#krate::Error::into_internal_error)
            }

            fn from_value(_method: &str, value: serde_json::Value) -> Result<Self, #krate::Error> {
                #krate::util::json_cast(value)
            }
        }
    };

    TokenStream::from(expanded)
}

fn default_crate_path() -> Path {
    syn::parse_quote!(agent_client_protocol_core)
}

fn parse_request_attrs(input: &DeriveInput) -> syn::Result<(String, Type, Path)> {
    let mut method: Option<String> = None;
    let mut response_type: Option<Type> = None;
    let mut krate: Option<Path> = None;

    for attr in &input.attrs {
        if !attr.path().is_ident("request") {
            continue;
        }

        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("method") {
                let value: Expr = meta.value()?.parse()?;
                if let Expr::Lit(expr_lit) = value
                    && let Lit::Str(lit_str) = expr_lit.lit
                {
                    method = Some(lit_str.value());
                    return Ok(());
                }
                return Err(meta.error("expected string literal for method"));
            }

            if meta.path.is_ident("response") {
                let value: Expr = meta.value()?.parse()?;
                if let Expr::Path(expr_path) = value {
                    response_type = Some(Type::Path(syn::TypePath {
                        qself: None,
                        path: expr_path.path,
                    }));
                    return Ok(());
                }
                return Err(meta.error("expected type for response"));
            }

            if meta.path.is_ident("crate") {
                let value: Expr = meta.value()?.parse()?;
                if let Expr::Path(expr_path) = value {
                    krate = Some(expr_path.path);
                    return Ok(());
                }
                return Err(meta.error("expected path for crate"));
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

fn parse_notification_attrs(input: &DeriveInput) -> syn::Result<(String, Path)> {
    let mut method: Option<String> = None;
    let mut krate: Option<Path> = None;

    for attr in &input.attrs {
        if !attr.path().is_ident("notification") {
            continue;
        }

        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("method") {
                let value: Expr = meta.value()?.parse()?;
                if let Expr::Lit(expr_lit) = value
                    && let Lit::Str(lit_str) = expr_lit.lit
                {
                    method = Some(lit_str.value());
                    return Ok(());
                }
                return Err(meta.error("expected string literal for method"));
            }

            if meta.path.is_ident("crate") {
                let value: Expr = meta.value()?.parse()?;
                if let Expr::Path(expr_path) = value {
                    krate = Some(expr_path.path);
                    return Ok(());
                }
                return Err(meta.error("expected path for crate"));
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
                let value: Expr = meta.value()?.parse()?;
                if let Expr::Path(expr_path) = value {
                    krate = Some(expr_path.path);
                    return Ok(());
                }
                return Err(meta.error("expected path for crate"));
            }

            Err(meta.error("unknown attribute"))
        })?;
    }

    Ok(krate.unwrap_or_else(default_crate_path))
}
