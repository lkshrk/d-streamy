FROM debian:trixie-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential pkg-config clang libclang-dev curl ca-certificates git \
      libpipewire-0.3-dev libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
      gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly \
      libopus-dev libssl-dev \
    && rm -rf /var/lib/apt/lists/*

ARG RUST_TOOLCHAIN=1.99.0
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain "$RUST_TOOLCHAIN" -c rustfmt,clippy \
    && chmod -R a+w $RUSTUP_HOME $CARGO_HOME
