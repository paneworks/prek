{ package, testName ? "cage_runtime" }:
package.overrideAttrs (_: {
  pname = "prek-local-${testName}-tests";
  outputs = [ "out" ];
  cargoBuildFlags = [ "--test" testName ];
  doCheck = false;
  installPhase = ''
    mkdir -p "$out/bin"
    count=0
    for candidate in target/*/release/deps/${testName}-* target/release/deps/${testName}-*; do
      if [ -f "$candidate" ] && [ -x "$candidate" ]; then
        install -m 755 "$candidate" "$out/bin/prek-local-cage-tests"
        count=$((count + 1))
      fi
    done
    test "$count" -eq 1
  '';
  postInstall = "";
})
