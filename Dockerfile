FROM --platform=$BUILDPLATFORM rust:1.89 AS builder

ARG TARGETPLATFORM
ARG BUILDPLATFORM

WORKDIR /build

RUN apt-get update && apt-get install -y --no-install-recommends \
      musl-tools \
      gcc-aarch64-linux-gnu \
      gcc-x86-64-linux-gnu \
    && rm -rf /var/lib/apt/lists/*

RUN case "$TARGETPLATFORM" in \
      "linux/amd64") rustup target add x86_64-unknown-linux-musl ;; \
      "linux/arm64") rustup target add aarch64-unknown-linux-musl ;; \
      *) echo "unsupported platform: $TARGETPLATFORM" >&2; exit 1 ;; \
    esac

COPY Cargo.toml Cargo.lock ./
COPY src/ src/

ENV CC_aarch64_unknown_linux_musl=aarch64-linux-gnu-gcc \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-gnu-gcc

RUN case "$TARGETPLATFORM" in \
      "linux/amd64") \
        cargo build --release --target x86_64-unknown-linux-musl && \
        cp target/x86_64-unknown-linux-musl/release/mcp-federation /out ;; \
      "linux/arm64") \
        cargo build --release --target aarch64-unknown-linux-musl && \
        cp target/aarch64-unknown-linux-musl/release/mcp-federation /out ;; \
    esac

FROM gcr.io/distroless/cc-debian12:nonroot

COPY --from=builder /out /mcp-federation

EXPOSE 8080

ENTRYPOINT ["/mcp-federation", "--http"]
