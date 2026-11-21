-- Insert-mode popup at `Hello::|`. The CLI / response-level path
-- already has coverage in completion_scope_resolution_spec.lua; this
-- one drives a real `:` keystroke through the child-nvim harness so
-- the autotrigger registration (server's `completion_provider.
-- triggerCharacters` + nvim's built-in completion module) is
-- exercised end-to-end. If `:` ever falls out of trigger_characters
-- again, this spec catches it where the response-level one wouldn't.

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

-- Sanity: server advertises `:` as a trigger char. If this ever
-- regresses to just `.`, the autotrigger below silently no-ops and
-- the failure mode is "no popup" — the assertion below would still
-- catch it, but failing here gives a clearer signal.
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

-- Cursor at end of "    let x: Hello = Hello:" on line 8 (1-based).
-- That line's char count is 25 (`    let x: Hello = Hello:` is 25
-- bytes incl. leading 4 spaces).
child:rpc('nvim_win_set_cursor', 0, { 8, 25 })
-- Enter insert mode at end of line, then type the second `:` to
-- form `::` and let the trigger char fire autocompletion.
child:input('a:')

local words = assert(child:popup_words())


for _, want in ipairs({ 'Alpha', 'Beta', 'LongThing' }) do
  if not vim.tbl_contains(words, want) then
    fail('popup missing variant ' .. want .. '; got ' .. vim.inspect(words))
  end
end

-- Scope resolution must not leak general identifiers into the popup.
-- If `Hello` (the type itself) or `foo` show up, scope detection
-- fell through to identifier completion at the popup layer.
for _, leak in ipairs({ 'Hello', 'foo' }) do
  if vim.tbl_contains(words, leak) then
    fail('popup leaked identifier ' .. leak .. '; got ' .. vim.inspect(words))
  end
end

child:stop()
print(string.format('OK: Hello:: pum has %d items, words=%s', #words, table.concat(words, ',')))
os.exit(0)
