//! A deterministic, in-process fake of the OpenAI-compatible embeddings endpoint
//! (`POST /v1/embeddings`), for the cases that need the product to embed text without a model or
//! the network beyond loopback (WP-AI-10.T4).
//!
//! The vectors are derived from the text with integers only, so a run is repeatable and no
//! random source or clock is read:
//!
//! * a fixed **topic table** gives a word an axis; a text's vector counts its topic words per
//!   axis, so a query and a document that share a topic word point the same way (their cosine
//!   is above zero) and a text with other topic words is orthogonal to them (cosine zero);
//! * a text with no topic word at all lands on one hash-chosen axis outside the topic axes, so
//!   it is never the zero vector (which has no direction) and two equal texts are equal vectors.
//!
//! The vectors are not unit length: the store ranks by cosine, which ignores length, and the
//! harness never does float arithmetic (the workspace denies it).
//!
//! The fake counts the requests it served and remembers the texts it was asked to embed, so a
//! case can prove "no re-index": after a permission change, the next query embeds the query and
//! nothing else. It refuses a request without the expected bearer key when it was started with
//! one. It listens on `127.0.0.1:0`, so it needs the product's egress policy to allow that
//! loopback address for the LLM purpose (the fixture writes that entry).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value as Json, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// The vector width the fixture configures, and the width agent memory needs.
pub const DIMENSIONS: usize = 64;

/// The topic words and the axis each one owns. Words are lower-case ASCII letters and digits.
pub const TOPICS: [(&str, usize); 8] = [
    ("harbor", 0),
    ("survey", 1),
    ("quay", 2),
    ("treasury", 3),
    ("ledger", 4),
    ("vault", 5),
    ("payroll", 6),
    ("audit", 7),
];

/// The first axis the hash fallback may use.
const FALLBACK_START: usize = 8;

/// The largest request the fake reads (header plus body).
const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;

