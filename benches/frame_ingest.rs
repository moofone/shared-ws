use bytes::Bytes;
use criterion::{Criterion, black_box, criterion_group, criterion_main};
use tokio_tungstenite::tungstenite::{
    Message as TungMessage, Utf8Bytes, protocol::CloseFrame as TungCloseFrame,
};

use shared_ws::ws::{WsCloseFrame, WsFrame, WsText};

#[inline]
fn close_to_core_clone(frame: Option<TungCloseFrame>) -> Option<WsCloseFrame> {
    frame.map(|f| WsCloseFrame {
        code: u16::from(f.code),
        reason: AsRef::<Bytes>::as_ref(&f.reason).clone(),
    })
}

#[inline]
fn close_to_core_move(frame: Option<TungCloseFrame>) -> Option<WsCloseFrame> {
    frame.map(|f| WsCloseFrame {
        code: u16::from(f.code),
        reason: f.reason.into(),
    })
}

#[inline]
fn msg_to_frame_clone(msg: TungMessage) -> WsFrame {
    match msg {
        TungMessage::Text(text) => {
            let bytes = AsRef::<Bytes>::as_ref(&text).clone();
            // SAFETY: `text` is validated UTF-8; we cloned the underlying bytes.
            WsFrame::Text(unsafe { WsText::from_bytes_unchecked(bytes) })
        }
        TungMessage::Binary(bytes) => WsFrame::Binary(bytes),
        TungMessage::Ping(bytes) => WsFrame::Ping(bytes),
        TungMessage::Pong(bytes) => WsFrame::Pong(bytes),
        TungMessage::Close(frame) => WsFrame::Close(close_to_core_clone(frame)),
        TungMessage::Frame(_) => WsFrame::Binary(Bytes::new()),
    }
}

#[inline]
fn msg_to_frame_move(msg: TungMessage) -> WsFrame {
    match msg {
        TungMessage::Text(text) => {
            // SAFETY: tungstenite `Text` payloads are validated UTF-8.
            WsFrame::Text(unsafe { WsText::from_bytes_unchecked(text.into()) })
        }
        TungMessage::Binary(bytes) => WsFrame::Binary(bytes),
        TungMessage::Ping(bytes) => WsFrame::Ping(bytes),
        TungMessage::Pong(bytes) => WsFrame::Pong(bytes),
        TungMessage::Close(frame) => WsFrame::Close(close_to_core_move(frame)),
        TungMessage::Frame(_) => WsFrame::Binary(Bytes::new()),
    }
}

fn bench_text_frame_conversion(c: &mut Criterion) {
    // Use a non-static buffer to better approximate real IO buffers.
    let payload = Bytes::from(vec![b'{'; 512]);
    let text = Utf8Bytes::try_from(payload).unwrap_or_else(|_| Utf8Bytes::from_static("{}"));

    c.bench_function("ingest_1000_text_frames_clone", |b| {
        b.iter(|| {
            for _ in 0..1000 {
                let msg = TungMessage::Text(text.clone());
                let frame = msg_to_frame_clone(black_box(msg));
                black_box(frame);
            }
        })
    });

    c.bench_function("ingest_1000_text_frames_move", |b| {
        b.iter(|| {
            for _ in 0..1000 {
                let msg = TungMessage::Text(text.clone());
                let frame = msg_to_frame_move(black_box(msg));
                black_box(frame);
            }
        })
    });
}

fn bench_close_reason_conversion(c: &mut Criterion) {
    let reason = Utf8Bytes::from("normal closure: testing bytes move");
    let close = TungCloseFrame {
        code: 1000.into(),
        reason,
    };

    c.bench_function("ingest_1000_close_reason_clone", |b| {
        b.iter(|| {
            for _ in 0..1000 {
                let f = close.clone();
                let out = close_to_core_clone(Some(black_box(f)));
                black_box(out);
            }
        })
    });

    c.bench_function("ingest_1000_close_reason_move", |b| {
        b.iter(|| {
            for _ in 0..1000 {
                let f = close.clone();
                let out = close_to_core_move(Some(black_box(f)));
                black_box(out);
            }
        })
    });
}

criterion_group!(
    benches,
    bench_text_frame_conversion,
    bench_close_reason_conversion
);
criterion_main!(benches);
