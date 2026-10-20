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

return M
