-- Typing inside a `//` comment or a string literal's text must not raise the
-- completion popup, while an interpolation hole must still raise it. The
-- response-level rules live in bone-query (completion_in_comment.rs,
-- completion_in_string.rs); this drives real `.` trigger keystrokes so an empty
-- response is confirmed to leave the pum closed rather than showing a
-- stale/fallback list.

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
  f:write('[package]\nname = "commentpop"\nversion = "0.1.0"\n'); f:close()
  f = assert(io.open(root .. '/main.rock', 'w'))
  f:write([[
struct Config {
    volume: float,
}

global cfg: Config = Config { volume: 1.0 }

fn foo() {
    // cfg
    print("text cfg")
    print("{cfg}")
}
]])
  f:close()
end

local child = NvimChild.start({ rtp = { vim.fn.getcwd() } })

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

-- Type `.` after the `cfg` on the given line so the member-access trigger char
-- fires at that position, then report whether the popup came up.
local function popup_after_dot(line)
  -- Sit on the `g` of that line's `cfg`; `a` then inserts right after it.
  local text = child:rpc('nvim_buf_get_lines', 0, line - 1, line, true)[1]
  local at = assert(text:find('cfg', 1, true), 'no `cfg` on line ' .. line)
  local col = at + 1 -- 1-based find start -> 0-based index of the trailing `g`
  child:rpc('nvim_win_set_cursor', 0, { line, col })
  child:input('a.')
  local appeared = child:wait_for(function()
    return child:call('pumvisible', {}) == 1
  end, 2000)
  local words = {}
  if appeared then
    for _, it in ipairs(child:call('complete_info', { { 'items' } }).items or {}) do
      table.insert(words, it.word)
    end
  end
  -- Undo the typed `.` so the next probe starts from the original buffer.
  child:input('<Esc>')
  child:cmd('undo')
  return appeared, words
end

-- `    // cfg` (line 8, 1-based).
local appeared, words = popup_after_dot(8)
if appeared then
  fail('popup appeared inside a comment; words=' .. vim.inspect(words))
end

-- `    print("text cfg")` -- that `cfg` is string text.
appeared, words = popup_after_dot(9)
if appeared then
  fail('popup appeared inside string text; words=' .. vim.inspect(words))
end

-- `    print("{cfg}")` -- that `cfg` is an interpolation hole. This one MUST
-- popup: a hole is code.
appeared, words = popup_after_dot(10)
if not appeared then
  fail('no popup inside an interpolation hole; mode='
    .. vim.inspect(child:rpc('nvim_get_mode'))
    .. '\nstderr=' .. table.concat(child.stderr, '\n'))
end
do
  local has_volume = false
  for _, w in ipairs(words) do if w == 'volume' then has_volume = true end end
  if not has_volume then
    fail('hole popup missing `volume` field; got ' .. vim.inspect(words))
  end
end

child:stop()
print('OK: comment and string text offer no popup, interpolation hole does')
os.exit(0)
