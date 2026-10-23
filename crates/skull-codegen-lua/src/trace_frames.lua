#!/usr/bin/env luajit

local bit = require("bit")
local util = require("jit.util")
local vmdef = require("jit.vmdef")

local function fail(message)
  io.stderr:write("trace_frames: ", tostring(message), "\n")
  os.exit(1)
end

local function is_identifier(value)
  return type(value) == "string"
    and value:match("^[A-Za-z_][A-Za-z0-9_]*$") ~= nil
end

local function long_bracket_open(source, start)
  if source:sub(start, start) ~= "[" then return nil end
  local at = start + 1
  while source:sub(at, at) == "=" do at = at + 1 end
  if source:sub(at, at) ~= "[" then return nil end
  local equals = source:sub(start + 1, at - 1)
  return at + 1, "]" .. equals .. "]"
end

local function record_header(headers, source, first, last, line_no)
  local line = source:sub(first, last)
  local outer = line:match(
    "^[ \t]*(__rock_mk_[A-Za-z_][A-Za-z0-9_]*)[ \t]*=[ \t]*function[ \t]*%("
  )
  if outer then
    local inner = line:match("return%s+function%s*%(")
      and outer:match("^__rock_mk_(.+)$") or nil
    headers[line_no] = { outer = outer, inner = inner }
  else
    local name = line:match("^[ \t]*function[ \t]+([A-Za-z_][A-Za-z0-9_]*)[ \t]*%(")
    local scoped = false
    if not name then
      scoped = true
      name = line:match(
        "^[ \t]*local[ \t]+function[ \t]+([A-Za-z_][A-Za-z0-9_]*)[ \t]*%("
      )
    end
    if is_identifier(name) then headers[line_no] = { name = name, scoped = scoped } end
  end
end

