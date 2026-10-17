//! Async INDI client: one TCP connection to an `indiserver`.
//!
//! The client owns a reader task (XML → [`Cache`] + [`Event`]s) and a writer
//! task (queued outgoing XML). All public methods are synchronous and cheap,
//! so they can be called directly from a GUI thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard};

use quick_xml::Reader;
use quick_xml::escape::escape;
use quick_xml::events::Event as XmlEvent;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, ToSocketAddrs};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use crate::cache::{Cache, Change};
use crate::parser::{IndiMessage, Parser, SetValue};
use crate::types::PropertyKind;

/// Notifications emitted by the client. The current property values are
/// always available from [`Client::cache`]; events only say what changed.
#[derive(Debug, Clone)]
pub enum Event {
    Defined {
        device: String,
        name: String,
    },
    Updated {
        device: String,
        name: String,
    },
    Deleted {
        device: String,
        name: Option<String>,
    },
    Message {
        device: Option<String>,
        timestamp: Option<String>,
        text: String,
    },
    Blob {
        device: String,
        property: String,
        element: String,
        format: String,
        data: Arc<Vec<u8>>,
    },
    /// The connection ended; `Some` carries the error that caused it.
    Disconnected(Option<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobMode {
    Never,
    Also,
    Only,
}

impl BlobMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Never => "Never",
            Self::Also => "Also",
            Self::Only => "Only",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("connection failed: {0}")]
    Connect(#[from] std::io::Error),
    #[error("not connected")]
    Disconnected,
}

type Waker = Arc<dyn Fn() + Send + Sync>;

pub struct Client {
    out: mpsc::UnboundedSender<String>,
    cache: Arc<RwLock<Cache>>,
    connected: Arc<AtomicBool>,
    tasks: [AbortHandle; 2],
}

impl Client {
    /// Connect and request all properties. Events are delivered on the
    /// returned receiver.
    pub async fn connect(
        addr: impl ToSocketAddrs,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Event>), ClientError> {
        Self::connect_with_waker(addr, Arc::new(|| {})).await
    }

    /// Like [`connect`](Self::connect), but `waker` is called after every
    /// event is queued (e.g. to request a GUI repaint).
    pub async fn connect_with_waker(
        addr: impl ToSocketAddrs,
        waker: Waker,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Event>), ClientError> {
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        let (rd, mut wr) = stream.into_split();

        let cache = Arc::new(RwLock::new(Cache::default()));
        let connected = Arc::new(AtomicBool::new(true));
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();

        let reader = tokio::spawn(read_loop(
            BufReader::with_capacity(1 << 20, rd),
            cache.clone(),
            connected.clone(),
            event_tx.clone(),
            waker.clone(),
        ));

        let writer_connected = connected.clone();
        let writer = tokio::spawn(async move {
            while let Some(xml) = out_rx.recv().await {
                if let Err(e) = wr.write_all(xml.as_bytes()).await {
                    if writer_connected.swap(false, Ordering::SeqCst) {
                        let _ = event_tx.send(Event::Disconnected(Some(e.to_string())));
                        waker();
                    }
                    break;
                }
            }
        });

        let client = Client {
            out: out_tx,
            cache,
            connected,
            tasks: [reader.abort_handle(), writer.abort_handle()],
        };
        client.get_properties(None, None)?;
        Ok((client, event_rx))
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// Read access to the property mirror. Keep the guard short-lived.
    pub fn cache(&self) -> RwLockReadGuard<'_, Cache> {
        self.cache.read().unwrap_or_else(|e| e.into_inner())
    }

    pub fn shared_cache(&self) -> Arc<RwLock<Cache>> {
        self.cache.clone()
    }

    pub fn disconnect(&self) {
        self.connected.store(false, Ordering::SeqCst);
        for t in &self.tasks {
            t.abort();
        }
    }

    fn send(&self, xml: String) -> Result<(), ClientError> {
        if !self.is_connected() {
            return Err(ClientError::Disconnected);
        }
        self.out.send(xml).map_err(|_| ClientError::Disconnected)
    }

