#!/bin/sh
# Symlink the release binary into ~/.local/bin so pane commands can run
# `herdr-slack` by name.
set -eu

root="$(cd "$(dirname "$0")/.." && pwd)"
bin="$root/target/release/herdr-slack"

[ -x "$bin" ] || cargo build --release --manifest-path "$root/Cargo.toml"

mkdir -p "$HOME/.local/bin"
ln -sf "$bin" "$HOME/.local/bin/herdr-slack"
echo "linked $HOME/.local/bin/herdr-slack -> $bin"

case ":$PATH:" in
  *":$HOME/.local/bin:"*) ;;
  *) echo "note: $HOME/.local/bin is not on PATH" >&2 ;;
esac
