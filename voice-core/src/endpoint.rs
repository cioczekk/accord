use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use iroh::{Endpoint, SecretKey};
use rand_core::OsRng;

use crate::error::{Result, VoiceError};
use crate::ticket::CALL_ALPN;

pub struct EndpointInfo {
    pub endpoint: Endpoint,
    pub node_id: String,
    pub config_dir: PathBuf,
}

pub fn default_config_dir() -> Result<PathBuf> {
    let project = directories::ProjectDirs::from("com", "Accord", "Accord")
        .ok_or_else(|| VoiceError::Other(anyhow!("Could not determine config directory")))?;
    Ok(project.config_dir().to_path_buf())
}

fn identity_path(config_dir: &Path) -> PathBuf {
    config_dir.join("identity.key")
}

fn load_or_create_secret_key(config_dir: &Path) -> Result<SecretKey> {
    let key_path = identity_path(config_dir);
    if key_path.exists() {
        let bytes = std::fs::read(&key_path)?;
        if bytes.len() != 32 {
            return Err(VoiceError::Other(anyhow!("Invalid secret key length")));
        }
        let mut key_bytes = [0u8; 32];
        key_bytes.copy_from_slice(&bytes);
        Ok(SecretKey::from_bytes(&key_bytes))
    } else {
        std::fs::create_dir_all(config_dir)?;
        let secret = SecretKey::generate(OsRng);
        let bytes = secret.to_bytes();
        std::fs::write(&key_path, bytes)?;
        Ok(secret)
    }
}

pub async fn init_endpoint(config_dir: &Path) -> Result<EndpointInfo> {
    let secret = load_or_create_secret_key(config_dir)?;
    let endpoint = Endpoint::builder()
        .secret_key(secret)
        .alpns(vec![CALL_ALPN.to_vec()])
        .bind()
        .await
        .context("Failed to bind iroh endpoint")
        .map_err(VoiceError::Other)?;

    let node_id = endpoint.node_id().to_string();

    Ok(EndpointInfo {
        endpoint,
        node_id,
        config_dir: config_dir.to_path_buf(),
    })
}
