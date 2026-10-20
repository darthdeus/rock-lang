vim.bo.smartindent = true
vim.bo.expandtab = true
vim.bo.shiftwidth = 4
vim.bo.tabstop = 4
vim.bo.softtabstop = 4

-- Comment leaders: `//` line, `/* */` block. Drives `gc`/commentstring and,
-- with formatoptions r/o, continues the leader on <CR> and o/O.
vim.bo.commentstring = '// %s'
vim.bo.comments = 's1:/*,mb:*,ex:*/,://'
vim.opt_local.formatoptions:append('ro')
