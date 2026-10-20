-- Insert-mode bug-capture snapshot.
--
-- When something looks wrong in the completion list or inlay hints and
-- you don't want to break your typing context to diagnose, press the
-- configured hotkey (default <C-g>c in insert mode) and we'll:
--
--   * Stat the current buffer + cursor WITHOUT leaving insert mode.
--   * Pair it with the most recent textDocument/completion response
--     already captured in state (usually <100ms old).
--   * Write a self-contained capture directory to /tmp so it survives
--     across sessions and tmux detach.
--   * Push a summary into state.captures so the side panel renders it
--     (tmux-selectable).
--
-- Shape of the on-disk capture:
--   /tmp/rock-nvim-captures/<ts>-<basename>/
--     buffer.rock              full source at capture time
--     info.txt                 human summary (cursor, typed prefix,
--                              client state, binary id, last completion)
--     last_completion.json     most recent textDocument/completion resp
--     REPRO.sh                 runs `rock lsp completion PATH:L:C`
--
-- The REPRO.sh takes the binary path from state so branch/worktree
-- mismatch is automatically reflected.

local state = require('rock.state')
local uv = vim.uv or vim.loop

local M = {}

local BASE_DIR = '/tmp/rock-nvim-captures'

local function mkdir_p(path)
  -- Equivalent to `mkdir -p`. Walks segments, creates each if missing.
  local parts = vim.split(path, '/', { plain = true, trimempty = true })
  local acc = (path:sub(1, 1) == '/') and '' or '.'
  for _, p in ipairs(parts) do
    acc = acc .. '/' .. p
    local st = uv.fs_stat(acc)
    if not st then
      local ok = uv.fs_mkdir(acc, 493)  -- 0755
      if not ok then return false end
    end
  end
  return true
end

local function write_file(path, data)
  local fd = uv.fs_open(path, 'w', 420)  -- 0644
  if not fd then return false end
  uv.fs_write(fd, data, 0)
  uv.fs_close(fd)
  return true
end

local function sanitize(name)
  -- Strip path separators and weird chars so we can use it in a
  -- directory name without escaping.
  return (name:gsub('[/\\:]', '_'))
end

