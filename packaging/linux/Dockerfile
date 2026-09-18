FROM ubuntu:24.04

ARG DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential ca-certificates clang cmake curl desktop-file-utils git \
    glslang-dev glslang-tools libasound2-dev libbz2-dev libdav1d-dev \
    libfontconfig1-dev libfreetype-dev libgnutls28-dev liblcms2-dev liblzma-dev \
    libshaderc-dev libvulkan-dev libwayland-dev libxcb1-dev libxkbcommon-x11-dev \
    meson nasm ninja-build patchelf pkg-config python3 python3-jinja2 \
    spirv-headers xz-utils zlib1g-dev \
    && rm -rf /var/lib/apt/lists/*

ARG RUST_VERSION=1.97.0
ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo
ENV PATH=/opt/cargo/bin:/usr/local/bin:/usr/bin:/bin
RUN curl --fail --location --retry 3 https://sh.rustup.rs -o /tmp/rustup-init.sh \
    && sh /tmp/rustup-init.sh -y --profile minimal --default-toolchain "$RUST_VERSION" \
    && chmod -R a+rX /opt/rustup /opt/cargo

COPY build-native.sh /opt/packaging/build-native.sh
RUN bash /opt/packaging/build-native.sh

ENV PKG_CONFIG_PATH=/opt/tiny-player/lib/pkgconfig:/opt/tiny-player/share/pkgconfig
ENV LD_LIBRARY_PATH=/opt/tiny-player/lib
ENV CARGO_HOME=/build/cargo CARGO_TARGET_DIR=/build/target
WORKDIR /src
