local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

local project = dofile('tests/_lib/project.lua')
local bufnr, client_obj = project.open('tokmods', { ['main.rock'] = [[
fn foo() {
    let x: int? = null
    let y: int = 1
    if x != null {
        print(x)
    }
}
]] })

local legend = client_obj.server_capabilities.semanticTokensProvider.legend
-- LuaJIT bit ops — `<<` shift syntax isn't available in nvim's Lua dialect.
local bit = require('bit')
local function modifier_bit(name)
  for i, m in ipairs(legend.tokenModifiers) do
    if m == name then return bit.lshift(1, i - 1) end
  end
  fail('legend missing modifier: ' .. name)
end

local nullable_bit = modifier_bit('nullable')
local refined_bit = modifier_bit('refined')

local response = project.request(bufnr, 'textDocument/semanticTokens/full')
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

local function tok_at(l, c)
  for _, t in ipairs(toks) do
    if t.line == l and t.col == c then return t end
  end
  return nil
end

local x_decl = tok_at(1, 8) or fail('missing x decl token')
local x_cond = tok_at(3, 7) or fail('missing x condition read')
local x_then = tok_at(4, 14) or fail('missing x then-branch read')
local y_decl = tok_at(2, 8) or fail('missing y decl token')

if bit.band(x_decl.mods, nullable_bit) == 0 then
  fail('x decl missing nullable: ' .. vim.inspect(x_decl))
end
if bit.band(x_decl.mods, refined_bit) ~= 0 then
  fail('x decl unexpectedly carries refined: ' .. vim.inspect(x_decl))
end

if bit.band(x_cond.mods, nullable_bit) == 0 then
  fail('x condition read missing nullable: ' .. vim.inspect(x_cond))
end

if bit.band(x_then.mods, refined_bit) == 0 then
  fail('x then-branch read missing refined: ' .. vim.inspect(x_then))
end
if bit.band(x_then.mods, nullable_bit) ~= 0 then
  fail('x then-branch must not also have nullable: ' .. vim.inspect(x_then))
end

if bit.band(y_decl.mods, bit.bor(nullable_bit, refined_bit)) ~= 0 then
  fail('non-nullable y carries nullable/refined: ' .. vim.inspect(y_decl))
end

print(string.format(
  'OK: %d tokens; nullable_bit=%d refined_bit=%d (decl=nullable, then-read=refined, y=neither)',
  #toks, nullable_bit, refined_bit))
os.exit(0)
