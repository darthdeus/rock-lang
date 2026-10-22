# stone-glsl-runner

Building requires SDL2 development libraries. Set `SDL2_LIB_DIR` to an absolute
library directory to use a custom SDL2 installation; otherwise the system linker
must locate SDL2.

At runtime, place SDL2's shared library beside the runner. Sibling library
loading is supported on Linux and macOS, and Windows DLL lookup finds the adjacent
SDL2 DLL.
