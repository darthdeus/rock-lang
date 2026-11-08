#!/usr/bin/env python3
"""Read the host gamelibs cache; sync is its only writer."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tempfile
import urllib.request
import zipfile


ROOT = Path(__file__).resolve().parents[1]
PLATFORMS = ('linux-x86_64', 'macos-arm64', 'windows-x86_64')


def version():
    return (ROOT / 'gamelibs.version').read_text().strip()


def host_platform():
    system = platform.system()
    if system == 'Darwin' and platform.machine() == 'arm64':
        return 'macos-arm64'
    if system == 'Windows':
        return 'windows-x86_64'
    if system in ('Linux', 'FreeBSD', 'OpenBSD', 'NetBSD') and platform.machine() in ('x86_64', 'AMD64'):
        return 'linux-x86_64'
    raise ValueError(f'no gamelibs for {system}/{platform.machine()}')


def gamelibs_root():
    if override := os.environ.get('ROCK_GAMELIBS_DIR'):
        root = Path(override)
        if not root.is_absolute():
            raise ValueError('ROCK_GAMELIBS_DIR must be an absolute version directory')
        return root
    root = Path(os.environ.get('ROCK_WORK_ROOT', Path.home()))
    if not root.is_absolute() or not root.is_dir():
        raise ValueError('ROCK_WORK_ROOT must be an existing absolute directory')
    return root / 'tmp/rock-gamelibs' / version()


def platform_dir(plat=None):
    path = gamelibs_root() / ('gamelibs-' + (plat or host_platform()))
    if not path.is_dir():
        raise ValueError(f'gamelibs cache missing at {path}; run `python3 scripts/gamelibs.py sync`')
    return path


def extract(archive, destination):
    # Release ZIPs contain symlinks; preserve their link targets and executable bits.
    with zipfile.ZipFile(archive) as z:
        for entry in z.infolist():
            path = destination / entry.filename
            if not path.resolve().is_relative_to(destination.resolve()):
                raise ValueError(f'unsafe archive path: {entry.filename}')
            mode = entry.external_attr >> 16
            if entry.is_dir():
                path.mkdir(parents=True, exist_ok=True)
            else:
                path.parent.mkdir(parents=True, exist_ok=True)
                if mode & 0o170000 == 0o120000:
                    path.symlink_to(z.read(entry).decode())
                else:
                    path.write_bytes(z.read(entry))
                    if mode & 0o777:
                        path.chmod(mode & 0o777)


def clear_quarantine(path):
    if platform.system() == 'Darwin':
        subprocess.run(['xattr', '-dr', 'com.apple.quarantine', str(path)], check=True)


def sync(platforms):
    root = gamelibs_root()
    root.mkdir(parents=True, exist_ok=True)
    hashes = json.loads((ROOT / 'gamelibs-artifacts.json').read_text())
    for plat in platforms:
        dest = root / (f'gamelibs-{plat}' if plat != 'lua-bindings' else 'lua-bindings')
        name = f'gamelibs-{plat}.zip'
        digest = hashes[name]
        if (dest / '.rock-gamelibs-sha256').is_file():
            if (dest / '.rock-gamelibs-sha256').read_text().strip() == digest:
                clear_quarantine(dest)
                print(f'present {dest}')
                continue
        if dest.exists():
            raise ValueError(f'incomplete or different cache at {dest}; remove it before `python3 scripts/gamelibs.py sync`')
        with tempfile.TemporaryDirectory(prefix='.install-', dir=root) as temporary:
            scratch = Path(temporary)
            print(f'downloading {name}', flush=True)
            with urllib.request.urlopen(f'https://github.com/darthdeus/gamelibs/releases/download/{version()}/{name}', timeout=60) as response, (scratch / name).open('wb') as output:
                shutil.copyfileobj(response, output)
            archive = scratch / name
            if hashlib.sha256(archive.read_bytes()).hexdigest() != digest:
                raise ValueError(f'checksum mismatch: {name}')
            staged = scratch / dest.name
            staged.mkdir()
            extract(archive, staged)
            (staged / '.rock-gamelibs-sha256').write_text(digest + '\n')
            clear_quarantine(staged)
            try:
                staged.rename(dest)
            except OSError:
                # Concurrent installers may finish the same immutable artifact first.
                if not (dest / '.rock-gamelibs-sha256').is_file() or (dest / '.rock-gamelibs-sha256').read_text().strip() != digest:
                    raise
        print(f'installed {dest}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=('root', 'path', 'sync', 'version'))
    parser.add_argument('--plat', choices=PLATFORMS)
    args = parser.parse_args()
    try:
        if args.command == 'sync':
            sync([args.plat or host_platform()])
        elif args.command == 'path':
            print(platform_dir(args.plat))
        elif args.command == 'root':
            print(gamelibs_root())
        else:
            print(version())
    except (RuntimeError, ValueError, OSError, subprocess.CalledProcessError) as error:
        parser.exit(1, f'gamelibs: {error}\n')


if __name__ == '__main__':
    main()
