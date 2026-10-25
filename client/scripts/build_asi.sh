#!/usr/bin/env bash
# Build mercs2_profiler.asi in the Docker builder (docker/builder.Dockerfile) and place it at ../src/
# (the Shipment's plugin path).
#
# WAD_SIMULATOR_DIR names the wad_simulator checkout whose mercs2_formats the client builds against
# (the profiler's site resolver, mercs2_formats::profiler_sites, is newer than the published crate).
set -euo pipefail
client="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
repo="$(cd "$client/.." && pwd)"
: "${WAD_SIMULATOR_DIR:?set WAD_SIMULATOR_DIR to the wad_simulator checkout to build mercs2_formats from}"
# The whole checkout: mercs2_formats inherits package fields from its workspace root.
wad_simulator="$(cd "$WAD_SIMULATOR_DIR" && pwd)"
test -f "$wad_simulator/crates/mercs2_formats/Cargo.toml"

image=mercs2-profiler-builder
docker build -t "$image" -f "$repo/docker/builder.Dockerfile" "$repo/docker"
docker run --rm \
  -v "$client:/work/client" \
  -v "$wad_simulator:/work/wad_simulator:ro" \
  -v mercs2-profiler-cargo-registry:/usr/local/cargo/registry \
  -w /work/client \
  "$image" \
  cargo build --release --target i686-pc-windows-gnu \
    --config 'patch.crates-io.mercs2_formats.path="/work/wad_simulator/crates/mercs2_formats"'

dll="$client/target/i686-pc-windows-gnu/release/mercs2_profiler.dll"
asi="$repo/src/mercs2_profiler.asi"
if [[ -f "$asi" ]]; then
  echo "before: $(shasum -a 256 "$asi")"
fi
cp "$dll" "$asi"
echo "after:  $(shasum -a 256 "$asi")"
