//! The embeddings interface (WP-AI-03.T6): request bounds, response checks, the mock embedder
//! and the stable `llm.embeddings_unsupported` code.

use std::sync::Arc;

use spot_llm::embed::{MAX_EMBED_INPUT_BYTES, MAX_EMBED_INPUTS};
use spot_llm::{EmbedRequest, EmbedResponse, Embedder, LlmError, MockEmbedder, Usage};

/// A request over `texts` for the test model.
fn request(texts: &[&str]) -> EmbedRequest {
    EmbedRequest::new(
        "embed-model",
        texts.iter().map(|text| (*text).to_owned()).collect(),
    )
}

/// Awaits one embed call, panicking with the error when it fails.
async fn embedded(embedder: &dyn Embedder, request: EmbedRequest) -> EmbedResponse {
    match embedder.embed(request).await {
        Ok(response) => response,
        Err(error) => panic!("embed failed: {error}"),
    }
}

/// The detail of a bad-request error, panicking on any other shape.
fn detail_of(error: LlmError) -> String {
    match error {
        LlmError::BadRequest { detail } => detail,
        other => panic!("expected llm.bad_request, got {other}"),
    }
}

#[test]
fn a_request_at_the_limits_is_valid() {
    let at_count = EmbedRequest::new("m", vec!["x".to_owned(); MAX_EMBED_INPUTS]);
    assert_eq!(at_count.validate(), Ok(()));
    let at_size = EmbedRequest::new("m", vec!["x".repeat(MAX_EMBED_INPUT_BYTES)]);
    assert_eq!(at_size.validate(), Ok(()));
    assert_eq!(request(&["a"]).with_dimensions(64).validate(), Ok(()));
}

#[test]
fn a_breach_is_a_bad_request_that_never_quotes_a_text() {
    let secret = "sk-planted-secret-text";

    let too_many = EmbedRequest::new("m", vec![secret.to_owned(); MAX_EMBED_INPUTS + 1]);
    let detail = detail_of(too_many.validate().err().unwrap_or(LlmError::Auth));
    assert!(detail.contains("257 inputs"), "{detail}");
    assert!(!detail.contains(secret), "{detail}");

    let too_big = EmbedRequest::new(
        "m",
        vec![
            secret.to_owned(),
            format!("{secret}{}", "y".repeat(MAX_EMBED_INPUT_BYTES)),
        ],
    );
    let detail = detail_of(too_big.validate().err().unwrap_or(LlmError::Auth));
    assert!(detail.contains("input 1"), "{detail}");
    assert!(!detail.contains(secret), "{detail}");

    let none = EmbedRequest::new("m", Vec::new());
    assert!(detail_of(none.validate().err().unwrap_or(LlmError::Auth)).contains("no input"));

    let no_model = EmbedRequest::new("  ", vec![secret.to_owned()]);
    assert!(detail_of(no_model.validate().err().unwrap_or(LlmError::Auth)).contains("no model"));

    let zero = request(&[secret]).with_dimensions(0);
    assert!(detail_of(zero.validate().err().unwrap_or(LlmError::Auth)).contains("zero"));
}

#[test]
fn debug_prints_counts_never_texts_or_vectors() {
    let secret = "sk-planted-secret-text";
    let printed = format!("{:?}", request(&[secret, "other"]));
    assert!(!printed.contains(secret), "{printed}");
    assert!(printed.contains("inputs: 2"), "{printed}");

    let response = EmbedResponse {
        vectors: vec![vec![0.123_456_f32, 0.5]],
        usage: Usage::ZERO,
        model: "embed-model".to_owned(),
    };
    let printed = format!("{response:?}");
    assert!(!printed.contains("0.123456"), "{printed}");
    assert!(printed.contains("width: 2"), "{printed}");
}

#[test]
fn a_response_is_checked_against_its_request() {
    let asked = request(&["a", "b"]);
    let good = EmbedResponse {
        vectors: vec![vec![1.0, 2.0], vec![3.0, 4.0]],
        usage: Usage::ZERO,
        model: "m".to_owned(),
    };
    assert_eq!(good.validate_against(&asked), Ok(()));
    assert_eq!(good.width(), 2);

    let with = |vectors: Vec<Vec<f32>>| EmbedResponse {
        vectors,
        ..good.clone()
    };
    for (bad, what) in [
        (with(vec![vec![1.0]]), "count"),
        (with(vec![vec![1.0], Vec::new()]), "empty"),
        (with(vec![vec![1.0], vec![f32::NAN]]), "nan"),
        (with(vec![vec![f32::INFINITY], vec![1.0]]), "infinite"),
        (with(vec![vec![1.0], vec![1.0, 2.0]]), "width"),
    ] {
        match bad.validate_against(&asked) {
            Err(error) => assert_eq!(error.code(), "llm.protocol", "{what}: {error}"),
            Ok(()) => panic!("{what} must be refused"),
        }
    }
}

