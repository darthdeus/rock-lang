-- Layer-4 repro: apply a code-action edit that fixes a diagnostic and check
-- the diagnostic clears in nvim. Editor-surface part of the four-layer
-- LSP-bug stack: the CLI and the in-process Rust tests both confirm the LSP
-- publishes correctly; this spec is the only thing that catches client/server
-- drift (the canonical case CLAUDE.md warns about).
--
-- Specifically: a `match` that misses enum variants triggers the
-- "Non-exhaustive match" error and the server offers a "Fill in N missing
-- match arms" action. This spec applies the *fabricated* equivalent of that
-- edit (the sibling spec drives the server's own WorkspaceEdit), then
-- requires the diagnostic to clear. The edit shape is what
-- `vim.lsp.util.apply_workspace_edit` produces for the action.

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
f:write([[
enum Color { Red, Green, Blue }

fn f(c: Color) -> int {
    match c {
        Color::Red => {}
    }
    return 0
}
]])
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

local function has_nonexhaustive_diag(diags)
  for _, d in ipairs(diags or {}) do
    if d.message and d.message:find('Non-exhaustive match', 1, true) then
      return true
    end
  end
  return false
end

-- Wait for the non-exhaustive diagnostic to appear.
local initial_diags
local ok = vim.wait(8000, function()
  initial_diags = vim.diagnostic.get(buf)
  return has_nonexhaustive_diag(initial_diags)
end, 100)
if not ok then
  fail(('expected a non-exhaustive match diagnostic on first compile, got: %s')
    :format(vim.inspect(initial_diags)))
end

-- Apply the arms the server's fill action would insert: append them after the
-- one existing arm. Mimics what apply_workspace_edit does for that action.
local lines = vim.api.nvim_buf_get_lines(buf, 0, -1, false)
local arm_line, arm_len = nil, nil
for i, line in ipairs(lines) do
  if line:find('Color::Red', 1, true) then
    arm_line, arm_len = i - 1, #line
    break
  end
end
if not arm_line then fail('could not locate the existing match arm') end

local text_edits = { {
  range = {
    start = { line = arm_line, character = arm_len },
    ['end'] = { line = arm_line, character = arm_len },
  },
  newText = '\n        Color::Green => {}\n        Color::Blue => {}',
} }

local client_obj = vim.lsp.get_client_by_id(attached_id)
local enc = client_obj and client_obj.offset_encoding or 'utf-16'
vim.lsp.util.apply_text_edits(text_edits, buf, enc)

-- Confirm the buffer reflects the new arms.
local after_text = table.concat(vim.api.nvim_buf_get_lines(buf, 0, -1, false), '\n')
if not (after_text:find('Color::Green', 1, true) and after_text:find('Color::Blue', 1, true)) then
  fail(('apply_text_edits did not add the arms; buffer is: %q'):format(after_text))
end

-- Now wait for the diagnostic to clear.
local final_diags
ok = vim.wait(8000, function()
  final_diags = vim.diagnostic.get(buf)
  return not has_nonexhaustive_diag(final_diags)
end, 100)

if not ok then
  fail(('non-exhaustive diagnostic did not clear after the edit; got: %s')
    :format(vim.inspect(final_diags)))
end

print('OK: diagnostic cleared after apply_text_edits')
os.exit(0)
