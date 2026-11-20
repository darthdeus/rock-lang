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

return require('rock.ui').float(M, {
  name = 'completion',
  title = 'last completion',
  nowrap = true,
  lines = function() return render(state.get().last_completion) end,
  size = function(lines)
    return math.min(90, math.floor(vim.o.columns * 0.8)),
      math.min(#lines + 2, math.floor(vim.o.lines * 0.8))
  end,
})
