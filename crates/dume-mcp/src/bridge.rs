use crate::client::McpClient;
use crate::types::{CallToolResult, McpTool};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

pub struct McpToolBridge {
    servers: HashMap<String, Arc<McpClient>>,
    bridged_tools: HashMap<String, (String, String)>, // namespaced_name -> (server_name, tool_name)
}

impl Default for McpToolBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl McpToolBridge {
    pub fn new() -> Self {
        Self {
            servers: HashMap::new(),
            bridged_tools: HashMap::new(),
        }
    }

    pub fn register_server(&mut self, server_name: &str, client: Arc<McpClient>) {
        self.servers.insert(server_name.to_string(), client);
    }

    pub async fn discover_tools(&mut self, server_name: &str) -> Result<Vec<McpTool>> {
        let client = self
            .servers
            .get(server_name)
            .context("Server not registered")?;

        let raw_tools = client.list_tools().await?;
        let mut namespaced = Vec::new();

        for tool in raw_tools {
            let namespaced_name = format!("mcp__{}__{}", server_name, tool.name);
            self.bridged_tools
                .insert(namespaced_name.clone(), (server_name.to_string(), tool.name.clone()));

            namespaced.push(McpTool {
                name: namespaced_name,
                description: tool.description,
                input_schema: tool.input_schema,
            });
        }

        Ok(namespaced)
    }

    pub async fn execute_tool(&self, namespaced_name: &str, arguments: Value) -> Result<CallToolResult> {
        let (server_name, tool_name) = self
            .bridged_tools
            .get(namespaced_name)
            .context("Unrecognized bridged tool name")?;

        let client = self
            .servers
            .get(server_name)
            .context("Server not registered")?;

        client.call_tool(tool_name, arguments).await
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn test_namespacing_contract() {
        let server = "sqlite";
        let tool = "query";
        let namespaced = format!("mcp__{}__{}", server, tool);
        assert_eq!(namespaced, "mcp__sqlite__query");

        let parts: Vec<&str> = namespaced.split("__").collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "mcp");
        assert_eq!(parts[1], "sqlite");
        assert_eq!(parts[2], "query");
    }
}
