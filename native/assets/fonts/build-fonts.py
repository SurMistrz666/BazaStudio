import sys
from fontTools.ttLib import TTFont
from fontTools.varLib import instancer
src, out = sys.argv[1], sys.argv[2]
for subset in ("latin", "latin-ext"):
    for name, weight in (("Regular", 400), ("Medium", 500), ("SemiBold", 600)):
        font = TTFont(f"{src}/files/inter-{subset}-wght-normal.woff2")
        axes = {a.axisTag: (a.minValue, a.defaultValue, a.maxValue) for a in font["fvar"].axes}
        pins = {"wght": weight}
        if "opsz" in axes:
            pins["opsz"] = axes["opsz"][1]
        static = instancer.instantiateVariableFont(font, pins, inplace=False)
        static.flavor = None
        path = f"{out}/Inter-{name}-{subset}.ttf"
        static.save(path)
        cmap = static.getBestCmap()
        print(path.split('/')[-1], len(cmap), "glyphs", [hex(c) for c in (0x41, 0x105, 0x142, 0x17c, 0x2192) if c in cmap])
