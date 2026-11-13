# serum WebSocket Protocol (v1)

The wire contract between serum and the customer-deployed proxy. This document is the spec.

Two implementations speak this protocol: the serum service, and the serde types in this repo's
`src/envelope.rs`.

Neither implementation is the source of truth. **The source of truth is the shared protocol
vectors, [`protocol-vectors.json`](protocol-vectors.json).** Both sides conform to the vectors; the
vectors do not conform to either side. See "Shared vectors and cross-repo parity" below.

## Transport framing

- One long-lived outbound WebSocket from the proxy to serum (`wss://`).
- Every WebSocket text message is exactly one JSON object: one frame.
- Every frame carries a top-level `type` discriminator. Parsers MUST route on `type` and MUST
  reject an unknown `type` rather than degrade silently.

```
type in {"hello", "heartbeat", "request", "response", "error"}
```

## Versioning

- `protocol_version` is an integer present on every post-handshake frame, pinned to `1` for this
  version. A frame whose `protocol_version` is not `1` MUST be rejected at the deserialize
  boundary (it is not a v1 frame).
- The `hello` frame is the exception: it is version-agnostic, because it is where versions are
  discovered. It carries no pinned `protocol_version`.
- Version negotiation: the proxy sends `hello` with `protocol_versions_supported`; serum picks
  the highest value in the intersection of its own supported set and the proxy's, and acks with a
  `hello` carrying that `protocol_version`. If the intersection is empty serum closes the
  WebSocket with close code `4001` (UNSUPPORTED_PROTOCOL). Mid-session version change is not
  supported.
- A version bump forks the frame types: v2 gets a parallel set of frame types alongside v1.

## Serialization contract

This contract makes a frame from serum and a frame from the proxy comparable.

- **Key order and whitespace are NOT part of the contract.** Equality is JSON *value* equality.
- **Omit, do not null (omit-None).** An optional field whose value is absent is omitted from the
  JSON entirely; it is NOT emitted as `null`. On the proxy side this is serde
  `#[serde(skip_serializing_if = "Option::is_none")]`. A reader MUST accept both an omitted field
  and an explicit `null` as "absent" (be liberal on input), but a writer MUST omit (be strict on
  output). The vectors are written in omitted form.
- Fields with a non-None default (`protocol_version`, `is_streaming`, `chunk_index`, `is_final`,
  `body`, `retryable`) are always present on the wire; they are never omitted.

## Frames

### request (serum -> proxy)

Asks the proxy to make one HTTP call against a local backend.

| field | type | notes |
|---|---|---|
| `type` | `"request"` | |
| `protocol_version` | `1` | |
| `request_id` | string | UUIDv7 in 32-char lowercase hex (`^[0-9a-f]{32}$`). Correlation key across the multiplexed tunnel and the log joins on both sides. |
| `target_service` | string | The kind key (e.g. `prometheus`). The proxy maps it to the URL and auth of the service with that `type` in local config; serum never sees the backend credentials. |
| `method` | `"GET"` \| `"POST"` | Closed set. Any other method is rejected. |
| `path` | string | URL path only; no query string. |
| `query` | object, optional | Map of name to a single value (string) or repeated values (array of strings). See "Query serialization". |
| `headers` | object, optional | The proxy strips `Authorization` on receipt and never forwards it; auth attaches from the proxy's local config. |
| `body` | string, optional | UTF-8 only in v1. Binary is out of scope (would be added additively as `body_encoding`). |

### response (proxy -> serum)

The proxy's reply to one `request`, matched on `request_id`. Carries **exactly one of** a backend
reply or a proxy-level error - never both:

- **Backend reply:** `error` is absent. `status_code` and `body` describe what the upstream
  returned, *including* upstream 4xx/5xx (those are still valid backend replies, not transport
  failures).
- **Proxy-level error:** `error` is set. `status_code`/`headers` are not meaningful and consumers
  MUST ignore them.

| field | type | notes |
|---|---|---|
| `type` | `"response"` | |
| `protocol_version` | `1` | |
| `request_id` | string | Matches the originating `request`. |
| `is_streaming` | bool | Describes the whole response, not this frame. `false` => single frame total. The current proxy always sends `false`. |
| `chunk_index` | int | `0` for the head frame; increments per chunk when streaming. |
| `is_final` | bool | `true` terminates the response, always - including error responses. |
| `status_code` | int, optional | Set on the head frame (`chunk_index == 0`) only. Omitted on later chunks. |
| `headers` | object, optional | Head frame only, like `status_code`. |
| `body` | string | Always present (may be empty). |
| `error` | object, optional | Typed proxy-level error (see "Error codes"). Mutually exclusive with a backend reply. |
| `redaction_summary` | object, optional | `{rules_fired: [string], bytes_redacted: int}`. Present only when the proxy redacted bytes from `body`. The current proxy doesn't set it. |

### error (proxy -> serum)

An out-of-band protocol error not tied to a specific request, for example a malformed frame from
serum. Per-request failures use `response.error` instead. serum does not send `error` frames; it
closes the WebSocket with a close code.

