-- Rock-specific LSP highlight links.
--
-- The rock-lsp server emits these distinguishable identifier kinds:
--
--   parameter                  -- function parameters (decl + refs)
--   variable                   -- local let bindings (decl + refs)
--   variable + readonly        -- `const` declarations (decl + refs)
--   variable + static          -- mutable `global`s (decl + refs)
--
-- (Enum members, macros, decorators, and type parameters emit their standard
-- LSP token types -- @lsp.type.enumMember/macro/decorator/typeParameter --
-- which colorschemes style on their own, so no links are set here.)
--
-- Neovim's LSP highlight groups and precedence:
--
--   @lsp.typemod.variable.readonly.rock   (most specific; consts here)
--   @lsp.typemod.variable.static.rock     (mutable globals)
--   @lsp.type.variable.rock               (locals, Rock only)
--   @lsp.type.variable                    (locals, cross-language)
--   @lsp.type.parameter.rock              (params, Rock only)
--   @lsp.type.parameter                   (params, cross-language)
--
-- We set links only on the `.rock` variants so we don't step on other
-- languages. If a link is left unset, Neovim falls through to the
-- non-suffixed group (usually driven by your colorscheme).
--
-- To customize, pass a `highlights = { ... }` table to require('rock').setup(),
-- keyed by the short names below. Any key you omit keeps its default.

local M = {}

-- Default links. Chosen to give a three-tier visual:
--   params  -> distinct accent so reading a fn body shows what's bound
--   locals  -> plain text color (common case)
--   globals -> strong accent so HOT_RELOAD / config constants pop
--
-- CONTRIBUTION WELCOME:
-- These defaults are conservative fallbacks to well-known Tree-sitter
-- capture groups that most colorschemes style. If you want a different
-- palette (e.g. bold for globals, italic for params, a dedicated color
-- for readonly), edit this table. Keep it to 3-6 lines; the goal is a
-- clear tier, not a rainbow.
-- Each value: a highlight-group name (string) or an hl spec table.
-- Spec tables accept { fg, bg, italic, bold, underline, ... } — see :h nvim_set_hl.
--
-- Rose-pine palette (substitute your own if you use a different theme):
--   iris   = '#c4a7e7'  -- params
--   gold   = '#f6c177'  -- globals (readonly)
--   text   = '#e0def4'  -- locals / default
--   foam   = '#9ccfd8'  -- properties / types
--   rose   = '#ebbcba'
--   love   = '#eb6f92'
--   pine   = '#31748f'
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

-- Apply the highlight table. Each value can be:
--   * a string: treated as a link to another highlight group
--   * a table : passed straight to nvim_set_hl (e.g. { fg = '#ebbcba', italic = true })
-- Re-applied on ColorScheme so theme swaps don't wipe us out.
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
  -- Globals — most specific wins. `const` emits readonly, a mutable
  -- `global` emits static; both get the same bold tier signal.
  set('@lsp.typemod.variable.readonly.rock', hl.global)
  set('@lsp.typemod.variable.static.rock', hl.global)
  -- Locals.
  set('@lsp.type.variable.rock', hl.variable)
  -- Parameters.
  set('@lsp.type.parameter.rock', hl.parameter)
  -- Properties (struct fields, map keys rendered via field access).
  set('@lsp.type.property.rock', hl.property)
  -- Nullable types — modifier-based group catches every type variant
  -- (struct, enum, interface, plain type) when the `?` suffix is present.
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
