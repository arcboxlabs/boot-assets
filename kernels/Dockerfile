FROM debian:bookworm-slim

RUN apt-get update && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      bash \
      ca-certificates \
      curl \
      xz-utils \
      bsdextrautils \
      python3 \
      build-essential \
      gcc-aarch64-linux-gnu \
      gcc-x86-64-linux-gnu \
      bc \
      bison \
      flex \
      libelf-dev \
      libssl-dev \
      file && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /work
