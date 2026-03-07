# ── build stage ──────────────────────────────────────────────────────────────
FROM rust:1.94.0-bookworm AS builder

WORKDIR /app

# Cache dependencies separately from source
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main(){}' > src/main.rs \
    && cargo build --release \
    && rm -rf src

# Build the real source
COPY src ./src
# Touch main.rs so cargo rebuilds it after the dummy above
RUN touch src/main.rs && cargo build --release

# ── runtime stage ─────────────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /app/target/release/mumdota /usr/local/bin/mumdota
COPY config.toml ./config.toml

EXPOSE 8080

ENTRYPOINT ["mumdota"]
CMD ["config.toml"]
