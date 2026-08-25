FROM rust:1.94.1-bookworm AS builder

WORKDIR /workspace
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY tests/reference-app ./tests/reference-app
RUN cargo build --locked --release \
    --bin tiv-stripe-pi-fixture \
    --bin tiv-reference-app

FROM debian:bookworm-slim AS fixture

COPY --from=builder /workspace/target/release/tiv-stripe-pi-fixture /usr/local/bin/

USER 65534:65534

FROM debian:bookworm-slim AS reference-app

COPY --from=builder /workspace/target/release/tiv-reference-app /usr/local/bin/

USER 65534:65534
