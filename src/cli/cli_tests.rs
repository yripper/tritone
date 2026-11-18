use super::daemon::{buffer_policy, ensure_bearer_token};
use crate::config::Config;
use crate::secrets::fake::MemSecrets;

#[tokio::test]
async fn bearer_token_created_once() {
    let s = MemSecrets::default();
    let a = ensure_bearer_token(&s).await.unwrap();
    let b = ensure_bearer_token(&s).await.unwrap();
    assert_eq!(a, b);
    assert_eq!(a.len(), 64);
    assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn buffer_policy_comes_from_config() {
    let p = buffer_policy(&Config::default());
    assert_eq!(p.per_channel, 200);
    assert_eq!(p.max_age_ms, 24 * 3_600_000);
}
