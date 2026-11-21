-- Capture writes buffer.rock, info.txt, last_completion.json and REPRO.sh;
-- the reproduction uses the attached binary path.

local state = require('rock.state')
local uv = vim.uv or vim.loop

local M = {}

local BASE_DIR = '/tmp/rock-nvim-captures'

local function mkdir_p(path)
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

-- Safe in insert mode: buffer APIs preserve mode and I/O is scheduled.
-- opts.note tags the capture; opts.base_dir overrides the destination.
function M.capture(opts)
  opts = opts or {}
  local bufnr = vim.api.nvim_get_current_buf()
  local buf_name = vim.api.nvim_buf_get_name(bufnr)
  local basename = buf_name ~= '' and vim.fn.fnamemodify(buf_name, ':t') or ('buf-' .. bufnr)

  -- Snapshot before yielding so later typing cannot change it.
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

  state.add_capture(summary)

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
