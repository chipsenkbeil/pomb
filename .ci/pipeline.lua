-- Run the dev verbs on a linux container, one job per verb
--
--   ci run --local --job lint -f
--   ci run --local --job test -f

-- Install the project's pinned channel, which the runner image usually bakes in
local TOOLCHAIN = "1.95.0"

local RUNNER = { os = "linux-ct", image = "rust-runner-ct" }

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
    run("command -v node >/dev/null || pacman -Sy --noconfirm --needed nodejs")
  end)
end

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
  },
}
