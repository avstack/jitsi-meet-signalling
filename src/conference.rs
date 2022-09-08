use std::{collections::HashMap, convert::TryFrom, fmt, sync::Arc, time::Duration};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use colibri::{ColibriMessage, JsonMessage};
use futures::stream::StreamExt;
use jitsi_jingle_sdp::{SessionDescription, SessionDescriptionJingleConversionsExt};
use jitsi_xmpp_parsers::jingle::{Action, Jingle, Transport};
use maplit::hashmap;
use once_cell::sync::Lazy;
use serde::Serialize;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, error, info, trace, warn};
use uuid::Uuid;
pub use xmpp_parsers::disco::Feature;
use xmpp_parsers::{
  caps::{self, Caps},
  disco::{DiscoInfoQuery, DiscoInfoResult, Identity},
  ecaps2::{self, ECaps2},
  hashes::{Algo, Hash},
  iq::{Iq, IqType},
  message::{Message, MessageType},
  muc::{user::Status as MucStatus, Muc, MucUser},
  nick::Nick,
  ns,
  presence::{self, Presence},
  stanza_error::{DefinedCondition, ErrorType, StanzaError},
  BareJid, FullJid, Jid,
};

use crate::{
  agent::Agent,
  colibri::ColibriChannel,
  source::MediaType,
  stanza_filter::StanzaFilter,
  util::generate_id,
  xmpp::{self, connection::Connection},
};

const SEND_STATS_INTERVAL: Duration = Duration::from_secs(10);

const DISCO_NODE: &str = "https://github.com/avstack/lib-jitsi-meet-rs";

static DISCO_INFO: Lazy<DiscoInfoResult> = Lazy::new(|| DiscoInfoResult {
  node: None,
  identities: vec![Identity::new("client", "bot", "en", "lib-jitsi-meet-rs")],
  features: vec![
    Feature::new(ns::DISCO_INFO),
    Feature::new(ns::JINGLE_RTP_AUDIO),
    Feature::new(ns::JINGLE_RTP_VIDEO),
    Feature::new(ns::JINGLE_ICE_UDP),
    Feature::new(ns::JINGLE_DTLS),
    Feature::new("urn:ietf:rfc:5888"), // BUNDLE
    Feature::new("urn:ietf:rfc:5761"), // RTCP-MUX
    Feature::new("urn:ietf:rfc:4588"), // RTX
    Feature::new("http://jitsi.org/tcc"),
  ],
  extensions: vec![],
});

static COMPUTED_CAPS_HASH: Lazy<Hash> =
  Lazy::new(|| caps::hash_caps(&caps::compute_disco(&DISCO_INFO), Algo::Sha_1).unwrap());

#[derive(Debug, Clone, Copy)]
enum ConferenceState {
  Discovering,
  JoiningMuc,
  Idle,
}

#[derive(Clone)]
pub struct ConferenceConfig {
  pub muc: BareJid,
  pub focus: Jid,
  pub nick: String,
  pub region: Option<String>,
  pub video_codec: String,
  pub extra_muc_features: Vec<String>,
  pub start_bitrate: u32,
  pub stereo: bool,
  pub agent: Arc<dyn Agent + Send + Sync>,
}

#[derive(Clone)]
pub struct Conference {
  pub(crate) jid: FullJid,
  pub(crate) xmpp_tx: mpsc::Sender<xmpp_parsers::Element>,
  pub(crate) config: ConferenceConfig,
  pub(crate) external_services: Vec<xmpp::extdisco::Service>,
  pub(crate) inner: Arc<Mutex<ConferenceInner>>,
  pub(crate) tls_insecure: bool,
}

impl fmt::Debug for Conference {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("Conference")
      .field("jid", &self.jid)
      .finish()
  }
}

#[derive(Debug, Clone)]
pub struct Participant {
  pub jid: Option<FullJid>,
  pub muc_jid: FullJid,
  pub nick: Option<String>,
}

