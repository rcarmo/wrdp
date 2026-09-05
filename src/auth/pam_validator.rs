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
        }
    }
    pub fn set_peer_ip(&self, ip: IpAddr) {
        self.peers.set_peer(ip)
    }
    pub fn prune_stale_entries(&self) {
        self.peers.prune()
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
            let service = self.service.clone();
            let password = c.password.clone();
            tokio::task::spawn_blocking(move || authenticate(&service, user, password))
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
    #[test]
    fn construction_preserves_account_restriction() {
        let v = PamValidator::new_with_allowed_username(None, Some("alice".into()));
        assert_eq!(v.allowed.as_deref(), Some("alice"));
    }
}
