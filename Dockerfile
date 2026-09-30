# Built against the Rust version this was developed on; bump deliberately rather
# than tracking `latest`, so a toolchain change is a commit rather than a surprise.
FROM rust:1.97-slim-bookworm AS build

WORKDIR /build

# Dependencies first, in their own layer: they change far less often than the source
# and take almost all of the build time.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
 && echo 'fn main() {}' > src/main.rs \
 && echo '' > src/lib.rs \
 && cargo build --release \
 && rm -rf src

COPY src ./src
# `cargo build` skips a rebuild when only mtimes moved, and the stub above left
# artifacts behind that would otherwise be mistaken for the real thing.
RUN touch src/main.rs src/lib.rs && cargo build --release

# Distroless rather than scratch: the binary needs a libc, CA certificates for
# HTTPS OTLP endpoints, and a non-root user, and this supplies all three.
FROM gcr.io/distroless/cc-debian12:nonroot

COPY --from=build /build/target/release/nestjs-observe-oss /usr/local/bin/nestjs-observe-oss

EXPOSE 4319
USER nonroot

# No shell in the image, so there is nothing for a HEALTHCHECK to run - point an
# orchestrator's probe at GET /health instead.
ENTRYPOINT ["/usr/local/bin/nestjs-observe-oss"]
