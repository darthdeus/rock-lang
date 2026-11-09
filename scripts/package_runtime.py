#!/usr/bin/env python3
"""Assemble a relocatable Rock/Stone runtime from built host artifacts."""
from pathlib import Path
import argparse
import platform
import shutil
import subprocess
import tempfile


def assemble(source: Path, built: Path, native: Path, output: Path) -> None:
    if output.exists():
        raise FileExistsError(output)
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".runtime-", dir=output.parent) as temporary:
        staged = Path(temporary) / output.name
        _assemble(source, built, native, staged)
        staged.rename(output)


def _assemble(source: Path, built: Path, native: Path, output: Path) -> None:
    output.mkdir(parents=True, exist_ok=False)
    for name in ('bin', 'lib'):
        (output / name).mkdir()
    system = platform.system()
    suffix = '.exe' if system == 'Windows' else ''
    library = {'Linux': ('libstone_rs.so', 'libstone.so'),
               'Darwin': ('libstone_rs.dylib', 'libstone.dylib'),
               'Windows': ('stone_rs.dll', 'libstone.dll')}[system]
    for executable in ('rock', 'stone-glsl-runner'):
        shutil.copy2(built / f'{executable}{suffix}', output / 'bin' / f'{executable}{suffix}')
    shutil.copy2(built / library[0], output / 'lib' / library[1])
    shutil.copy2(native / 'bin' / f'luajit{suffix}', output / 'bin' / f'luajit{suffix}')
    patterns = {'Linux': ('lib', '*.so*'), 'Darwin': ('lib', '*.dylib'),
                'Windows': ('bin', '*.dll')}
    directory, pattern = patterns[system]
    for path in sorted((native / directory).glob(pattern)):
        shutil.copy2(path, output / directory / path.name)
    for name in ('stdlib', 'packages'):
        shutil.copytree(source / name, output / name)
    shutil.copytree(native / 'share/luajit-2.1', output / 'share/luajit-2.1')
    shutil.copytree(native / 'licenses', output / 'licenses/gamelibs')
    for name in ('LICENSE-MIT', 'LICENSE-APACHE', 'README.md'):
        shutil.copy2(source / name, output / name)
    shutil.copytree(source / 'demos/testbed', output / 'demos/testbed')
    if system == 'Darwin':
        binaries = list((output / 'lib').glob('*.dylib')) + list((output / 'bin').iterdir())
        for binary in binaries:
            if binary.suffix == '.dylib':
                subprocess.run(['install_name_tool', '-id', f'@rpath/{binary.name}', str(binary)], check=True)
            dependencies = subprocess.check_output(['otool', '-L', str(binary)], text=True)
            for line in dependencies.splitlines()[1:]:
                dependency = line.strip().split(' ', 1)[0]
                name = Path(dependency).name
                if (output / 'lib' / name).is_file():
                    relative = '@loader_path/' if binary.parent.name == 'lib' else '@loader_path/../lib/'
                    subprocess.run(['install_name_tool', '-change', dependency, relative + name, str(binary)], check=True)
            subprocess.run(['codesign', '--force', '--sign', '-', str(binary)], check=True)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('built', type=Path, help='Cargo host output directory')
    parser.add_argument('native', type=Path, help='matching gamelibs platform directory')
    parser.add_argument('output', type=Path, help='new staging directory')
    args = parser.parse_args()
    assemble(Path(__file__).resolve().parents[1], args.built, args.native, args.output)


if __name__ == '__main__':
    main()
