# Observe → OpenTelemetry mapping

What the bridge turns each piece of `@nestjs/observe` telemetry into, and why.

Two rules decide everything below:

1. **If OpenTelemetry has a convention for it, use the convention's name.** Even
   when the SDK sends the same measurement under an older name.
2. **If it is a Nest concept OpenTelemetry has no word for, put it under
   `nestjs.*`.** Nothing else gets a namespace, and application tags are passed
   through untouched under whatever keys the application chose.

The names live in one file — [`src/mapping/attributes.rs`](../src/mapping/attributes.rs)
— so this document and the code cannot drift far apart.

---

## Identifiers

Neither Observe identifier is the size OTLP requires, so both are derived.
[`src/mapping/ids.rs`](../src/mapping/ids.rs) is the whole of it.

### Trace ids

A trace id is a UUIDv7 the agent minted, **or** an inbound `x-request-id` adopted
verbatim — so it may be any token matching `^[A-Za-z0-9._:-]{1,128}$`.

| Input | Becomes |
|---|---|
| A UUID (dashed or not, any case) | Its own 16 bytes |
| Anything else | `SHA-256(domain ‖ id)[0..16]` |

A UUID keeps its own bytes so the trace is recognisable, and identical to what the
hosted collector would have stored. Everything else is hashed, which is a pure
function of the id — so **two services that saw the same `x-request-id` land on the
same OTLP trace id and their spans join up**, which is the point of the SDK
propagating it in the first place.

An all-zero result is nudged off zero, since OTLP reserves that for "no trace".

### Span ids

Observe sends a UUIDv7 per invocation in `s`. It is hashed to
`SHA-256(domain ‖ id)[0..8]`, under a different domain separator from trace ids so
the two cannot be related.

Hashed rather than truncated for one reason that matters: **log records carry the
same `spanId`**, and they go through the same function, so a log line lands on
exactly the span that wrote it.

`s` is documented as optional. A span without one is given an id derived from its
**position** — the path of child indices from the root — which is unique within a
trace by construction. That fallback is strictly worse (it moves if the tree's shape
changes, and nothing can reference it), so it is only ever a fallback. If two spans
ever derived the same id, the second falls back to its position rather than
corrupting the tree.

Both derivations are deterministic, so **re-exporting a batch reproduces the same
tree** instead of a duplicate one with fresh ids.

---

## Traces

### The synthesized root

Observe does not send a root span. A snapshot *is* the root — it carries the route,
status and duration as fields of its own, and `t` holds only the calls made beneath
it. So the bridge creates one per operation.

| | Request | Job |
|---|---|---|
| Span kind | `SERVER` | `CONSUMER` |
| Name | `{method} {route}` for HTTP, else the operation id | `{name} {queue}`, or the handler name for a scheduled run |
| Parent | none | none |

### Timing

The only absolute timestamp in a batch is the snapshot's `calledAt`; the SDK deletes
`startTimestamp` before sending. Everything else is reconstructed:

```
span.start = calledAt + span.startOffset
span.end   = span.start + span.duration
```

An operation with no `calledAt` is dated at **receipt**. That is off by at most one
flush interval, and the batch carries no better clock. Dropping it would lose a real
trace over a missing field.

A negative or non-finite duration is clamped to zero, so a malformed value cannot
wrap into a span that appears to last centuries.

### Span kinds

| Span | Kind |
|---|---|
| Request root | `SERVER` |
| Job root | `CONSUMER` |
| Outgoing database query or HTTP call | `CLIENT` |
| Everything else | `INTERNAL` |

### Status

This is the rule most worth understanding, because the obvious implementation is
wrong for NestJS.

**For HTTP, the response code decides and nothing else.** The HTTP conventions make
a server span an error only on 5xx. That matters more in a Nest application than
most, because **Nest answers "not found", "forbidden" and "invalid input" by
throwing** — so a captured `NotFoundException` sits on a perfectly ordinary 404.
Letting the exception decide would mark all of that traffic failed and make the
service's error rate meaningless.

