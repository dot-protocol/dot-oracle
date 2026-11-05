#!/usr/bin/env bash
# Compare release binaries from two fresh source paths and independent target directories.
# This is a same-runner Linux test, not independent-builder or archive reproducibility.
set -euo pipefail
cd "$(dirname "$0")/.."
target=x86_64-unknown-linux-musl
revision=$(git rev-parse HEAD)
epoch=$(git show -s --format=%ct "$revision")
output=${REPRO_OUTPUT:?set REPRO_OUTPUT to a directory for the evidence}
reference=${REPRO_REFERENCE:?set REPRO_REFERENCE to the unpacked release binaries}
reference=$(cd "$reference" && pwd)
mkdir -p "$output"
output=$(cd "$output" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
rustc -vV > "$output/toolchain.txt"
cargo -V >> "$output/toolchain.txt"
printf '%s\n' "$revision" > "$output/source.txt"
for pass in first second; do
    source="$work/$pass"
    mkdir -p "$source"
    git archive "$revision" | tar -x -C "$source"
    (
        cd "$source"
        export ORACLE_BUILD_COMMIT="$revision" SOURCE_DATE_EPOCH="$epoch" CARGO_INCREMENTAL=0
        export CARGO_TARGET_DIR="$source/target"
        export RUSTFLAGS="--remap-path-prefix=$source=/oracle/source"
        cargo build --release --locked --target "$target" -p oracle-server -p ox
    )
    mkdir -p "$output/$pass"
    cp "$source/target/$target/release/oracle" "$source/target/$target/release/ox" "$output/$pass/"
    (cd "$output/$pass" && sha256sum oracle ox) > "$output/$pass.sha256"
done
(cd "$reference" && sha256sum oracle ox) > "$output/release.sha256"
for binary in oracle ox; do
    cmp "$output/first/$binary" "$output/second/$binary"
    cmp "$output/first/$binary" "$reference/$binary"
done
cmp "$output/first.sha256" "$output/second.sha256"
printf 'Two fresh Linux builds of %s match the release binaries.\n' "$revision"
