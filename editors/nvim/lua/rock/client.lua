
local state = require('rock.state')
local binary = require('rock.binary')

local M = {}

local SERVER_NAME = 'rock'
local crash_ns = vim.api.nvim_create_namespace('rock.nvim.crash')

-- Cap summaries without discarding the expandable payload.
local function summarize(v, depth)
  depth = depth or 0
  if depth > 3 then return '<...>' end
  local t = type(v)
  if t == 'string' then
    if #v > 200 then return v:sub(1, 200) .. ('...(+%d)'):format(#v - 200) end
    return v
  elseif t == 'number' or t == 'boolean' or t == 'nil' then
    return v
  elseif t == 'table' then
    local n = 0
    for _ in pairs(v) do n = n + 1 end
    if n == 0 then return {} end
    if n > 12 then return ('<table n=%d>'):format(n) end
    local out = {}
    local i = 0
    for k, vv in pairs(v) do
      i = i + 1
      if i > 12 then out['...'] = true; break end
      out[k] = summarize(vv, depth + 1)
    end
    return out
  end
  return tostring(v)
end

local function mark_buffers_crashed(code, signal)
  for bufnr in pairs(state.get().buffers) do
    if vim.api.nvim_buf_is_valid(bufnr) then
      vim.diagnostic.set(crash_ns, bufnr, { {
        lnum = 0, col = 0,
        severity = vim.diagnostic.severity.ERROR,
        message = ('rock-lsp crashed (exit=%d sig=%d) -- see rock.nvim panel'):format(code or -1, signal or 0),
        source = 'rock.nvim',
      } })
    end
  end
end

local function clear_crash_marks()
  for bufnr in pairs(state.get().buffers) do
    if vim.api.nvim_buf_is_valid(bufnr) then
      vim.diagnostic.reset(crash_ns, bufnr)
    end
  end
end

local opts_cache = nil

-- Open packages share a tag union across buffers.
local rock_tag_dirs = {}

-- Commas and spaces need escaping in the tags option.
local function esc_tag_path(p)
  return (p:gsub('[, ]', '\\%0'))
end

-- Exclude scratch and special buffers: their resolved cwd can look like a project.
local function tags_root(bufnr)
  if vim.bo[bufnr].buftype ~= '' then
    return nil
  end
  if vim.api.nvim_buf_get_name(bufnr) == '' then
    return nil
  end
  return vim.fs.root(bufnr, 'stick.toml')
end

