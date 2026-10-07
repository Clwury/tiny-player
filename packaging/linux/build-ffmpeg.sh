#!/usr/bin/env bash
set -euo pipefail

[[ $(uname -m) == x86_64 && $(getconf GNU_LIBC_VERSION) == 'glibc 2.39' ]]
prefix=/opt/tiny-player
jobs=${BUILD_JOBS:-4}
export PKG_CONFIG_PATH="$prefix/lib/pkgconfig:$prefix/share/pkgconfig"
export LD_LIBRARY_PATH="$prefix/lib"
export CFLAGS="-I$prefix/include"
export CXXFLAGS="-I$prefix/include"
mkdir -p /tmp/ffmpeg
cd /tmp/ffmpeg
git init
git remote add origin https://github.com/FFmpeg/FFmpeg.git
git fetch --depth 1 origin "${FFMPEG_REF:-n9.0.1}"
git checkout --detach FETCH_HEAD
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
rm -rf /tmp/ffmpeg
