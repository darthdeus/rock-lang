local M = {}

local function write(path, text)
  local file = assert(io.open(path, 'w'))
  assert(file:write(text))
  assert(file:close())
end

function M.open(name, sources, entry)
  local binary = vim.env.ROCK_LSP_BIN
  if not binary or binary == '' then
    print('SKIP: $ROCK_LSP_BIN not set')
    os.exit(0)
  end
  assert(vim.fn.executable(binary) == 1, 'ROCK_LSP_BIN is not executable: ' .. binary)

  vim.notify = function() end
  vim.notify_once = vim.notify
  vim.filetype.add({ extension = { rock = 'rock' } })
  local root = vim.fn.tempname()
  vim.fn.mkdir(root, 'p')
  write(root .. '/stick.toml', ('[package]\nname = "%s"\nversion = "0.1.0"\n'):format(name))
  for filename, source in pairs(sources) do
    write(root .. '/' .. filename, source)
  end

  local plugin = require('rock.client')
  vim.lsp.config('rock', plugin.make_config({ binary = binary }))
  plugin.setup_autocmds()
  vim.lsp.enable('rock')
  vim.cmd.edit(vim.fn.fnameescape(root .. '/' .. (entry or 'main.rock')))

  local buf = vim.api.nvim_get_current_buf()
  local client
  assert(vim.wait(8000, function()
    client = vim.lsp.get_clients({ bufnr = buf, name = 'rock' })[1]
    return client ~= nil
  end, 25), 'rock did not attach to ' .. vim.api.nvim_buf_get_name(buf))
  return buf, client
end

function M.request(buf, method, params)
  params = params or {}
  params.textDocument = { uri = vim.uri_from_bufnr(buf) }
  local done, failure, result
  local requests = vim.lsp.buf_request(buf, method, params, function(err, reply)
    failure, result, done = err, reply, true
  end)
  assert(requests and next(requests), 'no client accepted ' .. method)
  assert(vim.wait(5000, function() return done end, 25), method .. ' did not reply')
  assert(not failure, method .. ': ' .. vim.inspect(failure))
  return result
end

return M
