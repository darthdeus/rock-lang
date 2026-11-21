local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

local project = dofile('tests/_lib/project.lua')
local buf = project.open('gotodef', {
  ['lib.rock'] = 'fn make_foo() -> int { return 42 }\n',
  ['main.rock'] = 'fn main() { let x = make_foo() }\n',
})
local def_result = project.request(buf, 'textDocument/definition', {
  position = { line = 0, character = 22 },
})

-- LSP allows either Location or Location[]. Normalize to a list.
local locs = def_result
if type(locs) == 'table' and locs.uri then
  locs = { locs }
end
if type(locs) ~= 'table' or #locs == 0 then
  fail('expected Location or non-empty Location[], got ' .. vim.inspect(def_result))
end

local target = locs[1]
if type(target.uri) ~= 'string' or type(target.range) ~= 'table' then
  fail('definition target malformed: ' .. vim.inspect(target))
end
if not target.uri:match('^file://') then
  fail('definition uri not a file URI: ' .. target.uri)
end
if not target.uri:match('/lib%.rock$') then
  fail('expected jump to lib.rock; got: ' .. target.uri)
end

-- The declaration `make_foo` lives at line 0, character 3 of lib.rock
-- (`fn make_foo`). We only assert line for stability — the LSP may
-- return either the ident span or the whole-fn span depending on
-- implementation. Line 0 is unambiguous either way.
if target.range.start.line ~= 0 then
  fail(('expected start.line=0 in lib.rock, got %s'):format(vim.inspect(target.range)))
end

print(('OK: goto-def round-trip jumped main.rock:0:22 -> %s:%d:%d'):format(
  target.uri:match('([^/]+)$') or target.uri,
  target.range.start.line,
  target.range.start.character))
os.exit(0)
