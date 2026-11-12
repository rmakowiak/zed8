#!/bin/sh
# Installs the Zed8.app that bundle.sh built, as soon as you quit Zed8.
#
# A Claude session inside Zed8 can start this script. It runs on after
# Zed8 quits: it waits, swaps the app in /Applications and opens Zed8.
#
# Usage: zed8/install.sh
set -eu
cd "$(dirname "$0")/.."

new_app="$PWD/target/zed8/Zed8.app"
log="$HOME/Library/Logs/zed8-install.log"

if [ ! -d "$new_app" ]; then
    echo "No build at $new_app. Run zed8/bundle.sh --no-install first."
    exit 1
fi

# Detach from the terminal of the caller, so the script survives when
# Zed8 quits and closes that terminal.
if [ "${ZED8_INSTALL_DETACHED:-}" != 1 ]; then
    # A new session (setsid) keeps Zed8 from taking the script down with
    # the terminal. macOS has no setsid command, so Python does it.
    ZED8_INSTALL_DETACHED=1 nohup python3 -c 'import os, sys; os.setsid(); os.execv(sys.argv[1], sys.argv[1:])' "$PWD/zed8/install.sh" >>"$log" 2>&1 </dev/null &
    echo "Waiting for Zed8 (and official Zed) to quit. Quit them (Cmd-Q) to install the new build."
    echo "Log: $log"
    exit 0
fi

echo "$(date): waiting for Zed8 and official Zed to quit"
# Official Zed uses the same data, so it must be closed too.
while pgrep -f "/Applications/Zed8.app/Contents/MacOS/zed" >/dev/null \
    || pgrep -f "/Applications/Zed.app/Contents/MacOS/zed" >/dev/null; do
    sleep 1
done

echo "$(date): installing $new_app"
rm -rf /Applications/Zed8.app.previous
if [ -d /Applications/Zed8.app ]; then
    # Keeps the last build, to go back with one command if the new one fails.
    mv /Applications/Zed8.app /Applications/Zed8.app.previous
fi
cp -R "$new_app" /Applications/Zed8.app
mkdir -p "$HOME/.local/bin"
ln -sf /Applications/Zed8.app/Contents/MacOS/cli "$HOME/.local/bin/zed8"

echo "$(date): opening Zed8"
open -a /Applications/Zed8.app
