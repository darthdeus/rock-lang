-- End-to-end check that the two-label `./` hint reaches nvim as TWO squiggles:
-- one on the `/` (the cause) and one on the use site. The LSP used to drop
-- every label but the first, so only the `/` line got a squiggle.
--
-- Requires $ROCK_LSP_BIN. Skips when unset (mirrors diagnostics_squiggle_spec).

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

local project = dofile('tests/_lib/project.lua')
local buf = project.open('float_div_hint', { ['main.rock'] = [[
fn main() {
  let h = 8 / 3
  let x: int = 2 * h
}
]] })

local diags = nil
local ok = vim.wait(8000, function()
  diags = vim.diagnostic.get(buf)
  return diags and #diags >= 2
end, 100)

if not ok then
  io.stderr:write('--- diagnostics state at timeout ---\n')
  io.stderr:write(vim.inspect(diags or {}) .. '\n')
  fail('expected >= 2 diagnostics (both lines) within 8s')
end

-- A squiggle on the `/` line (lnum 1) AND the use-site line (lnum 2).
local on_div, on_use = false, false
for _, d in ipairs(diags) do
  if d.lnum == 1 then on_div = true end
  if d.lnum == 2 then on_use = true end
  -- Range must be non-empty or nvim draws nothing.
  if d.col == d.end_col and d.lnum == d.end_lnum then
    fail(('diagnostic has empty range: %s'):format(vim.inspect(d)))
  end
end

if not (on_div and on_use) then
  fail(('expected squiggles on BOTH the `/` line (1) and the use line (2); got: %s')
    :format(vim.inspect(vim.tbl_map(function(d) return { lnum = d.lnum, msg = d.message } end, diags))))
end

print(('OK: %d diagnostics, squiggles on both the `/` and the use site'):format(#diags))
os.exit(0)
