# MPDantic

Pedantic about your library, so MPD does not choke on it.

MPDantic is a Volumio 4 plugin that audits and repairs a music library for MPD, one folder at a time or the whole library from its root. It sizes MPD's output buffer to the player's memory, manages `.mpdignore` files with suggestions drawn from what is on the share, checks and repairs CUE sheets, rebases playlists written on other machines to the player's paths, and validates the tags the Albums view is built from. The work is done from MPDantic's own pages, served by its core on the player; the heavy work runs in that Rust core, and a thin Node shell carries the plugin's lifecycle and a settings page that says where the pages are.

## Status

In design. There is nothing to install yet: the repository carries the core, which serves the pages, the library one folder at a time and the first analysis, the shell that starts it and announces the pages on the network, and the pipeline that builds, checks and signs a release. The first release will appear on the releases page and in the changelog.

## The pages

The core serves them on a port of the player (6218 unless set on the settings page), from its memory, for any browser on the network: `http://<player>:6218/`. The settings page opens them inside Volumio's own page or in a tab of their own, and shows the address; on the network they are announced as "MPDantic on <player>" for anything that lists Bonjour services. The first page is the library as MPD sees it, folder by folder from the roots (`NAS`, `USB`, `INTERNAL`), read on request one directory at a time; it never asks for the whole library.

Under the listing, the `.mpdignore` analysis of the folder in hand: a job walks everything under it, one directory at a time, and says what MPD's update would meet there by class (audio, CUE sheets, playlists, images, archives MPD would open and walk inside, video containers its ffmpeg decoder happens to claim, the folders a NAS writes for itself, the sidecar files a desktop leaves) and which `.mpdignore` lines, already in force from the root down, keep what out. It suggests the lines that would keep the junk out, each with its reason, its count and examples. The audio suffixes come from the running MPD's own decoder list, kept per plugin, so the analysis matches the MPD on the player: what MPD's own audio plugins claim is music, what its ffmpeg decoder alone claims is shown as its own class, with the text, lyrics and subtitle files it opens and probes at every update suggested as lines. A `.mpdignore` line that starts with a hash is a comment to MPD, and a line like `#recycle` is shown as dead with the line that works, `[#]recycle`. The matching is the same `fnmatch` MPD uses. Video in the library is wanted where Glass plays it: a switch on the page says whether video is in use here, following Glass's presence on the player unless set, and with video in use the video containers are counted and never suggested. Nothing is written from the page yet.

Beside it, the CUE check of the folder in hand: every sheet under it read the way MPD reads it (the six commands MPD takes, the FILE types it accepts, the index rule it follows) and what MPD makes of it said per sheet: a FILE type that drops its tracks, a FILE not beside the sheet or there in another case or with another suffix, a FILE no decoder claims, a foreign path, an unquoted name, a byte-order mark, a sheet that is not UTF-8, an index MPD cannot use, two sheets naming the same file, a sheet beside its own split tracks. Each is what MPD does with the sheet as it is; the tracks MPD would make are listed with their starts and ends, and an index past the file's length is said. Nothing is repaired from the page yet.

And the tag check of the folder in hand: every audio file under it read for its tags as written (Vorbis comments, ID3v2 and ID3v1, APE, MP4 atoms, the ID3 tag a DSF or DFF file carries at the offset its header names), then MPD's own rules applied to them, read from MPD's source: which names it takes from each format and as what. The report says, per folder, whether the files agree on Album and AlbumArtist, since Volumio groups an album by those and a folder that disagrees shows as several albums; and per file what MPD would miss: a title, artist or album it cannot read, an album artist or a date written under a name MPD does not read ("ALBUM ARTIST" with a space in a FLAC, "YEAR", a TXXX frame), a track or disc number missing or not a number, text that is UTF-8 read as Latin-1, ID3v1 alone, a tag given twice, a placeholder, an embedded cue sheet beside an external one. Nothing is edited from the page yet.

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
| The shell (`plugin/`) | The Volumio plugin as the store wants it: the settings page, the install, the core's service, the pages' announcement on the network. | This repository. |
| The core (`bins/mpdantic`) | One daemon in Rust, started by the shell as its own systemd unit: it serves the pages and the API, and does the work on the library. Static, linked against musl. | This repository, cross-built in CI. |

Every release is built by GitHub Actions from a tag: the check, the cross builds, the zip with `MANIFEST` and its Ed25519 signature inside and `SHA256SUMS` with its signature beside, published as a pre-release and promoted by hand once tried. `build.json` in the zip names the commit.

To work on it: `scripts/check.sh` is the workshop pass (format, lints, tests, docs, the shell scripts, the plugin's files and tests, the pages' scripts), `scripts/ship.sh` the cross builds, `scripts/package.sh` the zip. The Rust toolchain is pinned by `rust-toolchain.toml`; Node is needed only for the plugin's checks. The core runs on a workstation too, for the pages against any folder: `mpdantic --run /tmp/mpdantic --music /path/to/music --mpd 127.0.0.1:6600` (without an MPD to ask, the analysis assumes the usual audio suffixes and says so).

## Licence

MIT.
