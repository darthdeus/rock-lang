local state = require('rock.state')
local stats = require('rock.stats')
local timers, replies, sent = {}, {}, 0
local clients = {}

for id = 1, 2 do
  clients[id] = {
    id = id,
    request = function(_, method, _, callback)
      assert(method == 'rock/stats')
      sent = sent + 1
      replies[id] = callback
      return true, sent
    end,
  }
end

vim.lsp.get_clients = function() return clients end
vim.lsp.get_client_by_id = function(id) return clients[id] end
vim.uv.new_timer = function()
  return {
    start = function(_, _, _, callback) timers[#timers + 1] = callback end,
    stop = function() end,
    close = function() end,
  }
end

stats.start()
timers[1]()
assert(vim.wait(200, function() return replies[2] ~= nil end))
local detached
stats.request(clients[1], 'rock/stats', function(_, _, _, failure) detached = failure end)
assert(sent == 2, 'a manual refresh duplicated an in-flight poll')

replies[2](nil, { total_modules = 9 })
clients[1] = nil
state.forget_client(1)
replies[1](nil, { total_modules = 4 })
assert(not state.get().stats[1], 'a late reply restored a detached client')
assert(detached == 'client detached')
assert(state.get().stats[2].total_modules == 9)
stats.stop()
print('OK: refreshes share requests and detached snapshots stay cleared')
vim.cmd('qa!')
