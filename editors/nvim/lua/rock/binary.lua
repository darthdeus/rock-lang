
local state = require('rock.state')
local uv = vim.uv or vim.loop

local M = {}

local function exists(p)
  if not p then return false end
  local st = uv.fs_stat(p)
  return st ~= nil and st.type == 'file'
end

local function find_up(start, marker)
  local dir = start
  for _ = 1, 32 do
    if uv.fs_stat(dir .. '/' .. marker) then return dir end
    local parent = vim.fn.fnamemodify(dir, ':h')
    if parent == dir then return nil end
    dir = parent
  end
  return nil
end

local WORKTREE_MARKER = 'crates/rock-cli/Cargo.toml'
local function find_worktree(start)
  return find_up(start, WORKTREE_MARKER)
end

local function workspace_from_bin(bin)
  if not bin then return nil end
  local m = bin:match('^(.*)/target/[^/]+/rock$')
  return m
end

local function short_id(mtime, size)
  if not mtime or not size then return '--------' end
  return vim.fn.sha256(tostring(mtime) .. ':' .. tostring(size)):sub(1, 8)
end

-- Wrapper scripts may contain the binary path to watch.
function M.resolve(configured)
  if not configured then
    if vim.env.ROCK_LSP_DIR and vim.env.ROCK_LSP_DIR ~= '' then
      configured = vim.env.ROCK_LSP_DIR .. '/target/debug/rock'
    else
      local wt = find_worktree(vim.fn.getcwd())
      local local_bin = wt and (wt .. '/target/debug/rock') or nil
      if local_bin and uv.fs_stat(local_bin) then
        configured = local_bin
      else
        configured = vim.env.HOME .. '/projects/rock-lang/target/debug/rock'
        if not uv.fs_stat(configured) then
          local on_path = vim.fn.exepath('rock')
          if on_path ~= '' then configured = on_path end
        end
      end
    end
  end

  local resolved = configured
  local st = uv.fs_stat(configured)
  if st and st.type == 'file' then
    local fd = uv.fs_open(configured, 'r', 438)
    if fd then
      local data = uv.fs_read(fd, 4096, 0)
      uv.fs_close(fd)
      if data and data:sub(1, 2) == '#!' then
        -- A word or dash after /rock would name a different executable.
        local bin = data:match('([%w%._/%-%$%{%}]+/target/[^/]+/rock)%f[^%w%-]')
        if bin then
          bin = bin:gsub('%$HOME', vim.env.HOME)
          bin = bin:gsub('%$PWD', vim.fn.getcwd())
          resolved = bin
        end
      end
    end
  end
  return vim.fn.resolve(resolved), configured
end

function M.refresh(configured)
  local resolved, cfg = M.resolve(configured)
  local st = uv.fs_stat(resolved)
  local cwd = vim.fn.getcwd()
  local worktree_expected = find_worktree(cwd)
  local worktree_actual = workspace_from_bin(resolved)
  local mismatch = false
  if worktree_expected and worktree_actual and worktree_expected ~= worktree_actual then
    mismatch = true
  end

  state.set_binary({
    path = resolved,
    configured_path = cfg,
    -- Devices and other non-files are not usable binaries.
    exists = st ~= nil and st.type == 'file',
    mtime = st and st.mtime.sec or nil,
    size = st and st.size or nil,
    sha8 = st and short_id(st.mtime.sec, st.size) or nil,
    worktree_expected = worktree_expected,
    worktree_actual = worktree_actual,
    mismatch = mismatch,
  })
end

local fs_handle = nil
local debounce_timer = nil

function M.watch(configured)
  if fs_handle then pcall(fs_handle.stop, fs_handle); pcall(fs_handle.close, fs_handle); fs_handle = nil end
  local resolved = M.resolve(configured)
  if not exists(resolved) then return end
  fs_handle = uv.new_fs_event()
  if not fs_handle then return end
  fs_handle:start(resolved, {}, function(err)
    if err then return end
    -- Debounce: rebuilds often produce several events in quick succession.
    if debounce_timer then debounce_timer:stop(); debounce_timer:close() end
    debounce_timer = uv.new_timer()
    debounce_timer:start(200, 0, vim.schedule_wrap(function()
      debounce_timer:stop(); debounce_timer:close(); debounce_timer = nil
      M.refresh(configured)
      state.record_event('local', 'binary/changed', { path = resolved })
    end))
  end)
end

function M.stop_watch()
  if fs_handle then pcall(fs_handle.stop, fs_handle); pcall(fs_handle.close, fs_handle); fs_handle = nil end
  if debounce_timer then pcall(debounce_timer.stop, debounce_timer); pcall(debounce_timer.close, debounce_timer); debounce_timer = nil end
end

return M
