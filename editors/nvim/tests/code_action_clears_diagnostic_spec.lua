-- Layer-4 repro: apply a code-action edit that fixes a diagnostic and check
-- the diagnostic clears in nvim. Editor-surface part of the four-layer
-- LSP-bug stack: the CLI and the in-process Rust tests both confirm the LSP
-- publishes correctly; this spec is the only thing that catches client/server
-- drift (the canonical case CLAUDE.md warns about).
--
-- Specifically: a `match` that misses enum variants triggers the
-- "Non-exhaustive match" error and the server offers a "Fill in N missing
-- match arms" action. This spec applies the *fabricated* equivalent of that
-- edit (the sibling spec drives the server's own WorkspaceEdit), then
-- requires the diagnostic to clear. The edit shape is what
-- `vim.lsp.util.apply_workspace_edit` produces for the action.

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

local project = dofile('tests/_lib/project.lua')
local buf, client_obj = project.open('code_action_clear', { ['main.rock'] = [[
enum Color { Red, Green, Blue }

fn f(c: Color) -> int {
    match c {
        Color::Red => {}
    }
    return 0
}
]] })

local function has_nonexhaustive_diag(diags)
  for _, d in ipairs(diags or {}) do
    if d.message and d.message:find('Non-exhaustive match', 1, true) then
      return true
    end
  end
  return false
end

-- Wait for the non-exhaustive diagnostic to appear.
local initial_diags
local ok = vim.wait(8000, function()
  initial_diags = vim.diagnostic.get(buf)
  return has_nonexhaustive_diag(initial_diags)
end, 100)
if not ok then
  fail(('expected a non-exhaustive match diagnostic on first compile, got: %s')
    :format(vim.inspect(initial_diags)))
end

-- Apply the arms the server's fill action would insert: append them after the
-- one existing arm. Mimics what apply_workspace_edit does for that action.
local lines = vim.api.nvim_buf_get_lines(buf, 0, -1, false)
local arm_line, arm_len = nil, nil
for i, line in ipairs(lines) do
  if line:find('Color::Red', 1, true) then
    arm_line, arm_len = i - 1, #line
    break
  end
end
if not arm_line then fail('could not locate the existing match arm') end

local text_edits = { {
  range = {
    start = { line = arm_line, character = arm_len },
    ['end'] = { line = arm_line, character = arm_len },
  },
  newText = '\n        Color::Green => {}\n        Color::Blue => {}',
} }

local enc = client_obj and client_obj.offset_encoding or 'utf-16'
vim.lsp.util.apply_text_edits(text_edits, buf, enc)

-- Confirm the buffer reflects the new arms.
local after_text = table.concat(vim.api.nvim_buf_get_lines(buf, 0, -1, false), '\n')
if not (after_text:find('Color::Green', 1, true) and after_text:find('Color::Blue', 1, true)) then
  fail(('apply_text_edits did not add the arms; buffer is: %q'):format(after_text))
end

-- Now wait for the diagnostic to clear.
local final_diags
ok = vim.wait(8000, function()
  final_diags = vim.diagnostic.get(buf)
  return not has_nonexhaustive_diag(final_diags)
end, 100)

if not ok then
  fail(('non-exhaustive diagnostic did not clear after the edit; got: %s')
    :format(vim.inspect(final_diags)))
end

print('OK: diagnostic cleared after apply_text_edits')
os.exit(0)
