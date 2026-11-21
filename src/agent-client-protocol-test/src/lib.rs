//! Fixtures for the Rust SDK's tests and documentation examples.
//!
//! The request and notification types here are example JSON-RPC methods,
//! not ACP methods. For tests that need an agent that answers ACP requests,
//! use [`testy`].

use std::future::{Future, Ready, ready};

use agent_client_protocol::{
    Builder, Client, ConnectTo, Error, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest,
    JsonRpcResponse, Result, Role, UntypedMessage,
};
use serde::{Deserialize, Serialize};

pub mod arrow_proxy;
pub mod test_binaries;
pub mod testy;

/// A mock transport for doctests that panics if actually used.
/// This is only for documentation examples that don't actually run.
#[derive(Debug)]
pub struct MockTransport;

impl<R: Role> ConnectTo<R> for MockTransport {
    fn connect_to(
        self,
        _client: impl ConnectTo<R::Counterpart>,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        std::future::poll_fn(|_| {
            panic!("MockTransport should never be used in running code - it's only for doctests")
        })
    }
}

/// Parameterless `myRequest` used in request-handler examples.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MyRequest {}

/// Reply to [`MyRequest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MyResponse {
    /// Status text chosen by the handler.
    pub status: String,
}

/// Asks the example handler to process some text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessRequest {
    /// Input to the operation.
    pub data: String,
}

/// Reply to [`ProcessRequest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessResponse {
    /// Text returned by the processing handler.
    pub result: String,
}

/// Notification that the processing handler has started work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessStarted {}

/// Requests an analysis that can finish after the handler replies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalyzeRequest {
    /// Text to analyze.
    pub data: String,
}

/// Immediate reply to [`AnalyzeRequest`]; [`AnalysisComplete`] carries the result later.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisStarted {
    /// Identifier assigned to the background job.
    pub job_id: u32,
}

/// Looks up an item by ID in the response-callback examples.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryRequest {
    /// ID of the item to look up, not the JSON-RPC request ID.
    pub id: u64,
}

/// Reply to [`QueryRequest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResponse {
    /// Data returned for the requested item.
    pub data: String,
}

/// Asks the example validation handler to check some text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidateRequest {
    /// Input to validate.
    pub data: String,
}

/// Validation outcome returned by the handler for [`ValidateRequest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidateResponse {
    /// Whether the input passed validation.
    pub is_valid: bool,
    /// Explanation of a validation failure, when provided.
    pub error: Option<String>,
}

/// Parameterless operation used as the next request in a chain of handlers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteRequest {}

/// Reply to [`ExecuteRequest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteResponse {
    /// Output of the example operation.
    pub result: String,
}

/// A second request type for examples that send a request from inside a handler.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OtherRequest {}

/// Reply to [`OtherRequest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OtherResponse {
    /// Value passed back to the calling handler.
    pub value: String,
}

/// Wraps a request for an example proxy to forward.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyRequest {
    /// Method and params of the request being forwarded.
    pub inner_request: UntypedMessage,
}

/// Parameterless `sessionUpdate` notification for dispatch examples.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionUpdate {}

/// Reports progress while a handler is working.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusUpdate {
    /// Human-readable progress message.
    pub message: String,
}

/// Reports that a background processing task finished.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessComplete {
    /// Output of the completed task.
    pub result: String,
}

/// Delivers the result of an analysis acknowledged by [`AnalysisStarted`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisComplete {
    /// Output of the analysis.
    pub result: String,
}

/// Signals that the callback has finished handling a [`QueryResponse`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryComplete {}

// Implement JsonRpcMessage for all types
macro_rules! impl_jr_message {
    ($type:ty, $method:expr) => {
        impl JsonRpcMessage for $type {
            fn matches_method(method: &str) -> bool {
                method == $method
            }
            fn method(&self) -> &str {
                $method
            }
            fn to_untyped_message(&self) -> Result<UntypedMessage, crate::Error> {
                UntypedMessage::new($method, self)
            }
            fn parse_message(
                method: &str,
                params: &serde_json::Value,
            ) -> Result<Self, crate::Error> {
                if !Self::matches_method(method) {
                    return Err(crate::Error::method_not_found());
                }
                agent_client_protocol::util::json_cast_params(params)
            }
        }
    };
}