    pub fn get_properties(&self, device: Option<&str>, name: Option<&str>) -> Result<(), ClientError> {
        let mut xml = String::from(r#"<getProperties version="1.7""#);
        if let Some(d) = device {
            xml += &format!(r#" device="{}""#, escape(d));
        }
        if let Some(n) = name {
            xml += &format!(r#" name="{}""#, escape(n));
        }
        xml += "/>\n";
        self.send(xml)
    }

    pub fn enable_blob(&self, device: &str, name: Option<&str>, mode: BlobMode) -> Result<(), ClientError> {
        let name_attr = name
            .map(|n| format!(r#" name="{}""#, escape(n)))
            .unwrap_or_default();
        self.send(format!(
            "<enableBLOB device=\"{}\"{name_attr}>{}</enableBLOB>\n",
            escape(device),
            mode.as_str()
        ))
    }

    pub fn send_text(&self, device: &str, name: &str, values: &[(&str, &str)]) -> Result<(), ClientError> {
        self.send(new_vector(
            PropertyKind::Text,
            device,
            name,
            values.iter().map(|(e, v)| (*e, escape(*v).into_owned())),
        ))
    }

    pub fn send_number(&self, device: &str, name: &str, values: &[(&str, f64)]) -> Result<(), ClientError> {
        self.send(new_vector(
            PropertyKind::Number,
            device,
            name,
            values.iter().map(|(e, v)| (*e, v.to_string())),
        ))
    }

    pub fn send_switch(&self, device: &str, name: &str, values: &[(&str, bool)]) -> Result<(), ClientError> {
        self.send(new_vector(
            PropertyKind::Switch,
            device,
            name,
            values
                .iter()
                .map(|(e, v)| (*e, if *v { "On" } else { "Off" }.to_owned())),
        ))
    }

    pub fn connect_device(&self, device: &str) -> Result<(), ClientError> {
        self.send_switch(device, "CONNECTION", &[("CONNECT", true), ("DISCONNECT", false)])
    }

    pub fn disconnect_device(&self, device: &str) -> Result<(), ClientError> {
        self.send_switch(device, "CONNECTION", &[("CONNECT", false), ("DISCONNECT", true)])
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.disconnect();
    }
}

/// `values` must already be XML-escaped.
fn new_vector<'a>(
    kind: PropertyKind,
    device: &str,
    name: &str,
    values: impl Iterator<Item = (&'a str, String)>,
) -> String {
    let tag = kind.tag();
    let mut xml = format!(
        "<new{tag}Vector device=\"{}\" name=\"{}\">\n",
        escape(device),
        escape(name)
    );
    for (el, v) in values {
        xml += &format!("  <one{tag} name=\"{}\">{v}</one{tag}>\n", escape(el));
    }
    xml += &format!("</new{tag}Vector>\n");
    xml
}

async fn read_loop(
    rd: BufReader<tokio::net::tcp::OwnedReadHalf>,
    cache: Arc<RwLock<Cache>>,
    connected: Arc<AtomicBool>,
    events: mpsc::UnboundedSender<Event>,
    waker: Waker,
) {
    let mut reader = Reader::from_reader(rd);
    let mut parser = Parser::new();
    let mut buf = Vec::new();
    let reason = loop {
        buf.clear();
        match reader.read_event_into_async(&mut buf).await {
            Ok(XmlEvent::Eof) => break None,
            Ok(ev) => match parser.feed(ev) {
                Some(Ok(msg)) => {
                    dispatch(msg, &cache, &events);
                    waker();
                }
                Some(Err(e)) => tracing::warn!("INDI: {e}"),
                None => {}
            },
            Err(e) => break Some(e.to_string()),
        }
    };
    if connected.swap(false, Ordering::SeqCst) {
        let _ = events.send(Event::Disconnected(reason));
        waker();
    }
}

fn dispatch(msg: IndiMessage, cache: &RwLock<Cache>, events: &mpsc::UnboundedSender<Event>) {
    let change = cache.write().unwrap_or_else(|e| e.into_inner()).apply(&msg);
    let send = |e| {
        let _ = events.send(e);
    };
    match msg {
        IndiMessage::Def { property, message } => {
            if let Some(text) = message.filter(|m| !m.is_empty()) {
                send(Event::Message {
                    device: Some(property.device),
                    timestamp: property.timestamp,
                    text,
                });
            }
        }
        IndiMessage::Set(set) => {
            if let Some(text) = set.message.filter(|m| !m.is_empty()) {
                send(Event::Message {
                    device: Some(set.device.clone()),
                    timestamp: set.timestamp.clone(),
                    text,
                });
            }
            for el in set.elements {
                if let SetValue::Blob(blob) = el.value {
                    send(Event::Blob {
                        device: set.device.clone(),
                        property: set.name.clone(),
                        element: el.name,
                        format: blob.format,
                        data: blob.data,
                    });
                }
            }
        }
        IndiMessage::Del {
            device,
            timestamp,
            message,
            ..
        } => {
            if let Some(text) = message.filter(|m| !m.is_empty()) {
                send(Event::Message {
                    device: Some(device),
                    timestamp,
                    text,
                });
            }
        }
        IndiMessage::Message {
            device,
            timestamp,
            message,
        } => send(Event::Message {
            device,
            timestamp,
            text: message,
        }),
    }
    match change {
        Change::Defined { device, name } => send(Event::Defined { device, name }),
        Change::Updated { device, name } => send(Event::Updated { device, name }),
        Change::Deleted { device, name } => send(Event::Deleted { device, name }),
        Change::None => {}
    }
}
