-- LSP client lifecycle, RPC traffic capture, and crash surfacing.
--
-- The `cmd` field in the server config is a function. When nvim wants to
-- start the server it calls our function, which spins up vim.lsp.rpc with
-- dispatchers that feed state.record_event for incoming traffic. We then
-- wrap the returned client's request/notify methods to capture outgoing
-- traffic. No TCP, no proxy, no server change.

local state = require('rock.state')
local binary = require('rock.binary')

local M = {}

local SERVER_NAME = 'rock'
local crash_ns = vim.api.nvim_create_namespace('rock.nvim.crash')

-- Bounded JSON summary for the event ring. Full payload is retained for the
-- panel's expand-event feature but everything is capped to avoid dragging a
-- multi-megabyte completion response around.
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

-- Set of absolute `.rock.tags` paths, one per stick.toml package the user has
-- opened a buffer in. rock-lsp writes tags per-package, so tag jumps (`<C-]>`)
-- from any rock buffer should search the UNION of every open package's tags --
-- editing a file in one package must still resolve a symbol defined in another
-- when both are open. Grows as buffers attach; never shrinks within a session.
local rock_tag_dirs = {}

-- Escape a path for inclusion in the comma-separated `tags` option (spaces and
-- commas are the only metacharacters there).
local function esc_tag_path(p)
  return (p:gsub('[, ]', '\\%0'))
end

-- The stick.toml package root whose `.rock.tags` `bufnr` should search, or nil.
-- The one gate for rock tag handling: any real file-backed buffer under a stick
-- root is "in a rock project" and gets the `.rock.tags` union regardless of
-- filetype. Scratch/unnamed buffers are excluded -- `vim.fs.root` resolves them
-- against cwd, so a `[No Name]` buffer opened with cwd inside a package would
-- otherwise have its `tags` clobbered; help/terminal/quickfix (`buftype`) have
-- no business with tags either.
local function tags_root(bufnr)
  if vim.bo[bufnr].buftype ~= '' then
    return nil
  end
  if vim.api.nvim_buf_get_name(bufnr) == '' then
    return nil
  end
  return vim.fs.root(bufnr, 'stick.toml')
end

