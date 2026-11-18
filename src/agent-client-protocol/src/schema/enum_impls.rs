//! SDK trait adapters for the ACP enums. Method dispatch lives in the schema.

use crate::v1::schema::{
    AgentNotification, AgentRequest, AgentResponse, ClientNotification, ClientRequest,
    ClientResponse,
};

impl_jsonrpc_request_enum!(ClientRequest);
impl_jsonrpc_response_enum!(AgentResponse);
impl_jsonrpc_notification_enum!(ClientNotification);

impl_jsonrpc_request_enum!(AgentRequest);
impl_jsonrpc_response_enum!(ClientResponse);
impl_jsonrpc_notification_enum!(AgentNotification);
