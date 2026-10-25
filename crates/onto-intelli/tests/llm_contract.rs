//! The LLM connection contract (`LLM_*`) end to end against a local HTTP
//! server: gateway mode sends the gateway token as a bearer token and no
//! provider key, a rotated key file applies to the next request, and
//! retries follow `LLM_MAX_RETRIES`.
#![cfg(feature = "http-llm")]

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use onto_intelli::config::LlmConfig;
use onto_intelli::llm::{self, ChatMessage, LlmContract, LlmRequest};

/// What the server saw: the path and the headers of each request.
type Seen = Arc<Mutex<Vec<(String, HashMap<String, String>)>>>;

/// Answers each request with the next (status, body) of `script`.
async fn server(script: Vec<(u16, String)>) -> (String, Seen) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen: Seen = Arc::default();
    let script = Arc::new(Mutex::new(VecDeque::from(script)));
    let s = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let (seen, script) = (s.clone(), script.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let head_end = loop {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let mut lines = head.lines();
                let path = lines.next().unwrap_or_default().split(' ').nth(1).unwrap_or_default().to_string();
                let headers: HashMap<String, String> = lines
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
                    .collect();
                let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
                while buf.len() < head_end + len {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                seen.lock().push((path, headers));
                let (status, body) = script.lock().pop_front().unwrap_or((500, "{}".into()));
                let resp = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    (base, seen)
}

fn contract(vars: &[(&str, &str)]) -> LlmConfig {
    let m: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let c = LlmContract::from_vars(|k| m.get(k).cloned()).unwrap().unwrap();
    let mut cfg = LlmConfig::default();
    c.apply(&mut cfg, &BTreeSet::new());
    cfg
}

fn ask() -> LlmRequest {
    LlmRequest { system: "s".into(), messages: vec![ChatMessage::user("hi")], max_tokens: 16 }
}

const ANTHROPIC_OK: &str = r#"{"content":[{"type":"text","text":"hello"}],"model":"m","stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":1}}"#;
const OPENAI_OK: &str = r#"{"choices":[{"message":{"content":"hello"},"finish_reason":"stop"}],"model":"m","usage":{"prompt_tokens":3,"completion_tokens":1}}"#;

#[tokio::test]
async fn gateway_mode_sends_the_gateway_token_and_retries() {
    let (base, seen) = server(vec![(503, "{}".into()), (200, ANTHROPIC_OK.into()), (200, ANTHROPIC_OK.into())]).await;
    let dir = std::env::temp_dir().join(format!("onto-llm-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let token = dir.join("gateway-token");
    std::fs::write(&token, "tok-1\n").unwrap();
    let cfg = contract(&[
        ("LLM_GATEWAY_URL", &format!("{base}/v1")),
        ("LLM_GATEWAY_TOKEN_FILE", token.to_str().unwrap()),
        ("LLM_API_KEY", "sk-provider-must-not-be-sent"),
        ("LLM_MODEL", "claude-opus-5-5"),
        ("LLM_MAX_RETRIES", "2"),
    ]);
    let provider = llm::build(&cfg).unwrap();
    assert_eq!((provider.name(), provider.model()), ("anthropic", "claude-opus-5-5"));
    assert!(provider.unavailable().is_none());
    // A 503 first: retried.
    assert_eq!(provider.complete(&ask()).await.unwrap().text, "hello");
    // The token rotated in the mounted Secret: the next request uses the new one.
    std::fs::write(&token, "tok-2").unwrap();
    provider.complete(&ask()).await.unwrap();
    let seen = seen.lock().clone();
    assert_eq!(seen.len(), 3, "one retry, then one more call");
    for (path, h) in &seen {
        assert_eq!(path, "/v1/messages");
        assert!(!h.contains_key("x-api-key"), "no provider key in gateway mode");
        assert!(!h.values().any(|v| v.contains("sk-provider")));
    }
    assert_eq!(seen[0].1["authorization"], "Bearer tok-1");
    assert_eq!(seen[2].1["authorization"], "Bearer tok-2");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn retries_stop_at_the_limit_and_the_key_file_is_required() {
    let (base, seen) = server(vec![(500, "{}".into()), (500, "{}".into()), (500, "{}".into())]).await;
    let dir = std::env::temp_dir().join(format!("onto-llm-key-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let key = dir.join("api-key");
    std::fs::write(&key, "sk-file").unwrap();
    let cfg = contract(&[
        ("LLM_PROVIDER", "openai-compatible"),
        ("LLM_BASE_URL", &format!("{base}/v1")),
        ("LLM_API_KEY_FILE", key.to_str().unwrap()),
        ("LLM_MODEL", "local"),
        ("LLM_MAX_RETRIES", "1"),
    ]);
    let provider = llm::build(&cfg).unwrap();
    let err = provider.complete(&ask()).await.unwrap_err().to_string();
    assert!(err.contains("500"), "{err}");
    assert_eq!(seen.lock().len(), 2, "the first try and one retry");
    assert_eq!(seen.lock()[0].1["authorization"], "Bearer sk-file");
    assert_eq!(seen.lock()[0].0, "/v1/chat/completions");
    // A missing key file fails the request, naming the file, not the key.
    std::fs::remove_file(&key).unwrap();
    let err = provider.complete(&ask()).await.unwrap_err().to_string();
    assert!(err.contains("LLM key file"), "{err}");
    let (base, _) = server(vec![(200, OPENAI_OK.into())]).await;
    let cfg = contract(&[
        ("LLM_PROVIDER", "openai-compatible"),
        ("LLM_BASE_URL", &format!("{base}/v1")),
        ("LLM_API_KEY", "sk-text"),
        ("LLM_MODEL", "local"),
    ]);
    assert_eq!(llm::build(&cfg).unwrap().complete(&ask()).await.unwrap().text, "hello");
    std::fs::remove_dir_all(&dir).unwrap();
}
