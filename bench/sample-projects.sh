#!/bin/bash
# Reproducible benchmark: 12 typical projects built from popular open-source packages,
# top-level versions pinned to the latest releases as of 2026-11-06.
# Usage: bench/sample-projects.sh <empty dir>
set -euo pipefail
DIR="${1:?usage: $0 <empty dir>}"
mkdir -p "$DIR"

REACT="react@19.3.1 react-dom@19.3.1"
TS="typescript@7.0.2"
LINT="eslint@10.14.0 prettier@3.9.10"
TEST="jest@30.5.2"
VITE="vite@8.3.6 $REACT $TS"

projects=(
  "web-app:$VITE $LINT $TEST"
  "dashboard:$VITE $LINT tailwindcss@4.3.4"
  "landing:next@16.4.5 $REACT $TS tailwindcss@4.3.4"
  "shop:next@16.4.5 $REACT $TS $LINT"
  "api:express@5.3.0 $TS $LINT $TEST"
  "worker:express@5.3.0 $TS lodash@4.18.1"
  "cli:$TS esbuild@0.28.2 $TEST"
  "lib-utils:$TS $LINT $TEST lodash@4.18.1"
  "lib-ui:$VITE $LINT"
  "webpack-app:webpack@5.112.2 webpack-cli@7.3.1 @babel/core@8.1.0 babel-loader@10.1.1 $REACT $TEST"
  "docs:vite@8.3.6 $REACT"
  "scripts:$TS esbuild@0.28.2 lodash@4.18.1"
)

for p in "${projects[@]}"; do
  name="${p%%:*}" deps="${p#*:}"
  mkdir -p "$DIR/$name"
  (cd "$DIR/$name" && npm init -y >/dev/null && npm install --no-audit --no-fund --silent $deps)
  echo "installed $name"
done
