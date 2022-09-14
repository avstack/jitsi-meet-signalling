use anyhow::{Context, Result};
use xmpp_parsers::{BareJid, FullJid, Jid};

pub(crate) trait JidEndpointIdExt {
  fn endpoint_id(&self) -> Result<&str>;
}

impl JidEndpointIdExt for Jid {
  fn endpoint_id(&self) -> Result<&str> {
    match self {
      Jid::Bare(bare_jid) => bare_jid.endpoint_id(),
      Jid::Full(full_jid) => full_jid.endpoint_id(),
    }
  }
}

impl JidEndpointIdExt for FullJid {
  fn endpoint_id(&self) -> Result<&str> {
    endpoint_id_from_node(self.node.as_ref().context("invalid JID: no node")?)
  }
}

impl JidEndpointIdExt for BareJid {
  fn endpoint_id(&self) -> Result<&str> {
    endpoint_id_from_node(self.node.as_ref().context("invalid JID: no node")?)
  }
}

fn endpoint_id_from_node(node: &str) -> Result<&str> {
  let (before, _after) = node
    .split_once('-')
    .context("invalid JID: invalid node format")?;
  Ok(before)
}
