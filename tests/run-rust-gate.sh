#!/usr/bin/env bash
set -euo pipefail

rustc --version
cargo fmt --all -- --check
for model in root_session root_routing cage_startup nix_dist; do
    python3 -m unittest discover -s tests/support -p "test_${model}.py" -v
done
cargo check --locked --all-targets --all-features
cargo clippy --locked --all-targets --all-features -- -Dwarnings
cargo test --locked --all-targets --all-features
cargo test --locked --doc --all-features
RUSTDOCFLAGS='-D warnings' cargo doc --locked --all-features --no-deps
cargo build --locked --bin prek
target="${CARGO_TARGET_DIR:-target}"
if [[ "$target" != /* ]]; then target="$PWD/$target"; fi
export PREK_TEST_LUA_BINARY="$target/debug/prek"
helpers=(
    route_worker::tests::recognizers::actual::supervised_callbacks_cross_the_sole_worker_protocol_with_native_ownership
    rules::tests::composition::helper::supervised_retained_touch_handoffs_use_one_native_coordinator
    routing::tests::custom::chains::helper::supervised_custom_steps_use_complete_native_routing_scopes
    route_worker::tests::recognizers::composed::helper::supervised_composed_plans_survive_real_private_configuration_reconstruction
)
for index in "${!helpers[@]}"; do
    cargo test --locked --lib "${helpers[index]}" -- --exact --ignored --test-threads=1 \
        | tee "$target/native-helper-${index}.log"
    grep -F 'test result: ok. 1 passed; 0 failed;' "$target/native-helper-${index}.log"
done
"$PREK_TEST_LUA_BINARY" --help
