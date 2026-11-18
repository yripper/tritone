//! `tritone login` and `tritone logout`.
//!
//! Login runs the QR flow (or takes a pasted token), saves the token and the
//! cookie jar to the keyring, then tells a running daemon over its loopback
//! port, so the new login takes effect without a restart.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use qrcode::render::unicode::Dense1x2;
use qrcode::{EcLevel, QrCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::config::{Config, Paths};
use crate::discord::fingerprint::Fingerprint;
use crate::discord::login::{LoginError, qr_login, validate_pasted};
use crate::discord::transport::{CookieJar, http_client};
use crate::secrets::{ACCOUNT_BEARER, ACCOUNT_COOKIES, ACCOUNT_TOKEN, SecretStore, default_store};
use crate::store::Store;

const NOTIFY_TIMEOUT: Duration = Duration::from_secs(10);

const SCAN_HELP: &str =
    "Scan this QR code with the Discord app on your phone (Settings → Scan QR Code), then confirm on the phone.";

const PASTE_HELP: &str = "To paste a token instead:
  1. Open https://discord.com/app in your browser and log in.
  2. Open the developer tools (Cmd+Option+I on macOS, Ctrl+Shift+I on Linux) and pick the Network tab.
  3. Click any request to /api/ and copy the value of its `Authorization` request header.
The token is as good as your password: never share it. tritone keeps it in the OS keyring only.";

/// The QR code as unicode blocks, light on dark (most terminals are dark;
/// the quiet zone then shows as the light border scanners need).
pub fn render_qr(url: &str) -> Result<String> {
    let code = QrCode::with_error_correction_level(url.as_bytes(), EcLevel::M)?;
    Ok(code.render::<Dense1x2>().dark_color(Dense1x2::Light).light_color(Dense1x2::Dark).quiet_zone(true).build())
}

fn prompt_token() -> Result<String> {
    let raw = rpassword::prompt_password("Discord token (input hidden): ").context("reading the token failed")?;
    Ok(validate_pasted(&raw)?)
}

/// The fingerprint the daemon uses (cached build number, installation id);
/// a fresh one when the store can't be opened.
fn fingerprint(paths: &Paths, secrets: &dyn SecretStore) -> Fingerprint {
    Store::open(&paths.db)
        .and_then(|store| Fingerprint::load(&store, secrets))
        .unwrap_or_else(|_| Fingerprint::for_this_machine())
}

/// Stores the token, and the cookie jar when the login changed it.
pub async fn save_login(secrets: &dyn SecretStore, jar: &CookieJar, token: &str) -> Result<()> {
    secrets.set(ACCOUNT_TOKEN, token).await.context("saving the token to the keyring failed")?;
    jar.save_if_dirty(secrets, i64::MAX).await.context("saving the cookies to the keyring failed")?;
    Ok(())
}

/// Removes the Discord token and cookies. The MCP bearer stays.
pub async fn forget_login(secrets: &dyn SecretStore) -> Result<()> {
    secrets.delete(ACCOUNT_TOKEN).await?;
    secrets.delete(ACCOUNT_COOKIES).await
}

pub enum Notified {
    Answered { status: u16, text: String },
    NotRunning,
}

/// `POST /login-notify` on the daemon's loopback port. Plain HTTP/1.1 over a
/// socket: it never leaves the machine and needs no client library.
pub async fn notify_daemon(port: u16, bearer: &str) -> Result<Notified> {
    let mut stream = match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => return Ok(Notified::NotRunning),
        Err(e) => return Err(e).context("connecting to tritone failed"),
    };
    let request = format!(
        "POST /login-notify HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {bearer}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let exchange = async {
        stream.write_all(request.as_bytes()).await?;
        let mut answer = Vec::new();
        stream.read_to_end(&mut answer).await?;
        Ok::<_, std::io::Error>(answer)
    };
    let answer = tokio::time::timeout(NOTIFY_TIMEOUT, exchange).await.context("tritone didn't answer")??;
    let answer = String::from_utf8_lossy(&answer);
    let (head, body) = answer.split_once("\r\n\r\n").unwrap_or((&answer, ""));
    let status = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).context("tritone's answer wasn't HTTP")?;
    Ok(Notified::Answered { status, text: body.trim().to_string() })
}

/// Tells a running daemon; a stopped one reads the keyring when it starts.
async fn tell_daemon(port: u16, secrets: &dyn SecretStore) {
    let bearer = match secrets.get(ACCOUNT_BEARER).await {
        Ok(Some(b)) => b,
        _ => {
            println!(
                "tritone hasn't run yet: start tritone (`pm2 start ecosystem.config.cjs`) and it will use this login."
            );
            return;
        }
    };
    match notify_daemon(port, &bearer).await {
        Ok(Notified::Answered { status: 200, text }) => println!("{text}"),
        Ok(Notified::Answered { text, .. }) => println!("tritone answered: {text}"),
        Ok(Notified::NotRunning) => {
            println!("tritone isn't running: start tritone (`pm2 start tritone`) and it will use this login.")
        }
        Err(e) => println!("couldn't reach tritone ({e:#}): restart it (`pm2 restart tritone`) to use this login."),
    }
}

/// `tritone login [--token]`.
pub async fn login(paste: bool) -> Result<()> {
    let paths = Paths::resolve()?;
    let port = Config::load(&paths)?.port;
    let secrets = default_store()?;
    let jar = Arc::new(CookieJar::load(secrets.as_ref()).await?);
    let token = if paste {
        println!("{PASTE_HELP}");
        prompt_token()?
    } else {
        let fp = fingerprint(&paths, secrets.as_ref());
        let client = http_client(jar.clone())?;
        println!("{SCAN_HELP}");
        let show = |url: &str| match render_qr(url) {
            Ok(qr) => println!("{qr}"),
            Err(e) => println!("couldn't draw the QR code: {e}"),
        };
        match qr_login(&client, &fp, &show).await {
            Ok(token) => token,
            Err(LoginError::Captcha) => {
                println!("Discord asked for a CAPTCHA. tritone never solves those, so the QR login stops here.");
                println!("{PASTE_HELP}");
                prompt_token()?
            }
            Err(e) => bail!("{e}. Run `tritone login` to try again, or `tritone login --token` to paste a token."),
        }
    };
    save_login(secrets.as_ref(), &jar, &token).await?;
    println!("logged in");
    tell_daemon(port, secrets.as_ref()).await;
    Ok(())
}

/// `tritone logout`.
pub async fn logout() -> Result<()> {
    forget_login(default_store()?.as_ref()).await?;
    println!("logged out: the Discord token and cookies are gone from the keyring.");
    println!("A running tritone keeps its current connection until it restarts (`pm2 restart tritone`).");
    Ok(())
}

#[cfg(test)]
#[path = "login_tests.rs"]
mod tests;