-- Rebuild from the saved base so repeated updates do not accumulate paths.
local function rebuild_buffer_tags(bufnr)
  if type(opts_cache.tags) ~= 'string' or opts_cache.tags == '' then
    return
  end
  -- Preserve the buffer-local tags that existed before attachment.
  local base = vim.b[bufnr].rock_tags_base
  if base == nil then
    base = vim.api.nvim_get_option_value('tags', { buf = bufnr })
    vim.b[bufnr].rock_tags_base = base
  end
  local parts = {}
  if base ~= '' then
    parts[#parts + 1] = base
  end
  for path in pairs(rock_tag_dirs) do
    parts[#parts + 1] = esc_tag_path(path)
  end
  parts[#parts + 1] = opts_cache.tags
  vim.bo[bufnr].tags = table.concat(parts, ',')
end

-- A new package must refresh every already-open project buffer.
local function note_package_tags(bufnr, root)
  if type(opts_cache.tags) ~= 'string' or opts_cache.tags == '' then
    return
  end
  root = root or tags_root(bufnr)
  local added = false
  if root then
    local key = root .. '/.rock.tags'
    if not rock_tag_dirs[key] then
      rock_tag_dirs[key] = true
      added = true
    end
  end
  if added then
    for _, b in ipairs(vim.api.nvim_list_bufs()) do
      if vim.api.nvim_buf_is_loaded(b) and tags_root(b) then
        rebuild_buffer_tags(b)
      end
    end
  else
    rebuild_buffer_tags(bufnr)
  end
end

-- Server type ranking must precede nvim-cmp fuzzy scoring.
local function reorder_sort_text_before_score(comparators, sort_text, score)
  local without = {}
  for _, c in ipairs(comparators) do
    if c ~= sort_text then
      without[#without + 1] = c
    end
  end
  local out = {}
  local inserted = false
  for _, c in ipairs(without) do
    if c == score and not inserted then
      out[#out + 1] = sort_text
      inserted = true
    end
    out[#out + 1] = c
  end
  if not inserted then
    table.insert(out, 1, sort_text)
  end
  return out
end
M._reorder_sort_text_before_score = reorder_sort_text_before_score

-- Preserve the user's comparators; skip when nvim-cmp is absent.
local function apply_cmp_type_priority(bufnr)
  local ok, cmp = pcall(require, 'cmp')
  if not ok then
    return
  end
  local compare = require('cmp.config.compare')
  local sorting = cmp.get_config().sorting or {}
  local base = sorting.comparators
    or {
      compare.offset, compare.exact, compare.score, compare.recently_used,
      compare.locality, compare.kind, compare.sort_text, compare.length, compare.order,
    }
  local out = reorder_sort_text_before_score(base, compare.sort_text, compare.score)
  vim.api.nvim_buf_call(bufnr, function()
    cmp.setup.buffer({ sorting = { comparators = out } })
  end)
end

-- A nil RPC leaves a half-built client in Neovim; error before registration.
local function fail_start(reason)
  error(reason, 0)
end

-- Deduplicate missing-binary notices until restart or a successful start.
local missing_binary_reported = nil

local function build_cmd(bin_path)
  return function(dispatchers)
    clear_crash_marks()
    state.set_client({ state = 'starting', started_at = os.time(), last_exit = nil })

    if not bin_path or not (vim.uv or vim.loop).fs_stat(bin_path) then
      state.set_client({ state = 'crashed', last_exit = { code = -1, signal = 0, at = os.time() } })
      state.record_event('local', 'server/binary_missing', { path = bin_path })
      -- Avoid a notification storm until restart re-enables autoattach.
      pcall(vim.lsp.enable, SERVER_NAME, false)
      if missing_binary_reported ~= bin_path then
        missing_binary_reported = bin_path
        vim.schedule(function()
          vim.notify(('rock binary not found: %s'):format(tostring(bin_path)),
            vim.log.levels.ERROR, { title = 'rock.nvim' })
        end)
      end
      fail_start('rock binary missing: ' .. tostring(bin_path))
    end
    missing_binary_reported = nil

    local wrapped = vim.tbl_extend('force', {}, dispatchers)

    wrapped.notification = function(method, params)
      state.record_event('<-notif', method, summarize(params))
      -- Keep warnings and errors visible even without an LSP log tail.
      if (method == 'window/logMessage' or method == 'window/showMessage')
          and type(params) == 'table' then
        local sev = params.type
        if sev == 1 or sev == 2 then
          state.record_error(sev, method, tostring(params.message or ''))
        end
      end
      return dispatchers.notification(method, params)
    end
    wrapped.server_request = function(method, params)
      state.record_event('<-req', method, summarize(params))
      return dispatchers.server_request(method, params)
    end
    wrapped.on_exit = function(code, signal)
      state.set_client({
        state = code == 0 and 'detached' or 'crashed',
        last_exit = { code = code, signal = signal, at = os.time() },
        pid = nil,
      })
      state.record_event('local', 'server/exit', { code = code, signal = signal })
      if code ~= 0 then
        vim.schedule(function()
          mark_buffers_crashed(code, signal)
          vim.notify(
            ('rock-lsp crashed (exit=%d sig=%d)'):format(code or -1, signal or 0),
            vim.log.levels.ERROR,
            { title = 'rock.nvim' }
          )
        end)
      end
      if dispatchers.on_exit then dispatchers.on_exit(code, signal) end
    end

    local rpc = vim.lsp.rpc.start({ bin_path, 'lsp', 'serve' }, wrapped)
    if not rpc then
      state.set_client({ state = 'crashed' })
      state.record_event('local', 'server/start_failed', { bin = bin_path })
      fail_start('rock failed to start: ' .. tostring(bin_path))
    end

    local orig_request = rpc.request
    local orig_notify = rpc.notify
    rpc.request = function(method, params, cb, notify_reply)
      local is_completion = method == 'textDocument/completion'
      local sent_hr = (vim.uv or vim.loop).hrtime()
      local ev_id
      -- Preserve message_id: Neovim uses it to reject stale semantic tokens.
      local success, req_id = orig_request(method, params, function(err, result, message_id)
        local elapsed_ms = ((vim.uv or vim.loop).hrtime() - sent_hr) / 1e6
        state.record_request_timing(method, elapsed_ms)
        state.record_event('<-resp', method, {
          req_id = ev_id,
          err = summarize(err),
          result = summarize(result),
        })
        if is_completion then
          local items = (result and result.items) or result or {}
          if type(items) ~= 'table' then items = {} end
          -- Cap stored items; the original event retains the full payload.
          local capped = items
          if #items > 500 then capped = vim.list_slice(items, 1, 500) end
          local ctx = (params and params.context) or {}
          local pos = params and params.position or nil
          local latency_ms = sent_hr and
            ((vim.uv or vim.loop).hrtime() - sent_hr) / 1e6 or nil
          state.set_completion({
            at = os.time(),
            wall = os.date('%H:%M:%S'),
            latency_ms = latency_ms,
            position = pos,
            uri = params and params.textDocument and params.textDocument.uri or nil,
            trigger_kind = ctx.triggerKind,
            trigger_char = ctx.triggerCharacter,
            item_count = #items,
            is_incomplete = (result and result.isIncomplete) or false,
            items = capped,
          })
        end
        if cb then return cb(err, result, message_id) end
      end, notify_reply)
      ev_id = req_id
      state.record_event('->req', method, { req_id = req_id, params = summarize(params) })
      return success, req_id
    end
    rpc.notify = function(method, params)
      state.record_event('->notif', method, summarize(params))
      return orig_notify(method, params)
    end

    state.set_client({ pid = rpc.pid, state = 'attached' })
    return rpc
  end
end

function M.make_config(opts)
  -- Direct callers may pass partial options; keep their values over defaults.
  local defaults = require('rock').defaults or {}
  opts_cache = vim.tbl_extend('keep', opts or {}, defaults)
  binary.refresh(opts_cache.binary)
  binary.watch(opts_cache.binary)
  local bin = state.get().binary.path
  return {
    cmd = build_cmd(bin),
    filetypes = { 'rock' },
    root_markers = { 'stick.toml', 'project.toml', '.git' },
    capabilities = opts_cache.capabilities,
    init_options = { allocationHints = opts_cache.allocation_hints == true },
    on_attach = function(client, bufnr)
      state.set_buffer(bufnr, {
        attached = true,
        name = vim.api.nvim_buf_get_name(bufnr),
        client_id = client.id,
      })
      state.set_client({
        id = client.id,
        pid = client.rpc and client.rpc.pid or nil,
        capabilities = client.server_capabilities or {},
      })
      if type(opts_cache.completeopt) == 'string' and opts_cache.completeopt ~= '' then
        vim.bo[bufnr].completeopt = opts_cache.completeopt
      end
      if type(opts_cache.complete) == 'string' then
        vim.bo[bufnr].complete = opts_cache.complete
      end
      note_package_tags(bufnr)
      if opts_cache.cmp_type_priority then
        apply_cmp_type_priority(bufnr)
      end
      if opts_cache.on_attach then opts_cache.on_attach(client, bufnr) end
    end,
  }
end

-- Failed starts can leave rpc-less clients that break Neovim client reuse.
local function purge_zombies()
  local all = vim.lsp.client and vim.lsp.client._all
  if type(all) ~= 'table' then return end
  for id, c in pairs(all) do
    if c and c.name == SERVER_NAME and c.rpc == nil then
      all[id] = nil
    end
  end
end

function M.restart()
  purge_zombies()
  local clients = vim.lsp.get_clients({ name = SERVER_NAME })
  local rock_bufs = {}
  for _, client in ipairs(clients) do
    for bufnr in pairs(client.attached_buffers or {}) do
      if vim.api.nvim_buf_is_valid(bufnr) then
        table.insert(rock_bufs, bufnr)
      end
    end
  end
  for _, buf in ipairs(vim.api.nvim_list_bufs()) do
    if vim.api.nvim_buf_is_valid(buf)
        and vim.api.nvim_get_option_value('filetype', { buf = buf }) == 'rock'
        and not vim.tbl_contains(rock_bufs, buf) then
      table.insert(rock_bufs, buf)
    end
  end

  for _, client in ipairs(clients) do
    client:stop(true)
  end

  state.inc_restart()
  state.record_event('local', 'client/restart', { buffers = #rock_bufs })

  binary.refresh(opts_cache and opts_cache.binary)

  -- Do not re-enable autoattach while the binary is missing.
  local bin_info = state.get().binary
  if not bin_info.exists then
    vim.notify(
      ('rock binary not found at %s -- not restarting LSP.\nBuild it (`cargo build --bin rock` in a rock-lang checkout) and run :RockRestart.')
        :format(bin_info.path or '<unresolved>'),
      vim.log.levels.ERROR, { title = 'rock.nvim' })
    return
  end
  missing_binary_reported = nil

  vim.defer_fn(function()
    -- Force-stopped clients linger in the registry until their exit handler
    -- fires; reuse_client() trips on `self.rpc == nil` if we race it.
    vim.wait(2000, function()
      return #vim.lsp.get_clients({ name = SERVER_NAME }) == 0
    end, 25)
    -- Re-enable triggers auto-attach via FileType autocmds.
    pcall(vim.lsp.enable, SERVER_NAME)
    for _, bufnr in ipairs(rock_bufs) do
      if vim.api.nvim_buf_is_valid(bufnr) then
        vim.api.nvim_buf_call(bufnr, function()
          vim.cmd('doautocmd FileType')
        end)
      end
    end
    vim.defer_fn(function()
      for _, bufnr in ipairs(rock_bufs) do
        local cs = vim.lsp.get_clients({ bufnr = bufnr, name = SERVER_NAME })
        if #cs > 0 and cs[1].server_capabilities.semanticTokensProvider then
          pcall(vim.lsp.semantic_tokens.force_refresh, bufnr)
        end
      end
    end, 300)
  end, 100)
end

function M.setup_autocmds()
  local g = vim.api.nvim_create_augroup('rock.nvim.client', { clear = true })
  vim.api.nvim_create_autocmd('LspDetach', {
    group = g,
    callback = function(args)
      local client = vim.lsp.get_client_by_id(args.data.client_id)
      if client and client.name == SERVER_NAME then
        state.set_buffer(args.buf, { attached = false })
        -- LspDetach is per-buffer; keep the snapshot until the last buffer detaches.
        local still_attached = vim.lsp.get_clients({ id = args.data.client_id })
        if not still_attached or #still_attached == 0 then
          state.forget_client(args.data.client_id)
        end
      end
    end,
  })
  vim.api.nvim_create_autocmd('BufDelete', {
    group = g,
    callback = function(args) state.remove_buffer(args.buf) end,
  })
  -- Non-Rock buffers under the project need tags without an LSP attachment.
  vim.api.nvim_create_autocmd({ 'BufReadPost', 'BufNewFile' }, {
    group = g,
    callback = function(args)
      local root = opts_cache and tags_root(args.buf)
      if root then
        note_package_tags(args.buf, root)
      end
    end,
  })
end

return M
