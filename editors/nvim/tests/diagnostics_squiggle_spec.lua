-- End-to-end check that lint warnings (e.g. unused variable) reach the
-- editor as nvim diagnostics. This is the spec the "no squiggles in nvim"
-- regression would have failed against — the LSP server emits the warning
-- correctly, but without this spec we only catch CLI/unit-test regressions
-- and miss the actual editor surface.
--
-- Requires $ROCK_LSP_BIN. Skips when unset (mirrors real_lsp_smoke_spec.lua)
-- so run.sh works standalone.

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

local project = dofile('tests/_lib/project.lua')
local buf = project.open('diag_squiggle', { ['main.rock'] = [[
@artifact_definition(catalog="widgets", key_param="key")
deco init widget(key: string) {}
fn main() {
    let unused_thing = 42
    print("hi")
}
]] })

local diags = nil
local ok = vim.wait(8000, function()
  diags = vim.diagnostic.get(buf)
  return diags and #diags > 0
end, 100)

if not ok then
  io.stderr:write('--- diagnostics state at timeout ---\n')
  io.stderr:write(vim.inspect(diags or {}) .. '\n')
  fail('no diagnostics arrived for unused-var file within 8s')
end

-- Find the unused-var WARN. Other diagnostics may coexist (HINT-level
-- linter advice, etc.) — we just need to prove at least one squiggle-
-- worthy WARNING reached the buffer.
local warn_severity = vim.diagnostic.severity.WARN
local saw_warn = false
local saw_unused = false
for _, d in ipairs(diags) do
  if d.severity == warn_severity then saw_warn = true end
  if d.message and d.message:find('unused variable `key`', 1, true) then
    fail(('decorator metadata parameter was reported unused: %s'):format(vim.inspect(d)))
  end
  if d.message and d.message:lower():find('unused', 1, true) then
    saw_unused = true
    -- Range must be non-empty for nvim to actually draw the squiggle.
    if d.col == d.end_col and d.lnum == d.end_lnum then
      fail(('unused-var diagnostic has empty range; nvim will draw nothing: %s')
        :format(vim.inspect(d)))
    end
  end
end

if not saw_warn then
  fail(('no WARN-severity diagnostic in buffer; got: %s'):format(vim.inspect(diags)))
end
if not saw_unused then
  fail(('no diagnostic mentioning "unused" reached the buffer; got: %s')
    :format(vim.inspect(diags)))
end

print(('OK: %d diagnostics published, WARN+unused both present'):format(#diags))
os.exit(0)
