//! WebSocket link to an engine, with automatic reconnection.
//! Works natively and in the browser (via ewebsock).

use eframe::egui;
use ewebsock::{WsEvent, WsMessage, WsReceiver, WsSender};
use protocol::{ToEngine, ToUi, preview_frame};

const RETRY_SECONDS: f64 = 2.0;

#[derive(Debug, Clone, PartialEq)]
pub enum LinkState {
    Connecting,
    Open,
    Closed(String),
}

pub enum Incoming {
    Msg(ToUi),
    /// JPEG preview of main-camera frame `id`.
    Preview { id: u64, jpeg: Vec<u8> },
    /// JPEG preview of a guide-camera frame.
    GuidePreview { jpeg: Vec<u8> },
}

pub struct EngineLink {
    pub url: String,
    pub state: LinkState,
    ctx: egui::Context,
    conn: Option<(WsSender, WsReceiver)>,
    retry_at: Option<f64>,
}

impl EngineLink {
    pub fn new(url: String, ctx: &egui::Context) -> Self {
        let mut link = Self {
            url,
            state: LinkState::Connecting,
            ctx: ctx.clone(),
            conn: None,
            retry_at: None,
        };
        link.open();
        link
    }

    fn open(&mut self) {
        let ctx = self.ctx.clone();
        let wakeup = move || ctx.request_repaint();
        match ewebsock::connect_with_wakeup(protocol::versioned_url(&self.url), Default::default(), wakeup) {
            Ok(conn) => {
                self.conn = Some(conn);
                self.state = LinkState::Connecting;
            }
            Err(e) => {
                self.conn = None;
                self.state = LinkState::Closed(e);
            }
        }
    }

    /// Process incoming events; returns decoded messages in order.
    pub fn poll(&mut self) -> Vec<Incoming> {
        let now = self.ctx.input(|i| i.time);
        let mut out = Vec::new();
        if let Some((_, rx)) = &self.conn {
            while let Some(ev) = rx.try_recv() {
                match ev {
                    WsEvent::Opened => self.state = LinkState::Open,
                    WsEvent::Message(WsMessage::Text(text)) => match ToUi::from_json(&text) {
                        Ok(msg) => out.push(Incoming::Msg(msg)),
                        Err(e) => eprintln!("unreadable engine message: {e}"),
                    },
                    WsEvent::Message(WsMessage::Binary(bytes)) => {
                        match preview_frame::decode(&bytes) {
                            Some((preview_frame::Kind::Main, id, jpeg)) => out.push(Incoming::Preview {
                                id,
                                jpeg: jpeg.to_vec(),
                            }),
                            Some((preview_frame::Kind::Guide, _, jpeg)) => out.push(Incoming::GuidePreview {
                                jpeg: jpeg.to_vec(),
                            }),
                            None => {}
                        }
                    }
                    WsEvent::Message(_) => {}
                    WsEvent::Error(e) => self.state = LinkState::Closed(e),
                    WsEvent::Closed => self.state = LinkState::Closed("connection closed".into()),
                }
            }
        }
        if matches!(self.state, LinkState::Closed(_)) {
            if self.conn.take().is_some() || self.retry_at.is_none() {
                self.retry_at = Some(now + RETRY_SECONDS);
            }
            match self.retry_at {
                Some(t) if now >= t => {
                    self.retry_at = None;
                    self.open();
                }
                Some(t) => self
                    .ctx
                    .request_repaint_after(std::time::Duration::from_secs_f64(t - now)),
                None => {}
            }
        }
        out
    }

    pub fn is_open(&self) -> bool {
        self.state == LinkState::Open
    }

    pub fn send(&mut self, msg: &ToEngine) -> Result<(), String> {
        match (&self.state, &mut self.conn) {
            (LinkState::Open, Some((tx, _))) => {
                tx.send(WsMessage::Text(msg.to_json()));
                Ok(())
            }
            _ => Err("not connected to the engine".into()),
        }
    }
}
