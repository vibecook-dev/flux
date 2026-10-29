# dmgbuild settings for flux's disk image (scripts/package/macos-app.sh, which
# passes the app, the volume icon and the background with -D).
# The window: flux.app on the left, a link to Applications on the right, an
# arrow between them (dmg-background.png, drawn for these positions).
import os.path

app = defines["app"]  # noqa: F821 (dmgbuild defines it)

files = [app]
symlinks = {"Applications": "/Applications"}
hide_extensions = [os.path.basename(app)]
icon = defines["icon"]  # noqa: F821
background = defines["background"]  # noqa: F821

format = "ULFO"
filesystem = "HFS+"

window_rect = ((200, 120), (640, 400))
default_view = "icon-view"
icon_size = 128
text_size = 13
icon_locations = {
    os.path.basename(app): (160, 190),
    "Applications": (480, 190),
}
show_status_bar = False
show_tab_view = False
show_toolbar = False
show_pathbar = False
show_sidebar = False
