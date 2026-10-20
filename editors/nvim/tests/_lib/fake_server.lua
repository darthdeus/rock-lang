-- Fake language-server binaries for the spawn-and-exit specs.
--
-- The specs used to point `binary` at /usr/bin/true and /usr/bin/false,
-- which assumes a FHS layout: NixOS has neither, and resolving them
-- through PATH hands back the canonicalized store path, where the
-- multi-call coreutils binary dispatches on argv[0] and rejects the
-- `lsp serve` arguments. A tiny script that ignores its arguments and
-- exits with the code the spec wants keeps the specs about the spawn,
-- crash and restart machinery instead of about the host.

local uv = vim.uv or vim.loop

local M = {}

-- Write an executable `name` that exits with `code`, returning its path.
function M.write(dir, name, code)
  vim.fn.mkdir(dir, 'p')
  local path = dir .. '/' .. name
  local fd = assert(uv.fs_open(path, 'w', 493))
  uv.fs_write(fd, '#!/bin/sh\nexit ' .. tostring(code) .. '\n', 0)
  uv.fs_close(fd)
  return path
end

return M