**For everything else the error decides**, because there is no status code worth
trusting: a GraphQL operation answers 200 with an `errors` array, and jobs and RPC
messages have no status of their own.

The exception is recorded as an event either way. Only the *status* is withheld.

| Case | Status |
|---|---|
| HTTP, `sc >= 500` | `ERROR` |
| HTTP, anything else | `UNSET` — even with a captured exception |
| GraphQL / RPC / WebSocket / job, error present | `ERROR` |
| Span with `e: true` (failed, uncaptured) | `ERROR`, no message |
| Span with `e: false` or absent | `UNSET` |

### Errors

A captured error becomes an `exception` **event** on its span, with
`exception.type`, `exception.message` and `exception.stacktrace`, plus any tags the
error carried. The event is timestamped at the span's end — the closest the wire
allows, since the SDK sends no timestamp with an error.

`e: true` is the SDK's "something failed here but was not captured", which every
failed non-root span uses. It still sets an error status, because otherwise a
failing call reads as a successful one; there is simply no event to record.

---

## Attributes

### `nestjs.*`

| Attribute | On | Meaning |
|---|---|---|
| `nestjs.type` | spans | Which Nest concept ran. **Inferred** — see below. |
| `nestjs.class.name` | spans | The class. Exact. |
| `nestjs.method.name` | spans | The method. Exact. |
| `nestjs.span.origin` | spans | `manual` or `auto`. |
| `nestjs.span.id` | spans, logs | The Observe UUID, for cross-referencing. |
| `nestjs.collapsed.calls` | spans | How many calls a collapsed node stands for. |
| `nestjs.protocol` | roots | `http`, `graphql`, `rpc`, `grpc`, `ws`, `job`. |
| `nestjs.operation.id` | roots | The SDK's own operation identifier. |
| `nestjs.user.id` | roots | |
| `nestjs.job.id` / `.queue` / `.status` | job roots | |
| `nestjs.job.attempts_made` / `.max_attempts` | job roots | |
| `nestjs.job.enqueued_at` / `.wait_duration_ms` | job roots | |
| `nestjs.schedule.kind` | scheduled roots | `cron`, `interval`, `timeout`, `schedule`. |
| `nestjs.log.context` | logs | Nest's logger context. |
| `nestjs.log.level` | logs | The level as Nest wrote it. |
| `nestjs.graphql.document` | GraphQL roots | The sanitized document. |
| `nestjs.request.captured` | roots | Captured headers/body, already redacted by the SDK. |
| `nestjs.metric.type` | metrics | Only for a metric type this bridge does not recognise. |
| `nestjs.gc.kind` | metrics | `minor`, `major`, `incremental`. |

### Values of `nestjs.type`

`controller`, `service`, `repository`, `guard`, `interceptor`, `pipe`,
`exception_filter`, `middleware`, `graphql_resolver`, `websocket_gateway`,
`queue_consumer`, `scheduled_task`, `database_client`, `http_client`, `manual`,
`provider`.

### How `nestjs.type` is inferred, and when it is wrong

**The wire does not carry it.** There is no field anywhere in `@nestjs/observe@0.3.5`
marking a span as a guard rather than a service — `origin` distinguishes only
`manual` from `auto`. The hosted dashboard renders class and method directly, so it
never needs the distinction to be machine-readable. A bridge does, because
`nestjs.type` is what makes a trace searchable by what the framework was doing.

So it is inferred, in this order
([`src/observe/component.rs`](../src/observe/component.rs)):

1. **A manual span** is `manual`, whatever its class is called. The application
   chose what to wrap, so the surrounding class says nothing.
2. **A driver name plus its tags.** Outgoing spans are opened against `pg`,
   `mysql2`, `mongodb` or `http` and carry `db.system` or `http.method`. Both are
   required — a bare name match would relabel an application's own class called
   `HttpService` as a client span.