local function strip_directives(source)
  local out = {}
  local headers = {}
  local at = 1
  local line_start = 1
  local line_no = 1
  local copy_start = 1
  local quote = nil
  local long_close = nil
  local line_comment = false

  while at <= #source do
    local stripped = false
    if at == line_start and not quote and not long_close and not line_comment then
      local first = at
      while source:byte(first) == 32 or source:byte(first) == 9 do
        first = first + 1
      end
      local first_byte = source:byte(first)
      if source:byte(at) == 35 then
        local newline = source:find("\n", at, true)
        local line_end = newline and newline - 1 or #source
        local line = source:sub(at, line_end)
        if line:match("^#%s+%d+") then
          stripped = true
          if copy_start < at then out[#out + 1] = source:sub(copy_start, at - 1) end
          out[#out + 1] = string.rep(" ", #line)
          if newline then
            out[#out + 1] = "\n"
            at = newline + 1
            line_start = at
            line_no = line_no + 1
            copy_start = at
          else
            at = #source + 1
            copy_start = at
          end
        end
      elseif first_byte == 102 or first_byte == 108 or first_byte == 95 then
        local newline = source:find("\n", at, true)
        local line_end = newline and newline - 1 or #source
        record_header(headers, source, at, line_end, line_no)
      end
    end

    if at <= #source and not stripped then
      local ch = source:byte(at)
      if line_comment then
        if ch == 10 then
          line_comment = false
          line_start = at + 1
          line_no = line_no + 1
        end
        at = at + 1
      elseif long_close then
        if ch == 93 and source:sub(at, at + #long_close - 1) == long_close then
          at = at + #long_close
          long_close = nil
        else
          if ch == 10 then
            line_start = at + 1
            line_no = line_no + 1
          end
          at = at + 1
        end
      elseif quote then
        if ch == 92 then
          if at < #source then
            if source:byte(at + 1) == 10 then
              line_start = at + 2
              line_no = line_no + 1
            end
            at = at + 2
          else
            at = at + 1
          end
        else
          if ch == quote then quote = nil end
          if ch == 10 then
            line_start = at + 1
            line_no = line_no + 1
          end
          at = at + 1
        end
      elseif ch == 45 and source:byte(at + 1) == 45 then
        local after, close = long_bracket_open(source, at + 2)
        if after then
          long_close = close
          at = after
        else
          line_comment = true
          at = at + 2
        end
      elseif ch == 91 then
        local after, close = long_bracket_open(source, at)
        if after then
          long_close = close
          at = after
        else
          at = at + 1
        end
      elseif ch == 39 or ch == 34 then
        quote = ch
        at = at + 1
      else
        if ch == 10 then
          line_start = at + 1
          line_no = line_no + 1
        end
        at = at + 1
      end
    end
  end

  if #out == 0 then return source, headers end
  if copy_start <= #source then out[#out + 1] = source:sub(copy_start) end
  return table.concat(out), headers
end

local function resolve_name(id, info, parent, headers)
  if id == 0 then return "<chunk>", nil, false end

  local line_no = tonumber(info.linedefined) or 0
  if parent and parent.factory_inner
      and line_no == (tonumber(parent.info.linedefined) or 0) then
    return parent.factory_inner, nil, true
  end

  local header = headers[line_no]
  if header and header.outer then return header.outer, header.inner, false end
  if header and header.scoped then
    return "<local:" .. header.name .. ":" .. tostring(id) .. ">", nil, false
  end
  if header and is_identifier(header.name) then return header.name, nil, false end
  return "<anonymous:" .. tostring(id) .. ">", nil, false
end

local function opcode_name(instruction)
  local opcode = bit.band(instruction, 0xff)
  local first = opcode * 6 + 1
  local name = vmdef.bcnames:sub(first, first + 5):match("%S+")
  if not name then fail("unknown bytecode opcode " .. tostring(opcode)) end
  return name
end

local function instruction_fields(instruction)
  return bit.band(bit.rshift(instruction, 8), 0xff),
    bit.band(bit.rshift(instruction, 16), 0xffff)
end

local function child_proto(proto)
  local info = util.funcinfo(proto)
  if not info then fail("jit.util.funcinfo returned no prototype information") end
  local children = {}
  for slot = -1, -(info.gcconsts or 0), -1 do
    local child = util.funck(proto, slot)
    if type(child) == "proto" then
      children[#children + 1] = { slot = slot, proto = child }
    end
  end
  return info, children
end

local function frame_links()
  local chunk, err = loadstring(
    "return function(x) return f(x) end",
    "=(trace_frames_layout_probe)"
  )
  if not chunk then fail("cannot parse frame-layout probe: " .. tostring(err)) end

  local _, children = child_proto(chunk)
  local body = nil
  for _, child in ipairs(children) do
    if body then fail("frame-layout probe has multiple function children") end
    body = child.proto
  end
  if not body then fail("frame-layout probe has no function child") end

  local info = util.funcinfo(body)
  local mov_a = nil
  local call_a = nil
  for pc = 1, (info.bytecodes or 1) - 1 do
    local instruction = util.funcbc(body, pc)
    if not instruction then fail("cannot read frame-layout probe bytecode") end
    local op = opcode_name(instruction)
    local a, d = instruction_fields(instruction)
    if op == "MOV" and d == 0 then
      if mov_a then fail("frame-layout probe has multiple copies of x") end
      mov_a = a
    elseif op == "CALLT" then
      if call_a then fail("frame-layout probe has multiple tail calls") end
      call_a = a
    end
  end
  if not mov_a or not call_a then
    fail("frame-layout probe lacks the expected MOV/CALLT pair")
  end

  local links = mov_a - call_a
  if links < 1 then fail("frame-layout probe derived an invalid frame-link count") end
  return links
end

local JUMPS = {
  JMP = true, UCLO = true,
  FORI = true, JFORI = true, FORL = true, IFORL = true, JFORL = true,
  ITERL = true, IITERL = true, JITERL = true,
  LOOP = true, ILOOP = true, JLOOP = true,
}

local TESTS = {
  ISLT = true, ISGE = true, ISLE = true, ISGT = true,
  ISEQV = true, ISNEV = true, ISEQS = true, ISNES = true,
  ISEQN = true, ISNEN = true, ISEQP = true, ISNEP = true,
  ISTC = true, ISFC = true, IST = true, ISF = true,
  ISTYPE = true, ISNUM = true, ISNEXT = true,
}

local TERMINATORS = {
  RET = true, RET0 = true, RET1 = true, RETM = true,
  CALLT = true, CALLMT = true,
}

local CALLS = { CALL = true, CALLM = true, CALLT = true, CALLMT = true }

local function scan_calls(entry)
  local info = entry.info
  local code = {}
  local count = (info.bytecodes or 1) - 1
  for pc = 1, count do
    local instruction, mode = util.funcbc(entry.proto, pc)
    if type(instruction) ~= "number" or type(mode) ~= "number" then
      fail("cannot read bytecode for prototype " .. tostring(entry.id))
    end
    local op = opcode_name(instruction)
    local a, d = instruction_fields(instruction)
    code[pc] = { pc = pc, instruction = instruction, mode = mode, op = op, a = a, d = d }
  end

  local block_entry = {}
  for pc = 1, count do
    local ins = code[pc]
    if JUMPS[ins.op] then
      local target = pc + 1 + ins.d - 0x8000
      if target >= 1 and target <= count + 1 then block_entry[target] = true end
      block_entry[pc + 1] = true
    elseif TESTS[ins.op] then
      block_entry[pc + 1] = true
      block_entry[pc + 2] = true
    elseif TERMINATORS[ins.op] then
      block_entry[pc + 1] = true
    end
  end

  local calls = {}
  local known = {}
  local function clear_from(first)
    for register in pairs(known) do
      if register >= first then known[register] = nil end
    end
  end

  for pc = 1, count do
    if block_entry[pc] then known = {} end
    local ins = code[pc]

    if CALLS[ins.op] then
      calls[#calls + 1] = {
        pc = pc,
        op = ins.op,
        a = ins.a,
        target = known[ins.a] or "?",
      }
      clear_from(ins.a)
      if ins.op == "CALLT" or ins.op == "CALLMT" then known = {} end
    elseif JUMPS[ins.op] or TESTS[ins.op] or TERMINATORS[ins.op] then
      known = {}
    elseif ins.op == "KNIL" then
      for register = ins.a, ins.d do known[register] = nil end
    elseif ins.op == "VARG" then
      clear_from(ins.a)
    else
      local copy = ins.op == "MOV" and known[ins.d] or nil
      local writes_a = bit.band(ins.mode, 7) == 1
      if writes_a then known[ins.a] = nil end

      if writes_a and ins.op == "GGET" then
        local value = util.funck(entry.proto, -ins.d - 1)
        if is_identifier(value) then known[ins.a] = value end
      elseif writes_a and ins.op == "UGET"
          and entry.trusted_factory
          and type(util.funcuvname) == "function" then
        local value = util.funcuvname(entry.proto, ins.d)
        if is_identifier(value) then known[ins.a] = value end
      elseif writes_a and ins.op == "MOV" and copy then
        known[ins.a] = copy
      end
    end
  end

  return calls
end

local source = io.read("*a")
if source == nil then fail("cannot read emitted Lua from stdin") end
local headers
source, headers = strip_directives(source)
local chunk, err = loadstring(source, "=@stdin")
if not chunk then fail("cannot parse emitted Lua: " .. tostring(err)) end

local prototypes = {}
local function walk(proto, parent)
  local info, children = child_proto(proto)
  local id = #prototypes
  local name, factory_inner, trusted_factory = resolve_name(id, info, parent, headers)
  local entry = {
    id = id,
    proto = proto,
    info = info,
    name = name,
    factory_inner = factory_inner,
    trusted_factory = trusted_factory,
  }
  prototypes[#prototypes + 1] = entry
  for _, child in ipairs(children) do
    walk(child.proto, entry)
  end
end

walk(chunk, nil)
local output = { "VM\t" .. tostring(frame_links()) }

for _, entry in ipairs(prototypes) do
  output[#output + 1] = table.concat({
    "F",
    tostring(entry.id),
    entry.name,
    tostring(entry.info.stackslots or 0),
    tostring(entry.info.params or 0),
    tostring(entry.info.linedefined or 0),
  }, "\t")
end

for _, entry in ipairs(prototypes) do
  for _, call in ipairs(scan_calls(entry)) do
    output[#output + 1] = table.concat({
      "C",
      tostring(entry.id),
      tostring(call.pc),
      call.op,
      tostring(call.a),
      call.target,
    }, "\t")
  end
end

io.write(table.concat(output, "\n"), "\n")
