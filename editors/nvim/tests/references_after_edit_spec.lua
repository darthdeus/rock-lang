-- References must track a line-shifting buffer edit (real client sync path).
--
-- User-reported flow: add a line inside fn A, move the cursor onto a symbol
-- in fn B -> occurrence spans must use post-edit offsets, not be off by the
-- line delta. This spec feeds the server Neovim's own didChange notification
-- after an incremental edit from nvim_buf_set_lines.

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

local project = dofile('tests/_lib/project.lua')
local bufnr = project.open('refsedit', { ['main.rock'] = [[
fn alpha() -> int {
    return 1
}

fn beta() -> int {
    let count = 2
    return count + count
}
]] })

--- Request references at (line, char); return sorted {line, char} starts.
local function ref_starts(line, character)
  local locations = project.request(bufnr, 'textDocument/references', {
    position = { line = line, character = character },
    context = { includeDeclaration = true },
  })
  local starts = {}
  for _, loc in ipairs(locations or {}) do
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
