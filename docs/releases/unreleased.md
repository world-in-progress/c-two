# Unreleased

The latest release is [C-Two 0.10.0 / c3 0.6.0](0.10.0.md); its source, publication and cleanup evidence are in the [acceptance record](../reports/2026-10-30-0100-release-acceptance.md).

## Fixed

- Python `cc.serve()` startup and shutdown messages use ASCII text, escape route names that stdout cannot encode, and flush output so piped banners appear immediately. Non-UTF-8 stdout no longer raises `UnicodeEncodeError` or skips graceful shutdown.
