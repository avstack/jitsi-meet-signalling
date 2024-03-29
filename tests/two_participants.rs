use std::sync::Arc;

use anyhow::Result;
use rand::{distributions::Alphanumeric, Rng, thread_rng};
use jitsi_meet_signalling::{
  Agent, Authentication, ColibriMessage, Conference, Connection, Participant, SessionDescription,
};
use tokio::sync::{oneshot, Mutex};
use tracing::info;

struct TestAgent {
  offer_received_tx: Mutex<Option<oneshot::Sender<()>>>,
}

impl TestAgent {
  fn new(offer_received_tx: oneshot::Sender<()>) -> Self {
    Self {
      offer_received_tx: Mutex::new(Some(offer_received_tx)),
    }
  }
}

#[async_trait::async_trait]
impl Agent for TestAgent {
  async fn participant_joined(
    &self,
    _conference: Conference,
    participant: Participant,
  ) -> Result<()> {
    info!("participant joined: {:?}", participant);
    Ok(())
  }

  async fn participant_left(
    &self,
    _conference: Conference,
    participant: Participant,
  ) -> Result<()> {
    info!("participant left: {:?}", participant);
    Ok(())
  }

  async fn colibri_message_received(
    &self,
    _conference: Conference,
    message: ColibriMessage,
  ) -> Result<()> {
    info!("colibri message received: {:?}", message);
    Ok(())
  }

  async fn offer_received(&self, _conference: Conference, offer: SessionDescription, _should_send_answer: bool) -> Result<()> {
    info!("offer received: {:?}", offer);
    if let Some(tx) = self.offer_received_tx.lock().await.take() {
      tx.send(()).unwrap();
    }
    Ok(())
  }

  async fn session_terminate(&self, _conference: Conference) -> Result<()> {
    info!("session terminated");
    Ok(())
  }
}

#[tokio::test]
async fn two_participants() {
  tracing_subscriber::fmt::init();

  let suffix: String = thread_rng().sample_iter(&Alphanumeric).take(8).map(char::from).collect();
  let room_name = format!("jitsi-meet-signalling-test-{}", suffix);

  let websocket_url = format!("wss://meet.avstack.io/avstack/xmpp-websocket?room={}", room_name);
  let xmpp_domain = "avstack.onavstack.net";

  let connection_1 = Connection::connect(
    &websocket_url,
    xmpp_domain,
    Authentication::Anonymous,
    false,
  )
  .await
  .unwrap();

  let (tx, rx_1) = oneshot::channel();
  let agent_1 = TestAgent::new(tx);

  let _conference_1 = connection_1
    .join(&room_name, "rust-1", Arc::new(agent_1))
    .await
    .unwrap();

  let connection_2 = Connection::connect(
    &websocket_url,
    xmpp_domain,
    Authentication::Anonymous,
    false,
  )
  .await
  .unwrap();

  let (tx, rx_2) = oneshot::channel();
  let agent_2 = TestAgent::new(tx);

  let _conference_2 = connection_2
    .join(&room_name, "rust-2", Arc::new(agent_2))
    .await
    .unwrap();

  let (r1, r2) = tokio::join!(rx_1, rx_2);
  r1.unwrap();
  r2.unwrap();
}
