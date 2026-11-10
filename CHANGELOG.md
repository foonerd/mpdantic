# Changelog

All notable changes to MPDantic. The format follows Keep a Changelog; the versions follow Semantic Versioning.

## [Unreleased]

### Added

- The skeleton: the core (`bins/mpdantic`, one static binary per architecture, starts and waits), the shell (`plugin/`, the settings page with the management port and a status button, the core's service), the workshop pass (`scripts/check.sh`), the cross builds and the plugin zip, CI and the signed release workflow. Nothing to use yet.
- The management pages, served by the core on a port of the player (6218): the library as MPD sees it, folder by folder from the roots, read one directory at a time in pages; the core's status. The settings page opens them inside Volumio's page or in a tab, with the address browsers reach the player by (found on the player, or set); the pages are announced on the network through Avahi as "MPDantic on <player>".
- The `.mpdignore` analysis of a folder, read-only: a job walks everything under it and reports what MPD's update would meet by class, the lines already in force from the root down, and the lines that would keep out the archives MPD opens, the video containers its ffmpeg decoder claims, the folders a NAS writes for itself and the sidecar files a desktop leaves, each with its reason, count and examples. The audio suffixes are the running MPD's own (`decoders`), kept per plugin: what its own audio plugins claim is music, what ffmpeg alone claims is a class of its own, with text, lyrics and subtitle files suggested as lines. A `.mpdignore` line like `#recycle` is shown as dead: a comment to MPD, with `[#]recycle` as the line that works. The matching is MPD's own `fnmatch`.
- A switch on the page for video in the library: as Glass says (in use when the Glass plugin is on the player), in use, or not wanted; with video in use, video containers are counted and never suggested. Kept by the core under `/data/INTERNAL/mpdantic`.
- The CUE check of a folder, read-only: every sheet read as MPD's parser reads it, its FILEs looked for beside it, and the findings in MPD's terms (the FILE type MPD drops, a FILE missing or in another case or with another suffix, a FILE no decoder claims, a foreign path, an unquoted name, a byte-order mark, an encoding that is not UTF-8, an index MPD cannot use, several FILEs, two sheets naming one file, a sheet beside its split tracks), with the tracks MPD would make.
