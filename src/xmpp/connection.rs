use std::{convert::TryFrom, fmt, sync::Arc};

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::{Engine, general_purpose::STANDARD_NO_PAD as BASE64};
use futures::{
  sink::{Sink, SinkExt},
  stream::{Stream, StreamExt, TryStreamExt},
};
use rand::{thread_rng, RngCore};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_stream::wrappers::ReceiverStream;
use tokio_tungstenite::tungstenite::{
  http::{Request, Uri},
  Message,
};
use tracing::{debug, error, info, warn};
use xmpp_parsers::{
  bind::{BindQuery, BindResponse},
  disco::{DiscoInfoQuery, DiscoInfoResult},
  iq::{Iq, IqType},
  sasl::{Auth, Mechanism, Success},
  websocket::Open,
  BareJid, Element, FullJid, Jid,
};

use crate::{
  agent::Agent,
  conference::{Conference, ConferenceConfig},
  pinger::Pinger,
  stanza_filter::StanzaFilter,
  tls::wss_connector,
  util::generate_id,
  xmpp,
};

#[derive(Debug, Clone, Copy)]
enum ConnectionState {
  OpeningPreAuthentication,
  ReceivingFeaturesPreAuthentication,
  Authenticating,
  OpeningPostAuthentication,
  ReceivingFeaturesPostAuthentication,
  Binding,
  Discovering,
  DiscoveringExternalServices,
  Idle,
}

struct ConnectionInner {
  state: ConnectionState,
  connected_tx: Option<oneshot::Sender<()>>,
  jid: Option<FullJid>,
  xmpp_domain: BareJid,
  authentication: Authentication,
  external_services: Vec<xmpp::extdisco::Service>,
  stanza_filters: Vec<Box<dyn StanzaFilter + Send + Sync>>,
}

impl fmt::Debug for ConnectionInner {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("ConnectionInner")
      .field("state", &self.state)
      .field("jid", &self.jid)
      .finish()
  }
}

#[derive(Debug, Clone)]
pub struct Connection {
  pub(crate) tx: mpsc::Sender<Element>,
  inner: Arc<Mutex<ConnectionInner>>,
  pub(crate) tls_insecure: bool,
}

pub enum Authentication {
  Anonymous,
  Plain { username: String, password: String },
}

impl Connection {
  pub async fn connect(
    websocket_url: &str,
    xmpp_domain: &str,
    authentication: Authentication,
    tls_insecure: bool,
  ) -> Result<Self> {
    let websocket_url: Uri = websocket_url.parse().context("invalid WebSocket URL")?;
    let xmpp_domain: BareJid = xmpp_domain.parse().context("invalid XMPP domain")?;

    info!("Connecting XMPP WebSocket to {}", websocket_url);
    let mut key = [0u8; 16];
    thread_rng().fill_bytes(&mut key);
    let request = Request::get(&websocket_url)
      .header("sec-websocket-protocol", "xmpp")
      .header("sec-websocket-key", BASE64.encode(key))
      .header("sec-websocket-version", "13")
      .header(
        "host",
        websocket_url
          .host()
          .context("invalid WebSocket URL: missing host")?,
      )
      .header("connection", "Upgrade")
      .header("upgrade", "websocket")
      .body(())
      .context("failed to build WebSocket request")?;

    let (websocket, _response) = tokio_tungstenite::connect_async_tls_with_config(
      request,
      None,
      Some(wss_connector(tls_insecure).context("failed to build TLS connector")?),
    )
    .await
    .context("failed to connect XMPP WebSocket")?;
    let (sink, stream) = websocket.split();
    let (tx, rx) = mpsc::channel(64);

    tx.send(Open::new(xmpp_domain.clone()).into()).await?;

    let (connected_tx, connected_rx) = oneshot::channel();

    let inner = Arc::new(Mutex::new(ConnectionInner {
      state: ConnectionState::OpeningPreAuthentication,
      connected_tx: Some(connected_tx),
      jid: None,
      xmpp_domain,
      authentication,
      external_services: vec![],
      stanza_filters: vec![],
    }));

    let connection = Self {
      tx: tx.clone(),
      inner: inner.clone(),
      tls_insecure,
    };

    let writer = Connection::write_loop(rx, sink);
    let reader = Connection::read_loop(inner, tx, stream);

    tokio::spawn(async move {
      tokio::select! {
        res = reader => if let Err(e) = res { error!("fatal (in read loop): {:?}", e) },
        res = writer => if let Err(e) = res { error!("fatal (in write loop): {:?}", e) },
      }
    });

    connected_rx.await?;

    Ok(connection)
  }

