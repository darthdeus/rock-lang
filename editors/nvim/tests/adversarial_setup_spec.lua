
local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

vim.notify = function() end
vim.notify_once = function() end
vim.filetype.add({ extension = { rock = 'rock' } })

local client = require('rock.client')
local state = require('rock.state')

do
  local ok, err = pcall(client.make_config, { binary = '' })
  if not ok then fail('make_config({binary=""}) raised: ' .. tostring(err)) end
end

-- A character device is not a usable executable file.
do
  local cfg = client.make_config({ binary = '/dev/null' })
  local info = state.get().binary
  if info.exists then
    fail('/dev/null reported as exists=true; binary.exists() check is wrong about non-files')
  end
  if type(cfg) ~= 'table' or type(cfg.cmd) ~= 'function' then
    fail('/dev/null make_config did not produce a usable config table')
  end
end

-- Reconfiguration must discard stale watchers and command closures.
do
  local first = '/tmp/adversarial-spec-first-' .. os.time()
  local second = '/tmp/adversarial-spec-second-' .. os.time()
  client.make_config({ binary = first })
  local info1 = state.get().binary
  if info1.configured_path ~= first then
    fail('first make_config did not set configured_path: ' .. vim.inspect(info1))
  end
  client.make_config({ binary = second })
  local info2 = state.get().binary
  if info2.configured_path ~= second then
    fail('second make_config did not overwrite configured_path: got '
      .. tostring(info2.configured_path))
  end
end

do
  local ok, cfg_or_err = pcall(client.make_config, {})
  if not ok then fail('make_config({}) raised: ' .. tostring(cfg_or_err)) end
end

do
  local ok, err = pcall(require('rock.binary').stop_watch)
  if not ok then fail('stop_watch() with no active watch raised: ' .. tostring(err)) end
  local ok2, err2 = pcall(require('rock.binary').stop_watch)
  if not ok2 then fail('stop_watch() second call raised: ' .. tostring(err2)) end
end

print('OK: empty binary, /dev/null, double-config, no-binary, idempotent stop_watch')
os.exit(0)
