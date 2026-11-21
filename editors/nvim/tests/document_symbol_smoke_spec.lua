-- End-to-end smoke test that the server serves document symbols.
--
-- Requires $ROCK_LSP_BIN and skips when unset.

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

local project = dofile('tests/_lib/project.lua')
local buf = project.open('symbol_smoke', { ['main.rock'] = [[
struct Player {
    health: int,
}

fn heal(p: Player) -> Player {
    return p
}

fn main() {
    let x = 1
}
]] })
local doc_res = project.request(buf, 'textDocument/documentSymbol')

local names = {}
for _, s in ipairs(doc_res or {}) do
  names[s.name] = true
end
for _, want in ipairs({ 'Player', 'heal', 'main' }) do
  if not names['symbol_smoke::' .. want] then
    fail(('missing %q in document symbols (got: %s)')
      :format(want, vim.inspect(doc_res)))
  end
end

print('OK: rock.nvim serves document symbols (Player, heal, main)')
os.exit(0)
