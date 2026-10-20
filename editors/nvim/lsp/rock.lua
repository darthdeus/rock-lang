-- Neovim 0.11+ auto-discovered LSP config for rock.
-- Returned when this directory is on runtimepath. The real config
-- (with wrapped cmd for RPC traffic capture) is installed by
-- require('rock').setup(), which calls vim.lsp.config('rock', ...) and
-- overrides these defaults. This file exists so that `vim.lsp.enable('rock')`
-- works even before setup() is called (e.g. in minimal debug configs).

return {
  cmd = { 'rock', 'lsp', 'serve' },
  filetypes = { 'rock' },
  root_markers = { 'stick.toml', 'project.toml', '.git' },
}
