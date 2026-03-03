use std::future::Future;

use crate::core::WebSocketError;

/// Fire-and-forget sink for websocket control/data commands.
pub trait WsCommandSink<M>: Send + Sync + 'static {
    fn tell(&self, msg: M) -> Result<(), WebSocketError>;
}

/// Request/reply sink for websocket queries.
pub trait WsRequestSink<Req, Res>: Send + Sync + 'static {
    fn ask(&self, req: Req) -> impl Future<Output = Result<Res, WebSocketError>> + Send;
}
