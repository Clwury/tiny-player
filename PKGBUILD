# Maintainer: Clwury <1931946508@qq.com>

# makepkg normally uses $startdir/src as disposable build storage. In an
# upstream Rust checkout that path contains the project sources, so redirect
# only the default local layout before `makepkg -C` can remove it.
if [[ -n ${startdir:-} && -n ${BUILDDIR:-} && \
      -f $startdir/src/main.rs && $BUILDDIR -ef $startdir ]]; then
    BUILDDIR="$startdir/.makepkg"
    if [[ -n ${SRCDEST:-} && $SRCDEST -ef $startdir ]]; then
        SRCDEST="$BUILDDIR/sources"
    fi
fi

pkgname=tiny-player-git
pkgver=0.1.0.r85.g8df5055
pkgrel=1
pkgdesc='Native Emby desktop client with FFmpeg and Vulkan playback'
arch=('x86_64')
url='https://github.com/Clwury/tiny-player'
license=('MIT')
# ring builds a native GCC archive that rust-lld cannot consume as GCC LTO.
options=('!lto')
depends=(
    'alsa-lib'
    'ffmpeg>=8.1'
    'fontconfig'
    'freetype2'
    'gcc-libs'
    'glibc'
    'hicolor-icon-theme'
    'libplacebo>=7'
    'libxcb'
    'libxkbcommon'
    'vulkan-icd-loader'
    'wayland'
)
makedepends=(
    'cargo'
    'clang'
    'desktop-file-utils'
    'git'
    'pkgconf'
    'vulkan-headers'
)
provides=("tiny-player=${pkgver}")
conflicts=('tiny-player')
source=('tiny-player::git+https://github.com/Clwury/tiny-player.git')
sha256sums=('SKIP')

pkgver() {
    cd tiny-player

    local upstream_version
    upstream_version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)
    printf '%s.r%s.g%s' \
        "$upstream_version" \
        "$(git rev-list --count HEAD)" \
        "$(git rev-parse --short=7 HEAD)"
}

prepare() {
    cd tiny-player

    export RUSTUP_TOOLCHAIN=stable
    cargo fetch --locked --target "$CARCH-unknown-linux-gnu"
}

build() {
    cd tiny-player

    export RUSTUP_TOOLCHAIN=stable
    export CARGO_TARGET_DIR=target
    export TINY_ASSET_DIR=/usr/share/tiny-player/assets
    cargo build --frozen --release
}

check() {
    cd tiny-player

    export RUSTUP_TOOLCHAIN=stable
    export CARGO_TARGET_DIR=target
    export TINY_ASSET_DIR=/usr/share/tiny-player/assets
    cargo test --frozen
    desktop-file-validate tiny-player.desktop
}

package() {
    cd tiny-player

    install -Dm755 target/release/tiny-player "$pkgdir/usr/bin/tiny-player"
    install -Dm644 tiny-player.desktop \
        "$pkgdir/usr/share/applications/tiny-player.desktop"
    install -Dm644 assets/icons/tiny-player.png \
        "$pkgdir/usr/share/icons/hicolor/512x512/apps/tiny-player.png"
    install -Dm644 assets/icons/tiny-player.svg \
        "$pkgdir/usr/share/icons/hicolor/scalable/apps/tiny-player.svg"
    install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"

    install -dm755 "$pkgdir/usr/share/tiny-player/assets/icons"
    install -m644 assets/icons/* "$pkgdir/usr/share/tiny-player/assets/icons/"
}
