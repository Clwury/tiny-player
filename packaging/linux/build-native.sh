#!/usr/bin/env bash
set -euo pipefail

# Build all bundled native code against Ubuntu 24.04's glibc 2.39.
[[ $(uname -m) == x86_64 && $(getconf GNU_LIBC_VERSION) == 'glibc 2.39' ]]
prefix=/opt/tiny-player
jobs=${BUILD_JOBS:-4}
export PKG_CONFIG_PATH="$prefix/lib/pkgconfig:$prefix/share/pkgconfig"
export LD_LIBRARY_PATH="$prefix/lib"
export CFLAGS="-I$prefix/include"
export CXXFLAGS="-I$prefix/include"
mkdir -p /tmp/native "$prefix/share/licenses" "$prefix/share/build-info"
cd /tmp/native

git clone --depth 1 --branch v1.4.357 https://github.com/KhronosGroup/Vulkan-Headers.git
cmake -S Vulkan-Headers -B vulkan-build -G Ninja -DCMAKE_INSTALL_PREFIX="$prefix"
cmake --install vulkan-build
git -C Vulkan-Headers rev-parse HEAD > "$prefix/share/build-info/vulkan-headers.commit"
cp -a Vulkan-Headers/LICENSE* "$prefix/share/licenses/"

git clone --depth 1 --branch v7.360.1 https://github.com/haasn/libplacebo.git
git -C libplacebo submodule update --init --depth 1 3rdparty/fast_float
meson setup placebo-build libplacebo --prefix="$prefix" --libdir=lib \
    --buildtype=release --default-library=shared --wrap-mode=nofallback \
    -Dvulkan=enabled -Dvulkan-registry="$prefix/share/vulkan/registry/vk.xml" \
    -Dshaderc=enabled -Dglslang=disabled -Dopengl=disabled -Dd3d11=disabled \
    -Dlcms=enabled -Ddovi=enabled -Dlibdovi=disabled -Dunwind=disabled \
    -Dxxhash=disabled -Ddemos=false -Dtests=false
meson compile -C placebo-build -j "$jobs"
meson install -C placebo-build
git -C libplacebo rev-parse HEAD > "$prefix/share/build-info/libplacebo.commit"
mkdir -p "$prefix/share/licenses/libplacebo"
cp libplacebo/LICENSE "$prefix/share/licenses/libplacebo/"
cp libplacebo/3rdparty/fast_float/LICENSE* "$prefix/share/licenses/libplacebo/"

git clone --depth 1 --branch n9.0.1 https://github.com/FFmpeg/FFmpeg.git ffmpeg
cd ffmpeg
./configure --prefix="$prefix" --libdir="$prefix/lib" \
    --arch=x86_64 --cpu=x86-64 --enable-shared --disable-static \
    --disable-debug --disable-doc --disable-programs --disable-avdevice \
    --disable-autodetect --disable-encoders --disable-muxers \
    --enable-pthreads --enable-vulkan --glslc=glslangValidator \
    --enable-libdav1d --enable-gnutls --enable-zlib --enable-bzlib --enable-lzma
make -j "$jobs"
make install
git rev-parse HEAD > "$prefix/share/build-info/ffmpeg.commit"
cp ffbuild/config.mak "$prefix/share/build-info/ffmpeg-config.mak"
mkdir -p "$prefix/share/licenses/ffmpeg"
cp COPYING* LICENSE.md "$prefix/share/licenses/ffmpeg/"
printf '%s\n' "$prefix/lib" > /etc/ld.so.conf.d/tiny-player.conf
/sbin/ldconfig
cd /
rm -rf /tmp/native
