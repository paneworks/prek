-- prek's build, as recipes. This replaced the Makefile; there is no other.
--
--   make            the recipes, with what each of them says it does
--   make build      the binary
--   make test       the suite
--   make verify     the whole local gate
--
-- At an oslo prompt in this directory `make` is enough; everywhere else it is `oslo make`.
-- CI has no oslo, so it calls the language's own tool -- nothing here is on the release path.

local make = oslo.make

-- The name is fixed at generation time; the version comes from Cargo.toml, the language's own
-- manifest and the file `veri` rewrites when `make release` cuts a version. One source, and it is not a file wing invented.
local NAME = "prek"
local VERSION = ((oslo.fs.read("Cargo.toml") or ""):match('version%s*=%s*"([^"]+)"')) or "0.0.0"
local PREFIX = os.getenv("PREFIX") or (os.getenv("HOME") .. "/.local")

------------------------------------------------------------------ what was built

local function dim(text)
  return oslo.ui.style(text, { dim = true })
end

local function line(label, value)
  print(dim(oslo.ui.pad(label, 8)) .. value)
end

-- `1524720` -> `1,524,720`. A number this long is read in groups or not at all.
local function grouped(n)
  local text = tostring(math.floor(n))
  local out = text:sub(-3)
  local at = #text - 3
  while at > 0 do
    out = text:sub(math.max(1, at - 2), at) .. "," .. out
    at = at - 3
  end
  return out
end

-- Asked of the ELF, not assumed. `ldd` is not enough on its own: it prints "statically linked" for
-- a binary that still carries an INTERP and will not start.
local function linkage(path)
  local segments = oslo.run{ "readelf", "-l", path, capture = true }
  if not segments.ok then return nil end
  local dynamic = oslo.run{ "readelf", "-d", path, capture = true }
  if (segments.out or ""):find("program interpreter") or (dynamic.out or ""):find("NEEDED") then
    return "dynamic"
  end
  return "static"
end

-- What was built, how big it is, and whether it needs anything on the target machine. Silent when
-- the artifact is not there, so a recipe that builds nothing does not pretend it did.
local function report(path)
  local stat = oslo.fs.stat(path)
  if not stat then return end
  local megabytes = ("%.2f MB"):format(stat.size / 1048576)

  print("")
  print(oslo.ui.title(("%s %s   %s"):format(NAME, VERSION, megabytes)))
  line("binary", path)
  -- Bytes beside megabytes: `1.45 MB` cannot be subtracted from last week's `1.42 MB` to get one.
  line("size", megabytes .. dim("   " .. grouped(stat.size) .. " bytes"))

  local kind = linkage(path)
  if kind == "static" then
    line("linking", oslo.ui.style("✓ static", { fg = "green" }) ..
                    dim("   no runtime dependencies"))
  elseif kind == "dynamic" then
    line("linking", oslo.ui.style("dynamic", { fg = "yellow" }) ..
                    dim("   needs a matching libc on the target machine"))
  end
  print("")
end

make.recipe{ name = "version", desc = "what this checkout calls itself",
             run = function() print(("%s v%s"):format(NAME, VERSION)) end }

local function need(tool, why)
  assert(oslo.run{ "sh", "-c", "command -v " .. tool, capture = true }.ok, why)
end

make.recipe{
  name = "release",
  desc = "cut a version: --type patch | minor | major | M.m.p",
  params = { { "--type", desc = "patch | minor | major | M.m.p" } },
  run = function(a)
    need("git-rel", "git-rel is not installed; install it first")
    assert(type(a.type) == "string",
           "which release? make release --type patch|minor|major|M.m.p")
    sh.git("rel", a.type)
  end,
}

make.recipe{
  name = "changelog",
  desc = "regenerate CHANGELOG.md",
  run = function()
    need("git-cliff", "git-cliff is not installed; install it first")
    sh.git("cliff", "-o", "CHANGELOG.md")
  end,
}

------------------------------------------------------------------ configuration

-- A project that carries its own configuration keeps it in `config/`, and this is what installs it:
-- `config/*` lands in `~/.config/prek/*`. Nothing to install is not a failure -- most
-- projects have no config/ at all, and this says so and stops.
local function project_root()
  -- Asked of git rather than assumed from the working directory, so `make configs` installs this
  -- project's configuration from anywhere in the tree. Outside a repository there is nothing to
  -- ask, and where the command was run is the best answer available.
  local top = oslo.run{ "git", "rev-parse", "--show-toplevel", capture = true }
  local path = top.ok and (top.out or ""):match("^%s*(.-)%s*$") or ""
  if path ~= "" then return path end
  return oslo.sys.pwd()
end

make.recipe{
  name = "configs",
  desc = "install config/ into $XDG_CONFIG_HOME/prek",
  params = { { "--dest", desc = "somewhere other than the config directory" } },
  run = function(a)
    local top = project_root()
    local source = top .. "/config"
    if not oslo.fs.stat(source .. "/") then
      print("no config/ in " .. top .. "; nothing to install")
      return
    end
    assert(oslo.run{ "sh", "-c", "command -v rsync", capture = true }.ok,
           "rsync is not installed; install it first")

    local dest = a.dest
    if not dest then
      local config = os.getenv("XDG_CONFIG_HOME")
      if not config or config == "" then config = os.getenv("HOME") .. "/.config" end
      dest = config .. "/" .. NAME
    end
    sh.mkdir("-p", dest)

    -- One entry at a time, each mirrored with --delete, rather than one --delete over the whole
    -- tree: the destination is also where the user keeps their own edits, and a tree-wide mirror
    -- would take those with it.
    local synced = 0
    for _, path in ipairs(oslo.fs.glob(source .. "/*")) do
      local name = oslo.path.name(path)
      if oslo.fs.stat(path .. "/") then
        sh.mkdir("-p", dest .. "/" .. name)
        sh.rsync("-a", "--delete", path .. "/", dest .. "/" .. name .. "/")
      else
        sh.rsync("-a", path, dest .. "/" .. name)
      end
      synced = synced + 1
    end
    print(("%d entr%s -> %s"):format(synced, synced == 1 and "y" or "ies", dest))
  end,
}

