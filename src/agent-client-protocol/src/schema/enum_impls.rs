//! JsonRpcMessage and JsonRpcNotification/JsonRpcRequest implementations for
//! the ACP enum types from agent-client-protocol-schema.

// Agent side (messages that agents receive).
impl_client_request_enum!();
impl_client_notification_enum!();

// Client side (messages that clients/editors receive).
impl_agent_request_enum!();
impl_agent_notification_enum!();
