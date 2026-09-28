//! Bearer-token auth. Each client gets its own token; the tokens file stores
//! only SHA-256 hashes, one client per line: `<sha256-hex> <source>`.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use anyhow::{Context, Result, bail};
use axum::{
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use sha2::{Digest, Sha256};

/// The authenticated caller, attached to each request's extensions.
#[derive(Debug, Clone)]
pub struct Client {
    pub source: String,
}

/// The tokens file, loaded at startup and reloadable (on SIGHUP) without a
/// restart, so adding a client doesn't interrupt the others.
#[derive(Clone)]
pub struct Tokens {
    path: PathBuf,
    map: Arc<RwLock<HashMap<String, Client>>>,
}

impl Tokens {
    pub fn load(path: &Path) -> Result<Self> {
        Ok(Self {
            path: path.to_path_buf(),
            map: Arc::new(RwLock::new(parse(path)?)),
        })
    }

    /// Re-reads the tokens file. On error the current tokens stay in effect.
    pub fn reload(&self) -> Result<usize> {
        let map = parse(&self.path)?;
        let n = map.len();
        *self.map.write().unwrap() = map;
        Ok(n)
    }

    fn lookup(&self, token: &str) -> Option<Client> {
        self.map.read().unwrap().get(&hash(token)).cloned()
    }
}

fn parse(path: &Path) -> Result<HashMap<String, Client>> {
    let contents =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut map = HashMap::new();
    for (n, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [hash, source] = fields[..] else {
            bail!("{}:{}: expected `<sha256> <source>`", path.display(), n + 1);
        };
        let client = Client {
            source: source.to_string(),
        };
        if map.insert(hash.to_lowercase(), client).is_some() {
            bail!("{}:{}: duplicate token hash", path.display(), n + 1);
        }
    }
    Ok(map)
}

pub fn hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub fn generate() -> String {
    let bytes: [u8; 32] = rand::random();
    format!("wa_{}", hex::encode(bytes))
}

pub async fn middleware(State(tokens): State<Tokens>, mut req: Request, next: Next) -> Response {
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match token.and_then(|t| tokens.lookup(t)) {
        Some(client) => {
            req.extensions_mut().insert(client);
            next.run(req).await
        }
        None => {
            tracing::warn!(
                token_present = token.is_some(),
                "rejected unauthenticated request"
            );
            (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Bearer")],
                "missing or invalid bearer token\n",
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reload_picks_up_new_tokens_and_keeps_old_ones_on_error() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("whatsapp-auth-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("tokens");
        std::fs::write(&path, format!("{} first\n", hash("t1")))?;
        let tokens = Tokens::load(&path)?;
        assert_eq!(tokens.lookup("t1").unwrap().source, "first");
        assert!(tokens.lookup("t2").is_none());

        std::fs::write(
            &path,
            format!("{} first\n{} second\n", hash("t1"), hash("t2")),
        )?;
        assert_eq!(tokens.reload()?, 2);
        assert_eq!(tokens.lookup("t2").unwrap().source, "second");

        // A broken file is rejected and the previous tokens stay in effect.
        std::fs::write(&path, "not a valid line\n")?;
        assert!(tokens.reload().is_err());
        assert!(tokens.lookup("t2").is_some());
        Ok(())
    }
}
