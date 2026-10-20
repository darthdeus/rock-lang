-- Edge-case inputs to make_config / setup that could plausibly hit
-- the plugin in the wild:
--   * binary = "" (user typoed config)
--   * binary = "/dev/null" (exists, not executable, not a regular file)
--   * make_config called twice in a row (idempotent re-config)
--
-- None of these should crash the plugin or the cmd() path on the
-- first buffer. They should either bail at the missing-binary gate or
-- be harmlessly absorbed.

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

vim.notify = function() end
vim.notify_once = function() end
vim.filetype.add({ extension = { rock = 'rock' } })

local client = require('rock.client')
local state = require('rock.state')

-- Phase 1: empty binary string. Should resolve via the fallback chain
-- ($ROCK_LSP_DIR or $HOME default) and likely end up missing — that's
-- fine; we only assert no exception thrown.
do
  local ok, err = pcall(client.make_config, { binary = '' })
  if not ok then fail('make_config({binary=""}) raised: ' .. tostring(err)) end
end

-- Phase 2: /dev/null exists as a character device, not a regular
-- file. binary.lua's exists() check requires st.type == 'file', so
-- /dev/null should NOT count as exists (correct behavior — it's not
-- executable as a binary).
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

-- Phase 3: double make_config. The second call must NOT leave stale
-- watch handles or stale cmd closures pointing at the first binary.
-- We inspect via state — second call should overwrite binary info.
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

-- Phase 4: make_config({}) — no binary key at all. Should resolve via
-- env or $HOME default, never crash.
do
  local ok, cfg_or_err = pcall(client.make_config, {})
  if not ok then fail('make_config({}) raised: ' .. tostring(cfg_or_err)) end
end

-- Phase 5: stop_watch can be called when no watch is active. (This
-- runs as cleanup at the end of every spec; it must be a no-op when
-- nothing was started.)
do
  local ok, err = pcall(require('rock.binary').stop_watch)
  if not ok then fail('stop_watch() with no active watch raised: ' .. tostring(err)) end
  -- Calling it twice should also be safe.
  local ok2, err2 = pcall(require('rock.binary').stop_watch)
  if not ok2 then fail('stop_watch() second call raised: ' .. tostring(err2)) end
end

print('OK: empty binary, /dev/null, double-config, no-binary, idempotent stop_watch')
os.exit(0)