---------------------------------------------------------------------------- rust

make.recipe{ name = "build", desc = "build the input tools",
             run = function()
               sh.cargo("build", "--locked", "--bin", NAME)
               report("target/debug/" .. NAME)
             end }
make.alias("b", "build")

make.recipe{ name="build-aarch64",desc="cross-build and inspect dynamic ARM64 ELF without executing it",
             run=function()
               assert(oslo.run{"nix","--option","eval-cache","false","--extra-experimental-features","nix-command flakes","--max-jobs","1","--cores","2","develop","--no-write-lock-file",".#cross-aarch64","--command","bash","tests/run-aarch64-build.sh"}.ok,"ARM64 cross-build or ELF verification failed")
             end }

make.recipe{
  name = "run",
  desc = "list devices, or observe one with --device PATH",
  params = { { "--device", desc = "event device to observe" } },
  run = function(a)
    if a.device then
      sh.cargo("run", "--locked", "--", "watch", a.device)
    else
      sh.cargo("run", "--locked", "--", "devices")
    end
  end,
}
make.alias("r", "run")

make.recipe{ name = "example", desc = "recognize a simulated double tap without hardware",
             run = function() sh.cargo("run", "--locked", "--example", "main") end }

make.recipe{ name = "test", desc = "the suite",
             run = function() sh.cargo("test", "--locked", "--all-targets") end }
make.alias("t", "test")

make.recipe{ name = "test-handoff-boundary", desc = "bounded evdev fanout-prefix and ownership models",
             run = function()
               sh.cargo("test", "--locked", "--lib", "linux::handoff::")
               sh.cargo("test", "--locked", "--lib", "linux::ingress::")
             end }

make.recipe{ name="test-hotplug-discovery",desc="descriptor-bound metadata admission and private filesystem replacement models",
             run=function() sh.cargo("test","--locked","--lib","linux::discovery::tests::") end }

make.recipe{ name="test-virtual-publication",desc="nonce-bound virtual lifetimes, processed metadata and stale consumer receipts",
             run=function()
               sh.cargo("test","--locked","--lib","kernel::tests::")
               sh.cargo("test","--locked","--lib","transport::tests::")
               sh.cargo("test","--locked","--lib","linux::discovery::tests::")
             end }

make.recipe{ name="test-native-preparation",desc="published native descriptor binding and refusal before unproven takeover",
             run=function() sh.cargo("test","--locked","--lib","route_worker::tests::preparation::") end }

make.recipe{ name="test-axis-fuzz",desc="bounded axis-filter counterexamples; no device access",
             run=function() sh.cargo("test","--locked","--lib","kernel::tests::fuzz::") end }

make.recipe{ name="test-native-geometry",desc="native slot/axis preflight and private publication refusal; no device access",
             run=function() sh.cargo("test","--locked","--lib","kernel::tests::geometry::") end }

make.recipe{ name="test-cli-output",desc="structured CLI diagnostics and deterministic closed-output handling",
             run=function() sh.cargo("test","--locked","--test","cli","t04_") end }

make.recipe{ name="test-inventory",desc="real private-file inventory failures and JSON/text output; no device access",
             run=function() sh.cargo("test","--locked","--lib","linux::inventory_tests::") end }

make.recipe{ name = "test-suspend-epoch", desc = "suspend-sensitive process clocks and native IO/reaping gates",
             run = function() sh.cargo("test", "--locked", "--lib", "process_clock::tests::") end }

make.recipe{ name = "test-sleep-watch", desc = "authoritative sleep-provider lifecycle and private bus models",
             run = function() sh.cargo("test", "--locked", "--lib", "sleep_watch::tests::") end }

make.recipe{ name="build-root-sleep-watch",desc="build user-launched delay-only root probe without running it",
             run=function()
               sh.mkdir("-p","target/root-sleep-watch")
               assert(oslo.run{"nix","--option","eval-cache","false","--extra-experimental-features","nix-command flakes","--max-jobs","1","--cores","2","build","--no-write-lock-file","--out-link",project_root().."/target/root-sleep-watch/probe",".#root-sleep-watch-test"}.ok,"root sleep probe build failed")
             end }

make.recipe{ name="test-sleep-watch-library",desc="unprivileged protected library ABI without system bus access",
             deps={"build-root-sleep-watch"},
             run=function() assert(oslo.run{project_root().."/target/root-sleep-watch/probe/bin/prek-root-sleep-watch-test","--check-library"}.ok,"protected sleep library preflight failed") end }

make.recipe{ name = "test-session-reconnect", desc = "same-login native agent re-execution, bounds and termination",
             run = function()
               sh.cargo("test", "--locked", "--lib", "session_launcher::tests::")
               sh.cargo("test", "--locked", "--test", "session_launcher")
             end }

make.recipe{ name = "test-routed-replay", desc = "offline downstream ownership, writes and recovery",
             run = function()
               sh.cargo("test", "--locked", "--lib", "replay::routed::")
               sh.cargo("test", "--locked", "--test", "cli", "routed_replay")
               sh.cargo("test", "--locked", "--lib", "lua::replay::tests::routed_")
             end }

