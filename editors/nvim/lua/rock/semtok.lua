-- Dump Neovim's cached LSP semantic tokens for a range.
--
-- The tokens come from vim.lsp.semantic_tokens.get_at_pos(), which reads
-- the client's decoded token cache populated by actual LSP responses.
-- This is the ground truth for "what tokens did the server send?" — not
-- what the CLI produces, not what the syntax file matches, but what
-- Neovim itself received and is using for highlighting.
--
-- Commands:
--   :RockSemTokens         -- current line
--   :'<,'>RockSemTokens    -- visual range
--   :RockSemTokensAll      -- whole buffer (may be large)
--
-- Output is a floating scratch buffer with plain text formatted for
-- pasting back into a conversation.

local M = {}

local function client_for(bufnr)
  local clients = vim.lsp.get_clients({ bufnr = bufnr, name = 'rock' })
  return clients[1]
end

-- Resolve what highlight group is actually applied at (row, col) in bufnr.
-- Uses get_captures_at_pos() which reports every source (syntax/treesitter/LSP/extmarks).
local function captures_at(bufnr, row, col)
  local ok, caps = pcall(vim.inspect_pos, bufnr, row, col)
  if not ok or not caps then return {} end
  local out = {}
  for _, e in ipairs(caps.semantic_tokens or {}) do
    table.insert(out, 'sem:' .. (e.opts and e.opts.hl_group or '?'))
  end
  for _, e in ipairs(caps.treesitter or {}) do
    table.insert(out, 'ts:' .. (e.capture or '?'))
  end
  for _, e in ipairs(caps.syntax or {}) do
    table.insert(out, 'syn:' .. (e.hl_group or e.hl_group_link or '?'))
  end
  for _, e in ipairs(caps.extmarks or {}) do
    local hl = e.opts and (e.opts.hl_group or e.opts.hl_group_link)
    if hl then table.insert(out, 'ext:' .. hl) end
  end
  return out
end

-- Read the decoded tokens Neovim has cached for this buffer, filter to
-- the rock client and the requested row range. This is the ground truth
-- for "what tokens did the server send?" — one entry per server token,
-- no run merging, no column scanning artefacts.
local function collect_tokens(bufnr, start_row, end_row)
  local client = client_for(bufnr)
  if not client then
    return nil, 'no rock LSP client attached to buffer'
  end
  local h = vim.lsp.semantic_tokens.__STHighlighter
  local entry = h and h.active and h.active[bufnr]
  local cs = entry and entry.client_state and entry.client_state[client.id]
  local highlights = cs and cs.current_result and cs.current_result.highlights
  if not highlights then
    return nil, 'no decoded highlights cached (did the response arrive?)'
  end
  local tokens = {}
  for _, hl in ipairs(highlights) do
    if hl.line >= start_row and hl.line <= end_row then
      local line = vim.api.nvim_buf_get_lines(bufnr, hl.line, hl.line + 1, false)[1] or ''
      local text = line:sub(hl.start_col + 1, hl.end_col)
      table.insert(tokens, {
        row = hl.line,
        start_col = hl.start_col,
        end_col = hl.end_col,
        type = hl.type,
        modifiers = (function()
          -- hl.modifiers is a map { name = true }; flatten to list.
          local list = {}
          for k, v in pairs(hl.modifiers or {}) do
            if v then table.insert(list, k) end
          end
          table.sort(list)
          return list
        end)(),
        text = text,
        captures = captures_at(bufnr, hl.line, hl.start_col),
      })
    end
  end
  table.sort(tokens, function(a, b)
    if a.row ~= b.row then return a.row < b.row end
    return a.start_col < b.start_col
  end)
  return tokens
end

