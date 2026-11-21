-- nvim-cmp defaults to score before sort_text, hiding the server ranking.

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

local client = require('rock.client')
local reorder = client._reorder_sort_text_before_score
if type(reorder) ~= 'function' then
  fail('rock.client._reorder_sort_text_before_score not exported')
end

-- Use distinct sentinel values for the comparator refs.
local offset, exact, score, sort_text, kind = 'offset', 'exact', 'score', 'sort_text', 'kind'

local function eq(a, b, where)
  if #a ~= #b then
    fail(where .. ': length ' .. #a .. ' ~= ' .. #b)
  end
  for i = 1, #a do
    if a[i] ~= b[i] then
      fail(where .. ': index ' .. i .. ' got ' .. tostring(a[i]) .. ' want ' .. tostring(b[i]))
    end
  end
end

-- Default cmp order: sort_text after score -> moved to just before score.
eq(
  reorder({ offset, exact, score, sort_text, kind }, sort_text, score),
  { offset, exact, sort_text, score, kind },
  'sort_text moved before score'
)

-- Already before score -> unchanged.
eq(
  reorder({ offset, exact, sort_text, score, kind }, sort_text, score),
  { offset, exact, sort_text, score, kind },
  'already ordered stays'
)

-- No score comparator -> sort_text goes to the front.
eq(
  reorder({ offset, exact, kind, sort_text }, sort_text, score),
  { sort_text, offset, exact, kind },
  'no score -> front'
)

print('OK: cmp sort_text reorder')
os.exit(0)
