-- Live-LSP smoke for the nullable / refined semantic-token modifiers.
-- Asserts that the legend exposes both modifiers and that a real
-- buffer with a nullable local + a refined read gets the expected
-- bits set on the response. Per CLAUDE.md "reproduce LSP bugs through
-- nvim, not the CLI" — this is the editor-surface counterpart to
-- rock-lsp/tests/semantic_tokens_nullable_var.rs.

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
  f:write('[package]\nname = "tokmods"\nversion = "0.1.0"\n'); f:close()
  -- `x` declared `int?`, narrowed inside `if x != null { ... }`.
  -- Decl + condition read carry `nullable`; then-branch read carries
  -- `refined`. `y` is `int` and carries neither.
  f = assert(io.open(root .. '/main.rock', 'w'))
  f:write([[
fn foo() {
    let x: int? = null
    let y: int = 1
    if x != null {
        print(x)
    }
}
]]); f:close()
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

local client_obj = vim.lsp.get_client_by_id(attached_id)
local legend = client_obj.server_capabilities.semanticTokensProvider.legend
-- LuaJIT bit ops — `<<` shift syntax isn't available in nvim's Lua dialect.
local bit = require('bit')
local function modifier_bit(name)
  for i, m in ipairs(legend.tokenModifiers) do
    if m == name then return bit.lshift(1, i - 1) end
  end
  fail('legend missing modifier: ' .. name)
end

-- Both modifiers must be advertised.
local nullable_bit = modifier_bit('nullable')
local refined_bit = modifier_bit('refined')

-- Request semantic tokens for the buffer.
local bufnr = vim.api.nvim_get_current_buf()
local response, err
vim.lsp.buf_request(bufnr, 'textDocument/semanticTokens/full', {
  textDocument = { uri = vim.uri_from_bufnr(bufnr) },
}, function(e, result) err = e; response = result end)
if not vim.wait(3000, function() return response ~= nil or err ~= nil end, 25) then
  fail('semantic tokens did not respond within 3s')
end
if err then fail('semantic tokens error: ' .. vim.inspect(err)) end
if not response or not response.data then
  fail('expected response.data, got: ' .. vim.inspect(response))
end

-- Decode delta-encoded tokens. LSP semantic-tokens wire format:
-- [deltaLine, deltaStart, length, tokenType, tokenModifiersBitset]
-- repeated. deltaStart is relative to the previous token on the same
-- line; deltaLine is relative to the previous token's line.
local toks = {}
local line, col = 0, 0
local data = response.data
for i = 1, #data, 5 do
  local dl, ds, len, ty, mods = data[i], data[i+1], data[i+2], data[i+3], data[i+4]
  if dl == 0 then col = col + ds else line = line + dl; col = ds end
  table.insert(toks, { line = line, col = col, len = len, ty = ty, mods = mods })
end

-- Helper: find the token at a specific (line, col).
local function tok_at(l, c)
  for _, t in ipairs(toks) do
    if t.line == l and t.col == c then return t end
  end
  return nil
end

-- Source layout (0-indexed):
--   1: "    let x: int? = null"   x decl at col 8
--   2: "    let y: int = 1"        y decl at col 8
--   3: "    if x != null {"        x cond read at col 7
--   4: "        print(x)"          x then-branch read at col 14
local x_decl = tok_at(1, 8) or fail('missing x decl token')
local x_cond = tok_at(3, 7) or fail('missing x condition read')
local x_then = tok_at(4, 14) or fail('missing x then-branch read')
local y_decl = tok_at(2, 8) or fail('missing y decl token')

-- x decl: nullable, NOT refined.
if bit.band(x_decl.mods, nullable_bit) == 0 then
  fail('x decl missing nullable: ' .. vim.inspect(x_decl))
end
if bit.band(x_decl.mods, refined_bit) ~= 0 then
  fail('x decl unexpectedly carries refined: ' .. vim.inspect(x_decl))
end

-- x condition read: nullable.
if bit.band(x_cond.mods, nullable_bit) == 0 then
  fail('x condition read missing nullable: ' .. vim.inspect(x_cond))
end

-- x then-branch read: refined, NOT nullable (mutually exclusive).
if bit.band(x_then.mods, refined_bit) == 0 then
  fail('x then-branch read missing refined: ' .. vim.inspect(x_then))
end
if bit.band(x_then.mods, nullable_bit) ~= 0 then
  fail('x then-branch must not also have nullable: ' .. vim.inspect(x_then))
end

-- y: neither modifier (int, no nullability).
if bit.band(y_decl.mods, bit.bor(nullable_bit, refined_bit)) ~= 0 then
  fail('non-nullable y carries nullable/refined: ' .. vim.inspect(y_decl))
end

print(string.format(
  'OK: %d tokens; nullable_bit=%d refined_bit=%d (decl=nullable, then-read=refined, y=neither)',
  #toks, nullable_bit, refined_bit))
os.exit(0)
