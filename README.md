# nestjs-observe-oss

**An open-source NestJS Observe → OpenTelemetry bridge.**

Use the official [`@nestjs/observe`](https://www.npmjs.com/package/@nestjs/observe)
instrumentation while keeping control of where the telemetry goes.

```
NestJS application
       │
       ▼
  @nestjs/observe          ← the official SDK, unmodified
       │
       │  Observe protocol
       ▼
nestjs-observe-oss         ← this project
       │
       │  OTLP
       ▼
 any OTLP backend          ← Collector, Grafana/Tempo, Better Stack,
                             SigNoz, Honeycomb, Datadog, New Relic…
```

The SDK instruments a Nest application from the inside — it hooks the framework's
own lifecycle, so spans are named after your controllers, guards and services rather
than after HTTP routes. That knowledge is the reason to use it. This bridge exists
so you do not have to send it to a hosted service to benefit from it.

**It does not replace or fork the SDK.** `@nestjs/observe` already supports pointing
its agent at another collector, via the `endpoint` option or `OBSERVE_ENDPOINT`.
This is a collector that speaks its protocol and re-emits OTLP.

---

## Quick start

```bash
docker run -p 4319:4319 \
  -e OTLP_ENDPOINT=http://otel-collector:4318 \
  ghcr.io/OWNER/nestjs-observe-oss:latest
```

Then point the SDK at it — no code change needed:

```bash
OBSERVE_ENDPOINT=http://localhost:4319
```

That is the whole integration. The app keeps its existing `ObserveModule.forRoot()`.

### From source

```bash
cargo build --release
OTLP_ENDPOINT=http://localhost:4318 ./target/release/nestjs-observe-oss
```

---

## What you get

A request through a Nest application arrives at your backend with its structure
intact:

```
GET /users/:id                        SERVER   nestjs.protocol=http
├── AuthGuard.canActivate             INTERNAL nestjs.type=guard
├── LoggingInterceptor.intercept      INTERNAL nestjs.type=interceptor
└── UsersController.getUser           INTERNAL nestjs.type=controller
    └── UsersService.findUser         INTERNAL nestjs.type=service
        └── SELECT                    CLIENT   db.system.name=postgresql
```

That tree is taken from `tests/fixtures/http-requests.json`, which is a real capture
from the example application, and the test that asserts it is
[`tests/compatibility.rs`](tests/compatibility.rs).

| Signal | Status |
|---|---|
| Traces | Requests, GraphQL operations, RPC, WebSocket messages, queue jobs, scheduled runs, database and outbound-HTTP spans |
| Logs | With trace **and** span correlation |
| Metrics | Runtime (CPU, memory, event loop, GC) and application counters, gauges and summaries |
| Errors | Span status plus `exception` events with type, message and stack |

Nest-specific context is preserved under `nestjs.*`, and standard OpenTelemetry
semantic conventions are used wherever one exists. [`docs/mapping.md`](docs/mapping.md)
is the full reference.

---

## Configuration

All configuration is environment variables. No backend is special-cased.

| Variable | Default | Meaning |
|---|---|---|
| `OTLP_ENDPOINT` | `http://localhost:4318` | Base URL, no signal path. |
| `OTLP_PROTOCOL` | `http/protobuf` | `http/protobuf` (or `http`), or `http/json`. |
| `OTLP_HEADERS` | — | `key1=value1,key2=value2`. |
| `OTLP_TIMEOUT_SECONDS` | `10` | |
| `LISTEN_ADDR` | `0.0.0.0:4319` | |
| `OBSERVE_APP_KEY` | — | If set, batches must present this `x-api-key`. |
| `OBSERVE_APP_SECRET` | — | If set, batches must present this `x-api-secret`. |
| `RUST_LOG` | `info` | |

`OBSERVE_APP_KEY` and `OBSERVE_APP_SECRET` must be set together or not at all —
half-configured authentication reads as "on" while accepting everything, so it is
refused at start-up. When neither is set every batch is accepted and the bridge says
so in a warning.

`GET /health` answers `200 ok` for container probes.

### Examples

<details>
<summary>OpenTelemetry Collector</summary>

```bash
OTLP_ENDPOINT=http://otel-collector:4318
```
</details>

<details>
<summary>Grafana Cloud</summary>

```bash
OTLP_ENDPOINT=https://otlp-gateway-<zone>.grafana.net/otlp
OTLP_HEADERS=Authorization=Basic <base64 instanceID:token>
```
</details>

<details>
<summary>Better Stack</summary>

```bash
OTLP_ENDPOINT=https://<your-ingest-host>
OTLP_HEADERS=Authorization=Bearer <source-token>
```
</details>

<details>
<summary>Honeycomb</summary>

```bash
OTLP_ENDPOINT=https://api.honeycomb.io
OTLP_HEADERS=x-honeycomb-team=<api-key>
```
</details>

<details>
<summary>SigNoz, Datadog, New Relic and others</summary>

Any OTLP/HTTP receiver works the same way: set `OTLP_ENDPOINT` to the base URL and
put whatever the vendor wants in `OTLP_HEADERS`. Nothing about these vendors is
compiled in.
</details>

### docker-compose

```yaml
services:
  observe-bridge:
    image: ghcr.io/OWNER/nestjs-observe-oss:latest
    ports: ["4319:4319"]
    environment:
      OTLP_ENDPOINT: http://otel-collector:4318

  api:
    build: .
    environment:
      OBSERVE_ENDPOINT: http://observe-bridge:4319
      OBSERVE_APP_KEY: local-key
      OBSERVE_APP_SECRET: local-secret
```

---

## Trying it

[`examples/nestjs-app`](examples/nestjs-app) is a small NestJS application using the
real SDK, with a controller, service, guard, interceptor, a route that throws and a
scheduled task.

```bash
# 1. a backend — the collector's debug exporter, or anything OTLP
# 2. the bridge
OTLP_ENDPOINT=http://localhost:4318 cargo run --release

# 3. the app
cd examples/nestjs-app && npm install && npm start

# 4. traffic
curl localhost:3000/users/1
curl localhost:3000/users/missing/9    # a thrown NotFoundException
curl localhost:3000/users/boom/all     # an unhandled error
```

---

## How it works

```
  HTTP ingestion   POST /applications/telemetry, gzipped JSON
        ▼
  Observe decoder  src/observe/  — knows the wire format, nothing about OTel
        ▼
  Internal model   normalized; the seam the two halves meet at
        ▼
  Semantic mapper  src/mapping/  — knows OTel, nothing about the wire format
        ▼
  OTLP exporter    src/otlp/
```

The split is the point: the Observe wire format and the OpenTelemetry conventions
change independently, and neither should be able to force a change in the other.

Everything the bridge knows about the protocol was established by reading the
published SDK **and** capturing real traffic from an application running it —
[`docs/protocol.md`](docs/protocol.md) records both, including three fields where
the SDK's own wire contract disagrees with what it actually sends.

### Stateless by design

No queue, no retry, no buffer, no storage. A failed export is reported, not held.
That is deliberate: this sits in front of a real collector, and buffering, retry and
backpressure belong there rather than in two places.

The bridge answers `502` when it could not forward a batch, so the failure shows up
in the application's log instead of being reported as success.

---

## Testing

```bash
cargo test
```

165 tests. The ones that matter most are in
[`tests/compatibility.rs`](tests/compatibility.rs): they run the mapping against
captured output from a real agent and assert the resulting span tree, the recovered
Nest roles, the correlation ids and the metric units. **That file should fail when a
new SDK version changes the wire format** — when it does, re-capture the fixtures
(steps at the end of `docs/protocol.md`) before changing an assertion.

---

## Limitations

- **OTLP/gRPC is not implemented.** `OTLP_PROTOCOL=grpc` is refused with a message
  saying so. OTLP/HTTP is accepted by every backend listed above.
- **`nestjs.type` is inferred, not received.** The wire carries no component type at
  all; it is recovered from Nest's naming conventions and interface hooks, and can
  be wrong for unconventionally named classes. `nestjs.class.name` and
  `nestjs.method.name` are always exact. See
  [the mapping doc](docs/mapping.md#how-nestjstype-is-inferred-and-when-it-is-wrong).
- **CPU profiles are not bridged.** The SDK uploads them to a separate endpoint, and
  the feature is shelved upstream.
- **SLO declarations (`@Objective()`) are dropped.** They describe a promise, not an
  observation, and have no OpenTelemetry counterpart.
- No batching, retry or buffering — see above.

## Compatibility

Developed against **`@nestjs/observe@0.3.5`**. The SDK is pre-1.0 and its wire
format carries no version, so treat any minor bump as worth re-running the
compatibility suite.

## License

MIT. This project is not affiliated with or endorsed by the NestJS project.
`@nestjs/observe` is © its authors and is also MIT licensed.
