#!/bin/sh
# A source checkout has no release identity or trusted update keys. Release jobs
# generate a standalone install.sh (packaging/release.py) containing both.
cat >&2 <<'HELP'
Download install.sh from your publisher's signed GitHub Release.
For an upstream v0.2.0 script installation, the legacy installer is at
https://raw.githubusercontent.com/Derrors/iphone-control/v0.2.0/install.sh
Pass --version v0.2.0 to install that version, or --uninstall to remove it
before migrating. If you used --prefix DIR, use the same prefix again.
HELP
exit 1
