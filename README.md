# MPDantic

Pedantic about your library, so MPD does not choke on it.

MPDantic is a Volumio 4 plugin that audits and repairs a music library for MPD, one folder at a time or the whole library from its root. It sizes MPD's output buffer to the player's memory, manages `.mpdignore` files with suggestions drawn from what is on the share, checks and repairs CUE sheets, rebases playlists written on other machines to the player's paths, and validates the tags the Albums view is built from. The heavy work runs in a Rust core; a thin Node shell carries the plugin's lifecycle and a settings page that says where the core's own pages are.

## Status

In design. There is nothing to install yet: the repository carries the skeleton, a core that starts and waits, a shell that starts it, and the pipeline that builds, checks and signs a release. The first release will appear on the releases page and in the changelog.

## What it will do

| Feature | What |
| --- | --- |
| Output buffer | MPD's `max_output_buffer_size` set from the player's memory, never below Volumio's default, written through Volumio's own MPD configuration hook so it survives every rewrite of `mpd.conf`. A mitigation for large libraries, not a cure. |
| `.mpdignore` | The ignore files of a folder tree, from the top down, with suggestions for what MPD cannot play: files whose suffix no installed decoder takes, disk images, video, and the folders a NAS writes for itself. Folder art is never suggested. |
| CUE sheets | Sheets checked for files that are not there, encodings MPD does not read, several FILE entries and index timing, and repaired in place. |
| Playlists | M3U, M3U8, PLS and XSPF written on another machine rebased to the player's paths, and imported as Volumio playlists. |
| Tags | The tags MPD reads (ID3, Vorbis comments, MP4 atoms, APE, WavPack) validated on any mount and edited where the share allows, Album, AlbumArtist, Artist and Date first. |

Writes to a network share go through a temporary read-write mount of the folder in hand, made for the session and taken down after; Volumio's own read-only mounts and MPD's view of the library are never touched. Every write can be undone, and MPD is told to update the folder afterwards.

## How it is built

Two parts in one zip, one copy of the core per Volumio architecture (`arm`, `armv7`, `armv8`, `x64`), nothing of the player's own software relied on:

| Part | What | Where it comes from |
| --- | --- | --- |
| The shell (`plugin/`) | The Volumio plugin as the store wants it: the settings page, the install, the core's service. | This repository. |
| The core (`bins/mpdantic`) | One daemon in Rust, started by the shell as its own systemd unit. Static, linked against musl. | This repository, cross-built in CI. |

Every release is built by GitHub Actions from a tag: the check, the cross builds, the zip with `MANIFEST` and its Ed25519 signature inside and `SHA256SUMS` with its signature beside, published as a pre-release and promoted by hand once tried. `build.json` in the zip names the commit.

To work on it: `scripts/check.sh` is the workshop pass (format, lints, tests, docs, the shell scripts, the plugin's files and tests), `scripts/ship.sh` the cross builds, `scripts/package.sh` the zip. The Rust toolchain is pinned by `rust-toolchain.toml`; Node is needed only for the plugin's checks.

## Licence

MIT.
