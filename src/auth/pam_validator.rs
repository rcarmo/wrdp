use super::{
    peer_limit::PeerGuard,
    username::{normalize_local_account_name, validate_username},
};
use ironrdp_server::{
    CredentialDecision, CredentialValidationError, CredentialValidator, Credentials,
};
#[cfg(feature = "pam-auth")]
use nonstick::{AuthnFlags, ConversationAdapter, Transaction, TransactionBuilder};
use std::net::IpAddr;
#[cfg(feature = "pam-auth")]
use zeroize::Zeroize;
pub struct PamValidator {
    service: String,
    allowed: Option<String>,
    peers: PeerGuard,
    #[cfg(feature = "pam-auth")]
    authentication_slot: std::sync::Arc<tokio::sync::Semaphore>,
}
impl PamValidator {
    pub fn new(service: Option<String>) -> Self {
        Self::new_with_allowed_username(service, None)
    }
    pub fn new_with_allowed_username(service: Option<String>, allowed: Option<String>) -> Self {
        Self {
            service: service.unwrap_or_else(|| "wrdp".into()),
            allowed,
            peers: PeerGuard::new(),
            #[cfg(feature = "pam-auth")]
            authentication_slot: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }
    pub fn set_peer_ip(&self, ip: IpAddr) {
        self.peers.set_peer(ip);
    }
    pub fn prune_stale_entries(&self) {
        self.peers.prune();
    }
}
#[cfg(feature = "pam-auth")]
struct Conversation {
    user: String,
    password: String,
}
#[cfg(feature = "pam-auth")]
impl Drop for Conversation {
    fn drop(&mut self) {
        self.password.zeroize();
    }
}
#[cfg(feature = "pam-auth")]
impl ConversationAdapter for Conversation {
    fn prompt(&self, _: impl AsRef<std::ffi::OsStr>) -> nonstick::Result<std::ffi::OsString> {
        Ok((&self.user).into())
    }
    fn masked_prompt(
        &self,
        _: impl AsRef<std::ffi::OsStr>,
    ) -> nonstick::Result<std::ffi::OsString> {
        Ok((&self.password).into())
    }
    fn error_msg(&self, _: impl AsRef<std::ffi::OsStr>) {}
    fn info_msg(&self, _: impl AsRef<std::ffi::OsStr>) {}
}
#[cfg(feature = "pam-auth")]
fn authenticate(service: &str, user: String, password: String) -> bool {
    let conversation = Conversation {
        user: user.clone(),
        password,
    };
    let Ok(mut tx) = TransactionBuilder::new_with_service(service)
        .username(&user)
        .build(conversation.into_conversation())
    else {
        return false;
    };
    tx.authenticate(AuthnFlags::DISALLOW_NULL_AUTHTOK).is_ok()
        && tx.account_management(AuthnFlags::empty()).is_ok()
}
#[async_trait::async_trait]
impl CredentialValidator for PamValidator {
    async fn validate(
        &self,
        c: &Credentials,
    ) -> Result<CredentialDecision, CredentialValidationError> {
        if validate_username(&c.username).is_err() {
            return Ok(CredentialDecision::Reject);
        }
        let user = normalize_local_account_name(&c.username);
        if self
            .allowed
            .as_ref()
            .is_some_and(|allowed| allowed != &user)
        {
            return Ok(CredentialDecision::Reject);
        }
        let ip = self.peers.peer();
        if self.peers.blocked_for(ip).is_some() {
            return Ok(CredentialDecision::Reject);
        }
        #[cfg(feature = "pam-auth")]
        let accepted = {
            let permit = std::sync::Arc::clone(&self.authentication_slot)
                .acquire_owned()
                .await
                .map_err(CredentialValidationError::new)?;
            let service = self.service.clone();
            let password = c.password.clone();
            tokio::task::spawn_blocking(move || {
                // PAM modules may block past caller cancellation. Keep admission
                // occupied until the transaction actually releases its resources.
                let _permit = permit;
                authenticate(&service, user, password)
            })
            .await
            .map_err(|e| CredentialValidationError::new(std::io::Error::other(e.to_string())))?
        };
        #[cfg(not(feature = "pam-auth"))]
        let accepted = {
            let _ = &self.service;
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
    #[cfg(feature = "pam-auth")]
    #[tokio::test]
    async fn cancelled_waiter_does_not_release_active_pam_slot() {
        let validator = PamValidator::new(None);
        let permit = std::sync::Arc::clone(&validator.authentication_slot)
            .acquire_owned()
            .await
            .unwrap();
        let credentials = Credentials {
            username: "alice".into(),
            password: "unused".into(),
            domain: None,
        };
        assert!(
            tokio::time::timeout(std::time::Duration::ZERO, validator.validate(&credentials))
                .await
                .is_err()
        );
        assert_eq!(validator.authentication_slot.available_permits(), 0);
        drop(permit);
        assert_eq!(validator.authentication_slot.available_permits(), 1);
    }

    #[test]
    fn construction_preserves_account_restriction() {
        let v = PamValidator::new_with_allowed_username(None, Some("alice".into()));
        assert_eq!(v.allowed.as_deref(), Some("alice"));
    }
}