-- Rebuild `bufnr`'s buffer-local `tags`: the buffer's own pre-rock base, then
-- every open package's `.rock.tags` (absolute, so they resolve from any cwd),
-- then the configured upward-search fallback (`opts.tags`, covers the file's own
-- package even before it's been recorded). Recomputed from the snapshotted base
-- so it never grows unboundedly across rebuilds.
local function rebuild_buffer_tags(bufnr)
  if type(opts_cache.tags) ~= 'string' or opts_cache.tags == '' then
    return
  end
  -- The buffer's `tags` before rock first touched it -- its buffer-local value
  -- if an ftplugin / gutentags set one, else the global default. Snapshotted
  -- once so a foreign buffer-local `tags` survives (we append to it, not replace
  -- it) and repeated rebuilds don't stack rock's own dirs onto themselves.
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

-- Record `bufnr`'s package (the nearest stick.toml ancestor) in the union. If
-- that package is NEW, refresh tags on EVERY open buffer that sits under a
-- stick root so an already-open buffer in another package immediately sees the
-- newcomer; otherwise refresh just this buffer. Filetype-agnostic: a `.glsl`
-- (or any) buffer under a package resolves rock tags the same as a `.rock` one.
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

-- Move `sort_text` to just before `score` in a comparator list. The rock LSP
-- emits `sortText` to rank call-argument completions by type (a variable of the
-- expected type first, then a function returning it); nvim-cmp's default order
-- puts fuzzy `score` BEFORE `sort_text`, so that ranking only broke ties. Pulling
-- `sort_text` ahead of `score` makes the server's type ranking authoritative.
-- Pure (no cmp dependency) so it's unit-testable; `sort_text`/`score` are the
-- comparator function refs to match by identity.
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

-- Reorder nvim-cmp's comparators for `bufnr` so the LSP's `sortText` wins over
-- fuzzy score (see above). Buffer-local (`cmp.setup.buffer`), preserving the
-- user's existing comparator list. No-op when nvim-cmp isn't installed.
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

-- Returning nil from cmd() leaves `client.rpc == nil` in the registry, and
-- erroring lets nvim's pcall in create_and_init_client abort cleanly without
-- inserting a half-built Client into `vim.lsp.client._all`.
local function fail_start(reason)
  error(reason, 0)
end

-- Tracks the last bin_path we surfaced a missing-binary notification for,
-- so we don't spam the user once per buffer attach. Cleared by restart()
-- and on a successful start so a real "binary disappeared" event later
-- in the session is still reported.
local missing_binary_reported = nil

local function build_cmd(bin_path)
  return function(dispatchers)
    clear_crash_marks()
    state.set_client({ state = 'starting', started_at = os.time(), last_exit = nil })

    if not bin_path or not (vim.uv or vim.loop).fs_stat(bin_path) then
      state.set_client({ state = 'crashed', last_exit = { code = -1, signal = 0, at = os.time() } })
      state.record_event('local', 'server/binary_missing', { path = bin_path })
      -- Disable autoattach for this session so further FileType events
      -- don't keep retriggering cmd() (each retry surfaces nvim's own
      -- "Client quit" + "cmd failed" notifications). Re-enabled by
      -- restart() once the user fixes the binary.
      pcall(vim.lsp.enable, SERVER_NAME, false)
      if missing_binary_reported ~= bin_path then
        missing_binary_reported = bin_path
        vim.schedule(function()
          vim.notify(('rock-lsp binary not found: %s'):format(tostring(bin_path)),
            vim.log.levels.ERROR, { title = 'rock.nvim' })
        end)
      end
      fail_start('rock-lsp binary missing: ' .. tostring(bin_path))
    end
    missing_binary_reported = nil

    local wrapped = vim.tbl_extend('force', {}, dispatchers)

    wrapped.notification = function(method, params)
      state.record_event('<-notif', method, summarize(params))
      -- Silent-error capture: window/logMessage and window/showMessage
      -- both use MessageType { 1=Error, 2=Warning, 3=Info, 4=Log }. We
      -- route Error+Warning into a dedicated ring so they surface in
      -- the panel even when nobody is tailing :LspLog.
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

    -- `rock lsp serve` is the LSP server (no separate rock-lsp binary); bone
    -- is the only typechecker backend.
    local extra = nil
    local rec_env = M.completion_record_env(opts_cache)
    if rec_env then extra = { env = rec_env } end
    local rpc = vim.lsp.rpc.start({ bin_path, 'lsp', 'serve' }, wrapped, extra)
    if not rpc then
      state.set_client({ state = 'crashed' })
      state.record_event('local', 'server/start_failed', { bin = bin_path })
      fail_start('rock-lsp failed to start: ' .. tostring(bin_path))
    end

    -- Wrap outgoing. vim.lsp.rpc clients expose request(method, params, cb) and notify(method, params).
    local orig_request = rpc.request
    local orig_notify = rpc.notify
    rpc.request = function(method, params, cb, notify_reply)
      local is_completion = method == 'textDocument/completion'
      -- Always time; per-method latency histograms want every request.
      local sent_hr = (vim.uv or vim.loop).hrtime()
      local ev_id
      -- rpc callback signature is (err, result, message_id) — see
      -- nvim's runtime/lua/vim/lsp/rpc.lua. The Client layer wraps this
      -- to build its ctx.request_id; dropping the third arg here breaks
      -- semantic tokens (handler treats every response as stale because
      -- ctx.request_id arrives nil).
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
          -- Cap stored items to keep memory bounded; full picture already
          -- available via event expand on the original response payload.
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

-- ROCK_LSP_RECORD env for the spawned server when `completion_record` is on
-- (the server-side corpus recorder; rock-lsp completion_record.rs). Pure so
-- specs can pin the resolution; vim.system merges it into the inherited env.
function M.completion_record_env(opts)
  opts = opts or {}
  if not opts.completion_record then return nil end
  local path = opts.completion_record_path
    or (vim.fn.stdpath('data') .. '/rock/completion-corpus.jsonl')
  return { ROCK_LSP_RECORD = path }
