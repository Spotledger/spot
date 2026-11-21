//! Anthropic has no embeddings endpoint (WP-AI-03.T6): the provider implements `Embedder` and
//! answers with the fixed `llm.embeddings_unsupported` error, never touching the transport.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use spot_llm::provider::BoxFuture;
use spot_llm::transport::{ByteStream, HttpRequest, HttpResponse, HttpTransport, TransportError};
use spot_llm::{ApiKey, EmbedRequest, Embedder as _, LlmError};
use spot_llm_anthropic::AnthropicProvider;

/// The key the test plants; it must appear in no error.
const KEY: &str = "sk-ant-planted-key-0123456789";

/// A transport that only counts how often it is called.
struct Counting {
    calls: AtomicUsize,
}

impl HttpTransport for Counting {
    fn send<'a>(
        &'a self,
        _request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, TransportError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(TransportError::Protocol("not expected".to_owned()))
        })
    }

    fn send_stream<'a>(
        &'a self,
        _request: HttpRequest,
    ) -> BoxFuture<'a, Result<ByteStream, TransportError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(TransportError::Protocol("not expected".to_owned()))
        })
    }
}

#[tokio::test]
async fn anthropic_refuses_embeddings_without_a_network_call() {
    let transport = Arc::new(Counting {
        calls: AtomicUsize::new(0),
    });
    let provider = AnthropicProvider::new(
        "https://anthropic.test",
        ApiKey::new(KEY),
        transport.clone(),
        Vec::new(),
    );
    let request = EmbedRequest::new("any-model", vec!["planted input text".to_owned()]);

    let error = match provider.embed(request).await {
        Err(error) => error,
        Ok(response) => panic!("Anthropic must not embed, got {response:?}"),
    };

    assert_eq!(error, LlmError::EmbeddingsUnsupported);
    assert_eq!(error.code(), "llm.embeddings_unsupported");
    assert!(!error.retryable());
    for printed in [error.to_string(), format!("{error:?}")] {
        assert!(!printed.contains(KEY), "{printed}");
        assert!(!printed.contains("planted input text"), "{printed}");
    }
    assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn an_invalid_request_gets_the_same_fixed_answer() {
    let transport = Arc::new(Counting {
        calls: AtomicUsize::new(0),
    });
    let provider = AnthropicProvider::new(
        "https://anthropic.test",
        ApiKey::new(KEY),
        transport.clone(),
        Vec::new(),
    );
    let request = EmbedRequest::new("any-model", Vec::new());
    assert_eq!(
        provider.embed(request).await.err(),
        Some(LlmError::EmbeddingsUnsupported)
    );
    assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
}
