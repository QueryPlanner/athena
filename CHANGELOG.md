# Changelog

All notable changes to Athena are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and releases use [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-10-05

This release covers changes since the `v0.1.1` tag. Earlier releases did not
have a changelog. The Cargo package version remained `0.1.0` through that
tag and is now aligned with this release.

### Added

- Image input and image inspection with compatible models. Telegram accepts
  photos and documents, and Athena can send generated images and files.
- Browser sign-in links with editable page fields and saved sign-ins for
  each user.
- Custom instructions and local Agent Skills, loaded through
  `ATHENA_INSTRUCTIONS` and `ATHENA_SKILLS_DIR`.
- MCP tools configured through `ATHENA_MCP_CONFIG`, with stdio and
  streamable HTTP connections.
- Conversation compaction that summarizes older messages near the model's
  context limit while preserving the original transcript.
- Telegram Markdown rendering with formatting preserved across long-reply
  chunks and a plain-text fallback.
- Everyday sandbox tools and libraries, including FFmpeg, yt-dlp, pandas,
  Pillow, Git, and tools for searching files and processing JSON.
- Non-secret runtime settings supplied by GitHub environment variables
  during staging and production deployments.

### Changed

- Replace the individual browser tools with the `agent_browser` CLI tool.
- Remove the limits on model turns and tool-call counts per run. Tool
  argument and output size limits remain.
- Document how to filter staging and production telemetry in OpenObserve.

[Unreleased]: https://github.com/QueryPlanner/athena/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/QueryPlanner/athena/compare/v0.1.1...v0.2.0
