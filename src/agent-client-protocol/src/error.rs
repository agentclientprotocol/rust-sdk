//! The SDK's protocol-version-neutral JSON-RPC error.
//!
//! Every SDK callback and connection method uses [`Error`], whichever ACP
//! version the connection speaks. It holds exactly what a JSON-RPC error
//! object carries on the wire (a code, a message and optional data), so it
//! converts losslessly to and from [`schema::v1::Error`] and
//! `schema::v2::Error`.
//!
//! Constructors for codes that mean the same in every protocol version live
//! here. Constructors whose error data differs between versions stay on the
//! schema types; convert their result with `.into()`:
//!
//! ```
//! use agent_client_protocol::{Error, schema::v1};
//!
//! let missing: Error = v1::Error::resource_not_found(Some("file:///a.txt".into())).into();
//! assert_eq!(missing.code, v1::ErrorCode::ResourceNotFound);
//! ```
//!
//! [`schema::v1::Error`]: crate::schema::v1::Error

use std::fmt;

use serde::{Deserialize, Serialize};

#[cfg(feature = "unstable_protocol_v2")]
use crate::schema::v2;
use crate::schema::{IntoOption, v1};

/// Result type whose error defaults to the SDK's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A JSON-RPC error code.
///
/// This is the integer on the wire. It is not tied to one ACP version: it
/// can hold any `i32`, converts to and from `i32`, and converts losslessly to
/// and from [`v1::ErrorCode`] and `v2::ErrorCode`. It can also be compared
/// with either schema enum directly:
///
/// ```
/// use agent_client_protocol::{Error, ErrorCode, schema::v1};
///
/// let error = Error::invalid_params();
/// assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
/// assert_eq!(error.code, v1::ErrorCode::InvalidParams);
/// assert_eq!(i32::from(error.code), -32602);
/// ```
///
/// The associated constants cover the codes that every ACP version defines
/// the same way. Codes defined by only some versions, such as v2's `-32022`
/// (Unsupported protocol version), are on that version's `ErrorCode`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(transparent)]
#[repr(transparent)]
pub struct ErrorCode(i32);

impl ErrorCode {
    /// `-32700`: invalid JSON was received.
    pub const PARSE_ERROR: Self = Self(-32700);
    /// `-32600`: the JSON sent is not a valid request object.
    pub const INVALID_REQUEST: Self = Self(-32600);
    /// `-32601`: the method does not exist or is not available.
    pub const METHOD_NOT_FOUND: Self = Self(-32601);
    /// `-32602`: invalid method parameters.
    pub const INVALID_PARAMS: Self = Self(-32602);
    /// `-32603`: internal JSON-RPC error.
    pub const INTERNAL_ERROR: Self = Self(-32603);
    /// `-32800`: the request was cancelled.
    pub const REQUEST_CANCELLED: Self = Self(-32800);
    /// `-32000`: authentication is required first.
    pub const AUTH_REQUIRED: Self = Self(-32000);
    /// `-32002`: a resource, such as a session or file, was not found.
    ///
    /// The error data for this code differs between protocol versions; build
    /// it with the schema types' constructors.
    pub const RESOURCE_NOT_FOUND: Self = Self(-32002);

    /// Wraps a raw JSON-RPC error code.
    #[must_use]
    pub const fn new(code: i32) -> Self {
        Self(code)
    }

    /// The raw JSON-RPC error code.
    #[must_use]
    pub const fn value(self) -> i32 {
        self.0
    }

    /// The default message for a code shared by every protocol version.
    fn name(self) -> Option<&'static str> {
        Some(match self {
            Self::PARSE_ERROR => "Parse error",
            Self::INVALID_REQUEST => "Invalid request",
            Self::METHOD_NOT_FOUND => "Method not found",
            Self::INVALID_PARAMS => "Invalid params",
            Self::INTERNAL_ERROR => "Internal error",
            Self::REQUEST_CANCELLED => "Request cancelled",
            Self::AUTH_REQUIRED => "Authentication required",
            Self::RESOURCE_NOT_FOUND => "Resource not found",
            _ => return None,
        })
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name().unwrap_or("Unknown error"))
    }
}

impl fmt::Debug for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {self}", self.0)
    }
}

impl From<i32> for ErrorCode {
    fn from(code: i32) -> Self {
        Self(code)
    }
}

impl From<ErrorCode> for i32 {
    fn from(code: ErrorCode) -> Self {
        code.0
    }
}