pub(crate) struct ConferenceInner {
  participants: HashMap<String, Participant>,
  presence: Vec<xmpp_parsers::Element>,
  state: ConferenceState,
  send_resolution: Option<i32>,
  accept_iq_id: Option<String>,
  jingle_session_id: Option<String>,
  remote_description: Option<SessionDescription>,
  colibri_url: Option<String>,
  colibri_channel: Option<ColibriChannel>,
  connected_tx: Option<oneshot::Sender<()>>,
}

impl fmt::Debug for ConferenceInner {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("ConferenceInner")
      .field("state", &self.state)
      .finish()
  }
}

impl Conference {
  #[tracing::instrument(level = "debug", skip(config), err)]
  pub(crate) async fn new(xmpp_connection: Connection, config: ConferenceConfig) -> Result<Self> {
    let conference_stanza = xmpp::jitsi::Conference {
      machine_uid: Uuid::new_v4().to_string(),
      room: config.muc.to_string(),
      properties: hashmap! {
        "stereo".to_string() => config.stereo.to_string(),
        "startBitrate".to_string() => config.start_bitrate.to_string(),
      },
    };

    let (tx, rx) = oneshot::channel();

    let focus = config.focus.clone();

    let ecaps2_hash = ecaps2::hash_ecaps2(&ecaps2::compute_disco(&DISCO_INFO)?, Algo::Sha_256)?;
    let mut presence = vec![
      Muc::new().into(),
      Caps::new(DISCO_NODE, COMPUTED_CAPS_HASH.clone()).into(),
      ECaps2::new(vec![ecaps2_hash]).into(),
      xmpp_parsers::Element::builder("stats-id", ns::DEFAULT_NS)
        .append("lib-jitsi-meet-rs")
        .build(),
      xmpp_parsers::Element::builder("jitsi_participant_codecType", ns::DEFAULT_NS)
        .append(config.video_codec.as_str())
        .build(),
      xmpp_parsers::Element::builder("audiomuted", ns::DEFAULT_NS)
        .append("false")
        .build(),
      xmpp_parsers::Element::builder("videomuted", ns::DEFAULT_NS)
        .append("false")
        .build(),
      xmpp_parsers::Element::builder("nick", "http://jabber.org/protocol/nick")
        .append(config.nick.as_str())
        .build(),
    ];
    if let Some(region) = &config.region {
      presence.extend([
        xmpp_parsers::Element::builder("jitsi_participant_region", ns::DEFAULT_NS)
          .append(region.as_str())
          .build(),
        xmpp_parsers::Element::builder("region", "http://jitsi.org/jitsi-meet")
          .attr("id", region)
          .build(),
      ]);
    }
    presence.extend(
      config
        .extra_muc_features
        .iter()
        .cloned()
        .map(|var| Feature { var })
        .map(|feature| feature.into()),
    );

    let conference = Self {
      jid: xmpp_connection
        .jid()
        .await
        .context("not connected (no JID)")?,
      xmpp_tx: xmpp_connection.tx.clone(),
      config,
      external_services: xmpp_connection.external_services().await,
      inner: Arc::new(Mutex::new(ConferenceInner {
        state: ConferenceState::Discovering,
        presence,
        participants: HashMap::new(),
        send_resolution: None,
        accept_iq_id: None,
        jingle_session_id: None,
        remote_description: None,
        colibri_url: None,
        colibri_channel: None,
        connected_tx: Some(tx),
      })),
      tls_insecure: xmpp_connection.tls_insecure,
    };

    xmpp_connection.add_stanza_filter(conference.clone()).await;

    let iq = Iq::from_set(generate_id(), conference_stanza).with_to(focus);
    xmpp_connection.tx.send(iq.into()).await?;

    rx.await?;

    Ok(conference)
  }

