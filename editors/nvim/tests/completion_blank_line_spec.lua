-- Space is a completion trigger character, so `let x = |` opens the popup on an
-- empty prefix. But `<CR>` auto-indent inserts spaces, which would trigger the
-- popup on a fresh line -- so `<CR><CR>` accepts item 1 instead of two newlines.
-- The server suppresses a space trigger whose line-prefix is only whitespace;
-- a space after a token, and any non-space (manual) invoke, still complete.
--
-- Response-level: asserts on `textDocument/completion` payloads for crafted
-- trigger contexts (TriggerCharacter=' ' vs Invoked). See completion_popup_spec
-- for the insert-mode pum variant.

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

local bin = vim.env.ROCK_LSP_BIN
if not bin or bin == '' then
  print('SKIP: $ROCK_LSP_BIN not set')
  os.exit(0)
end

vim.filetype.add({ extension = { rock = 'rock' } })

local root = vim.fn.tempname()
vim.fn.mkdir(root, 'p')
do
  local f = assert(io.open(root .. '/stick.toml', 'w'))
  f:write('[package]\nname = "blank"\nversion = "0.1.0"\n'); f:close()
  -- line0 `fn f() {`, line1 `    let count = 1`, line2 `    ` (indent only), line3 `}`
  f = assert(io.open(root .. '/main.rock', 'w'))
  f:write('fn f() {\n    let count = 1\n    \n}\n'); f:close()
end

vim.cmd('edit ' .. vim.fn.fnameescape(root .. '/main.rock'))
local buf = vim.api.nvim_get_current_buf()
local cid = vim.lsp.start({ name = 'rock', cmd = { bin, 'lsp', 'serve' }, root_dir = root }, { bufnr = buf })
if not cid then fail('could not start client') end
if not vim.wait(8000, function()
  return #vim.lsp.get_clients({ bufnr = buf, name = 'rock' }) > 0
end, 50) then fail('client did not attach') end

local function count(line, char, ctx)
  local res = vim.lsp.buf_request_sync(buf, 'textDocument/completion', {
    textDocument = { uri = vim.uri_from_bufnr(buf) },
    position = { line = line, character = char },
    context = ctx,
  }, 5000)
  if not res then return 0 end
  for _, r in pairs(res) do
    if r.result then
      local items = r.result.items or r.result
      return #items
    end
  end
  return 0
end

local SPACE = { triggerKind = 2, triggerCharacter = ' ' }
local INVOKED = { triggerKind = 1 }

-- line 2 is indentation only (`    `): a bare newline.
if count(2, 4, SPACE) ~= 0 then
  fail('space trigger on a whitespace-only line must be suppressed')
end
-- `    let count = ` on line 1: a real space-after-token context.
if count(1, 15, SPACE) == 0 then
  fail('space after a token must still complete')
end
-- Manual (Invoked) on the same blank line is never suppressed.
if count(2, 4, INVOKED) == 0 then
  fail('manual invoke on a blank line must still complete')
end

print('OK: completion_blank_line_spec passed')
os.exit(0)
