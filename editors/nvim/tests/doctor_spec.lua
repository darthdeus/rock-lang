-- doctor.lua coverage. The :RockLspDoctor command is the user-facing
-- "is everything OK?" check. We drive M.run() directly (skipping the
-- floating-window UI) and assert the synchronous probes return
-- structured results without erroring.
--
-- The async probes (rock/stats, rock/modules, rock/consistency)
-- require a real LSP. With no client attached, the consistency probe
-- short-circuits via the "no rock client" branch — we accept that and
-- only assert on the sync portion of the result list.

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

vim.notify = function() end
vim.notify_once = function() end

-- doctor.run pulls binary state from rock.state. Seed it with a known
-- shape so check_binary has something to assert against.
require('rock.binary').refresh('/tmp/rock-lsp-doctor-spec-' .. os.time())

local doctor = require('rock.doctor')
local got_results
doctor.run(function(results) got_results = results end)

-- doctor.run does sync checks immediately and emits placeholders for
-- async probes; the on_done callback fires from the consistency
-- probe's "no client" branch, which schedules its callback. Drain.
vim.wait(500, function() return got_results ~= nil end, 25)

if not got_results then fail('doctor.run did not call on_done within 500ms') end
if type(got_results) ~= 'table' then fail('on_done received non-table results') end
if #got_results < 6 then
  fail(('expected at least 6 result rows, got %d'):format(#got_results))
end

-- Every row must have status + msg fields. Status should be a known
-- glyph token: PASS / FAIL / WARN / ----.
local known = { PASS = true, FAIL = true, WARN = true, ['----'] = true }
for i, r in ipairs(got_results) do
  if type(r) ~= 'table' then fail(('row %d is not a table: %s'):format(i, type(r))) end
  if type(r.status) ~= 'string' then
    fail(('row %d has non-string status: %s'):format(i, vim.inspect(r.status)))
  end
  if not known[r.status] then
    fail(('row %d has unknown status %q'):format(i, r.status))
  end
  if type(r.msg) ~= 'string' or r.msg == '' then
    fail(('row %d has invalid msg: %s'):format(i, vim.inspect(r.msg)))
  end
end

-- state.doctor must have been written.
local state = require('rock.state').get()
if state.doctor ~= got_results then
  fail('state.doctor was not set to the same results table')
end
if type(state.doctor_at) ~= 'number' then
  fail('state.doctor_at not populated')
end

print(('OK: doctor.run produced %d rows, all with valid status/msg'):format(#got_results))
os.exit(0)
