local devos = require("@dev/os")
local fs = require("@dev/fs")

---@class pomb.testkit
local M = {}

---Absolute path of the checkout that holds this module.
---@type string
M.root = require("@dev/project").root()

---Write an executable `#!/bin/sh` script.
---@param path string absolute path of the script
---@param body string script text after the shebang line
function M.stub(path, body)
    assert(fs.write(path, "#!/bin/sh\n" .. body .. "\n"))
    assert(devos.run({ "chmod", "755", path }) == 0)
end

return M
