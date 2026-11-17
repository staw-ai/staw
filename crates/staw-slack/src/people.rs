//! People and their Staw credentials.
//!
//! Each Slack account connects its own Staw account once, by the OAuth
//! device grant, through a client registered for that account alone. The
//! refresh token goes to the store; the access token (an hour long) stays
//! in memory and is refreshed before it runs out, or at once when Staw
//! turns it away. A refresh token that Staw no longer knows — revoked in
//! Staw's settings — means the person is not connected any more: the
//! store forgets it, and the next request starts connecting again.
//!
//! A token is only ever used for its own key's request (rule 3); there is
//! no shared token, and nothing here hands one out across keys.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::staw::{self, Oauth, Opened, Poll, RefreshError, Staw};
use crate::store::{self, Key, Person, Store};

/// A cached access token and when it runs out.
#[derive(Clone)]
struct Access {
    token: String,
    expires: Instant,
}

/// A device grant waiting for the person's approval.
#[derive(Debug, Clone)]
pub struct Pending {
    pub link: String,
    pub code: String,
    pub expires: Instant,
}

impl Pending {
    /// Whole minutes left, rounded up, for the connect message.
    pub fn minutes_left(&self) -> u64 {
        self.expires
            .saturating_duration_since(Instant::now())
            .as_secs()
            .div_ceil(60)
    }
}

/// What [`People::begin`] did.
pub enum Begun {
    /// A grant was opened: tell the person, then [`People::wait`].
    Opened {
        client_id: String,
        client: String,
        opened: Opened,
        pending: Pending,
    },
    /// A grant for this person is already open: tell them again.
    Already(Pending),
}

/// How waiting for an approval ended.
pub enum Outcome {
    Connected(Person),
    Denied,
    Expired,
    Failed,
}

pub struct People {
    store: Store,
    staw: Staw,
    oauth: Oauth,
    access: Mutex<HashMap<Key, Access>>,
    pending: tokio::sync::Mutex<HashMap<Key, Pending>>,
}

