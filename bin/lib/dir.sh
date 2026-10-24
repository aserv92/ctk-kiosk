#!/usr/bin/env bash

set -euo pipefail

dir.clean() {
  local dir="$1"

  rm -rf "$dir"
  mkdir -p "$dir"
}
