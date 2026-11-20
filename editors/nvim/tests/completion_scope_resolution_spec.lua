-- Live-LSP enum-variant completion at `Hello::|` with no prefix.
--
-- The CLI path (`rock-lsp completion file:line:col` → in-process
-- `do_completion`) returns Alpha/Beta/LongThing here. Reproducing
-- through the docker-spec harness because the live server has drifted
-- from the CLI before (CLAUDE.md "LSP bugs: reproduce through nvim,
-- not the CLI"), and because completion trigger registration
-- (`completion_provider.trigger_characters`) is only exercised
-- end-to-end through the editor.

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

local project = dofile('tests/_lib/project.lua')
local buf, client = project.open('scope', { ['main.rock'] = [[
enum Hello {
    Alpha,
    Beta,
    LongThing,
}

fn foo() {
    let x: Hello = Hello::
}
]] })

-- Without `:` in the advertised trigger_characters, nvim's built-in
-- completion (and most plugins) won't auto-fire on `::`, and the user
-- has to manually invoke omnifunc. This is a server-capability
-- regression — the in-process / explicit-request path still returns
-- variants, so a popup-doesn't-appear bug only shows up here.
do
  local c = client
  local triggers = c and c.server_capabilities
    and c.server_capabilities.completionProvider
    and c.server_capabilities.completionProvider.triggerCharacters
  if type(triggers) ~= 'table' then
    fail('completionProvider.triggerCharacters missing: ' .. vim.inspect(triggers))
  end
  local has_colon = false
  for _, t in ipairs(triggers) do if t == ':' then has_colon = true end end
  if not has_colon then
    fail('triggerCharacters must include `:` for `Foo::|` to auto-fire; got '
      .. vim.inspect(triggers))
  end
end

local response = project.request(buf, 'textDocument/completion', {
  position = { line = 7, character = 26 },
  context = { triggerKind = 1 },
})

local items = response
if type(items) == 'table' and items.items then items = items.items end
if type(items) ~= 'table' or #items == 0 then
  fail('expected non-empty items, got ' .. vim.inspect(response))
end

local labels = {}
for _, it in ipairs(items) do table.insert(labels, it.label) end

local function has(label)
  for _, l in ipairs(labels) do if l == label then return true end end
  return false
end

for _, want in ipairs({ 'Alpha', 'Beta', 'LongThing' }) do
  if not has(want) then
    fail('missing variant ' .. want .. '; got ' .. vim.inspect(labels))
  end
end

-- The scope-resolution path must NOT leak general identifiers
-- (globals/fns/locals) into the popup — that defeats the variant-only
-- intent of `Foo::|`. If `Hello` (the type itself) shows up, scope
-- detection fell through to identifier completion.
for _, leak in ipairs({ 'Hello', 'foo' }) do
  if has(leak) then
    fail('scope completion leaked identifier ' .. leak .. '; got ' .. vim.inspect(labels))
  end
end

print(string.format('OK: Hello:: -> %d items: %s', #items, table.concat(labels, ',')))
os.exit(0)
