use bytes::Bytes;

/// UTF-8 validated text payload.
///
/// This is a zero-cost wrapper around `Bytes` that carries the invariant that the payload is
/// valid UTF-8. It enables downstream parsing to safely skip redundant UTF-8 validation.
#[repr(transparent)]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WsText(Bytes);

impl WsText {
    #[inline]
    pub fn from_static(s: &'static str) -> Self {
        Self(Bytes::from_static(s.as_bytes()))
    }

    #[inline]
    pub fn try_from_bytes(bytes: Bytes) -> Result<Self, std::str::Utf8Error> {
        std::str::from_utf8(bytes.as_ref())?;
        Ok(Self(bytes))
    }

    /// # Safety
    /// `bytes` must be valid UTF-8.
    #[inline]
    pub unsafe fn from_bytes_unchecked(bytes: Bytes) -> Self {
        Self(bytes)
    }

    #[inline]
    pub fn as_str(&self) -> &str {
        // SAFETY: invariant upheld by constructors.
        unsafe { std::str::from_utf8_unchecked(self.0.as_ref()) }
    }

    #[inline]
    pub fn as_bytes(&self) -> &Bytes {
        &self.0
    }

    #[inline]
    pub fn into_bytes(self) -> Bytes {
        self.0
    }
}

/// Transport-neutral websocket frame type.
///
/// This is the public "wire" surface for the library: transports convert their native frame
/// representation into/from `WsFrame`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WsFrame {
    Text(WsText),
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
        Self::Text(WsText::from_static(s))
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
        WsFrame::Text(text) => Some(text.as_bytes().as_ref()),
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
        // SAFETY: we just validated UTF-8.
        WsFrame::Text(unsafe { WsText::from_bytes_unchecked(payload) })
    } else {
        WsFrame::Binary(payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_text_is_transparent_over_bytes() {
        assert_eq!(std::mem::size_of::<WsText>(), std::mem::size_of::<Bytes>());
        assert_eq!(std::mem::align_of::<WsText>(), std::mem::align_of::<Bytes>());
    }
}
