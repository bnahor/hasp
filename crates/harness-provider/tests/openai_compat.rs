//! End-to-end against a fake OpenAI-compatible endpoint: a real HTTP server
//! hand-rolled on a `std::TcpListener` (no new dependencies) streaming real
//! SSE chunks, including fragmented tool calls and a trailing usage chunk.

use harness_core::{
    AgentMessage, ContentBlock, ModelRequest, Provider, Sink, StopReason, ToolSpec,
};
use harness_provider::openai_compat::{OpenAICompat, list_models};
use serde_json::json;
use std::io::{Read, Write};
use std::net::TcpListener;

/// Serve one HTTP request with `status`/`content_type`/`body`; joins with the
/// request head so tests can assert on what we actually sent.
fn serve_once(
    listener: TcpListener,
    status: &str,
    content_type: &str,
    body: String,
) -> std::thread::JoinHandle<String> {
    let (status, content_type) = (status.to_string(), content_type.to_string());
    std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("client connects");
        let mut buf = vec![0u8; 65536];
        let n = sock.read(&mut buf).expect("read request");
        let head = String::from_utf8_lossy(&buf[..n]).to_string();
        let resp = format!(
            "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        );
        sock.write_all(resp.as_bytes()).expect("write response");
        head
    })
}

/// Serve several requests in order from one thread, returning each request's
/// head. Two `serve_once` threads on clones of one listener would race for the
/// first connection, and the loser would wait forever for one more.
fn serve_seq(
    listener: TcpListener,
    responses: Vec<(&'static str, &'static str, String)>,
) -> std::thread::JoinHandle<Vec<String>> {
    std::thread::spawn(move || {
        responses
            .into_iter()
            .map(|(status, content_type, body)| {
                let (mut sock, _) = listener.accept().expect("client connects");
                let mut buf = vec![0u8; 65536];
                let n = sock.read(&mut buf).expect("read request");
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let resp = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                );
                sock.write_all(resp.as_bytes()).expect("write response");
                head
            })
            .collect()
    })
}

struct Rec(Vec<String>);
impl Sink for Rec {
    fn on_delta(&mut self, t: &str) {
        self.0.push(format!("d:{t}"));
    }
    fn on_thinking(&mut self, t: &str) {
        self.0.push(format!("t:{t}"));
    }
}

fn req() -> ModelRequest {
    ModelRequest {
        system: "sys".into(),
        tools: vec![ToolSpec {
            name: "bash".into(),
            description: "run".into(),
            input_schema: json!({"type": "object"}),
        }],
        messages: vec![AgentMessage::User {
            content: "hi".into(),
            ts: 0,
            images: Vec::new(),
        }],
        model: "m-x".into(),
        max_tokens: 64,
    }
}

#[tokio::test]
async fn streams_thinking_text_and_fragmented_tool_calls() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let sse = concat!(
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking hard\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\" there\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"ba\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"sh\",\"arguments\":\"{\\\"command\\\":\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"ls\\\"}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7,\"prompt_tokens_details\":{\"cached_tokens\":3}}}\n\n",
        "data: [DONE]\n\n",
    )
    .to_string();
    let handle = serve_once(listener, "200 OK", "text/event-stream", sse);

    let p = OpenAICompat::new(
        "compat",
        &format!("http://{addr}/v1"),
        "key".into(),
        "m-x".into(),
        128_000,
    );
    let mut sink = Rec(vec![]);
    let turn = p.complete(&req(), &mut sink).await.expect("turn ok");
    let head = handle.join().expect("server thread");

    assert!(head.contains("\"model\":\"m-x\""), "model in request");
    assert!(head.contains("\"name\":\"bash\""), "tool schema in request");
    assert!(head.contains("stream_options"), "usage requested");

    assert!(sink.0.contains(&"d:Hello".to_string()));
    assert!(sink.0.contains(&"d: there".to_string()));
    assert!(sink.0.contains(&"t:thinking hard".to_string()));

    match turn.blocks.as_slice() {
        [
            ContentBlock::Thinking { thinking, .. },
            ContentBlock::Text { text },
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
            },
        ] => {
            assert_eq!(thinking, "thinking hard");
            assert_eq!(text, "Hello there");
            assert_eq!(id, "call_1");
            assert_eq!(name, "bash");
            assert_eq!(arguments, &json!({"command": "ls"}));
        }
        other => panic!("unexpected blocks: {other:?}"),
    }
    assert_eq!(turn.stop_reason, StopReason::ToolUse);
    assert_eq!(turn.usage.input, 11);
    assert_eq!(turn.usage.output, 7);
    assert_eq!(turn.usage.cache_read, 3);
}

#[tokio::test]
async fn discovers_models_from_the_endpoint() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = serve_once(
        listener,
        "200 OK",
        "application/json",
        r#"{"data":[{"id":"llama-x:8b"},{"id":"qwen-y:32b"}]}"#.into(),
    );
    let models = list_models(&format!("http://{addr}/v1"), "k")
        .await
        .expect("list");
    assert_eq!(models, vec!["llama-x:8b", "qwen-y:32b"]);
    let head = handle.join().expect("server thread");
    assert!(
        head.contains("GET /v1/models"),
        "hits the models route: {head}"
    );
}

#[tokio::test]
async fn retries_without_stream_options_when_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    // First request: rejected exactly the way pre-stream_options servers do.
    let server = serve_seq(
        listener,
        vec![
            (
                "400 Bad Request",
                "application/json",
                r#"{"error":{"message":"unknown field: stream_options"}}"#.into(),
            ),
            (
                "200 OK",
                "text/event-stream",
                "data: {\"choices\":[{\"delta\":{\"content\":\"fine\"}}]}\n\n\
                 data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                 data: [DONE]\n\n"
                    .into(),
            ),
        ],
    );

    let p = OpenAICompat::new(
        "compat",
        &format!("http://{addr}/v1"),
        "key".into(),
        "m".into(),
        128_000,
    );
    struct Noop;
    impl Sink for Noop {
        fn on_delta(&mut self, _t: &str) {}
    }
    let turn = p.complete(&req(), &mut Noop).await.expect("turn ok");
    let heads = server.join().expect("server thread");
    let (first, second) = (&heads[0], &heads[1]);
    assert!(first.contains("stream_options"));
    assert!(!second.contains("stream_options"), "retried without it");
    assert_eq!(turn.stop_reason, StopReason::Stop);
}
