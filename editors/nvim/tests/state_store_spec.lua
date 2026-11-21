-- Request timing intentionally does not notify subscribers on every event.

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

local state = require('rock.state')

local hits = 0
local unsub = state.subscribe(function() hits = hits + 1 end)
state.set_binary({ path = '/tmp/x', exists = true })
if hits ~= 1 then fail('set_binary should notify once, got ' .. hits) end
state.set_client({ state = 'starting' })
if hits ~= 2 then fail('set_client should notify, got ' .. hits) end
unsub()
state.set_binary({ path = '/tmp/y', exists = false })
if hits ~= 2 then fail('unsubscribed callback still fired, hits=' .. hits) end

local timing_hits = 0
local unsub2 = state.subscribe(function() timing_hits = timing_hits + 1 end)
for i = 1, 50 do
  state.record_request_timing('textDocument/completion', i * 1.0)
end
if timing_hits ~= 0 then
  fail('record_request_timing fired ' .. timing_hits .. ' notifications; should be 0')
end
local samples = state.get().request_timings['textDocument/completion'].samples
if #samples ~= 50 then fail('expected 50 samples, got ' .. #samples) end
unsub2()

local cleared = 0
local unsub3 = state.subscribe(function() cleared = cleared + 1 end)
state.clear_request_timings()
if cleared ~= 1 then fail('clear_request_timings should notify, got ' .. cleared) end
if next(state.get().request_timings) ~= nil then
  fail('clear_request_timings did not clear table')
end
unsub3()

state.clear_events()
for i = 1, 1500 do
  state.record_event('local', 'spec/insert', { i = i })
end
local events = state.get().events
if #events < 1000 or #events > 1000 + 256 then
  fail(('event ring out of bounds: %d (expected 1000..1256)'):format(#events))
end
local newest = events[#events]
if not newest or newest.payload.i ~= 1500 then
  fail('newest event lost in compaction: ' .. vim.inspect(newest))
end
local oldest = events[1]
if not oldest or oldest.payload.i < 244 then
  fail('compaction kept events older than slack window: i=' .. tostring(oldest and oldest.payload.i))
end

for i = 1, 60 do
  state.record_error(1, 'window/showMessage', 'err ' .. i)
end
local errs = state.get().errors
if #errs ~= 50 then fail('errors ring should be 50, got ' .. #errs) end
if errs[1].message ~= 'err 60' then
  fail('errors ring is not newest-first: front=' .. tostring(errs[1].message))
end
state.clear_errors()
if #state.get().errors ~= 0 then fail('clear_errors did not empty ring') end

print('OK: state store pub/sub, timing skip-notify, ring bounds')
os.exit(0)
