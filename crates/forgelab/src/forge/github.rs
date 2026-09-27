//! Placeholder until the M6 stream lands the real client.
use super::*;
use async_trait::async_trait;
use secrecy::SecretString;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
pub const DEFAULT_BASE_URL: &str = "https://github.com";
pub struct Client;
impl Client {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(
        _base_url: &str,
        _org: &str,
        _token: SecretString,
        _t: Arc<dyn Transport>,
        _c: CancellationToken,
    ) -> Result<Client, ForgeError> {
        Err(ForgeError::msg("GitHub support arrives with the M6 stream"))
    }
}
#[async_trait]
impl Forge for Client {
    fn name(&self) -> &'static str {
        "GitHub"
    }
    fn caps(&self) -> Caps {
        unreachable!()
    }
    fn policy(&self) -> ForgePolicy {
        unreachable!()
    }
    async fn ensure_org(&self) -> Result<(), ForgeError> {
        unreachable!()
    }
    async fn get(&self, _: &str) -> Result<Option<Repo>, ForgeError> {
        unreachable!()
    }
    async fn create(&self, _: &str, _: &str, _: &str, _: &[String]) -> Result<(), ForgeError> {
        unreachable!()
    }
    async fn delete(&self, _: &str) -> Result<(), ForgeError> {
        unreachable!()
    }
    async fn delete_namespace(&self, _: &str) -> Result<Removal, ForgeError> {
        unreachable!()
    }
    async fn update_settings(&self, _: &str, _: Settings) -> Result<(), ForgeError> {
        unreachable!()
    }
    async fn set_topics(&self, _: &str, _: &[String]) -> Result<(), ForgeError> {
        unreachable!()
    }
    async fn branches(&self, _: &str) -> Result<Vec<Ref>, ForgeError> {
        unreachable!()
    }
    async fn delete_branch(&self, _: &str, _: &str) -> Result<(), ForgeError> {
        unreachable!()
    }
    async fn delete_tag(&self, _: &str, _: &str) -> Result<(), ForgeError> {
        unreachable!()
    }
    async fn allow_force_push(&self, _: &str, _: &str) -> Result<(), ForgeError> {
        unreachable!()
    }
    async fn open_requests(&self, _: &str) -> Result<Vec<Request>, ForgeError> {
        unreachable!()
    }
    async fn close_request(&self, _: &str, _: i64) -> Result<(), ForgeError> {
        unreachable!()
    }
    fn git_remote(&self, _: &str) -> Result<GitRemote, ForgeError> {
        unreachable!()
    }
}
