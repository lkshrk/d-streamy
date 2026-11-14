#!/usr/bin/env bash
# Usage: notice-licenses.sh THIRD_PARTY_NOTICES.txt...
# Prints every SPDX licence id, or "<id> WITH <exception>", named by a crate label in the notices, one per line, sorted.
set -euo pipefail

[ $# -gt 0 ] || { echo "usage: notice-licenses.sh THIRD_PARTY_NOTICES.txt..." >&2; exit 1; }

awk '
  /^=+$/ && length($0) == 78 { header = !header; next }
  !header { next }
  {
    if (!match($0, /\([^()]*(\([^()]*\)[^()]*)*\)$/)) { print FILENAME ": no licence expression in: " $0 > "/dev/stderr"; bad = 1; next }
    expr = substr($0, RSTART + 1, RLENGTH - 2)
    if (expr == "see below") { print FILENAME ": no SPDX licence for: " $0 > "/dev/stderr"; bad = 1; next }
    gsub(/\//, " OR ", expr)
    gsub(/[()]/, " ", expr)
    n = split(expr, words, " ")
    for (i = 1; i <= n; i++) {
      w = words[i]
      if (w == "OR" || w == "AND") continue
      if (w !~ /^[A-Za-z0-9.+-]+$/) { print FILENAME ": not an SPDX id: " w " in: " $0 > "/dev/stderr"; bad = 1; continue }
      if (words[i + 1] == "WITH") { w = w " WITH " words[i + 2]; i += 2 }
      print w
    }
  }
  END { exit bad }
' "$@" | sort -u
