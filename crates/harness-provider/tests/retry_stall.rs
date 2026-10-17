//! Retry and stall behaviour, driven against a fake endpoint that answers with
//! a scripted sequence of responses and counts what it was asked.

use harness_core::{AgentMessage, ModelRequest, Provider, Sink, ToolSpec};
use harness_provider::anthropic::Anthropic;
use harness_provider::openai_compat::OpenAICompat;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

/// One canned response: (status line, body, extra headers).
type Reply = (&'static str, String, Vec<(&'static str, &'static str)>);

/// Serves `replies` in order, one per connection; joins with the request count.
fn scripted_server(
    listener: TcpListener,
    replies: Vec<Reply>,
    hold_after_body: bool,
) -> std::thread::JoinHandle<usize> {
    std::thread::spawn(move || {
        let mut served = 0;
        for (status, body, headers) in replies {
            let (mut sock, _) = listener.accept().expect("connect");
            let mut buf = [0u8; 65536];
            let _ = sock.read(&mut buf);
            let extra: String = headers
                .iter()
                .map(|(k, v)| format!("{k}: {v}\r\n"))
                .collect();
            // A stalled stream has to be connection-delimited: with a
            // content-length the client would see a complete (if tiny)
            // response and return normally instead of waiting for more.
            let head = if hold_after_body {
                format!(
                    "HTTP/1.1 {status}\r\ncontent-type: text/event-stream\r\n{extra}\
                     connection: keep-alive\r\n\r\n{body}"
                )
            } else {
                format!(
                    "HTTP/1.1 {status}\r\ncontent-type: text/event-stream\r\n{extra}\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
            };
            let _ = sock.write_all(head.as_bytes());
            let _ = sock.flush();
            served += 1;
            if hold_after_body {
                // Keep the socket open and silent: the client must decide the
                // stream is dead on its own.
                std::thread::sleep(Duration::from_secs(3));
            }
        }
        served
    })
}

fn anthropic_turn(text: &str) -> String {
    format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"usage\":{{\"input_tokens\":5,\"output_tokens\":1}}}}}}\n\n\
         event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\"}}}}\n\n\
         event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n\
         event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":2}}}}\n\n\
         event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
    )
}

fn openai_turn(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\n\
         data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
}

fn request() -> ModelRequest {
    ModelRequest {
        system: "sys".into(),
        tools: Vec::<ToolSpec>::new(),
        messages: vec![AgentMessage::User {
            content: "hi".into(),
            ts: 0,
            images: Vec::new(),
        }],
        model: "m".into(),
        max_tokens: 32,
        effort: None,
    }
}

struct Rec(String);
impl Sink for Rec {
    fn on_delta(&mut self, t: &str) {
        self.0.push_str(t);
    }
}

#[tokio::test]
async fn retries_a_rate_limit_then_completes() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = scripted_server(
        listener,
        vec![
            (
                "429 Too Many Requests",
                "slow down".into(),
                vec![("retry-after", "0")],
            ),
            ("200 OK", anthropic_turn("recovered"), vec![]),
        ],
        false,
    );

    let client = Anthropic::new("key".into(), "m".into())
        .with_base_url(&format!("http://{addr}"))
        .with_retry_base(10);
    let mut sink = Rec(String::new());
    let turn = client.complete(&request(), &mut sink).await.expect("turn");
    assert!(sink.0.contains("recovered"), "streamed: {:?}", sink.0);
    assert_eq!(turn.usage.input, 5);
    assert_eq!(server.join().unwrap(), 2, "one retry, then success");
}

#[tokio::test]
async fn a_client_error_is_not_retried() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = scripted_server(
        listener,
        vec![("400 Bad Request", "bad request".into(), vec![])],
        false,
    );

    let client = Anthropic::new("key".into(), "m".into())
        .with_base_url(&format!("http://{addr}"))
        .with_retry_base(10);
    let mut sink = Rec(String::new());
    let err = client.complete(&request(), &mut sink).await.unwrap_err();
    assert!(matches!(
        err,
        harness_core::ProviderError::Api { status: 400, .. }
    ));
    assert_eq!(server.join().unwrap(), 1, "no retry for our own mistake");
}

#[tokio::test]
async fn a_silent_stream_is_abandoned() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    // A stream that starts and then says nothing: the classic hang.
    let server = scripted_server(
        listener,
        vec![(
            "200 OK",
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n".into(),
            vec![],
        )],
        true,
    );

    let client = Anthropic::new("key".into(), "m".into())
        .with_base_url(&format!("http://{addr}"))
        .with_retry_base(10)
        .with_stall_timeout(Duration::from_millis(250));
    let mut sink = Rec(String::new());
    let started = std::time::Instant::now();
    let err = client.complete(&request(), &mut sink).await.unwrap_err();
    assert!(
        format!("{err}").contains("sent nothing"),
        "stall reported: {err}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "gave up promptly: {:?}",
        started.elapsed()
    );
    let _ = server.join();
}

#[tokio::test]
async fn the_compat_endpoint_retries_server_errors() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = scripted_server(
        listener,
        vec![
            ("503 Service Unavailable", "later".into(), vec![]),
            ("200 OK", openai_turn("compat ok"), vec![]),
        ],
        false,
    );

    let client = OpenAICompat::new(
        "compat",
        &format!("http://{addr}/v1"),
        "key".into(),
        "m".into(),
        128_000,
    )
    .with_retry_base(10);
    let mut sink = Rec(String::new());
    let turn = client.complete(&request(), &mut sink).await.expect("turn");
    assert!(sink.0.contains("compat ok"), "{:?}", sink.0);
    assert_eq!(turn.stop_reason, harness_core::StopReason::Stop);
    assert_eq!(server.join().unwrap(), 2);
}

/// A body that stops before `message_stop` is a cut connection, and an
/// `event: error` after the 200 is a failure — neither may come back as a
/// (truncated) successful turn. Both are transient, so the engine retries.
#[tokio::test]
async fn truncated_streams_and_mid_stream_errors_are_transient_failures() {
    let full = anthropic_turn("cut short");
    let truncated = full[..full.find("event: message_delta").unwrap()].to_string();
    let overloaded = format!(
        "{}event: error\ndata: {{\"type\":\"error\",\"error\":{{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}}}\n\n",
        truncated
    );
    for (body, want) in [
        (truncated, "message_stop"),
        (overloaded, "overloaded_error"),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = scripted_server(listener, vec![("200 OK", body, vec![])], false);
        let client = Anthropic::new("key".into(), "m".into())
            .with_base_url(&format!("http://{addr}"))
            .with_retry_base(10);
        let err = match client.complete(&request(), &mut Rec(String::new())).await {
            Ok(t) => panic!("accepted a cut stream as a turn: {:?}", t.blocks),
            Err(e) => e,
        };
        assert!(err.to_string().contains(want), "{err}");
        assert!(err.is_transient(), "{err}");
        server.join().unwrap();
    }
}
