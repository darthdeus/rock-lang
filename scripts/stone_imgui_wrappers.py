#!/usr/bin/env python3
"""Select Stone's wrappers and C declarations from the pinned gamelibs bindings."""
import argparse
from collections import defaultdict
import re

import gamelibs


BINDINGS = gamelibs.ROOT / 'packages/stone/prebuilt/cimgui'
NAME = r'[A-Za-z_]\w*'
# The generator emits each function's closing end at column zero.
FUNCTION = re.compile(
    rf'^(?:M\.(?P<name>{NAME})|(?P<owner>{NAME})\["(?P<method>{NAME})"\])'
    r' = [^\n]*?function\([^\n]*\)\n.*?^end\n', re.M | re.S,
)
ALIAS = re.compile(
    rf'^(?P<owner>{NAME})\.(?P<method>{NAME}) = (?P=owner)\["(?P<target>{NAME})"\]\n', re.M,
)
METATYPE = re.compile(
    rf'^local ({NAME}) = \1 or \{{\}}\n.*?^ffi\.metatype\("\1", \1\)\n\n', re.M | re.S,
)
CONSTRUCTOR = re.compile(
    r'^local mt = getmetatable\([^\n]+\) or \{\}\n.*?^setmetatable\([^\n]+\)\n', re.M | re.S,
)
MODULE_REF = re.compile(rf'\bM\.({NAME})')
C_REF = re.compile(rf'\bC\.({NAME})')
IDENTIFIER = re.compile(NAME)
CDEF_HEADER = 'require("ffi").cdef[[\n'
CDEF_FOOTER = '\n]]'


def wrapper_name(match):
    owner = match['owner']
    return f'{owner}.{match["method"]}' if owner else match['name']


def select(source, roots):
    source = '\n'.join(line.rstrip() for line in source.splitlines()) + '\n'
    deps = {}
    for match in METATYPE.finditer(source):
        constructor = CONSTRUCTOR.search(match[0])
        deps[match[1]] = set(MODULE_REF.findall(constructor[0])) if constructor else set()
    for match in FUNCTION.finditer(source):
        name = wrapper_name(match)
        deps[name] = set(MODULE_REF.findall(match[0])) - {name}
    for match in ALIAS.finditer(source):
        deps[wrapper_name(match)] = {f'{match["owner"]}.{match["target"]}'}

    kept = set()
    pending = set(roots)
    while pending:
        name = pending.pop()
        if name not in deps:
            raise ValueError(f'unknown cimgui wrapper: {name}')
        kept.add(name)
        pending.update(deps[name] - kept)

    method_owners = {name.split('.')[0] for name in kept if '.' in name}

    def keep_if_used(match):
        return match[0] if wrapper_name(match) in kept else ''

    def keep_type(match):
        if match[1] in kept:
            return match[0]
        if match[1] in method_owners:
            return CONSTRUCTOR.sub('', match[0])
        return ''.join(wrapper[0] for wrapper in FUNCTION.finditer(match[0]))

    source = FUNCTION.sub(keep_if_used, source)
    source = ALIAS.sub(keep_if_used, source)
    return METATYPE.sub(keep_type, source)


def declarations(source):
    # The pinned generator emits plain declarations, with semicolons inside aggregates.
    if not source.startswith(CDEF_HEADER) or not source.rstrip().endswith(CDEF_FOOTER):
        raise ValueError('unexpected cimgui cdef module')
    body = source.rstrip()[len(CDEF_HEADER):-len(CDEF_FOOTER)]
    start = depth = 0
    for match in re.finditer(r'[{};]', body):
        token = match[0]
        if token == '{':
            depth += 1
        elif token == '}':
            depth -= 1
        elif depth == 0:
            yield body[start:match.end()].strip()
            start = match.end()
    if depth or body[start:].strip():
        raise ValueError('unterminated cimgui declaration')


def declaration_names(decl):
    if match := re.fullmatch(rf'typedef enum\s*\{{(.*)\}}\s*({NAME});', decl, re.S):
        return [match[2], *re.findall(rf'(?:^|,)\s*({NAME})', match[1])]
    for pattern in (
        rf'extern [^(;]*\b({NAME})(?:\s*\(.*\))?;',
        rf'struct ({NAME})(?:\s*\{{.*\}})?;',
        rf'typedef [^{{;]*\(\*({NAME})\)\(.*\);',
        rf'typedef (?:[^{{;]+|[^{{;]*\{{.*\}}\s*)\b({NAME});',
    ):
        if match := re.fullmatch(pattern, decl, re.S):
            return [match[1]]
    raise ValueError(f'unrecognized cimgui declaration: {decl}')


def select_cdef(source, wrapper, roots, opaque=()):
    entries = list(declarations(source))
    definitions = defaultdict(list)
    omitted_layouts = set()
    for index, decl in enumerate(entries):
        names = declaration_names(decl)
        if decl.startswith('struct ') and '{' in decl and names[0] in opaque:
            omitted_layouts.add(names[0])
            continue
        for name in names:
            definitions[name].append(index)

    if missing := set(opaque) - omitted_layouts:
        raise ValueError(f'unknown cimgui opaque structs: {", ".join(sorted(missing))}')
    required = set(roots) | set(opaque) | set(C_REF.findall(wrapper))
    if missing := required - definitions.keys():
        raise ValueError(f'unknown cimgui C declarations: {", ".join(sorted(missing))}')

    pending = required | (set(IDENTIFIER.findall(wrapper)) & definitions.keys())
    seen = set()
    kept = set()
    while pending:
        name = pending.pop()
        seen.add(name)
        for index in definitions[name]:
            if index not in kept:
                kept.add(index)
                pending.update((set(IDENTIFIER.findall(entries[index])) & definitions.keys()) - seen)
    selected = ''.join(decl + '\n' for index, decl in enumerate(entries) if index in kept)
    return CDEF_HEADER + selected + CDEF_FOOTER


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true', help='check the committed output')
    args = parser.parse_args()
    gamelibs.sync(('lua-bindings',))
    upstream = gamelibs.gamelibs_root() / 'lua-bindings/cimgui'
    if (upstream / 'enums.lua').read_bytes() != (BINDINGS / 'enums.lua').read_bytes():
        parser.error('enums.lua differs from the pinned gamelibs Lua bindings')
    roots = (BINDINGS / 'roots.txt').read_text().split()
    wrapper_roots = [name for name in roots if not name.startswith(('C.', 'opaque.'))]
    cdef_roots = [name.removeprefix('C.') for name in roots if name.startswith('C.')]
    opaque = {name.removeprefix('opaque.') for name in roots if name.startswith('opaque.')}
    try:
        wrapper = select((upstream / 'wrap.lua').read_text(), wrapper_roots)
        cdef = select_cdef((upstream / 'cdef.lua').read_text(), wrapper, cdef_roots, opaque)
    except ValueError as error:
        parser.error(str(error))
    for name, output in (('wrap.lua', wrapper), ('cdef.lua', cdef)):
        path = BINDINGS / name
        if args.check:
            if path.read_bytes() != output.encode():
                parser.error(f'{name} is stale. Run scripts/stone_imgui_wrappers.py to regenerate it.')
        else:
            path.write_bytes(output.encode())


if __name__ == '__main__':
    main()
