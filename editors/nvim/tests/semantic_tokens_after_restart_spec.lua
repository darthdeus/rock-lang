-- Some Neovim versions do not automatically refresh tokens after reattach.

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

local project = dofile('tests/_lib/project.lua')
local buf, attached = project.open('semtok_restart', { ['main.rock'] = 'fn main() {\n    let x = 42\n}\n' })

local client = require('rock.client')
local state = require('rock.state')

local function semtok_keys()
  local rt = state.get().request_timings
  local keys = {}
  for k in pairs(rt) do
    if k:match('^textDocument/semanticTokens') then table.insert(keys, k) end
  end
  return keys
end

local function semtok_total()
  local n = 0
  for _, k in ipairs(semtok_keys()) do
    n = n + #(state.get().request_timings[k].samples or {})
  end
  return n
end

if not vim.wait(4000, function() return semtok_total() > 0 end, 50) then
  fail('initial semanticTokens request never captured before restart')
end
local pre_restart = semtok_total()
local first_id = attached.id
client.restart()
local attached_id = project.attached(buf, first_id).id

-- Phase 3: assert at least one ADDITIONAL semanticTokens request
-- fires after the restart. client.lua:328 force_refreshes 300ms
-- after the buffer re-attaches, so allow up to 4s.
if not vim.wait(4000, function() return semtok_total() > pre_restart end, 50) then
  io.stderr:write('--- semtok keys after restart ---\n')
  for _, k in ipairs(semtok_keys()) do
    io.stderr:write(('  %s -> %d samples\n'):format(k,
      #(state.get().request_timings[k].samples or {})))
  end
  fail(('post-restart semanticTokens never re-fired (pre=%d post=%d)')
    :format(pre_restart, semtok_total()))
end

print(('OK: semanticTokens re-fired after restart (pre=%d post=%d, client %d -> %d)')
  :format(pre_restart, semtok_total(), first_id, attached_id))
os.exit(0)
