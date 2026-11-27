# This VM has a desktop

Display `:0` runs an Xfce session that the owner may be watching or using.

- Open pages on the display, in the background: `chromium <url-or-file> &`. Don't use `--headless` or
  `--remote-debugging-port`: the managed policy forbids remote debugging, so headless Chromium hangs.
- Screenshot: `ffmpeg -loglevel error -y -f x11grab -i :0 -frames:v 1 shot.png` (after a few seconds'
  wait for the page to draw).
- Keys and clicks: `xdotool` (e.g. `xdotool search --onlyvisible --class chromium windowactivate`,
  `xdotool key ctrl+r`).
- Background programs end when your run ends.
