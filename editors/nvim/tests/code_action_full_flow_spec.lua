-- Layer-4 repro that drives the *real* code-action request path: send
-- `textDocument/codeAction`, take the action's WorkspaceEdit, apply it via
-- `vim.lsp.util.apply_workspace_edit`, then check the diagnostic clears.
--
-- The fixture's `match` misses enum variants, so the server publishes the
-- "Non-exhaustive match" diagnostic and offers "Fill in N missing match
-- arms" -- the one code action a stock server produces from a compiler
-- diagnostic without a host lint policy. The advice-driven rewrites (UFCS
-- and friends) come from a host-supplied ActionLayerPolicy and are not part
-- of the public surface.
--
-- Distinct from code_action_clears_diagnostic_spec.lua, which fabricates the
-- TextEdit. This spec catches issues that only surface when the
-- server-supplied WorkspaceEdit shape (changes vs documentChanges, ranges,
-- etc.) round-trips through nvim's apply_workspace_edit.

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

local initial_diags
local ok = vim.wait(8000, function()
  initial_diags = vim.diagnostic.get(buf)
  return has_nonexhaustive_diag(initial_diags)
end, 100)
if not ok then
  fail(('expected a non-exhaustive match diagnostic on first compile, got: %s')
    :format(vim.inspect(initial_diags)))
end

-- Place the cursor on the diagnostic so codeAction offers the fill fix.
local target
for _, d in ipairs(initial_diags) do
  if d.message and d.message:find('Non-exhaustive match', 1, true) then target = d; break end
end
vim.api.nvim_win_set_cursor(0, { target.lnum + 1, target.col })

local client_obj = vim.lsp.get_client_by_id(attached_id)
local enc = client_obj.offset_encoding or 'utf-16'

local uri = vim.uri_from_bufnr(buf)
local params = {
  textDocument = { uri = uri },
  range = {
    start = { line = target.lnum, character = target.col },
    ['end'] = { line = target.lnum, character = target.col },
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
if not (result and result.result) then fail('no codeAction result: ' .. vim.inspect(result)) end

-- Find the fill action.
local action
for _, a in ipairs(result.result) do
  if a.title and a.title:find('missing match arm', 1, true) then action = a; break end
end
if not action then
  fail(('no fill-match-arms action in codeActions: %s'):format(vim.inspect(result.result)))
end
if not (action.edit) then
  fail(('fill action has no edit: %s'):format(vim.inspect(action)))
end

-- Apply the WorkspaceEdit the server returned. apply_workspace_edit fires
-- nvim_buf_set_text which triggers TextChanged -> didChange.
vim.lsp.util.apply_workspace_edit(action.edit, enc)

-- Verify the buffer reflects the fill (so we know the edit applied).
local after_text = table.concat(vim.api.nvim_buf_get_lines(buf, 0, -1, false), '\n')
if not (after_text:find('Color::Green', 1, true) and after_text:find('Color::Blue', 1, true)) then
  fail(('apply_workspace_edit did not fill the arms; buffer is: %q'):format(after_text))
end

-- Wait for the diagnostic to clear.
local final_diags
ok = vim.wait(8000, function()
  final_diags = vim.diagnostic.get(buf)
  return not has_nonexhaustive_diag(final_diags)
end, 100)

if not ok then
  fail(('non-exhaustive diagnostic did not clear after apply_workspace_edit; got: %s')
    :format(vim.inspect(final_diags)))
end

print('OK: diagnostic cleared after server-driven code action')
os.exit(0)
