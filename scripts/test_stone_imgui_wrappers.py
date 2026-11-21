#!/usr/bin/env python3
"""Selection from the pinned cimgui declaration format."""
import unittest

import stone_imgui_wrappers


class BindingTests(unittest.TestCase):
    def test_enums_keep_exact_names_and_computed_families_in_source_order(self):
        source = '''local M = require("master")
M.Key_A = 4
M.Flag_Used = 16
M.Flag_Unused = 32
M.Key_B = 259
'''
        self.assertEqual(stone_imgui_wrappers.select_enums(source, ['Flag_Used', 'Key_*']),
                         source.replace('M.Flag_Unused = 32\n', ''))
        with self.assertRaisesRegex(ValueError, 'unknown cimgui enum: Keypad'):
            stone_imgui_wrappers.select_enums(source, ['Keypad_*'])

    def test_cdef_follows_wrapper_and_raw_references_in_source_order(self):
        cdef = '''require("ffi").cdef[[
typedef struct Draw Draw;
typedef enum { Count = 2, Last = Count - 1 } Index;
typedef void (*Callback)(Draw*);
struct Draw
{
    struct { int xy[Count]; } point;
    Callback draw;
};
typedef struct Raw { int index; } Raw;
typedef int Unused;
extern void draw(Draw* self);
extern Raw raw(void);
extern void unused(Unused value);

]]'''
        expected = cdef.replace('typedef int Unused;\n', '').replace(
            'extern void unused(Unused value);\n', '',
        )
        self.assertEqual(
            stone_imgui_wrappers.select_cdef(cdef, 'return C.draw(x)', {'raw'}),
            expected,
        )


    def test_opaque_pointer_drops_its_layout_dependencies(self):
        cdef = '''require("ffi").cdef[[
typedef struct Context Context;
typedef int State;
struct Context { State state; };
extern Context* get_context(void);

]]'''
        expected = cdef.replace('typedef int State;\nstruct Context { State state; };\n', '')
        self.assertEqual(
            stone_imgui_wrappers.select_cdef(cdef, 'return C.get_context()', [], {'Context'}),
            expected,
        )


if __name__ == '__main__':
    unittest.main()
