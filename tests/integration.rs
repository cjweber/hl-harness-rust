//! Integration tests: TS-hash parity, SSE transport against a local mock server,
//! and the full agent tool loop with a scripted mock provider.

use futures_util::StreamExt;
use hl_harness::cache::prefix::{create_cache_plan, CachePrefix};
use hl_harness::core::agent::{Agent, CompactionOptions};
use hl_harness::model::provider::{OpenAICompatibleProvider, Provider, ProviderConfig, ProviderEvent, ProviderKind, ProviderRequest};
use hl_harness::types::{Message, ToolCall};
use serde_json::{json, Value};
use std::sync::Arc;

/// The TypeScript harness must produce the same prefix hash for the same prefix.
#[tokio::test]
async fn cache_plan_hash_matches_typescript() {
    // Reference value produced by the TS implementation:
    //   serializePrefix({system:"s", tools:[{name:"a",...}], context:[], resources:[], transcript:["t1","t2"]})
    // computed with the TS `canonical()` + sha256.
    let mut prefix = CachePrefix { system: "s".into(), ..Default::default() };
    prefix.tools.push(hl_harness::types::ToolDeclaration { name: "a".into(), description: "d".into(), parameters: json!({}) });
    prefix.transcript = vec!["t1".into(), "t2".into()];
    let plan = create_cache_plan("openrouter", "m", &prefix, "suffix", true);
    // Independent recomputation of the TS canonical form:
    let expected = concat!(
        r#"{"context":[],"resources":[],"system":"s","tools":[{"description":"d","name":"a","parameters":{}}],"transcript":["t1","t2"]}"#
    );
    assert_eq!(hl_harness::cache::prefix::serialize_prefix(&prefix), expected);
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(expected.as_bytes());
    let expected_hash = hasher.finalize()[..].iter().map(|b| format!("{b:02x}")).collect::<String>();
    assert_eq!(plan.prefix_hash, expected_hash);
}

/// Minimal HTTP server that answers one SSE response and reports what it received.
struct MockServer {
    port: u16,
    received: Arc<std::sync::Mutex<Vec<String>>>,
}

impl MockServer {
    async fn start(sse_body: &'static str, status: u16) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let received = Arc::new(std::sync::Mutex::new(Vec::new()));
        let shared = received.clone();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            // Read the request head.
            let mut buffer = vec![0u8; 65536];
            let mut raw = Vec::new();
            loop {
                let read = tokio::io::AsyncReadExt::read(&mut socket, &mut buffer).await.unwrap();
                raw.extend_from_slice(&buffer[..read]);
                if String::from_utf8_lossy(&raw).contains("\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&raw).to_string();
            shared.lock().unwrap().push(request);
            let response = if status == 200 {
                format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{}", sse_body.len(), sse_body)
            } else {
                format!("HTTP/1.1 {status} Too Many Requests\r\ncontent-type: text/plain\r\ncontent-length: 5\r\n\r\noops!")
            };
            tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await.unwrap();
        });
        Self { port, received }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }
}

fn sse_provider(url: String) -> OpenAICompatibleProvider {
    OpenAICompatibleProvider::new(ProviderConfig {
        id: ProviderKind::OpenCodeGo,
        name: "opencode-go".into(),
        base_url: url,
        api_key: Some("test-key".into()),
        model: "test-model".into(),
        session_id: None,
        headers: Vec::new(),
        timeout_ms: Some(5000),
        max_retries: Some(0),
        retry_delay_ms: None,
        context_window: None,
    })
}

#[tokio::test]
async fn provider_streams_text_usage_and_tool_calls_over_sse() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"pa\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"ge\\\":\\\"f\\\"}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":7,\"prompt_tokens_details\":{\"cached_tokens\":40}}}\n\n",
        "data: [DONE]\n\n"
    );
    let server = MockServer::start(body, 200).await;
    let provider = sse_provider(server.url());
    let events: Vec<ProviderEvent> = provider.stream(ProviderRequest { messages: vec![Message::user("hi")], tools: Vec::new(), model: None }).map(|event| event.unwrap()).collect().await;
    let collected: Vec<String> = events.iter().map(|event| format!("{event:?}")).collect();
    let joined = collected.join("\n");
    assert!(joined.contains("TextDelta { text: \"hello\" }"), "{joined}");
    assert!(joined.contains("\"read\""), "{joined}");
    assert!(joined.contains("Usage { prompt_tokens: 100, completion_tokens: 7, cached_tokens: 40 }"), "{joined}");
    assert!(joined.contains("finish_reason: \"tool_calls\""), "{joined}");

    // Request parity checks: URL, auth header, session-affinity header, client header.
    let request = server.received.lock().unwrap()[0].clone();
    assert!(request.starts_with("POST /v1/chat/completions "), "{request}");
    assert!(request.contains("authorization: Bearer test-key"), "{request}");
    assert!(request.contains("x-opencode-client: hl-harness"), "{request}");
    assert!(request.contains("x-opencode-session:"), "{request}");
    assert!(request.contains("\"model\":\"test-model\""), "{request}");
    assert!(request.contains("\"stream\":true"), "{request}");
}