/// FNV-1a over the bytes: a fixed, dependency-free hash.
fn fnv1a(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The integer vector of `text` at `dimensions` (at least 9): topic-word counts per axis, or a
/// single hash-chosen axis when the text holds no topic word.
pub fn embed_counts(text: &str, dimensions: usize) -> Vec<u32> {
    let width = dimensions.max(FALLBACK_START + 1);
    let mut counts = vec![0u32; width];
    let lowered = text.to_ascii_lowercase();
    let mut any = false;
    for word in lowered.split(|c: char| !c.is_ascii_alphanumeric()) {
        if let Some((_, axis)) = TOPICS.iter().find(|(topic, _)| *topic == word) {
            counts[*axis] += 1;
            any = true;
        }
    }
    if !any {
        let spread = (width - FALLBACK_START) as u64;
        let axis = FALLBACK_START + (fnv1a(text) % spread) as usize;
        counts[axis] = 1;
    }
    counts.truncate(dimensions.max(1));
    if counts.iter().all(|count| *count == 0) {
        // A width below the topic axes cut every counted axis away: keep a direction anyway.
        let last = counts.len() - 1;
        counts[last] = 1;
    }
    counts
}

/// The vector as the endpoint sends it: the integer counts as `f32` values (casts, no float
/// arithmetic).
pub fn embed_vector(text: &str, dimensions: usize) -> Vec<f32> {
    embed_counts(text, dimensions)
        .into_iter()
        .map(|count| count as f32)
        .collect()
}

/// What the fake remembers and counts, shared with its connection tasks.
struct State {
    dimensions: usize,
    expected_key: Option<String>,
    calls: AtomicUsize,
    refused: AtomicUsize,
    inputs: Mutex<Vec<String>>,
}

/// The running fake. Dropping it stops the listener.
pub struct EmbedFake {
    addr: SocketAddr,
    state: Arc<State>,
    task: JoinHandle<()>,
}

impl EmbedFake {
    /// Binds `127.0.0.1:0` and serves until dropped. `expected_key` is the bearer key a request
    /// must carry (`None` accepts any request, including one without credentials).
    ///
    /// # Errors
    ///
    /// The bind failed.
    pub async fn start(dimensions: usize, expected_key: Option<&str>) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let state = Arc::new(State {
            dimensions,
            expected_key: expected_key.map(str::to_owned),
            calls: AtomicUsize::new(0),
            refused: AtomicUsize::new(0),
            inputs: Mutex::new(Vec::new()),
        });
        let served = Arc::clone(&state);
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let state = Arc::clone(&served);
                tokio::spawn(async move {
                    serve(stream, state).await;
                });
            }
        });
        Ok(Self { addr, state, task })
    }

    /// The listening port.
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// The base URL a tenant's `endpoint` names (the provider appends `/v1/embeddings`).
    pub fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.addr.port())
    }

    /// How many embeddings requests were served (an unauthorised one is not counted).
    pub fn calls(&self) -> usize {
        self.state.calls.load(Ordering::SeqCst)
    }

    /// How many requests were refused for a missing or wrong key.
    pub fn refused(&self) -> usize {
        self.state.refused.load(Ordering::SeqCst)
    }

    /// Every text embedded so far, in request order. For a case's own assertions: it is kept in
    /// the harness process only and no failure message should print it.
    pub fn inputs(&self) -> Vec<String> {
        self.state
            .inputs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl Drop for EmbedFake {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// One HTTP request: its method, path, lower-cased header names and body.
struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Reads one request, or `None` when the peer closed early, sent junk or sent too much.
async fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(at) = find(&buffer, b"\r\n\r\n") {
            break at;
        }
        if buffer.len() > MAX_REQUEST_BYTES {
            return None;
        }
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_owned();
    let path = request_line.next()?.to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let mut rest: Vec<u8> = buffer[head_end + 4..].to_vec();
    let chunked = headers.iter().any(|(key, value)| {
        key == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked")
    });
    let length = headers
        .iter()
        .find(|(key, _)| key == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok());
    let body = if chunked {
        loop {
            if let Some(body) = decode_chunked(&rest) {
                break body;
            }
            if rest.len() > MAX_REQUEST_BYTES {
                return None;
            }
            let read = stream.read(&mut chunk).await.ok()?;
            if read == 0 {
                return None;
            }
            rest.extend_from_slice(&chunk[..read]);
        }
    } else {
        let length = length.unwrap_or(0);
        if length > MAX_REQUEST_BYTES {
            return None;
        }
        while rest.len() < length {
            let read = stream.read(&mut chunk).await.ok()?;
            if read == 0 {
                return None;
            }
            rest.extend_from_slice(&chunk[..read]);
        }
        rest.truncate(length);
        rest
    };
    Some(Request {
        method,
        path,
        headers,
        body,
    })
}

/// The index of the first `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// The body of a complete chunked message, or `None` while more bytes are needed or the framing
/// is broken (the caller then reads more, up to its size bound).
fn decode_chunked(data: &[u8]) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    let mut at = 0usize;
    loop {
        let line_end = at + find(&data[at..], b"\r\n")?;
        let size_text = std::str::from_utf8(&data[at..line_end]).ok()?;
        let size_text = size_text.split(';').next()?.trim();
        let size = usize::from_str_radix(size_text, 16).ok()?;
        at = line_end + 2;
        if size == 0 {
            return Some(body);
        }
        if data.len() < at + size + 2 {
            return None;
        }
        body.extend_from_slice(&data[at..at + size]);
        at += size + 2;
    }
}