macro_rules! schema_error_code_conversions {
    ($version:ident) => {
        impl From<$version::ErrorCode> for ErrorCode {
            fn from(code: $version::ErrorCode) -> Self {
                Self(code.into())
            }
        }

        impl From<ErrorCode> for $version::ErrorCode {
            fn from(code: ErrorCode) -> Self {
                code.0.into()
            }
        }

        impl PartialEq<$version::ErrorCode> for ErrorCode {
            fn eq(&self, other: &$version::ErrorCode) -> bool {
                self.0 == i32::from(*other)
            }
        }

        impl PartialEq<ErrorCode> for $version::ErrorCode {
            fn eq(&self, other: &ErrorCode) -> bool {
                i32::from(*self) == other.0
            }
        }

        impl From<$version::Error> for Error {
            fn from(error: $version::Error) -> Self {
                Self {
                    code: error.code.into(),
                    message: error.message,
                    data: error.data,
                }
            }
        }

        impl From<Error> for $version::Error {
            fn from(error: Error) -> Self {
                $version::Error::new(error.code.0, error.message).data(error.data)
            }
        }
    };
}

schema_error_code_conversions!(v1);
#[cfg(feature = "unstable_protocol_v2")]
schema_error_code_conversions!(v2);

/// A JSON-RPC error object, independent of the ACP protocol version.
///
/// This is the error type of every SDK callback and connection method. An
/// error returned from a request handler is sent to the peer as an Error
/// Response with exactly this code, message and data.
///
/// Build errors with the shared constructors below, or with a schema
/// version's constructors followed by `.into()` when the error data differs
/// between versions:
///
/// | Code | Constructor |
/// |------|-------------|
/// | `-32700` | [`Error::parse_error`] |
/// | `-32600` | [`Error::invalid_request`] |
/// | `-32601` | [`Error::method_not_found`] |
/// | `-32602` | [`Error::invalid_params`] |
/// | `-32603` | [`Error::internal_error`], [`Error::into_internal_error`] |
/// | `-32800` | [`Error::request_cancelled`] |
/// | `-32000` | [`Error::auth_required`] |
/// | `-32002` | `v1::Error::resource_not_found(uri)`, `v1::Error::session_not_found(id)`, `v2::Error::resource_not_found()`, `v2::Error::session_not_found(id)` |
/// | `-32022` | `v2::Error::unsupported_protocol_version(requested, supported)` |
///
/// Convert back with `v1::Error::from(error)` or `v2::Error::from(error)` to
/// use a schema version's view of the error. Typed v2 error data is read
/// with `serde_json`:
///
/// ```
/// # #[cfg(feature = "unstable_protocol_v2")] {
/// use agent_client_protocol::{Error, schema::v2};
///
/// let error: Error = v2::Error::session_not_found("sess_1").into();
/// if error.code == v2::ErrorCode::ResourceNotFound
///     && let Some(data) = error.data.clone()
///     && let Ok(data) = serde_json::from_value::<v2::SessionNotFoundErrorData>(data)
/// {
///     assert_eq!(data.session_id.0.as_ref(), "sess_1");
/// }
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[non_exhaustive]
pub struct Error {
    /// The JSON-RPC error code.
    pub code: ErrorCode,
    /// A short description of the error.
    pub message: String,
    /// Optional additional information about the error.
    ///
    /// `Some(Value::Null)` is sent as an explicit `"data": null`; `None`
    /// omits the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl Error {
    /// Creates an error with the given code and message and no data.
    #[must_use]
    pub fn new(code: impl Into<ErrorCode>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            data: None,
        }
    }

    /// Sets the error's data, replacing any existing data.
    #[must_use]
    pub fn data(mut self, data: impl IntoOption<serde_json::Value>) -> Self {
        self.data = data.into_option();
        self
    }

    /// `-32700`: invalid JSON was received.
    #[must_use]
    pub fn parse_error() -> Self {
        ErrorCode::PARSE_ERROR.into()
    }

    /// `-32600`: the JSON sent is not a valid request object.
    #[must_use]
    pub fn invalid_request() -> Self {
        ErrorCode::INVALID_REQUEST.into()
    }

    /// `-32601`: the method does not exist or is not available.
    #[must_use]
    pub fn method_not_found() -> Self {
        ErrorCode::METHOD_NOT_FOUND.into()
    }

    /// `-32602`: invalid method parameters.
    #[must_use]
    pub fn invalid_params() -> Self {
        ErrorCode::INVALID_PARAMS.into()
    }

    /// `-32603`: internal JSON-RPC error.
    #[must_use]
    pub fn internal_error() -> Self {
        ErrorCode::INTERNAL_ERROR.into()
    }

    /// `-32800`: the request was cancelled, either by the caller or because
    /// of resource constraints or shutdown.
    #[must_use]
    pub fn request_cancelled() -> Self {
        ErrorCode::REQUEST_CANCELLED.into()
    }

    /// `-32000`: authentication is required before this operation.
    #[must_use]
    pub fn auth_required() -> Self {
        ErrorCode::AUTH_REQUIRED.into()
    }

    /// Converts any error into an internal error (`-32603`) whose data is the
    /// error's string representation.
    #[must_use]
    pub fn into_internal_error(err: impl std::error::Error) -> Self {
        Self::internal_error().data(err.to_string())
    }
}

