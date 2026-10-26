#!/bin/bash
set -euo pipefail
DEPLOY_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
exec /usr/bin/perl "$DEPLOY_DIR/manage.pl" deactivate "$@"
