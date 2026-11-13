# serum proxy

Open-source, customer-deployed proxy for serum. It facilitates communication from serum, especially in behind-firewall scenarios.

The proxy holds one long-lived outbound WebSocket to the serum service. The service sends HTTP-shaped envelopes over the tunnel; the proxy runs them against locally-configured backends (Prometheus, Elasticsearch, Kubernetes, more...) and returns the responses. See [Trust model](#trust-model) for what serum can and cannot do through the proxy.

Wire contract (envelope spec and shared protocol vectors): [`docs/protocol.md`](docs/protocol.md).

## Quick start

### Public Container Image

Replace `<public-image>` with the public image path (TBD).

1. Copy `config.example.yaml` to `config.yaml` and edit the `services` section to point at your internal Prometheus / Elasticsearch.

2. Set the proxy token issued by serum:

   ```sh
   export SERUM_PROXY_TOKEN=<token>
   ```

3. Run the image:

   ```sh
   docker run --rm \
     -e SERUM_PROXY_TOKEN \
     -e ES_TOKEN \
     -v "$PWD/config.yaml:/app/config.yaml:ro" \
     -p 127.0.0.1:8088:8088 \
     <public-image>
   ```

4. Confirm it is connected:

   ```sh
   curl http://127.0.0.1:8088/healthz
   ```

### Build Image from Source

Build the image:

```sh
docker build -t serum-proxy:dev .
```

Then do the steps in [Public Container Image](#public-container-image), with `serum-proxy:dev` in place of `<public-image>`.

## Operator endpoints

Both bind to `proxy.admin_bind` from `config.yaml`. Left unset it is `SERUM_ADMIN_BIND` (the image sets `0.0.0.0:8088`), otherwise `127.0.0.1:8088`. These are internal and for the cluster operator. They are not exposed to serum.

| Path       | Purpose                                                                                       |
|------------|-----------------------------------------------------------------------------------------------|
| `/healthz` | WS connection state, seconds since last frame, configured services. For Docker healthchecks.  |
| `/egress`  | Last N (default 200) requests, also guardrail denials: target, status, bytes. No bodies.      |

## Shutdown

On `SIGTERM` or `SIGINT`, the proxy stops reading new requests from serum. It waits up to 10s for the requests in progress, closes the WebSocket with close code 1000 (Normal) and exits. If a request is still in progress after 10s, the proxy exits without the close frame.

## Trust model

The proxy runs in your network. serum sends requests to the proxy, the proxy sends them to your backends, and the proxy returns the replies to serum.

What serum controls in a request:

- The service `type`, the method, the path, the query parameters and the body. The [guardrails](#safety-guardrails) and the [allowlists](#built-in-allowlists) limit the method and the path.
- The request headers. The proxy removes `Authorization` and forwards all other headers without change. The proxy adds the headers from its config after the headers from serum. If serum sends a header that the config also sets, the backend gets both values.

What serum gets back:

- The backend status code, all response headers and the full body, up to 5 MB. The proxy does not remove data from the body. The proxy does not follow redirects, so a `3xx` reply and its `Location` header go back to serum.
- Error messages. An error message can contain the backend URL, with the host, the port and the path.
- The proxy version (the `hello` frame in [`docs/protocol.md`](docs/protocol.md)).

What serum cannot do:

- Read the backend credentials. The proxy adds them from the local config.
- Change the config. The proxy reads the config one time, at startup.
- Send a request to a service that is not in the config, or to a server other than the `base_url` of the service. serum names a service by its `type`, not by an address. The proxy maps the `type` to the `base_url` from its config.
- Use a method or a path that the allowlist does not permit.
- Connect to the operator endpoints.

What you should do:

- Give the proxy read-only backend credentials with the minimum permissions whenever possible.
- For the kubernetes connector, use the RBAC template [`deploy/kubernetes/rbac.yaml`](deploy/kubernetes/rbac.yaml). It gives the `serum-proxy` ServiceAccount least required privileges.
- Use `/egress` to see the requests that serum sent.

## Safety guardrails

- HTTP `GET` and `POST` only. `POST` is for Elasticsearch search (`/*/_search`) only.
- Per-service-type method and path allowlist enforced before any outbound call.
- 15s upstream timeout, no redirects followed.
- 5 MB cap on the upstream response body.
- Per-service concurrency cap (default 10; queue ceiling at 2x).

Allowlists are logged at startup.

## Built-in allowlists

Each `type` has a built-in read-only allowlist: the reads that serum sends. Each entry has one method. A path with no entry gives 403. A path with an entry for a different method gives 405. The allowlists are in [`src/guardrails.rs`](src/guardrails.rs).

| Type            | Method | Paths |
|-----------------|--------|-------|
| `prometheus`    | `GET`  | `/api/v1/query`, `/api/v1/query_range`, `/api/v1/series`, `/api/v1/label/*/values`, `/api/v1/metadata` |
| `elasticsearch` | `GET`  | `/_cat/indices/*`, `/*/_mapping` |
| `elasticsearch` | `POST` | `/*/_search` |
| `kubernetes`    | `GET`  | `/api/v1/namespaces/*/pods`, `/api/v1/namespaces/*/events` |

Patterns are globs. For all types, a `*` matches one path segment: it does not match `/`.

`path_allowlist` (paths only, same glob rules) makes the built-in list shorter. It cannot add a path or a method. A path that `path_allowlist` does not match gives 403. A `path_allowlist` entry that overlaps no built-in entry (e.g. `/api/v1/admin/*`) allows nothing, and the proxy logs a warning for it at startup.

The proxy builds the outbound URL first and rejects (403) a path that URL normalization changes: `..`, `%2e%2e`, `\`, `?`, `#` or a character that gets percent-encoded. It also rejects `%2F` and `%5C`, because a backend can decode them to a separator. It also rejects an empty segment (`//` or a trailing `/`), so that a `*` cannot match an empty value. Thus the allowlist checks the path that the backend gets. If `base_url` has a path (e.g. `http://host/prometheus`), the allowlist checks only the part after it.

## Configuration

See `config.example.yaml` for the schema. `${VAR}` references in the file are interpolated from the environment at startup. `SERUM_PROXY_TOKEN` is read from the environment.

Kubernetes deployment manifests live in [`deploy/kubernetes/`](deploy/kubernetes/README.md).

## Development

All cargo commands run inside a container, so the host needs only Docker. `make help` lists the targets; common ones are `make check`, `make test`, `make fmt`, `make fmt-check`, and `make lint`.

`fmt`/`lint` need `rustfmt`/`clippy`, which the slim Rust image omits, so the Makefile builds a small `serum-proxy-toolchain:dev` image (Dockerfile `toolchain` stage) with those components plus `pkg-config` baked in. It is built automatically on first use and reused after that.

### Lints

Lint levels live in the `[lints]` tables of `Cargo.toml`; lint options live in `clippy.toml`. `make lint` treats every warning as an error. `clippy::unwrap_used` is set in `src/lib.rs` and `src/main.rs` instead, so tests can still unwrap. To silence a lint, use `#[expect(<lint>, reason = "...")]`, not `#[allow]`: an `expect` that no longer fires is itself a warning, so stale exceptions show up.

### Updating the dev docker image

The image is built only if it does not already exist; it is not rebuilt when the Dockerfile changes, and `make clean` does not remove it (that target only clears the build cache volume). To pick up a Dockerfile change or a Rust version bump, drop the image so the next `make` rebuilds it:

```sh
docker rmi serum-proxy-toolchain:dev
```
