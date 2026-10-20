-- End-to-end check that lint warnings (e.g. unused variable) reach the
-- editor as nvim diagnostics. This is the spec the "no squiggles in nvim"
-- regression would have failed against — the LSP server emits the warning
-- correctly, but without this spec we only catch CLI/unit-test regressions
-- and miss the actual editor surface.
--
-- Requires $ROCK_LSP_BIN. Skips when unset (mirrors real_lsp_smoke_spec.lua)
-- so run.sh works standalone.

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
stick:write('[package]\nname = "diag_squiggle"\nversion = "0.1.0"\n')
stick:close()
local rock_path = root .. '/main.rock'
local f = assert(io.open(rock_path, 'w'))
f:write('@artifact_definition(catalog="widgets", key_param="key")\n'
  .. 'deco init widget(key: string) {}\n'
  .. 'fn main() {\n    let unused_thing = 42\n    print("hi")\n}\n')
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

-- Wait for at least one diagnostic to be published. The LSP runs the
-- full lint pipeline on first compile, so the warning should land
-- alongside the initial publishDiagnostics. Generous timeout: cold
-- compiles can take a few seconds.
local diags = nil
local ok = vim.wait(8000, function()
  diags = vim.diagnostic.get(buf)
  return diags and #diags > 0
end, 100)

if not ok then
  io.stderr:write('--- diagnostics state at timeout ---\n')
  io.stderr:write(vim.inspect(diags or {}) .. '\n')
  fail('no diagnostics arrived for unused-var file within 8s')
end

-- Find the unused-var WARN. Other diagnostics may coexist (HINT-level
-- linter advice, etc.) — we just need to prove at least one squiggle-
-- worthy WARNING reached the buffer.
local warn_severity = vim.diagnostic.severity.WARN
local saw_warn = false
local saw_unused = false
for _, d in ipairs(diags) do
  if d.severity == warn_severity then saw_warn = true end
  if d.message and d.message:find('unused variable `key`', 1, true) then
    fail(('decorator metadata parameter was reported unused: %s'):format(vim.inspect(d)))
  end
  if d.message and d.message:lower():find('unused', 1, true) then
    saw_unused = true
    -- Range must be non-empty for nvim to actually draw the squiggle.
    if d.col == d.end_col and d.lnum == d.end_lnum then
      fail(('unused-var diagnostic has empty range; nvim will draw nothing: %s')
        :format(vim.inspect(d)))
    end
  end
end

if not saw_warn then
  fail(('no WARN-severity diagnostic in buffer; got: %s'):format(vim.inspect(diags)))
end
if not saw_unused then
  fail(('no diagnostic mentioning "unused" reached the buffer; got: %s')
    :format(vim.inspect(diags)))
end

print(('OK: %d diagnostics published, WARN+unused both present'):format(#diags))
os.exit(0)