impl From<ErrorCode> for Error {
    /// An error with the code's default message and no data.
    fn from(code: ErrorCode) -> Self {
        Self::new(code, code.to_string())
    }
}

impl std::error::Error for Error {}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.message.is_empty() {
            write!(f, "{}", self.code.0)?;
        } else {
            write!(f, "{}", self.message)?;
        }

        if let Some(data) = &self.data {
            let pretty = serde_json::to_string_pretty(data).unwrap_or_else(|_| data.to_string());
            write!(f, ": {pretty}")?;
        }

        Ok(())
    }
}

impl From<anyhow::Error> for Error {
    /// Recovers an SDK or schema error wrapped in the `anyhow::Error`, and
    /// converts anything else into an internal error.
    fn from(error: anyhow::Error) -> Self {
        let error = match error.downcast::<Self>() {
            Ok(error) => return error,
            Err(error) => error,
        };
        let error = match error.downcast::<v1::Error>() {
            Ok(error) => return error.into(),
            Err(error) => error,
        };
        #[cfg(feature = "unstable_protocol_v2")]
        let error = match error.downcast::<v2::Error>() {
            Ok(error) => return error.into(),
            Err(error) => error,
        };
        Self::into_internal_error(&*error)
    }
}

impl From<serde_json::Error> for Error {
    /// An invalid params error (`-32602`) whose data is the serde message.
    fn from(error: serde_json::Error) -> Self {
        Self::invalid_params().data(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn sample_codes() -> [i32; 13] {
        [
            -32700,
            -32600,
            -32601,
            -32602,
            -32603,
            -32800,
            -32000,
            -32002,
            -32022,
            0,
            12345,
            i32::MIN,
            i32::MAX,
        ]
    }

    fn sample_data() -> [Option<Value>; 6] {
        [
            None,
            Some(Value::Null),
            Some(json!("text")),
            Some(json!(42)),
            Some(json!([1, null, { "a": [] }])),
            Some(json!({ "sessionId": "s", "_meta": { "x": true }, "extra": null })),
        ]
    }

    fn samples() -> impl Iterator<Item = Error> {
        sample_codes().into_iter().flat_map(|code| {
            sample_data().into_iter().flat_map(move |data| {
                ["", "message", "Unknown error"].map(|message| Error {
                    code: code.into(),
                    message: message.into(),
                    data: data.clone(),
                })
            })
        })
    }

    #[test]
    fn v1_round_trip_is_lossless() {
        for error in samples() {
            let schema = v1::Error::from(error.clone());
            assert_eq!(i32::from(schema.code), error.code.value());
            assert_eq!(schema.message, error.message);
            assert_eq!(schema.data, error.data);
            assert_eq!(Error::from(schema.clone()), error);
            assert_eq!(v1::Error::from(Error::from(schema.clone())), schema);
            assert_eq!(error.to_string(), schema.to_string());
        }
    }

    #[cfg(feature = "unstable_protocol_v2")]
    #[test]
    fn v2_round_trip_is_lossless() {
        for error in samples() {
            let schema = v2::Error::from(error.clone());
            assert_eq!(i32::from(schema.code), error.code.value());
            assert_eq!(schema.message, error.message);
            assert_eq!(schema.data, error.data);
            assert_eq!(Error::from(schema.clone()), error);
            assert_eq!(v2::Error::from(Error::from(schema.clone())), schema);
            assert_eq!(error.to_string(), schema.to_string());
        }
    }

    #[cfg(feature = "unstable_protocol_v2")]
    #[test]
    fn v1_and_v2_convert_through_the_sdk_error() {
        for error in samples() {
            let v1 = v1::Error::from(error.clone());
            let v2 = v2::Error::from(Error::from(v1));
            assert_eq!(Error::from(v2), error);
        }
    }

    #[test]
    fn serializes_like_the_schema_errors() {
        for error in samples() {
            let json = serde_json::to_value(&error).unwrap();
            assert_eq!(
                json,
                serde_json::to_value(v1::Error::from(error.clone())).unwrap()
            );
            #[cfg(feature = "unstable_protocol_v2")]
            assert_eq!(
                json,
                serde_json::to_value(v2::Error::from(error.clone())).unwrap()
            );
            let parsed: Error = serde_json::from_value(json.clone()).unwrap();
            let schema: v1::Error = serde_json::from_value(json).unwrap();
            assert_eq!(parsed, Error::from(schema));
        }
    }

    #[test]
    fn shared_constructors_match_both_schema_versions() {
        let pairs = [
            (Error::parse_error(), v1::Error::parse_error()),
            (Error::invalid_request(), v1::Error::invalid_request()),
            (Error::method_not_found(), v1::Error::method_not_found()),
            (Error::invalid_params(), v1::Error::invalid_params()),
            (Error::internal_error(), v1::Error::internal_error()),
            (Error::request_cancelled(), v1::Error::request_cancelled()),
            (Error::auth_required(), v1::Error::auth_required()),
            (
                Error::into_internal_error(std::io::Error::other("io")),
                v1::Error::into_internal_error(std::io::Error::other("io")),
            ),
        ];
        for (sdk, schema) in pairs {
            assert_eq!(sdk, Error::from(schema));
        }
        #[cfg(feature = "unstable_protocol_v2")]
        {
            let pairs = [
                (Error::parse_error(), v2::Error::parse_error()),
                (Error::invalid_request(), v2::Error::invalid_request()),
                (Error::method_not_found(), v2::Error::method_not_found()),
                (Error::invalid_params(), v2::Error::invalid_params()),
                (Error::internal_error(), v2::Error::internal_error()),
                (Error::request_cancelled(), v2::Error::request_cancelled()),
                (Error::auth_required(), v2::Error::auth_required()),
            ];
            for (sdk, schema) in pairs {
                assert_eq!(sdk, Error::from(schema));
            }
        }
    }

    #[test]
    fn error_codes_compare_with_schema_codes() {
        for code in sample_codes() {
            let sdk = ErrorCode::from(code);
            let v1 = v1::ErrorCode::from(code);
            assert_eq!(sdk, v1);
            assert_eq!(v1, sdk);
            assert_eq!(ErrorCode::from(v1), sdk);
            assert_eq!(v1::ErrorCode::from(sdk), v1);
            assert_eq!(format!("{sdk:?}"), format!("{v1:?}"));
            assert_eq!(sdk.to_string(), v1.to_string());
            #[cfg(feature = "unstable_protocol_v2")]
            {
                let v2 = v2::ErrorCode::from(code);
                assert_eq!(sdk, v2);
                assert_eq!(ErrorCode::from(v2), sdk);
                assert_eq!(v2::ErrorCode::from(sdk), v2);
            }
        }
        assert_ne!(ErrorCode::INVALID_PARAMS, v1::ErrorCode::InvalidRequest);
    }

    #[cfg(feature = "unstable_protocol_v2")]
    #[test]
    fn typed_v2_data_survives_conversion() {
        let error: Error = v2::Error::unsupported_protocol_version(
            crate::schema::ProtocolVersion::V2,
            &[crate::schema::ProtocolVersion::V1],
        )
        .into();
        assert_eq!(error.code, v2::ErrorCode::UnsupportedProtocolVersion);
        let data: v2::UnsupportedProtocolVersionErrorData =
            serde_json::from_value(v2::Error::from(error).data.unwrap()).unwrap();
        assert_eq!(
            data,
            v2::UnsupportedProtocolVersionErrorData::new(
                crate::schema::ProtocolVersion::V2,
                vec![crate::schema::ProtocolVersion::V1],
            )
        );
    }

    #[test]
    fn anyhow_recovers_wrapped_errors() {
        let error = Error::new(12345, "custom").data(json!({ "kept": true }));
        assert_eq!(Error::from(anyhow::Error::new(error.clone())), error);
        let schema = v1::Error::new(-32002, "missing").data(Value::Null);
        assert_eq!(
            Error::from(anyhow::Error::new(schema.clone())),
            Error::from(schema)
        );
        #[cfg(feature = "unstable_protocol_v2")]
        {
            let schema = v2::Error::session_not_found("sess_1");
            assert_eq!(
                Error::from(anyhow::Error::new(schema.clone())),
                Error::from(schema)
            );
        }
        assert_eq!(
            Error::from(anyhow::anyhow!("boom")),
            Error::internal_error().data("boom")
        );
    }
}