| field | type | notes |
|---|---|---|
| `type` | `"error"` | |
| `protocol_version` | `1` | |
| `error` | object | Typed error (see "Error codes"). |

### hello (either direction, on connect)

| field | type | notes |
|---|---|---|
| `type` | `"hello"` | |
| `protocol_versions_supported` | array of int, optional | Proxy -> serum: the versions this proxy speaks. |
| `protocol_version` | int, optional | serum -> proxy: the negotiated version ack. Version-agnostic field (not pinned to 1). |
| `proxy_build_version` | string, optional | Proxy build, e.g. `0.1.0`. |

`hello` is the one frame that accepts unknown fields: it negotiates the version, so the receiver
cannot assume the field set of the sender.

### heartbeat (proxy -> serum)

| field | type | notes |
|---|---|---|
| `type` | `"heartbeat"` | |
| `protocol_version` | `1` | |
| `uptime_seconds` | int | |
| `requests_handled_since_last` | int | |

Interval 30s. serum marks a proxy as stale when its heartbeats stop.

## Query serialization

`query` is a map of name to either one string or an array of strings. The proxy reassembles it
into the outbound URL. **A list value MUST serialize as one repeated `name=value` pair per item**
(`match[]=a&match[]=b`), never as a single stringified `name=['a','b']` pair. This matches Python
`urlencode(..., doseq=True)` and, on the proxy side, per-item `form_urlencoded` /
`Url::query_pairs_mut().append_pair(...)`. A naive serializer that stringifies the list silently
breaks Prometheus `/api/v1/series`. The vectors include a list-valued case to lock this in.

## Error codes

A closed set. The proxy emits them in `response.error` (per-request) or in an `error` frame
(out-of-band).

| code | meaning | retryable |
|---|---|---|
| `403_ALLOWLIST` | Path outside the allowlist (built-in or operator `path_allowlist`), or unsafe (e.g. `..`, `//`) | no |
| `404_UNKNOWN_SERVICE` | `target_service` not configured in this proxy | no |
| `405_METHOD` | Path has an allowlist entry only for a different method (e.g. `POST /*/_mapping`). A method outside {GET, POST} fails earlier at deserialize, not as a 405. | no |
| `413_BODY_LIMIT` | Upstream response exceeded 5 MB | no |
| `429_CONCURRENT_REQUESTS` | Per-service concurrency cap exceeded | yes |
| `502_BACKEND_UNREACHABLE` | Backend refused / DNS / network unreachable | varies |
| `504_TIMEOUT` | Upstream exceeded the 15s timeout | yes |
| `PROXY_DISCONNECTED` | Tunnel dropped mid-request | yes |
| `VERSION_MISMATCH` | No protocol-version overlap at handshake | no |
| `FRAME_PARSE_FAILED` | Malformed / unknown-shape frame on the wire | no |

`retryable` is advisory. The proxy does not retry a request at the moment.

## Validation layers

Two distinct layers. The shared vectors only standardize the first.

1. **Deserialize-boundary rejections (`must_reject` vectors).** Both implementations reject these
   while parsing, with no extra step: unknown `type`, `protocol_version != 1` on a post-handshake
   frame, an unknown field (except on `hello`), a method outside {GET, POST}, an unknown error
   code.
2. **Semantic validations (per-repo, NOT in the vectors).** These are required by the spec but
   enforced at *different layers* in each language, so they cannot be expressed as a shared
   parse-fails vector:
   - **`request_id` shape** (`^[0-9a-f]{32}$`): serum enforces it at parse; the proxy validates
     it via `is_valid_request_id()` because it is the *receiver* of requests and checks on
     receipt.
   - **Head-fields-only-on-first-chunk** (a `chunk_index > 0` frame must omit `status_code` /
     `headers`): serum enforces it at parse; the proxy validates it via
     `ResponseFrame::check_head_fields()`. The proxy is the *sender* of responses and is
     expected to construct valid ones; serum validates on receipt.

   Each repo's own test suite covers its semantic validations. This asymmetry is intentional:
   each side validates strictly what it *receives*.

## Shared vectors and cross-repo parity

[`protocol-vectors.json`](protocol-vectors.json) is the contract artifact. Structure:

```
{ "protocol_version": 1,
  "vectors":     [ { "description": "...", "frame": { ... } }, ... ],   // must round-trip
  "must_reject": [ { "description": "...", "frame": { ... } }, ... ] }  // must fail to parse
```

- **Round-trip (`vectors`).** Each side deserializes the frame into its native type, re-serializes
  with the omit-None contract, and asserts JSON *value* equality against the original. A vector one
  side rejects, or round-trips to a different value, is a contract break.
- **Rejection (`must_reject`).** Each side asserts the frame fails to parse.
- **Tests.** Proxy: `tests/protocol_vectors.rs`. serum runs the same checks in its own test suite.
- **Parity.** `protocol-vectors.json` is canonical. serum keeps an identical copy and checks that
  the two SHA-256 hashes are equal. A non-additive change requires a `protocol_version` bump (see
  "Versioning" above). Adding a vector is an additive change.

Streaming responses and binary bodies are expected to be additive (new vectors, no bump), unless
they change the shape of an existing frame.