  pub async fn join(
    &self,
    conference_name: &str,
    nick: &str,
    agent: Arc<dyn Agent + Send + Sync>,
  ) -> Result<Conference> {
    let conference_config = {
      let locked_inner = self.inner.lock().await;
      ConferenceConfig {
        muc: format!(
          "{}@conference.{}",
          conference_name.to_lowercase(), locked_inner.xmpp_domain
        )
        .parse()?,
        focus: format!("focus@auth.{}/focus", locked_inner.xmpp_domain).parse()?,
        nick: nick.into(),
        region: None,
        video_codec: "vp9".into(),
        extra_muc_features: vec![],
        start_bitrate: 800,
        stereo: false,
        agent,
      }
    };
    self.join_config(conference_config).await
  }

  pub async fn join_config(&self, conference_config: ConferenceConfig) -> Result<Conference> {
    Conference::new(self.clone(), conference_config).await
  }

  pub(crate) async fn add_stanza_filter(
    &self,
    stanza_filter: impl StanzaFilter + Send + Sync + 'static,
  ) {
    let mut locked_inner = self.inner.lock().await;
    locked_inner.stanza_filters.push(Box::new(stanza_filter));
  }

  pub async fn jid(&self) -> Option<FullJid> {
    let locked_inner = self.inner.lock().await;
    locked_inner.jid.clone()
  }

  pub(crate) async fn external_services(&self) -> Vec<xmpp::extdisco::Service> {
    let locked_inner = self.inner.lock().await;
    locked_inner.external_services.clone()
  }

  async fn write_loop<S>(rx: mpsc::Receiver<Element>, mut sink: S) -> Result<()>
  where
    S: Sink<Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
  {
    let mut rx = ReceiverStream::new(rx);
    while let Some(element) = rx.next().await {
      let mut bytes = Vec::new();
      match element.write_to(&mut bytes) {
        Ok(_) => match String::from_utf8(bytes) {
          Ok(xml) => {
            debug!("XMPP    >>> {}", xml);
            sink.send(Message::Text(xml)).await?;
          },
          Err(e) => {
            bail!("Element serialised to invalid UTF-8. Lossy UTF-8 decode follows:\n{}", String::from_utf8_lossy(e.as_bytes()));
          },
        },
        Err(e) => bail!("Failed to serialise element: {:?}", e),
      }
    }
    Ok(())
  }

