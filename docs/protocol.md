# The NestJS Observe wire protocol

What `@nestjs/observe` puts on the network, as observed from **version 0.3.5**.

Two sources, and where they disagree the second one wins:

1. The SDK's published source — its `src/encoders/*` key maps and
   `src/agent/telemetry-wire-contract.js`, which is the SDK's own hand-written copy
   of the agreement it has with the hosted collector.
2. Real traffic from a NestJS application running the unmodified SDK, captured by
   decompressing what it POSTs. `tests/fixtures/` holds sanitized captures, and the
   compatibility tests run against them.

Three fields differ between the two. Those are called out under
[Where the contract and reality disagree](#where-the-contract-and-reality-disagree);
they are the reason this document is written from captures rather than from reading
the SDK alone.

---

## Transport

| | |
|---|---|
| Protocol | HTTP/1.1 |
| Method | `POST` |
| Path | `{endpoint}/applications/telemetry` |
| Encoding | gzip |
| Body | JSON |
| Batching | Yes — one batch per flush |
| Flush interval | `flushInterval`, default 5s, floor 1s |

`endpoint` is the SDK's own option, documented as "Base URL of the collector,
without a trailing path", and it defaults to `https://observe-api.nestjs.com`. The
environment variable `OBSERVE_ENDPOINT` overrides it without a code change. **This
is what makes the bridge possible without forking the SDK** — the agent is designed
to be pointed at a self-hosted collector.

Telemetry is serialized and sent from a detached worker thread, so none of this runs
on the request path.

### Request headers

Exactly as captured:

```http
POST /applications/telemetry HTTP/1.1
content-type: application/json
content-encoding: gzip
x-api-key: <appKey>
x-api-secret: <appSecret>
accept-encoding: gzip, deflate
user-agent: node
```

There is no protocol version header. The SDK version is the only thing that
determines the wire format, and it is not transmitted.

### Responses, and what the agent does with each

The agent's behaviour per status is worth knowing, because it decides what a
collector should answer:

| Status | What the agent does |
|---|---|
| `2xx` | Reads the JSON body and looks for `{"degraded": true}`. A body that is absent or unparseable is treated as "not degraded". |
| `400` | Strips fields its contract refuses, drops unsalvageable entries, and re-sends **once**. If that is refused too, the batch is dropped. |
| `401` / `403` | Logs once, then counts silently. **Does not recover without a process restart** — credentials are read at start-up. |
| `429` | Pauses sending for `Retry-After` seconds (delta-seconds form only), or 5 minutes, capped at 1 hour. Batches in the meantime are dropped. |
| anything else | Logs and drops the batch. |
| unreachable | Logs and drops the batch. There is no retry queue. |

`degraded` is how the hosted collector says it is accepting batches while discarding
the span trees inside them — what happens to an account past its plan's allowance.
An agent that sees it withholds spans for 5 minutes, except for executions it judges
notable (a 5xx, or slower than 1s).

A batch is **dropped, never retried**, on any failure. The agent's buffer is
fixed-size and holding a failed batch would block every later one behind it.

---

## The batch

```jsonc
{
  "serviceId": "example-api",      // required
  "serviceVersion": "1.4.0",
  "forwardLogs": true,
  "snapshots": [ /* RequestSnapshot */ ],
  "jobs":      [ /* JobSnapshot */ ],
  "runtime":   { /* RuntimeMetrics */ },
  "custom":    [ /* CustomMetric */ ],
  "logs":      [ /* LogEntry */ ],
  "objectives":[ /* ObjectiveDeclaration */ ]
}
```

Every section is optional and omitted when empty. Keys are shortened to single
letters everywhere except `logs` and `objectives`, which use full names.

The hosted collector validates with `forbidNonWhitelisted`, so an **undeclared key
fails the whole batch** with a 400. A bridge need not be that strict, and this one
is not: unknown keys are ignored so that a newer SDK keeps working.

### `RequestSnapshot` — an HTTP, GraphQL, RPC or WebSocket execution

| Key | Field | Type | Notes |
|---|---|---|---|
| `ti` | `traceId` | string | **Required.** See [Identifiers](#identifiers). |
| `ct` | `calledAt` | string | ISO 8601. The only absolute timestamp in the batch. |
| `d` | `duration` | number | Milliseconds, fractional. |
| `p` | `protocol` | string | `http` \| `graphql` \| `rpc` \| `grpc` \| `ws` |
| `op` | `operationId` | string | Route template for HTTP (`/users/:id`), `Type.field` for GraphQL, `gateway:pattern` for WebSockets. |
| `u` | `userId` | string | |
| `tg` | `tags` | object | Application tags. |
| `e` | `error` | object | `{ cls, message, stack, tags }` |
| `rq` | `request` | object | Captured headers/body, already redacted by the SDK. |
| `a` | `attributes` | object | `m` = method, `sc` = status code, `ou` = original URL. |
| `t` | `traces` | array | The span forest. |

`a.ou` carries the **sanitized GraphQL document** rather than a URL for GraphQL
operations, because every operation shares one mount path.

`st` (`startTimestamp`) is mapped by the encoder but **deleted before sending** —
the collector rejects it. So `ct` is the only time anchor, and span timestamps must
be reconstructed from it.

### `TraceNode` — one provider method call

| Key | Field | Type | Notes |
|---|---|---|---|
| `s` | `spanId` | string | UUIDv7, one per invocation. Documented as optional. |
| `n` | `name` | string | Manual spans, and collapsed nodes. |
| `o` | `origin` | string | `manual` \| `auto`. **The only classification on the wire.** |
| `c` | `className` | string | Or a driver name for an outgoing call. |
| `m` | `methodKey` | string | Or the operation, for an outgoing call. |
| `so` | `startOffset` | number | Milliseconds from the start of the operation. |
| `d` | `duration` | number | Milliseconds. |
| `e` | `error` | object \| bool | `true` means "failed, details not captured". |
| `t` | `tags` | object | |
| `ch` | `children` | array | Nested `TraceNode`s, arbitrarily deep. |

There is **no field saying whether a span is a guard, a controller or a service.**
`origin` distinguishes only hand-written spans from instrumented ones. The hosted
dashboard renders class and method directly, so it never needs the distinction to be
machine-readable. How this bridge recovers it is in [mapping.md](./mapping.md).

### `JobSnapshot` — a queue or scheduled execution

The letters mean different things here than in a `TraceNode`. **`c` is `calledAt`,
not `className`; `s` is `status`, not `spanId`.**

| Key | Field | Notes |
|---|---|---|
| `i` | `id` | **Required.** |
| `ti` | `traceId` | Optional — carries the enqueueing request's trace id. |
| `n` | `name` | |
| `q` | `queueName` | **Or the scheduler kind** — see below. |
| `s` | `status` | |
| `c` | `calledAt` | |
| `d` | `duration` | |
| `ea` | `enqueuedAt` | |
| `wd` | `waitDuration` | |
| `am` / `ma` | `attemptsMade` / `maxAttempts` | |
| `tg` / `e` / `t` | `tags` / `error` / `traces` | |

`@nestjs/schedule` jobs arrive in this same section with **the scheduler kind in
`q`**: `cron`, `interval`, `timeout`, or `schedule` when the kind is unknown. That
is the only thing distinguishing a timer firing from a queue job.

A job enqueued while handling a request carries that request's trace id, which is
what makes the request and its job one trace. Repeatable (cron) jobs start their own.

### `LogEntry`

Full names, not letters.

| Field | Type | Notes |
|---|---|---|
| `timestamp` | number | **Required.** Milliseconds since the epoch. |
| `text` | string | **Required.** |
| `traceId` | string | |
| `spanId` | string | The id of the span that wrote the line. |
| `level` | string | `verbose` \| `debug` \| `log` \| `warn` \| `error` \| `fatal` |
| `context` | string | Nest's logger context, usually the class name. |
| `attributes` | object | |

Nest's default level is `log`, not `info`. A multi-line stack trace arrives as
**several entries**, one per line, sharing a trace id.

### `RuntimeMetrics`

Sampled on an interval (`runtimeMetricsInterval`, default 60s, floor 30s), and sent
without a timestamp.

```jsonc
{
  "c": { "u": 0, "s": 0, "p": 0 },                    // cpu: user/system in ms, percent
  "m": { "r":0,"ht":0,"hu":0,"e":0,"ab":0,"p":0 },    // memory: MEGABYTES, and percent
  "g": { "c": 0, "td": 0, "b": { "m":…,"j":…,"i":… }},// gc: count, total duration, by kind
  "e": { "l": 0, "u": 0 }                             // event loop: lag ms, utilization 0..1
}
```

Two traps:

- **Memory is in megabytes**, not bytes — the SDK divides by 1024 twice before
  sending. Reporting these as bytes overstates memory by a factor of 10⁶.
- **GC figures are deltas.** `gcCount` and `gcTotalDuration` are reset after every
  collection window, so they describe that window, not a running total.

In `g.b`, `m` is minor, **`j` is major** (because `m` was taken), `i` is incremental.

### `CustomMetric`

| Key | Field | | Key | Field |
|---|---|---|---|---|
| `n` | `name` (**required**) | | `k` | `kind` |
| `t` | `type` — `counter`/`gauge`/`summary` | | `iv` | `increase` |
| `v` | `value` | | `q50`/`q95`/`q99` | quantiles |
| `tg` | `tags` | | `ct` | `observations` |
| `d` | `description` | | `sm` | `total` |
| `l` | `labels` (declared *names*) | | `mx` | `maximum` |
| `lu` | `lastUpdated` | | | |

**Every value field is a map of label-set to number**, never a bare number. An
unlabelled metric arrives as `{"default": 42}`. A labelled one is keyed by
`JSON.stringify` of the label object with its keys sorted:

```jsonc
{ "n": "logins", "t": "counter",
  "v":  { "{\"route\":\"/login\"}": 12 },
  "iv": { "{\"route\":\"/login\"}": 3 } }
```

`v` is the cumulative total held in the application's memory — it resets to zero
when the process restarts. `iv` is the rise since the last **successful** flush, and
is the additive quantity intended for aggregation.

### `ObjectiveDeclaration`

`@Objective()` SLO declarations: `handler`, `operationId`, `method`, `objectives[]`.
Full names. They describe a promise about a route rather than an observation, and
have no OpenTelemetry counterpart.

---

## Identifiers

**Trace ids (`ti`)** are a UUIDv7 the agent mints, *or* an inbound `x-request-id`
adopted verbatim when it matches `^[A-Za-z0-9._:-]{1,128}$`. So a trace id is **not
reliably a UUID** — it can be any short token a caller supplied.

**Span ids (`s`)** are a UUIDv7 per invocation. Log records carry the same value in
their `spanId`, which is what ties a log line to the exact call that wrote it.

Neither is the size OTLP requires (16 bytes for a trace, 8 for a span).
[mapping.md](./mapping.md#identifiers) covers the conversion.

The agent forwards the current trace id as **`x-request-id`** on outbound HTTP calls,
so a downstream service running the agent continues the same trace.

---

## Span collapsing

When more than `spanCollapse.threshold` (default 20) sibling spans under one parent
share a class and method, the agent replaces all but the slowest `keepSlowest`
(default 3) — and any that errored — with a single node:

- `t["observe.collapsed"]` is how many calls it stands for.
- `n` reads `ValidationPipe.transform ×27`.
- **`d` is the sum of the replaced durations**, not a wall-clock span. This keeps the
  parent's self-time correct, but it means the node's end time is not meaningful.
- `so` is the earliest replaced call's offset.
- The replaced calls' children are carried onto the node; their tags are dropped.

## Skipped spans

`skipSpans` reports a request without its span tree — used for expected outcomes
like 404s. The snapshot still arrives with its route, status, duration and error;
`t` is simply absent. A snapshot with no spans is normal, not an error.

---

## Where the contract and reality disagree

The SDK's `telemetry-wire-contract.js` is hand-written and, in 0.3.5, wrong in three
places. This matters: a decoder written from the contract alone will reject real
traffic.

| Field | Contract says | Actually sends |
|---|---|---|
| `runtime.g.b.{m,j,i}` | `number` | `{ count, duration }` |
| `custom[].v` | `number` | `{ [labelSet]: number }` |
| `custom[].iv` | `number` | `{ [labelSet]: number }` |

This bridge accepts both shapes for all three, so it keeps working whichever side is
eventually corrected. The contract is only consulted by the SDK on a 400, which is
why the mismatch does not break the hosted path.

---

## Reproducing these captures

```bash
# 1. A server that decompresses and prints what arrives.
node -e '
const http=require("http"),zlib=require("zlib");
http.createServer((req,res)=>{const c=[];req.on("data",d=>c.push(d));req.on("end",()=>{
  console.log(JSON.stringify(JSON.parse(zlib.gunzipSync(Buffer.concat(c))),null,2));
  res.writeHead(200,{"content-type":"application/json"});res.end("{}");});
}).listen(4399);'

# 2. Point the example app at it.
cd examples/nestjs-app
OBSERVE_ENDPOINT=http://localhost:4399 npm start
curl localhost:3000/users/1
```

Runtime metrics take a full `runtimeMetricsInterval` (60s by default) to appear.
