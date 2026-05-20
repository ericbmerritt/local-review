#!/usr/bin/env bash
# Creates a sample jj stack at the path given as $1, used by the README
# screenshot recorder (docs/screenshots/jjr.tape). Mirrors the "retry
# policy" example from specs/jjr-tui-design.md so the screenshot lines up
# with how the TUI is documented.

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

# Base: scaffold client module.
cat >src/client.rs <<'RS'
use std::time::Duration;

pub struct Client {
    inner: reqwest::Client,
}

impl Client {
    pub fn new() -> Self {
        Self {
            inner: reqwest::Client::new(),
        }
    }

    pub async fn send(&self, req: Request) -> Result<Response> {
        let req = self.prepare(req)?;
        let resp = self.inner.request(req).await?;
        Ok(resp)
    }

    pub async fn fetch(&self, id: Id) -> Result<Item> {
        self.inner.fetch(id).await
    }

    fn prepare(&self, req: Request) -> Result<Request> {
        Ok(req)
    }
}
RS
jj describe -m "Scaffold client module" --quiet

# Change 2: wrap send() in retry_wrapper.
jj new --quiet -m "Add retry policy to client requests"
cat >src/client.rs <<'RS'
use std::time::Duration;

pub struct Client {
    inner: reqwest::Client,
    retry_wrapper: RetryWrapper,
}

impl Client {
    pub fn new() -> Self {
        Self {
            inner: reqwest::Client::new(),
            retry_wrapper: RetryWrapper::default(),
        }
    }

    pub async fn send(&self, req: Request) -> Result<Response> {
        let req = self.prepare(req)?;
        let resp = self.retry_wrapper
            .execute(|| self.inner.request(req.clone()))
            .await?;
        Ok(resp)
    }

    pub async fn fetch(&self, id: Id) -> Result<Item> {
        self.inner.fetch(id).await
    }

    fn prepare(&self, req: Request) -> Result<Request> {
        Ok(req)
    }
}
RS

# Change 3: add a backoff schedule.
jj new --quiet -m "Add backoff schedule to retry policy"
cat >>src/client.rs <<'RS'

impl RetryWrapper {
    pub fn with_backoff(mut self, schedule: BackoffSchedule) -> Self {
        self.schedule = schedule;
        self
    }
}
RS
