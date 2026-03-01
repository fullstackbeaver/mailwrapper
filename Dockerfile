# ── Build stage ──────────────────────────────────────────────────────────────
FROM rust:1.85-slim AS builder

WORKDIR /app

# Cache dependencies
COPY Cargo.toml Cargo.lock* ./
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release 2>/dev/null || true
RUN rm src/main.rs

# Build real binary
COPY src ./src
RUN cargo build --release

# ── Runtime stage ─────────────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/mailbridge /usr/local/bin/mailbridge

EXPOSE 8025

CMD ["mailbridge"]
