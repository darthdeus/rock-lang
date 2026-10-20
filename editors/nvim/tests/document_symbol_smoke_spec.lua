-- End-to-end smoke test that the server serves document symbols.
--
-- Requires $ROCK_LSP_BIN to point at an executable rock binary (supplied by
-- tests/docker-run.sh); skips with exit 0 when unset, like
-- real_lsp_smoke_spec.lua.
--
-- Drives the real spawn + attach + `textDocument/documentSymbol` path: bone
-- walks the module arenas, so a NON-EMPTY response proves the server compiled
-- the unit and answered a structural query, not just that it launched.

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

vim.notify = function() end
vim.notify_once = function() end
vim.filetype.add({ extension = { rock = 'rock' } })

local root = vim.fn.tempname()
vim.fn.mkdir(root, 'p')
local f = assert(io.open(root .. '/stick.toml', 'w'))
f:write('[package]\nname = "symbol_smoke"\nversion = "0.1.0"\n')
f:close()
local rock_path = root .. '/main.rock'
f = assert(io.open(rock_path, 'w'))
f:write(table.concat({
  'struct Player {',
  '    health: int,',
  '}',
  '',
  'fn heal(p: Player) -> Player {',
  '    return p',
  '}',
  '',
  'fn main() {',
  '    let x = 1',
  '}',
  '',
}, '\n'))
f:close()

local client = require('rock.client')
local cfg = client.make_config({ binary = bin })
vim.lsp.config('rock', cfg)
client.setup_autocmds()
vim.lsp.enable('rock')
vim.cmd('edit ' .. vim.fn.fnameescape(rock_path))

local attached_id = nil
vim.api.nvim_create_autocmd('LspAttach', {
  callback = function(args)
    local c = vim.lsp.get_client_by_id(args.data.client_id)
    if c and c.name == 'rock' then attached_id = args.data.client_id end
  end,
})
if not vim.wait(8000, function() return attached_id ~= nil end, 50) then
  fail('rock-lsp did not attach within 8s')
end

-- Let didOpen reach the bone session before querying.
vim.wait(500)

local doc_done, doc_err, doc_res = false, nil, nil
vim.lsp.buf_request(0, 'textDocument/documentSymbol',
  { textDocument = { uri = vim.uri_from_bufnr(0) } },
  function(err, res)
    doc_done, doc_err, doc_res = true, err, res
  end)
if not vim.wait(4000, function() return doc_done end, 50) then
  fail('documentSymbol request timed out')
end
if doc_err then
  fail('documentSymbol returned an error: ' .. vim.inspect(doc_err))
end

local names = {}
for _, s in ipairs(doc_res or {}) do
  names[s.name] = true
end
for _, want in ipairs({ 'Player', 'heal', 'main' }) do
  if not names[want] then
    fail(('missing %q in document symbols (got: %s)')
      :format(want, vim.inspect(doc_res)))
  end
end

print('OK: rock.nvim serves document symbols (Player, heal, main)')
os.exit(0)
