#!/bin/sh
# A source checkout has no release identity or trusted update keys. Release jobs
# generate a standalone install.sh (packaging/release.py) containing both.
echo "Use the install.sh asset from your publisher's GitHub Release. Unbound source installs do not select a publisher." >&2
exit 1
