use super::{
    peer_limit::PeerGuard,
    username::{normalize_local_account_name, validate_username},
};
use anyhow::Result;
use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng},
};
use ironrdp_server::{
    CredentialDecision, CredentialValidationError, CredentialValidator, Credentials,
};
use std::{collections::BTreeMap, net::IpAddr};
pub struct StaticPasswordValidator {
    hashes: BTreeMap<String, String>,
    peers: PeerGuard,
}
impl StaticPasswordValidator {
    pub fn new_hash(username: String, hash: String) -> Result<Self> {
        Self::new_hashes([(username, hash)])
    }
    pub fn new_hashes(values: impl IntoIterator<Item = (String, String)>) -> Result<Self> {
        let mut hashes = BTreeMap::new();
        for (username, hash) in values {
            validate_username(&username)?;
            let account = normalize_local_account_name(&username);
            anyhow::ensure!(
                hashes.insert(account.clone(), hash).is_none(),
                "duplicate static credential for local account {account}"
            );
        }
        anyhow::ensure!(
            !hashes.is_empty(),
            "at least one static credential is required"
        );
        for (user, hash) in &hashes {
            validate_username(user)?;
            let parsed = PasswordHash::new(hash).map_err(|e| anyhow::anyhow!(e.to_string()))?;
            anyhow::ensure!(
                parsed.algorithm.as_str() == "argon2id",
                "credential for {user} is not Argon2id"
            );
        }
        Ok(Self {
            hashes,
            peers: PeerGuard::new(),
        })
    }
    pub fn set_peer_ip(&self, ip: IpAddr) {
        self.peers.set_peer(ip)
    }
    pub fn prune_stale_entries(&self) {
        self.peers.prune()
    }
}
pub fn hash_static_password(password: &str) -> Result<String> {
    Argon2::default()
        .hash_password(password.as_bytes(), &SaltString::generate(&mut OsRng))
        .map(|v| v.to_string())
        .map_err(|e| anyhow::anyhow!(e.to_string()))
}
#[async_trait::async_trait]
impl CredentialValidator for StaticPasswordValidator {
    async fn validate(
        &self,
        c: &Credentials,
    ) -> Result<CredentialDecision, CredentialValidationError> {
        if validate_username(&c.username).is_err() {
            return Ok(CredentialDecision::Reject);
        }
        let ip = self.peers.peer();
        if self.peers.blocked_for(ip).is_some() {
            return Ok(CredentialDecision::Reject);
        }
        let user = normalize_local_account_name(&c.username);
        let accepted = if let Some(hash) = self.hashes.get(&user).cloned() {
            // Argon2 is deliberately CPU/memory intensive. Keep it off the
            // async executor so connection deadlines and cancellation can run.
            let password = zeroize::Zeroizing::new(c.password.clone());
            tokio::task::spawn_blocking(move || {
                PasswordHash::new(&hash).ok().is_some_and(|hash| {
                    Argon2::default()
                        .verify_password(password.as_bytes(), &hash)
                        .is_ok()
                })
            })
            .await
            .map_err(CredentialValidationError::new)?
        } else {
            false
        };
        if accepted {
            self.peers.success(ip);
            Ok(CredentialDecision::Accept)
        } else {
            self.peers.failure(ip);
            Ok(CredentialDecision::Reject)
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use ironrdp_server::CredentialOrigin;
    #[tokio::test(flavor = "current_thread")]
    async fn password_verification_yields_to_the_executor() {
        use std::{
            future::Future as _,
            task::{Context, Poll, Waker},
        };

        let validator = StaticPasswordValidator::new_hash(
            "alice".into(),
            hash_static_password("secret").unwrap(),
        )
        .unwrap();
        let credentials = Credentials {
            username: "alice".into(),
            password: "secret".into(),
            domain: None,
        };
        let mut verification =
            validator.validate_received(&credentials, CredentialOrigin::ClientInfo);
        // The previous synchronous implementation completed the entire costly
        // verification in this poll, preventing timers from being serviced.
        assert!(matches!(
            verification
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
        assert_eq!(verification.await.unwrap(), CredentialDecision::Accept);
    }

    #[test]
    fn rejects_duplicate_normalized_accounts() {
        let hash = hash_static_password("secret").unwrap();
        let result = StaticPasswordValidator::new_hashes([
            ("alice".into(), hash.clone()),
            ("DOMAIN\\alice".into(), hash),
        ]);
        assert!(result.err().unwrap().to_string().contains("duplicate"));
    }

    #[tokio::test]
    async fn configured_domain_account_matches_normalized_login() {
        let validator = StaticPasswordValidator::new_hash(
            "DOMAIN\\alice".into(),
            hash_static_password("secret").unwrap(),
        )
        .unwrap();
        assert_eq!(
            validator
                .validate_received(
                    &Credentials {
                        username: "alice".into(),
                        password: "secret".into(),
                        domain: None,
                    },
                    CredentialOrigin::ClientInfo
                )
                .await
                .unwrap(),
            CredentialDecision::Accept
        );
    }

    #[tokio::test]
    async fn verifies_argon2id() {
        let v = StaticPasswordValidator::new_hash(
            "alice".into(),
            hash_static_password("secret").unwrap(),
        )
        .unwrap();
        assert_eq!(
            v.validate_received(
                &Credentials {
                    username: "DOMAIN\\alice".into(),
                    password: "secret".into(),
                    domain: None,
                },
                CredentialOrigin::ClientInfo,
            )
            .await
            .unwrap(),
            CredentialDecision::Accept
        );
    }
}
