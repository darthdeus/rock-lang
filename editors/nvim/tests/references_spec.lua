local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

local project = dofile('tests/_lib/project.lua')
local buf = project.open('refs', {
  ['lib.rock'] = 'fn make_foo() -> int { return 42 }\n',
  ['main.rock'] = 'fn main() { let x = make_foo() }\n',
}, 'lib.rock')
local refs_result = project.request(buf, 'textDocument/references', {
  position = { line = 0, character = 3 },
  context = { includeDeclaration = true },
})

if type(refs_result) ~= 'table' or #refs_result == 0 then
  fail('expected non-empty Location[] array, got ' .. vim.inspect(refs_result))
end

-- Validate every entry has the expected shape, and bucket by basename.
-- We assert the cross-module case specifically: at least one Location
-- in lib.rock (the decl) and at least one in main.rock (the call site).
local in_lib, in_main = false, false
for i, loc in ipairs(refs_result) do
  if type(loc) ~= 'table' or type(loc.uri) ~= 'string' or type(loc.range) ~= 'table' then
    fail('Location[' .. i .. '] malformed: ' .. vim.inspect(loc))
  end
  if not loc.uri:match('^file://') then
    fail('Location[' .. i .. '].uri not a file URI: ' .. loc.uri)
  end
  if loc.uri:match('/lib%.rock$') then in_lib = true end
  if loc.uri:match('/main%.rock$') then in_main = true end
end

if not in_lib then
  fail('expected a reference in lib.rock (decl); got: ' .. vim.inspect(refs_result))
end
if not in_main then
  fail('expected a reference in main.rock (cross-module call site); got: '
    .. vim.inspect(refs_result))
end

print(('OK: references round-trip returned %d locations across lib.rock + main.rock')
  :format(#refs_result))
os.exit(0)