make.recipe{ name = "test-custom-replay", desc = "deterministic source-bound custom offline replay",
             run = function()
               sh.cargo("test", "--locked", "--lib", "replay::configured::")
               sh.cargo("test", "--locked", "--lib", "lua::replay::tests::")
               sh.cargo("test", "--locked", "--test", "cli", "replay")
             end }

make.recipe{ name = "test-custom-chains", desc = "private native custom-scope and ticket-deadline checks",
             run = function()
               sh.cargo("test", "--locked", "--lib", "routing::tests::custom::chains::")
               sh.cargo("test", "--locked", "--lib", "recognition::tests::native_ticket_deadline")
             end }

make.recipe{ name = "test-composed-worker", desc = "complete composed native plans through sole-worker commands",
             run = function()
               sh.cargo("test", "--locked", "--lib", "route_worker::tests::recognizers::composed::")
               sh.cargo("test", "--locked", "--lib", "config::recognizer_walk_tests::coordinated_")
               sh.cargo("test", "--locked", "--lib", "service::rule_control::tests::composed_native_")
               sh.cargo("test", "--locked", "--lib", "service::rule_control::tests::a_public_flat_")
             end }

make.recipe{ name = "test-composition", desc = "native sequence routing and recursive recognizer approval",
             deps = { "test-custom-chains", "test-composed-worker" },
             run = function()
               sh.cargo("test", "--locked", "--lib", "rules::tests::composition::")
               sh.cargo("test", "--locked", "--lib", "routing::tests::sequence::")
               sh.cargo("test", "--locked", "--lib", "config::recognizer_walk_tests::")
               sh.cargo("test", "--locked", "--lib", "lua::recognizers::constructor_tests::nested_native_declarations")
               sh.cargo("test", "--locked", "--lib", "recognition::tests::native_touch_continuation")
               sh.cargo("test", "--locked", "--lib", "lua::recognizers::tests::an_initial_native_touch_timer")
               sh.cargo("test", "--locked", "--lib", "gestures::tests::native_owned_held_contacts")
               sh.cargo("test", "--locked", "--lib", "router::tests::chain::")
             end }

make.recipe{ name = "test-composed-source", desc = "public source, broker catalogs and composed replay admission",
             deps = { "test-composed-worker", "test-custom-replay" },
             run = function()
               sh.cargo("test", "--locked", "--lib", "lua::recognizers::constructor_tests::source_constructor_compiles_nested_")
               sh.cargo("test", "--locked", "--lib", "service::source::tests::")
               sh.cargo("test", "--locked", "--lib", "service::recognizers::tests::composed_source_")
             end }

make.recipe{
  name = "test-routing-helper",
  desc = "test sole-worker callbacks with the real unprivileged Lua helper",
  deps = { "build" },
  run = function()
    local binary = project_root() .. "/target/debug/" .. NAME
    local tested = oslo.run{ "env", "PREK_TEST_LUA_BINARY=" .. binary,
      "cargo", "test", "--locked", "--lib",
      "route_worker::tests::recognizers::actual::supervised_callbacks_cross_the_sole_worker_protocol_with_native_ownership",
      "--", "--exact", "--ignored", "--test-threads=1", capture = true }
    io.write(tested.out or "")
    io.stderr:write(tested.err or "")
    assert(tested.ok and (tested.out or ""):find("test result: ok. 1 passed;", 1, true),
           "native routing helper test failed or selected no case")
  end,
}

make.recipe{ name = "test-all", desc = "the suite, with every feature on",
             run = function() sh.cargo("test", "--locked", "--all-targets", "--all-features") end }

make.recipe{ name="test-lua-tasks", desc="owned coroutine scheduling, joining and cancellation; no devices",
             run=function()
               for _,args in ipairs({{"cargo","test","--locked","--lib","lua::tasks"}, {"cargo","test","--locked","--test","lua_session","task"}}) do
                 args.capture=true
                 local tested=oslo.run(args)
                 io.stdout:write(tested.out or ""); io.stderr:write(tested.err or "")
                 assert(tested.ok and (tested.out or ""):match("running [1-9]%d* tests?"),"Lua task gate failed or selected no tests")
               end
             end }

make.recipe{ name="test-lua-lifetimes", desc="disposed subscription and timer capture release; no devices",
             run=function()
               local tested=oslo.run{"cargo","test","--locked","--lib","lua::lifetimes::",capture=true}
               io.stdout:write(tested.out or ""); io.stderr:write(tested.err or "")
               assert(tested.ok and (tested.out or ""):match("running [1-9]%d* tests?"),"Lua lifetime gate failed or selected no tests")
             end }

make.recipe{
  name = "test-composition-helper",
  desc = "test private held-touch composition with the real unprivileged Lua helper",
  deps = { "build" },
  run = function()
    local tested = oslo.run{ "env", "PREK_TEST_LUA_BINARY=" .. project_root() .. "/target/debug/" .. NAME,
      "cargo", "test", "--locked", "--lib",
      "rules::tests::composition::helper::supervised_retained_touch_handoffs_use_one_native_coordinator",
      "--", "--exact", "--ignored", "--test-threads=1", capture = true }
    io.write(tested.out or "")
    io.stderr:write(tested.err or "")
    assert(tested.ok and (tested.out or ""):find("test result: ok. 1 passed;", 1, true),
           "native composition helper test failed or selected no case")
  end,
}

