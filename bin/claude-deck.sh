#!/bin/sh
# Runs claude-deck for herdr-plugin.toml. The manifest calls this through
# $HERDR_PLUGIN_ROOT because the deck pane opens in the session directory, not the
# plugin's. Prefers the binary the install step downloaded beside this script, then
# one on PATH or in the usual Homebrew and cargo locations.

root=${HERDR_PLUGIN_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}
for bin in "$root/bin/claude-deck" "$(command -v claude-deck 2>/dev/null)" \
    /opt/homebrew/bin/claude-deck /usr/local/bin/claude-deck \
    /home/linuxbrew/.linuxbrew/bin/claude-deck "$HOME/.cargo/bin/claude-deck"; do
    if [ -n "$bin" ] && [ -x "$bin" ]; then
        exec "$bin" "$@"
    fi
done
echo "claude-deck: binary not found; install it (https://github.com/bigsan/claude-deck#install) and reinstall the plugin" >&2
exit 127
