-- rock.nvim - editor integration for the Rock language server.
--
-- Usage (lazy.nvim, pointing at this directory):
--   { dir = '~/projects/rock-lang/editors/nvim', name = 'rock.nvim', dev = true,
--     ft = 'rock', config = function() require('rock').setup({}) end }

local M = {}

M.defaults = {
  -- Absolute path to the `rock` binary. If nil, resolved from
  -- $ROCK_LSP_DIR/target/debug/rock, the cwd worktree, the rock-lang
  -- checkout, or `rock` on PATH (see binary.lua).
  binary = nil,
  -- Additional capabilities to merge in (e.g. cmp_nvim_lsp's).
  capabilities = nil,
  -- Extra on_attach, called after ours.
  on_attach = nil,
  -- Auto-open the sidebar panel on first .rock buffer.
  auto_open_panel = true,
  -- Keymap to toggle panel (nil to skip).
  toggle_key = '<leader>rp',
  -- Keymap to toggle the completion-debug floating window (nil to skip).
  completion_key = '<leader>rc',
  -- Keymap to toggle the per-method latency/histogram floater (nil to skip).
  perf_key = '<leader>rt',
  -- Keymap to toggle the loaded-modules floater (nil to skip).
  modules_key = '<leader>rm',
  -- Keymap to open the color-picker floater (highlight groups + hex
  -- codes under cursor). nil to skip.
  colorpick_key = '<leader>rk',
  -- Panel side: 'right' | 'left' (togglable in-panel with `s`).
  panel_side = 'right',
  -- Panel width in columns.
  panel_width = 50,
  -- Insert-mode hotkey for bug-capture snapshot. Stays in insert mode.
  -- Set to nil to disable. Default <C-g>c -- <C-g> is unused in insert
  -- mode and doesn't collide with nvim-cmp keybindings.
  capture_key = '<C-g>c',
  -- Highlight-link overrides for Rock identifier tiers. See
  -- editors/nvim/lua/rock/highlights.lua for the keys. nil keeps defaults.
  highlights = nil,
  -- Buffer-local `completeopt` applied to .rock buffers on LspAttach.
  -- Default surfaces the menu even for one match (`menuone`), doesn't
  -- auto-insert (`noinsert`, leaves the user driving), and uses nvim's
  -- built-in fuzzy matching so editor-side filtering matches the
  -- server's relevance ranking. Set to `false` (or empty string) to
  -- skip the override and inherit whatever the user has globally.
  completeopt = 'menuone,noinsert,fuzzy',
  -- Buffer-local `complete` (keyword-completion sources) for .rock
  -- buffers. Default `'.'` keeps current-buffer keyword scraping —
  -- so `<C-n>` / `<C-p>` still surface local identifiers — but drops
  -- the noisier sources (other windows, loaded/unloaded buffers,
  -- tags) from the default `.,w,b,u,t`. Set to `''` to suppress
  -- keyword completion entirely, or `false` to leave the global
  -- value untouched.
  complete = '.',
  -- Buffer-local `tags` for .rock buffers. rock-lsp writes a `.rock.tags` to
  -- each stick.toml package root (covering that package + its deps). The plugin
  -- tracks every package you open a buffer in and points all rock buffers at
  -- the UNION of their tags (absolute paths), so a tag jump from a file in one
  -- package still resolves a symbol defined in another while both are open.
  -- This value is the upward-search fallback (the `;` climbs from the file's
  -- dir) appended
  -- after the union, covering the current file's own package. Set to '' or
  -- false to disable rock tag handling entirely.
  tags = './.rock.tags;',
  -- Reorder nvim-cmp's comparators for .rock buffers so the LSP's `sortText`
  -- wins over fuzzy `score`. The server ranks call-argument completions by type
  -- (a variable of the expected param type first, a function returning it next)
  -- via `sortText`; cmp's default order scores fuzzily first, so that ranking
  -- otherwise only breaks ties. Buffer-local, preserves the rest of your
  -- comparator list. Set false to leave cmp untouched (no-op without nvim-cmp).
  cmp_type_priority = true,
  -- Append a JSONL row per completion accept/dismiss to
  -- $XDG_DATA_HOME/nvim/rock/completion-log.jsonl. Restricted to rock
  -- buffers. Requires nvim-cmp. See lua/rock/completion_log.lua.
  completion_log = false,
  -- Override the log path. nil = stdpath('data')/rock/completion-log.jsonl.
  completion_log_path = nil,
  -- Spawn the server with ROCK_LSP_RECORD so it writes the authoritative
  -- replay-corpus row per accepted completion (buffer as the ranker saw it,
  -- full ranked list for completion analytics). Client-agnostic
  -- server feature; pairs with completion_log via chosen.record_id.
  completion_record = false,
  -- Override the corpus path. nil = stdpath('data')/rock/completion-corpus.jsonl.
  completion_record_path = nil,
}

M.opts = nil

-- Minimum supported Neovim version. Bumped in lockstep with the version
-- the rock-lsp client surface targets — older nvim has gaps in the LSP
-- API we rely on (semantic tokens re-request, vim.lsp.config, etc.).
M.min_nvim = { 0, 12, 2 }

function M.setup(user)
  do
    local v = vim.version()
    if vim.version.lt({ v.major, v.minor, v.patch }, M.min_nvim) then
      vim.notify(
        ('rock.nvim requires Neovim >= %d.%d.%d (you have %d.%d.%d). Plugin not loaded.')
          :format(M.min_nvim[1], M.min_nvim[2], M.min_nvim[3], v.major, v.minor, v.patch),
        vim.log.levels.ERROR, { title = 'rock.nvim' }
      )
      return
    end
  end

  M.opts = vim.tbl_deep_extend('force', M.defaults, user or {})

  local client = require('rock.client')
  local panel = require('rock.panel')

  local cfg = client.make_config(M.opts)
  -- Register with nvim 0.11's native API. If the user placed our
  -- editors/nvim dir on runtimepath, lsp/rock.lua is auto-discovered and
  -- this call merges into that config. Either way, the final config wins.
  vim.lsp.config('rock', cfg)

  -- Only enable if the binary exists. Enabling with a missing binary
  -- leaves a half-initialized client in nvim's registry; the next
  -- FileType event trips client:is_stopped() on a nil rpc field
  -- ("attempt to index field 'rpc' (a nil value)").
  local bin_info = require('rock.state').get().binary
  if bin_info.exists then
    vim.lsp.enable('rock')
  else
    vim.notify(
      ('rock binary not found at %s\nBuild it (`cargo build --bin rock` in a rock-lang checkout) and restart nvim.')
        :format(bin_info.path or '<unresolved>'),
      vim.log.levels.WARN, { title = 'rock.nvim' }
    )
  end

  client.setup_autocmds()

  panel.set_side(M.opts.panel_side)
  panel.set_width(M.opts.panel_width)

  if M.opts.auto_open_panel then
    panel.setup_autoopen()
  end

  require('rock.stats').start()
  require('rock.log').start()
  require('rock.semtok').setup()
  require('rock.highlights').setup(M.opts.highlights)
  if M.opts.completion_log then
    require('rock.completion_log').setup(M.opts)
  end

  -- Pass the client id into log.lua once the server attaches so its
  -- relevance filter can match log lines tagged with our client id.
  vim.api.nvim_create_autocmd('LspAttach', {
    group = vim.api.nvim_create_augroup('rock.nvim.log', { clear = true }),
    callback = function(args)
      local c = vim.lsp.get_client_by_id(args.data.client_id)
      if c and c.name == 'rock' then
        require('rock.log').set_client_id(c.id)
      end
    end,
  })

  if M.opts.toggle_key then
    vim.keymap.set('n', M.opts.toggle_key, panel.toggle,
      { silent = true, desc = 'rock.nvim: toggle panel' })
  end
  if M.opts.completion_key then
    vim.keymap.set('n', M.opts.completion_key,
      function() require('rock.completion').toggle() end,
      { silent = true, desc = 'rock.nvim: last completion' })
  end
  if M.opts.perf_key then
    vim.keymap.set('n', M.opts.perf_key,
      function() require('rock.perf').toggle() end,
      { silent = true, desc = 'rock.nvim: per-method latency histograms' })
  end
  if M.opts.modules_key then
    vim.keymap.set('n', M.opts.modules_key,
      function() require('rock.modules').toggle() end,
      { silent = true, desc = 'rock.nvim: loaded modules' })
  end
  if M.opts.colorpick_key then
    vim.keymap.set('n', M.opts.colorpick_key,
      function() require('rock.colorpick').open() end,
      { silent = true, desc = 'rock.nvim: color pick under cursor' })
  end
  if M.opts.capture_key then
    -- Mode-global. Preserves whatever mode you're in (insert, visual,
    -- normal, select) so pressing the chord the moment something looks
    -- wrong doesn't disturb context.
    vim.keymap.set({ 'n', 'i', 'v', 'x', 's' }, M.opts.capture_key,
      function() require('rock.capture').capture() end,
      { silent = true, desc = 'rock.nvim: capture bug snapshot' })
  end

  vim.api.nvim_create_user_command('RockCompletion',
    function() require('rock.completion').toggle() end,
    { desc = 'rock.nvim: show last completion response' })
  vim.api.nvim_create_user_command('RockPerf',
    function() require('rock.perf').toggle() end,
    { desc = 'rock.nvim: per-method latency + histogram' })
  vim.api.nvim_create_user_command('RockModules',
    function() require('rock.modules').toggle() end,
    { desc = 'rock.nvim: loaded modules' })
  vim.api.nvim_create_user_command('RockColorPick',
    function() require('rock.colorpick').open() end,
    { desc = 'rock.nvim: highlight groups + hex colors under cursor' })
  vim.api.nvim_create_user_command('RockLspDoctor',
    function() require('rock.doctor').toggle() end,
    { desc = 'rock.nvim: health check' })
  vim.api.nvim_create_user_command('RockDump',
    function(args) require('rock.dump').write(args.args ~= '' and args.args or nil) end,
    { nargs = '?', complete = 'file',
      desc = 'rock.nvim: write a JSON snapshot to /tmp (or path arg)' })
  vim.api.nvim_create_user_command('RockCapture',
    function(args)
      require('rock.capture').capture({
        note = args.args ~= '' and args.args or nil,
      })
    end,
    { nargs = '*',
      desc = 'rock.nvim: capture bug snapshot, optional note args '
        .. '(:RockCapture bad: wrong-type suggestion). <C-g>c = note-less.' })

  vim.api.nvim_create_user_command('RockPanel', function(args)
    local sub = args.args ~= '' and args.args or 'toggle'
    if sub == 'open' then panel.open()
    elseif sub == 'close' then panel.close()
    elseif sub == 'toggle' then panel.toggle()
    elseif sub == 'left' then panel.set_side('left'); panel.open()
    elseif sub == 'right' then panel.set_side('right'); panel.open()
    else vim.notify('usage: RockPanel [toggle|open|close|left|right]', vim.log.levels.WARN) end
  end, {
    nargs = '?',
    complete = function() return { 'toggle', 'open', 'close', 'left', 'right' } end,
    desc = 'rock.nvim panel',
  })
  vim.api.nvim_create_user_command('RockRestart', function() require('rock.client').restart() end,
    { desc = 'rock.nvim: restart LSP' })

  -- Runtime toggle for completion logging — A/B perf testing without
  -- restarting nvim. `:RockCompletionLog` reports state.
  vim.api.nvim_create_user_command('RockCompletionLog', function(args)
    local a = (args.args or ''):lower()
    if a == 'on' or a == 'enable' then
      vim.g.rock_completion_log_disabled = false
    elseif a == 'off' or a == 'disable' then
      vim.g.rock_completion_log_disabled = true
    elseif a == 'toggle' then
      vim.g.rock_completion_log_disabled = not (vim.g.rock_completion_log_disabled == true)
    elseif a ~= '' then
      vim.notify('RockCompletionLog: usage on|off|toggle', vim.log.levels.WARN)
      return
    end
    local on = not (vim.g.rock_completion_log_disabled == true
                 or vim.g.rock_completion_log_disabled == 1)
    vim.notify('rock.nvim completion_log: ' .. (on and 'on' or 'off'))
  end, {
    nargs = '?',
    complete = function() return { 'on', 'off', 'toggle' } end,
    desc = 'rock.nvim: toggle completion logging at runtime',
  })
end

-- Convenience re-exports for users.
M.restart = function() require('rock.client').restart() end
M.panel = function() return require('rock.panel') end
M.statusline = function() return require('rock.statusline').component() end

return M
