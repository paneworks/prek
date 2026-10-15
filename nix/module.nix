{ config, lib, pkgs, utils, ... }:
let
  cfg = config.services.prek;
  inherit (lib) mkEnableOption mkIf mkOption types;
  moduleName = name:
    builtins.match "[A-Za-z_][A-Za-z0-9_]*(\\.[A-Za-z_][A-Za-z0-9_]*)*" name != null
    && name != "prek" && !lib.hasPrefix "prek." name
    && builtins.length (lib.splitString "." name) <= 16;
  inlineSource = pkgs.writeText "prek-config.lua" (
    if cfg.luaConfig != null then cfg.luaConfig else ''
      local p = require("prek")
      return p.config.preset("minimal")
    ''
  );
  entry = if cfg.configFile != null then /. + toString cfg.configFile else null;
  fileTree = if entry != null then lib.fileset.toSource {
    root = builtins.dirOf entry;
    fileset = lib.fileset.unions [
      entry
      (lib.fileset.maybeMissing ((builtins.dirOf entry) + "/modules"))
    ];
  } else null;
  source = if entry != null then "${fileTree}/${builtins.baseNameOf entry}" else inlineSource;
  configuration = pkgs.runCommand "prek-configuration" {
    nativeBuildInputs = [ cfg.package pkgs.jq ];
  } (''
    mkdir -p "$out/modules"
    cp -- ${lib.escapeShellArg (toString source)} "$out/config.lua"
  '' + lib.optionalString (cfg.configFile != null) ''
    if test -d ${lib.escapeShellArg "${fileTree}/modules"}; then
      cp -R -- ${lib.escapeShellArg "${fileTree}/modules/."} "$out/modules/"
    fi
  '' + lib.concatStrings (lib.mapAttrsToList (name: text:
    let
      file = "modules/${builtins.replaceStrings [ "." ] [ "/" ] name}.lua";
      content = pkgs.writeText "prek-module.lua" text;
    in ''
      mkdir -p -- "$out/$(dirname -- ${lib.escapeShellArg file})"
      cp -- ${content} "$out/"${lib.escapeShellArg file}
    ''
  ) cfg.luaModules) + ''
    ${lib.getExe cfg.package} check --config "$out/config.lua" --json > "$out/validation.json"
    jq -e '.status == "valid" and .mode == "observe"' "$out/validation.json" > /dev/null
  '');
  compositor = pkgs.runCommand "prek-compositor-identity" {
    nativeBuildInputs = [ pkgs.binutils ];
  } ''
    mkdir "$out"
    candidate=${lib.escapeShellArg "${cfg.compositorPackage}/bin/Hyprland"}
    if test -e ${lib.escapeShellArg "${cfg.compositorPackage}/bin/.Hyprland-wrapped"}; then
      candidate=${lib.escapeShellArg "${cfg.compositorPackage}/bin/.Hyprland-wrapped"}
    fi
    readelf -h "$candidate" > /dev/null
    ln -s "$candidate" "$out/Hyprland"
  '';
  compositorExecutable = if cfg.compositorExecutable != null
    then cfg.compositorExecutable else "${compositor}/Hyprland";
  brokerArgs = [
    (lib.getExe cfg.package) "broker" "--mode" cfg.mode
    "--config" "${configuration}/config.lua"
    "--runtime-dir" "/run/${cfg.runtimeDirectory}"
    "--compositor-executable" (toString compositorExecutable)
    "--logind-library" (toString cfg.logindLibrary) "--json"
  ] ++ lib.optionals (cfg.morfExecutable != null) [
    "--morf-executable" (toString cfg.morfExecutable)
  ];
  sessionArgs = [ (lib.getExe cfg.package) "session" "--socket" "/run/${cfg.runtimeDirectory}/control.sock" ]
    ++ lib.optional cfg.session.observations "--observations"
    ++ lib.optional cfg.session.context "--context"
    ++ lib.optional cfg.session.lua "--lua"
    ++ lib.optional cfg.session.input "--input"
    ++ lib.optional cfg.session.ruleMutations "--rule-mutations";
  sessionPackage = pkgs.writeShellScriptBin "prek-session" ''
    exec ${lib.escapeShellArgs sessionArgs} "$@"
  '';
