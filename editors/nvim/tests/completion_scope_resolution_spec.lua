-- Live-LSP enum-variant completion at `Hello::|` with no prefix.
--
-- The CLI path (`rock-lsp completion file:line:col` → in-process
-- `do_completion`) returns Alpha/Beta/LongThing here. Reproducing
-- through the docker-spec harness because the live server has drifted
-- from the CLI before (CLAUDE.md "LSP bugs: reproduce through nvim,
-- not the CLI"), and because completion trigger registration
-- (`completion_provider.trigger_characters`) is only exercised
-- end-to-end through the editor.

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

local bin = vim.env.ROCK_LSP_BIN
if not bin or bin == '' then
  print('SKIP: $ROCK_LSP_BIN not set')
  os.exit(0)
end

vim.notify = function() end
vim.notify_once = function() end
vim.filetype.add({ extension = { rock = 'rock' } })

local root = vim.fn.tempname()
vim.fn.mkdir(root, 'p')
do
  local f = assert(io.open(root .. '/stick.toml', 'w'))
  f:write('[package]\nname = "scope"\nversion = "0.1.0"\n'); f:close()
  f = assert(io.open(root .. '/main.rock', 'w'))
  -- Cursor will sit immediately after `::` on the assignment line.
  -- Empty prefix is the focus: we want to see every variant.
  f:write([[
enum Hello {
    Alpha,
    Beta,
    LongThing,
}

fn foo() {
    let x: Hello = Hello::
}
]])
  f:close()
end

local client = require('rock.client')
vim.lsp.config('rock', client.make_config({ binary = bin }))
client.setup_autocmds()
vim.lsp.enable('rock')

vim.cmd('edit ' .. vim.fn.fnameescape(root .. '/main.rock'))
local attached_id
vim.api.nvim_create_autocmd('LspAttach', {
  callback = function(args)
    local c = vim.lsp.get_client_by_id(args.data.client_id)
    if c and c.name == 'rock' then attached_id = args.data.client_id end
  end,
})
if not vim.wait(5000, function() return attached_id ~= nil end, 50) then
  fail('LspAttach did not fire')
end
vim.wait(500, function() return false end)

-- Without `:` in the advertised trigger_characters, nvim's built-in
-- completion (and most plugins) won't auto-fire on `::`, and the user
-- has to manually invoke omnifunc. This is a server-capability
-- regression — the in-process / explicit-request path still returns
-- variants, so a popup-doesn't-appear bug only shows up here.
do
  local c = vim.lsp.get_client_by_id(attached_id)
  local triggers = c and c.server_capabilities
    and c.server_capabilities.completionProvider
    and c.server_capabilities.completionProvider.triggerCharacters
  if type(triggers) ~= 'table' then
    fail('completionProvider.triggerCharacters missing: ' .. vim.inspect(triggers))
  end
  local has_colon = false
  for _, t in ipairs(triggers) do if t == ':' then has_colon = true end end
  if not has_colon then
    fail('triggerCharacters must include `:` for `Foo::|` to auto-fire; got '
      .. vim.inspect(triggers))
  end
end

local bufnr = vim.api.nvim_get_current_buf()

-- Line 7 (0-indexed): "    let x: Hello = Hello::"
-- Char 26 is right after the second `:` (length-of-line cursor).
local response, err
vim.lsp.buf_request(bufnr, 'textDocument/completion', {
  textDocument = { uri = vim.uri_from_bufnr(bufnr) },
  position = { line = 7, character = 26 },
  context = { triggerKind = 1 },
}, function(e, result) err = e; response = result end)

if not vim.wait(3000, function() return response ~= nil or err ~= nil end, 25) then
  fail('completion did not respond within 3s')
end
if err then fail('completion returned error: ' .. vim.inspect(err)) end

local items = response
if type(items) == 'table' and items.items then items = items.items end
if type(items) ~= 'table' or #items == 0 then
  fail('expected non-empty items, got ' .. vim.inspect(response))
end

local labels = {}
for _, it in ipairs(items) do table.insert(labels, it.label) end

local function has(label)
  for _, l in ipairs(labels) do if l == label then return true end end
  return false
end

for _, want in ipairs({ 'Alpha', 'Beta', 'LongThing' }) do
  if not has(want) then
    fail('missing variant ' .. want .. '; got ' .. vim.inspect(labels))
  end
end

-- The scope-resolution path must NOT leak general identifiers
-- (globals/fns/locals) into the popup — that defeats the variant-only
-- intent of `Foo::|`. If `Hello` (the type itself) shows up, scope
-- detection fell through to identifier completion.
for _, leak in ipairs({ 'Hello', 'foo' }) do
  if has(leak) then
    fail('scope completion leaked identifier ' .. leak .. '; got ' .. vim.inspect(labels))
  end
end

print(string.format('OK: Hello:: -> %d items: %s', #items, table.concat(labels, ',')))
os.exit(0)
