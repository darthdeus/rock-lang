package.path = package.path .. ';' .. vim.fn.getcwd() .. '/tests/?.lua'
local NvimChild = require('_lib.nvim_child')

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

local child = NvimChild.open_completion('commentpop', [[
struct Config {
    volume: float,
}

global cfg: Config = Config { volume: 1.0 }

fn foo() {
    // cfg
    print("text cfg")
    print("{cfg}")
}
]], true)

-- Type `.` after the `cfg` on the given line so the member-access trigger char
-- fires at that position, then report whether the popup came up.
local function popup_after_dot(line)
  -- Sit on the `g` of that line's `cfg`; `a` then inserts right after it.
  local text = child:rpc('nvim_buf_get_lines', 0, line - 1, line, true)[1]
  local at = assert(text:find('cfg', 1, true), 'no `cfg` on line ' .. line)
  local col = at + 1 -- 1-based find start -> 0-based index of the trailing `g`
  child:rpc('nvim_win_set_cursor', 0, { line, col })
  child:input('a.')
  local words = child:popup_words(2000)
  -- Undo the typed `.` so the next probe starts from the original buffer.
  child:input('<Esc>')
  child:cmd('undo')
  return words ~= nil, words
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
