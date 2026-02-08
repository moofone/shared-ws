# JSON Parse Modes (Full vs Partial vs Hybrid)

This library has two places you can parse inbound frames:

- **Ingress path**: `WsIngress::on_frame(WsFrame) -> WsIngressAction<M>`
  - Runs in the reader task (tight IO loop), outside the actor runtime.
  - Best place to do *high-volume, low-allocation* parsing and filtering.
- **Actor path**: `WsEndpointHandler::parse_frame(&WsFrame)` / `parse(&[u8])`
  - Runs on the actor mailbox.
  - Best for simpler endpoints where throughput is lower and API ergonomics matter more.

Because `WsEndpointHandler::Message: Send + 'static`, **messages cannot borrow `&str` directly**
from a single frame buffer. Partial parsing must either:

- extract primitives (`i64`, `f64`, `bool`) and only allocate/copy the minimal strings you need, or
- intern/canonicalize strings (e.g., symbols) into `Arc<str>` to amortize allocations.

## Goals

- Support endpoints that need **full JSON -> typed struct** (Serde) for some message types.
- Support endpoints that need **only a few fields** for hot-path messages.
- Support **hybrid routing**: inspect a tiny set of fields first, then decide `Ignore` vs `Partial` vs `Full`.

## Constraints / Invariants

1. `WsFrame::Text(Bytes)` is *intended* to contain valid UTF-8 (transports should enforce this),
   but it is a public enum, so callers can construct invalid values. If you want to skip UTF-8
   validation for performance, do it only where you can justify the invariant.
2. The partial path should strive for **no DOM construction** (`sonic_rs::Value`) and avoid
   per-frame heap allocations where feasible.
3. The ingress path should remain **non-blocking** and should not introduce time-based batching
   unless explicitly configured (this crate currently does not do time-based batching of emits).

## Mode A: Full Typed Deserialization

Use when you truly need the entire payload, but prefer a typed struct over a DOM.

Pattern (ingress):

```rust
use shared_ws::ws::{WsFrame, WsIngress, WsIngressAction, WsDisconnectCause};

#[derive(serde::Deserialize, Debug)]
struct FullMsg {
    // owned fields (String, Vec, etc) are 'static-friendly
    kind: String,
    // ...
}

struct FullIngress;

impl WsIngress for FullIngress {
    type Message = FullMsg;

    fn on_frame(&mut self, frame: WsFrame) -> WsIngressAction<Self::Message> {
        let WsFrame::Text(bytes) = frame else {
            return WsIngressAction::Forward(frame);
        };

        // Safe option (validates UTF-8):
        let Ok(text) = std::str::from_utf8(bytes.as_ref()) else {
            return WsIngressAction::Ignore;
        };

        match sonic_rs::from_str::<FullMsg>(text) {
            Ok(m) => WsIngressAction::Emit(m),
            Err(_) => WsIngressAction::Ignore,
        }
    }
}
```

Notes:

- `sonic_rs::from_str` avoids UTF-8 validation because `&str` is already valid UTF-8.
- If your transport guarantees text UTF-8 and you want max speed, you can justify
  `unsafe { std::str::from_utf8_unchecked(...) }` locally (with a `debug_assert!`).

## Mode B: Partial Field Extraction (No DOM)

Use when the hot path only needs a few fields and you want minimal allocations.

Recommended approach:

- Use `sonic_rs::get(&str, path)` for 1-3 fields (no Vec allocation).
- Use `sonic_rs::get_many(&str, &PointerTree)` for many fields (single parse pass) but note it
  returns a `Vec`, so there is a per-call allocation.
- Convert extracted string numbers with `lexical-core` (fast, ASCII-oriented).

Pattern (ingress, minimal fields):

```rust
#[derive(Debug)]
pub struct TradeLite {
    pub ts: i64,
    pub price: f64,
    pub qty: f64,
    pub symbol: std::sync::Arc<str>, // intern/canonicalize to avoid per-frame String allocs
}

fn parse_f64(s: &str) -> Option<f64> {
    Some(lexical_core::parse::<f64>(s.as_bytes()).ok()?)
}

fn parse_i64(s: &str) -> Option<i64> {
    Some(lexical_core::parse::<i64>(s.as_bytes()).ok()?)
}
```

Then in `on_frame`, do:

- `type/topic` classification via `sonic_rs::get(text, &["type"])`
- `price/qty/ts/symbol` extraction only for the message kinds you actually process

### String allocation strategy for partial mode

Because `Message: 'static`, you cannot return `&str`. Options:

- `Arc<str>` plus interning table keyed by the symbol bytes.
- `Bytes` for opaque identifiers, if you don’t need `&str` semantics later.
- `SmallString`/`smol_str` if you want inline storage for short strings (not currently a dependency).

## Mode C: Hybrid Router (Classify then Partial or Full)

This is the common best-of-both worlds:

1. Extract 1-2 fields to classify (`type`, `op`, `event`, `method`, `topic`, etc).
2. If irrelevant: `Ignore`.
3. If hot path: partial extraction -> emit `TradeLite` (or similar).
4. If rare path: full typed deserialize -> emit `FullMsg`.

You typically represent your application message as an enum:

```rust
#[derive(Debug)]
pub enum AppMsg {
    Trade(TradeLite),
    Control(FullMsg),
}
```

Ingress then emits `AppMsg` variants.

## Where This Hooks In

- If you implement parsing in **ingress**, configure your actor with that ingress and keep
  `WsEndpointHandler::parse*` either unused or as a fallback for forwarded frames.
- If you implement parsing in the **actor**, you can still use the partial mode ideas inside
  `WsEndpointHandler::parse_frame`, but you lose the benefit of filtering before mailbox enqueue.

## Performance Notes / DoD

- Partial mode should avoid `sonic_rs::Value` DOM creation in the hot path.
- Hybrid mode should ensure classification does not allocate (prefer `get` on `&str`).
- Any use of unchecked UTF-8 operations should be paired with an explicit invariant statement
  and a `debug_assert!` that would catch violations in dev/test builds.

## Suggested Bench Coverage

- Existing DOM parse benchmark: `inject_1000_json_frames_parse_only`
- Add/keep partial extraction benches:
  - `get_fields_only` (multiple `get` calls)
  - `get_many_only` (PointerTree)
- Add a hybrid bench that classifies and then does partial/full based on a skewed distribution
  (e.g., 99% hot messages, 1% full decode).

