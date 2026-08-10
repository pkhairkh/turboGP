# Wave 11: Multi-stage Dockerfile for turboGP.
# Build with: docker build -t turbogp:latest .
# Run with:   docker run -p 5432:5432 -v ./data:/data turbogp:latest

# ─── Builder stage ────────────────────────────────────────────────────────────
FROM rust:1-bookworm AS builder

WORKDIR /build

# Copy manifests first for layer caching.
COPY Cargo.toml Cargo.lock* ./

# Create a dummy src/lib.rs so cargo can resolve the package.
RUN mkdir -p src/bin && \
    echo "pub fn _dummy() {}" > src/lib.rs && \
    echo "fn main() {}" > src/bin/turbogp.rs

# Build dependencies only (cached layer).
RUN cargo build --release || true

# Copy the real source.
COPY src/ src/
COPY benches/ benches/
COPY examples/ examples/
COPY tests/ tests/

# Touch the source to force a rebuild of the turboGP crate (not deps).
RUN touch src/lib.rs src/bin/turbogp.rs

# Build the release binary.
RUN cargo build --release --bin turbogp

# ─── Runtime stage ────────────────────────────────────────────────────────────
FROM debian:stable-slim AS runtime

# Install minimal runtime deps (libssl for TLS, ca-certificates for HTTPS).
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        ca-certificates \
        libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Copy the binary.
COPY --from=builder /build/target/release/turbogp /usr/local/bin/turbogp

# Create a data directory.
RUN mkdir -p /data
VOLUME ["/data"]

# Expose pgwire port.
EXPOSE 5432

# Health check: verify the process is running.
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD pgrep turbogp > /dev/null || exit 1

# Run the server with default options.
# Override with: docker run -e TURBOGP_PORT=6543 ...
ENTRYPOINT ["turbogp"]
CMD ["--host", "0.0.0.0", "--port", "5432", "--data-dir", "/data", "--auth"]
