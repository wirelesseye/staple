#!/bin/sh
# Installs the Staple standard library and prebuilds its objects.
#
# usage: install-stdlib.sh [destination]
#
#   STAPLE=<path>            the compiler used to prebuild objects (default:
#                            `staple` on PATH, else built from this repository)
#   STAPLE_INSTALL_OBJECTS=0 copy the sources only; skip prebuilding objects
#
# Cached objects are named by a fingerprint that includes the compiler
# executable itself, so they are only used by the very `staple` that built
# them. Install with the `staple` you will run, and reinstall after upgrading.
set -eu

script_directory=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo="$script_directory/.."
source_directory="$repo/stdlib"
destination=${1:-"${HOME:?HOME must be set}/.local/lib/staple/stdlib"}

if [ ! -d "$source_directory/std" ]; then
    echo "install-stdlib: could not find the Staple standard library at $source_directory" >&2
    exit 1
fi

# Rebuilding replaces any objects left by an earlier install.
rm -rf "$destination/target"
mkdir -p "$destination"
(cd "$source_directory" && tar -cf - --exclude=target .) | (cd "$destination" && tar -xf -)

echo "Installed the Staple standard library to $destination"

if [ "${STAPLE_INSTALL_OBJECTS:-1}" = 0 ]; then
    echo "Skipped prebuilding objects (STAPLE_INSTALL_OBJECTS=0); the first build will cache them instead."
    exit 0
fi

if [ -n "${STAPLE:-}" ]; then
    set -- "$STAPLE"
elif command -v staple >/dev/null 2>&1; then
    set -- staple
else
    echo "install-stdlib: no \`staple\` on PATH; building the compiler from this repository"
    set -- cargo run --release --quiet -p staple --manifest-path "$repo/Cargo.toml" --
fi

"$@" build --manifest-path "$destination/staple.kdl" --stdlib "$destination"

echo "Prebuilt standard library objects are in $destination/target"
echo "They are only used by the exact compiler that built them. Reinstall after"
echo "upgrading staple, and install with the same \`staple\` you will run."
