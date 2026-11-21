"""Finder layout written directly to .DS_Store; no Finder automation needed."""
from pathlib import Path

root = Path(defines["root"])
application = Path(defines["app"])
format = "UDZO"
filesystem = "HFS+"
files = [str(application)]
symlinks = {"Applications": "/Applications"}
icon = str(application / "Contents/Resources/AppIcon.icns")
background = str(root / "packaging/macos/dmg-background.png")
window_rect = ((160, 120), (700, 460))
icon_locations = {"Tramoing.app": (190, 224), "Applications": (510, 224)}
icon_size = 88
text_size = 13
# SetFile would add FinderInfo to the already signed bundle and invalidate it.
hide_extensions = []
default_view = "icon-view"
show_toolbar = False
show_status_bar = False
show_pathbar = False
show_tab_view = False
include_icon_view_settings = True
include_list_view_settings = False
