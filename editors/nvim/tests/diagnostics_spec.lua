local project = dofile('tests/_lib/project.lua')

local function assert_range(d)
  assert(d.col ~= d.end_col or d.lnum ~= d.end_lnum,
    'empty diagnostic range: ' .. vim.inspect(d))
end

local cases = {
  {
    name = 'unused_variable',
    source = [[
@artifact_definition(catalog="widgets", key_param="key")
deco init widget(key: string) {}
fn main() {
    let unused_thing = 42
    print("hi")
}
]],
    check = function(diags)
      local saw_warn, saw_unused = false, false
      for _, d in ipairs(diags) do
        saw_warn = saw_warn or d.severity == vim.diagnostic.severity.WARN
        assert(not d.message:find('unused variable `key`', 1, true),
          'decorator metadata parameter was reported unused: ' .. vim.inspect(d))
        if d.message:lower():find('unused', 1, true) then
          saw_unused = true
          assert_range(d)
        end
      end
      assert(saw_warn and saw_unused, 'expected a warning and an unused-variable diagnostic: ' .. vim.inspect(diags))
    end,
  },
  {
    name = 'float_div_hint',
    source = [[
fn main() {
  let h = 8 / 3
  let x: int = 2 * h
}
]],
    count = 2,
    check = function(diags)
      local lines = {}
      for _, d in ipairs(diags) do
        lines[d.lnum] = true
        assert_range(d)
      end
      assert(lines[1] and lines[2], 'expected squiggles at division and use site: ' .. vim.inspect(diags))
    end,
  },
  { name = 'parse_error', source = 'fn foo(\n' },
}

for _, case in ipairs(cases) do
  local buf = project.open(case.name, { ['main.rock'] = case.source })
  local diags
  assert(vim.wait(8000, function()
    diags = vim.diagnostic.get(buf)
    return #diags >= (case.count or 1)
  end, 100), case.name .. ': insufficient buffer diagnostics: ' .. vim.inspect(diags))
  if case.check then
    case.check(diags)
  else
    for _, d in ipairs(diags) do assert_range(d) end
  end
  print('OK: ' .. case.name)
end
