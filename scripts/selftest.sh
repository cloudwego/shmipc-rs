#!/bin/bash

set -o errexit
set -o nounset
set -o pipefail

cargo +nightly fmt --all -- --check

cargo clippy --all-features -- --deny warnings
cargo test --all-features
cargo bench --all-features
