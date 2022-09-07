use anyhow::Result;
use colibri::ColibriMessage;
use jitsi_jingle_sdp::SessionDescription;

use crate::conference::{Conference, Participant};

#[async_trait::async_trait]
pub trait Agent {
  async fn participant_joined(
    &self,
    conference: Conference,
    participant: Participant,
  ) -> Result<()>;
  async fn participant_left(&self, conference: Conference, participant: Participant) -> Result<()>;
  async fn colibri_message_received(
    &self,
    conference: Conference,
    message: ColibriMessage,
  ) -> Result<()>;
  async fn offer_received(&self, conference: Conference, offer: SessionDescription) -> Result<()>;
  async fn source_added(&self, conference: Conference, offer: SessionDescription) -> Result<()>;
}
