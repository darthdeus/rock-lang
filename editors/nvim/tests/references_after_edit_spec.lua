-- References must track a line-shifting buffer edit (real client sync path).
--
-- User-reported flow: add a line inside fn A, move the cursor onto a symbol
-- in fn B -> occurrence spans must use post-edit offsets, not be off by the
-- line delta. The `.rock.lsp` edit_sequence harness proves the server handles
-- didChange correctly; this spec closes the remaining gap -- nvim's own
-- didChange emission (incremental sync from nvim_buf_set_lines) feeding the
-- same request.

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
  f:write('[package]\nname = "refsedit"\nversion = "0.1.0"\n'); f:close()

  f = assert(io.open(root .. '/main.rock', 'w'))
  f:write(table.concat({
    'fn alpha() -> int {',
    '    return 1',
    '}',
    '',
    'fn beta() -> int {',
    '    let count = 2',
    '    return count + count',
    '}',
    '',
  }, '\n')); f:close()
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
  fail('LspAttach did not fire within 5s')
end

local bufnr = vim.api.nvim_get_current_buf()

--- Request references at (line, char); return sorted {line, char} starts.
local function ref_starts(line, character)
  local done, r_err, r_result = false, nil, nil
  local ok = vim.lsp.buf_request(bufnr, 'textDocument/references', {
    textDocument = { uri = vim.uri_from_bufnr(bufnr) },
    position = { line = line, character = character },
    context = { includeDeclaration = true },
  }, function(err, result)
    r_err, r_result, done = err, result, true
  end)
  if not ok then fail('buf_request returned false') end
  if not vim.wait(5000, function() return done end, 25) then
    fail('references response did not arrive within 5s')
  end
  if r_err then fail('references error: ' .. vim.inspect(r_err)) end
  local starts = {}
  for _, loc in ipairs(r_result or {}) do
    starts[#starts + 1] = { loc.range.start.line, loc.range.start.character }
  end
  table.sort(starts, function(a, b)
    if a[1] ~= b[1] then return a[1] < b[1] end
    return a[2] < b[2]
  end)
  return starts
end

local function assert_starts(got, want, label)
  local g, w = vim.inspect(got), vim.inspect(want)
  if g ~= w then fail(label .. ': got ' .. g .. ', want ' .. w) end
end

-- Baseline: `count` decl (5,8), uses (6,11) and (6,19).
assert_starts(ref_starts(5, 8), { { 5, 8 }, { 6, 11 }, { 6, 19 } }, 'baseline')

-- Add a line inside alpha's body (the user's exact edit), via the buffer API
-- so the real client emits didChange.
vim.api.nvim_buf_set_lines(bufnr, 1, 1, false, { '    let pad = 0' })
vim.wait(200) -- let the didChange flush + server recompile

assert_starts(
  ref_starts(6, 8),
  { { 6, 8 }, { 7, 11 }, { 7, 19 } },
  'after line insert in other fn'
)

-- Remove it again.
vim.api.nvim_buf_set_lines(bufnr, 1, 2, false, {})
vim.wait(200)

assert_starts(
  ref_starts(5, 8),
  { { 5, 8 }, { 6, 11 }, { 6, 19 } },
  'after line removal in other fn'
)

print('OK: references track line-shifting edits through the real client')
os.exit(0)
