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

local bin = vim.env.ROCK_LSP_BIN
if not bin or bin == '' then
  print('SKIP: $ROCK_LSP_BIN not set')
  os.exit(0)
end

local root = vim.fn.tempname()
vim.fn.mkdir(root, 'p')
do
  local f = assert(io.open(root .. '/stick.toml', 'w'))
  f:write('[package]\nname = "scopepop"\nversion = "0.1.0"\n'); f:close()
  -- File has the first `:` already typed so the test only needs to
  -- send the trigger char itself.
  f = assert(io.open(root .. '/main.rock', 'w'))
  f:write([[
enum Hello {
    Alpha,
    Beta,
    LongThing,
}

fn foo() {
    let x: Hello = Hello:
}
]])
  f:close()
end

local rtp = vim.fn.getcwd()
local child = NvimChild.start({ rtp = { rtp } })

-- autotrigger=true so typing `:` actually fires a request through
-- the trigger-characters path (not a manual <C-x><C-o>).
child:exec_lua([[
  vim.filetype.add({ extension = { rock = 'rock' } })
  local client = require('rock.client')
  vim.lsp.config('rock', client.make_config({ binary = ... }))
  client.setup_autocmds()
  vim.lsp.enable('rock')
  vim.api.nvim_create_autocmd('LspAttach', { callback = function(args)
    local c = vim.lsp.get_client_by_id(args.data.client_id)
    if c and c.name == 'rock' then
      vim.lsp.completion.enable(true, c.id, args.buf, { autotrigger = true })
    end
  end })
]], { bin })

child:cmd('edit ' .. vim.fn.fnameescape(root .. '/main.rock'))
child:wait_for_lsp_attach('rock', 5000)

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

local ok = child:wait_for(function()
  return child:call('pumvisible', {}) == 1
end, 3000)
if not ok then
  fail('popup did not appear after typing `:` (autotrigger). mode='
    .. vim.inspect(child:rpc('nvim_get_mode'))
    .. '\nstderr=' .. table.concat(child.stderr, '\n'))
end

local items = child:call('complete_info', { { 'items' } }).items or {}
if #items == 0 then fail('complete_info returned empty items') end

local words = {}
for _, it in ipairs(items) do table.insert(words, it.word) end
local function has(w)
  for _, x in ipairs(words) do if x == w then return true end end
  return false
end

for _, want in ipairs({ 'Alpha', 'Beta', 'LongThing' }) do
  if not has(want) then
    fail('popup missing variant ' .. want .. '; got ' .. vim.inspect(words))
  end
end

-- Scope resolution must not leak general identifiers into the popup.
-- If `Hello` (the type itself) or `foo` show up, scope detection
-- fell through to identifier completion at the popup layer.
for _, leak in ipairs({ 'Hello', 'foo' }) do
  if has(leak) then
    fail('popup leaked identifier ' .. leak .. '; got ' .. vim.inspect(words))
  end
end

child:stop()
print(string.format('OK: Hello:: pum has %d items, words=%s', #items, table.concat(words, ',')))
os.exit(0)