-- Main entry point. Safe to call from insert-mode mappings (works
-- entirely off nvim_buf_* which do not alter the mode, and schedules
-- I/O so the keystroke returns immediately).
-- opts.note: free-text tag stored in the summary + info.txt ("bad:
-- suggested T? where T expected") -- how a mis-ranked completion gets
-- flagged for later corpus work. opts.base_dir overrides BASE_DIR.
function M.capture(opts)
  opts = opts or {}
  local bufnr = vim.api.nvim_get_current_buf()
  local buf_name = vim.api.nvim_buf_get_name(bufnr)
  local basename = buf_name ~= '' and vim.fn.fnamemodify(buf_name, ':t') or ('buf-' .. bufnr)

  -- Capture everything we need synchronously before yielding -- the
  -- user is in the middle of typing; don't risk the buffer mutating
  -- between the snapshot and its use.
  local cursor = vim.api.nvim_win_get_cursor(0)
  local row, col = cursor[1], cursor[2]  -- 1-indexed row, 0-indexed col
  local lines = vim.api.nvim_buf_get_lines(bufnr, 0, -1, false)
  local line_content = lines[row] or ''
  local typed_prefix = line_content:sub(math.max(1, col - 79), col)
  local doc_version = vim.lsp.util.buf_versions and vim.lsp.util.buf_versions[bufnr] or nil
  local ts = os.time()
  local ts_ms = math.floor(uv.hrtime() / 1e6) % 1000
  local s = state.get()
  local last_completion = s.last_completion
  -- Episode state: the struggle so far, not
  -- just the last response. Read NOW -- cmp events mutate it constantly.
  local episode = nil
  do
    local ok, cl = pcall(require, 'rock.completion_log')
    if ok and type(cl.episode_snapshot) == 'function' then
      local ok_ep, ep = pcall(cl.episode_snapshot)
      if ok_ep then episode = ep end
    end
  end

  local dir = ('%s/%d-%s'):format(opts.base_dir or BASE_DIR, ts, sanitize(basename))
  local summary = {
    ts = ts,
    wall = os.date('%Y-%m-%d %H:%M:%S'),
    dir = dir,
    file = buf_name,
    basename = basename,
    note = opts.note,
    cursor = { row = row, col = col },
    typed_prefix = typed_prefix,
    doc_version = doc_version,
    last_completion = last_completion and {
      wall = last_completion.wall,
      item_count = last_completion.item_count,
      latency_ms = last_completion.latency_ms,
      trigger_char = last_completion.trigger_char,
      age_s = (os.time() - (last_completion.at or os.time())),
    } or nil,
    binary_id = s.binary.sha8,
  }

  -- Push to in-memory ring NOW so the panel updates immediately even
  -- before disk I/O completes.
  state.add_capture(summary)

  -- The disk write can happen off the keystroke's critical path. We
  -- already captured all the data needed into locals.
  vim.schedule(function()
    if not mkdir_p(dir) then
      vim.notify('rock.nvim: failed to create ' .. dir, vim.log.levels.ERROR,
        { title = 'rock.nvim' })
      return
    end
    write_file(dir .. '/buffer.rock', table.concat(lines, '\n') .. '\n')

    local info_lines = {
      'rock.nvim capture',
      'time:         ' .. summary.wall,
      'note:         ' .. (summary.note or '-'),
      'file:         ' .. (buf_name ~= '' and buf_name or '(no name)'),
      'cursor:       ' .. row .. ':' .. (col + 1),
      'typed prefix: |' .. typed_prefix .. '|',
      'doc version:  ' .. tostring(doc_version),
      'binary id:    ' .. tostring(s.binary.sha8),
      'binary path:  ' .. tostring(s.binary.path),
      'client state: ' .. tostring(s.client.state),
      'restarts:     ' .. tostring(s.client.restart_count),
    }
    if last_completion then
      table.insert(info_lines, '')
      table.insert(info_lines, 'last textDocument/completion:')
      table.insert(info_lines, '  wall:       ' .. tostring(last_completion.wall))
      table.insert(info_lines, '  items:      ' .. tostring(last_completion.item_count))
      table.insert(info_lines, '  latency_ms: ' .. tostring(last_completion.latency_ms))
      table.insert(info_lines, '  trigger:    ' .. tostring(last_completion.trigger_char or last_completion.trigger_kind))
      table.insert(info_lines, '  age_s:      ' .. summary.last_completion.age_s)
    end
    write_file(dir .. '/info.txt', table.concat(info_lines, '\n') .. '\n')

    if last_completion then
      local ok, encoded = pcall(vim.json.encode, last_completion)
      if ok then write_file(dir .. '/last_completion.json', encoded) end
    end

    if episode then
      local ok, encoded = pcall(vim.json.encode, episode)
      if ok then write_file(dir .. '/episode.json', encoded) end
    end

    local repro_path = dir .. '/buffer.rock'
    local repro = {
      '#!/usr/bin/env bash',
      '# Reproduce the completion request that was active when this',
      '# capture was taken, using the same binary the LSP was hitting.',
      '',
      ('BIN=%q'):format(tostring(s.binary.path or 'rock')),
      ('FILE=%q'):format(repro_path),
      ('LINE=%d'):format(row),
      ('COL=%d'):format(col + 1),
      '',
      'echo "[repro] binary: $BIN"',
      'echo "[repro] file:   $FILE:$LINE:$COL"',
      '"$BIN" lsp completion "$FILE:$LINE:$COL"',
      '',
      '# The diagnosis view: each item with its RankKey rungs decoded',
      '# (exact/type/band/case/kind/dist/.../prox/freq/rec).',
      'echo; echo "[repro] --explain:"',
      '"$BIN" lsp completion "$FILE:$LINE:$COL" --explain',
      '',
    }
    write_file(dir .. '/REPRO.sh', table.concat(repro, '\n'))
    uv.fs_chmod(dir .. '/REPRO.sh', 493)  -- 0755

    if not opts.silent then
      vim.notify(('rock.nvim: captured %s:%d:%d -> %s'):format(
        basename, row, col + 1, dir),
        vim.log.levels.INFO, { title = 'rock.nvim' })
    end
  end)

  return summary
end

return M
