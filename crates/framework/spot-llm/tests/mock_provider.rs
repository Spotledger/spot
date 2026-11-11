//! The mock provider: scripting, streaming equivalence, failure mapping, recording,
//! and the tokio-clock pause.

use std::time::Duration;

use spot_llm::stream::{assemble, drain};
use spot_llm::{
    Capabilities, LlmError, Message, MockProvider, Provider, Request, Response, StopReason, Usage,
};

/// A request for `request_id` with one user line.
fn request(id: &str) -> Request {
    Request::new("mock-model", id).with_message(Message::user("hello"))
}

/// A plain-text response.
fn response(text: &str, usage: Usage) -> Response {
    Response::new(Message::assistant(text), StopReason::Stop, usage)
}

#[tokio::test]
async fn responds_and_records() {
    let provider = MockProvider::new()
        .responds_to("r1", response("hi", Usage::ZERO))
        .responds_to("r2", response("ho", Usage::ZERO));

    let reply = match provider.complete(request("r1")).await {
        Ok(reply) => reply,
        Err(e) => panic!("complete failed: {e}"),
    };
    assert_eq!(reply.text(), "hi");
    assert_eq!(reply.stop_reason, StopReason::Stop);

    assert_eq!(provider.recorded_count(), 1);
    let recorded = provider.requests();
    assert_eq!(recorded[0].request_id, "r1");
    assert_eq!(recorded[0].messages[0].text_content(), "hello");
    // Steps are reusable: the same id answers again, and both calls are recorded.
    assert_eq!(
        provider.complete(request("r1")).await.map(|r| r.text()),
        Ok("hi".to_owned())
    );
    assert_eq!(provider.recorded_count(), 2);
}

#[tokio::test]
async fn stream_matches_complete() {
    let usage = Usage {
        input_tokens: 5,
        output_tokens: 3,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
    };
    let provider = MockProvider::new().responds_to("s1", response("hello world", usage));

    let completed = match provider.complete(request("s1")).await {
        Ok(completed) => completed,
        Err(e) => panic!("complete failed: {e}"),
    };
    let events = match provider.stream(request("s1")).await {
        Ok(mut stream) => drain(&mut stream).await,
        Err(e) => panic!("stream failed: {e}"),
    };
    let mut assembled_input = Vec::new();
    for event in events {
        match event {
            Ok(event) => assembled_input.push(event),
            Err(e) => panic!("stream yielded {e}"),
        }
    }
    let assembled = match assemble(&assembled_input) {
        Ok(assembled) => assembled,
        Err(e) => panic!("events did not assemble: {e}"),
    };
    assert_eq!(assembled, completed);
    assert_eq!(assembled.usage, usage);
}

#[tokio::test]
async fn scripted_events_stream_verbatim_and_complete_assembles() {
    let events = vec![
        spot_llm::StreamEvent::TextDelta {
            delta: "ab".to_owned(),
        },
        spot_llm::StreamEvent::TextDelta {
            delta: "cd".to_owned(),
        },
        spot_llm::StreamEvent::Usage { usage: Usage::ZERO },
        spot_llm::StreamEvent::Stop {
            reason: StopReason::Stop,
        },
    ];
    let provider = MockProvider::new().streams_to("e1", events.clone());

    let got = match provider.stream(request("e1")).await {
        Ok(mut stream) => drain(&mut stream).await,
        Err(e) => panic!("stream failed: {e}"),
    };
    let unwrapped: Vec<_> = got
        .into_iter()
        .map(|item| {
            item.ok()
                .unwrap_or_else(|| spot_llm::StreamEvent::TextDelta {
                    delta: String::new(),
                })
        })
        .collect();
    assert_eq!(unwrapped, events);

    let completed = match provider.complete(request("e1")).await {
        Ok(completed) => completed,
        Err(e) => panic!("complete failed: {e}"),
    };
    assert_eq!(completed.text(), "abcd");
}

#[tokio::test]
async fn failures_map_on_both_paths() {
    let provider = MockProvider::new().fails_for(
        "f1",
        LlmError::RateLimited {
            retry_after: Some(7),
        },
    );

    let completed = provider.complete(request("f1")).await;
    let Err(error) = completed else {
        panic!("complete should have failed");
    };
    assert_eq!(error.code(), "llm.rate_limited");
    assert!(error.retryable());
    assert_eq!(error.retry_after(), Some(Duration::from_secs(7)));

    let streamed = provider.stream(request("f1")).await;
    let Err(error) = streamed else {
        panic!("stream should have failed");
    };
    assert_eq!(error.code(), "llm.rate_limited");
}

#[tokio::test]
async fn unscripted_requests_fail_as_bad_request() {
    let provider = MockProvider::new().responds_to("other", response("hi", Usage::ZERO));
    let error = provider.complete(request("mine")).await;
    let Err(error) = error else {
        panic!("an unscripted request must fail");
    };
    assert_eq!(error.code(), "llm.bad_request");
    assert!(!error.retryable());
    assert!(
        error.to_string().contains("mine"),
        "the detail names the request id only"
    );
    assert!(
        !error.to_string().contains("hello"),
        "the detail never quotes the body"
    );
}

#[tokio::test(start_paused = true)]
async fn pause_sleeps_on_the_tokio_clock() {
    let provider = MockProvider::new()
        .pauses_for("p1", 30_000)
        .responds_to("p1", response("after the pause", Usage::ZERO));

    let start = tokio::time::Instant::now();
    let reply = match provider.complete(request("p1")).await {
        Ok(reply) => reply,
        Err(e) => panic!("complete failed: {e}"),
    };
    // A paused clock advances by the pause without wall-clock delay: the sleep was real.
    assert_eq!(start.elapsed(), Duration::from_secs(30));
    assert_eq!(reply.text(), "after the pause");
}

#[test]
fn ids_and_capabilities_are_plumbed() {
    let provider = MockProvider::titled("mock-2").with_capabilities(Capabilities {
        context_window: 1_000,
        max_output: 100,
        tools: false,
        streaming: false,
        vision: false,
        prompt_caching: true,
    });
    assert_eq!(provider.id(), "mock-2");
    let caps = provider.capabilities("any-model");
    assert_eq!(caps.context_window, 1_000);
    assert!(caps.prompt_caching);
    assert!(!caps.tools);
    // A fresh mock reports the sample capability floor.
    let fresh = MockProvider::new();
    assert_eq!(fresh.id(), "mock");
    assert_eq!(fresh.capabilities("m").context_window, 128_000);
    assert!(fresh.capabilities("m").streaming);
}

#[tokio::test]
async fn the_mock_implements_provider_across_arc() {
    use std::sync::Arc;

    let provider: Arc<dyn Provider> =
        Arc::new(MockProvider::new().responds_to("a1", response("via arc", Usage::ZERO)));
    let reply = match provider.complete(request("a1")).await {
        Ok(reply) => reply,
        Err(e) => panic!("complete failed: {e}"),
    };
    assert_eq!(reply.text(), "via arc");
}
