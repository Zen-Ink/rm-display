# Linux CLI producers

`rm-display-cli stream` deliberately consumes a simple transport-neutral
adapter format: tightly packed fixed-size Gray8 frames on stdin. Existing media
tools can therefore act as producers without learning RMD2. The CLI performs
the receiver handshake, opens the surface, scales the declared source geometry,
sends frames, applies backpressure, and repeats the last frame as `SETTLED` at
clean EOF.

## FFmpeg

FFmpeg is the most portable adapter. Its `rawvideo` muxer emits no geometry
metadata, so the width and height supplied to `rm-display-cli` must exactly
match the output filter.

X11 full-screen capture at 4 fps:

```sh
SOURCE_WIDTH=1920
SOURCE_HEIGHT=1080
ffmpeg -hide_banner -loglevel warning \
  -f x11grab -framerate 4 \
  -video_size "${SOURCE_WIDTH}x${SOURCE_HEIGHT}" -i "${DISPLAY}" \
  -vf format=gray -pix_fmt gray -f rawvideo pipe:1 |
  rm-display-cli stream --width "$SOURCE_WIDTH" --height "$SOURCE_HEIGHT" \
    --stats
```

A video file or any FFmpeg-supported input can use the same adapter:

```sh
SOURCE_WIDTH=1280
SOURCE_HEIGHT=720
ffmpeg -hide_banner -loglevel warning -re -i input.mp4 \
  -vf "fps=4,scale=${SOURCE_WIDTH}:${SOURCE_HEIGHT},format=gray" \
  -pix_fmt gray -f rawvideo pipe:1 |
  rm-display-cli stream --width "$SOURCE_WIDTH" --height "$SOURCE_HEIGHT" \
    --stats
```

For direct DRM/KMS capture, FFmpeg provides `kmsgrab`; it requires DRM master
or `CAP_SYS_ADMIN`, and the scanout must be downloadable:

```sh
SOURCE_WIDTH=1280
SOURCE_HEIGHT=720
ffmpeg -hide_banner -loglevel warning -f kmsgrab -framerate 4 -i - \
  -vf "hwdownload,format=bgr0,scale=${SOURCE_WIDTH}:${SOURCE_HEIGHT},format=gray" \
  -pix_fmt gray -f rawvideo pipe:1 |
  rm-display-cli stream --width "$SOURCE_WIDTH" --height "$SOURCE_HEIGHT" \
    --stats
```

The FFmpeg input-device documentation covers `x11grab`, `kmsgrab`, V4L2,
AVFoundation, and Windows `gdigrab`:
<https://ffmpeg.org/ffmpeg-devices.html>.

## GStreamer on X11

`ximagesrc` uses XDamage when available. `videoconvertscale` can produce
GRAY8, and `fdsink` writes buffers to stdout. Use an output width divisible by
four so GStreamer's Gray8 row stride is tightly packed and agrees with the CLI:

```sh
SOURCE_WIDTH=1920
SOURCE_HEIGHT=1080
GST_XINITTHREADS=1 gst-launch-1.0 -q \
  ximagesrc use-damage=true show-pointer=true \
  ! video/x-raw,framerate=4/1 \
  ! videoconvertscale \
  ! "video/x-raw,format=GRAY8,width=${SOURCE_WIDTH},height=${SOURCE_HEIGHT},pixel-aspect-ratio=1/1" \
  ! fdsink fd=1 sync=false |
  rm-display-cli stream --width "$SOURCE_WIDTH" --height "$SOURCE_HEIGHT" \
    --stats
```

Upstream element documentation:
[`ximagesrc`](https://gstreamer.freedesktop.org/documentation/ximagesrc/),
[`videoconvertscale`](https://gstreamer.freedesktop.org/documentation/videoconvertscale/),
and [`fdsink`](https://gstreamer.freedesktop.org/documentation/coreelements/fdsink.html).

## Wayland

For wlroots compositors that implement `wlr-screencopy-v1`, `wf-recorder` can
write raw Gray8 to stdout. Pin the capture rectangle so the CLI knows its exact
geometry:

```sh
SOURCE_WIDTH=1920
SOURCE_HEIGHT=1080
wf-recorder --geometry "0,0 ${SOURCE_WIDTH}x${SOURCE_HEIGHT}" \
  --framerate 4 --codec rawvideo --muxer rawvideo \
  --pixel-format gray --file - |
  rm-display-cli stream --width "$SOURCE_WIDTH" --height "$SOURCE_HEIGHT" \
    --stats
```

`wf-recorder` is wlroots-specific, not a generic GNOME/KDE capture solution;
see its [upstream README](https://github.com/ammen99/wf-recorder). Generic
Wayland capture uses the XDG ScreenCast portal to obtain an authorized PipeWire
stream. That requires a portal session and user selection before a consumer can
open the returned PipeWire node, so there is no reliable compositor-neutral
one-line `gst-launch` producer. A future portal adapter can still emit the same
Gray8 stdin contract without changing RMD2.

## Diagnostics

Add `--stats` for per-frame human-readable timings on stderr, or
`--stats-jsonl metrics.jsonl` for structured records. The producer pixels remain
on stdout/stdin as appropriate; metrics never contaminate the Gray8 pipe.

```sh
ffmpeg ... -f rawvideo pipe:1 |
  rm-display-cli stream --width 1920 --height 1080 \
    --stats --stats-jsonl metrics.jsonl
```

`backend_submit_us` ends when Quill/libqsgepaper returns. No available vendor
API currently proves when the physical pigment has settled, so the metrics
explicitly report `physical_ink_settle_us` as unavailable.
