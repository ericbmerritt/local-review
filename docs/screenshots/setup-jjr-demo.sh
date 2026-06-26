#!/usr/bin/env bash
# Creates a sample jj stack at the path given as $1, used by the README
# screenshot recorder (docs/screenshots/jjr.tape).
#
# The stack demonstrates the entity list: functions that call each other,
# so tree-sitter extraction produces multiple rows with caller counts.

set -euo pipefail

if [ $# -ne 1 ]; then
    echo "usage: setup-jjr-demo.sh <repo-path>" >&2
    exit 2
fi

repo="$1"
rm -rf "$repo"
mkdir -p "$repo"

config="$repo/.jjconfig.toml"
cat >"$config" <<'TOML'
[user]
name = "Demo"
email = "demo@example.com"
TOML
export JJ_CONFIG="$config"

cd "$repo"
jj git init --quiet
echo '/.jj-review' >.gitignore
mkdir -p src

# Base: minimal scaffolding.
cat >src/auth.rs <<'RS'
pub struct Credentials {
    pub username: String,
    pub password: String,
}

pub struct Session {
    pub token: String,
}
RS
jj describe -m "Scaffold auth types" --quiet

# Change 1: add the auth functions (four functions that call each other).
jj new --quiet -m "Add authentication layer"
cat >src/auth.rs <<'RS'
use std::time::Duration;

pub struct Credentials {
    pub username: String,
    pub password: String,
}

pub struct Session {
    pub token: String,
    pub expires_in: Duration,
}

pub fn login(creds: &Credentials) -> Result<Session, AuthError> {
    verify_credentials(creds)?;
    check_rate_limit(&creds.username)?;
    create_session(creds)
}

pub fn verify_credentials(creds: &Credentials) -> Result<(), AuthError> {
    let stored = load_hash(&creds.username)?;
    if hash_password(&creds.password) != stored {
        return Err(AuthError::InvalidCredentials);
    }
    Ok(())
}

pub fn create_session(creds: &Credentials) -> Result<Session, AuthError> {
    Ok(Session {
        token: generate_token(&creds.username),
        expires_in: Duration::from_secs(3600),
    })
}

fn check_rate_limit(username: &str) -> Result<(), AuthError> {
    let attempts = recent_attempts(username);
    if attempts >= 5 {
        return Err(AuthError::RateLimited);
    }
    Ok(())
}

fn hash_password(password: &str) -> String {
    format!("sha256:{password}")
}

fn generate_token(username: &str) -> String {
    format!("tok:{username}:{}", rand_hex())
}

fn load_hash(_username: &str) -> Result<String, AuthError> {
    Ok("sha256:hunter2".to_owned())
}

fn recent_attempts(_username: &str) -> u32 { 0 }
fn rand_hex() -> &'static str { "deadbeef" }

#[derive(Debug)]
pub enum AuthError {
    InvalidCredentials,
    RateLimited,
    StorageError,
}
RS

# Change 2: tighten rate limit and improve session TTL.
jj new --quiet -m "Tighten rate limit; make session TTL configurable"
cat >src/auth.rs <<'RS'
use std::time::Duration;

pub struct Credentials {
    pub username: String,
    pub password: String,
}

pub struct Session {
    pub token: String,
    pub expires_in: Duration,
}

pub struct Config {
    pub session_ttl: Duration,
    pub max_attempts: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            session_ttl: Duration::from_secs(3600),
            max_attempts: 3,
        }
    }
}

pub fn login(creds: &Credentials, cfg: &Config) -> Result<Session, AuthError> {
    verify_credentials(creds)?;
    check_rate_limit(&creds.username, cfg.max_attempts)?;
    create_session(creds, cfg.session_ttl)
}

pub fn verify_credentials(creds: &Credentials) -> Result<(), AuthError> {
    let stored = load_hash(&creds.username)?;
    if hash_password(&creds.password) != stored {
        return Err(AuthError::InvalidCredentials);
    }
    Ok(())
}

pub fn create_session(creds: &Credentials, ttl: Duration) -> Result<Session, AuthError> {
    Ok(Session {
        token: generate_token(&creds.username),
        expires_in: ttl,
    })
}

fn check_rate_limit(username: &str, max: u32) -> Result<(), AuthError> {
    let attempts = recent_attempts(username);
    if attempts >= max {
        return Err(AuthError::RateLimited);
    }
    Ok(())
}

fn hash_password(password: &str) -> String {
    format!("sha256:{password}")
}

fn generate_token(username: &str) -> String {
    format!("tok:{username}:{}", rand_hex())
}

fn load_hash(_username: &str) -> Result<String, AuthError> {
    Ok("sha256:hunter2".to_owned())
}

fn recent_attempts(_username: &str) -> u32 { 0 }
fn rand_hex() -> &'static str { "deadbeef" }

#[derive(Debug)]
pub enum AuthError {
    InvalidCredentials,
    RateLimited,
    StorageError,
}
RS
