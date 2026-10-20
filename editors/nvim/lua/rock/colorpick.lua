-- :RockColorPick — show all highlight groups active under the cursor with
-- their resolved fg/bg hex codes. Built for tuning the palette in
-- highlights.lua: hover the token, see what hl group is winning, yank the
-- hex value, edit the default.
--
-- Sources collected (in priority order, most-specific first):
--   1. semantic tokens (LSP)  -- @lsp.type.* / @lsp.typemod.*
--   2. treesitter captures    -- @variable, @function, ...
--   3. syntax (legacy)
--   4. extmarks (virt_text/sign hl)
--
-- Keymaps in the float:
--   q       close
--   y       yank hex under cursor (or first hex on the line)
--   <CR>    yank the highlight group name on the current line
--   r       refresh (re-sample at the original cursor position)

local ui = require('rock.ui')

local M = {}

local FLOAT_NAME = 'rock://colorpick'

-- Resolve a highlight group through links to its concrete definition.
-- Returns a table with fg/bg/style keys (any may be nil) and the final
-- group name we resolved to (so we can show "foo -> bar" if linked).
local function resolve(group)
  local seen = {}
  local cur = group
  while cur and not seen[cur] do
    seen[cur] = true
    local ok, hl = pcall(vim.api.nvim_get_hl, 0, { name = cur, link = true })
    if not ok or not hl then break end
    if hl.link then
      cur = hl.link
    else
      local concrete = vim.api.nvim_get_hl(0, { name = cur, link = false })
      return concrete or {}, cur
    end
  end
  return {}, cur or group
end

local function hex(n)
  if type(n) ~= 'number' then return nil end
  return string.format('#%06x', n)
end

