# Online e-paper profile control

This optional protocol-v2 extension controls named receiver refresh policies;
v2.2 adds an atomic CUSTOM semantic policy. It does not expose native Quill or
vendor waveform values to producers.

## Layer boundary

- `Frame.intent` and `Frame.content_class` remain the only per-frame hints.
- `EpaperProfile` selects an rm-display `RefreshProfile` preset:
  `REALTIME`, `ANIMATE`, `BALANCED`, `READING`, or `QUALITY`; v2.2 CUSTOM
  supplies all portable semantic choices at once.
- The receiver maps that policy to Quill's per-submit mode/full arguments.
- Quill's C ABI is unchanged; there is no profile state in `libquill.so`.
- Existing `FrameMetrics.waveform` remains a read-only diagnostic of the
  receiver's completed decision; it cannot influence a later frame.
- The receiver may preserve operator overrides such as cleanup interval and
  damage tile, and always reports the effective state.

## Producer integration checklist

1. Add `PROTOCOL_FEATURE_EPAPER_PROFILE_CONTROL` to `ClientHello.features`.
2. Enable the control only if `ServerHello.features` echoes that feature.
3. Allocate `EpaperProfileRequest.request_id` from a connection-local counter
   starting at 1. It must be nonzero and strictly increase; reset it only after
   reconnect creates a new protocol session.
4. Send one of:
   - QUERY: `operation=QUERY`, `requested_profile=UNSPECIFIED`;
   - preset SET: `operation=SET`, `requested_profile=REALTIME|ANIMATE|BALANCED|READING|QUALITY`, no `custom`;
   - CUSTOM SET after v2.2 and `EPAPER_CUSTOM_PROFILE` negotiation: `operation=SET`, `requested_profile=CUSTOM`, and one complete `custom` configuration.
5. Put the request in `Envelope.epaper_profile_request` with the established
   `session_id` and the normal strictly increasing Envelope `message_id`.
6. Correlate `Envelope.epaper_profile_result` by `request_id`. Accept
   `APPLIED` or `UNCHANGED`; surface `REJECTED` and `UNSUPPORTED` to the caller.
7. Read `result.active` as authoritative. It contains the active profile,
   cleanup interval, large-update threshold, static-fast-debt threshold,
   damage tile, and first-frame cleanup setting. `active.effective` also
   contains all four effective waveform selections and complete refresh
   configuration. Do not infer these values from the requested preset.
8. If `cleanup_pending` is true, the policy is active but a required full
   cleanup is still armed. `cleanup_performed` is true only after the backend
   successfully accepted that full-panel submission.

A profile SET can flush a pending LATEST or SETTLED frame immediately. In that
case its terminal `FrameResult` arrives before the correlated profile result.
The producer's message reader must continue routing ordinary frame/input/action envelopes
while awaiting `EpaperProfileResult`; it must not assume the next envelope is
the result.

An unnegotiated request receives a nonfatal `UNSUPPORTED` result. Unknown
operations/profiles, QUERY with a non-UNSPECIFIED profile, SET with
UNSPECIFIED, illegal custom presence, incomplete/invalid custom values, and
stale/duplicate request IDs receive nonfatal `REJECTED`. Custom validation is
atomic: rejection changes no policy field.

The selected profile is scoped to the current TCP/PSK protocol connection. It
is not a persistent receiver setting and is discarded on reconnect.

## Preset decisions

The enum keeps its original wire values (`ANIMATE=1`, `BALANCED=2`,
`QUALITY=3`) and appends `REALTIME=4`, `READING=5`, and `CUSTOM=6`. Numeric order is not
quality order.

| Profile | cleanup_after_updates | large threshold | static fast debt | LATEST text | LATEST photo | LATEST video | SETTLED |
| --- | ---: | ---: | ---: | --- | --- | --- | --- |
| REALTIME | 360 | disabled | 12 | Fastest | Fastest | Fastest | Fastest |
| ANIMATE | 180 | disabled | 8 | Fastest | Quality | Fastest | Fast |
| BALANCED | 90 | disabled | 6 | Fast | Quality | Fastest | Quality |
| READING | 45 | 50% | 3 | Quality | Quality | Fast | Quality |
| QUALITY | 20 | 33% | disabled | Quality | Quality | Quality | Quality |

When cleanup is due, QUALITY selects FullQuality and the other named presets
and Custom select Quality; the receiver separately forces a complete panel
refresh.
Intervals count successful physical partial submissions. `clean_first_frame`
and 64-pixel damage tiles remain the defaults for every preset.

CUSTOM requires Fastest, Fast, or Quality for LATEST text/mixed, photo, video,
and SETTLED; FullQuality is not exposed. It also carries partial permission,
cleanup intervals, first-frame cleanup, 0..100 large-area threshold, and a
power-of-two damage tile from 8 through 512. Validation is atomic. The receiver
reports the complete effective configuration and retains the latest valid
CUSTOM policy for the current session so the local menu can switch back to it.

SETTLED remains an unsupersedable terminal barrier under every profile.
Realtime's Fastest, Animate's Fast, or a fast Custom SETTLED presents the exact
final pixels but does not itself promise ghost cleanup. Periodic, static-fast-
debt, large-area, first-frame, explicit, and recovery policy remain the only
paths to a receiver-selected complete refresh.

Profile switching retains `partial_refresh_enabled`, `clean_first_frame`, and
damage-tile settings. Cleanup interval, large-area threshold, and static fast
debt threshold move to the new preset only when they still equal the old
preset; explicit overrides remain authoritative. A valid switch remains a
full-cleanup barrier.
