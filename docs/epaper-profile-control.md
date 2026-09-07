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

| Profile | adaptive cleanup | LATEST text | LATEST photo | LATEST video | SETTLED |
| --- | --- | --- | --- | --- | --- |
| REALTIME | 8 screens, 10 s idle | Fastest | Quality | Fastest | Quality |
| ANIMATE | 6 screens, 8 s idle | Fastest | Quality | Fastest | Quality |
| BALANCED | 4 screens, 6 s idle | Fast | Quality | Fastest | Quality |
| READING | 3 screens, 5 s idle | Quality | Quality | Fast | Quality |
| QUALITY | 2 screens, 4 s idle | Quality | Quality | Quality | Quality |

When cleanup is due, QUALITY selects FullQuality and the other named presets
and Custom select Quality; the receiver separately forces a complete panel
refresh.
Named presets disable periodic, large-area, and static-fast-debt cleanup by
default. Their adaptive budgets count actual submitted partial damage from
remote frames and overlays, then wait until the configured idle time has
elapsed. Sixty-four-pixel damage tiles remain the
default for every preset.

CUSTOM requires Fastest, Fast, or Quality for LATEST text/mixed, photo, video,
and SETTLED; FullQuality is not exposed. It also carries partial permission,
cleanup intervals, first-frame cleanup, 0..100 large-area threshold, and a
power-of-two damage tile from 8 through 512. Validation is atomic. The receiver
reports the complete effective configuration and retains the latest valid
CUSTOM policy for the current session so the local menu can switch back to it.

SETTLED remains an unsupersedable terminal barrier under every profile.
Named-profile SETTLED uses a sparse Quality partial repaint to restore exact
grayscale without a full-panel flash. A fast Custom SETTLED presents the final
pixels but does not itself promise ghost cleanup. Periodic, static-fast-debt,
large-area, named-profile adaptive-idle, first-frame, explicit, and recovery
policy remain the paths to a receiver-selected complete refresh.

Named profiles have distinct receiver-local adaptive cleanup budgets shown in
the table. This internal physical-panel policy is deliberately not a
producer-configurable profile field and does not change the wire protocol.

Profile switching retains `partial_refresh_enabled`, `clean_first_frame`, and
damage-tile settings. Cleanup interval, large-area threshold, and static fast
debt threshold move to the new preset only when they still equal the old
preset; explicit overrides remain authoritative. A valid switch remains a
full-cleanup barrier.
