use crate::client::McpClient;
use crate::config::{McpConfig, McpConfigLoad, ProjectTrust, ToolExposure, TrustStore};
use crate::types::{CallToolResult, McpTool};
use anyhow::{Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerState {
    Disabled,
    Deferred,
    Connecting,
    Ready,
    Failed(String),
}
struct ServerEntry {
    client: Option<Arc<McpClient>>,
    config: crate::config::McpServerConfig,
    state: ServerState,
    tools: Vec<McpTool>,
}
pub struct McpToolBridge {
    servers: BTreeMap<String, ServerEntry>,
    routes: HashMap<String, (String, String)>,
    pub trust: TrustStore,
}
impl Default for McpToolBridge {
    fn default() -> Self {
        Self::new()
    }
}
impl McpToolBridge {
    pub fn new() -> Self {
        Self {
            servers: BTreeMap::new(),
            routes: HashMap::new(),
            trust: TrustStore::default(),
        }
    }
    pub fn register_server(&mut self, server_name: &str, client: Arc<McpClient>) {
        self.servers.insert(
            server_name.to_owned(),
            ServerEntry {
                client: Some(client),
                config: crate::config::McpServerConfig {
                    command: String::new(),
                    args: vec![],
                    env: BTreeMap::new(),
                    cwd: None,
                    enabled: true,
                    exposure: ToolExposure::Direct,
                    tool_overrides: BTreeMap::new(),
                    timeout_ms: 30_000,
                    output_limit_bytes: 1_048_576,
                },
                state: ServerState::Ready,
                tools: Vec::new(),
            },
        );
    }
    pub fn server_states(&self) -> Vec<(String, ServerState, usize)> {
        self.servers
            .iter()
            .map(|(name, e)| (name.clone(), e.state.clone(), e.tools.len()))
            .collect()
    }
    pub fn inspect_project_trust(&self, repo: &Path, digest: &str) -> Result<bool> {
        self.trust.inspect(repo, digest)
    }
    pub fn trust_project(
        &mut self,
        repo: &Path,
        digest: impl Into<String>,
    ) -> Result<ProjectTrust> {
        self.trust.trust(repo, digest)
    }

    pub async fn connect_config(
        &mut self,
        config: &McpConfig,
        project_repo: Option<&Path>,
    ) -> Result<()> {
        let loaded = McpConfigLoad {
            config: config.clone(),
            project_servers: if project_repo.is_some() {
                config.servers.keys().cloned().collect()
            } else {
                Default::default()
            },
        };
        self.connect_loaded_config(&loaded, project_repo).await
    }

    pub async fn connect_loaded_config(
        &mut self,
        loaded: &McpConfigLoad,
        project_repo: Option<&Path>,
    ) -> Result<()> {
        let config = &loaded.config;
        for (name, server) in &config.servers {
            if !server.enabled
                || (server.exposure == ToolExposure::Hidden
                    && server
                        .tool_overrides
                        .values()
                        .all(|exposure| *exposure == ToolExposure::Hidden))
            {
                self.servers.insert(
                    name.clone(),
                    ServerEntry {
                        client: None,
                        config: server.clone(),
                        state: ServerState::Disabled,
                        tools: vec![],
                    },
                );
                continue;
            }
            if loaded.project_servers.contains(name) {
                let repo = project_repo
                    .context("project-origin MCP server requires repository trust context")?;
                let digest = config.canonical_digest(name)?;
                anyhow::ensure!(
                    self.trust.inspect(repo, &digest)?,
                    "project MCP configuration is not trusted"
                );
            }
            if !valid_component(name) {
                anyhow::bail!("invalid MCP server name: {name}");
            }
            if server.exposure != ToolExposure::Direct
                && !server
                    .tool_overrides
                    .values()
                    .any(|exposure| *exposure == ToolExposure::Direct)
            {
                self.servers.insert(
                    name.clone(),
                    ServerEntry {
                        client: None,
                        config: server.clone(),
                        state: ServerState::Deferred,
                        tools: vec![],
                    },
                );
                continue;
            }
            self.connect_server(name, server).await?;
        }
        Ok(())
    }

    async fn connect_server(
        &mut self,
        name: &str,
        server: &crate::config::McpServerConfig,
    ) -> Result<()> {
        let expanded = McpConfig {
            servers: [(name.to_owned(), server.clone())].into_iter().collect(),
        }
        .expand_environment()?;
        let cfg = expanded.servers.get(name).unwrap();
        let args: Vec<&str> = cfg.args.iter().map(String::as_str).collect();
        let env: Vec<(String, String)> = cfg
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let timeout = Duration::from_millis(cfg.timeout_ms.max(1));
        let entry = ServerEntry {
            client: None,
            config: cfg.clone(),
            state: ServerState::Connecting,
            tools: vec![],
        };
        self.servers.insert(name.to_owned(), entry);
        match McpClient::spawn_with_options(
            &cfg.command,
            &args,
            cfg.cwd.as_deref(),
            Some(&env),
            timeout,
        )
        .await
        {
            Ok(client) => {
                self.servers.get_mut(name).unwrap().client = Some(Arc::new(client));
                self.servers.get_mut(name).unwrap().state = ServerState::Ready;
            }
            Err(err) => {
                self.servers.get_mut(name).unwrap().state = ServerState::Failed(err.to_string());
            }
        }
        Ok(())
    }

    pub async fn activate_deferred(&mut self, server_name: &str) -> Result<Vec<McpTool>> {
        let entry = self
            .servers
            .get(server_name)
            .context("server not configured")?;
        anyhow::ensure!(
            entry.config.enabled && entry.state != ServerState::Disabled,
            "MCP server disabled or hidden"
        );
        if entry.client.is_none() {
            let config = entry.config.clone();
            self.servers.get_mut(server_name).unwrap().state = ServerState::Connecting;
            self.connect_server(server_name, &config).await?;
        }
        self.discover_tools(server_name).await
    }

    pub async fn discover_tools(&mut self, server_name: &str) -> Result<Vec<McpTool>> {
        let entry = self
            .servers
            .get_mut(server_name)
            .context("server not registered")?;
        anyhow::ensure!(
            entry.config.enabled && entry.state == ServerState::Ready,
            "MCP server is not ready"
        );
        entry.tools.clear();
        self.routes.retain(|_, (server, _)| server != server_name);
        let client = entry.client.as_ref().context("server is not connected")?;
        let raw = client.list_tools().await?;
        let mut names = std::collections::HashSet::new();
        for tool in &raw {
            anyhow::ensure!(!tool.name.is_empty(), "empty MCP tool name");
            let exposure = entry
                .config
                .tool_overrides
                .get(&tool.name)
                .copied()
                .unwrap_or(entry.config.exposure);
            if exposure == ToolExposure::Hidden {
                continue;
            }
            let canonical = model_tool_name(server_name, &tool.name);
            if !names.insert(canonical.clone()) {
                anyhow::bail!("duplicate canonical MCP tool name: {canonical}");
            }
            self.routes.insert(
                canonical.clone(),
                (server_name.to_owned(), tool.name.clone()),
            );
            names.insert(canonical);
        }
        let visible: Vec<McpTool> = raw
            .into_iter()
            .filter(|tool| {
                entry
                    .config
                    .tool_overrides
                    .get(&tool.name)
                    .copied()
                    .unwrap_or(entry.config.exposure)
                    != ToolExposure::Hidden
            })
            .map(|tool| McpTool {
                name: model_tool_name(server_name, &tool.name),
                description: tool.description,
                input_schema: tool.input_schema,
            })
            .collect();
        entry.tools = visible.clone();
        Ok(visible)
    }

    pub fn direct_tools(&self) -> Vec<McpTool> {
        self.visible_tools(Some(ToolExposure::Direct))
    }
    pub fn deferred_tools(&self) -> Vec<McpTool> {
        self.visible_tools(Some(ToolExposure::Deferred))
    }
    fn visible_tools(&self, wanted: Option<ToolExposure>) -> Vec<McpTool> {
        let mut out = Vec::new();
        for (canonical, tool) in self
            .servers
            .values()
            .filter(|e| e.state == ServerState::Ready && e.config.enabled)
            .flat_map(|entry| {
                entry
                    .tools
                    .iter()
                    .map(move |tool| (tool.name.as_str(), tool))
            })
        {
            let Some((server, raw)) = self.routes.get(canonical) else {
                continue;
            };
            let Some(entry) = self.servers.get(server) else {
                continue;
            };
            if entry
                .config
                .tool_overrides
                .get(raw)
                .copied()
                .unwrap_or(entry.config.exposure)
                == wanted.unwrap()
            {
                out.push(tool.clone());
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }
    pub fn search_tools(&self, query: &str, limit: usize) -> Vec<McpTool> {
        let terms: Vec<String> = query
            .to_lowercase()
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        let mut candidates = self
            .servers
            .values()
            .filter(|e| e.state == ServerState::Ready && e.config.enabled)
            .flat_map(|e| e.tools.iter())
            .filter(|tool| {
                self.routes
                    .get(&tool.name)
                    .and_then(|(server, raw)| {
                        self.servers.get(server).map(|entry| {
                            entry
                                .config
                                .tool_overrides
                                .get(raw)
                                .copied()
                                .unwrap_or(entry.config.exposure)
                                != ToolExposure::Hidden
                        })
                    })
                    .unwrap_or(false)
            })
            .cloned()
            .collect::<Vec<_>>();
        candidates.sort_by(|a, b| {
            score(b, &terms)
                .cmp(&score(a, &terms))
                .then_with(|| a.name.cmp(&b.name))
        });
        candidates.into_iter().take(limit).collect()
    }
    pub async fn search_tools_async(&mut self, query: &str, limit: usize) -> Vec<McpTool> {
        let pending: Vec<String> = self
            .servers
            .iter()
            .filter(|(_, e)| e.state == ServerState::Deferred)
            .map(|(name, _)| name.clone())
            .collect();
        for name in pending {
            let _ = self.activate_deferred(&name).await;
        }
        self.search_tools(query, limit)
    }
    pub async fn execute_tool(&self, name: &str, arguments: Value) -> Result<CallToolResult> {
        let (server, tool) = self.routes.get(name).context("unrecognized MCP tool")?;
        let entry = self.servers.get(server).context("server not registered")?;
        anyhow::ensure!(
            entry.config.enabled && entry.state == ServerState::Ready,
            "MCP server is not available"
        );
        anyhow::ensure!(
            entry.tools.iter().any(|t| t.name == name),
            "MCP tool route is stale or hidden"
        );
        anyhow::ensure!(
            entry
                .config
                .tool_overrides
                .get(tool)
                .copied()
                .unwrap_or(entry.config.exposure)
                != ToolExposure::Hidden,
            "MCP tool is hidden"
        );
        let client = entry.client.as_ref().context("server is not connected")?;
        let result = client.call_tool(tool, arguments).await?;
        anyhow::ensure!(
            serde_json::to_vec(&result)?.len() <= entry.config.output_limit_bytes,
            "MCP tool output exceeded configured limit ({} bytes)",
            entry.config.output_limit_bytes
        );
        Ok(result)
    }
    pub async fn shutdown(&mut self) {
        for entry in self.servers.values_mut() {
            if let Some(client) = entry.client.take() {
                client.shutdown().await;
            }
        }
    }
}
fn valid_component(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
fn model_tool_name(server: &str, tool: &str) -> String {
    let digest = Sha256::digest(format!("{server}\0{tool}").as_bytes());
    let safe = format!("mcp_{}_{}", slug(server), slug(tool));
    let prefix = &safe[..safe.len().min(50)];
    format!("{prefix}_{}", hex::encode(&digest[..6]))
}
fn slug(value: &str) -> String {
    let mut slug = String::with_capacity(value.len().min(50));
    let mut separator = false;
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() {
            slug.push((byte as char).to_ascii_lowercase());
            separator = false;
        } else if !separator && !slug.is_empty() {
            slug.push('_');
            separator = true;
        }
        if slug.len() >= 32 {
            break;
        }
    }
    while slug.ends_with('_') {
        slug.pop();
    }
    if slug.is_empty() {
        "tool".to_owned()
    } else {
        slug
    }
}
fn score(tool: &McpTool, terms: &[String]) -> usize {
    let hay = format!(
        "{} {}",
        tool.name,
        tool.description.as_deref().unwrap_or("")
    )
    .to_lowercase();
    terms
        .iter()
        .map(|term| {
            if tool.name.to_lowercase() == *term {
                100
            } else if tool.name.to_lowercase().contains(term) {
                20
            } else if hay.contains(term) {
                5
            } else {
                0
            }
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_names_are_bounded_and_distinguish_legal_raw_names() {
        let a = model_tool_name("server", "read.file");
        let b = model_tool_name("server", "read file");
        assert!(a.len() <= 64);
        assert!(a.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
        assert_ne!(a, b);
        assert!(valid_component("server-name"));
    }
}
