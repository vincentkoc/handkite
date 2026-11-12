#!/bin/sh
# Focused checks for the no-PIN preparation route; never starts a device runner.
set -eu
cd "$(dirname "$0")/.."
export MACOSX_DEPLOYMENT_TARGET=11.0
export SDKROOT="$(xcrun --sdk macosx --show-sdk-path)"
cargo +1.95.0 test --offline --locked -p handkite-host --lib pin_backend::tests::
cargo +1.95.0 test --offline --locked -p handkite-host --bin handkite-unlock-once
cargo +1.95.0 clippy --offline --locked -p handkite-host --lib --bin handkite-unlock-once -- -D warnings
cargo +1.95.0 fmt --all -- --check
