-- The `completion_record` option must resolve to the ROCK_LSP_RECORD env
-- the server is spawned with (the server-side corpus recorder's gate).
-- `completion_record_env` is the pure resolver build_cmd passes to
-- vim.lsp.rpc.start; spec pins: off -> nil, on -> default corpus path
-- under stdpath('data'), explicit path -> that path.

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end
local function assert_eq(actual, expected, what)
  if actual ~= expected then
    fail(('%s: expected %s, got %s'):format(what, vim.inspect(expected), vim.inspect(actual)))
  end
end

vim.notify = function() end
vim.notify_once = function() end

local client = require('rock.client')

assert_eq(client.completion_record_env({}), nil, 'off by default')
assert_eq(client.completion_record_env({ completion_record = false }), nil, 'explicit off')

local env = client.completion_record_env({ completion_record = true })
if type(env) ~= 'table' then fail('on -> env table, got ' .. vim.inspect(env)) end
assert_eq(env.ROCK_LSP_RECORD,
  vim.fn.stdpath('data') .. '/rock/completion-corpus.jsonl',
  'default corpus path under stdpath(data)')

local custom = client.completion_record_env({
  completion_record = true,
  completion_record_path = '/tmp/custom.jsonl',
})
assert_eq(custom.ROCK_LSP_RECORD, '/tmp/custom.jsonl', 'explicit path wins')

print('PASS  completion_record_env_spec')
os.exit(0)