  async fn read_loop<S>(
    inner: Arc<Mutex<ConnectionInner>>,
    tx: mpsc::Sender<Element>,
    mut stream: S,
  ) -> Result<()>
  where
    S: Stream<Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
  {
    loop {
      let message = stream
        .try_next()
        .await?
        .ok_or_else(|| anyhow!("unexpected EOF"))?;
      let element: Element = match message {
        Message::Text(xml) => {
          debug!("XMPP    <<< {}", xml);
          xml.parse()?
        },
        _ => {
          warn!(
            "unexpected non-text message on XMPP WebSocket stream: {:?}",
            message
          );
          continue;
        },
      };

      let mut locked_inner = inner.lock().await;

      use ConnectionState::*;
      match locked_inner.state {
        OpeningPreAuthentication => {
          Open::try_from(element)?;
          info!("Connected XMPP WebSocket");
          locked_inner.state = ReceivingFeaturesPreAuthentication;
        },
        ReceivingFeaturesPreAuthentication => {
          let auth = match &locked_inner.authentication {
            Authentication::Anonymous => Auth {
              mechanism: Mechanism::Anonymous,
              data: vec![],
            },
            Authentication::Plain { username, password } => {
              let mut data = Vec::with_capacity(username.len() + password.len() + 2);
              data.push(0u8);
              data.extend_from_slice(username.as_bytes());
              data.push(0u8);
              data.extend_from_slice(password.as_bytes());
              Auth {
                mechanism: Mechanism::Plain,
                data,
              }
            },
          };
          tx.send(auth.into()).await?;
          locked_inner.state = Authenticating;
        },
        Authenticating => {
          Success::try_from(element)?;

          let open = Open::new(locked_inner.xmpp_domain.clone());
          tx.send(open.into()).await?;
          locked_inner.state = OpeningPostAuthentication;
        },
        OpeningPostAuthentication => {
          Open::try_from(element)?;
          match &locked_inner.authentication {
            Authentication::Anonymous => info!("Logged in anonymously"),
            Authentication::Plain { .. } => info!("Logged in with PLAIN"),
          }
          locked_inner.state = ReceivingFeaturesPostAuthentication;
        },
        ReceivingFeaturesPostAuthentication => {
          let iq = Iq::from_set(generate_id(), BindQuery::new(None));
          tx.send(iq.into()).await?;
          locked_inner.state = Binding;
        },
        Binding => match Iq::try_from(element) {
          Ok(iq) => {
            let jid = if let IqType::Result(Some(element)) = iq.payload {
              let bind = BindResponse::try_from(element)?;
              FullJid::try_from(bind)?
            }
            else {
              bail!("bind failed");
            };
            info!("My JID: {}", jid);
            locked_inner.jid = Some(jid.clone());

            let iq = Iq::from_get(generate_id(), DiscoInfoQuery { node: None })
              .with_from(Jid::Full(jid.clone()))
              .with_to(Jid::Bare(locked_inner.xmpp_domain.clone()));
            tx.send(iq.into()).await?;
            locked_inner.state = Discovering;
          },
          Err(e) => debug!(
            "received unexpected element while waiting for bind response: {}",
            e
          ),
        },
        Discovering => {
          let iq = Iq::try_from(element)?;
          if let IqType::Result(Some(element)) = iq.payload {
            let _disco_info = DiscoInfoResult::try_from(element)?;
          }
          else {
            bail!("disco failed");
          }

          let iq = Iq::from_get(generate_id(), xmpp::extdisco::ServicesQuery {})
            .with_from(Jid::Full(
              locked_inner.jid.as_ref().context("missing jid")?.clone(),
            ))
            .with_to(Jid::Bare(locked_inner.xmpp_domain.clone()));
          tx.send(iq.into()).await?;
          locked_inner.state = DiscoveringExternalServices;
        },
        DiscoveringExternalServices => {
          let iq = Iq::try_from(element)?;
          if let IqType::Result(Some(element)) = iq.payload {
            let services = xmpp::extdisco::ServicesResult::try_from(element)?;
            debug!("external services: {:?}", services.services);
            locked_inner.external_services = services.services;
          }
          else {
            warn!("discovering external services failed: STUN/TURN will not work");
          }
          if let Some(tx) = locked_inner.connected_tx.take() {
            tx.send(()).unwrap();
          }

          let jid = locked_inner.jid.as_ref().unwrap().clone();
          locked_inner
            .stanza_filters
            .push(Box::new(Pinger::new(jid, tx.clone())));

          locked_inner.state = Idle;
        },
        Idle => {
          for filter in &locked_inner.stanza_filters {
            if filter.filter(&element) {
              filter.take(element).await?;
              break;
            }
          }
        },
      }
    }
  }
}
