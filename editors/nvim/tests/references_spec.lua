-- textDocument/references round-trip against a real rock-lsp.
--
-- The Rust integration tests (`rock-lsp/tests/find_references_*.rs`)
-- exercise `find_references` directly via in-memory SourceFiles —
-- they do not touch the JSON-RPC server. The `.rock.lsp` harness is
-- single-file only. This spec closes the CLI/server drift gap by
-- driving `textDocument/references` over the live LSP protocol on
-- a real two-file project.
--
-- Asserts:
--   * the request reaches the server (no error)
--   * the response is an array of Location objects covering the
--     declaration and at least one cross-file use site
--   * URIs match `file://`-prefixed paths inside the test project

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

-- Two-file project: declaration in lib.rock, use in main.rock.
local root = vim.fn.tempname()
vim.fn.mkdir(root, 'p')
do
  local f = assert(io.open(root .. '/stick.toml', 'w'))
  f:write('[package]\nname = "refs"\nversion = "0.1.0"\n'); f:close()

  -- line 0: fn make_foo() -> int { return 42 }
  f = assert(io.open(root .. '/lib.rock', 'w'))
  f:write('fn make_foo() -> int { return 42 }\n'); f:close()

  -- line 0: fn main() { let x = make_foo() }
  f = assert(io.open(root .. '/main.rock', 'w'))
  f:write('fn main() { let x = make_foo() }\n'); f:close()
end

local client = require('rock.client')
vim.lsp.config('rock', client.make_config({ binary = bin }))
client.setup_autocmds()
vim.lsp.enable('rock')

-- Open lib.rock so the cursor lands on the declaration.
vim.cmd('edit ' .. vim.fn.fnameescape(root .. '/lib.rock'))

local attached_id
vim.api.nvim_create_autocmd('LspAttach', {
  callback = function(args)
    local c = vim.lsp.get_client_by_id(args.data.client_id)
    if c and c.name == 'rock' then attached_id = args.data.client_id end
  end,
})
if not vim.wait(5000, function() return attached_id ~= nil end, 50) then
  fail('LspAttach did not fire within 5s')
end

local bufnr = vim.api.nvim_get_current_buf()
local done, refs_err, refs_result = false, nil, nil
-- Cursor on `make_foo` declaration at lib.rock:0:3.
local req_ok, _ = vim.lsp.buf_request(bufnr, 'textDocument/references', {
  textDocument = { uri = vim.uri_from_bufnr(bufnr) },
  position = { line = 0, character = 3 },
  context = { includeDeclaration = true },
}, function(err, result)
  refs_err = err
  refs_result = result
  done = true
end)
if not req_ok then fail('vim.lsp.buf_request returned false (no client?)') end

if not vim.wait(5000, function() return done end, 25) then
  fail('references response did not arrive within 5s')
end

if refs_err then
  fail('references returned error: ' .. vim.inspect(refs_err))
end

if type(refs_result) ~= 'table' or #refs_result == 0 then
  fail('expected non-empty Location[] array, got ' .. vim.inspect(refs_result))
end

-- Validate every entry has the expected shape, and bucket by basename.
-- We assert the cross-module case specifically: at least one Location
-- in lib.rock (the decl) and at least one in main.rock (the call site).
local in_lib, in_main = false, false
for i, loc in ipairs(refs_result) do
  if type(loc) ~= 'table' or type(loc.uri) ~= 'string' or type(loc.range) ~= 'table' then
    fail('Location[' .. i .. '] malformed: ' .. vim.inspect(loc))
  end
  if not loc.uri:match('^file://') then
    fail('Location[' .. i .. '].uri not a file URI: ' .. loc.uri)
  end
  if loc.uri:match('/lib%.rock$') then in_lib = true end
  if loc.uri:match('/main%.rock$') then in_main = true end
end

if not in_lib then
  fail('expected a reference in lib.rock (decl); got: ' .. vim.inspect(refs_result))
end
if not in_main then
  fail('expected a reference in main.rock (cross-module call site); got: '
    .. vim.inspect(refs_result))
end

print(('OK: references round-trip returned %d locations across lib.rock + main.rock')
  :format(#refs_result))
os.exit(0)
