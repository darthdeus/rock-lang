-- textDocument/definition round-trip against a real rock-lsp.
--
-- The `.rock.lsp` harness covers single-file goto-def cases (~34
-- tests in `tests/lsp/goto_definition.rock.lsp`) and the Rust
-- integration tests cover multi-file jumps
-- (`rock-lsp/tests/multi_file_goto_def.rs`). This spec closes the
-- CLI/server drift gap by driving `textDocument/definition` over
-- the live LSP protocol — the same path nvim's
-- `vim.lsp.buf.definition()` uses.
--
-- Asserts:
--   * the request reaches the server (no error)
--   * the response is a Location/Location[] pointing at the
--     declaration in a different file
--   * URI is a `file://` path inside the test project
--
-- See `references_spec.lua` for the sibling pattern.

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

-- Two-file project: declaration in lib.rock, use in main.rock. The
-- jump goes main.rock -> lib.rock so we exercise the cross-file path.
local root = vim.fn.tempname()
vim.fn.mkdir(root, 'p')
do
  local f = assert(io.open(root .. '/stick.toml', 'w'))
  f:write('[package]\nname = "gotodef"\nversion = "0.1.0"\n'); f:close()

  -- line 0: fn make_foo() -> int { return 42 }
  f = assert(io.open(root .. '/lib.rock', 'w'))
  f:write('fn make_foo() -> int { return 42 }\n'); f:close()

  -- line 0: fn main() { let x = make_foo() }
  -- `make_foo` starts at column 20 in `fn main() { let x = make_foo() }`.
  f = assert(io.open(root .. '/main.rock', 'w'))
  f:write('fn main() { let x = make_foo() }\n'); f:close()
end

local client = require('rock.client')
vim.lsp.config('rock', client.make_config({ binary = bin }))
client.setup_autocmds()
vim.lsp.enable('rock')

-- Open main.rock so the cursor lands on the call site.
vim.cmd('edit ' .. vim.fn.fnameescape(root .. '/main.rock'))

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
local done, def_err, def_result = false, nil, nil
-- Cursor on `make_foo` call site at main.rock:0:22 (middle of the name).
local req_ok, _ = vim.lsp.buf_request(bufnr, 'textDocument/definition', {
  textDocument = { uri = vim.uri_from_bufnr(bufnr) },
  position = { line = 0, character = 22 },
}, function(err, result)
  def_err = err
  def_result = result
  done = true
end)
if not req_ok then fail('vim.lsp.buf_request returned false (no client?)') end

if not vim.wait(5000, function() return done end, 25) then
  fail('definition response did not arrive within 5s')
end

if def_err then
  fail('definition returned error: ' .. vim.inspect(def_err))
end

-- LSP allows either Location or Location[]. Normalize to a list.
local locs = def_result
if type(locs) == 'table' and locs.uri then
  locs = { locs }
end
if type(locs) ~= 'table' or #locs == 0 then
  fail('expected Location or non-empty Location[], got ' .. vim.inspect(def_result))
end

local target = locs[1]
if type(target.uri) ~= 'string' or type(target.range) ~= 'table' then
  fail('definition target malformed: ' .. vim.inspect(target))
end
if not target.uri:match('^file://') then
  fail('definition uri not a file URI: ' .. target.uri)
end
if not target.uri:match('/lib%.rock$') then
  fail('expected jump to lib.rock; got: ' .. target.uri)
end

-- The declaration `make_foo` lives at line 0, character 3 of lib.rock
-- (`fn make_foo`). We only assert line for stability — the LSP may
-- return either the ident span or the whole-fn span depending on
-- implementation. Line 0 is unambiguous either way.
if target.range.start.line ~= 0 then
  fail(('expected start.line=0 in lib.rock, got %s'):format(vim.inspect(target.range)))
end

print(('OK: goto-def round-trip jumped main.rock:0:22 -> %s:%d:%d'):format(
  target.uri:match('([^/]+)$') or target.uri,
  target.range.start.line,
  target.range.start.character))
os.exit(0)
