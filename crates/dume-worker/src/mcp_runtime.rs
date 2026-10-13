use anyhow::{Context, Result};
use dume_mcp::{CallToolResult, McpConfig, McpConfigLoad, McpToolBridge, ToolExposure, TrustStore};
use dume_provider::types::ToolDefinition;
use dume_store::HarnessStore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

const MAX_QUERY: usize = 512;
const MAX_RESULTS: usize = 20;
const MAX_DESCRIPTION: usize = 4096;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Fingerprint {
    server: String,
    config: String,
    schema: String,
}

pub struct McpRuntime {
    root: PathBuf,
    store: Option<Arc<HarnessStore>>,
    conversation: Option<String>,
    bridge: McpToolBridge,
    config: McpConfig,
    project_servers: BTreeSet<String>,
    trust_records: Vec<dume_mcp::ProjectTrust>,
    direct: HashSet<String>,
    active: HashSet<String>,
    failures: Vec<String>,
    tool_servers: HashMap<String, String>,
}

impl McpRuntime {
    pub async fn load(
        project_root: &Path,
        store: Option<Arc<HarnessStore>>,
        conversation_id: Option<&str>,
    ) -> Result<Self> {
        let mut this = Self {
            root: project_root.to_path_buf(),
            store,
            conversation: conversation_id.map(str::to_owned),
            bridge: McpToolBridge::new(),
            config: McpConfig::default(),
            project_servers: BTreeSet::new(),
            trust_records: vec![],
            direct: HashSet::new(),
            active: HashSet::new(),
            failures: vec![],
            tool_servers: HashMap::new(),
        };
        this.initialize().await?;
        Ok(this)
    }

    async fn initialize(&mut self) -> Result<()> {
        let global_path = McpConfig::global_path()?;
        let trust_path = global_path.with_file_name("mcp-trust.json");
        self.initialize_from_paths(
            Some(&global_path),
            &self.root.join(".dume/mcp.json"),
            &trust_path,
        )
        .await
    }

    async fn initialize_from_paths(
        &mut self,
        global: Option<&Path>,
        project: &Path,
        trust_path: &Path,
    ) -> Result<()> {
        let loaded = McpConfig::load_with_origins(global, Some(project))?;
        let trust = TrustStore::load(trust_path)?;
        self.initialize_from(loaded, trust).await
    }

    async fn initialize_from(&mut self, loaded: McpConfigLoad, trust: TrustStore) -> Result<()> {
        let config = loaded.config.expand_environment()?;
        let project_servers = loaded.project_servers.clone();
        let trust_records = trust.records().to_vec();
        self.bridge.shutdown().await;
        self.bridge = McpToolBridge::new();
        self.direct.clear();
        self.active.clear();
        self.failures.clear();
        self.tool_servers.clear();
        self.config = config.clone();
        self.project_servers = project_servers;
        self.trust_records = trust_records;
        self.bridge.trust = trust;
        self.bridge
            .connect_loaded_config(
                &McpConfigLoad {
                    config: config.clone(),
                    project_servers: loaded.project_servers.clone(),
                },
                Some(&self.root),
            )
            .await?;
        for (name, state, _) in self.bridge.server_states() {
            if let dume_mcp::ServerState::Failed(err) = state {
                self.failures.push(format!("{name}: {err}"));
            }
        }
        let direct_servers: Vec<String> = self
            .bridge
            .server_states()
            .into_iter()
            .filter(|(_, state, _)| *state == dume_mcp::ServerState::Ready)
            .map(|(name, _, _)| name)
            .collect();
        for server in direct_servers {
            match self.bridge.discover_tools(&server).await {
                Ok(tools) => {
                    for tool in tools {
                        self.tool_servers.insert(tool.name.clone(), server.clone());
                    }
                }
                Err(e) => self.failures.push(format!("{server}: {e}")),
            }
            self.direct
                .extend(self.bridge.direct_tools().into_iter().map(|tool| tool.name));
        }
        if let (Some(store), Some(conversation)) = (&self.store, &self.conversation) {
            let persisted = store.active_tools(conversation)?;
            for (id, fingerprint) in persisted {
                let fp: Fingerprint = match serde_json::from_str(&fingerprint) {
                    Ok(fp) => fp,
                    Err(_) => continue,
                };
                let server = fp.server.as_str();
                if !self.config.servers.contains_key(server) {
                    continue;
                }
                if self.ensure_server_discovered(server).await.is_err() {
                    continue;
                }
                if self.fingerprint(&id).as_ref() == Some(&fp) {
                    self.active.insert(id);
                }
            }
            self.persist(&self.active)?;
        }
        Ok(())
    }

