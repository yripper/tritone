use super::*;
use std::sync::Mutex;

#[derive(Default)]
struct FakeRunner {
    code: i32,
    stdout: String,
    calls: Mutex<Vec<(Vec<String>, Option<String>)>>,
}

#[async_trait]
impl CommandRunner for &FakeRunner {
    async fn run(&self, args: &[&str], stdin: Option<&str>) -> Result<CmdOutput> {
        self.calls.lock().unwrap().push((args.iter().map(|s| s.to_string()).collect(), stdin.map(str::to_string)));
        Ok(CmdOutput { code: self.code, stdout: self.stdout.clone() })
    }
}

#[tokio::test]
async fn get_returns_none_on_exit_44() {
    let r = FakeRunner { code: 44, ..Default::default() };
    assert_eq!(Keychain::new(&r).get(ACCOUNT_TOKEN).await.unwrap(), None);
}

#[tokio::test]
async fn get_strips_trailing_newline() {
    let r = FakeRunner { stdout: "tok-1\n".into(), ..Default::default() };
    assert_eq!(Keychain::new(&r).get(ACCOUNT_TOKEN).await.unwrap().as_deref(), Some("tok-1"));
    let calls = r.calls.lock().unwrap();
    assert_eq!(calls[0].0, ["find-generic-password", "-s", "tritone", "-a", "token", "-w"]);
}

#[tokio::test]
async fn set_writes_via_stdin_not_argv() {
    let r = FakeRunner::default();
    Keychain::new(&r).set(ACCOUNT_TOKEN, "tok.abc-1").await.unwrap();
    let calls = r.calls.lock().unwrap();
    assert_eq!(calls[0].0, ["-i"]);
    assert_eq!(calls[0].1.as_deref(), Some("add-generic-password -U -s tritone -a token -w tok.abc-1\n"));
}

#[tokio::test]
async fn set_rejects_unsafe_chars() {
    let r = FakeRunner::default();
    let k = Keychain::new(&r);
    assert!(k.set(ACCOUNT_TOKEN, "a b").await.is_err());
    assert!(k.set(ACCOUNT_TOKEN, "a;b").await.is_err());
    assert!(k.set(ACCOUNT_TOKEN, "").await.is_err());
    assert!(r.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn set_accepts_percent_encoded_value() {
    let r = FakeRunner::default();
    Keychain::new(&r).set(ACCOUNT_COOKIES, "abc%2Fdef%3D").await.unwrap();
}

#[tokio::test]
async fn delete_ignores_not_found() {
    let r = FakeRunner { code: 44, ..Default::default() };
    Keychain::new(&r).delete(ACCOUNT_TOKEN).await.unwrap();
}

#[tokio::test]
async fn secret_tool_get_looks_up_service_and_account() {
    let r = FakeRunner { stdout: "tok-1".into(), ..Default::default() };
    assert_eq!(SecretTool::new(&r).get(ACCOUNT_TOKEN).await.unwrap().as_deref(), Some("tok-1"));
    assert_eq!(r.calls.lock().unwrap()[0].0, ["lookup", "service", "tritone", "account", "token"]);
}

#[tokio::test]
async fn secret_tool_get_returns_none_when_missing() {
    let r = FakeRunner { code: 1, ..Default::default() };
    assert_eq!(SecretTool::new(&r).get(ACCOUNT_TOKEN).await.unwrap(), None);
}

#[tokio::test]
async fn secret_tool_set_passes_value_on_stdin_only() {
    let r = FakeRunner::default();
    SecretTool::new(&r).set(ACCOUNT_COOKIES, "abc%2F").await.unwrap();
    let calls = r.calls.lock().unwrap();
    assert_eq!(calls[0].0, ["store", "--label", "tritone cookies", "service", "tritone", "account", "cookies"]);
    assert_eq!(calls[0].1.as_deref(), Some("abc%2F"));
    assert!(!calls[0].0.iter().any(|a| a.contains("abc")));
}

#[tokio::test]
async fn secret_tool_set_fails_on_nonzero_exit() {
    let r = FakeRunner { code: 1, ..Default::default() };
    assert!(SecretTool::new(&r).set(ACCOUNT_TOKEN, "tok-1").await.is_err());
}

#[tokio::test]
async fn secret_tool_delete_clears_the_item() {
    let r = FakeRunner::default();
    SecretTool::new(&r).delete(ACCOUNT_TOKEN).await.unwrap();
    assert_eq!(r.calls.lock().unwrap()[0].0, ["clear", "service", "tritone", "account", "token"]);
}

#[tokio::test]
async fn missing_program_explains_how_to_install() {
    let err = Cli::new("/nonexistent/secret-tool", MISSING_SECRET_TOOL).run(&["lookup"], None).await.unwrap_err();
    assert!(err.to_string().contains("libsecret-tools"));
}
