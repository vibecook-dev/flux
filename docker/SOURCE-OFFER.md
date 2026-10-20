# jellyfin-ffmpeg in this image: licence and source

This image contains **jellyfin-ffmpeg @JELLYFIN_FFMPEG_VERSION@**, Jellyfin's
build of FFmpeg, installed from the upstream `.deb` into
`/usr/lib/jellyfin-ffmpeg`. flux runs its `ffmpeg` and `ffprobe` programs as
separate processes. flux does not link FFmpeg's libraries, so flux itself
stays under the MIT licence (`/opt/flux/LICENSE`).

This FFmpeg build includes GPL components (libx264, libx265 and others), so
as a whole it is licensed under the **GNU General Public License, version 3
or later**. Its copyright and licence file is next to this one:
`/usr/share/licenses/jellyfin-ffmpeg/copyright`.

## Corresponding source

- jellyfin-ffmpeg @JELLYFIN_FFMPEG_VERSION@ (FFmpeg with Jellyfin's patches
  and build scripts):
  https://github.com/jellyfin/jellyfin-ffmpeg/tree/v@JELLYFIN_FFMPEG_VERSION@
- the same source as a tarball:
  https://github.com/jellyfin/jellyfin-ffmpeg/archive/refs/tags/v@JELLYFIN_FFMPEG_VERSION@.tar.gz
- every flux release also attaches that tarball as
  `jellyfin-ffmpeg-@JELLYFIN_FFMPEG_VERSION@-source.tar.gz`:
  https://github.com/vibecook-dev/flux/releases
- the Debian packages the build depends on (for example `libx264-164` and
  `libx265-215`) are unmodified Debian 13 packages; their source is on
  https://sources.debian.org and https://snapshot.debian.org.

## Written offer

For at least three years after we last distribute this image, anyone can ask
us for the complete corresponding source of the GPL software in it, on a
medium customarily used for software interchange, for no more than the cost
of providing it. Open an issue at https://github.com/vibecook-dev/flux/issues
and name the image tag or digest.