local function format_tokens(bufnr, tokens, start_row, end_row)
  local lines = {
    '# rock-nvim semantic tokens dump',
    ('buffer: %s'):format(vim.api.nvim_buf_get_name(bufnr)),
    ('rows: %d..%d (1-indexed: %d..%d)'):format(
      start_row, end_row, start_row + 1, end_row + 1),
    ('tokens: %d'):format(#tokens),
    '',
    '## source lines',
  }
  for i = start_row, end_row do
    local text = (vim.api.nvim_buf_get_lines(bufnr, i, i + 1, false)[1] or '')
    table.insert(lines, ('%4d | %s'):format(i + 1, text))
  end
  table.insert(lines, '')
  table.insert(lines, '## tokens (from Neovim\'s LSP cache — this is what the server actually sent)')
  for _, t in ipairs(tokens) do
    local mods = #t.modifiers > 0 and ('[' .. table.concat(t.modifiers, ',') .. ']') or ''
    local caps = #t.captures > 0 and ('  {' .. table.concat(t.captures, ', ') .. '}') or ''
    table.insert(lines, ('%4d:%-3d len=%-3d  %-14s %-18s  %q%s'):format(
      t.row + 1, t.start_col, t.end_col - t.start_col,
      t.type, mods, t.text, caps))
  end
  table.insert(lines, '')
  table.insert(lines, '## positions without any rock LSP semantic token')
  -- Scan gaps — positions where highlight comes from syntax/treesitter only.
  for i = start_row, end_row do
    local text = vim.api.nvim_buf_get_lines(bufnr, i, i + 1, false)[1] or ''
    local col = 0
    while col < #text do
      local toks = vim.lsp.semantic_tokens.get_at_pos(bufnr, i, col) or {}
      local has_rock = false
      local client = client_for(bufnr)
      if client then
        for _, t in ipairs(toks) do
          if t.client_id == client.id then has_rock = true; break end
        end
      end
      if not has_rock and text:sub(col + 1, col + 1):match('%S') then
        local caps = captures_at(bufnr, i, col)
        if #caps > 0 then
          table.insert(lines, ('%4d:%-3d  %q  {%s}'):format(
            i + 1, col, text:sub(col + 1, col + 1), table.concat(caps, ', ')))
        end
      end
      col = col + 1
    end
  end
  return lines
end

local function open_float(lines)
  -- nvim_buf_set_lines rejects embedded newlines; flatten any multi-line
  -- strings produced by vim.inspect into separate lines.
  local flat = {}
  for _, l in ipairs(lines) do
    if type(l) ~= 'string' then l = tostring(l) end
    if l:find('\n', 1, true) then
      for piece in (l .. '\n'):gmatch('([^\n]*)\n') do
        table.insert(flat, piece)
      end
    else
      table.insert(flat, l)
    end
  end
  local buf = vim.api.nvim_create_buf(false, true)
  vim.api.nvim_buf_set_lines(buf, 0, -1, false, flat)
  lines = flat
  vim.bo[buf].bufhidden = 'wipe'
  vim.bo[buf].filetype = 'markdown'
  local width = math.min(vim.o.columns - 4, 120)
  local height = math.min(vim.o.lines - 4, #lines + 2)
  local win = vim.api.nvim_open_win(buf, true, {
    relative = 'editor',
    width = width,
    height = height,
    row = math.floor((vim.o.lines - height) / 2),
    col = math.floor((vim.o.columns - width) / 2),
    style = 'minimal',
    border = 'rounded',
    title = ' RockSemTokens ',
    title_pos = 'center',
  })
  vim.keymap.set('n', 'q', '<cmd>close<cr>', { buffer = buf, silent = true })
  vim.keymap.set('n', '<esc>', '<cmd>close<cr>', { buffer = buf, silent = true })
  return win, buf
end

function M.dump(start_row, end_row)
  local bufnr = vim.api.nvim_get_current_buf()
  if vim.bo[bufnr].filetype ~= 'rock' then
    vim.notify('RockSemTokens: not a rock buffer', vim.log.levels.WARN)
    return
  end
  local line_count = vim.api.nvim_buf_line_count(bufnr)
  start_row = math.max(0, math.min(start_row, line_count - 1))
  end_row = math.max(start_row, math.min(end_row, line_count - 1))
  local tokens, err = collect_tokens(bufnr, start_row, end_row)
  if not tokens then
    vim.notify('RockSemTokens: ' .. tostring(err), vim.log.levels.ERROR)
    return
  end
  local lines = format_tokens(bufnr, tokens, start_row, end_row)
  open_float(lines)
end

-- Probe Neovim's internal semantic-tokens highlighter state for the
-- current buffer. Answers: is the highlighter attached? how many
-- decoded highlights are in the cache? This exists because
-- get_at_pos() returning empty doesn't distinguish "no highlighter"
-- from "highlighter attached but empty result."
function M.debug()
  local bufnr = vim.api.nvim_get_current_buf()
  local lines = {
    '# RockSemTokensDebug',
    ('buffer: %s'):format(vim.api.nvim_buf_get_name(bufnr)),
    ('nvim: %s'):format(tostring(vim.version())),
    '',
  }

  local clients = vim.lsp.get_clients({ bufnr = bufnr, name = 'rock' })
  table.insert(lines, ('rock clients attached: %d'):format(#clients))
  for _, c in ipairs(clients) do
    local caps = c.server_capabilities and c.server_capabilities.semanticTokensProvider
    table.insert(lines, ('  client id=%d  sem_tokens_provider=%s'):format(
      c.id, caps and 'yes' or 'no'))
  end
  table.insert(lines, '')

  -- vim.lsp.semantic_tokens internals: __STHighlighter.active[bufnr].client_state[client_id]
  local h = vim.lsp.semantic_tokens.__STHighlighter
  if not h or not h.active then
    table.insert(lines, 'NO __STHighlighter.active -- neovim internals changed?')
  else
    local entry = h.active[bufnr]
    if not entry then
      table.insert(lines, 'NO HIGHLIGHTER ATTACHED to this buffer')
      table.insert(lines, ('h.active has %d buffers: %s'):format(
        vim.tbl_count(h.active),
        table.concat(vim.tbl_keys(h.active), ',')))
    else
      table.insert(lines, 'highlighter attached')
      table.insert(lines, ('  client_state entries: %d'):format(
        vim.tbl_count(entry.client_state or {})))
      for cid, cs in pairs(entry.client_state or {}) do
        local hl_count = cs.current_result and cs.current_result.highlights
          and #cs.current_result.highlights or 'nil'
        local version = cs.current_result and cs.current_result.version or 'nil'
        local data_len = cs.current_result and cs.current_result.data
          and #cs.current_result.data or 'nil'
        local has_err = cs.current_result and cs.current_result.err ~= nil
        table.insert(lines, ('  client id=%d  highlights=%s  version=%s  data_len=%s  err=%s'):format(
          cid, tostring(hl_count), tostring(version), tostring(data_len), tostring(has_err)))
        if cs.current_result and cs.current_result.err then
          table.insert(lines, '  err detail: ' .. vim.inspect(cs.current_result.err))
        end
        -- If highlights exist, sample first 5.
        if type(hl_count) == 'number' and hl_count > 0 then
          table.insert(lines, '  first highlights:')
          for i = 1, math.min(5, hl_count) do
            local hl = cs.current_result.highlights[i]
            table.insert(lines, '    ' .. vim.inspect(hl))
          end
        end
      end
    end
  end

  table.insert(lines, '')
  table.insert(lines, 'forcing refresh; results will update in 1s...')

  -- Fire refresh then re-probe.
  pcall(vim.lsp.semantic_tokens.force_refresh, bufnr)
  vim.defer_fn(function()
    local h2 = vim.lsp.semantic_tokens.__STHighlighter
    table.insert(lines, '')
    table.insert(lines, '## after force_refresh (1s later)')
    local entry = h2 and h2.active and h2.active[bufnr]
    if not entry then
      table.insert(lines, 'still no highlighter')
    else
      -- Dump outer keys of the per-buffer highlighter entry too — in 0.12
      -- the decoded state may live directly on `entry` instead of inside
      -- client_state.
      table.insert(lines, 'entry top-level keys: ' ..
        table.concat(vim.tbl_keys(entry), ','))
      for cid, cs in pairs(entry.client_state or {}) do
        table.insert(lines, ('  client id=%d  cs keys: %s'):format(
          cid, table.concat(vim.tbl_keys(cs), ',')))
        -- Deep-dump cs with truncation to keep the float sane.
        local dump = vim.inspect(cs, { depth = 4 })
        if #dump > 4000 then dump = dump:sub(1, 4000) .. '\n...(truncated)' end
        for l in dump:gmatch('[^\n]+') do
          table.insert(lines, '    ' .. l)
        end
      end
      -- Also sample any extmarks in the LSP semantic tokens namespace(s)
      -- to see if highlights got applied even without a populated cache.
      local namespaces = vim.api.nvim_get_namespaces()
      for ns_name, ns_id in pairs(namespaces) do
        if ns_name:match('semantic') or ns_name:match('lsp') then
          local marks = vim.api.nvim_buf_get_extmarks(bufnr, ns_id, 0, -1,
            { details = true, limit = 5 })
          if #marks > 0 then
            table.insert(lines, ('namespace %s (id=%d): %d marks sampled'):format(
              ns_name, ns_id, #marks))
            for _, m in ipairs(marks) do
              table.insert(lines, '  ' .. vim.inspect(m))
            end
          end
        end
      end
    end
    open_float(lines)
  end, 1200)
end

-- Hook tracing buffer. Tail this with :RockSemTokensTrace.
M._trace = {}
local TRACE_CAP = 500
local TRACE_SLACK = 128
local function trc(msg)
  local t = M._trace
  t[#t + 1] = ('%s %s'):format(os.date('%H:%M:%S'), msg)
  if #t > TRACE_CAP + TRACE_SLACK then
    local drop = #t - TRACE_CAP
    local n = #t
    for i = 1, n - drop do t[i] = t[i + drop] end
    for i = n - drop + 1, n do t[i] = nil end
  end
end

-- Monkey-patch Neovim's semantic tokens internals so we can tell whether
-- process_response is even being called, and whether tokens_to_ranges is
-- returning. This lets us distinguish "response never processed" from
-- "coroutine abandoned mid-decode" from "decode completes but cache reset".
function M.install_hooks()
  local st = vim.lsp.semantic_tokens
  local STH = st.__STHighlighter
  if not STH or STH.__rock_hooked then return end
  STH.__rock_hooked = true

  local orig_process = STH.process_response
  STH.process_response = function(self, response, client, request_id, version, is_range_request)
    local buf_ver = require('vim.lsp.util').buf_versions[self.bufnr]
    trc(('process_response ENTER buf=%d req=%s ver=%s buf_ver=%s data_len=%s'):format(
      self.bufnr, tostring(request_id), tostring(version), tostring(buf_ver),
      tostring(response and response.data and #response.data)))
    local ok, err = pcall(orig_process, self, response, client, request_id, version, is_range_request)
    local state = self.client_state[client.id] or {}
    local cr = state.current_result or {}
    trc(('process_response EXIT  ok=%s err=%s hl=%s tok_stored=%s active_req=%s'):format(
      tostring(ok), tostring(err),
      tostring(cr.highlights and #cr.highlights),
      tostring(cr.tokens and #cr.tokens),
      tostring(state.active_request and state.active_request.request_id)))
  end

  trc('hooks installed')
end

function M.show_trace()
  local lines = { '# RockSemTokensTrace', ('entries: %d'):format(#M._trace), '' }
  for _, l in ipairs(M._trace) do table.insert(lines, l) end
  open_float(lines)
end

function M.setup()
  vim.api.nvim_create_user_command('RockSemTokensDebug', function()
    M.debug()
  end, { desc = 'rock.nvim: diagnose semantic tokens highlighter state' })
  vim.api.nvim_create_user_command('RockSemTokensHook', function()
    M.install_hooks()
    vim.notify('rock.nvim: process_response hooks installed; now trigger activity, then :RockSemTokensTrace')
  end, { desc = 'rock.nvim: install process_response trace hooks' })
  vim.api.nvim_create_user_command('RockSemTokensTrace', function()
    M.show_trace()
  end, { desc = 'rock.nvim: show process_response trace log' })
  vim.api.nvim_create_user_command('RockSemTokens', function(args)
    -- With a range (including visual): line1..line2 are 1-indexed.
    -- Without a range: both default to current line.
    local s = (args.line1 or 1) - 1
    local e = (args.line2 or args.line1 or 1) - 1
    M.dump(s, e)
  end, {
    range = true,
    desc = 'rock.nvim: dump LSP semantic tokens for range/line',
  })
  vim.api.nvim_create_user_command('RockSemTokensAll', function()
    local bufnr = vim.api.nvim_get_current_buf()
    M.dump(0, vim.api.nvim_buf_line_count(bufnr) - 1)
  end, { desc = 'rock.nvim: dump LSP semantic tokens for whole buffer' })
end

return M