end

function M.make_config(opts)
  -- Specs (and any caller bypassing rock.setup) call us directly with
  -- a partial opts table; merge in the static defaults so completion
  -- ergonomics (completeopt/complete) apply without setup() running.
  -- `keep` preserves caller-provided values.
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
    on_attach = function(client, bufnr)
      state.set_buffer(bufnr, {
        attached = true,
        name = vim.api.nvim_buf_get_name(bufnr),
        client_id = client.id,
      })
      -- By the time on_attach fires the rpc client's pid is populated
      -- (server has responded to initialize). Capture it now.
      state.set_client({
        id = client.id,
        pid = client.rpc and client.rpc.pid or nil,
        capabilities = client.server_capabilities or {},
      })
      -- Apply buffer-local completion ergonomics. Scoped to the .rock
      -- buffer so other filetypes are untouched. See M.defaults in
      -- init.lua for rationale.
      if type(opts_cache.completeopt) == 'string' and opts_cache.completeopt ~= '' then
        vim.bo[bufnr].completeopt = opts_cache.completeopt
      end
      if type(opts_cache.complete) == 'string' then
        vim.bo[bufnr].complete = opts_cache.complete
      end
      -- Point tag jumps at the per-package `.rock.tags` files rock-lsp writes
      -- to each stick.toml root, searching the UNION across every open package
      -- (so a jump from one package resolves symbols defined in another). See
      -- M.defaults.tags and `note_package_tags`.
      note_package_tags(bufnr)
      -- Make the server's type-directed `sortText` ranking authoritative in
      -- nvim-cmp for this buffer (it orders `score` before `sort_text` by default).
      if opts_cache.cmp_type_priority then
        apply_cmp_type_priority(bufnr)
      end
      if opts_cache.on_attach then opts_cache.on_attach(client, bufnr) end
    end,
  }
end

-- Drop any "zombie" rock clients (rpc==nil) lingering in the registry from a
-- prior failed start. These are invisible to get_clients() but still iterated
-- by reuse_client_default and crash on `is_stopped`.
local function purge_zombies()
  local all = vim.lsp.client and vim.lsp.client._all
  if type(all) ~= 'table' then return 0 end
  local n = 0
  for id, c in pairs(all) do
    if c and c.name == SERVER_NAME and c.rpc == nil then
      all[id] = nil
      n = n + 1
    end
  end
  return n
end

function M.purge_zombies()
  local n = purge_zombies()
  if n > 0 then state.record_event('local', 'client/purge_zombies', { count = n }) end
  return n
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

  -- Refresh binary info so the panel shows the new hash immediately.
  binary.refresh(opts_cache and opts_cache.binary)

  -- Mirror setup()'s gate: don't re-enable autoattach if the binary is
  -- still missing -- we'd just retrigger the cmd() failure path on every
  -- buffer's FileType, producing a notification storm.
  local bin_info = state.get().binary
  if not bin_info.exists then
    vim.notify(
      ('rock-lsp binary not found at %s -- not restarting LSP.\nBuild it (`cargo build --bin rock`) and run :RockRestart.')
        :format(bin_info.path or '<unresolved>'),
      vim.log.levels.ERROR, { title = 'rock.nvim' })
    return
  end
  -- Binary is back; clear the dedup flag so a future disappearance is
  -- reported again.
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
        -- Drop the dead client's stats/modules so multi-client dumps
        -- don't accumulate frozen snapshots from clients that exited.
        -- Only fire when no other buffers are still attached to this
        -- client (LspDetach is per-buffer, not per-client).
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
  -- Point buffer-local `tags` at the package's `.rock.tags` union for ANY file
  -- under a stick root, not just `.rock` buffers -- the rock LSP only attaches
  -- to `.rock`, so shader/config/etc. buffers would otherwise never learn the
  -- tags exist ("tags not found. Generate?"). Gated on stick root so buffers
  -- outside any rock project keep their inherited `tags`.
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
