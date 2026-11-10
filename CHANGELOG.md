# Changelog

All notable changes to MPDantic. The format follows Keep a Changelog; the versions follow Semantic Versioning.

## [Unreleased]

### Added

- The skeleton: the core (`bins/mpdantic`, one static binary per architecture, starts and waits), the shell (`plugin/`, the settings page with the management port and a status button, the core's service), the workshop pass (`scripts/check.sh`), the cross builds and the plugin zip, CI and the signed release workflow. Nothing to use yet.
- The management pages, served by the core on a port of the player: the library as MPD sees it, folder by folder from the roots, read one directory at a time in pages; the core's status. The settings page opens them inside Volumio's page or in a tab, with the address browsers reach the player by (found on the player, or set); the pages are announced on the network through Avahi as "MPDantic on <player>".
