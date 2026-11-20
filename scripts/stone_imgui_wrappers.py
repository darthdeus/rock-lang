#!/usr/bin/env python3
"""Select Stone's wrappers from the pinned gamelibs Lua bindings."""
import argparse
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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true', help='check the committed output')
    args = parser.parse_args()
    gamelibs.sync(('lua-bindings',))
    upstream = gamelibs.gamelibs_root() / 'lua-bindings/cimgui'
    for name in ('cdef.lua', 'enums.lua'):
        if (upstream / name).read_text() != (BINDINGS / name).read_text():
            parser.error(f'{name} differs from the pinned gamelibs Lua bindings')
    roots = (BINDINGS / 'wrappers.txt').read_text().split()
    try:
        output = select((upstream / 'wrap.lua').read_text(), roots)
    except ValueError as error:
        parser.error(str(error))
    path = BINDINGS / 'wrap.lua'
    if args.check:
        if path.read_text() != output:
            parser.error('wrap.lua is stale. Run scripts/stone_imgui_wrappers.py to regenerate it.')
    else:
        path.write_bytes(output.encode())


if __name__ == '__main__':
    main()