make.recipe{
  name = "test-chain-helper",
  desc = "test private consuming chains with the real unprivileged Lua helper",
  deps = { "build" },
  run = function()
    local tested = oslo.run{ "env", "PREK_TEST_LUA_BINARY=" .. project_root() .. "/target/debug/" .. NAME,
      "cargo", "test", "--locked", "--lib",
      "routing::tests::custom::chains::helper::supervised_custom_steps_use_complete_native_routing_scopes",
      "--", "--exact", "--ignored", "--test-threads=1", capture = true }
    io.write(tested.out or "")
    io.stderr:write(tested.err or "")
    assert(tested.ok and (tested.out or ""):find("test result: ok. 1 passed;", 1, true),
           "native chain helper test failed or selected no case")
  end,
}

make.recipe{
  name = "test-composed-worker-helper",
  desc = "test full worker plan reconstruction with the real unprivileged Lua helper",
  deps = { "build" },
  run = function()
    local tested = oslo.run{ "env", "PREK_TEST_LUA_BINARY=" .. project_root() .. "/target/debug/" .. NAME,
      "cargo", "test", "--locked", "--lib",
      "route_worker::tests::recognizers::composed::helper::supervised_composed_plans_survive_real_private_configuration_reconstruction",
      "--", "--exact", "--ignored", "--test-threads=1", capture = true }
    io.write(tested.out or "")
    io.stderr:write(tested.err or "")
    assert(tested.ok and (tested.out or ""):find("test result: ok. 1 passed;", 1, true),
           "native composed worker helper test failed or selected no case")
  end,
}

make.recipe{ name = "test-root-session", desc = "unprivileged remote-handoff transport and cleanup models",
             run = function()
               sh.python3("-m", "unittest", "discover", "-s", "tests/support", "-p", "test_root_session.py", "-v")
               sh.bash("-n", "tests/run-root-handoff.sh")
               sh.shellcheck("tests/run-root-handoff.sh")
             end }

make.recipe{
  name = "test-feedback",
  desc = "bounded LED relay and independent source-writer models; no device access",
  run = function()
    sh.cargo("test", "--locked", "--lib", "linux::feedback::tests", "--", "--test-threads=1")
    sh.cargo("test", "--locked", "--lib", "linux::reader_buffer_tests::writable_reopen_does_not_share_the_reader_cursor_or_read_access", "--", "--exact")
  end,
}

make.recipe{ name = "test-root-routing", desc = "unprivileged kernel-runner receipts and cleanup tests",
             run = function()
               sh.python3("-m", "unittest", "discover", "-s", "tests/support", "-p", "test_root_routing.py", "-v")
               sh.bash("-n", "tests/run-root-routing.sh")
               sh.shellcheck("tests/run-root-routing.sh")
             end }

make.recipe{ name="build-root-routing",desc="build the user-launched kernel runner without running it",
             run=function()
               sh.mkdir("-p","target/root-routing")
               assert(oslo.run{"nix","--option","eval-cache","false","--extra-experimental-features","nix-command flakes","--max-jobs","1","--cores","2","build","--no-write-lock-file","--out-link",project_root().."/target/root-routing/probe",".#root-routing-test"}.ok,"root routing probe build failed")
             end }

