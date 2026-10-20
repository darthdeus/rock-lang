-- End-to-end check that the two-label `./` hint reaches nvim as TWO squiggles:
-- one on the `/` (the cause) and one on the use site. The LSP used to drop
-- every label but the first, so only the `/` line got a squiggle.
--
-- Requires $ROCK_LSP_BIN. Skips when unset (mirrors diagnostics_squiggle_spec).

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
stick:write('[package]\nname = "float_div_hint"\nversion = "0.1.0"\n')
stick:close()
local rock_path = root .. '/main.rock'
local f = assert(io.open(rock_path, 'w'))
-- `h` is float (via `/`); using it in an int hole one line later is the
-- mismatch. Hint points at the `/` (line 1) and the use (line 2), 0-based.
f:write('fn main() {\n  let h = 8 / 3\n  let x: int = 2 * h\n}\n')
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
  return diags and #diags >= 2
end, 100)

if not ok then
  io.stderr:write('--- diagnostics state at timeout ---\n')
  io.stderr:write(vim.inspect(diags or {}) .. '\n')
  fail('expected >= 2 diagnostics (both lines) within 8s')
end

-- A squiggle on the `/` line (lnum 1) AND the use-site line (lnum 2).
local on_div, on_use = false, false
for _, d in ipairs(diags) do
  if d.lnum == 1 then on_div = true end
  if d.lnum == 2 then on_use = true end
  -- Range must be non-empty or nvim draws nothing.
  if d.col == d.end_col and d.lnum == d.end_lnum then
    fail(('diagnostic has empty range: %s'):format(vim.inspect(d)))
  end
end

if not (on_div and on_use) then
  fail(('expected squiggles on BOTH the `/` line (1) and the use line (2); got: %s')
    :format(vim.inspect(vim.tbl_map(function(d) return { lnum = d.lnum, msg = d.message } end, diags))))
end

print(('OK: %d diagnostics, squiggles on both the `/` and the use site'):format(#diags))
os.exit(0)
