# Online refresh parameter control

This negotiated extension changes receiver-owned refresh parameters for one
connected session. It complements named/custom profile control and never
places a native Quill waveform in a producer request or `Frame`.

## Producer integration contract

1. Add `PROTOCOL_FEATURE_EPAPER_REFRESH_CONTROL` to `ClientHello.features`.
2. Enable the API only when `ServerHello.features` echoes it.
3. Use `Envelope.epaper_refresh_request` (field 47) and route
   `Envelope.epaper_refresh_result` (field 48).
4. Allocate `EpaperRefreshRequest.request_id` from a connection-local,
   nonzero, strictly increasing counter. It is independent of the profile
   request counter and resets on reconnect.
5. Construct operations exactly as follows:
   - `QUERY`: no optional parameter is present;
   - `UPDATE`: at least one of `partial_refresh_enabled`,
     `cleanup_after_updates`, `large_update_threshold_percent`, or
     `static_cleanup_after_fast_updates` is present;
   - `CLEANUP`: no optional parameter is present.
6. Preserve protobuf presence. `partial_refresh_enabled=false` and numeric
   zero are explicit updates, not omission. Threshold zero disables the area
   trigger; otherwise only 1..100 is valid. Cleanup interval zero disables the
   periodic trigger. Static-cleanup zero disables the fast-update debt trigger.
7. Correlate by `request_id`. Treat `APPLIED` and `UNCHANGED` as success;
   surface `REJECTED`, `UNSUPPORTED`, and `FAILED` to the caller.
8. Read `result.active` as authoritative. It returns all four effective
   parameters, `presented_since_full_refresh`,
   `fast_updates_since_settled`, and `cleanup_pending`.
   `cleanup_performed` is true only after a successful full-panel backend
   submission for this request.

`presented_since_full_refresh` counts successful physical partial panel
submissions. A zero-damage logical presentation does not increment it.

`fast_updates_since_settled` counts successful Fast/Fastest submissions in the
current motion episode. When its configured threshold is reached, the receiver
waits for `SETTLED`, performs one complete cleanup, and clears the count. A
normal successful SETTLED repaint below the threshold also ends the episode
and clears the count. Its waveform follows the active preset/custom policy;
only a due cleanup trigger makes it a complete refresh. A Fast/Fastest SETTLED
is still the terminal final-frame barrier, but does not by itself remove
ghosting.

`CLEANUP` may flush an outstanding LATEST or SETTLED frame. Its terminal
`FrameResult` is sent before the cleanup result, so the producer message router
must continue dispatching unrelated envelopes while awaiting the correlated
result. With no presented frame, cleanup remains armed for the next valid
presentation. A backend failure returns `FAILED`, invalidates the frame delta
base, and leaves cleanup armed.

The four settings, physical partial-submission counter, and pending cleanup are
connection-scoped and survive both SurfaceClose/Open and direct surface
replacement. The motion-local fast counter and its settle damage are
surface-local and reset when that surface is replaced. Disconnect restores the
receiver process's command-line baseline for the next producer. The existing
profile remains active; UPDATE changes only the explicitly present parameters.