-- Collect (group_name, kind) pairs from vim.inspect_pos in priority order.
local function collect_groups(buf, row, col)
  local info = vim.inspect_pos(buf, row, col)
  local out = {}
  local seen = {}
  local function push(name, kind)
    if not name or name == '' then return end
    local key = kind .. '\0' .. name
    if seen[key] then return end
    seen[key] = true
    out[#out + 1] = { name = name, kind = kind }
  end

  for _, t in ipairs(info.semantic_tokens or {}) do
    local opts = t.opts or {}
    push(opts.hl_group_link or opts.hl_group, 'lsp')
  end
  -- Treesitter entries are returned outermost-first; reverse for innermost-wins.
  local ts = info.treesitter or {}
  for i = #ts, 1, -1 do
    push(ts[i].hl_group_link or ts[i].hl_group, 'ts')
  end
  for _, s in ipairs(info.syntax or {}) do
    push(s.hl_group_link or s.hl_group, 'syn')
  end
  for _, e in ipairs(info.extmarks or {}) do
    push(e.opts and (e.opts.hl_group or e.opts.line_hl_group), 'ext')
  end
  return out
end

local function build_lines(buf, row, col)
  local entries = collect_groups(buf, row, col)
  local lines = {}
  local meta = {}  -- per-line: { hexes = { '#...', ... }, group = '...' }

  local function add(line, hexes, group)
    lines[#lines + 1] = line
    meta[#lines] = { hexes = hexes or {}, group = group }
  end

  add(string.format('Cursor: line %d col %d  (buf %d)', row + 1, col + 1, buf), {})
  add('Sources: lsp > ts > syn > ext.  Press q to close, y to yank hex, <CR> to yank group, r to refresh.', {})
  add('', {})

  if #entries == 0 then
    add('No highlight groups active here.', {})
  else
    add(string.format('%-4s  %-10s  %-10s  %s', 'KIND', 'FG', 'BG', 'GROUP'), {})
    add(string.rep('-', 70), {})
    for _, e in ipairs(entries) do
      local def, resolved = resolve(e.name)
      local fg, bg = hex(def.fg), hex(def.bg)
      local style = {}
      for _, k in ipairs({ 'bold', 'italic', 'underline', 'undercurl', 'reverse', 'strikethrough' }) do
        if def[k] then style[#style + 1] = k end
      end
      local label = e.name
      if resolved ~= e.name then
        label = label .. '  -> ' .. resolved
      end
      if #style > 0 then
        label = label .. '  [' .. table.concat(style, ',') .. ']'
      end
      local hexes = {}
      if fg then hexes[#hexes + 1] = fg end
      if bg then hexes[#hexes + 1] = bg end
      add(string.format('%-4s  %-10s  %-10s  %s',
        e.kind, fg or '-', bg or '-', label), hexes, e.name)
    end
  end

  return lines, meta
end

-- Apply per-hex color swatches as virtual text on each row. Tiny squares
-- on the right of each hex code so you can eyeball-confirm the value.
local function apply_swatches(buf, lines)
  local ns = vim.api.nvim_create_namespace('rock.colorpick.swatch')
  vim.api.nvim_buf_clear_namespace(buf, ns, 0, -1)
  for i, line in ipairs(lines) do
    -- Find any '#rrggbb' on the line; create a unique hl group per color
    -- and attach an extmark virt_text overlay at that column.
    local start = 1
    while true do
      local s, e, hx = string.find(line, '(#%x%x%x%x%x%x)', start)
      if not s then break end
      local group = 'RockColorPickSwatch_' .. hx:sub(2)
      pcall(vim.api.nvim_set_hl, 0, group, { fg = hx, bg = hx })
      vim.api.nvim_buf_set_extmark(buf, ns, i - 1, e, {
        virt_text = { { ' ##', group } },
        virt_text_pos = 'inline',
      })
      start = e + 1
    end
  end
end

local function open_float(lines, meta, source)
  local buf = ui.ensure_named_buffer(FLOAT_NAME)
  vim.api.nvim_set_option_value('modifiable', true, { buf = buf })
  vim.api.nvim_buf_set_lines(buf, 0, -1, false, lines)
  vim.api.nvim_set_option_value('modifiable', false, { buf = buf })
  vim.api.nvim_set_option_value('filetype', 'rock-colorpick', { buf = buf })

  apply_swatches(buf, lines)

  local width = 0
  for _, l in ipairs(lines) do
    if #l > width then width = #l end
  end
  width = math.min(math.max(width + 4, 60), vim.o.columns - 4)
  local height = math.min(#lines + 1, math.floor(vim.o.lines * 0.6))

  local win = vim.api.nvim_open_win(buf, true, {
    relative = 'editor',
    width = width,
    height = height,
    row = math.floor((vim.o.lines - height) / 2),
    col = math.floor((vim.o.columns - width) / 2),
    style = 'minimal',
    border = 'rounded',
    title = ' RockColorPick ',
    title_pos = 'center',
  })
  vim.api.nvim_set_option_value('cursorline', true, { win = win })

  local function close()
    if vim.api.nvim_win_is_valid(win) then vim.api.nvim_win_close(win, true) end
  end

  local function yank_hex()
    local row = vim.api.nvim_win_get_cursor(win)[1]
    local m = meta[row]
    if not m or #m.hexes == 0 then
      vim.notify('rock.nvim: no hex on this line', vim.log.levels.INFO)
      return
    end
    local hx = m.hexes[1]
    vim.fn.setreg('+', hx)
    vim.fn.setreg('"', hx)
    vim.notify('Yanked ' .. hx)
  end

  local function yank_group()
    local row = vim.api.nvim_win_get_cursor(win)[1]
    local m = meta[row]
    if not m or not m.group then
      vim.notify('rock.nvim: no group on this line', vim.log.levels.INFO)
      return
    end
    vim.fn.setreg('+', m.group)
    vim.fn.setreg('"', m.group)
    vim.notify('Yanked ' .. m.group)
  end

  local function refresh()
    local new_lines, new_meta = build_lines(source.buf, source.row, source.col)
    lines, meta = new_lines, new_meta
    vim.api.nvim_set_option_value('modifiable', true, { buf = buf })
    vim.api.nvim_buf_set_lines(buf, 0, -1, false, lines)
    vim.api.nvim_set_option_value('modifiable', false, { buf = buf })
    apply_swatches(buf, lines)
  end

  local map = function(lhs, fn) vim.keymap.set('n', lhs, fn, { buffer = buf, nowait = true, silent = true }) end
  map('q', close)
  map('<Esc>', close)
  map('y', yank_hex)
  map('<CR>', yank_group)
  map('r', refresh)
end

function M.open()
  local win = vim.api.nvim_get_current_win()
  local buf = vim.api.nvim_win_get_buf(win)
  local pos = vim.api.nvim_win_get_cursor(win)
  local row, col = pos[1] - 1, pos[2]
  local lines, meta = build_lines(buf, row, col)
  open_float(lines, meta, { buf = buf, row = row, col = col })
end

return M
