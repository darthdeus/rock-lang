-- Bug-capture snapshot (capture.lua): the on-disk capture must carry the
-- user's note (":RockCapture bad: nullable ignored" -- how a mis-ranked
-- completion gets flagged for later corpus work) and a REPRO.sh that also
-- runs `--explain` (the RankKey rung decode is the diagnosis, not just the
-- item list).

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

vim.notify = function() end
vim.notify_once = function() end

-- Stub the completion log: capture must dump its episode snapshot as
-- episode.json (the struggle so far), read SYNCHRONOUSLY at capture time.
package.loaded['rock.completion_log'] = {
  episode_snapshot = function()
    return { recent = { { event = 'dismiss', post_word = 'spawn_mob' } } }
  end,
}

local tmp = vim.fn.tempname()
vim.fn.mkdir(tmp, 'p')
local fname = tmp .. '/thing.rock'
local f = assert(io.open(fname, 'w'))
f:write('fn main() {\n    let x = mob_k\n}\n'); f:close()
vim.cmd('edit ' .. vim.fn.fnameescape(fname))
vim.api.nvim_win_set_cursor(0, { 2, 17 })

local capture = require('rock.capture')
local summary = capture.capture({
  silent = true,
  base_dir = tmp .. '/captures',
  note = 'bad: suggested T? where T expected',
})

if not summary or not summary.dir then fail('capture returned no summary.dir') end
if summary.note ~= 'bad: suggested T? where T expected' then
  fail('summary.note missing, got ' .. vim.inspect(summary.note))
end
if not summary.dir:find(tmp .. '/captures', 1, true) then
  fail('base_dir not respected: ' .. summary.dir)
end

-- Disk writes are scheduled off the keystroke path; wait for the last file.
local repro_path = summary.dir .. '/REPRO.sh'
if not vim.wait(2000, function() return vim.fn.filereadable(repro_path) == 1 end, 10) then
  fail('REPRO.sh never written to ' .. summary.dir)
end

local function slurp(p)
  local fh = assert(io.open(p, 'r'))
  local s = fh:read('*a'); fh:close(); return s
end

local info = slurp(summary.dir .. '/info.txt')
if not info:find('bad: suggested T? where T expected', 1, true) then
  fail('info.txt missing the note:\n' .. info)
end

local repro = slurp(repro_path)
if not repro:find('--explain', 1, true) then
  fail('REPRO.sh missing the --explain run:\n' .. repro)
end

local buffer = slurp(summary.dir .. '/buffer.rock')
if not buffer:find('let x = mob_k', 1, true) then
  fail('buffer.rock missing source:\n' .. buffer)
end

local episode = slurp(summary.dir .. '/episode.json')
if not episode:find('spawn_mob', 1, true) then
  fail('episode.json missing the episode snapshot:\n' .. episode)
end

print('PASS  capture_spec')
os.exit(0)