/// Answers one connection: one request, one reply, then close.
async fn serve(mut stream: TcpStream, state: Arc<State>) {
    let Some(request) = read_request(&mut stream).await else {
        return;
    };
    let (status, body) = answer(&request, &state);
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Error",
    };
    let reply = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(reply.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn error_body(message: &str, kind: &str) -> String {
    json!({"error": {"message": message, "type": kind}}).to_string()
}

/// The status and JSON body for one request.
fn answer(request: &Request, state: &State) -> (u16, String) {
    if request.method != "POST" || request.path != "/v1/embeddings" {
        return (404, error_body("no such route", "not_found"));
    }
    if let Some(key) = &state.expected_key {
        let wanted = format!("Bearer {key}");
        if request.header("authorization") != Some(wanted.as_str()) {
            state.refused.fetch_add(1, Ordering::SeqCst);
            return (
                401,
                error_body("the key is missing or wrong", "invalid_request_error"),
            );
        }
    }
    let Ok(parsed) = serde_json::from_slice::<Json>(&request.body) else {
        return (
            400,
            error_body("the body is not JSON", "invalid_request_error"),
        );
    };
    let inputs: Vec<String> = match parsed.get("input") {
        Some(Json::String(text)) => vec![text.clone()],
        Some(Json::Array(items)) => {
            let texts: Vec<String> = items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect();
            if texts.len() != items.len() {
                return (
                    400,
                    error_body("every input must be a string", "invalid_request_error"),
                );
            }
            texts
        }
        _ => {
            return (
                400,
                error_body(
                    "`input` must be a string or a list",
                    "invalid_request_error",
                ),
            );
        }
    };
    let width = parsed
        .get("dimensions")
        .and_then(Json::as_u64)
        .and_then(|asked| usize::try_from(asked).ok())
        .filter(|asked| *asked > 0)
        .unwrap_or(state.dimensions);
    let model = parsed
        .get("model")
        .and_then(Json::as_str)
        .unwrap_or("l3-fake-embedding")
        .to_owned();
    let data: Vec<Json> = inputs
        .iter()
        .enumerate()
        .map(|(index, text)| {
            json!({
                "object": "embedding",
                "index": index,
                "embedding": embed_vector(text, width),
            })
        })
        .collect();
    // One token per four bytes, rounded up: the number the usage block reports.
    let tokens: usize = inputs.iter().map(|text| text.len().div_ceil(4)).sum();
    state.calls.fetch_add(1, Ordering::SeqCst);
    state
        .inputs
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .extend(inputs);
    (
        200,
        json!({
            "object": "list",
            "data": data,
            "model": model,
            "usage": {"prompt_tokens": tokens, "total_tokens": tokens},
        })
        .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dot(a: &[u32], b: &[u32]) -> u64 {
        a.iter()
            .zip(b)
            .map(|(x, y)| u64::from(*x) * u64::from(*y))
            .sum()
    }

    /// One raw HTTP exchange against the fake: the status line's code and the body.
    async fn post(fake: &EmbedFake, auth: Option<&str>, body: &str) -> (u16, Json) {
        let mut stream = TcpStream::connect(fake.addr).await.unwrap();
        let credential = auth
            .map(|key| format!("Authorization: Bearer {key}\r\n"))
            .unwrap_or_default();
        let request = format!(
            "POST /v1/embeddings HTTP/1.1\r\nHost: 127.0.0.1\r\n{credential}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).await.unwrap();
        let text = String::from_utf8(reply).unwrap();
        let code: u16 = text.split_whitespace().nth(1).unwrap().parse().unwrap();
        let payload = text.split("\r\n\r\n").nth(1).unwrap_or("null");
        (code, serde_json::from_str(payload).unwrap_or(Json::Null))
    }

    #[test]
    fn the_vector_is_deterministic_and_has_the_asked_width() {
        for width in [9, 16, DIMENSIONS, 200] {
            let first = embed_counts("The harbor survey of the quay", width);
            let second = embed_counts("The harbor survey of the quay", width);
            assert_eq!(first, second);
            assert_eq!(first.len(), width);
            assert!(first.iter().any(|count| *count > 0));
        }
        assert_eq!(embed_vector("harbor", 16).len(), 16);
    }

    #[test]
    fn a_text_with_no_topic_word_is_never_the_zero_vector() {
        for text in ["", "hello", "completely unrelated words", "12345"] {
            let vector = embed_counts(text, DIMENSIONS);
            assert!(vector.iter().any(|count| *count > 0), "{text:?}");
            assert_eq!(vector.iter().filter(|count| **count > 0).count(), 1);
        }
        // A very narrow width keeps a direction too.
        assert!(embed_counts("harbor", 1).iter().any(|count| *count > 0));
    }

    #[test]
    fn a_shared_topic_word_is_near_and_another_topic_is_far() {
        let query = embed_counts("harbor survey", DIMENSIONS);
        let same_topic = embed_counts("Harbor survey of the northern quay", DIMENSIONS);
        let other_topic = embed_counts("Treasury ledger vault", DIMENSIONS);
        let unrelated = embed_counts("nothing to see here", DIMENSIONS);
        assert!(dot(&query, &same_topic) > 0, "a shared topic word is near");
        assert_eq!(dot(&query, &other_topic), 0, "another topic is orthogonal");
        assert_eq!(dot(&query, &unrelated), 0, "no topic word is orthogonal");
        assert_eq!(dot(&other_topic, &unrelated), 0);
    }

    #[test]
    fn case_and_punctuation_do_not_change_a_topic_word() {
        assert_eq!(
            embed_counts("HARBOR, Survey!", DIMENSIONS),
            embed_counts("harbor survey", DIMENSIONS)
        );
    }

    #[tokio::test]
    async fn it_serves_deterministic_vectors_and_counts_the_calls() {
        let fake = EmbedFake::start(DIMENSIONS, None).await.unwrap();
        let body =
            r#"{"model":"m","input":["harbor survey","treasury"],"encoding_format":"float"}"#;
        let (code, first) = post(&fake, None, body).await;
        assert_eq!(code, 200);
        let (_, second) = post(&fake, None, body).await;
        assert_eq!(first["data"], second["data"]);
        assert_eq!(first["data"].as_array().unwrap().len(), 2);
        assert_eq!(first["data"][1]["index"], json!(1));
        assert_eq!(
            first["data"][0]["embedding"].as_array().unwrap().len(),
            DIMENSIONS
        );
        assert_eq!(fake.calls(), 2);
        assert_eq!(fake.inputs().len(), 4);
        assert_eq!(fake.inputs()[0], "harbor survey");
    }

    #[tokio::test]
    async fn the_asked_width_wins_over_the_default() {
        let fake = EmbedFake::start(DIMENSIONS, None).await.unwrap();
        let body = r#"{"model":"m","input":["harbor"],"dimensions":12}"#;
        let (code, reply) = post(&fake, None, body).await;
        assert_eq!(code, 200);
        assert_eq!(reply["data"][0]["embedding"].as_array().unwrap().len(), 12);
    }

    #[tokio::test]
    async fn a_request_without_the_expected_key_is_refused_and_not_counted() {
        let fake = EmbedFake::start(DIMENSIONS, Some("l3-key")).await.unwrap();
        let body = r#"{"model":"m","input":["harbor"]}"#;
        let (code, _) = post(&fake, None, body).await;
        assert_eq!(code, 401);
        let (code, _) = post(&fake, Some("wrong"), body).await;
        assert_eq!(code, 401);
        assert_eq!((fake.calls(), fake.refused()), (0, 2));
        assert!(fake.inputs().is_empty());
        let (code, _) = post(&fake, Some("l3-key"), body).await;
        assert_eq!(code, 200);
        assert_eq!((fake.calls(), fake.refused()), (1, 2));
    }

    #[tokio::test]
    async fn a_bad_body_or_route_is_an_error_status() {
        let fake = EmbedFake::start(DIMENSIONS, None).await.unwrap();
        let (code, _) = post(&fake, None, "not json").await;
        assert_eq!(code, 400);
        let (code, _) = post(&fake, None, r#"{"model":"m","input":[1]}"#).await;
        assert_eq!(code, 400);
        let (code, _) = post(&fake, None, r#"{"model":"m"}"#).await;
        assert_eq!(code, 400);
        assert_eq!(fake.calls(), 0);
    }

    #[test]
    fn a_chunked_body_decodes_when_complete_and_waits_when_not() {
        let whole = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        assert_eq!(decode_chunked(whole).unwrap(), b"hello world");
        assert!(decode_chunked(&whole[..whole.len() - 4]).is_none());
        assert!(decode_chunked(b"zz\r\nhello\r\n").is_none());
    }
}
