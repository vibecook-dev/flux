flux for Windows (x86_64)
=========================

flux is a self-hosted media server that serves its own web client. This
folder holds:

  flux-server.exe            the server
  sidecar-slim.exe           Truffle's Tailscale sidecar, for --tailnet embedded
  web\                       the web client
  LICENSE.txt                flux's licence (MIT)
  THIRD_PARTY_LICENSES.txt   the licences of the software flux includes
  README.txt                 this file

Keep them together: flux-server.exe looks for web\ and sidecar-slim.exe
next to itself.

1. Get ffmpeg. flux runs ffmpeg.exe and ffprobe.exe for transcodes, posters
   and some probes, but doesn't include them. Download Jellyfin's portable
   build, jellyfin-ffmpeg_<version>_portable_win64-clang-gpl.zip, from
   https://github.com/jellyfin/jellyfin-ffmpeg/releases and unzip it. Then
   add that folder to PATH, or start flux with
   --ffmpeg C:\path\to\jellyfin-ffmpeg\ffmpeg.exe (ffprobe.exe is taken from
   the same folder).

2. Open PowerShell or Command Prompt in this folder and run

     .\flux-server.exe --check

   to see which ffmpeg and which video encoder flux found, then

     .\flux-server.exe --library "Movies:movies:D:\Media\Movies" --library "TV:shows:D:\Media\TV"

   and open http://127.0.0.1:8096. Libraries named this way are created on
   the first run; you can also add them on the Admin screen.

3. Stop flux with Ctrl-C (Ctrl-Break and closing its window also stop it
   cleanly).

The catalog, artwork and caches live in %LOCALAPPDATA%\flux unless you pass
--data-dir. Every option is in `.\flux-server.exe --help`.

The binaries are not code-signed yet, so Windows SmartScreen may warn about
them. Check the zip against the release's SHA256SUMS and its attestation
first. How to do that, run flux as a service, use a config file, and let
other devices in (--lan) is in docs/INSTALL.md, section Windows:
https://github.com/vibecook-dev/flux/blob/main/docs/INSTALL.md#windows
