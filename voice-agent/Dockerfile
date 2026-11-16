# Two stages: build the Rust binary, then a small runtime image. CPU only.
FROM rust:1-slim-bookworm AS build
WORKDIR /src
# Dependencies first, so they are cached until Cargo.toml or Cargo.lock change.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && cargo build --release && rm -rf src
COPY src ./src
RUN touch src/main.rs && cargo build --release

FROM debian:bookworm-slim
ARG ONNXRUNTIME_VERSION=1.23.2
ARG TARGETARCH
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
 && ARCH=$([ "$TARGETARCH" = "arm64" ] && echo aarch64 || echo x64) \
 && curl -fsSL "https://github.com/microsoft/onnxruntime/releases/download/v${ONNXRUNTIME_VERSION}/onnxruntime-linux-${ARCH}-${ONNXRUNTIME_VERSION}.tgz" \
    | tar -xz -C /tmp \
 && cp /tmp/onnxruntime-linux-${ARCH}-${ONNXRUNTIME_VERSION}/lib/libonnxruntime.so.${ONNXRUNTIME_VERSION} /usr/local/lib/libonnxruntime.so \
 && rm -rf /tmp/onnxruntime-* /var/lib/apt/lists/*

WORKDIR /app
COPY --from=build /src/target/release/voice-agent /usr/local/bin/voice-agent
COPY assets ./assets
COPY config ./config
COPY ui ./ui
RUN mkdir -p logs/sessions data/conversations data/audio && useradd -m app && chown -R app /app
USER app

ENV VOICE_AGENT_ROOT=/app ORT_DYLIB_PATH=/usr/local/lib/libonnxruntime.so
EXPOSE 8080
CMD ["voice-agent"]