    pub async fn refresh(&mut self) -> Result<()> {
        let global_path = McpConfig::global_path()?;
        let trust_path = global_path.with_file_name("mcp-trust.json");
        self.refresh_from_paths(&global_path, &self.root.join(".dume/mcp.json"), &trust_path)
            .await
    }

    async fn refresh_from_paths(
        &mut self,
        global: &Path,
        project: &Path,
        trust_path: &Path,
    ) -> Result<()> {
        let loaded = McpConfig::load_with_origins(Some(global), Some(project))?;
        let trust = TrustStore::load(trust_path)?;
        let config = loaded.config.expand_environment()?;
        if serde_json::to_value(&config)? != serde_json::to_value(&self.config)?
            || loaded.project_servers != self.project_servers
            || trust.records() != self.trust_records
        {
            return self.initialize_from(loaded, trust).await;
        }

        self.failures.clear();
        let previous_fingerprints: HashMap<String, Fingerprint> = self
            .active
            .iter()
            .filter_map(|id| {
                self.fingerprint(id)
                    .map(|fingerprint| (id.clone(), fingerprint))
            })
            .collect();
        let servers: Vec<String> = self
            .bridge
            .server_states()
            .into_iter()
            .filter(|(_, state, _)| *state == dume_mcp::ServerState::Ready)
            .map(|(name, _, _)| name)
            .collect();
        for server in servers {
            match self.bridge.discover_tools(&server).await {
                Ok(tools) => {
                    self.tool_servers.retain(|_, owner| owner != &server);
                    for tool in tools {
                        self.tool_servers.insert(tool.name, server.clone());
                    }
                }
                Err(error) => self.failures.push(format!("{server}: {error}")),
            }
        }
        self.direct = self
            .bridge
            .direct_tools()
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        let mut next_active = self.active.clone();
        next_active.retain(|id| self.fingerprint(id).as_ref() == previous_fingerprints.get(id));
        if let Err(error) = self.persist(&next_active) {
            self.active.clear();
            return Err(error);
        }
        self.active = next_active;
        Ok(())
    }
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.all_tools()
            .into_iter()
            .filter(|t| self.direct.contains(&t.name) || self.active.contains(&t.name))
            .map(|t| ToolDefinition {
                name: t.name,
                description: t.description.unwrap_or_default(),
                parameters: t.input_schema,
            })
            .collect()
    }
    pub async fn search(&mut self, query: &str, limit: usize) -> Result<String> {
        anyhow::ensure!(
            query.len() <= MAX_QUERY,
            "MCP search query exceeds {MAX_QUERY} bytes"
        );
        let limit = limit.min(MAX_RESULTS);
        let pending: Vec<String> = self
            .bridge
            .server_states()
            .into_iter()
            .filter(|(_, s, _)| *s == dume_mcp::ServerState::Deferred)
            .map(|(n, _, _)| n)
            .collect();
        for server in pending {
            if let Err(e) = self.ensure_server_discovered(&server).await {
                self.failures.push(format!("{server}: {e}"));
            }
        }
        let terms: Vec<String> = query
            .to_lowercase()
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        let mut found = self
            .all_tools()
            .into_iter()
            .filter(|t| {
                self.exposure(&t.name) == Some(ToolExposure::Deferred)
                    && terms.iter().all(|term| {
                        format!("{} {}", t.name, t.description.as_deref().unwrap_or(""))
                            .to_lowercase()
                            .contains(term)
                    })
            })
            .collect::<Vec<_>>();
        found.sort_by(|a, b| a.name.cmp(&b.name));
        found.truncate(limit);
        let mut next = self.active.clone();
        next.extend(found.iter().map(|t| t.name.clone()));
        let persisted = self.persist(&next);
        self.commit_activation(&next, || persisted)?;
        let mut text = found
            .iter()
            .map(|t| format!("{}: {}", t.name, t.description.as_deref().unwrap_or("")))
            .collect::<Vec<_>>()
            .join("\n");
        if text.len() > MAX_DESCRIPTION {
            let mut end = MAX_DESCRIPTION;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
        }
        if !self.failures.is_empty() {
            text.push_str(&format!(
                "\nMCP server failures: {}",
                self.failures.join("; ")
            ));
        }
        if text.len() > MAX_DESCRIPTION {
            let mut end = MAX_DESCRIPTION;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
        }
        Ok(text)
    }
    pub async fn execute(&self, name: &str, arguments: Value) -> Result<CallToolResult> {
        anyhow::ensure!(
            self.direct.contains(name) || self.active.contains(name),
            "MCP tool is not active"
        );
        anyhow::ensure!(
            self.exposure(name) != Some(ToolExposure::Hidden),
            "MCP tool is hidden"
        );
        self.bridge.execute_tool(name, arguments).await
    }
    pub fn status(&self) -> String {
        let mut lines = self
            .bridge
            .server_states()
            .into_iter()
            .map(|(n, s, count)| format!("{n}: {s:?} ({count} tools)"))
            .collect::<Vec<_>>();
        lines.extend(self.failures.iter().cloned());
        lines.join("\n")
    }
    pub async fn shutdown(&mut self) {
        self.bridge.shutdown().await;
    }
    pub fn owns_name(&self, name: &str) -> bool {
        name == "tool_search" || self.all_tools().iter().any(|t| t.name == name)
    }
    pub fn is_active(&self, name: &str) -> bool {
        self.direct.contains(name) || self.active.contains(name)
    }
    async fn ensure_server_discovered(&mut self, server: &str) -> Result<()> {
        if self
            .bridge
            .server_states()
            .iter()
            .find(|(n, _, _)| n == server)
            .is_some_and(|(_, s, count)| *s == dume_mcp::ServerState::Ready && *count > 0)
        {
            return Ok(());
        }
        let tools = self.bridge.activate_deferred(server).await?;
        for tool in tools {
            self.tool_servers
                .insert(tool.name.clone(), server.to_owned());
        }
        Ok(())
    }
    fn all_tools(&self) -> Vec<dume_mcp::types::McpTool> {
        let mut tools = self.bridge.direct_tools();
        tools.extend(self.bridge.deferred_tools());
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        tools
    }
    fn server_for(&self, id: &str) -> Option<&str> {
        self.tool_servers.get(id).map(String::as_str)
    }
    fn exposure(&self, id: &str) -> Option<ToolExposure> {
        if self.bridge.direct_tools().iter().any(|t| t.name == id) {
            Some(ToolExposure::Direct)
        } else if self.bridge.deferred_tools().iter().any(|t| t.name == id) {
            Some(ToolExposure::Deferred)
        } else {
            None
        }
    }
    fn fingerprint(&self, id: &str) -> Option<Fingerprint> {
        let tool = self.all_tools().into_iter().find(|t| t.name == id)?;
        let server = self.server_for(id)?;
        let schema = hex::encode(Sha256::digest(serde_json::to_vec(&tool.input_schema).ok()?));
        Some(Fingerprint {
            server: server.to_owned(),
            config: self.config.canonical_digest(server).ok()?,
            schema,
        })
    }
    fn commit_activation(
        &mut self,
        active: &HashSet<String>,
        persist: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        persist()?;
        self.active = active.clone();
        Ok(())
    }
    fn persist(&self, active: &HashSet<String>) -> Result<()> {
        let (Some(store), Some(conversation)) = (&self.store, &self.conversation) else {
            return Ok(());
        };
        let ids: Vec<String> = active.iter().cloned().collect();
        let mut fps = HashMap::new();
        for id in &ids {
            fps.insert(
                id.clone(),
                serde_json::to_string(
                    &self
                        .fingerprint(id)
                        .context("cannot fingerprint activated MCP tool")?,
                )?,
            );
        }
        store.activate_tools(conversation, &ids, &fps)?;
        Ok(())
    }
}

