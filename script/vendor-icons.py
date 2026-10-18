#!/usr/bin/env python3
"""Copy Studio's Nucleo icons from the licensed bundle into `assets/icons/nucleo/`.

    script/vendor-icons.py ~/Downloads/nucleo_ui_svg_v1.8.0.zip

The bundle is a licensed download, not a dependency, and is never committed. The mapping lives
in `crates/studio_theme/src/icons.rs` (`NUCLEO`): this script reads the Nucleo names from it,
copies each one's `outline-duo` 18 px drawing, removes drawings no row names, and writes the
notice the licence asks for. Change an icon by changing its row and re-running this.
"""

import re
import sys
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MAPPING = ROOT / "crates/studio_theme/src/icons.rs"
OUT = ROOT / "assets/icons/nucleo"
STYLE = "outline-duo"
SIZE = "18px"
NOTICE = """Copyright © Nucleo

Version 1.3, January 3, 2024

Nucleo Icons

https://nucleoapp.com/

- Redistribution of icons is prohibited.
- Icons are restricted for use only within the product they are bundled with.

For more details:

https://nucleoapp.com/license
"""


def names() -> list[str]:
    rows = re.findall(r'^\s*\("[a-z0-9_]+", "([a-z0-9-]+)"\),$', MAPPING.read_text(), re.M)
    if not rows:
        sys.exit(f"no rows found in {MAPPING}")
    return sorted(set(rows))


def main() -> None:
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    wanted = names()
    if len(wanted) > 100:
        sys.exit(f"{len(wanted)} icons: Nucleo's licence allows 100 in an open-source project")
    with zipfile.ZipFile(Path(sys.argv[1]).expanduser()) as bundle:
        drawings = {}
        for entry in bundle.namelist():
            parts = entry.strip("/").split("/")
            if len(parts) == 3 and parts[0] == STYLE and parts[2].startswith(f"{SIZE}_"):
                drawings[parts[2][len(SIZE) + 1 : -len(".svg")]] = entry
        missing = [name for name in wanted if name not in drawings]
        if missing:
            sys.exit(f"not in the bundle's {STYLE} {SIZE} set: {', '.join(missing)}")
        OUT.mkdir(parents=True, exist_ok=True)
        for old in OUT.glob("*.svg"):
            if old.stem not in wanted:
                old.unlink()
        for name in wanted:
            (OUT / f"{name}.svg").write_bytes(bundle.read(drawings[name]))
    (OUT / "NOTICE").write_text(NOTICE)
    print(f"{len(wanted)} icons in {OUT.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
