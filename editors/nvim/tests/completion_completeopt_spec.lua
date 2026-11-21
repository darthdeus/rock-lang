-- After LspAttach on a .rock buffer, the plugin should set buffer-local
-- `completeopt` and `complete` to the opinionated defaults that
-- suppress text-keyword pollution and surface the menu predictably.
--
-- Bug shape this guards against: typing `let x = 0` previously had the
-- popup mix in `0.5` and other tokens scraped from elsewhere in the
-- file (default `complete=.,w,b,u,t`). Restricting `complete=''` plus
-- `completeopt=menuone,noinsert,fuzzy` keeps the popup LSP-only and
-- editor-side fuzzy matching aligned with the server's ranking.

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

local project = dofile('tests/_lib/project.lua')

vim.opt.completeopt = 'menu'
vim.go.complete = '.,w,b,u,t'
local bufnr = project.open('co', { ['main.rock'] = 'fn main() {\n    let x = 0\n}\n' })

local got_co = vim.bo[bufnr].completeopt
if got_co ~= 'menuone,noinsert,fuzzy' then
  fail('expected completeopt=menuone,noinsert,fuzzy, got ' .. tostring(got_co))
end

-- `complete='.'` keeps current-buffer keyword scraping (so `<C-n>`
-- still surfaces local identifiers) but drops other-buffer / tag
-- sources that were pulling tokens like `0.5` from elsewhere.
local got_complete = vim.bo[bufnr].complete
if got_complete ~= '.' then
  fail('expected complete=".", got ' .. vim.inspect(got_complete))
end

-- Tag jumps (`<C-]>`) must reach the per-package `.rock.tags` rock-lsp writes
-- to each stick.toml root: the buffer-local `tags` should include the
-- upward-searching `./.rock.tags;` so a file anywhere under a package resolves
-- that package's tags regardless of cwd.
local got_tags = vim.bo[bufnr].tags
if not got_tags:find('./.rock.tags;', 1, true) then
  fail('expected buffer tags to include "./.rock.tags;", got ' .. tostring(got_tags))
end

print('OK: completeopt=' .. got_co .. ', complete=' .. vim.inspect(got_complete)
  .. ', tags=' .. got_tags)
os.exit(0)