pub fn tool_search_definition() -> ToolDefinition {
    ToolDefinition { name:"tool_search".into(), description:"Search available deferred MCP tools by name and description; matching tools become available for this conversation.".into(), parameters:json!({"type":"object","properties":{"query":{"type":"string","maxLength":512},"limit":{"type":"integer","minimum":1,"maximum":20}},"required":["query"],"additionalProperties":false}) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        global: PathBuf,
        trust_path: PathBuf,
        db_path: PathBuf,
        db: Arc<HarnessStore>,
        script: PathBuf,
    }

    fn fixture(schema: &str, extra: &str) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        fs::create_dir_all(root.join(".dume")).unwrap();
        let global = dir.path().join("global.json");
        fs::write(&global, r#"{"servers":{}}"#).unwrap();
        let trust_path = dir.path().join("mcp-trust.json");
        let script = dir.path().join("server.sh");
        fs::write(&script, format!(r#"#!/bin/sh
printf 'start\n' >> "$0.starts"
payload="$1"; extra="$2"
while IFS= read -r line; do
  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  [ -n "$id" ] || continue
  case "$method" in
    initialize) printf '{{"jsonrpc":"2.0","id":%s,"result":{{"protocolVersion":"2024-11-05","capabilities":{{}}}}}}\n' "$id";;
    tools/list) schema=$(cat "$0.schema"); printf '{{"jsonrpc":"2.0","id":%s,"result":{{"tools":[{{"name":"lookup","description":"find %s records","inputSchema":%s}}]}}}}\n' "$id" "$extra" "$schema";;
    tools/call) args=$(printf '%s' "$line" | sed -n 's/.*"arguments":\({{[^}}]*}}\).*/\1/p'); printf '%s' "$args" > "$payload"; printf '{{"jsonrpc":"2.0","id":%s,"result":{{"content":[{{"type":"text","text":"called"}}]}}}}\n' "$id";;
  esac
done
"#)).unwrap();
        fs::write(script.with_extension("sh.schema"), schema).unwrap();
        write_project_config(&root, &script, extra, "deferred");
        let loaded =
            McpConfig::load_with_origins(Some(&global), Some(&root.join(".dume/mcp.json")))
                .unwrap();
        let mut trust = TrustStore::default();
        trust
            .trust(
                &root,
                loaded.config.canonical_digest("project_server").unwrap(),
            )
            .unwrap();
        trust.save(&trust_path).unwrap();
        let db_path = dir.path().join("state.sqlite");
        let db = Arc::new(HarnessStore::open(&db_path, dir.path().join("artifacts")).unwrap());
        db.create_conversation(
            "conversation",
            root.to_str().unwrap(),
            "model",
            "provider-session",
        )
        .unwrap();
        Fixture {
            _dir: dir,
            root,
            global,
            trust_path,
            db_path,
            db,
            script,
        }
    }

    fn write_project_config(root: &Path, script: &Path, args: &str, exposure: &str) {
        let config = json!({"servers":{"project_server":{"command":"/bin/sh","args":[script.to_string_lossy(),root.parent().unwrap().join("payload").to_string_lossy(),args],"exposure":exposure}}});
        fs::write(
            root.join(".dume/mcp.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
    }

    async fn load(f: &Fixture) -> McpRuntime {
        let mut runtime = McpRuntime {
            root: f.root.clone(),
            store: Some(f.db.clone()),
            conversation: Some("conversation".into()),
            bridge: McpToolBridge::new(),
            config: McpConfig::default(),
            project_servers: BTreeSet::new(),
            trust_records: vec![],
            direct: HashSet::new(),
            active: HashSet::new(),
            failures: vec![],
            tool_servers: HashMap::new(),
        };
        runtime
            .initialize_from_paths(
                Some(&f.global),
                &f.root.join(".dume/mcp.json"),
                &f.trust_path,
            )
            .await
            .unwrap();
        runtime
    }

    #[tokio::test]
    async fn search_activates_and_executes_selected_deferred_tool() {
        let f = fixture(
            r#"{"type":"object","properties":{"key":{"type":"string"}}}"#,
            "customer",
        );
        let mut runtime = load(&f).await;
        assert!(runtime.definitions().is_empty());
        let result = runtime.search("CUSTOMER", 5).await.unwrap();
        assert!(result.contains("find customer records"));
        let defs = runtime.definitions();
        assert_eq!(defs.len(), 1);
        let call = runtime
            .execute(&defs[0].name, json!({"key":"value"}))
            .await
            .unwrap();
        assert_eq!(call.content[0].text.as_deref(), Some("called"));
        assert_eq!(
            fs::read_to_string(f._dir.path().join("payload")).unwrap(),
            r#"{"key":"value"}"#
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn direct_server_deferred_override_is_not_declared_until_search() {
        let f = fixture(r#"{"type":"object"}"#, "customer");
        write_project_config(&f.root, &f.script, "customer", "direct");
        let config_path = f.root.join(".dume/mcp.json");
        let mut value: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
        value["servers"]["project_server"]["tool_overrides"]["lookup"] = json!("deferred");
        fs::write(&config_path, serde_json::to_vec(&value).unwrap()).unwrap();
        let loaded = McpConfig::load_with_origins(Some(&f.global), Some(&config_path)).unwrap();
        let mut trust = TrustStore::default();
        trust
            .trust(
                &f.root,
                loaded.config.canonical_digest("project_server").unwrap(),
            )
            .unwrap();
        trust.save(&f.trust_path).unwrap();
        let mut runtime = load(&f).await;
        assert!(runtime.definitions().is_empty());
        runtime.search("customer", 1).await.unwrap();
        assert_eq!(runtime.definitions().len(), 1);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn deferred_server_direct_override_is_declared_without_search() {
        let f = fixture(r#"{"type":"object"}"#, "customer");
        let config_path = f.root.join(".dume/mcp.json");
        let mut value: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
        value["servers"]["project_server"]["tool_overrides"]["lookup"] = json!("direct");
        fs::write(&config_path, serde_json::to_vec(&value).unwrap()).unwrap();
        let loaded = McpConfig::load_with_origins(Some(&f.global), Some(&config_path)).unwrap();
        let mut trust = TrustStore::default();
        trust
            .trust(
                &f.root,
                loaded.config.canonical_digest("project_server").unwrap(),
            )
            .unwrap();
        trust.save(&f.trust_path).unwrap();
        let mut runtime = load(&f).await;
        let definitions = runtime.definitions();
        assert_eq!(definitions.len(), 1);
        assert!(
            runtime
                .execute(&definitions[0].name, json!({}))
                .await
                .is_ok()
        );
        assert_eq!(
            fs::read_to_string(f.script.with_extension("sh.starts"))
                .unwrap()
                .lines()
                .count(),
            1
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn unchanged_refresh_reuses_client_and_schema_change_revokes_activation() {
        let f = fixture(
            r#"{"type":"object","properties":{"a":{"type":"string"}}}"#,
            "customer",
        );
        let mut runtime = load(&f).await;
        runtime.search("customer", 1).await.unwrap();
        assert_eq!(runtime.definitions().len(), 1);
        let starts = f.script.with_extension("sh.starts");
        assert_eq!(fs::read_to_string(&starts).unwrap().lines().count(), 1);
        runtime
            .refresh_from_paths(&f.global, &f.root.join(".dume/mcp.json"), &f.trust_path)
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(&starts).unwrap().lines().count(), 1);
        fs::write(
            f.script.with_extension("sh.schema"),
            r#"{"type":"object","properties":{"b":{"type":"string"}}}"#,
        )
        .unwrap();
        runtime
            .refresh_from_paths(&f.global, &f.root.join(".dume/mcp.json"), &f.trust_path)
            .await
            .unwrap();
        assert!(runtime.definitions().is_empty());
        assert!(f.db.active_tools("conversation").unwrap().is_empty());
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn inactive_and_hidden_tools_are_refused() {
        let f = fixture(r#"{"type":"object"}"#, "customer");
        let mut runtime = load(&f).await;
        runtime
            .ensure_server_discovered("project_server")
            .await
            .unwrap();
        let id = runtime.all_tools()[0].name.clone();
        assert!(
            runtime
                .execute(&id, json!({}))
                .await
                .unwrap_err()
                .to_string()
                .contains("not active")
        );
        runtime.shutdown().await;

        write_project_config(&f.root, &f.script, "", "hidden");
        let loaded =
            McpConfig::load_with_origins(Some(&f.global), Some(&f.root.join(".dume/mcp.json")))
                .unwrap();
        let mut trust = TrustStore::default();
        trust
            .trust(
                &f.root,
                loaded.config.canonical_digest("project_server").unwrap(),
            )
            .unwrap();
        trust.save(&f.trust_path).unwrap();
        let mut runtime = load(&f).await;
        assert!(runtime.definitions().is_empty());
        assert!(!runtime.search("customer", 1).await.unwrap().contains(&id));
        assert!(runtime.execute(&id, json!({})).await.is_err());
        assert_eq!(
            fs::read_to_string(f.script.with_extension("sh.starts"))
                .unwrap()
                .lines()
                .count(),
            1
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn failed_rediscovery_revokes_saved_activation() {
        let fixture = fixture(r#"{"type":"object"}"#, "customer");
        let mut runtime = load(&fixture).await;
        runtime.search("customer", 1).await.unwrap();
        let id = runtime.definitions()[0].name.clone();
        fs::write(fixture.script.with_extension("sh.schema"), "{").unwrap();
        runtime
            .refresh_from_paths(
                &fixture.global,
                &fixture.root.join(".dume/mcp.json"),
                &fixture.trust_path,
            )
            .await
            .unwrap();
        assert!(runtime.definitions().is_empty());
        assert!(fixture.db.active_tools("conversation").unwrap().is_empty());
        assert!(runtime.execute(&id, json!({})).await.is_err());
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn reopen_restores_only_selected_tool_and_changed_schema_invalidates_it() {
        let f = fixture(
            r#"{"type":"object","properties":{"a":{"type":"string"}}}"#,
            "customer",
        );
        let mut runtime = load(&f).await;
        runtime.search("customer", 1).await.unwrap();
        let selected = runtime.definitions()[0].name.clone();
        runtime.shutdown().await;
        let restored = load(&f).await;
        assert_eq!(
            restored
                .definitions()
                .iter()
                .map(|t| t.name.clone())
                .collect::<Vec<_>>(),
            vec![selected]
        );
        let mut restored = restored;
        restored.shutdown().await;

        fs::write(
            f.script.with_extension("sh.schema"),
            r#"{"type":"object","properties":{"b":{"type":"string"}}}"#,
        )
        .unwrap();
        let changed = load(&f).await;
        assert!(changed.definitions().is_empty());
        let mut changed = changed;
        changed.shutdown().await;
    }

    #[tokio::test]
    async fn config_mutation_invalidates_persisted_selection() {
        let f = fixture(r#"{"type":"object"}"#, "customer");
        let mut runtime = load(&f).await;
        runtime.search("customer", 1).await.unwrap();
        runtime.shutdown().await;
        write_project_config(&f.root, &f.script, "changed", "deferred");
        let loaded =
            McpConfig::load_with_origins(Some(&f.global), Some(&f.root.join(".dume/mcp.json")))
                .unwrap();
        let mut trust = TrustStore::default();
        trust
            .trust(
                &f.root,
                loaded.config.canonical_digest("project_server").unwrap(),
            )
            .unwrap();
        trust.save(&f.trust_path).unwrap();
        let mut changed = load(&f).await;
        assert!(changed.definitions().is_empty());
        changed.shutdown().await;
    }

    #[tokio::test]
    async fn failed_activation_persistence_does_not_change_exposure() {
        let f = fixture(r#"{"type":"object"}"#, "customer");
        let mut runtime = load(&f).await;
        let mut bad = HashSet::new();
        bad.insert("not-a-tool".to_owned());
        assert!(
            runtime
                .commit_activation(&bad, || Err(anyhow::anyhow!(
                    "simulated store write failure"
                )))
                .is_err()
        );
        assert!(runtime.active.is_empty());
        assert!(runtime.definitions().is_empty());
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn actual_store_failure_does_not_activate_tool() {
        let f = fixture(r#"{"type":"object"}"#, "customer");
        let mut runtime = load(&f).await;
        let connection = rusqlite::Connection::open(&f.db_path).unwrap();
        connection.execute_batch("CREATE TRIGGER reject_mcp_activation BEFORE INSERT ON conversation_active_tools BEGIN SELECT RAISE(ABORT, 'activation rejected'); END").unwrap();
        assert!(runtime.search("customer", 1).await.is_err());
        assert!(runtime.active.is_empty());
        assert!(runtime.definitions().is_empty());
        assert!(f.db.active_tools("conversation").unwrap().is_empty());
        runtime.shutdown().await;
    }
}
