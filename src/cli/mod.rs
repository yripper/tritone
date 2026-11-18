//! Command line: `start` (daemon), `login`, `logout`, `status`, `token`.

pub mod daemon;
pub mod login;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::config::{Config, Paths};
use crate::secrets::{ACCOUNT_BEARER, ACCOUNT_TOKEN, SecretStore, default_store};

#[derive(Parser)]
#[command(
    name = "tritone",
    version,
    about = "Local MCP server that lets Claude triage and answer your Discord, as you"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run the MCP server (what pm2 runs).
    Start,
    /// Log in to Discord (QR code by default) and save the session to the OS keyring.
    Login {
        /// Paste a token instead of scanning a QR code.
        #[arg(long)]
        token: bool,
    },
    /// Remove the Discord session from the OS keyring.
    Logout,
    /// Show the port and which secrets are stored. Never contacts Discord.
    Status,
    /// Print the MCP bearer token (for `claude mcp add`).
    Token,
}

fn keychain() -> Result<Box<dyn SecretStore>> {
    default_store()
}

fn present(v: Option<String>) -> &'static str {
    if v.is_some() { "stored" } else { "missing" }
}

/// Local facts only: validating the token with a REST call would look like a bot.
async fn status() -> Result<()> {
    let k = keychain()?;
    let port = Config::load(&Paths::resolve()?)?.port;
    println!("port: {port}");
    println!("bearer token: {}", present(k.get(ACCOUNT_BEARER).await?));
    match k.get(ACCOUNT_TOKEN).await? {
        Some(_) => println!("discord: token stored (the daemon's status tool says whether it works)"),
        None => println!("discord: not logged in, run `tritone login`"),
    }
    Ok(())
}

pub async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Start => daemon::start().await,
        Command::Login { token } => login::login(token).await,
        Command::Logout => login::logout().await,
        Command::Status => status().await,
        Command::Token => {
            println!("{}", daemon::ensure_bearer_token(keychain()?.as_ref()).await?);
            Ok(())
        }
    }
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod tests;
