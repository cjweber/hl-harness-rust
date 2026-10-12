pub mod client;
pub use client::{
    add_mcp_server, load_mcp_servers, remove_mcp_server, sanitize_mcp_name, save_mcp_servers, McpManager,
    McpOAuthConfig, McpOAuthProvider, McpServerConfig, McpTool, OAuthTokens,
};
