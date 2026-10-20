-- Child-nvim harness for tests that need a real event loop, real
-- mode transitions, and a real popup menu.
--
-- WHY THIS EXISTS
--
-- `nvim --headless -l script.lua` runs the script on the main thread
-- and never enters a key-driven event loop. `feedkeys('a','tx')`,
-- `nvim_input('a')`, `startinsert!` — none of them switch `mode()`
-- to `i`. `complete()` (the API the LSP omnifunc ultimately calls)
-- is a no-op outside insert mode, so the popup never populates and
-- `complete_info()` returns an empty table. Response-level specs
-- (`buf_request -> assert payload`) work fine in that environment;
-- popup-level specs do not.
--
-- This module spawns a child nvim with `--embed` over jobstart RPC.
-- The host script becomes the UI, the child has its own event loop
-- and processes input the way a normal terminal nvim would. Mode
-- transitions are real, autotriggers fire, `complete_info()` returns
-- the popup the user would actually see.
--
-- Geometry note: `nvim_ui_attach` is required. Without an attached
-- UI the child will buffer everything and never run the loop.
--
-- Usage:
--
--   local NvimChild = require('_lib.nvim_child')
--   local child = NvimChild.start()
--   child:exec_lua([[ ... set up rock client ... ]], { rock_lsp_bin })
--   child:cmd('edit ' .. path)
--   child:wait_for_lsp_attach('rock', 5000)
--   child:input('a<C-x><C-o>')
--   child:wait_for(function() return child:eval('pumvisible()') == 1 end, 3000)
--   local items = child:call('complete_info', { { 'items' } }).items
--   ...
--   child:stop()

local M = {}

local Child = {}
Child.__index = Child

--- Start a child nvim. `opts.rtp` adds entries to the child's
--- runtimepath (typical: this plugin's editors/nvim dir). `opts.size`
--- = { width, height }. `opts.args` extends the spawn command line.
function M.start(opts)
  opts = opts or {}
  local self = setmetatable({}, Child)

  self.stderr = {}
  -- `_argv` (not `cmd`) so the field doesn't shadow `Child:cmd` —
  -- `self:cmd(...)` resolves through the instance first, and a list
  -- there means `attempt to call method 'cmd' (a table value)`.
  self._argv = vim.list_extend({ vim.v.progpath, '--embed', '--headless', '--clean' },
    opts.args or {})

  self.jobid = vim.fn.jobstart(self._argv, {
    rpc = true,
    on_stderr = function(_, data)
      for _, l in ipairs(data) do
        if l ~= '' then table.insert(self.stderr, l) end
      end
    end,
  })
  if self.jobid <= 0 then
    error('nvim_child: jobstart failed (' .. tostring(self.jobid) .. ')')
  end

  local size = opts.size or { 80, 24 }
  self:rpc('nvim_ui_attach', size[1], size[2],
    { rgb = false, ext_popupmenu = false })

  -- Quiet noisy notifications so they don't pollute test output.
  self:cmd('lua vim.notify = function() end')
  self:cmd('lua vim.notify_once = function() end')

  for _, p in ipairs(opts.rtp or {}) do
    self:cmd('set rtp+=' .. vim.fn.fnameescape(p))
  end

  return self
end

--- Raw RPC request to the child.
function Child:rpc(method, ...)
  return vim.rpcrequest(self.jobid, method, ...)
end

--- Send keys via `nvim_input`. Termcodes (`<C-x>`, `<Esc>`, etc.) are
--- recognized by the child. Returns immediately; pair with
--- `wait_for` to assert on resulting state.
function Child:input(keys)
  return self:rpc('nvim_input', keys)
end

--- Run an Ex command in the child.
function Child:cmd(cmd)
  return self:rpc('nvim_command', cmd)
end

--- Evaluate a vimscript expression. Useful for `pumvisible()`, etc.
function Child:eval(expr)
  return self:rpc('nvim_eval', expr)
end

--- Call a vimscript function with args (a list).
function Child:call(fn, args)
  return self:rpc('nvim_call_function', fn, args or {})
end

--- Run a chunk of Lua in the child. `chunk` should be a string; `args`
--- is forwarded as `...`. Returns whatever the chunk returns.
function Child:exec_lua(chunk, args)
  return self:rpc('nvim_exec_lua', chunk, args or {})
end

--- Spin until `predicate()` returns truthy or `timeout_ms` elapses.
--- `predicate` is called from the host; typically it `:eval`s or
--- `:exec_lua`s into the child. Returns true on success, false on
--- timeout. Polls every `interval_ms` (default 25).
function Child:wait_for(predicate, timeout_ms, interval_ms)
  timeout_ms = timeout_ms or 3000
  interval_ms = interval_ms or 25
  local deadline = vim.uv.now() + timeout_ms
  while vim.uv.now() < deadline do
    if predicate() then return true end
    vim.wait(interval_ms)
  end
  return false
end

--- Wait until a client named `client_name` is attached in the child.
--- Returns the client id on success, raises on timeout.
---
--- Polls `vim.lsp.get_clients()` directly rather than relying on an
--- LspAttach autocmd: the docker harness can race past LspAttach
--- before this function runs (sub-250ms attach), and a freshly-
--- registered autocmd never sees past events. Also: `nvim_exec_lua`
--- returns Lua `nil` as `vim.NIL` (userdata), and `vim.NIL ~= nil`
--- is true — so a naive `~= nil` predicate succeeds vacuously.
function Child:wait_for_lsp_attach(client_name, timeout_ms)
  timeout_ms = timeout_ms or 5000
  local cid
  local ok = self:wait_for(function()
    local v = self:exec_lua([[
      local name = ...
      for _, c in ipairs(vim.lsp.get_clients()) do
        if c.name == name then return c.id end
      end
      return nil
    ]], { client_name })
    if v ~= nil and v ~= vim.NIL then
      cid = v
      return true
    end
    return false
  end, timeout_ms)
  if not ok then
    error(('nvim_child: client %q did not attach within %dms\nstderr: %s')
      :format(client_name, timeout_ms, table.concat(self.stderr, '\n')))
  end
  return cid
end

--- Stop the child. Idempotent.
function Child:stop()
  if self.jobid and self.jobid > 0 then
    pcall(vim.fn.jobstop, self.jobid)
    self.jobid = -1
  end
end

return M
