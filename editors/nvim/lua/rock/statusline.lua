-- Compact status-line segment suitable for lualine or native statusline.
--
-- Example lualine:
--   lualine_x = { { require('rock.statusline').component,
--                   cond = function() return vim.bo.filetype == 'rock' end } }

local state = require('rock.state')

local M = {}

local DOT = {
  attached = '%#DiagnosticOk#*%*',
  starting = '%#DiagnosticWarn#*%*',
  detached = '%#Comment#.%*',
  crashed  = '%#DiagnosticError#x%*',
}

function M.component()
  local s = state.get()
  local dot = DOT[s.client.state] or DOT.detached
  local up = '-'
  if s.client.started_at and s.client.state == 'attached' then
    local d = os.time() - s.client.started_at
    if d < 60 then up = d .. 's'
    elseif d < 3600 then up = math.floor(d / 60) .. 'm'
    else up = math.floor(d / 3600) .. 'h' end
  end
  local sha = s.binary.sha8 or '--------'
  local mm = s.binary.mismatch and '%#DiagnosticError#!%*' or ''
  return ('%s rock %s%s up %s r%d'):format(dot, sha, mm, up, s.client.restart_count)
end

-- Plain string version (no statusline markup) for non-lualine users.
function M.plain()
  local s = state.get()
  return ('rock[%s] %s up=%s r=%d%s'):format(
    s.client.state or '-',
    s.binary.sha8 or '--------',
    s.client.started_at and tostring(os.time() - s.client.started_at) .. 's' or '-',
    s.client.restart_count,
    s.binary.mismatch and ' MISMATCH' or ''
  )
end

return M