3. **The class name's suffix**: `…Controller`, `…Service`, `…Repository`, `…Guard`,
   `…Interceptor`, `…Pipe`, `…Filter`/`…ExceptionFilter`, `…Middleware`,
   `…Resolver`, `…Gateway`, `…Processor`/`…Consumer`/`…Subscriber`. Longest suffix
   wins.
4. **An interface hook**: `canActivate`, `intercept`, `catch`. These are declared by
   `CanActivate`, `NestInterceptor` and `ExceptionFilter`, so an unconventionally
   named class is still recognisable.
5. Otherwise `provider`.

`transform` and `use` are deliberately **not** in step 4. They are the
`PipeTransform` and `NestMiddleware` hooks, but they are also ordinary method names
on ordinary services, and filing a service under `pipe` is worse than leaving it as
`provider`.

**This is a heuristic and it can be wrong.** A guard called `Permissions` with a
method other than `canActivate` will read as `provider`. `nestjs.class.name` and
`nestjs.method.name` are always exact — prefer them when precision matters.

### Semantic conventions

The SDK emits its outgoing-call tags under **older** convention names. Those are
translated forward rather than passed through, because a backend's built-in database
and HTTP views key off the current ones. The old keys are replaced, not duplicated.

| SDK sends | Exported as |
|---|---|
| `db.system` | `db.system.name` |
| `db.statement` | `db.query.text` |
| `http.method` | `http.request.method` |
| `http.url` | `url.full` |

Derived from structured fields rather than tags:

