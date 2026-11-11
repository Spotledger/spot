# spot-link-external

The external carrier of the tenant link (WP-CTL-11.T1, ADR-058 rule 12, ADR-080): a peer that is
not a platform tenant exchanges **message topics** with a tenant under a grant, outbound through
a connector or a signed webhook, inbound through a signature-verified webhook, with credentials
by reference only and the same delivery states, audit and conditions as a tenant link.

The crate's module documentation and rustdoc carry the detail; this file carries the one thing a
future HTTP task must not have to rediscover: **how an inbound body is allowed to arrive.**

## Inbound route contract

A webhook route for an external peer is served in exactly this order, and nothing is skipped:

1. **Read the body under the peer's cap** — `ExternalReceiver::read(path, headers, chunks)`.
   It takes the peer's own `max_body_bytes` and hands the chunks to
   `InboundRequest::from_stream(path, headers, chunks, cap)`. There is no other way to build an
   `InboundRequest` in a product build: `body` is private and `InboundRequest::new` (an
   already-buffered `Vec`) exists only under `#[cfg(any(test, feature = "testing"))]`.
2. **Handle it** — `ExternalReceiver::handle(&link, &request, now_ms)`, which verifies the
   signature, checks the link, then feeds the tenant link's own `Inbox::receive`.
3. **Answer** the peer from the returned `InboundResponse` (status, stable code, fixed body).

### What `read` / `from_stream` guarantee

- A declared `Content-Length` above the cap — or one that is not a whole number of bytes — is
  refused **before one chunk is pulled**, so a header cannot buy memory.
- A chunked or header-less body is counted as it arrives: the read stops **one byte past the
  cap** and the buffer is reserved to the cap at most, so neither the sender's stream nor its
  chunk size decides the allocation. The rest of the stream is left unread.
- A body of exactly the cap is accepted; the receiver refuses only what is larger.
- The cap is never optional: `ExternalReceiver::read` refuses a peer whose cap is zero or does
  not fit a `usize` (`link_external.invalid_peer`) instead of reading without one.
- Refusals are `link_external.body_too_large` (413) and, for an unreadable length or a failed
  chunk read, `link_external.invalid_body` (400). Neither error, nor any log line, carries a
  header value, a body byte or the configured cap.

### What the route must not do

- Do not collect the body first (`read_to_end`, a whole `Bytes` frame, a `String` of the
  payload) and then build a request: that is the defect this crate now makes unrepresentable.
- Do not pass a cap of your own: the peer's cap is the only cap, and `read` is the only entry.
- Do not answer a `body_too_large` refusal by reading the rest of the stream to "drain" it —
  stop, answer, and close.

## Tests

`tests/capped_body.rs` holds the reader's own tests (one byte past the cap, a declared length
refused with nothing read, a body at the cap, a peer with no cap); `tests/inbound.rs` holds the
end-to-end ones. The proof that the buffered constructor is absent from a default build is the
`compile_fail` doctest in `src/receiver.rs`: it runs whenever the `testing` feature is off, which
is every product build.
