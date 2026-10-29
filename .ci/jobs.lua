-- Run the dev verbs on an Arch Linux container, one job per verb
--
--   ci run --local --job lint -f
--   ci run --local --job test -f
--   ci run --local --job build -f
--
-- The regress job builds the shipped tree and a baseline tree, serves both
-- under a frozen clock, and fails when the parity harness's regress.mjs finds
-- any pixel difference between them. The runner has no git history and no way
-- to take arguments, so the baseline is the source tree in .ci-baseline/.
-- `dev regress [REV]` exports REV (default origin/main) there, runs the job
-- and removes the directory again.
--
-- The job keeps regress-report.tgz (report.json plus candidate, baseline and
-- diff screenshots); fetch it with `ci artifacts last`.

local TOOLCHAIN = "1.99.1"

local RUNNER = { target = "archlinux-rust" }

-- Prefix cargo's directory on every command, since each ci.sh gets a fresh shell
local function run(command)
  return ci.sh("export PATH=$HOME/.cargo/bin:$PATH RUSTUP_TOOLCHAIN=stable; " .. command)
end

local function check_rust()
  ci.group("check rust " .. TOOLCHAIN, function()
    run("rustc --version | grep -q '^rustc " .. TOOLCHAIN .. " ' && cargo nextest --version >/dev/null && cargo +nightly fmt --version >/dev/null")
  end)
end

-- Install node for the lint's JavaScript syntax check, which the runner image lacks
local function install_node()
  ci.group("install node", function()
    run("command -v node >/dev/null || sudo pacman -Sy --noconfirm --needed nodejs")
  end)
end

local BROWSER_RUNNER = { target = "archlinux-browser" }

local REGRESS = [=[
set -euo pipefail
export PATH=$HOME/.cargo/bin:$PATH RUSTUP_TOOLCHAIN=stable
export PW_NODE_MODULES=/opt/playwright/node_modules
export PLAYWRIGHT_BROWSERS_PATH=/opt/playwright/browsers
export FAKETIME_LIB=/usr/lib/faketime/libfaketime.so.1
root=$PWD
out=$root/regress-out
rm -rf "$out" && mkdir -p "$out"

[ -d .ci-baseline ] || { echo "no .ci-baseline/ in the shipped tree" >&2; exit 1; }
CARGO_TARGET_DIR=$root/target .dev/build.sh
CARGO_TARGET_DIR=$root/target-baseline .dev/build.sh --manifest-path .ci-baseline/Cargo.toml

trap 'for s in "$out"/cand/state.json "$out"/base/state.json; do
  [ -f "$s" ] && kill "$(jq -r .pid "$s")" 2>/dev/null || true; done' EXIT
.dev/parity/start-server.sh "$root/target/release/pomb" 3421 "$out/cand" >/dev/null
.dev/parity/start-server.sh "$root/target-baseline/release/pomb" 3420 "$out/base" >/dev/null

rc=0
node .dev/parity/regress.mjs --candidate "$out/cand/state.json" \
  --baseline "$out/base/state.json" --out "$out/report" || rc=$?
echo "$rc" >"$out/rc"
tar czf "$root/regress-report.tgz" -C "$out" report
]=]

return {
  jobs = {
    lint = {
      matrix = { RUNNER },
      run = function()
        check_rust()
        install_node()
        run(".dev/lint.sh")
      end,
    },
    test = {
      matrix = { RUNNER },
      run = function()
        check_rust()
        run(".dev/test.sh")
      end,
    },
    build = {
      matrix = { RUNNER },
      run = function()
        check_rust()
        run(".dev/build.sh")
      end,
    },
    regress = {
      matrix = { BROWSER_RUNNER },
      run = function()
        check_rust()
        ci.sh(REGRESS)
        ci.artifact("regress-report.tgz")
        ci.sh("exit $(cat regress-out/rc)")
      end,
    },
  },
}
