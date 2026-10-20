-- Floating window showing the raw textDocument/completion response
-- from rock-lsp. This is the ground-truth "what did the server send"
-- view -- useful when nvim-cmp appears to show the wrong items and you
-- want to know whether it's a server bug or a client-side filter bug.

local state = require('rock.state')

local M = {}

-- LSP CompletionItemKind -> short label. Numbers come from the LSP spec.
local KIND = {
  [1] = 'Text', [2] = 'Method', [3] = 'Function', [4] = 'Constructor',
  [5] = 'Field', [6] = 'Variable', [7] = 'Class', [8] = 'Interface',
  [9] = 'Module', [10] = 'Property', [11] = 'Unit', [12] = 'Value',
  [13] = 'Enum', [14] = 'Keyword', [15] = 'Snippet', [16] = 'Color',
  [17] = 'File', [18] = 'Reference', [19] = 'Folder', [20] = 'EnumMember',
  [21] = 'Constant', [22] = 'Struct', [23] = 'Event', [24] = 'Operator',
  [25] = 'TypeParameter',
}

local TRIGGER_KIND = { [1] = 'Invoked', [2] = 'TriggerChar', [3] = 'IncompleteReTrigger' }

local FLOAT_BUF = nil
local FLOAT_WIN = nil

local function short_uri(u)
  if not u then return '-' end
  return vim.fn.fnamemodify(u:gsub('^file://', ''), ':t')
end

local function render(lc)
  local lines = {}
  local function push(s) table.insert(lines, s) end

  if not lc then
    push(' no completion captured yet')
    push(' (trigger one in a rock buffer first)')
    return lines
  end

  local pos = lc.position
  local loc = pos and (('%s:%d:%d'):format(short_uri(lc.uri), pos.line + 1, pos.character + 1))
    or '-'
  local trig = lc.trigger_char and ("'" .. lc.trigger_char .. "'")
    or (TRIGGER_KIND[lc.trigger_kind] or '-')
  local lat = lc.latency_ms and ('%.2fms'):format(lc.latency_ms) or '-'

  push((' LAST COMPLETION  %s'):format(lc.wall))
  push(('   location : %s'):format(loc))
  push(('   trigger  : %s   kind=%s'):format(trig, TRIGGER_KIND[lc.trigger_kind] or '-'))
  push(('   latency  : %s   items=%d%s'):format(lat, lc.item_count,
    lc.is_incomplete and '  (isIncomplete)' or ''))
  push('')
  push(string.rep('-', 78))
  push((' %-32s %-12s %s'):format('LABEL', 'KIND', 'DETAIL'))
  push(string.rep('-', 78))

  local items = lc.items or {}
  for i, it in ipairs(items) do
    if i > 200 then push(('  ... %d more'):format(#items - 200)); break end
    if type(it) == 'table' then
      local label = tostring(it.label or '?')
      local kind = KIND[it.kind] or (it.kind and tostring(it.kind)) or '-'
      local detail = tostring(it.detail or '')
      if #label > 32 then label = label:sub(1, 29) .. '...' end
      if #detail > 40 then detail = detail:sub(1, 37) .. '...' end
      push((' %-32s %-12s %s'):format(label, kind, detail))
    end
  end

  if #items == 0 then push(' (no items)') end
  return lines
end

function M.is_open()
  return FLOAT_WIN and vim.api.nvim_win_is_valid(FLOAT_WIN)
end

function M.close()
  if M.is_open() then pcall(vim.api.nvim_win_close, FLOAT_WIN, true) end
  FLOAT_WIN = nil
end

function M.open()
  if M.is_open() then M.close(); return end
  if not FLOAT_BUF or not vim.api.nvim_buf_is_valid(FLOAT_BUF) then
    FLOAT_BUF = require('rock.ui').ensure_named_buffer('rock://completion')
    vim.keymap.set('n', 'q', M.close, { buffer = FLOAT_BUF, silent = true })
    vim.keymap.set('n', '<esc>', M.close, { buffer = FLOAT_BUF, silent = true })
  end

  local lc = state.get().last_completion
  local lines = render(lc)
  vim.api.nvim_set_option_value('modifiable', true, { buf = FLOAT_BUF })
  vim.api.nvim_buf_set_lines(FLOAT_BUF, 0, -1, false, lines)
  vim.api.nvim_set_option_value('modifiable', false, { buf = FLOAT_BUF })

  local w = math.min(90, math.floor(vim.o.columns * 0.8))
  local h = math.min(#lines + 2, math.floor(vim.o.lines * 0.8))
  FLOAT_WIN = vim.api.nvim_open_win(FLOAT_BUF, true, {
    relative = 'editor', width = w, height = h,
    row = math.floor((vim.o.lines - h) / 2),
    col = math.floor((vim.o.columns - w) / 2),
    style = 'minimal', border = 'rounded',
    title = ' rock.nvim: last completion ', title_pos = 'center',
  })
  vim.api.nvim_set_option_value('wrap', false, { win = FLOAT_WIN })
end

function M.toggle()
  if M.is_open() then M.close() else M.open() end
end

return M
