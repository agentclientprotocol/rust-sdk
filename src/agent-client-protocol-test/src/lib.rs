use agent_client_protocol::*;
use serde::{Deserialize, Serialize};

pub mod arrow_proxy;
pub mod test_binaries;
pub mod testy;

/// A mock transport for doctests that panics if actually used.
/// This is only for documentation examples that don't actually run.
#[derive(Debug)]
pub struct MockTransport;

impl<R: Role> ConnectTo<R> for MockTransport {
    async fn connect_to(self, _client: impl ConnectTo<R::Counterpart>) -> Result<(), Error> {
        panic!("MockTransport should never be used in running code - it's only for doctests")
    }
}

// Mock request/response types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MyRequest {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MyResponse {
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessRequest {
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessResponse {
    pub result: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessStarted {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalyzeRequest {
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisStarted {
    pub job_id: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryRequest {
    pub id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResponse {
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidateRequest {
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidateResponse {
    pub is_valid: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteRequest {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteResponse {
    pub result: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OtherRequest {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OtherResponse {
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyRequest {
    pub inner_request: UntypedMessage,
}

// Mock notification types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionUpdate {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusUpdate {
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessComplete {
    pub result: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisComplete {
    pub result: String,
}

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
            fn parse_message(method: &str, params: &impl Serialize) -> Result<Self, crate::Error> {
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
            fn from_value(_method: &str, value: serde_json::Value) -> Result<Self, crate::Error> {
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

// Mock async functions
#[expect(clippy::unused_async)]
pub async fn expensive_analysis(_data: &str) -> Result<String, crate::Error> {
    Ok("analysis result".into())
}

#[expect(clippy::unused_async)]
pub async fn expensive_operation(_data: &str) -> Result<String, crate::Error> {
    Ok("operation result".into())
}

pub fn update_session_state(_update: &SessionUpdate) -> Result<(), crate::Error> {
    Ok(())
}

pub fn process(data: &str) -> Result<String, crate::Error> {
    Ok(data.to_string())
}

// Helper to create a mock connection for examples
pub fn mock_connection() -> Builder<Client> {
    Client.builder()
}

pub trait Make {
    fn make() -> Self;
}

impl<T> Make for T {
    fn make() -> Self {
        panic!()
    }
}
