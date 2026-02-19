# Build context: repo root (needs soul/ + amai/)
# docker build -t amai/agent:latest -f amai/crates/amai-agent/Dockerfile .

FROM rust:latest AS builder
WORKDIR /build
COPY . .
RUN cargo build --release --manifest-path amai/crates/amai-agent/Cargo.toml

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates curl && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/amai/target/release/amai /usr/local/bin/amai
COPY configs/docker-agent.toml /etc/amai/agent.toml
COPY configs/docker-minimax.toml /etc/amai/minimax.toml
ENTRYPOINT ["amai"]