local function cage_test(fixture, flags)
  local command = { "nix", "--option", "eval-cache", "false", "--extra-experimental-features",
    "nix-command flakes", "--max-jobs", "1", "--cores", "2", "run", "--no-write-lock-file",
    ".#" .. fixture, "--", "--hardware-rendering" }
  for _, flag in ipairs(flags) do command[#command + 1] = flag end
  assert(oslo.run(command).ok, "local Cage fixture failed")
end

make.recipe{ name = "test-cage", desc = "local GPU Cage/Hyprland keyboard, Lua and window receipts",
             run = function() cage_test("cage-test", { "--layers", "--input", "--power", "--windows" }) end }

make.recipe{ name = "test-cage-stock", desc = "modern pinned stock Cage/Hyprland without a dependency override",
             run = function() cage_test("cage-test-stock", { "--layers", "--input", "--power", "--windows" }) end }

make.recipe{ name = "test-cage-lua-stock", desc = "modern stock Cage/Hyprland with an actual Lua config",
             run = function() cage_test("cage-test-stock", { "--hyprland-config", "lua", "--modes", "--input", "--power", "--windows" }) end }

make.recipe{ name="test-keyboard-quiesce",desc="real held-key disable characterization with explicit test-only Wayland backend fix",
             run=function()
               cage_test("cage-test-wayland-fix",{"--keyboard-quiesce"})
               cage_test("cage-test-wayland-fix",{"--keyboard-quiesce","--hyprland-config","lua"})
             end }

make.recipe{ name="test-keyboard-ownership",desc="independent keyboard modifier ownership in the owned local fixture",
             run=function()
               cage_test("cage-test-wayland-fix",{"--keyboard-ownership","--hyprland-config","lua"})
               cage_test("cage-test-wayland-fix",{"--keyboard-ownership"})
             end }

make.recipe{ name="test-key-output",desc="transactional remap and chord ownership, backpressure and retirement; no devices",
             run=function()
               sh.cargo("test","--locked","--lib","keys::output::tests")
               sh.cargo("test","--locked","--lib","kernel::tests::cancelled_key_batch")
             end }

make.recipe{ name="test-key-worker",desc="scoped key scheduling, repeats and raw-prefix ordering; no devices",
             run=function()
               sh.cargo("test","--locked","--lib","keys::worker::tests")
               sh.cargo("test","--locked","--lib","routing::keys::tests")
             end }

make.recipe{ name="test-software-worker",desc="private software leases, exact output scopes and real regular-file worker IPC; no devices",
             run=function()
               for _,filter in ipairs({"keys::worker::tests::producers","route_worker::keys::tests::producers","route_worker::tests::recognizers::keys::producers"}) do
                 local tested=oslo.run{"cargo","test","--locked","--lib",filter,capture=true}
                 io.stdout:write(tested.out or ""); io.stderr:write(tested.err or "")
                 assert(tested.ok and (tested.out or ""):match("running [1-9]%d* tests?"),"software worker gate failed or selected no tests: "..filter)
               end
             end }

make.recipe{ name="test-key-positions",desc="ordered key edges, original positions and retained cancellation scopes; no devices",
             run=function()
               sh.cargo("test","--locked","--lib","rules::edges::tests")
               sh.cargo("test","--locked","--lib","rules::tests::composition::motion")
             end }

make.recipe{ name="test-key-provenance",desc="bounded original press evidence across native key recognizers; no devices",
             run=function()
               for _,filter in ipairs({"rules::presses::tests","routing::keys::projection::tests"}) do
                 local tested=oslo.run{"cargo","test","--locked","--lib",filter,capture=true}
                 io.stdout:write(tested.out or ""); io.stderr:write(tested.err or "")
                 assert(tested.ok and (tested.out or ""):match("running [1-9]%d* tests?"),"key provenance gate failed or selected no tests: "..filter)
               end
             end }

make.recipe{ name="test-key-admission",desc="event-local timed conditions and exact native/custom parent cancellation; no devices",
             run=function()
               for _,filter in ipairs({"rules::presses::tests::admission","routing::tests::custom::chains","rules::tests::composition"}) do
                 local tested=oslo.run{"cargo","test","--locked","--lib",filter,capture=true}
                 io.stdout:write(tested.out or ""); io.stderr:write(tested.err or "")
                 assert(tested.ok and (tested.out or ""):match("running [1-9]%d* tests?"),"key admission gate failed or selected no tests: "..filter)
               end
             end }

make.recipe{ name="test-key-participants",desc="exact participants, original-edge sequences and bounded deferred completions; no devices",
             run=function()
               for _,filter in ipairs({"rules::presses::tests","keys::tests","routing::tests::custom::chains","rules::tests::composition"}) do
                 local tested=oslo.run{"cargo","test","--locked","--lib",filter,capture=true}
                 io.stdout:write(tested.out or ""); io.stderr:write(tested.err or "")
                 assert(tested.ok and (tested.out or ""):match("running [1-9]%d* tests?"),"key participants gate failed or selected no tests: "..filter)
               end
             end }

make.recipe{ name="test-key-projection",desc="per-press raw-key ownership and complete report projection; no devices",
             run=function()
               sh.cargo("test","--locked","--lib","router::keyboard::tests")
             end }

make.recipe{ name="test-key-journal",desc="pending press decisions, bounded raw FIFO and fallback cadence; no devices",
             run=function()
               for _,filter in ipairs({"router::keyboard::tests::pending","routing::keys::projection::tests","keys::worker::tests::coordinator","routing::keys::ordering"}) do
                 local tested=oslo.run{"cargo","test","--locked","--lib",filter,capture=true}
                 io.stdout:write(tested.out or ""); io.stderr:write(tested.err or "")
                 assert(tested.ok and (tested.out or ""):match("running [1-9]%d* tests?"),"key journal gate failed or selected no tests: "..filter)
               end
             end }

make.recipe{ name="test-key-coordinator",desc="fenced selective routing and shared raw/generated ordering; no devices",
             run=function()
               for _,filter in ipairs({"keys::worker::tests::coordinator","keys::worker::tests::independence","routing::keys::ordering","router::keyboard::tests",
                   "route_worker::keys::tests::coordinator","route_worker::tests::recognizers::keys"}) do
                 local tested=oslo.run{"cargo","test","--locked","--lib",filter,capture=true}
                 io.stdout:write(tested.out or ""); io.stderr:write(tested.err or "")
                 assert(tested.ok and (tested.out or ""):match("running [1-9]%d* tests?"),"coordinator gate failed or selected no tests: "..filter)
               end
             end }

make.recipe{ name="test-native-key-worker",desc="managed generated-output slots, publication replies and native worker policy; no devices",
             run=function()
               sh.cargo("test","--locked","--lib","route_worker::keys::tests")
               sh.cargo("test","--locked","--lib","service::transport_offer::tests")
               sh.cargo("test","--locked","--lib","route_worker::tests::recognizers::keys")
               sh.cargo("test","--locked","--lib","linux::owned_service_tests")
             end }

make.recipe{ name="test-generated-keyboard",desc="generated keyboard role, publication and readiness models; no devices",
             run=function()
               sh.cargo("test","--locked","--lib","keys::native::tests")
               sh.cargo("test","--locked","--lib","kernel::tests::generated")
             end }

make.recipe{ name="test-native-managed-model",desc="managed native fixture models, build and inventory only; no devices",
             run=function()
               local tested=oslo.run{"cargo","test","--locked","--features","native-test-harness","--lib","route_worker::native_fixture",capture=true}
               io.stdout:write(tested.out or ""); io.stderr:write(tested.err or "")
               assert(tested.ok and (tested.out or ""):match("running [1-9]%d* tests?"),"managed fixture models failed or selected no tests")
               local built=oslo.run{"bash","-c",[[set -euo pipefail
cargo test --locked --features native-test-harness --test native_managed --no-run --message-format=json | jq -ers '
  [.[] | select(.reason=="compiler-artifact" and .target.name=="native_managed" and .executable!=null) | .executable] | unique |
  if length==1 then .[0] else error("expected one managed fixture executable") end'
]],capture=true}
               assert(built.ok,"managed fixture build failed: "..(built.err or ""))
               local executable=(built.out or ""):gsub("%s+$","")
               local listed=oslo.run{executable,"--inventory",capture=true}
               local expected='PREK_MANAGED_KEY_WORKER_INVENTORY_V1\n["managed::selective_key_worker"]'
               assert(listed.ok and (listed.out==expected or listed.out==expected.."\n"),"managed fixture inventory differs: "..string.format("%q",listed.out or ""))
             end }

make.recipe{ name="test-key-output-cage",desc="actual key output driver and app rendering on owned protocol keyboards",
             run=function()
               cage_test("cage-test-wayland-fix",{"--key-output","--hyprland-config","lua"})
               cage_test("cage-test-wayland-fix",{"--key-output"})
             end }

make.recipe{ name="test-key-worker-cage",desc="scoped scheduler and raw/generated ordering on owned protocol keyboards",
             run=function()
               cage_test("cage-test-wayland-fix",{"--key-output","--key-worker","--hyprland-config","lua"})
               cage_test("cage-test-wayland-fix",{"--key-output","--key-worker"})
             end }

make.recipe{ name="test-key-worker-cage-dev",desc="current Cargo scheduler fixture with an explicit immutable compositor runner",
             params={{"--runner",desc="absolute Nix store prek-cage-host-test executable"}},
             run=function(a)
               assert(type(a.runner)=="string" and a.runner:match("^/nix/store/[^/]+/bin/prek%-cage%-host%-test$"),
                      "supply the immutable --runner executable explicitly")
               local built=oslo.run{"bash","-c",[[set -euo pipefail
cargo test --locked --features native-test-harness --test key_output_cage --no-run --message-format=json | jq -ers '
  [.[] | select(.reason=="compiler-artifact" and .target.name=="key_output_cage" and .profile.test and .executable!=null) | .executable] | unique |
  if length==1 then .[0] else error("expected one current scheduler test executable") end'
]],capture=true}
               if not built.ok then io.stderr:write(built.err or "") end
               assert(built.ok,"scheduler test failed to build")
               local fixture=(built.out or ""):match("^%s*([^\r\n]+)%s*$")
               assert(fixture and fixture:sub(1,1)=="/","missing current Cargo test executable")
               print("DEV: current Cargo fixture; explicitly pinned compositor runner: "..a.runner)
               assert(oslo.run{"sha256sum",fixture}.ok,"could not identify scheduler fixture")
               local helper=project_root().."/tests/support/keyboard_quiesce.py"
               assert(oslo.run{"sha256sum",helper}.ok,"could not identify scheduler helper")
               for _,provider in ipairs{"lua","hyprlang"} do
                 assert(oslo.run{a.runner,"--hardware-rendering","--key-output","--key-worker",
                   "--key-output-fixture",fixture,"--keyboard-quiesce-check",helper,"--hyprland-config",provider}.ok,"local debug scheduler fixture failed")
               end
             end }

make.recipe{ name = "test-cage-wayland-fix", desc = "explicit test-only patched nested compositor receipts",
             run = function()
               cage_test("cage-test-wayland-fix", { "--modes", "--input", "--power", "--windows" })
               cage_test("cage-test-wayland-fix", { "--layers", "--input", "--power", "--windows" })
             end }

make.recipe{ name = "test-cage-touch", desc = "local GPU Cage two-contact delivery and neutral releases",
             run = function() cage_test("cage-touch-test", {}) end }

make.recipe{ name = "test-touch-runtime", desc = "touch packet bridge and source-bound Lua configuration models",
             run = function() sh.cargo("test", "--locked", "--test", "cage_touch_runtime") end }

make.recipe{ name = "test-cage-touch-dev", desc = "current debug touch controller with explicitly supplied fixture tools",
             params = {{"--cage",desc="test-only Cage touch source executable"},
                       {"--receiver",desc="Wayland receiver executable"},
                       {"--grim",desc="grim executable"},
                       {"--dbus",desc="dbus-run-session executable"}},
             run = function(a)
               for _, name in ipairs{"cage","receiver","grim","dbus"} do
                 assert(type(a[name])=="string" and a[name]:sub(1,1)=="/", "supply an absolute --"..name.." path")
               end
               local built = oslo.run{"bash","-c",[[set -euo pipefail
app=$(cargo build --locked --bin prek --message-format=json | jq -ers '
  [.[] | select(.reason=="compiler-artifact" and .target.name=="prek" and (.profile.test|not) and .executable!=null) | .executable] | unique |
  if length==1 then .[0] else error("expected exactly one current application executable") end')
fixture=$(cargo test --locked --test cage_touch_runtime --no-run --message-format=json | jq -ers '
  [.[] | select(.reason=="compiler-artifact" and .target.name=="cage_touch_runtime" and .profile.test and .executable!=null) | .executable] | unique |
  if length==1 then .[0] else error("expected exactly one current touch test executable") end')
printf '%s\n%s\n' "$app" "$fixture"]],capture=true}
               if not built.ok then io.stderr:write(built.err or "") end
               assert(built.ok,"touch application/controller failed to build")
               local app,fixture=(built.out or ""):match("^%s*([^\r\n]+)\r?\n([^\r\n]+)%s*$")
               assert(app and fixture,"missing current Cargo application/test executable")
               assert(oslo.run{a.dbus,"--config-file="..project_root().."/tests/support/dbus-session.conf","--",
                 "bash","-c",'export PREK_TEST_SESSION_BUS="$DBUS_SESSION_BUS_ADDRESS"; exec "$@"',"--",
                 "python3","tests/cage-touch.py","--hardware-rendering","--cage",a.cage,"--receiver",a.receiver,
                 "--screencopy",a.grim,"--fixture",fixture,"--app",app}.ok,"local debug touch fixture failed")
             end }

make.recipe{ name = "test-cage-lua-wayland-fix", desc = "typed window/power effects in an owned Lua-config compositor",
             run = function()
               cage_test("cage-test-wayland-fix", { "--hyprland-config", "lua", "--modes", "--input", "--power", "--windows" })
               cage_test("cage-test-wayland-fix", { "--hyprland-config", "lua", "--layers", "--input", "--power", "--windows" })
             end }

make.recipe{ name="build-cage-receiver",desc="immutable instrumented receiver without rebuilding Rust",
             run=function()
               assert(oslo.run{"nix","--option","eval-cache","false","--extra-experimental-features","nix-command flakes","--max-jobs","1","--cores","2","build","--no-write-lock-file","--out-link",project_root().."/target/reorder-rendering/receiver",".#cage-test-receiver"}.ok,"receiver build failed")
             end }
make.recipe{ name="test-cage-runtime-dev",desc="current Cargo controller with an explicit immutable owned test runner",
             params={{"--runner",desc="absolute pinned test-only Cage runner"},{"--receiver",desc="absolute current instrumented receiver"}},
             run=function(a)
               for _,name in ipairs{"runner","receiver"} do assert(type(a[name])=="string" and a[name]:sub(1,1)=="/","supply absolute --"..name) end
               local built=oslo.run{"bash","-c",[[set -euo pipefail
app=$(cargo build --locked --bin prek --message-format=json | jq -ers '[.[]|select(.reason=="compiler-artifact" and .target.name=="prek" and (.profile.test|not) and .executable!=null)|.executable]|unique|if length==1 then .[0] else error("expected one application") end')
fixture=$(cargo test --locked --test cage_runtime --no-run --message-format=json | jq -ers '[.[]|select(.reason=="compiler-artifact" and .target.name=="cage_runtime" and .profile.test and .executable!=null)|.executable]|unique|if length==1 then .[0] else error("expected one controller") end')
printf '%s\n%s\n' "$app" "$fixture"]],capture=true}
               if not built.ok then io.stderr:write(built.err or "") end
               assert(built.ok,"current application/controller build failed")
               local app,fixture=(built.out or ""):match("^%s*([^\r\n]+)\r?\n([^\r\n]+)%s*$")
               assert(app and fixture,"missing Cargo artifacts")
               assert(oslo.run{a.runner,"--hyprland-config","lua","--modes","--input","--power","--windows","--receiver",a.receiver,"--prek-fixture",fixture,"--prek-app",app}.ok,"owned current-source Cage fixture failed")
             end }

make.recipe{ name = "test-cage-startup", desc = "unprivileged nested output and library-provenance checks",
             run = function()
               sh.python3("-m", "unittest", "discover", "-s", "tests/support", "-p", "test_cage_startup.py", "-v")
             end }

make.recipe{ name="test-cage-rendering",desc="bounded real capture pixel decoder and marker checks",
             run=function() sh.cargo("test","--locked","--test","cage_runtime","rendering::tests") end }

make.recipe{ name = "test-hyprland", desc = "bound Hyprland queries, window and power adapter models",
             run = function()
               sh.cargo("test", "--locked", "--lib", "hyprland::dispatch::tests")
               sh.cargo("test", "--locked", "--lib", "hyprland::scrolling::tests")
               sh.cargo("test", "--locked", "--lib", "hyprland::layouts::tests")
               sh.cargo("test", "--locked", "--lib", "hyprland::reorder::tests")
               sh.cargo("test", "--locked", "--lib", "routing::tests::native_claims::")
               sh.cargo("test", "--locked", "--lib", "service::tests::native_reorder::")
               sh.cargo("test", "--locked", "--lib", "route_worker::tests::recognizers::native_reorder::")
               sh.cargo("test", "--locked", "--test", "hyprland")
             end }

make.recipe{ name = "check", desc = "type-check every target",
             run = function() sh.cargo("check", "--locked", "--all-targets") end }

make.recipe{ name="update-lock",desc="resolve workspace lockfile while retaining locked external packages",
             run=function() sh.cargo("update","--workspace") end }

make.recipe{ name = "api-json", desc = "refresh the shipped Lua API manifest",
             run = function()
               sh.bash("-c", [[set -euo pipefail
tmp=$(mktemp docs/.lua-api.XXXXXX)
trap 'rm -f "$tmp"' EXIT
cargo run --locked -- api --json | jq . > "$tmp"
mv "$tmp" docs/lua-api.json]])
             end }

make.recipe{ name = "test-context-layouts", desc = "bound native context transfer, Lua copies and sampling",
             run = function()
               sh.cargo("test", "--locked", "--lib", "context_transfer::tests::native_layout")
               sh.cargo("test", "--locked", "--lib", "lua::context::tests::native_layout")
               sh.cargo("test", "--locked", "--lib", "service::tests::native_layout")
               sh.cargo("test", "--locked", "--lib", "worker::tests::native_layout")
               sh.cargo("test", "--locked", "--test", "hyprland", "layouts::")
             end }

make.recipe{ name = "test-subscription-transfer", desc = "bounded private worker updates and revocation barriers",
             run = function()
               sh.cargo("test", "--locked", "--lib", "subscription_transfer::tests::")
               sh.cargo("test", "--locked", "--lib", "private_subscription_")
               sh.cargo("test", "--locked", "--lib", "service::tests::native_layout")
             end }

make.recipe{
  name = "test-distribution",
  desc = "unprivileged Nix bundle validation and refusal tests",
  run = function()
    sh.python3("-m", "unittest", "discover", "-s", "tests/support", "-p", "test_nix_dist.py", "-v")
    sh.python3("-m", "unittest", "discover", "-s", "tests/support", "-p", "test_build_tools.py", "-v")
    sh.bash("-n", "tests/run-rust-gate.sh")
    sh.shellcheck("tests/run-rust-gate.sh")
    sh.bash("-n", "tests/run-dist-offline.sh")
    sh.shellcheck("tests/run-dist-offline.sh")
    for _, path in ipairs{"tests/run-nix-gate.sh","tests/run-aarch64-build.sh"} do
      sh.bash("-n",path)
      sh.shellcheck(path)
    end
    sh.actionlint()
  end,
}

make.recipe{
  name = "verify-msrv",
  desc = "run the native Rust gate using the declared 1.88 minimum toolchain",
  run = function()
    assert(oslo.run{ "env", "CARGO_TARGET_DIR=target/msrv", "nix", "--option", "eval-cache", "false",
      "--extra-experimental-features", "nix-command flakes", "--max-jobs", "1", "--cores", "2",
      "develop", "--no-write-lock-file", ".#msrv", "--command", "bash", "tests/run-rust-gate.sh" }.ok,
      "minimum Rust version gate failed")
  end,
}

make.recipe{
  name = "dist-nix",
  desc = "build and export the native application as a Nix closure bundle",
  run = function()
    local root = project_root()
    sh.mkdir("-p", root .. "/target/distribution")
    assert(oslo.run{ "nix", "--option", "eval-cache", "false", "--extra-experimental-features",
      "nix-command flakes", "--max-jobs", "1", "--cores", "2", "build", "--no-write-lock-file",
      "--out-link", root .. "/target/distribution/package", ".#default" }.ok, "native package failed")
    local system = oslo.run{ "nix", "--extra-experimental-features", "nix-command", "eval", "--impure",
      "--raw", "--expr", "builtins.currentSystem", capture = true }
    local revision = oslo.run{ "git", "rev-parse", "HEAD", capture = true }
    assert(system.ok and revision.ok, "distribution metadata unavailable")
    local arch = (system.out or ""):match("^%s*(.-)%s*$")
    local args = { "python3", "scripts/nix-dist.py", "export", "--package", root .. "/target/distribution/package",
      "--replace", "--version", VERSION, "--system", arch, "--revision", (revision.out or ""):match("^%s*(.-)%s*$"),
      "--output", root .. "/target/distribution/prek-" .. VERSION .. "-" .. arch .. "-nix.tar.gz" }
    if not oslo.run{ "git", "diff", "--quiet", "HEAD" }.ok then args[#args + 1] = "--dirty" end
    assert(oslo.run(args).ok, "native closure export failed")
  end,
}

make.recipe{ name = "check-all", desc = "type-check every target, every feature",
             run = function() sh.cargo("check", "--locked", "--all-targets", "--all-features") end }

make.recipe{
  name = "test-dist-offline",
  desc = "import the bundle into an empty isolated store with builders disabled",
  run = function()
    assert(oslo.run{ "bash", "tests/run-dist-offline.sh" }.ok, "isolated closure import failed")
  end,
}

make.recipe{
  name = "check-dist-nix",
  desc = "build every exported native flake check, including installed helper gates",
  run = function()
    assert(oslo.run{"bash","tests/run-nix-gate.sh",project_root().."/target/distribution/check"}.ok,"native flake checks failed")
  end,
}

make.recipe{ name = "clippy", desc = "clippy, with warnings denied",
             run = function()
               sh.cargo("clippy", "--locked", "--all-targets", "--all-features", "--", "-Dwarnings")
             end }

make.recipe{
  name = "rustdoc",
  desc = "build the docs, with warnings denied",
  run = function()
    local built = oslo.run{ "env", "RUSTDOCFLAGS=-Dwarnings",
                            "cargo", "doc", "--locked", "--all-features", "--no-deps" }
    assert(built.ok, "rustdoc failed")
  end,
}

make.recipe{ name = "fmt", desc = "format the workspace",
             run = function() sh.cargo("fmt", "--all") end }

make.recipe{ name = "fmt-check", desc = "fail if anything is unformatted",
             run = function() sh.cargo("fmt", "--all", "--", "--check") end }

make.recipe{ name = "clean", desc = "remove every build output",
             run = function() sh.cargo("clean") end }

make.recipe{ name = "compile", desc = "clean, then build", deps = { "clean", "build" } }
make.alias("c", "compile")

make.recipe{
  name = "verify",
  desc = "the whole local gate",
  deps = { "fmt-check", "check-all", "test-all", "test-lua-lifetimes", "test-lua-tasks", "test-key-provenance", "test-key-admission", "test-key-participants", "test-key-journal", "test-software-worker", "test-routing-helper", "test-composition-helper", "test-chain-helper", "test-composed-worker-helper", "test-root-session", "test-root-routing", "test-native-managed-model", "test-cage-startup", "test-distribution", "clippy", "rustdoc" },
}
make.alias("v", "verify")
