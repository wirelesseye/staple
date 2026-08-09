#!/bin/sh
set -eu

script_directory=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
source_directory="$script_directory/../stdlib"
destination=${1:-"${HOME:?HOME must be set}/.local/lib/staple/stdlib"}

if [ ! -d "$source_directory/std" ]; then
    echo "install-stdlib: could not find the Staple standard library at $source_directory" >&2
    exit 1
fi

mkdir -p "$destination"
cp -R "$source_directory/." "$destination/"

echo "Installed the Staple standard library to $destination"
