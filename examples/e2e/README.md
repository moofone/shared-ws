# examples-ws

Deribit-style JSON-RPC over WebSocket examples using `shared-ws` delegated requests.

## Unconfirmed Decision Tree (Caller-Owned)

When a delegated request returns `Unconfirmed`, the frame was sent (or assumed sent) but no
confirmation was observed before the deadline. Recommended caller strategies:

1. Probe: send a follow-up “list subscriptions” (or equivalent) request and confirm via state.
2. Treat first notification as implied confirmation (only if safe for the endpoint/method).
3. Retry with an explicit idempotency guard if the endpoint supports it (or use a unique request id
   and tolerate duplicates).

`shared-ws` intentionally does not implement probing; it only surfaces the ambiguity.

