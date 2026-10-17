#!/usr/bin/env bash
set -euo pipefail
receiver=$1
scratch=$(mktemp -d)
trap 'rm -rf -- "$scratch"' EXIT
export PREK_CAGE_HOST=1
printf '\377\000\000\000\377\000\000\000\377\040\012\015' > "$scratch/rgb"
timeout 5 "$receiver" --rgb-png 4 1 "$scratch/capture.png" < "$scratch/rgb"
timeout 5 "$receiver" --png-rgb 4 1 "$scratch/capture.png" > "$scratch/decoded"
cmp "$scratch/rgb" "$scratch/decoded"
refused() {
    if timeout 5 "$receiver" "$@" < "$scratch/rgb" > "$scratch/refused"; then
        echo "image converter accepted invalid input: $*" >&2
        exit 1
    else
        result=$?
        case "$result" in 1|2) ;; *) echo "unexpected converter failure: $result" >&2; exit 1;; esac
    fi
}
refused --rgb-png 0 1 "$scratch/zero.png"
refused --rgb-png 8193 1 "$scratch/large.png"
refused --rgb-png 1x 1 "$scratch/number.png"
refused --rgb-png 4 2 "$scratch/truncated.png"
refused --rgb-png 1 1 "$scratch/trailing.png"
refused --rgb-png 4 1 "$scratch/capture.png"
refused --png-rgb 3 1 "$scratch/capture.png"
refused --png-rgb 4 1 "$scratch/rgb"
timeout 5 "$receiver" --png-rgb 4 1 "$scratch/capture.png" > "$scratch/decoded"
cmp "$scratch/rgb" "$scratch/decoded"
if env -u PREK_CAGE_HOST -u PREK_CAGE_VM "$receiver" --png-rgb 4 1 "$scratch/capture.png"; then
    echo 'image converter accepted an unowned invocation' >&2
    exit 1
fi
echo 'receiver image conversion: exact opaque RGB roundtrip and eight invalid inputs refused'
