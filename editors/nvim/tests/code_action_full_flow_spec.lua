-- Layer-4 repro that drives the *real* code-action request path:
-- send `textDocument/codeAction`, take the first action's WorkspaceEdit,
-- apply it via `vim.lsp.util.apply_workspace_edit`, then check the
-- diagnostic clears.
--
-- Distinct from code_action_clears_diagnostic_spec.lua, which fabricates
-- the TextEdit. This spec catches issues that only surface when the
-- server-supplied WorkspaceEdit shape (changes vs documentChanges,
-- ranges, etc.) round-trips through nvim's apply_workspace_edit.

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
stick:write('[package]\nname = "code_action_full"\nversion = "0.1.0"\n')
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

local initial_diags
local ok = vim.wait(8000, function()
  initial_diags = vim.diagnostic.get(buf)
  return has_ufcs_diag(initial_diags)
end, 100)
if not ok then
  fail(('expected vec_remove_at UFCS advice on first compile, got: %s'):format(vim.inspect(initial_diags)))
end

-- Place cursor inside the vec_remove_at call so codeAction returns the
-- UFCS rewrite action.
local lines = vim.api.nvim_buf_get_lines(buf, 0, -1, false)
local target_line, target_col
for i, line in ipairs(lines) do
  local s, _ = line:find('vec_remove_at%(', 1)
  if s then target_line = i - 1; target_col = s + 2; break end
end
assert(target_line, 'could not locate call in buffer')
vim.api.nvim_win_set_cursor(0, { target_line + 1, target_col })

local client_obj = vim.lsp.get_client_by_id(attached_id)
local enc = client_obj.offset_encoding or 'utf-16'

local uri = vim.uri_from_bufnr(buf)
local params = {
  textDocument = { uri = uri },
  range = {
    start = { line = target_line, character = target_col },
    ['end'] = { line = target_line, character = target_col },
  },
  context = { diagnostics = {}, triggerKind = 2 },
}

local results, err = vim.lsp.buf_request_sync(buf, 'textDocument/codeAction', params, 30000)
if err then fail('codeAction request error: ' .. vim.inspect(err)) end
if not results then fail('no codeAction results') end
local result = nil
for _, r in pairs(results) do
  if r.result then result = { result = r.result }; break end
end
if err then fail('codeAction request error: ' .. vim.inspect(err)) end
if not (result and result.result) then fail('no codeAction result: ' .. vim.inspect(result)) end

-- Find the UFCS rewrite action.
local action
for _, a in ipairs(result.result) do
  if a.title and a.title:find('UFCS', 1, true) then action = a; break end
end
if not action then
  fail(('no UFCS rewrite action in codeActions: %s'):format(vim.inspect(result.result)))
end
if not (action.edit) then
  fail(('UFCS action has no edit: %s'):format(vim.inspect(action)))
end

-- Apply the WorkspaceEdit the server returned. apply_workspace_edit fires
-- nvim_buf_set_text which triggers TextChanged → didChange.
vim.lsp.util.apply_workspace_edit(action.edit, enc)

-- Verify the buffer reflects the rewrite (so we know the edit applied).
local after_lines = vim.api.nvim_buf_get_lines(buf, target_line, target_line + 1, false)
if not (after_lines[1] and after_lines[1]:find('remove_at', 1, true) and not after_lines[1]:find('vec_remove_at', 1, true)) then
  fail(('apply_workspace_edit did not rewrite the call; line is: %q'):format(after_lines[1] or '<empty>'))
end

-- Wait for the diagnostic to clear.
local final_diags
ok = vim.wait(8000, function()
  final_diags = vim.diagnostic.get(buf)
  return not has_ufcs_diag(final_diags)
end, 100)

if not ok then
  fail(('vec_remove_at UFCS diagnostic did not clear after apply_workspace_edit; got: %s')
    :format(vim.inspect(final_diags)))
end

print('OK: diagnostic cleared after server-driven code action')
os.exit(0)
