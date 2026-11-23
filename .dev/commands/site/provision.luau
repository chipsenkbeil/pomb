local devos = require("@dev/os")
local fs = require("@dev/fs")
local site = require("@lib/pomb/site")
local dev_command = require("@dev/command")

local FILES = { "apt", "sshd", "systemd", "env.in" }
local STANDALONE = { "pomb-update", "setup.sh" }
local UNPACK = "rm -rf /root/pomb-box && mkdir -p /root/pomb-box && tar -C /root/pomb-box -xf -"
local ROLLBACK = "rm -f /etc/ssh/sshd_config.d/10-pomb.conf && sshd -t && systemctl reload ssh"

local function temp(argv)
    local out, code = devos.capture(argv)
    if code ~= 0 then
        dev_command.die("site: cannot make a temporary path")
    end
    return (out:gsub("\n$", ""))
end

local function stage_files(ctx, stage, admin_ip)
    local copy = { "cp", "-R" }
    for _, name in ipairs(FILES) do
        copy[#copy + 1] = name
    end
    copy[#copy + 1] = stage .. "/"
    local code = devos.run(copy, { cwd = site.dir(ctx) }) or 1
    if code ~= 0 then
        return code
    end
    local standalone = { "cp" }
    for _, name in ipairs(STANDALONE) do
        standalone[#standalone + 1] = name
    end
    standalone[#standalone + 1] = stage .. "/"
    code = devos.run(standalone, { cwd = ctx.root .. "/.dev/standalone" }) or 1
    if code ~= 0 then
        return code
    end
    local path = stage .. "/sshd/10-pomb.conf"
    local text, why = fs.read(path)
    if not text then
        return 1, why
    end
    local conf = text:gsub("\n+$", "")
    local filled = conf:gsub("@ADMIN_IP@", function()
        return admin_ip
    end)
    local ok, write_why = fs.write(path, filled .. "\n")
    if not ok then
        return 1, write_why
    end
    return 0
end

local function fresh_login(host, sock)
    local fresh = site.append({ "ssh" }, site.ssh_opts())
    site.append(fresh, { "-o", "ControlPath=none", host, "true" })
    if (devos.run(fresh) or 1) == 0 then
        dev_command.say("provision: fresh key login ok")
        return 0
    end
    dev_command.note("provision: fresh login failed, rolling back sshd hardening")
    local code = devos.run({ "ssh", "-o", "ControlPath=" .. sock, host, ROLLBACK }) or 1
    if code ~= 0 then
        return code
    end
    return 1
end

local function provision(host, stage, sock)
    -- Open the ssh control master
    local master = site.append({ "ssh" }, site.ssh_opts())
    site.append(master, { "-o", "ControlMaster=yes", "-o", "ControlPath=" .. sock, "-fN", host })
    local code = devos.run(master) or 1
    if code ~= 0 then
        return code
    end

    -- Copy the staged files to the box
    code = devos.run({
        "bash",
        "-o",
        "pipefail",
        "-c",
        'tar -C "$1" -cf - . | ssh -o ControlPath="$2" "$3" "$4"',
        "bash",
        stage,
        sock,
        host,
        UNPACK,
    }, { env = { COPYFILE_DISABLE = "1" } }) or 1
    if code ~= 0 then
        return code
    end

    -- Run the setup script and check a fresh login
    code = devos.run({ "ssh", "-o", "ControlPath=" .. sock, host, "bash /root/pomb-box/setup.sh" })
        or 1
    if code ~= 0 then
        return code
    end
    return fresh_login(host, sock)
end

return dev_command({
    summary = "Provision and harden the box over SSH; idempotent.",
    runs_local = true,
    remote = false,
    positional = true,
    run = function(_opts, _args, ctx)
        -- Read the host and the admin address
        local host = site.host()
        local admin_ip = site.pass_line(site.ADMIN_IP_ENTRY)
        if not admin_ip:match("^[%x.:/]+$") then
            dev_command.die(
                "site: the password store entry " .. site.ADMIN_IP_ENTRY .. " is not an address"
            )
        end

        -- Make the stage directory and socket paths
        local stage = temp({ "mktemp", "-d", "/tmp/pomb-site.XXXXXX" })
        local sock = temp({ "mktemp", "-u", "/tmp/pomb-ssh.XXXXXX" })

        -- Stage the files and provision the box
        local ok, code, message = pcall(function()
            local staged, why = stage_files(ctx, stage, admin_ip)
            if staged ~= 0 then
                return staged, why
            end
            return provision(host, stage, sock)
        end)

        -- Close the master, remove the stage and pass on the result
        devos.run({ "ssh", "-o", "ControlPath=" .. sock, "-O", "exit", host }, { stderr = "null" })
        fs.remove(stage, { recursive = true })
        if not ok then
            error(code, 0)
        end
        if message then
            dev_command.die(message)
        end
        return code
    end,
})
