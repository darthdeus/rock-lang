-- Use only .rock groups so other languages retain their colorscheme links.

local M = {}

-- Values are highlight links or nvim_set_hl specs.
M.defaults = {
  parameter = { fg = '#c4a7e7' },                  -- iris
  variable  = '@variable',                         -- falls through to theme default
  global    = { bold = true },     -- bold-only tier signal; theme drives color
  property  = { fg = '#9ccfd8' },                  -- foam
  -- Applied to type tokens whose AST has is_nullable=true (e.g. `Foo?`).
  -- Rose-pine rose (warm pink) cues "this may be null" at declaration sites.
  -- Set to nil to disable.
  nullable  = { fg = '#ebbcba', italic = true },
}

-- Reapply on ColorScheme so theme changes do not erase the links.
local function apply(overrides)
  local hl = vim.tbl_extend('force', M.defaults, overrides or {})
  local set = function(group, spec)
    if spec == nil or spec == '' then return end
    if type(spec) == 'string' then
      vim.api.nvim_set_hl(0, group, { link = spec, default = false })
    elseif type(spec) == 'table' then
      local copy = vim.deepcopy(spec)
      copy.default = false
      vim.api.nvim_set_hl(0, group, copy)
    end
  end
  -- Mutable globals use static; constants use readonly.
  set('@lsp.typemod.variable.readonly.rock', hl.global)
  set('@lsp.typemod.variable.static.rock', hl.global)
  set('@lsp.type.variable.rock', hl.variable)
  set('@lsp.type.parameter.rock', hl.parameter)
  set('@lsp.type.property.rock', hl.property)
  -- The modifier covers each nullable type kind.
  set('@lsp.mod.nullable.rock', hl.nullable)
end

function M.setup(overrides)
  apply(overrides)
  local group = vim.api.nvim_create_augroup('rock.nvim.highlights', { clear = true })
  vim.api.nvim_create_autocmd('ColorScheme', {
    group = group,
    callback = function() apply(overrides) end,
  })
end

return M
