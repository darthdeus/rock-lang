-- Exercise actual didOpen/didClose ordering from restored and unsaved buffers.
local bin = vim.env.ROCK_LSP_BIN
if not bin or bin == '' then
  print('SKIP: $ROCK_LSP_BIN not set')
  os.exit(0)
end
local root = vim.fn.tempname()
vim.fn.mkdir(root, 'p')
local clients = {}
local ok, err = xpcall(function()
  vim.filetype.add({ extension = { rock = 'rock' } })
  vim.fn.writefile({'[package]', 'name = "session_buffers"'}, root .. '/stick.toml')
  vim.fn.writefile({'struct Foo { x: int }'}, root .. '/types.rock')
  vim.fn.writefile({'fn use_it() -> Foo { Foo { x: 1 } }'}, root .. '/main.rock')
  local client = require('rock.client')
  vim.lsp.config('rock', client.make_config({binary = bin}))
  client.setup_autocmds()
  vim.lsp.enable('rock')
  vim.cmd.edit(vim.fn.fnameescape(root .. '/missing.rock'))
  local missing = vim.api.nvim_get_current_buf()
  assert(vim.wait(15000, function()
    clients = vim.lsp.get_clients({bufnr = missing, name = 'rock'})
    return #clients > 0 and clients[1].initialized
  end, 20), 'LSP failed to attach to missing buffer')
  local function request(buf, method, params)
    local result = vim.lsp.buf_request_sync(buf, method, params, 15000)
    assert(result and result[clients[1].id], 'LSP request timed out: ' .. method)
    local response = result[clients[1].id]
    assert(not response.err, vim.inspect(response.err))
    return response.result
  end
  -- A request acts as a barrier after didOpen; checking an initially empty
  -- diagnostics list alone would pass before the server had processed it.
  request(missing, 'textDocument/documentSymbol', {textDocument = {uri = vim.uri_from_bufnr(missing)}})
  for _, d in ipairs(vim.diagnostic.get(missing)) do
    assert(d.severity ~= vim.diagnostic.severity.ERROR, d.message)
  end
  vim.api.nvim_buf_set_lines(missing, 0, -1, false, {'fn draft() -> Foo { Foo { x: 1 } }'})
  local function definition(buf, line, character)
    return request(buf, 'textDocument/definition', {
      textDocument = {uri = vim.uri_from_bufnr(buf)}, position = {line = line, character = character},
    })
  end
  assert(vim.inspect(definition(missing, 0, 14)):find('types.rock', 1, true), 'unsaved file cannot resolve project types')
  vim.bo[missing].bufhidden = 'hide'
  vim.cmd.edit(vim.fn.fnameescape(root .. '/types.rock'))
  local types = vim.api.nvim_get_current_buf()
  vim.api.nvim_buf_set_lines(types, 0, -1, false, {'struct Draft { x: int }'})
  request(types, 'textDocument/documentSymbol', {textDocument = {uri = vim.uri_from_bufnr(types)}})
  vim.bo[types].bufhidden = 'hide'
  vim.cmd.edit(vim.fn.fnameescape(root .. '/main.rock'))
  local main = vim.api.nvim_get_current_buf()
  vim.api.nvim_buf_delete(types, {force = true})
  assert(vim.inspect(definition(main, 0, 15)):find('types.rock', 1, true), 'closing buffer lost disk symbols')
  assert(vim.api.nvim_buf_get_lines(missing, 0, -1, false)[1]:find('draft', 1, true), 'unsaved text was lost')
  assert(vim.uv.fs_stat(root .. '/missing.rock') == nil, 'LSP wrote the unsaved buffer to disk')
end, debug.traceback)
for _, c in ipairs(clients) do c:stop(true) end
vim.fn.delete(root, 'rf')
if not ok then
  io.stderr:write(err .. '\n')
  os.exit(1)
end
print('OK: missing first buffer, unsaved cross-file lookup, and close-to-disk recovery')
os.exit(0)
