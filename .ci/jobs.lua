-- Run the dev verbs on a linux container, one job per verb
--
--   ci run --local --job lint -f
--   ci run --local --job test -f
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

-- Install the project's pinned channel, which the runner image usually bakes in
local TOOLCHAIN = "1.95.0"

local RUNNER = { os = "linux-rust" }

-- Prefix cargo's directory on every command, since each ci.sh gets a fresh shell
local function run(command)
  return ci.sh("export PATH=$HOME/.cargo/bin:$PATH; " .. command)
end

local function install_rust()
  ci.group("install rust " .. TOOLCHAIN, function()
    run("rustup toolchain install " .. TOOLCHAIN .. " --component clippy,rustfmt")
    run("rustup toolchain install nightly --profile minimal --component rustfmt")
    run("command -v cargo-nextest >/dev/null || { mkdir -p $HOME/.cargo/bin && curl -LsSf https://get.nexte.st/latest/linux | tar zxf - -C $HOME/.cargo/bin; }")
  end)
end

-- Install node for the lint's JavaScript syntax check, which the runner image lacks
local function install_node()
  ci.group("install node", function()
    run("command -v node >/dev/null || sudo pacman -Sy --noconfirm --needed nodejs")
  end)
end

local BROWSER_RUNNER = { os = "linux-browser" }

local REGRESS = [=[
set -euo pipefail
export PATH=$HOME/.cargo/bin:$PATH
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
        install_rust()
        install_node()
        run(".dev/lint.sh")
      end,
    },
    test = {
      matrix = { RUNNER },
      run = function()
        install_rust()
        run(".dev/test.sh")
      end,
    },
    regress = {
      matrix = { BROWSER_RUNNER },
      run = function()
        install_rust()
        ci.sh(REGRESS)
        ci.artifact("regress-report.tgz")
        ci.sh("exit $(cat regress-out/rc)")
      end,
    },
  },
}
