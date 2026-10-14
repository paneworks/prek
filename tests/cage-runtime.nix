{ package }:
package.overrideAttrs (_: {
  pname = "prek-local-cage-tests";
  cargoBuildFlags = [ "--test" "cage_runtime" ];
  doCheck = false;
  installPhase = ''
    mkdir -p "$out/bin"
    count=0
    for candidate in target/*/release/deps/cage_runtime-* target/release/deps/cage_runtime-*; do
      if [ -f "$candidate" ] && [ -x "$candidate" ]; then
        install -m 755 "$candidate" "$out/bin/prek-local-cage-tests"
        count=$((count + 1))
      fi
    done
    test "$count" -eq 1
  '';
  postInstall = "";
})