impl People {
    pub fn new(store: Store, staw: Staw, oauth: Oauth) -> Self {
        Self {
            store,
            staw,
            oauth,
            access: Mutex::new(HashMap::new()),
            pending: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// The store of refresh tokens.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// An access token for `key`, valid for at least `margin`: the cached
    /// one, or a fresh one from the refresh token (always fresh with
    /// `force`, after Staw turned the cached one away). `None` when the
    /// person is not connected — never was, or Staw no longer knows their
    /// refresh token, which the store then forgets.
    pub async fn access(
        &self,
        key: &Key,
        margin: Duration,
        force: bool,
    ) -> Result<Option<String>, staw::Error> {
        if !force && let Some(access) = self.cached(key, margin) {
            return Ok(Some(access));
        }
        let Some(person) = self.store.get(key) else {
            self.forget_access(key);
            return Ok(None);
        };
        match self.staw.refresh(&self.oauth, &person.refresh_token).await {
            Ok(grant) => {
                let token = grant.access_token.clone();
                self.cache(key, grant.access_token, grant.expires_in);
                Ok(Some(token))
            }
            Err(RefreshError::Revoked) => {
                tracing::info!(team = %key.team, user = %key.user, "staw refresh token revoked; forgetting it");
                self.forget_access(key);
                if let Err(e) = self.store.remove(key) {
                    tracing::error!(error = %e, "could not forget a revoked token");
                }
                Ok(None)
            }
            Err(RefreshError::Staw(e)) => Err(e),
        }
    }

    fn cached(&self, key: &Key, margin: Duration) -> Option<String> {
        let access = self.access.lock().expect("access lock");
        access
            .get(key)
            .filter(|a| a.expires > Instant::now() + margin)
            .map(|a| a.token.clone())
    }

    fn cache(&self, key: &Key, token: String, expires_in: u64) {
        self.access.lock().expect("access lock").insert(
            key.clone(),
            Access {
                token,
                expires: Instant::now() + Duration::from_secs(expires_in),
            },
        );
    }

    fn forget_access(&self, key: &Key) {
        self.access.lock().expect("access lock").remove(key);
    }

    /// Put an access token in the cache as if Staw had just issued it,
    /// valid for `valid_for`. For tests that need an expired or a stale
    /// token; the bot itself only caches what Staw grants.
    #[doc(hidden)]
    pub fn set_access(&self, key: &Key, token: &str, valid_for: Duration) {
        self.access.lock().expect("access lock").insert(
            key.clone(),
            Access {
                token: token.to_string(),
                expires: Instant::now() + valid_for,
            },
        );
    }

    /// Start connecting `key` with a client named `client`, unless a
    /// grant for it is still open.
    pub async fn begin(&self, key: &Key, client: &str) -> Result<Begun, staw::Error> {
        let mut pending = self.pending.lock().await;
        if let Some(open) = pending.get(key).filter(|p| p.expires > Instant::now()) {
            return Ok(Begun::Already(open.clone()));
        }
        let client_id = self.staw.register(&self.oauth, client).await?;
        let opened = self.staw.device(&self.oauth, &client_id).await?;
        let open = Pending {
            link: opened.link().to_string(),
            code: opened.user_code.clone(),
            expires: Instant::now() + Duration::from_secs(opened.expires_in),
        };
        pending.insert(key.clone(), open.clone());
        Ok(Begun::Opened {
            client_id,
            client: client.to_string(),
            opened,
            pending: open,
        })
    }

    /// Poll a grant until the person decides or it expires. On approval
    /// the refresh token is stored and the access token cached.
    pub async fn wait(&self, key: &Key, client_id: &str, client: &str, opened: &Opened) -> Outcome {
        let outcome = self.poll(key, client_id, client, opened).await;
        // Only this grant: a newer one may have replaced it meanwhile.
        let mut pending = self.pending.lock().await;
        if pending.get(key).is_some_and(|p| p.code == opened.user_code) {
            pending.remove(key);
        }
        outcome
    }

    async fn poll(&self, key: &Key, client_id: &str, client: &str, opened: &Opened) -> Outcome {
        let deadline = Instant::now() + Duration::from_secs(opened.expires_in);
        let mut interval = opened.interval.max(1);
        loop {
            if Instant::now() >= deadline {
                return Outcome::Expired;
            }
            tokio::time::sleep(Duration::from_secs(interval)).await;
            match self
                .staw
                .poll(&self.oauth, client_id, &opened.device_code)
                .await
            {
                Ok(Poll::Pending) => continue,
                Ok(Poll::SlowDown) => interval += 5,
                Ok(Poll::Denied) => return Outcome::Denied,
                Ok(Poll::Expired) => return Outcome::Expired,
                Ok(Poll::Granted(grant)) => {
                    return self.connected(key, client, grant).await;
                }
                // A blip while the person is still approving: keep
                // polling until the code's own deadline.
                Err(staw::Error::Http(..)) => continue,
                Err(e) => {
                    tracing::warn!(error = %e, "the device grant failed");
                    return Outcome::Failed;
                }
            }
        }
    }

    async fn connected(&self, key: &Key, client: &str, grant: staw::Grant) -> Outcome {
        let Some(refresh_token) = grant.refresh_token.clone() else {
            tracing::warn!("staw granted no refresh token");
            return Outcome::Failed;
        };
        let name = match self.staw.me(&grant.access_token).await {
            Ok(me) => me.name,
            Err(e) => {
                tracing::warn!(error = %e, "could not read the connected account");
                return Outcome::Failed;
            }
        };
        let person = Person {
            refresh_token,
            name,
            client: client.to_string(),
        };
        if let Err(e) = self.store.put(key, person.clone()) {
            tracing::error!(error = %e, "could not store a refresh token");
            return Outcome::Failed;
        }
        self.cache(key, grant.access_token, grant.expires_in);
        Outcome::Connected(person)
    }

    /// Forget a person: the stored refresh token, the cached access
    /// token, an open grant. Returns what was stored.
    pub async fn disconnect(&self, key: &Key) -> Result<Option<Person>, store::Error> {
        self.forget_access(key);
        self.pending.lock().await.remove(key);
        self.store.remove(key)
    }
}
