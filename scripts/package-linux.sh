#!/usr/bin/env bash
set -euo pipefail

if [[ ${1:-} == --help ]]; then
    cat <<'EOF'
Usage: scripts/package-linux.sh
Build an x86_64 Linux tar.gz with a glibc 2.39 baseline using Docker.
Outputs: dist/*.tar.gz, dist/*.sha256 and dist/*.manifest.json
Environment: CONTAINER_ENGINE (docker), BUILD_JOBS (4),
             TINY_LINUX_IMAGE (tiny-player-builder:glibc2.39)
EOF
    exit 0
fi
[[ $# == 0 ]] || { echo 'Unexpected arguments; use --help.' >&2; exit 2; }
[[ $(uname -m) == x86_64 ]] || { echo 'Build on an x86_64 Linux host.' >&2; exit 1; }
repo_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
engine=${CONTAINER_ENGINE:-docker}
builder_image=${TINY_LINUX_IMAGE:-tiny-player-builder:glibc2.39}
build_jobs=${BUILD_JOBS:-4}
[[ $build_jobs =~ ^[1-9][0-9]*$ ]] || { echo 'BUILD_JOBS must be positive.' >&2; exit 2; }
command -v "$engine" >/dev/null
mkdir -p "$repo_dir/dist" "$repo_dir/target/linux-x86_64-glibc2.39"
"$engine" build --platform linux/amd64 --tag "$builder_image" "$repo_dir/packaging/linux"
"$engine" run --rm --interactive --platform linux/amd64 --user "$(id -u):$(id -g)" \
    --mount "type=bind,src=$repo_dir,dst=/src,readonly" \
    --mount "type=bind,src=$repo_dir/target/linux-x86_64-glibc2.39,dst=/build" \
    --mount "type=bind,src=$repo_dir/dist,dst=/out" \
    --env "CARGO_BUILD_JOBS=$build_jobs" \
    "$builder_image" bash -s <<'BUILD'
set -euo pipefail
[[ $(uname -m) == x86_64 && $(getconf GNU_LIBC_VERSION) == 'glibc 2.39' ]]
unset TINY_ASSET_DIR RUSTFLAGS CARGO_ENCODED_RUSTFLAGS
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS='-C target-cpu=x86-64'
cargo build --locked --release --target x86_64-unknown-linux-gnu
python3 packaging/linux/bundle.py \
    --binary "$CARGO_TARGET_DIR/x86_64-unknown-linux-gnu/release/tiny-player" \
    --output /out
BUILD