#[tokio::test]
async fn the_mock_is_deterministic_and_per_text() {
    let embedder = MockEmbedder::new(8);
    let first = embedded(&embedder, request(&["alpha", "beta", "alpha"])).await;
    assert_eq!(first.vectors.len(), 3);
    assert!(first.vectors.iter().all(|vector| vector.len() == 8));
    assert_eq!(first.vectors[0], first.vectors[2]);
    assert_ne!(first.vectors[0], first.vectors[1]);
    assert!(
        first
            .vectors
            .iter()
            .flatten()
            .all(|value| value.is_finite() && *value != 0.0)
    );
    assert_eq!(first.model, "embed-model");
    assert_eq!(first.validate_against(&request(&["a", "b", "c"])), Ok(()));

    // A second embedder (and a second call) gives the same vectors for the same text.
    let again = embedded(&MockEmbedder::new(8), request(&["beta"])).await;
    assert_eq!(again.vectors[0], first.vectors[1]);
    assert_eq!(first.vectors[0], MockEmbedder::vector_for("alpha", 8));
}

#[tokio::test]
async fn the_mock_honours_dimensions_and_counts_tokens() {
    let embedder = MockEmbedder::new(8);
    let reply = embedded(&embedder, request(&["abcd", "abcde"]).with_dimensions(3)).await;
    assert_eq!(reply.width(), 3);
    // 4 bytes is one token, 5 bytes is two (one per four bytes, rounded up).
    assert_eq!(reply.usage.input_tokens, 3);
    assert_eq!(reply.usage.output_tokens, 0);
}

#[tokio::test]
async fn the_mock_checks_bounds_like_a_provider() {
    let embedder = MockEmbedder::new(4);
    match embedder.embed(EmbedRequest::new("m", Vec::new())).await {
        Err(error) => assert_eq!(error.code(), "llm.bad_request"),
        Ok(_) => panic!("an empty request must be refused"),
    }
    // The refused request is still recorded: a test can see what was attempted.
    assert_eq!(embedder.recorded_count(), 1);
}

#[tokio::test]
async fn the_mock_fails_on_demand_and_records_every_request() {
    let embedder = MockEmbedder::new(4);
    embedder.fail_next(LlmError::Overloaded);
    match embedder.embed(request(&["a"])).await {
        Err(LlmError::Overloaded) => {}
        other => panic!("the queued failure must fire first, got {other:?}"),
    }
    // Only the next call failed.
    let ok = embedded(&embedder, request(&["a"])).await;
    assert_eq!(ok.vectors.len(), 1);
    assert_eq!(embedder.recorded_count(), 2);
    assert_eq!(embedder.requests()[1].inputs, vec!["a".to_owned()]);

    let broken = MockEmbedder::new(4).failing_with(LlmError::Auth);
    for _ in 0..2 {
        match broken.embed(request(&["a"])).await {
            Err(LlmError::Auth) => {}
            other => panic!("a failing embedder always fails, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn smart_pointers_forward() {
    let shared: Arc<dyn Embedder> = Arc::new(MockEmbedder::new(4).titled("shared"));
    assert_eq!(shared.id(), "shared");
    assert_eq!(embedded(shared.as_ref(), request(&["a"])).await.width(), 4);
    assert_eq!(embedded(&shared, request(&["a"])).await.width(), 4);

    let boxed: Box<dyn Embedder> = Box::new(MockEmbedder::new(5));
    assert_eq!(boxed.id(), "mock-embedder");
    assert_eq!(embedded(&boxed, request(&["a"])).await.width(), 5);
}

#[test]
fn the_unsupported_code_is_stable() {
    let error = LlmError::EmbeddingsUnsupported;
    assert_eq!(error.code(), "llm.embeddings_unsupported");
    assert!(!error.retryable());
    assert_eq!(error.retry_after(), None);
    assert!(error.to_string().starts_with("llm.embeddings_unsupported"));
    assert_eq!(
        LlmError::from_code("llm.embeddings_unsupported"),
        Some(LlmError::EmbeddingsUnsupported)
    );
    let wire = match serde_json::to_value(&error) {
        Ok(wire) => wire,
        Err(e) => panic!("did not serialize: {e}"),
    };
    assert_eq!(
        wire,
        serde_json::json!({ "code": "llm.embeddings_unsupported" })
    );
    let back: LlmError = match serde_json::from_value(wire) {
        Ok(back) => back,
        Err(e) => panic!("did not deserialize: {e}"),
    };
    assert_eq!(back, error);
}
