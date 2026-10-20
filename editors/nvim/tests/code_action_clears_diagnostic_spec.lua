-- Layer-4 repro: apply a code-action edit that fixes a lint and check
-- the diagnostic clears in nvim. Editor-surface part of the four-layer
-- LSP-bug stack. The CLI and the in-process Rust tests both confirm
-- the LSP publishes correctly; this spec is the only thing that
-- catches client/server drift (the canonical case CLAUDE.md warns
-- about).
--
-- Specifically: file with `vec_remove_at(v, 0)` triggers the
-- StdlibUfcsLintPass advice. Apply a TextEdit that rewrites it to UFCS
-- form (mimicking what `vim.lsp.util.apply_workspace_edit` does for a
-- code action). After the edit, the advice should be gone.

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
stick:write('[package]\nname = "code_action_clear"\nversion = "0.1.0"\n')
stick:close()
local rock_path = root .. '/main.rock'
local f = assert(io.open(rock_path, 'w'))
f:write('fn main() {\n    let v = [1, 2, 3]\n    let _ = vec_remove_at(v, 0)\n}\n')
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

local function has_ufcs_diag(diags)
  for _, d in ipairs(diags or {}) do
    if d.message and d.message:find('vec_remove_at', 1, true) and d.message:find('UFCS', 1, true) then
      return true
    end
  end
  return false
end

-- Wait for initial diagnostic to appear.
local initial_diags
local ok = vim.wait(8000, function()
  initial_diags = vim.diagnostic.get(buf)
  return has_ufcs_diag(initial_diags)
end, 100)
if not ok then
  fail(('expected vec_remove_at UFCS advice on first compile, got: %s'):format(vim.inspect(initial_diags)))
end

-- Apply a TextEdit that rewrites `vec_remove_at(v, 0)` → `v.remove_at(0)`.
-- Mimics what apply_workspace_edit does for a code action.
local lines = vim.api.nvim_buf_get_lines(buf, 0, -1, false)
local line_idx, col_start = nil, nil
for i, line in ipairs(lines) do
  local s, _ = line:find('vec_remove_at%(', 1)
  if s then line_idx = i - 1; col_start = s - 1; break end
end
assert(line_idx, 'could not locate vec_remove_at in buffer')

-- Find end of the call (matching `)`).
local target_line = lines[line_idx + 1]
local depth = 0
local close = nil
for i = col_start + 1, #target_line do
  local ch = target_line:sub(i, i)
  if ch == '(' then depth = depth + 1
  elseif ch == ')' then depth = depth - 1; if depth == 0 then close = i; break end
  end
end
assert(close, 'could not locate closing paren')

local text_edits = {{
  range = {
    start = { line = line_idx, character = col_start },
    ['end'] = { line = line_idx, character = close },
  },
  newText = 'v.remove_at(0)',
}}

local client_obj = vim.lsp.get_client_by_id(attached_id)
local enc = client_obj and client_obj.offset_encoding or 'utf-16'
vim.lsp.util.apply_text_edits(text_edits, buf, enc)

-- Confirm the buffer reflects the new text.
local after_lines = vim.api.nvim_buf_get_lines(buf, line_idx, line_idx + 1, false)
if not (after_lines[1] and after_lines[1]:find('v.remove_at(0)', 1, true)) then
  fail(('apply_text_edits did not modify buffer; line is: %q'):format(after_lines[1] or '<empty>'))
end

-- Now wait for the diagnostic to clear.
local final_diags
ok = vim.wait(8000, function()
  final_diags = vim.diagnostic.get(buf)
  return not has_ufcs_diag(final_diags)
end, 100)

if not ok then
  fail(('vec_remove_at UFCS diagnostic did not clear after edit; got: %s'):format(vim.inspect(final_diags)))
end

print('OK: diagnostic cleared after apply_text_edits')
os.exit(0)