// Implement JsonRpcRequest for request types
macro_rules! impl_jr_request {
    ($req:ty, $resp:ty, $method:expr) => {
        impl_jr_message!($req, $method);
        impl JsonRpcRequest for $req {
            type Response = $resp;
        }
    };
}

// Implement JsonRpcNotification for notification types
macro_rules! impl_jr_notification {
    ($type:ty, $method:expr) => {
        impl_jr_message!($type, $method);
        impl JsonRpcNotification for $type {}
    };
}

impl_jr_request!(MyRequest, MyResponse, "myRequest");
impl_jr_request!(ProcessRequest, ProcessResponse, "processRequest");
impl_jr_request!(AnalyzeRequest, AnalysisStarted, "analyzeRequest");
impl_jr_request!(QueryRequest, QueryResponse, "queryRequest");
impl_jr_request!(ValidateRequest, ValidateResponse, "validateRequest");
impl_jr_request!(ExecuteRequest, ExecuteResponse, "executeRequest");
impl_jr_request!(OtherRequest, OtherResponse, "otherRequest");
impl_jr_request!(ProxyRequest, serde_json::Value, "proxyRequest");

impl_jr_notification!(SessionUpdate, "sessionUpdate");
impl_jr_notification!(StatusUpdate, "statusUpdate");
impl_jr_notification!(ProcessComplete, "processComplete");
impl_jr_notification!(AnalysisComplete, "analysisComplete");
impl_jr_notification!(QueryComplete, "queryComplete");
impl_jr_notification!(ProcessStarted, "processStarted");

// Implement JsonRpcResponse for response types
macro_rules! impl_jr_response_payload {
    ($type:ty, $method:expr) => {
        impl JsonRpcResponse for $type {
            fn into_json(self, _method: &str) -> Result<serde_json::Value, crate::Error> {
                Ok(serde_json::to_value(self)?)
            }
            fn from_json(_method: &str, value: serde_json::Value) -> Result<Self, crate::Error> {
                Ok(serde_json::from_value(value)?)
            }
        }
    };
}

impl_jr_response_payload!(MyResponse, "myRequest");
impl_jr_response_payload!(ProcessResponse, "processRequest");
impl_jr_response_payload!(AnalysisStarted, "analyzeRequest");
impl_jr_response_payload!(QueryResponse, "queryRequest");
impl_jr_response_payload!(ValidateResponse, "validateRequest");
impl_jr_response_payload!(ExecuteResponse, "executeRequest");
impl_jr_response_payload!(OtherResponse, "otherRequest");

/// Stands in for async analysis and immediately returns `"analysis result"`.
pub fn expensive_analysis(_data: &str) -> Ready<Result<String, crate::Error>> {
    ready(Ok("analysis result".into()))
}

/// Stands in for async processing and immediately returns `"operation result"`.
pub fn expensive_operation(_data: &str) -> Ready<Result<String, crate::Error>> {
    ready(Ok("operation result".into()))
}

/// Stand-in for a fallible state update in documentation examples.
///
/// # Errors
///
/// This mock always succeeds.
pub fn update_session_state(_update: &SessionUpdate) -> Result<(), crate::Error> {
    Ok(())
}

/// Stand-in for fallible request processing in documentation examples.
///
/// # Errors
///
/// This mock always returns a copy of `data`.
pub fn process(data: &str) -> Result<String, crate::Error> {
    Ok(data.to_string())
}

/// Returns a v1 client builder for examples. No transport is connected.
pub fn mock_connection() -> Builder<
    Client,
    agent_client_protocol::NullHandler,
    agent_client_protocol::NullRun,
    agent_client_protocol::NullClose,
    agent_client_protocol::V1,
> {
    Client.v1()
}

/// Supplies placeholder values in examples that are compiled but not run.
pub trait Make {
    /// Stands in for constructing a value of this type.
    ///
    /// # Panics
    ///
    /// Always panics if the example actually evaluates this call.
    fn make() -> Self;
}

impl<T> Make for T {
    fn make() -> Self {
        panic!()
    }
}