| Signal | Attributes |
|---|---|
| HTTP root | `http.request.method`, `http.route`, `http.response.status_code`, `url.path` |
| GraphQL root | `graphql.operation.name`, `graphql.document`, `http.response.status_code` |
| RPC / gRPC root | `rpc.system`, `rpc.method` |
| Queue job root | `messaging.system`, `messaging.operation.name`, `messaging.destination.name`, `messaging.message.id` |
| Database span | `db.operation.name` (from the SDK's method key) |
| HTTP client span | `http.request.method`, `server.address` (parsed from `GET api.stripe.com`) |

`http.route` is the Nest route template **verbatim** — `/users/:id`, not
`/users/{id}`. Rewriting it would be inventing data; a backend groups by the string
either way.

**A scheduled run is not messaging.** `@nestjs/schedule` jobs arrive in the same
section as queue jobs with the scheduler kind in the queue-name field, and they get
`nestjs.schedule.kind` instead of `messaging.*` — otherwise a backend's queue views
would count cron firings as queue traffic.

Application tags are applied **before** derived conventions, so a user tag that
happens to be called `http.route` cannot displace the route the framework matched.

### Resource

| Attribute | Value |
|---|---|
| `service.name` | `serviceId` |
| `service.version` | `serviceVersion`, when sent |
| `telemetry.sdk.name` | `nestjs-observe` |
| `telemetry.sdk.language` | `nodejs` |

The SDK name is the *instrumentation*, not this bridge — a backend grouping by SDK
should see what actually produced the telemetry. The bridge identifies itself as the
instrumentation **scope** instead.

---

## Logs

Observe log records become OTLP `LogRecord`s directly.

| Observe | OTLP |
|---|---|
| `timestamp` | `time_unix_nano` and `observed_time_unix_nano` |
| `text` | `body` |
| `level` | `severity_number` / `severity_text`, and `nestjs.log.level` |
| `traceId` | `trace_id`, by the same derivation traces use |
| `spanId` | `span_id`, by the same derivation spans use |
| `context` | `nestjs.log.context` |
| `attributes` | passed through |

### Severity

| Nest | OTLP |
|---|---|
| `verbose` | `TRACE` (1) |
| `debug` | `DEBUG` (5) |
| `log`, `info` | `INFO` (9) |
| `warn` | `WARN` (13) |
| `error` | `ERROR` (17) |
| `fatal` | `FATAL` (21) |
| absent | unspecified (0) |

Nest's default level is `log`, and `verbose` has no OTLP equivalent — TRACE is the
closest. The original string is always kept in `nestjs.log.level`.

A log naming a span from a batch that has not arrived yet still exports with the
link. Logs and traces flush independently, so a briefly dangling reference is normal;
dropping the id to avoid it would break the common case to tidy the rare one.

---

## Metrics

### Temporality

**Counters are exported as delta sums.** Both kinds of counter the SDK sends are
already deltas — the runtime GC figures are zeroed after every collection window,
and a custom counter's `increase` is the rise since the last successful flush.
Declaring them cumulative would make a backend read each window as a running total
that keeps falling back towards zero.

### Runtime

| Metric | Unit | Type |
|---|---|---|
| `nestjs.runtime.cpu.user` / `.system` | `ms` | gauge |
| `nestjs.runtime.cpu.utilization` | `%` | gauge |
| `nestjs.runtime.memory.rss` | `MBy` | gauge |
| `nestjs.runtime.memory.heap.total` / `.heap.used` | `MBy` | gauge |
| `nestjs.runtime.memory.external` / `.array_buffers` | `MBy` | gauge |
| `nestjs.runtime.memory.utilization` | `%` | gauge |
| `nestjs.runtime.event_loop.delay` | `ms` | gauge |
| `nestjs.runtime.event_loop.utilization` | `1` | gauge |
| `nestjs.runtime.gc.collections` | `{collection}` | delta sum |
| `nestjs.runtime.gc.duration` | `ms` | delta sum |
| `nestjs.runtime.gc.collections.by_kind` | `{collection}` | delta sum, `nestjs.gc.kind` |
| `nestjs.runtime.gc.duration.by_kind` | `ms` | delta sum, `nestjs.gc.kind` |

**Memory is `MBy`, not `By`** — the SDK divides by 1024 twice before sending.
Labelling these as bytes would overstate memory by a factor of 10⁶.

These arrive without a timestamp and are dated at receipt.

### Custom

Named `nestjs.custom.{name}`. Every value field the SDK sends is a **map of label
set to number**, so one metric expands into one stream per label set. The key is
`JSON.stringify` of the sorted label object and is parsed back into real attributes,
so `{"route":"/login"}` arrives as a filterable `route` attribute rather than an
opaque string. The literal key `"default"` means the metric was never labelled and
contributes no attributes.

| Type | Exported as |
|---|---|
| `counter` | delta sum of `iv` (`increase`), falling back to `v` if absent |
| `gauge` | gauge of `v` |
| `summary` | one gauge per quantile with a `quantile` attribute, plus `.count`, `.sum` and `.max` |

A counter uses `increase` rather than `value` because `value` is a cumulative total
held in the application's memory that resets on restart, while `increase` is
additive across instances and restarts.

OTLP does have a summary point type, but it is legacy and thinly supported; a
`quantile` attribute is what a backend can actually chart.

---

## What is deliberately not mapped

| | Why |
|---|---|
| `objectives` | SLO *declarations*, not observations. OpenTelemetry has no counterpart, and inventing one would misrepresent them. |
| `forwardLogs` | An agent-side switch, not telemetry. |
| CPU profiles | The SDK's profiling upload is a separate endpoint and is shelved upstream. |

## Known distortions

Two places where the output is faithful to Observe but slightly odd in OTLP terms,
both documented rather than silently corrected:

- **A collapsed node's duration is a sum, not an interval.** The SDK replaces a run
  of identical sibling calls with one node whose `d` is their total, which keeps the
  parent's self-time correct but makes the node's end time meaningless. It is
  exported as sent, with `nestjs.collapsed.calls` saying how many calls it covers.
- **A job and the request that enqueued it share a trace but are both roots.**
  Observe says which trace a job belongs to but not which span enqueued it, so
  linking them to a parent would be a guess. They appear as two roots in one trace.
