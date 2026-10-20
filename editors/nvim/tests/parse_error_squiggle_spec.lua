-- End-to-end: type a syntax error in a .rock file, expect an LSP
-- diagnostic in the buffer.
--
-- Pre-fix (silent-errors audit, 2026-05-03; deferred item 2), parse
-- errors in LSP recovery mode were dropped at five distinct sites
-- (parser/mod.rs:215, 1732, 1805 + parser_util.rs:117, 234). Typing
-- `fn foo(` produced no squiggle, with the only trace being a line
-- in /tmp/lsp_debug.log nobody saw. This spec is the editor-surface
-- regression fence — the rock-lsp/tests Rust integration tests pin
-- the underlying plumbing (storage.warnings + collect helper); this
-- spec proves the diagnostic actually reaches nvim's diagnostic
-- store (not just the publishDiagnostics notification we send).
--
-- Mirrors `diagnostics_squiggle_spec.lua` for the unused-var path.

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

local bin = vim.env.ROCK_LSP_BIN
if not bin or bin == '' then
  print('SKIP: $ROCK_LSP_BIN not set')
  os.exit(0)
end
if not (vim.uv or vim.loop).fs_stat(bin) then
  fail('$ROCK_LSP_BIN points at non-existent file: ' .. bin)
end

vim.notify = function(_, _, _) end
vim.notify_once = vim.notify

vim.filetype.add({ extension = { rock = 'rock' } })

local root = vim.fn.tempname()
vim.fn.mkdir(root, 'p')
local stick = assert(io.open(root .. '/stick.toml', 'w'))
stick:write('[package]\nname = "parse_err_squiggle"\nversion = "0.1.0"\n')
stick:close()
local rock_path = root .. '/main.rock'
local f = assert(io.open(rock_path, 'w'))
-- Plain syntax error: open paren never closed, no body.
f:write('fn foo(\n')
f:close()

local client = require('rock.client')
local cfg = client.make_config({ binary = bin })
vim.lsp.config('rock', cfg)
client.setup_autocmds()
vim.lsp.enable('rock')

vim.cmd('edit ' .. vim.fn.fnameescape(rock_path))
local buf = vim.api.nvim_get_current_buf()

local attached_id = nil
vim.api.nvim_create_autocmd('LspAttach', {
  callback = function(args)
    local c = vim.lsp.get_client_by_id(args.data.client_id)
    if c and c.name == 'rock' then attached_id = args.data.client_id end
  end,
})

if not vim.wait(5000, function() return attached_id ~= nil end, 50) then
  fail('LspAttach did not fire within 5s')
end

local diags = nil
local ok = vim.wait(8000, function()
  diags = vim.diagnostic.get(buf)
  return diags and #diags > 0
end, 100)

if not ok then
  io.stderr:write('--- diagnostics state at timeout ---\n')
  io.stderr:write(vim.inspect(diags or {}) .. '\n')
  fail('no diagnostics arrived for syntax-error file within 8s')
end

-- Range must be non-empty for nvim to actually draw the squiggle.
for _, d in ipairs(diags) do
  if d.col == d.end_col and d.lnum == d.end_lnum then
    fail(('parse-error diagnostic has empty range; nvim will draw nothing: %s')
      :format(vim.inspect(d)))
  end
end

print(('OK: %d diagnostics published for syntax-error file'):format(#diags))
os.exit(0)
