{ package }:
package.overrideAttrs (_: {
  pname = "prek-isolated-managed-tests";
  outputs = [ "out" ];
  cargoBuildFlags = [ "--features" "native-test-harness" "--test" "native_managed" ];
  doCheck = false;
  installPhase = ''
    mkdir -p "$out/bin"
    count=0
    for candidate in target/*/release/deps/native_managed-* target/release/deps/native_managed-*; do
      if [ -f "$candidate" ] && [ -x "$candidate" ]; then
        "$candidate" --inventory > inventory
        test "$(wc -l < inventory)" -eq 2
        grep -Fx 'PREK_MANAGED_KEY_WORKER_INVENTORY_V1' inventory
        grep -Fx '["managed::selective_key_worker"]' inventory
        install -m 755 "$candidate" "$out/bin/prek-native-managed-tests"
        count=$((count + 1))
      fi
    done
    test "$count" -eq 1
  '';
  postInstall = "";
})
