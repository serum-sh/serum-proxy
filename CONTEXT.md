# serum-proxy

The customer-deployed proxy for serum. It maintains a WebSocket connection to serum and forwards HTTP calls to internal services on serum's behalf.

## Language

### Core Concepts

**serum**:
The cloud service providing the on-call incident response assistance.
_Avoid_: SaaS, server, controller, capitalized "Serum".

**Proxy**:
This application.
_Avoid_: agent, client, gateway.

**Backend**:
An HTTP service the proxy forwards serum requests to. Defined by that forwarding relationship, not by being telemetry: examples span observability stores (Prometheus, Elasticsearch) and cluster control-plane state (the Kubernetes API).
_Avoid_: origin, downstream.

**Operator**:
The customer staff member running the proxy on their infrastructure; the audience for the local admin endpoints (`/healthz`, `/egress`).
_Avoid_: admin, user, customer.

### Wire protocol

**Frame**:
One typed message on the WebSocket; the tagged union discriminated by `type`.
_Avoid_: message, packet, event.

**Envelope**:
The versioned frame schema shared with serum: the contract, not any one frame. The shared protocol vectors live in `docs/protocol-vectors.json`.
_Avoid_: schema, protocol types, DTO.

### Request handling

**Guardrails**:
The per-service safety policy (method and path allowlist plus concurrency cap) that performs admission.
_Avoid_: filters, middleware, policy engine.

### Transparency

**Egress panel**:
The in-memory ring buffer of recently forwarded requests, served to the operator at `/egress`; response bodies are never retained.
_Avoid_: log, audit trail, history.

**Egress entry**:
One row in the egress panel: identity (request_id, target_service, method, stripped path) plus result (status or error_code, bytes).
_Avoid_: record, log line.

**Redaction / RedactionSummary**:
What the proxy stripped from a response body before forwarding (rules fired, bytes removed); surfaced to the egress panel and the serum-side view.
_Avoid_: scrubbing, sanitization, masking.