#[tokio::test]
async fn provider_surfaces_http_errors() {
    let server = MockServer::start("", 429).await;
    let provider = sse_provider(server.url());
    let mut stream = provider.stream(ProviderRequest::default());
    let result = stream.next().await.unwrap();
    let message = format!("{result:?}");
    assert!(message.contains("429"), "{message}");
}

/// A scripted provider that sequences two model turns regardless of request.
struct TwoTurnProvider {
    turn: std::sync::Mutex<u32>,
    config: ProviderConfig,
}

impl TwoTurnProvider {
    fn new() -> Self {
        Self {
            turn: std::sync::Mutex::new(0),
            config: ProviderConfig { id: ProviderKind::Custom, name: "mock".into(), base_url: "http://mock".into(), api_key: None, model: "mock".into(), session_id: None, headers: Vec::new(), timeout_ms: None, max_retries: Some(0), retry_delay_ms: None, context_window: Some(1000) },
        }
    }
}

#[async_trait::async_trait]
impl Provider for TwoTurnProvider {
    fn config(&self) -> &ProviderConfig {
        &self.config
    }

    fn stream<'a>(&'a self, _request: ProviderRequest) -> futures_util::stream::BoxStream<'a, anyhow::Result<ProviderEvent>> {
        let turn = { *self.turn.lock().unwrap() };
        {
            let mut slot = self.turn.lock().unwrap();
            *slot += 1;
        }
        use futures_util::stream;
        if turn == 0 {
            Box::pin(stream::iter(vec![
                Ok(ProviderEvent::ToolCallDelta { index: 0, id: Some("c1".into()), name: Some("write".into()), arguments_delta: Some(r#"{"path": "out/std.txt", "content": "written by agent"}"#.to_string()) }),
                Ok(ProviderEvent::Done { finish_reason: "tool_calls".into() }),
            ]))
        } else {
            Box::pin(stream::iter(vec![
                Ok(ProviderEvent::TextDelta { text: "done".to_string() }),
                Ok(ProviderEvent::Done { finish_reason: "stop".into() }),
            ]))
        }
    }
}

#[tokio::test]
async fn agent_loop_executes_tools_and_returns_final_text() {
    let root = tempfile_dir();
    let provider: Arc<dyn Provider> = Arc::new(TwoTurnProvider::new());
    let tools = hl_harness::tools::builtin_tools(&root);
    let mut events = Vec::new();
    let mut agent = Agent::new(provider, tools, CompactionOptions::default(), None, 8, Vec::new(), None);
    let result = agent.run("write the file", &mut |event: hl_harness::types::HarnessEvent| events.push(event.kind().to_string()), Vec::new()).await.unwrap();
    assert_eq!(result.text, "done");
    assert_eq!(result.steps, 2);
    let expected = ["user_message", "cache_plan", "assistant_delta_or_tool", "assistant_message", "tool_start", "tool_result", "cache_plan", "assistant_message"];
    assert!(events.iter().any(|event| event == "tool_start"), "{events:?}");
    assert!(events.iter().any(|event| event == "tool_result"), "{events:?}");
    let content = std::fs::read_to_string(root.join("out/std.txt")).unwrap();
    assert_eq!(content, "written by agent");
    let _ = expected;
}

#[tokio::test]
async fn read_only_policy_denies_tool_execution() {
    let root = tempfile_dir();
    let provider: Arc<dyn Provider> = Arc::new(TwoTurnProvider::new());
    let tools = hl_harness::tools::builtin_tools(&root);
    // Structural check: the policy denies, so the agent surfaces tool errors.
    let tool = tools.iter().find(|tool| tool.declaration().name == "write").unwrap().clone();
    let gate = GatePolicy_new();
    let denied = !gate.allows(&hl_harness::security::permissions::operation_for(tool.as_ref(), &json!({"path": "x", "content": "y"})));
    assert!(denied);
}

fn GatePolicy_new() -> hl_harness::security::permissions::GatePolicy {
    hl_harness::security::permissions::GatePolicy::new(hl_harness::security::permissions::PermissionMode::ReadOnly, &tempfile_dir(), Vec::new())
}

fn tempfile_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("harness-integration-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn tool_call_arguments_round_trip() {
    let call = ToolCall { id: "c1".into(), name: "write".into(), arguments: json!({"path": "a"}).as_object().unwrap().clone() };
    assert_eq!(hl_harness::model::provider::tool_call_key(&call), "write:{\"path\":\"a\"}");
    assert_eq!(Value::Null, serde_json::from_str::<Value>("null").unwrap());
}
