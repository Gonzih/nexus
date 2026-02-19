# Stage 1: Build WASM frontend
FROM rust:latest AS wasm-builder
RUN cargo install wasm-pack
RUN rustup target add wasm32-unknown-unknown
WORKDIR /build
COPY . .
RUN wasm-pack build crates/amai-wasm --target web --out-dir ../../web/pkg --no-typescript

# Stage 2: Build native server
FROM rust:latest AS server-builder
WORKDIR /build
COPY . .
COPY --from=wasm-builder /build/web/pkg /build/web/pkg
RUN cargo build --release -p amai-server

# Stage 3: Runtime
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=server-builder /build/target/release/amai-server /app/amai-server
COPY --from=wasm-builder /build/web /app/web
ENV PORT=8090
ENV WEB_DIR=/app/web
EXPOSE 8090
CMD ["/app/amai-server"]
