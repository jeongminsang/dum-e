use crate::auth::CredentialStore;
use crate::catalog::{ModelCatalog, ModelInfo};
use anyhow::{Context, Result};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:10100/v1";
const CACHE_FILE: &str = "models-opencodex-cache.json";

#[derive(Serialize, Deserialize)]
struct ModelCache {
    base_url: String,
    models: Vec<ModelInfo>,
}

pub fn validate_base_url(base_url: &str) -> Result<String> {
    let mut url = Url::parse(base_url).context("Invalid OpenCodex base URL")?;
    let host = url.host_str().unwrap_or("");
    let loopback = host == "localhost"
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    anyhow::ensure!(
        loopback
            && matches!(url.scheme(), "http" | "https")
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && matches!(url.path(), "/" | "/v1" | "/v1/"),
        "OpenCodex base URL must be a loopback HTTP(S) address with an optional /v1 path"
    );
    if url.host_str() == Some("localhost") {
        url.set_host(Some("127.0.0.1"))?;
    }
    url.set_path("/v1");
    Ok(url.to_string())
}

pub fn base_url() -> Result<String> {
    validate_base_url(
        &std::env::var("OPENCODEX_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.into()),
    )
}

pub(crate) fn cached_models(root: &Path, base_url: &str) -> Option<Vec<ModelInfo>> {
    let cache: ModelCache =
        serde_json::from_slice(&std::fs::read(root.join(CACHE_FILE)).ok()?).ok()?;
    if cache.base_url != base_url || cache.models.is_empty() {
        return None;
    }
    if cache.models.iter().any(|model| {
        model.provider != "opencodex"
            || model.api != "openai-responses"
            || model.base_url.as_deref() != Some(base_url)
            || model.id.trim().is_empty()
    }) {
        return None;
    }
    Some(cache.models)
}

fn parse_models(payload: &Value, base_url: &str) -> Result<Vec<ModelInfo>> {
    let rows = payload["data"]
        .as_array()
        .context("OpenCodex models response requires data array")?;
    let mut models = std::collections::BTreeMap::new();
    for row in rows {
        let Some(id) = row["id"]
            .as_str()
            .filter(|id| !id.trim().is_empty() && !id.chars().any(char::is_control))
        else {
            continue;
        };
        if row["api_types"]
            .as_array()
            .is_some_and(|types| !types.iter().any(|api| api == "responses"))
            || row["capabilities"]["supports_tool_use"] == false
            || row["capabilities"]["supports_streaming"] == false
        {
            continue;
        }
        let capabilities = &row["capabilities"];
        let positive = |value: &Value| value.as_u64().filter(|value| *value > 0);
        models.insert(
            id.to_string(),
            ModelInfo {
                id: id.to_string(),
                name: row["name"].as_str().unwrap_or(id).to_string(),
                provider: "opencodex".into(),
                api: "openai-responses".into(),
                base_url: Some(base_url.into()),
                reasoning_levels: row
                    .get("supported_reasoning_levels")
                    .or_else(|| row.get("reasoning_efforts"))
                    .and_then(Value::as_array)
                    .map(|levels| {
                        levels
                            .iter()
                            .filter_map(|level| {
                                serde_json::from_value(
                                    level
                                        .get("effort")
                                        .or_else(|| level.get("value"))
                                        .or_else(|| level.get("id"))
                                        .unwrap_or(level)
                                        .clone(),
                                )
                                .ok()
                            })
                            .collect()
                    }),
                reasoning: row["supports_reasoning_effort"]
                    .as_bool()
                    .or_else(|| capabilities["supports_reasoning"].as_bool())
                    .unwrap_or(false),
                input: capabilities["input_modalities"]
                    .as_array()
                    .map(|input| {
                        input
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_else(|| vec!["text".into()]),
                context_window: positive(&row["context_window"])
                    .or_else(|| positive(&row["context_length"]))
                    .or_else(|| positive(&capabilities["context_length"])),
                max_tokens: positive(&row["max_output_tokens"])
                    .or_else(|| positive(&capabilities["max_output_tokens"])),
                cost: None,
            },
        );
    }
    anyhow::ensure!(
        !models.is_empty(),
        "OpenCodex has no streaming Responses models with tool support"
    );
    Ok(models.into_values().collect())
}

pub async fn refresh_cache_if_stale() -> Result<Option<usize>> {
    refresh_cache_with(&ModelCatalog::models_dir(), &base_url()?).await
}

pub(crate) async fn refresh_cache_with(root: &Path, endpoint: &str) -> Result<Option<usize>> {
    let endpoint = validate_base_url(endpoint)?;
    let fresh = std::fs::metadata(root.join(CACHE_FILE))
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| std::time::SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age < Duration::from_secs(300));
    if fresh && cached_models(root, &endpoint).is_some() {
        return Ok(None);
    }
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(3))
        .build()?;
    let health: Value = client
        .get(format!("{}/healthz", endpoint.trim_end_matches("/v1")))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    anyhow::ensure!(
        health["status"] == "ok" && health["service"] == "opencodex",
        "Local endpoint is not a healthy OpenCodex server"
    );
    let store = CredentialStore::new(root.join("auth.json"));
    let mut request = client.get(format!("{endpoint}/models"));
    if let Some(credential) = store.resolve_credential("opencodex").await? {
        anyhow::ensure!(
            credential.cred_type == "api_key",
            "OpenCodex requires an API key, not OAuth"
        );
        request = request.bearer_auth(credential.key.context("OpenCodex API key missing")?);
    }
    let payload = request.send().await?.error_for_status()?.json().await?;
    let models = parse_models(&payload, &endpoint)?;
    let count = models.len();
    std::fs::create_dir_all(root)?;
    let mut file = tempfile::NamedTempFile::new_in(root)?;
    serde_json::to_writer_pretty(
        &mut file,
        &ModelCache {
            base_url: endpoint,
            models,
        },
    )?;
    file.as_file().sync_all()?;
    file.persist(root.join(CACHE_FILE))?;
    Ok(Some(count))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{Read, Write};

    fn fixture(responses: Vec<Value>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for response in responses {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                while !bytes.windows(4).any(|chunk| chunk == b"\r\n\r\n") {
                    let mut chunk = [0; 1024];
                    let size = socket.read(&mut chunk).unwrap();
                    assert!(size > 0);
                    bytes.extend_from_slice(&chunk[..size]);
                }
                requests.push(String::from_utf8(bytes).unwrap());
                let body = response.to_string();
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
            requests
        });
        (endpoint, server)
    }

    #[tokio::test]
    async fn discovery_checks_service_and_caches_only_for_the_same_endpoint() {
        let root = tempfile::tempdir().unwrap();
        let (endpoint, server) = fixture(vec![
            json!({"status":"ok","service":"opencodex"}),
            json!({"data":[{"id":"routed/model","api_types":["responses"]}]}),
        ]);
        assert_eq!(
            refresh_cache_with(root.path(), &endpoint).await.unwrap(),
            Some(1)
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /healthz "));
        assert!(!requests[0].to_ascii_lowercase().contains("authorization:"));
        assert!(requests[1].starts_with("GET /v1/models "));
        assert_eq!(
            cached_models(root.path(), &endpoint).unwrap()[0].id,
            "routed/model"
        );
        assert!(cached_models(root.path(), DEFAULT_BASE_URL).is_none());
        // The server is gone; a fresh cache must work without a network request.
        assert_eq!(
            refresh_cache_with(root.path(), &endpoint).await.unwrap(),
            None
        );

        let (endpoint, server) = fixture(vec![json!({"status":"ok","service":"other"})]);
        let error = refresh_cache_with(root.path(), &endpoint)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not a healthy OpenCodex"));
        assert_eq!(server.join().unwrap().len(), 1);
        assert!(cached_models(root.path(), &endpoint).is_none());
    }

    #[test]
    fn local_catalog_is_available_without_login_and_cannot_change_its_endpoint() {
        let root = tempfile::tempdir().unwrap();
        let endpoint = base_url().unwrap();
        let models = parse_models(&json!({"data":[{"id":"routed/model"}]}), &endpoint).unwrap();
        let cache = ModelCache {
            base_url: endpoint.clone(),
            models,
        };
        std::fs::write(
            root.path().join(CACHE_FILE),
            serde_json::to_vec(&cache).unwrap(),
        )
        .unwrap();
        let model =
            crate::runtime::resolve_model_with_root("opencodex/routed/model", root.path()).unwrap();
        assert_eq!(model.base_url.as_deref(), Some(endpoint.as_str()));
        assert!(!root.path().join("auth.json").exists());

        let mut cache = cache;
        cache.models[0].base_url = Some("https://example.com/v1".into());
        std::fs::write(
            root.path().join(CACHE_FILE),
            serde_json::to_vec(&cache).unwrap(),
        )
        .unwrap();
        assert!(
            crate::runtime::resolve_model_with_root("opencodex/routed/model", root.path()).is_err()
        );
    }

    #[test]
    fn endpoint_validation_requires_loopback_and_normalizes_api_root() {
        for url in ["http://127.0.0.1:10100", "http://localhost:10100/v1/"] {
            assert_eq!(validate_base_url(url).unwrap(), DEFAULT_BASE_URL);
        }
        assert_eq!(
            validate_base_url("http://[::1]:10100/v1").unwrap(),
            "http://[::1]:10100/v1"
        );
        for url in [
            "https://example.com/v1",
            "http://192.168.1.2/v1",
            "file:///tmp/v1",
            "http://key@127.0.0.1/v1",
            "http://127.0.0.1/v1?key=secret",
            "http://127.0.0.1/v1#fragment",
            "http://localhost.example.com/v1",
            "http://127.0.0.1/other",
        ] {
            assert!(validate_base_url(url).is_err(), "{url}");
        }
    }

    #[test]
    fn catalog_keeps_routed_ids_metadata_and_unknown_prices() {
        let models = parse_models(&json!({"data": [
            {"id":"anthropic/model", "api_types":["responses"], "supports_reasoning_effort":true,
             "reasoning_efforts":[{"id":"low","value":"low"},{"effort":"high"},"unknown"],
             "context_window":200000, "capabilities":{"max_output_tokens":8000, "input_modalities":["text","image"]}},
            {"id":"no-tools", "capabilities":{"supports_tool_use":false}},
            {"id":"chat-only", "api_types":["chat_completions"]},
            {"id":"no-stream", "capabilities":{"supports_streaming":false}},
            {"id":"bad\nmodel"}
        ]}), DEFAULT_BASE_URL).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "anthropic/model");
        assert_eq!(models[0].provider, "opencodex");
        assert_eq!(models[0].context_window, Some(200000));
        assert_eq!(models[0].max_tokens, Some(8000));
        assert_eq!(models[0].input, ["text", "image"]);
        assert!(models[0].reasoning);
        assert_eq!(
            models[0].reasoning_levels,
            Some(vec![
                crate::reasoning::ReasoningEffort::Low,
                crate::reasoning::ReasoningEffort::High
            ])
        );
        assert!(models[0].cost.is_none());
        assert!(parse_models(&json!({"data":[]}), DEFAULT_BASE_URL).is_err());
    }
}
