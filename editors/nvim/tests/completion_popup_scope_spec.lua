package.path = package.path .. ';' .. vim.fn.getcwd() .. '/tests/?.lua'
local NvimChild = require('_lib.nvim_child')

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

local child = NvimChild.open_completion('scopepop', [[
enum Hello {
    Alpha,
    Beta,
    LongThing,
}

fn foo() {
    let x: Hello = Hello:
}
]], true)

local triggers = child:exec_lua([[
  local cs = vim.lsp.get_clients({ name = 'rock' })
  if #cs == 0 then return nil end
  return (cs[1].server_capabilities.completionProvider or {}).triggerCharacters
]], {})
do
  local has_colon = false
  for _, t in ipairs(triggers or {}) do if t == ':' then has_colon = true end end
  if not has_colon then
    fail('triggerCharacters missing `:`; got ' .. vim.inspect(triggers))
  end
end

child:rpc('nvim_win_set_cursor', 0, { 8, 25 })
child:input('a:')

local words = assert(child:popup_words())


for _, want in ipairs({ 'Alpha', 'Beta', 'LongThing' }) do
  if not vim.tbl_contains(words, want) then
    fail('popup missing variant ' .. want .. '; got ' .. vim.inspect(words))
  end
end

for _, leak in ipairs({ 'Hello', 'foo' }) do
  if vim.tbl_contains(words, leak) then
    fail('popup leaked identifier ' .. leak .. '; got ' .. vim.inspect(words))
  end
end

child:stop()
print(string.format('OK: Hello:: pum has %d items, words=%s', #words, table.concat(words, ',')))
os.exit(0)
