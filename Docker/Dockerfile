# ---- build stage ----------------------------------------------------------
# Deliberately newer than the crate's MSRV (Cargo.toml `rust-version`): the
# published binary gets current codegen and security fixes, while CI's `msrv`
# job proves the source still compiles on the declared floor. The two move
# independently — bumping this line is not an MSRV bump.
FROM rust:1.95-slim AS build
WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends pkg-config && \
    rm -rf /var/lib/apt/lists/*

# Cache the dependency graph.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && \
    echo "fn main() {}" > src/main.rs && \
    echo "" > src/lib.rs && \
    cargo build --release 2>/dev/null || true

COPY src ./src
COPY migrations ./migrations
RUN touch src/main.rs src/lib.rs && cargo build --release

# ---- runtime stage ---------------------------------------------------------
# Must track the Debian release under rust:*-slim above, or the binary links
# against a newer glibc than the runtime ships.
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl && \
    rm -rf /var/lib/apt/lists/* && \
    useradd --system --home /app bus
WORKDIR /app
COPY --from=build /app/target/release/ai-crew-sync /usr/local/bin/ai-crew-sync
USER bus

EXPOSE 8787
HEALTHCHECK --interval=30s --timeout=3s \
    CMD curl -fsS http://localhost:8787/health || exit 1

ENTRYPOINT ["ai-crew-sync"]
CMD ["serve"]
