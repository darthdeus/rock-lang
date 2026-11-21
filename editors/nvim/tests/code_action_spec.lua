local project = dofile('tests/_lib/project.lua')
local sources = { ['main.rock'] = [[
enum Color { Red, Green, Blue }

fn f(c: Color) -> int {
    match c {
        Color::Red => {}
    }
    return 0
}
]] }

local function nonexhaustive(buf)
  for _, d in ipairs(vim.diagnostic.get(buf)) do
    if d.message:find('Non-exhaustive match', 1, true) then return d end
  end
end

local function apply_text_edit(buf, encoding)
  for i, line in ipairs(vim.api.nvim_buf_get_lines(buf, 0, -1, false)) do
    if line:find('Color::Red', 1, true) then
      local pos = { line = i - 1, character = #line }
      vim.lsp.util.apply_text_edits({ {
        range = { start = pos, ['end'] = pos },
        newText = '\n        Color::Green => {}\n        Color::Blue => {}',
      } }, buf, encoding)
      return
    end
  end
  error('could not locate the existing match arm')
end

local function request_workspace_edit(buf, encoding, diagnostic)
  vim.api.nvim_win_set_cursor(0, { diagnostic.lnum + 1, diagnostic.col })
  local pos = { line = diagnostic.lnum, character = diagnostic.col }
  local actions = assert(project.request(buf, 'textDocument/codeAction', {
    range = { start = pos, ['end'] = pos },
    context = { diagnostics = {}, triggerKind = 2 },
  }, 30000), 'no codeAction result')
  for _, action in ipairs(actions) do
    if action.title and action.title:find('missing match arm', 1, true) then
      assert(action.edit, 'fill action has no edit: ' .. vim.inspect(action))
      vim.lsp.util.apply_workspace_edit(action.edit, encoding)
      return
    end
  end
  error('no fill-match-arms action: ' .. vim.inspect(actions))
end

for _, case in ipairs({
  { 'code_action_clear', apply_text_edit },
  { 'code_action_full', request_workspace_edit },
}) do
  local name, apply = unpack(case)
  local buf, client = project.open(name, sources)
  local diagnostic
  assert(vim.wait(8000, function()
    diagnostic = nonexhaustive(buf)
    return diagnostic ~= nil
  end, 100), name .. ': expected a non-exhaustive match diagnostic: ' .. vim.inspect(vim.diagnostic.get(buf)))

  apply(buf, client.offset_encoding or 'utf-16', diagnostic)
  local text = table.concat(vim.api.nvim_buf_get_lines(buf, 0, -1, false), '\n')
  assert(text:find('Color::Green', 1, true) and text:find('Color::Blue', 1, true),
    name .. ': edit did not add both arms: ' .. text)
  assert(vim.wait(8000, function() return not nonexhaustive(buf) end, 100),
    name .. ': diagnostic did not clear: ' .. vim.inspect(vim.diagnostic.get(buf)))
  print('OK: ' .. name)
end
