-- Real-LSP completion request. Beyond verifying a response comes
-- back, this drives the `is_completion` branch in client.lua's
-- wrapped rpc.request — the one that populates state.last_completion
-- (the floater's data source).

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

local project = dofile('tests/_lib/project.lua')
local state = require('rock.state')
local bufnr = project.open('comp', {
  ['main.rock'] = 'fn main() {\n    let xyz = 1\n    let abc = x\n}\n',
})
project.request(bufnr, 'textDocument/completion', {
  position = { line = 2, character = 15 },
  context = { triggerKind = 1 },
})

-- The wrapped rpc.request in client.lua has an `is_completion` branch
-- that records a snapshot into state.last_completion. Verify it.
local lc = state.get().last_completion
if not lc then
  fail('state.last_completion was not populated by completion handler')
end
if type(lc.item_count) ~= 'number' then
  fail('state.last_completion.item_count is not a number: ' .. vim.inspect(lc))
end
if type(lc.latency_ms) ~= 'number' then
  fail('state.last_completion.latency_ms is not a number')
end
if lc.uri ~= vim.uri_from_bufnr(bufnr) then
  fail(('state.last_completion.uri mismatch: got %s expected %s')
    :format(tostring(lc.uri), vim.uri_from_bufnr(bufnr)))
end

print(('OK: completion captured (items=%d, latency=%.2fms, incomplete=%s)')
  :format(lc.item_count, lc.latency_ms, tostring(lc.is_incomplete)))
os.exit(0)
