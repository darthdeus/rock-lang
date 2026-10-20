-- Tail nvim's LSP log file and feed lines into state.
--
-- We read incrementally from the last known offset on each fs_event
-- fire. Lines are filtered so only ones that look rock-related end up
-- in the panel (matching 'rock' client id or the server name). The
-- full log remains on disk; state keeps a bounded ring for display.

local state = require('rock.state')
local uv = vim.uv or vim.loop

local M = {}

local HANDLE = nil
local PATH = nil
local OFFSET = 0
local CLIENT_ID_RE = nil  -- set when we know the rock client id

-- Patterns nvim's own log uses. Typical lines:
--   [INFO][2025-04-21 14:22:03] ...rpc.lua:123    "LSP[rock]" "..."
--   [START][2025-04-21 14:22:01] LSP logging initiated
-- We match the quoted "LSP[rock]" tag or an explicit client_id suffix.
local function is_relevant(line)
  if line:find('"LSP%[rock%]"', 1, false) then return true end
  if CLIENT_ID_RE and line:find(CLIENT_ID_RE) then return true end
  return false
end

local function drain()
  if not PATH then return end
  local st = uv.fs_stat(PATH)
  if not st then return end
  -- If the log was rotated/truncated below our offset, reset.
  if st.size < OFFSET then OFFSET = 0 end
  if st.size == OFFSET then return end

  local fd = uv.fs_open(PATH, 'r', 438)
  if not fd then return end
  local want = st.size - OFFSET
  -- Guardrail: don't slurp more than 1 MiB per tick.
  if want > 1024 * 1024 then OFFSET = st.size - 1024 * 1024; want = 1024 * 1024 end
  local data = uv.fs_read(fd, want, OFFSET)
  uv.fs_close(fd)
  if not data or #data == 0 then return end
  OFFSET = OFFSET + #data

  for line in data:gmatch('[^\r\n]+') do
    if is_relevant(line) then
      if #line > 500 then line = line:sub(1, 500) .. '...' end
      state.record_log(line)
    end
  end
end

function M.start()
  PATH = vim.lsp.get_log_path()
  if not PATH then return end
  local st = uv.fs_stat(PATH)
  -- Start from current end-of-file so we don't flood the panel with old
  -- lines on startup. Only new activity matters.
  OFFSET = st and st.size or 0
  HANDLE = uv.new_fs_event()
  if not HANDLE then return end
  HANDLE:start(PATH, {}, vim.schedule_wrap(function(err) if not err then drain() end end))
end

function M.stop()
  if HANDLE then pcall(HANDLE.stop, HANDLE); pcall(HANDLE.close, HANDLE); HANDLE = nil end
end

function M.set_client_id(id)
  if id then CLIENT_ID_RE = 'client_id=' .. tostring(id) end
end

return M
