-- Verifies rock.binary.resolve()'s precedence rules and shell-script peek.
--
-- Resolution order documented in the doctor: opts.binary > $ROCK_LSP_DIR
-- > cwd-walk-find crates/rock-cli/Cargo.toml > $HOME default. We exercise each
-- branch in isolation by manipulating the relevant input and clearing
-- the others.
--
-- The shell-script peek lets the user point `binary` at a wrapper like
-- rerock.sh; resolve() should read the script's first 4 KiB and pull
-- out the embedded `*/target/*/rock` path so fs-watch tracks the
-- real binary, not the script.

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

local uv = vim.uv or vim.loop
local binary = require('rock.binary')

local tmp_root = vim.fn.tempname()
vim.fn.mkdir(tmp_root, 'p')

local function write_file(path, contents, mode)
  vim.fn.mkdir(vim.fn.fnamemodify(path, ':h'), 'p')
  local fd = assert(uv.fs_open(path, 'w', mode or 420))
  uv.fs_write(fd, contents, 0)
  uv.fs_close(fd)
end

-- Save + restore env / cwd around the test.
local orig_env_dir = vim.env.ROCK_LSP_DIR
local orig_env_home = vim.env.HOME
local orig_cwd = vim.fn.getcwd()

local function restore()
  vim.env.ROCK_LSP_DIR = orig_env_dir
  vim.env.HOME = orig_env_home
  vim.cmd('cd ' .. vim.fn.fnameescape(orig_cwd))
end

-- Move to a directory with no crates/rock-cli/Cargo.toml above it so the
-- cwd-walk branch is dormant by default.
local neutral_cwd = tmp_root .. '/neutral'
vim.fn.mkdir(neutral_cwd, 'p')
vim.cmd('cd ' .. vim.fn.fnameescape(neutral_cwd))

-- Phase 1: explicit opts.binary wins over everything else.
do
  vim.env.ROCK_LSP_DIR = tmp_root .. '/env'
  local explicit = tmp_root .. '/explicit/rock'
  write_file(explicit, '\127ELF...', 493)
  local resolved, configured = binary.resolve(explicit)
  if configured ~= explicit then
    restore(); fail('configured != explicit: ' .. tostring(configured))
  end
  if resolved ~= vim.fn.resolve(explicit) then
    restore(); fail('resolved != explicit: ' .. tostring(resolved))
  end
end

-- Phase 2: $ROCK_LSP_DIR fallback when no opts.binary.
do
  vim.env.ROCK_LSP_DIR = tmp_root .. '/envroot'
  local expected = vim.env.ROCK_LSP_DIR .. '/target/debug/rock'
  local resolved, configured = binary.resolve(nil)
  if configured ~= expected then
    restore(); fail('expected env path, got configured=' .. tostring(configured))
  end
end

-- Phase 3: cwd-walk finds crates/rock-cli/Cargo.toml above the cwd (the
-- post-reorg layout). Force this by removing $ROCK_LSP_DIR and putting the
-- marker dir above us.
do
  vim.env.ROCK_LSP_DIR = nil
  local fake_repo = tmp_root .. '/repo'
  vim.fn.mkdir(fake_repo .. '/crates/rock-cli', 'p')
  write_file(fake_repo .. '/crates/rock-cli/Cargo.toml', '[package]\nname = "rock-cli"\n', 420)
  -- The local_bin only wins if it exists; create one.
  write_file(fake_repo .. '/target/debug/rock', 'fake', 493)
  vim.cmd('cd ' .. vim.fn.fnameescape(fake_repo .. '/crates/rock-cli'))
  local resolved, configured = binary.resolve(nil)
  -- Normalize via resolve(): on macOS, tempname() returns /var/... but
  -- getcwd() reports /private/var/... after symlink resolution, so
  -- find_up's result picks up the /private prefix.
  if vim.fn.resolve(configured) ~= vim.fn.resolve(fake_repo .. '/target/debug/rock') then
    restore(); fail('cwd-walk did not pick local target: ' .. tostring(configured))
  end
end

-- Phase 4: $HOME default when nothing else matches.
do
  vim.cmd('cd ' .. vim.fn.fnameescape(neutral_cwd))
  vim.env.ROCK_LSP_DIR = nil
  vim.env.HOME = tmp_root .. '/fakehome'
  local _, configured = binary.resolve(nil)
  local expected = tmp_root .. '/fakehome/projects/rock/target/debug/rock'
  if configured ~= expected then
    restore(); fail('$HOME default mismatch: ' .. tostring(configured))
  end
end

-- Phase 5: shell-script peek extracts the embedded binary path.
do
  local script_path = tmp_root .. '/rerock.sh'
  local real_bin = tmp_root .. '/realtree/target/release/rock'
  write_file(real_bin, 'fake-elf', 493)
  -- Use $HOME expansion to also exercise the gsub.
  vim.env.HOME = tmp_root
  local script = '#!/usr/bin/env bash\nexec $HOME/realtree/target/release/rock lsp serve "$@"\n'
  write_file(script_path, script, 493)

  local resolved, configured = binary.resolve(script_path)
  if configured ~= script_path then
    restore(); fail('shell-script peek changed configured (should keep script path): '
      .. tostring(configured))
  end
  if resolved ~= vim.fn.resolve(real_bin) then
    restore(); fail('shell-script peek did not extract real bin: resolved='
      .. tostring(resolved) .. ', expected=' .. tostring(real_bin))
  end
end

-- Phase 6: refresh() sets state.binary.exists correctly for present + missing.
do
  local present = tmp_root .. '/present/rock'
  write_file(present, 'x', 493)
  binary.refresh(present)
  local info = require('rock.state').get().binary
  if not info.exists then
    restore(); fail('refresh(present) did not set exists=true')
  end
  if info.sha8 == nil or #info.sha8 ~= 8 then
    restore(); fail('refresh(present) did not compute sha8')
  end

  local absent = tmp_root .. '/never-existed'
  binary.refresh(absent)
  info = require('rock.state').get().binary
  if info.exists then
    restore(); fail('refresh(absent) reported exists=true')
  end
end

restore()
print('OK: binary resolution covers explicit / env / cwd-walk / $HOME / script-peek / refresh')
os.exit(0)
