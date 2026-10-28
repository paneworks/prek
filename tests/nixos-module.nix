{ pkgs, module, package }:
let
  inherit (pkgs) lib;
  evaluate = settings: (import (pkgs.path + "/nixos/lib/eval-config.nix") {
    system = pkgs.stdenv.hostPlatform.system;
    modules = [
      module
      {
        system.stateVersion = "26.05";
        services.prek = {
          enable = true;
          inherit package;
          compositorExecutable = "${pkgs.coreutils}/bin/true";
        };
      }
      { services.prek = settings; }
    ];
  }).config;
  ownAssertions = cfg: builtins.filter
    (entry: !entry.assertion && lib.hasPrefix "services.prek" entry.message) cfg.assertions;
  accepted = cfg: lib.all (entry: entry.assertion) (ownAssertions cfg);
  disabled = evaluate { enable = lib.mkForce false; };
  minimal = evaluate { };
  inline = evaluate {
    runtimeDirectory = "prek-test";
    luaConfig = ''
      local p = require("prek")
      local settings = require("bindings.keyboard")
      settings.policy.allow_runtime_rules = true
      settings.policy.allow_input_events = true
      return settings
    '';
    luaModules."bindings.keyboard" = ''
      local p = require("prek")
      local trigger = p.recognizers.define("module_key", {
        selectors={"keyboard"},inputs={"key"},reply_within="250ms",
      }, function(event)
        if event.cause == "frame" then return {phase="discrete"} end
        return nil
      end)
      return p.config.new {
        policy = {allowed_devices={"keyboard"}},
        devices = {keyboard={keys={"KEY_A"}}},
        modes = {p.modes.define{id="normal",active=true}},
        rules = {p.rules.define{id="a",when_mode="normal",
          trigger=trigger,action=p.actions.event{name="a"}}},
      }
    '';
    session.input = true;
    session.recognizers = true;
    session.ruleMutations = true;
  };
  fromFile = evaluate { configFile = ../examples/lua/modular/config.lua; };
  sessionDisabled = evaluate { session.enable = false; };
  reconnectDisabled = evaluate { session.reconnect = false; };
  route = evaluate {
    mode = "route";
    allowPhysicalTakeover = true;
    runtimeDirectory = "prek-route";
    luaConfig = ''
      local p = require("prek")
      return p.config.new {
        mode = "route",
        policy = {allowed_devices={"keyboard"}},
        devices = {keyboard={keys={"KEY_A"}}},
      }
    '';
    session.commands = true;
    session.keys = true;
  };
  cases = { inherit minimal inline fromFile sessionDisabled reconnectDisabled route; };
  snapshot = cfg: {
    unit = cfg.systemd.services.prek.serviceConfig;
    configuration = toString cfg.services.prek.configurationPackage;
    launcher = toString cfg.services.prek.session.package;
    installedPackage = builtins.elem cfg.services.prek.package cfg.environment.systemPackages;
    installedLauncher = builtins.elem cfg.services.prek.session.package cfg.environment.systemPackages;
    udevPackage = builtins.elem cfg.services.prek.package cfg.services.udev.packages;
    linkedData = builtins.elem "/share/prek" cfg.environment.pathsToLink;
    restartTriggers = map toString cfg.systemd.services.prek.restartTriggers;
    userManagerAgent = builtins.hasAttr "prek" cfg.systemd.user.services;
  };
  snapshots = pkgs.writeText "prek-module-cases.json"
    (builtins.toJSON (lib.mapAttrs (_: snapshot) cases));
  invalid = [
    { mode = "route"; }
    { configFile = ../examples/lua/modular/config.lua; luaConfig = "return {version=1}"; }
    { configFile = ../examples/lua/modular/config.lua; luaModules.extra = "return {}"; }
    { luaModules."../escape" = "return {}"; }
    { luaModules."prek.hidden" = "return {}"; }
    { session.lua = false; session.input = true; }
    { session.lua = false; session.recognizers = true; }
    { session.lua = false; session.ruleMutations = true; }
    { session.commands = true; }
    { session.keys = true; }
  ];
in
assert !(builtins.hasAttr "prek" disabled.systemd.services);
assert !(builtins.hasAttr "prek" disabled.systemd.user.services);
assert lib.all accepted (builtins.attrValues cases);
assert lib.all (settings: !(accepted (evaluate settings))) invalid;
pkgs.runCommand "prek-nixos-module-check" {
  nativeBuildInputs = [ pkgs.python3 pkgs.systemd pkgs.shellcheck ];
} ''
  python3 ${./nixos-module.py} ${snapshots} ${package}/bin/prek "$out"
''