in {
  options.services.prek = {
    enable = mkEnableOption "Prek's supervised input observation broker";
    package = mkOption {
      type = types.package;
      description = "Prek package; the flake module supplies this flake's package for the host architecture.";
    };
    mode = mkOption {
      type = types.enum [ "observe" "route" ];
      default = "observe";
      description = "Broker mode. Route remains unavailable until production takeover gates pass.";
    };
    luaConfig = mkOption {
      type = types.nullOr types.lines;
      default = null;
      description = "Primary Lua configuration. Null uses the minimal preset with no selected devices or rules.";
    };
    luaModules = mkOption {
      type = types.attrsOf types.lines;
      default = { };
      description = "Approved Lua modules keyed by dotted module name, copied as regular files beside the entry source.";
    };
    configFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = "Alternative Lua entry file. Its sibling modules directory is copied and validated with it; cannot be combined with inline Lua configuration/modules.";
    };
    configurationPackage = mkOption {
      type = types.package;
      readOnly = true;
      description = "Immutable configuration tree, checked by the selected Prek package before service activation.";
    };
    compositorPackage = mkOption {
      type = types.package;
      default = config.programs.hyprland.package;
      defaultText = lib.literalExpression "config.programs.hyprland.package";
      description = "Exact installed Hyprland package. Its actual ELF is selected and checked, including the Nix wrapper case.";
    };
    compositorExecutable = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = "Override the trusted actual Hyprland ELF for a nonstandard package layout.";
    };
    morfExecutable = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = "Optional trusted Morf ELF for native lock-provider verification.";
    };
    logindLibrary = mkOption {
      type = types.path;
      default = "${pkgs.systemd}/lib/libsystemd.so.0";
      description = "Trusted absolute libsystemd.so.0 used to verify active logind membership.";
    };
    runtimeDirectory = mkOption {
      type = types.strMatching "[A-Za-z0-9][A-Za-z0-9_.-]*";
      default = "prek";
      description = "Root-owned directory under /run for the authenticated broker socket.";
    };
    session = {
      enable = mkEnableOption "the prek-session launcher, to run as a child of the compositor login session" // { default = true; };
      observations = mkOption { type = types.bool; default = true; description = "Request own-seat configured semantic outcomes."; };
      context = mkOption { type = types.bool; default = true; description = "Request broker-verified compositor context."; };
      lua = mkOption { type = types.bool; default = true; description = "Run approved callbacks in the unprivileged session helper."; };
      input = mkEnableOption "permitted normalized Lua input events; protected Lua policy must also allow them";
      ruleMutations = mkEnableOption "acknowledged Lua rule/layer/mode transactions; protected Lua policy must also allow them";
      package = mkOption {
        type = types.package;
        readOnly = true;
        description = "Launcher for compositor exec-once or equivalent; never starts in the shared systemd user manager.";
      };
    };
  };
  config = mkIf cfg.enable {
    assertions = [
      { assertion = cfg.mode == "observe"; message = "services.prek: production route mode is unavailable; takeover/recovery gates remain pending."; }
      { assertion = cfg.configFile == null || cfg.luaConfig == null && cfg.luaModules == { }; message = "services.prek: choose configFile or inline luaConfig/luaModules."; }
      { assertion = lib.all moduleName (builtins.attrNames cfg.luaModules); message = "services.prek.luaModules: use bounded dotted identifiers outside the reserved prek namespace."; }
      { assertion = cfg.session.lua || !(cfg.session.input || cfg.session.ruleMutations); message = "services.prek.session: input and ruleMutations require lua."; }
    ];
    services.prek.configurationPackage = configuration;
    services.prek.session.package = sessionPackage;
    environment.systemPackages = [ cfg.package ] ++ lib.optional cfg.session.enable sessionPackage;
    environment.pathsToLink = [ "/share/prek" "/share/doc/prek" ];
    services.udev.packages = [ cfg.package ];
    systemd.services.prek = {
      description = "Prek supervised input observation broker";
      wantedBy = [ "multi-user.target" ];
      wants = [ "systemd-logind.service" ];
      after = [ "systemd-logind.service" "systemd-udevd.service" ];
      restartTriggers = [ configuration ];
      serviceConfig = {
        Type = "simple";
        ExecStart = utils.escapeSystemdExecArgs brokerArgs;
        Restart = "on-failure";
        RestartSec = 2;
        RuntimeDirectory = cfg.runtimeDirectory;
        RuntimeDirectoryMode = "0755";
        KillMode = "control-group";
        TimeoutStopSec = 5;
        UMask = "0077";
        NoNewPrivileges = true;
        CapabilityBoundingSet = [ "CAP_DAC_READ_SEARCH" "CAP_SETUID" "CAP_SETGID" "CAP_KILL" "CAP_SYS_PTRACE" ];
        DevicePolicy = "closed";
        DeviceAllow = [ "char-input r" ];
        PrivateTmp = true;
        ProtectSystem = "strict";
        ProtectHome = "read-only";
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        RestrictSUIDSGID = true;
        RestrictAddressFamilies = [ "AF_UNIX" ];
        ReadWritePaths = [ "/run/${cfg.runtimeDirectory}" ];
      };
    };
  };
}
