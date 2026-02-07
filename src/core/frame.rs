use bytes::Bytes;

/// Transport-neutral websocket frame type.
///
/// This is the public "wire" surface for the library: transports convert their native frame
/// representation into/from `WsFrame`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WsFrame {
    Text(Bytes),
    Binary(Bytes),
    Ping(Bytes),
    Pong(Bytes),
    Close(Option<WsCloseFrame>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WsCloseFrame {
    pub code: u16,
    pub reason: Bytes,
}

impl WsFrame {
    #[inline]
    pub fn text_static(s: &'static str) -> Self {
        // &'static str is valid UTF-8 by construction.
        Self::Text(Bytes::from_static(s.as_bytes()))
    }

    #[inline]
    pub fn binary_static(b: &'static [u8]) -> Self {
        Self::Binary(Bytes::from_static(b))
    }

    #[inline]
    pub fn close(code: u16, reason: Bytes) -> Self {
        Self::Close(Some(WsCloseFrame { code, reason }))
    }
}

/// Borrow the underlying bytes from frames without allocation.
#[inline]
pub fn frame_bytes(frame: &WsFrame) -> Option<&[u8]> {
    match frame {
        WsFrame::Text(bytes) => Some(bytes.as_ref()),
        WsFrame::Binary(bytes) => Some(bytes.as_ref()),
        WsFrame::Ping(bytes) => Some(bytes.as_ref()),
        WsFrame::Pong(bytes) => Some(bytes.as_ref()),
        WsFrame::Close(_) => None,
    }
}

/// Convert owned bytes into a `WsFrame`, preferring text when bytes are valid UTF-8.
#[inline]
pub fn into_ws_frame<B>(bytes: B) -> WsFrame
where
    B: Into<Bytes>,
{
    let payload = bytes.into();
    if std::str::from_utf8(payload.as_ref()).is_ok() {
        WsFrame::Text(payload)
    } else {
        WsFrame::Binary(payload)
    }
}
