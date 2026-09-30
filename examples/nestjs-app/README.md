# Example NestJS app

A small NestJS application running the **real, unmodified `@nestjs/observe` SDK**,
pointed at `nestjs-observe-oss` instead of the hosted collector.

It exists for three reasons: to discover and re-capture the wire protocol, to
demonstrate the project, and to prove that Nest-specific context survives the
translation. The fixtures in [`../../tests/fixtures`](../../tests/fixtures) were
captured from this app.

## What it produces

| Route | Telemetry |
|---|---|
| `GET /users/:id` | A request with a guard, an interceptor, a controller and two nested service calls |
| `GET /users/missing/:id` | A thrown `NotFoundException` — a 404 carrying an exception |
| `GET /users/boom/all` | An unhandled error — a 500 with a stack trace |
| *(every 15s)* | A `@Interval` scheduled task |

Logs are emitted from the service and correlated to the request that wrote them.
Runtime metrics follow once a minute.

## Running it

```bash
npm install

# The bridge should already be listening on 4319.
npm start

curl localhost:3000/users/1
curl localhost:3000/users/missing/9
curl localhost:3000/users/boom/all
```

| Variable | Default |
|---|---|
| `OBSERVE_ENDPOINT` | `http://localhost:4319` |
| `OBSERVE_APP_KEY` / `OBSERVE_APP_SECRET` | `local-key` / `local-secret` |
| `OBSERVE_SERVICE_ID` | `example-api` |
| `PORT` | `3000` |

The credentials are sent on every request and are only checked if the bridge was
started with a matching pair.

## Why it compiles with `tsc` rather than `tsx`

Nest resolves constructor dependencies from the `design:paramtypes` metadata that
`emitDecoratorMetadata` produces. **esbuild does not implement it**, so
esbuild-backed runners (`tsx`, `esbuild-register`, `@swc/register` without
`decoratorMetadata`) silently drop it — the application starts, and then every
injected provider is `undefined` at the first request. `tsconfig.json` also sets
`useDefineForClassFields: false`, without which parameter properties are re-declared
as `undefined` at construction.

This is a NestJS toolchain detail rather than anything to do with Observe, but it
costs an hour to diagnose, so it is written down here.
