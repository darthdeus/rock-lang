
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
  -- Capture key; nil disables it.
  capture_key = '<C-g>c',
  -- Highlight-link overrides for Rock identifier tiers. See
  -- editors/nvim/lua/rock/highlights.lua for the keys. nil keeps defaults.
  highlights = nil,
  -- Set false or empty to inherit the global completeopt.
  completeopt = 'menuone,noinsert,fuzzy',
  -- Empty disables keyword completion; false inherits the global value.
  complete = '.',
  -- Tags combine open package roots and this upward-search fallback; false or '' disables them.
  tags = './.rock.tags;',
  -- Type ranking precedes fuzzy score; false leaves nvim-cmp untouched.
  allocation_hints = false,
  cmp_type_priority = true,
}

M.opts = nil

-- The LSP client requires Neovim APIs introduced in this version.
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
  vim.lsp.config('rock', cfg)

  -- A missing binary leaves a client with nil RPC in Neovim's registry.
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

  -- Log filtering needs the attached client id.
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
    -- Capture must not change the current mode.
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
end

M.restart = function() require('rock.client').restart() end
M.panel = function() return require('rock.panel') end
M.statusline = function() return require('rock.statusline').component() end

return M
