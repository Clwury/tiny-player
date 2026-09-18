#!/usr/bin/env bash
set -euo pipefail

if [[ $# -gt 1 || ${1:-} == --help ]]; then
    echo "Usage: $0 [PREFIX]  (default: ~/.local)"
    exit 0
fi
bundle_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
prefix=${1:-"$HOME/.local"}
mkdir -p -- "$prefix"
prefix=$(cd -- "$prefix" && pwd)
destination="$prefix/tiny-player.app"
[[ $bundle_dir != "$destination" ]] || { echo 'Already installed at this location.'; exit 0; }
[[ ! -e $destination && ! -L $destination ]] || {
    echo "Destination exists: $destination. Move it aside before installing this version." >&2
    exit 1
}
[[ ! -e $prefix/bin/tiny-player || -L $prefix/bin/tiny-player ]] || {
    echo "Refusing to replace an existing executable: $prefix/bin/tiny-player" >&2
    exit 1
}
staged=$(mktemp -d "$prefix/.tiny-player.installing.XXXXXXXX")
trap 'rm -rf -- "$staged"' EXIT
cp -a -- "$bundle_dir/." "$staged/"
mv -- "$staged" "$destination"
mkdir -p -- "$prefix/bin" "$prefix/share/applications" "$prefix/share/icons"
ln -sfn -- "$destination/bin/tiny-player" "$prefix/bin/tiny-player"
cp -a -- "$destination/share/icons/." "$prefix/share/icons/"
# Desktop Entry escaping differs from shell quoting. Keep installation paths
# with spaces, backslashes, dollars, backticks and percent signs literal.
desktop_exec="$prefix/bin/tiny-player"
desktop_exec=${desktop_exec//\\/\\\\\\\\}
desktop_exec=${desktop_exec//\"/\\\\\"}
desktop_exec=${desktop_exec//\$/\\\\\$}
desktop_exec=${desktop_exec//\`/\\\\\`}
desktop_exec=${desktop_exec//%/%%}
while IFS= read -r line; do
    if [[ $line == Exec=* ]]; then
        printf 'Exec="%s"\n' "$desktop_exec"
    else
        printf '%s\n' "$line"
    fi
done < "$destination/share/applications/tiny-player.desktop" \
    > "$prefix/share/applications/tiny-player.desktop"
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$prefix/share/applications" || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -f -t "$prefix/share/icons/hicolor" || true
fi
printf 'Installed Tiny Player to %s\nLaunch: %s/bin/tiny-player\n' "$destination" "$prefix"