  pub async fn accept(&self, sdp: SessionDescription) -> Result<()> {
    let session_id = self
      .inner
      .lock()
      .await
      .jingle_session_id
      .take()
      .context("no jingle session to accept")?;
    let jingle = sdp.try_to_jingle(
      Action::SessionAccept,
      &session_id,
      &self.focus_jid_in_muc()?.to_string(),
      &self.jid.to_string(),
    )?;
    let accept_iq_id = generate_id();
    self.inner.lock().await.accept_iq_id = Some(accept_iq_id.clone());
    let session_accept_iq = Iq::from_set(accept_iq_id, jingle)
      .with_to(Jid::Full(self.focus_jid_in_muc()?))
      .with_from(Jid::Full(self.jid.clone()));
    self.xmpp_tx.send(session_accept_iq.into()).await?;
    Ok(())
  }

  #[tracing::instrument(level = "debug")]
  pub async fn leave(self) {
    // TODO
  }

  fn endpoint_id(&self) -> Result<&str> {
    self
      .jid
      .node
      .as_ref()
      .ok_or_else(|| anyhow!("invalid jid"))?
      .split('-')
      .next()
      .ok_or_else(|| anyhow!("invalid jid"))
  }

  fn jid_in_muc(&self) -> Result<FullJid> {
    Ok(self.config.muc.clone().with_resource(self.endpoint_id()?))
  }

  pub(crate) fn focus_jid_in_muc(&self) -> Result<FullJid> {
    Ok(self.config.muc.clone().with_resource("focus"))
  }

  #[tracing::instrument(level = "debug", err)]
  async fn send_presence(&self, payloads: &[xmpp_parsers::Element]) -> Result<()> {
    let mut presence = Presence::new(presence::Type::None).with_to(self.jid_in_muc()?);
    presence.payloads = payloads.to_owned();
    self.xmpp_tx.send(presence.into()).await?;
    Ok(())
  }

  #[tracing::instrument(level = "debug", err)]
  pub async fn set_muted(&self, media_type: MediaType, muted: bool) -> Result<()> {
    let mut locked_inner = self.inner.lock().await;
    let element = xmpp_parsers::Element::builder(
      media_type.jitsi_muted_presence_element_name(),
      ns::DEFAULT_NS,
    )
    .append(muted.to_string())
    .build();
    locked_inner
      .presence
      .retain(|el| el.name() != media_type.jitsi_muted_presence_element_name());
    locked_inner.presence.push(element);
    self.send_presence(&locked_inner.presence).await
  }

  /// Set the maximum resolution that we are currently sending.
  pub async fn set_send_resolution(&self, height: i32) {
    self.inner.lock().await.send_resolution = Some(height);
  }

  pub async fn send_colibri_message(&self, message: ColibriMessage) -> Result<()> {
    self
      .inner
      .lock()
      .await
      .colibri_channel
      .as_ref()
      .context("no colibri channel")?
      .send(message)
      .await
  }

  pub async fn send_json_message<T: Serialize>(&self, payload: &T) -> Result<()> {
    let message = Message {
      from: Some(Jid::Full(self.jid.clone())),
      to: Some(Jid::Bare(self.config.muc.clone())),
      id: Some(Uuid::new_v4().to_string()),
      type_: MessageType::Groupchat,
      bodies: Default::default(),
      subjects: Default::default(),
      thread: None,
      payloads: vec![xmpp_parsers::Element::try_from(xmpp::jitsi::JsonMessage {
        payload: serde_json::to_value(payload)?,
      })?],
    };
    self.xmpp_tx.send(message.into()).await?;
    Ok(())
  }
}

#[async_trait]
impl StanzaFilter for Conference {
  #[tracing::instrument(level = "trace")]
  fn filter(&self, element: &xmpp_parsers::Element) -> bool {
    element.attr("from") == Some(self.config.focus.to_string().as_str())
      && element.is("iq", ns::DEFAULT_NS)
      || element
        .attr("from")
        .and_then(|from| from.parse::<BareJid>().ok())
        .map(|jid| jid == self.config.muc)
        .unwrap_or_default()
        && (element.is("presence", ns::DEFAULT_NS) || element.is("iq", ns::DEFAULT_NS))
  }

