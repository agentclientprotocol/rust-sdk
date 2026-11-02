//! JSON-RPC implementations for the unstable native MCP-over-ACP transport.

use crate::v1::schema::{MessageMcpNotification, MessageMcpRequest, MessageMcpResponse};

impl_jsonrpc_request!(MessageMcpRequest, MessageMcpResponse, "mcp/message");
impl_jsonrpc_notification!(MessageMcpNotification, "mcp/message");
