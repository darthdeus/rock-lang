-- End-to-end: type a syntax error in a .rock file, expect an LSP
-- diagnostic in the buffer.
--
-- Pre-fix (silent-errors audit, 2026-05-03; deferred item 2), parse
-- errors in LSP recovery mode were dropped at five distinct sites
-- (parser/mod.rs:215, 1732, 1805 + parser_util.rs:117, 234). Typing
-- `fn foo(` produced no squiggle, with the only trace being a line
-- in /tmp/lsp_debug.log nobody saw. This spec is the editor-surface
-- regression fence — the rock-lsp/tests Rust integration tests pin
-- the underlying plumbing (storage.warnings + collect helper); this
-- spec proves the diagnostic actually reaches nvim's diagnostic
-- store (not just the publishDiagnostics notification we send).
--
-- Mirrors `diagnostics_squiggle_spec.lua` for the unused-var path.

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

local project = dofile('tests/_lib/project.lua')
local buf = project.open('parse_err_squiggle', { ['main.rock'] = 'fn foo(\n' })

local diags = nil
local ok = vim.wait(8000, function()
  diags = vim.diagnostic.get(buf)
  return diags and #diags > 0
end, 100)

if not ok then
  io.stderr:write('--- diagnostics state at timeout ---\n')
  io.stderr:write(vim.inspect(diags or {}) .. '\n')
  fail('no diagnostics arrived for syntax-error file within 8s')
end

-- Range must be non-empty for nvim to actually draw the squiggle.
for _, d in ipairs(diags) do
  if d.col == d.end_col and d.lnum == d.end_lnum then
    fail(('parse-error diagnostic has empty range; nvim will draw nothing: %s')
      :format(vim.inspect(d)))
  end
end

print(('OK: %d diagnostics published for syntax-error file'):format(#diags))
os.exit(0)
