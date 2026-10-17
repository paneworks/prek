{ pkgs, package }:
pkgs.writeShellScriptBin "prek-root-sleep-watch-test" ''
  set -euo pipefail
  case "''${1-}" in
    --check-library) test_name=sleep_watch::native_probe::library_preflight ;;
    "") test_name=sleep_watch::native_probe::root_delay_preflight ;;
    *) echo 'Expected --check-library or no arguments.' >&2; exit 2 ;;
  esac
  if [ "$#" -gt 1 ]; then exit 2; fi
  export PREK_SLEEP_LOGIND_LIBRARY=${pkgs.systemd}/lib/libsystemd.so.0
  umask 077
  log=$(${pkgs.coreutils}/bin/mktemp)
  trap '${pkgs.coreutils}/bin/rm -f -- "$log"' EXIT
  ${pkgs.coreutils}/bin/timeout --signal=TERM --kill-after=2s 15s \
    ${package.nativeTests}/libexec/prek-native-tests --ignored --exact "$test_name" --nocapture 2>&1 \
    | ${pkgs.coreutils}/bin/tee "$log"
  ${pkgs.gnugrep}/bin/grep -F 'test result: ok. 1 passed; 0 failed;' "$log"
  if [ "$test_name" = sleep_watch::native_probe::root_delay_preflight ]; then
    ${pkgs.gnugrep}/bin/grep -F 'PREK_SLEEP_RECEIPT={"status":"passed"' "$log"
  fi
''
