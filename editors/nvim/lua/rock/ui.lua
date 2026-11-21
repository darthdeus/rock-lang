-- Small UI helpers shared between panel/floater modules.

local M = {}

-- Return a scratch buffer named `name`. If one already exists (e.g.
-- restored by a session plugin or left over after :bwipeout wasn't
-- called), reuse it instead of creating a new one with the same name --
-- nvim_buf_set_name will error with E95 otherwise.
function M.ensure_named_buffer(name)
  for _, b in ipairs(vim.api.nvim_list_bufs()) do
    if vim.api.nvim_buf_is_valid(b) and vim.api.nvim_buf_get_name(b) == name then
      -- Make sure it has scratch-buffer options; session restore may
      -- have left it as a regular file buffer.
      pcall(vim.api.nvim_set_option_value, 'buftype', 'nofile', { buf = b })
      pcall(vim.api.nvim_set_option_value, 'bufhidden', 'hide', { buf = b })
      pcall(vim.api.nvim_set_option_value, 'swapfile', false, { buf = b })
      return b, true  -- (bufnr, reused)
    end
  end
  local buf = vim.api.nvim_create_buf(false, true)
  vim.api.nvim_buf_set_name(buf, name)
  return buf, false
end

function M.centered(buf, width, height, title)
  return vim.api.nvim_open_win(buf, true, {
    relative = 'editor', width = width, height = height,
    row = math.floor((vim.o.lines - height) / 2),
    col = math.floor((vim.o.columns - width) / 2),
    style = 'minimal', border = 'rounded',
    title = title, title_pos = 'center',
  })
end

function M.write_lines(buf, lines)
  vim.bo[buf].modifiable = true
  vim.api.nvim_buf_set_lines(buf, 0, -1, false, lines)
  vim.bo[buf].modifiable = false
end

function M.float(view, opts)
  local buf, win

  local function show(lines)
    if not buf or not vim.api.nvim_buf_is_valid(buf) then return end
    M.write_lines(buf, lines)
  end

  function view.is_open()
    return win and vim.api.nvim_win_is_valid(win)
  end

  function view.close()
    if view.is_open() then pcall(vim.api.nvim_win_close, win, true) end
    win = nil
  end

  function view.open()
    if view.is_open() then view.close(); return end
    if not buf or not vim.api.nvim_buf_is_valid(buf) then
      buf = M.ensure_named_buffer('rock://' .. opts.name)
      local keys = { buffer = buf, silent = true }
      vim.keymap.set('n', 'q', view.close, keys)
      vim.keymap.set('n', '<esc>', view.close, keys)
      if opts.refresh then
        vim.keymap.set('n', 'r', function() opts.refresh(show) end, keys)
      end
    end
    local lines = opts.lines and opts.lines()
    if lines then show(lines) end
    local w, h = opts.size(lines)
    win = M.centered(buf, w, h, ' rock.nvim: ' .. opts.title .. ' ')
    if opts.nowrap then vim.wo[win].wrap = false end
    if opts.on_open then opts.on_open(show) end
  end

  view.toggle = view.open
  return view
end

return M
