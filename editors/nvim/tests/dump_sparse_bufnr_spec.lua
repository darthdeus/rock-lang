-- :RockDump must not blow up when state has a high/sparse integer key.
--
-- Repro: `state.buffers` is keyed by bufnr, `stats`/`modules` by
-- client_id. After a long session those integers get large while only
-- one or two are tracked, so the table is an "excessively sparse
-- array" to vim.json.encode -> dump.write notifies an error and
-- returns nil. Field report: ":RockDump on bindings.rock failed to
-- encode snapshot: Cannot serialise table: excessively sparse array".
--
-- This spec needs no LSP binary; it drives state + dump directly.

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

local state = require('rock.state')
local dump = require('rock.dump')

-- Simulate a long session: a high bufnr and a high client_id, each the
-- sole entry in their map (maximally sparse).
state.set_buffer(1247, { attached = true, name = '/x/bindings.rock' })
state.set_stats(31, { fps = 60 })
state.set_modules(31, { count = 3 })

local path = os.tmpname()
os.remove(path)  -- tmpname creates it; dump.write wants to create fresh

local out = dump.write(path)
if out == nil then
  fail('dump.write returned nil -- snapshot encode failed on sparse '
    .. 'integer-keyed map (buffers/stats/modules)')
end

-- Sanity: file exists, is valid JSON, and the high keys survived as
-- object keys (stringified), not lost to array coercion.
local fd = assert(io.open(path, 'r'))
local body = fd:read('*a'); fd:close()
local ok, decoded = pcall(vim.json.decode, body)
if not ok then fail('dump wrote invalid JSON: ' .. tostring(decoded)) end
if not (decoded.buffers and decoded.buffers['1247']) then
  fail('bufnr 1247 missing from decoded.buffers (key not preserved)')
end
if not (decoded.stats and decoded.stats['31']) then
  fail('client_id 31 missing from decoded.stats')
end

os.remove(path)
print('OK: RockDump encodes sparse integer-keyed maps')
os.exit(0)