  #[tracing::instrument(level = "trace", err)]
  async fn take(&self, element: xmpp_parsers::Element) -> Result<()> {
    use ConferenceState::*;
    let state = self.inner.lock().await.state;
    match state {
      Discovering => {
        let iq = Iq::try_from(element)?;
        if let IqType::Result(Some(element)) = iq.payload {
          let ready: bool = element
            .attr("ready")
            .ok_or_else(|| anyhow!("missing ready attribute on conference IQ"))?
            .parse()?;
          if !ready {
            bail!("focus reports room not ready");
          }
        }
        else {
          bail!("focus IQ failed");
        };

        let mut locked_inner = self.inner.lock().await;
        self.send_presence(&locked_inner.presence).await?;
        locked_inner.state = JoiningMuc;
      },
      JoiningMuc => {
        let presence = Presence::try_from(element)?;
        if let Some(payload) = presence
          .payloads
          .iter()
          .find(|payload| payload.is("x", ns::MUC_USER))
        {
          let muc_user = MucUser::try_from(payload.clone())?;
          if muc_user.status.contains(&MucStatus::SelfPresence) {
            debug!("Joined MUC: {}", self.config.muc);
            if let Some(connected_tx) = self.inner.lock().await.connected_tx.take() {
              connected_tx.send(()).unwrap();
            }
            self.inner.lock().await.state = Idle;
          }
        }
      },
      Idle => {
        if let Ok(iq) = Iq::try_from(element.clone()) {
          match iq.payload {
            IqType::Get(element) => {
              if let Ok(query) = DiscoInfoQuery::try_from(element) {
                debug!(
                  "Received disco info query from {} for node {:?}",
                  iq.from.as_ref().unwrap(),
                  query.node
                );
                if let Some(node) = query.node {
                  match node.splitn(2, '#').collect::<Vec<_>>().as_slice() {
                    // TODO: also support ecaps2, as we send it in our presence.
                    [uri, hash]
                      if *uri == DISCO_NODE && *hash == COMPUTED_CAPS_HASH.to_base64() =>
                    {
                      let mut disco_info = DISCO_INFO.clone();
                      disco_info.node = Some(node);
                      let iq = Iq::from_result(iq.id, Some(disco_info))
                        .with_from(Jid::Full(self.jid.clone()))
                        .with_to(iq.from.unwrap());
                      self.xmpp_tx.send(iq.into()).await?;
                    },
                    _ => {
                      let error = StanzaError::new(
                        ErrorType::Cancel,
                        DefinedCondition::ItemNotFound,
                        "en",
                        format!("Unknown disco#info node: {}", node),
                      );
                      let iq = Iq::from_error(iq.id, error)
                        .with_from(Jid::Full(self.jid.clone()))
                        .with_to(iq.from.unwrap());
                      self.xmpp_tx.send(iq.into()).await?;
                    },
                  }
                }
                else {
                  let iq = Iq::from_result(iq.id, Some(DISCO_INFO.clone()))
                    .with_from(Jid::Full(self.jid.clone()))
                    .with_to(iq.from.unwrap());
                  self.xmpp_tx.send(iq.into()).await?;
                }
              }
            },
            IqType::Set(element) => match Jingle::try_from(element) {
              Ok(jingle) => {
                if let Some(Jid::Full(from_jid)) = iq.from {
                  if jingle.action == Action::SessionInitiate {
                    if from_jid.resource == "focus" {
                      // Acknowledge the IQ
                      let result_iq = Iq::empty_result(Jid::Full(from_jid.clone()), iq.id.clone())
                        .with_from(Jid::Full(self.jid.clone()));
                      self.xmpp_tx.send(result_iq.into()).await?;

                      {
                        let mut locked_inner = self.inner.lock().await;
                        locked_inner.jingle_session_id = Some(jingle.sid.0.clone());
                        if let Some(Transport::IceUdp(ice_transport)) =
                          jingle.contents.first().and_then(|c| c.transport.as_ref())
                        {
                          locked_inner.colibri_url =
                            ice_transport.web_socket.clone().map(|ws| ws.url);
                        }
                      };

                      let offer_sdp = SessionDescription::try_from_jingle(&jingle)?;

                      match self
                        .config
                        .agent
                        .offer_received(self.clone(), offer_sdp.clone(), true)
                        .await
                      {
                        Ok(_) => {
                          self.inner.lock().await.remote_description = Some(offer_sdp);
                          debug!("Saved new remote description");
                        },
                        Err(e) => error!("agent offer_received failed: {:?}", e),
                      }
                    }
                    else {
                      debug!(
                        "Ignored Jingle session-initiate from {} (P2P not supported)",
                        from_jid
                      );
                    }
                  }
                  else if jingle.action == Action::SessionTerminate {
                    if from_jid.resource == "focus" {
                      debug!("Received Jingle session-terminate");

                      // Acknowledge the IQ
                      let result_iq = Iq::empty_result(Jid::Full(from_jid.clone()), iq.id.clone())
                        .with_from(Jid::Full(self.jid.clone()));
                      self.xmpp_tx.send(result_iq.into()).await?;

                      let mut locked_inner = self.inner.lock().await;
                      locked_inner.accept_iq_id = None;
                      locked_inner.jingle_session_id = None;
                      locked_inner.remote_description = None;
                      locked_inner.colibri_url = None;
                      if let Some(colibri_channel) = locked_inner.colibri_channel.take() {
                        colibri_channel.stop().await;
                      }

                      if let Err(e) = self.config.agent.session_terminate(self.clone()).await {
                        error!("agent session_terminate failed: {:?}", e);
                      }
                    }
                  }
                  else if jingle.action == Action::SourceAdd {
                    debug!("Received Jingle source-add");

                    // Acknowledge the IQ
                    let result_iq = Iq::empty_result(Jid::Full(from_jid.clone()), iq.id.clone())
                      .with_from(Jid::Full(self.jid.clone()));
                    self.xmpp_tx.send(result_iq.into()).await?;

                    if let Some(offer_sdp) = self.inner.lock().await.remote_description.as_mut() {
                      offer_sdp.add_sources_from_jingle(&jingle)?;
                      if let Err(e) = self
                        .config
                        .agent
                        .offer_received(self.clone(), offer_sdp.clone(), false)
                        .await
                      {
                        error!("agent offer_received failed: {:?}", e);
                      }
                    }
                    else {
                      error!("Received source-add with no saved remote description");
                    }
                  }
                  else if jingle.action == Action::SourceRemove {
                    debug!("Received Jingle source-remove");

                    // Acknowledge the IQ
                    let result_iq = Iq::empty_result(Jid::Full(from_jid.clone()), iq.id.clone())
                      .with_from(Jid::Full(self.jid.clone()));
                    self.xmpp_tx.send(result_iq.into()).await?;

                    if let Some(offer_sdp) = self.inner.lock().await.remote_description.as_mut() {
                      offer_sdp.remove_sources_from_jingle(&jingle)?;
                      if let Err(e) = self
                        .config
                        .agent
                        .offer_received(self.clone(), offer_sdp.clone(), false)
                        .await
                      {
                        error!("agent offer_received failed: {:?}", e);
                      }
                    }
                    else {
                      error!("Received source-add with no saved remote description");
                    }
                  }
                  else {
                    warn!("Received unhandled Jingle action: {:?}", jingle.action);
                  }
                }
                else {
                  debug!("Received Jingle IQ from invalid JID: {:?}", iq.from);
                }
              },
              Err(e) => debug!("IQ did not successfully parse as Jingle: {:?}", e),
            },
            IqType::Result(_) => {
              let maybe_colibri_url = {
                let mut locked_inner = self.inner.lock().await;
                if locked_inner.accept_iq_id.as_ref() == Some(&iq.id) {
                  locked_inner.accept_iq_id = None;
                  Some(locked_inner.colibri_url.clone())
                }
                else {
                  error!(
                    "focus acknowledged a session-accept that we didn't send: {}",
                    iq.id
                  );
                  None
                }
              };
              if let Some(maybe_colibri_url) = maybe_colibri_url {
                debug!("Focus acknowledged session-accept");

                if let Some(colibri_url) = maybe_colibri_url {
                  info!("Connecting Colibri WebSocket to {}", colibri_url);
                  let colibri_channel =
                    ColibriChannel::new(&colibri_url, self.tls_insecure).await?;
                  let (tx, rx) = mpsc::channel(8);
                  colibri_channel.subscribe(tx).await;
                  self.inner.lock().await.colibri_channel = Some(colibri_channel.clone());

                  let my_endpoint_id = self.endpoint_id()?.to_owned();

                  {
                    let self_ = self.clone();
                    tokio::spawn(async move {
                      let mut stream = ReceiverStream::new(rx);
                      while let Some(msg) = stream.next().await {
                        // Some message types are handled internally rather than passed to the on_colibri_message handler.
                        let handled = match &msg {
                          ColibriMessage::EndpointMessage {
                            to: Some(to),
                            from,
                            msg_payload,
                          } if to == &my_endpoint_id => {
                            match serde_json::from_value::<JsonMessage>(msg_payload.clone()) {
                              Ok(JsonMessage::E2ePingRequest { id }) => {
                                if let Err(e) = colibri_channel
                                  .send(ColibriMessage::EndpointMessage {
                                    from: None,
                                    to: from.clone(),
                                    msg_payload: serde_json::to_value(
                                      JsonMessage::E2ePingResponse { id },
                                    )
                                    .unwrap(),
                                  })
                                  .await
                                {
                                  warn!("failed to send e2e ping response: {:?}", e);
                                }
                                true
                              },
                              _ => false,
                            }
                          },
                          _ => false,
                        };

                        if handled {
                          continue;
                        }

                        if let Err(e) = self_
                          .config
                          .agent
                          .colibri_message_received(self_.clone(), msg)
                          .await
                        {
                          error!("agent colibri_message_received failed: {:?}", e);
                        }
                      }
                      Ok::<_, anyhow::Error>(())
                    });
                  }
                }
                else {
                  warn!("No Colibri websocket URL");
                }
              }
            },
            _ => {},
          }
        }
        else if let Ok(presence) = Presence::try_from(element) {
          if let Jid::Full(from) = presence
            .from
            .as_ref()
            .context("missing from in presence")?
            .clone()
          {
            let bare_from: BareJid = from.clone().into();
            if bare_from == self.config.muc && from.resource != "focus" {
              trace!("received MUC presence from {}", from.resource);
              let nick_payload = presence
                .payloads
                .iter()
                .find(|e| e.is("nick", ns::NICK))
                .map(|e| Nick::try_from(e.clone()))
                .transpose()?;
              if let Some(muc_user_payload) = presence
                .payloads
                .into_iter()
                .find(|e| e.is("x", ns::MUC_USER))
              {
                let muc_user = MucUser::try_from(muc_user_payload)?;
                for item in muc_user.items {
                  if let Some(jid) = &item.jid {
                    if jid == &self.jid {
                      continue;
                    }
                    let participant = Participant {
                      jid: Some(jid.clone()),
                      muc_jid: from.clone(),
                      nick: item
                        .nick
                        .or_else(|| nick_payload.as_ref().map(|nick| nick.0.clone())),
                    };
                    if presence.type_ == presence::Type::Unavailable {
                      if self
                        .inner
                        .lock()
                        .await
                        .participants
                        .remove(&from.resource.clone())
                        .is_some()
                      {
                        debug!("participant left: {:?}", jid);
                        if let Err(e) = self
                          .config
                          .agent
                          .participant_left(self.clone(), participant)
                          .await
                        {
                          error!("agent participant_left failed: {:?}", e);
                        }
                      }
                    }
                    else if self
                      .inner
                      .lock()
                      .await
                      .participants
                      .insert(from.resource.clone(), participant.clone())
                      .is_none()
                    {
                      debug!("new participant: {:?}", jid);
                      if let Err(e) = self
                        .config
                        .agent
                        .participant_joined(self.clone(), participant)
                        .await
                      {
                        error!("agent participant_joined failed: {:?}", e);
                      }
                    }
                  }
                }
              }
            }
          }
        }
      },
    }
    Ok(())
  }
}
