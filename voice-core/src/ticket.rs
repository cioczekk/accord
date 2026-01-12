use std::str::FromStr;

use anyhow::Context;
use iroh::Endpoint;
use iroh::NodeAddr;
use iroh_base::ticket::NodeTicket;

use crate::error::{Result, VoiceError};

pub const CALL_ALPN: &[u8] = iroh_roq::ALPN;

pub async fn create_ticket(endpoint: &Endpoint) -> Result<String> {
    let node_addr = endpoint
        .node_addr()
        .await
        .context("Failed to build node address")
        .map_err(VoiceError::Other)?;
    let ticket = NodeTicket::from(node_addr);
    Ok(ticket.to_string())
}

pub fn parse_ticket(input: &str) -> Result<NodeAddr> {
    let ticket = NodeTicket::from_str(input)
        .map_err(|err| VoiceError::Ticket(err.to_string()))?;
    Ok(ticket.node_addr().clone())
}
