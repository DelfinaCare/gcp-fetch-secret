FROM rust:1.99.1-slim@sha256:4df2ae8cfdfebdfc13a65da7f253b73d68983dc0339f2719bc17a279c913b13d AS builder

ARG TARGETARCH

# musl-tools: static C toolchain (aws-lc-rs, the TLS crypto backend, is C).
# ca-certificates: copied into the final image so TLS to googleapis.com works.
RUN apt-get update && apt-get install -y --no-install-recommends musl-tools ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Set Rust target based on Docker target architecture
RUN if [ "$TARGETARCH" = "arm64" ]; then \
      echo "aarch64-unknown-linux-musl" > /rust-target; \
    else \
      echo "x86_64-unknown-linux-musl" > /rust-target; \
    fi && \
    rustup target add $(cat /rust-target)

WORKDIR /app

# Cache dependency build
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && touch src/lib.rs
RUN cargo build --release --locked --target $(cat /rust-target)
RUN rm -rf src

# Build the actual application
COPY src ./src
RUN touch src/main.rs src/lib.rs && cargo build --release --locked --target $(cat /rust-target) && \
    cp target/$(cat /rust-target)/release/gcp-fetch-secret /gcp-fetch-secret-bin

FROM scratch
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=builder /gcp-fetch-secret-bin /gcp-fetch-secret
ENTRYPOINT ["/gcp-fetch-secret"]
