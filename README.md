# MPDantic

Pedantic about your library, so MPD does not choke on it.

MPDantic is a Volumio 4 plugin that audits and repairs a music library for MPD, one folder at a time or the whole library from its root. It sizes MPD's output buffer to the player's memory, manages `.mpdignore` files with suggestions drawn from what is on the share, checks and repairs CUE sheets, rebases playlists written on other machines to the player's paths, and validates the tags the Albums view is built from. The heavy work runs in a Rust core; a thin Node shell carries the settings page and the plugin's lifecycle.

## Status

In design. There is nothing to install yet. The first release will appear on the releases page and in the changelog.

## What it will do

| Feature | What |
| --- | --- |
| Output buffer | MPD's `max_output_buffer_size` set from the player's memory, never below Volumio's default, written through Volumio's own MPD configuration hook so it survives every rewrite of `mpd.conf`. A mitigation for large libraries, not a cure. |
| `.mpdignore` | The ignore files of a folder tree, from the top down, with suggestions for what MPD cannot play: files whose suffix no installed decoder takes, disk images, video, and the folders a NAS writes for itself. Folder art is never suggested. |
| CUE sheets | Sheets checked for files that are not there, encodings MPD does not read, several FILE entries and index timing, and repaired in place. |
| Playlists | M3U, M3U8, PLS and XSPF written on another machine rebased to the player's paths, and imported as Volumio playlists. |
| Tags | The tags MPD reads (ID3, Vorbis comments, MP4 atoms, APE, WavPack) validated on any mount and edited where the share allows, Album, AlbumArtist, Artist and Date first. |

Writes to a network share go through a temporary read-write mount of the folder in hand, made for the session and taken down after; Volumio's own read-only mounts and MPD's view of the library are never touched. Every write can be undone, and MPD is told to update the folder afterwards.

## How it will be built

Two parts in one zip, one copy of the core per Volumio architecture: the shell (`plugin/`), the Volumio plugin as the store wants it; and the core (`bins/mpdantic`), one static binary in Rust that the shell starts for the work. Every release is built by GitHub Actions from a tag, signed, and published as a pre-release until tried.

## Licence

MIT.
