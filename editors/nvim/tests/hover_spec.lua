-- Hover wording is owned by the server; this test checks the request and instrumentation.

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

local project = dofile('tests/_lib/project.lua')
local state = require('rock.state')
local buf = project.open('hover', { ['main.rock'] = 'fn main() {\n    let x = 42\n}\n' })
local hover_result = project.request(buf, 'textDocument/hover', {
  position = { line = 0, character = 3 },
})

-- Verify the timing ring captured this call. record_request_timing is
-- driven from client.lua's wrapped orig_request callback; if the
-- wrapper got dropped, this would be empty.
local samples = (state.get().request_timings['textDocument/hover'] or {}).samples
if not samples or #samples < 1 then
  fail('expected at least one textDocument/hover sample in request_timings; got '
    .. tostring(samples and #samples or 0))
end

print(('OK: hover round-trip completed (latency=%.2fms, result=%s)')
  :format(samples[1], hover_result and 'non-nil' or 'nil'))
os.exit(0)
