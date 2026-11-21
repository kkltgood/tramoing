"""Check the Finder layout from a mounted release image without opening Finder."""
from pathlib import Path
import sys
from ds_store import DSStore

mount = Path(sys.argv[1])
assert (mount / "Applications").is_symlink(), "Applications shortcut is missing"
assert (mount / "Applications").readlink() == Path("/Applications")
assert (mount / ".VolumeIcon.icns").is_file(), "Volume icon is missing"
assert (mount / ".background.tiff").is_file(), "Retina background is missing"
with DSStore.open(str(mount / ".DS_Store"), "r") as store:
    assert store["Tramoing.app"]["Iloc"] == (190, 224)
    assert store["Applications"]["Iloc"] == (510, 224)
    assert store["."]["icvp"]["iconSize"] == 88
    assert store["."]["icvp"]["backgroundType"] == 2
    assert not store["."]["bwsp"]["ShowToolbar"]
print("PASS DMG layout, draggable application, Applications link, Retina background and volume icon")
