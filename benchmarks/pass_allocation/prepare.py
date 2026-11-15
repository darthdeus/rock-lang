"""Extract current Stone data declarations, without loading its GPU runtime."""

from pathlib import Path
import re
import sys

root = Path(__file__).resolve().parents[2]
groups = {
    "packages/stone/engine/bindings.rock": ["Image", "CanvasId", "Canvas", "SampledTexture"],
    "packages/stone/engine/rendering/pass.rock": ["Pipeline", "Pass"],
    "packages/stone/engine/rendering/texture_sampling.rock": ["TextureFilter"],
    "packages/stone/engine/rendering/commands.rock": ["BlendMode"],
    "packages/stone/engine/rendering/uniform_block.rock": ["PassArrayMember", "OwnedUniformBlock"],
    "packages/stone-core/types.rock": ["IRect", "Color"],
}
parts = []
for path, names in groups.items():
    source = (root / path).read_text()
    for name in names:
        pattern = rf"^pub (?:nobox )?(?:value|struct|enum|newtype) {name}\b[^\n]*"
        matches = list(re.finditer(pattern, source, re.MULTILINE))
        if len(matches) != 1:
            raise SystemExit(f"Expected one declaration of {name} in {path}")
        start = matches[0].start()
        end = matches[0].end()
        if "newtype " not in matches[0].group():
            opening = source.index("{", start, end)
            depth = 1
            end = opening + 1
            while depth:
                depth += (source[end] == "{") - (source[end] == "}")
                end += 1
        parts.append(source[start:end].removeprefix("pub "))
parts.append(Path(__file__).with_name("fixture.rock").read_text())
Path(sys.argv[1]).write_text("\n\n".join(parts))
