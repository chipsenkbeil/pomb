-- Check the project on a linux container with .dev/lint.sh, then .dev/test.sh
--
--   ci run --local

-- Install the project's pinned channel, which the runner image usually bakes in
local TOOLCHAIN = "1.95.0"

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

return {
  jobs = {
    check = {
      matrix = { { os = "linux-ct", image = "rust-runner-ct" } },
      run = function()
        install_rust()
        run(".dev/lint.sh")
        run(".dev/test.sh")
      end,
    },
  },
}
