# E-paper frame and refresh strategy

This document summarizes the implemented policy shared by producers, the
protocol scheduler, and the Quill backend. It is the operational reference for
diagnosing an unexpected partial or complete refresh.

## Ownership boundary

- Producers decide which rendered captures are worth sending. Motion captures
  are conflated before pixel conversion and encoding.
- The protocol carries only `Frame.intent` (`LATEST` or `SETTLED`) and
  `content_class` per frame. A negotiated v2.2 Custom profile may carry stable
  semantic waveform policy, but never a native Quill mode or partial/full flag.
- The receiver computes pixel damage, chooses waveform and partial/complete
  update, tracks ghost-cleanup debt, and submits one union rectangle to Quill.
- Quill receives `mode`, `complete_refresh`, and `is_color` from the receiver.
  A producer cannot call or configure the vendor API directly.

This boundary keeps browser, phone mirror, image, and Linux producers on the
same protocol while leaving physical-panel policy on the device.

## Producer motion phases

Producers may expose three motion modes for scrolling or captured animation:

| Mode | During motion | At 200 ms quiet |
| --- | --- | --- |
| Live | sample up to the ordinary stream/receiver FPS | one guaranteed `SETTLED` |
| Sampled | leading capture plus configured low-rate representative captures | latest capture as `SETTLED` |
| Final only | no intermediate capture is sent | latest capture as `SETTLED` |

Every retained intermediate is `LATEST`. Repeated source activity extends the
quiet deadline even when no sampled capture is sent, so a sampling gap cannot
be mistaken for a static page. Capture queues, the conversion queue, and the
receiver's pending `LATEST` are all conflated; stale scroll frames do not form
a FIFO backlog. Transport flow control still applies the negotiated frame and
decoded-byte credit limits.

A producer may use a bounded final-only window for discrete page turns so it
does not transmit a native fling or page animation. Producer shutdown should
also complete or cancel outstanding work according to the protocol's terminal
frame rules.

## Damage and partial refresh

The receiver composites the remote base and local overlay, compares the result
against the last physically presented surface in 64-pixel tiles, and submits
only changed damage. Multiple rectangles are reduced to the bounding union at
the current Quill C ABI.

When `partial_refresh_enabled=true` and no complete-refresh trigger is active,
the update remains partial. A `SETTLED` frame is still partial; its waveform is
profile-specific: Fastest for Realtime, Fast for Animate, and Quality for the
other presets. Quality means the changed region is repainted accurately; it
does not imply a full-panel flash.

Fast/Fastest damage is retained as a settle region. The next `SETTLED` repaints
that region with its selected settled waveform even if its pixels already
equal the latest software surface. After that successful repaint, the
motion-local fast debt is cleared. A fast SETTLED still guarantees that the
exact final frame reaches a terminal result; it deliberately does not promise
ghost removal. Periodic, static-fast-debt, large-damage, first-frame, explicit,
and recovery policy own full-panel cleanup.

If composed pixels are already physically equivalent and there is no fast
settle region, the receiver completes the frame logically without calling
Quill. Zero-damage frames do not accrue cleanup debt.

## Waveform presets

| Profile | LATEST text | LATEST photo | LATEST video | SETTLED | periodic | large area | static fast debt |
| --- | --- | --- | --- | --- | ---: | ---: | ---: |
| Realtime | Fastest | Fastest | Fastest | Fastest | 360 | off | 12 |
| Animate | Fastest | Quality | Fastest | Fast | 180 | off | 8 |
| Balanced | Fast | Quality | Fastest | Quality | 90 | off | 6 |
| Reading | Quality | Quality | Fast | Quality | 45 | 50% | 3 |
| Quality | Quality | Quality | Quality | Quality | 20 | 33% | off |

Fastest, Fast, Quality, and FullQuality map to current Quill mode values 0, 1,
3, and 4. For a complete refresh, the Quality profile uses FullQuality; other
named profiles and Custom use Quality plus the complete-refresh flag.

Protocol v2.2 CUSTOM supplies all four semantic waveform columns,
partial-refresh permission, all three automatic cleanup parameters,
first-frame cleanup, and damage tile atomically. Each waveform accepts only
Fastest, Fast, or Quality; FullQuality is intentionally unavailable because
producers cannot turn a waveform choice into a complete refresh. Damage tile
must be a power of two from 8 through 512, and the large-area threshold remains
0..100.

## Complete-refresh triggers and priority

The receiver reports the selected cause in
`FrameMetrics.full_refresh_reason`. The effective priority is:

1. `PARTIAL_DISABLED`: disabling partial refresh makes every nonempty physical
   update complete. It does not mean "disable refresh".
2. `FORCED`: explicit cleanup, five-finger cleanup, profile switch, or recovery
   protection.
3. `STATIC_FAST_DEBT`: the configured number of Fast/Fastest submissions was
   reached during one motion episode and the source has now sent `SETTLED`.
4. `FIRST_FRAME`: the profile requests a clean initial physical presentation.
5. `PERIODIC`: successful partial submissions reached
   `cleanup_after_updates`.
6. `LARGE_DAMAGE`: bounding damage reached
   `large_update_threshold_percent`.

`cleanup_after_updates=0`, `large_update_threshold_percent=0`, and
`static_cleanup_after_fast_updates=0` independently disable those three
automatic triggers. They do not disable first-frame safety, explicit/five-
finger cleanup, profile-switch cleanup, backend recovery, or the effect of
turning partial refresh off.

Static cleanup deliberately runs only at a `SETTLED` barrier. Fast updates
increment its counter only after Quill successfully accepts them. Reaching the
threshold never inserts a full flash in the middle of scrolling. A successful
SETTLED presentation below the threshold clears the motion-local count even if
the selected waveform is Fast/Fastest; that fast barrier is not itself a
cleanup. A complete cleanup clears all associated settle damage and count.

## Online control and persistence

The negotiated refresh-control state contains:

- `partial_refresh_enabled`;
- `cleanup_after_updates`;
- `large_update_threshold_percent`;
- `static_cleanup_after_fast_updates`;
- `presented_since_full_refresh`;
- `fast_updates_since_settled`;
- `cleanup_pending`.

The profile-control result additionally returns a complete `effective`
configuration. In v2.2 this is the authoritative echo for either a named preset
or an atomic Custom policy, including all four semantic waveforms and refresh
parameters. Producers must display this result rather than assume their request
was accepted verbatim.

Producers may persist explicit user preferences and replay them after
reconnect. A "follow receiver" choice should only query state and leave the
receiver baseline authoritative. Settings, periodic physical-update debt, and explicit
cleanup-pending state survive surface replacement inside one connection.
Motion-local fast debt and settle damage reset with the surface. A new
connection resets all state to the receiver process baseline.

Changing a profile is a cleanup barrier. It applies online without reopening a
surface, but a required full cleanup must successfully reach Quill before the
receiver reports it as performed.

## Diagnosing a refresh

Use producer metrics or the Linux CLI `info`/refresh query. For every
complete update inspect `full_refresh_reason` before changing thresholds. Also
compare:

- `receiver_queue_us`: scheduler/FPS waiting;
- `receiver_compose_us`: base plus overlay and damage work;
- `receiver_convert_us`: protocol pixels to Quill framebuffer format;
- `receiver_submit_us`: Quill/vendor submission;
- `receiver_present_us`: total receiver presentation path.

`physical_ink_settle_us` remains unknown: Quill submission completion is not a
measurement of the panel's visible pigment settling time.
