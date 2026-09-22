#!/usr/bin/env bash
set -euo pipefail

if [[ $# -eq 1 && $1 == --help ]]; then
    echo "Usage: $0 [--help]"
    echo 'Install or update Tiny Player at ~/.local/tiny-player.app.'
    exit 0
fi
if [[ $# -ne 0 ]]; then
    echo "Usage: $0 [--help]" >&2
    echo 'The installation directory is fixed at ~/.local/tiny-player.app.' >&2
    exit 2
fi
bundle_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
[[ -x $bundle_dir/bin/tiny-player && -f $bundle_dir/share/applications/tiny-player.desktop \
    && -d $bundle_dir/share/icons ]] || {
    echo 'Incomplete bundle. Extract the complete archive before installing.' >&2
    exit 1
}
prefix="$HOME/.local"
mkdir -p -- "$prefix"
prefix=$(cd -- "$prefix" && pwd -P)
destination="$prefix/tiny-player.app"
[[ $bundle_dir != "$destination" ]] || { echo 'Already installed at this location.'; exit 0; }
[[ $prefix != "$bundle_dir" && $prefix != "$bundle_dir/"* ]] || {
    echo 'The installation directory must be outside the source bundle.' >&2
    exit 1
}
# Keep concurrent installers from moving each other's application or backups.
exec {install_lock_fd}>"$prefix/.tiny-player.install.lock"
flock -n "$install_lock_fd" || { echo 'Another Tiny Player installation is in progress.' >&2; exit 1; }
install_action=Installed
if [[ -e $destination || -L $destination ]]; then
    [[ ! -L $destination && -d $destination && -x $destination/bin/tiny-player \
        && -f $destination/share/applications/tiny-player.desktop && -f $destination/install.sh ]] || {
        echo "Refusing to replace an unrecognized installation: $destination" >&2
        exit 1
    }
    install_action=Updated
fi
[[ ! -e $prefix/bin/tiny-player || -L $prefix/bin/tiny-player ]] || {
    echo "Refusing to replace an existing executable: $prefix/bin/tiny-player" >&2
    exit 1
}
staged=$(mktemp -d "$prefix/.tiny-player.installing.XXXXXXXX")
committed=0
replaced_paths=()
had_original=()
cleanup() {
    local install_status=$? recovery_failed=0 index target backup
    trap - EXIT HUP INT TERM
    if (( ! committed )); then
        for ((index=${#replaced_paths[@]}-1; index>=0; index--)); do
            target=${replaced_paths[index]}
            backup="$staged/previous/$index"
            if [[ -e $backup || -L $backup ]]; then
                if ! { rm -rf -- "$target" && mv -T -- "$backup" "$target"; }; then
                    recovery_failed=1
                fi
            elif [[ ${had_original[index]:-1} == 0 ]]; then
                rm -rf -- "$target" || recovery_failed=1
            fi
        done
    fi
    if (( recovery_failed )); then
        printf 'Could not fully restore the installation. Recovery files remain in %s\n' "$staged" >&2
        install_status=1
    elif ! rm -rf -- "$staged"; then
        printf 'Could not remove temporary installation files: %s\n' "$staged" >&2
    fi
    exit "$install_status"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

replace_path() {
    local source=$1 target=$2 index=${#replaced_paths[@]}
    [[ ! -d $target || -L $target || -d $source ]] || {
        printf 'Refusing to replace a directory with a file: %s\n' "$target" >&2
        return 1
    }
    mkdir -p -- "$(dirname -- "$target")"
    replaced_paths+=("$target")
    if [[ -e $target || -L $target ]]; then
        had_original+=(1)
        mv -T -- "$target" "$staged/previous/$index"
    else
        had_original+=(0)
    fi
    mv -T -- "$source" "$target"
}

# Finish copying before touching the installed version. Replacing the entire
# application directory also removes libraries no longer shipped by this version.
cp -a -- "$bundle_dir/." "$staged/new-app"
cp -a -- "$staged/new-app/share/icons" "$staged/icons"
mkdir -- "$staged/previous"
ln -s -- "$destination/bin/tiny-player" "$staged/launcher"
find "$staged/icons" \( -type f -o -type l \) -print0 > "$staged/icon-files"
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
done < "$staged/new-app/share/applications/tiny-player.desktop" > "$staged/desktop-entry"

replace_path "$staged/new-app" "$destination"
replace_path "$staged/launcher" "$prefix/bin/tiny-player"
while IFS= read -r -d '' icon; do
    replace_path "$icon" "$prefix/share/icons/${icon#"$staged/icons/"}"
done < "$staged/icon-files"
replace_path "$staged/desktop-entry" "$prefix/share/applications/tiny-player.desktop"
committed=1
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$prefix/share/applications" || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -f -t "$prefix/share/icons/hicolor" || true
fi
printf '%s Tiny Player to %s\nLaunch: %s/bin/tiny-player\n' "$install_action" "$destination" "$prefix"
