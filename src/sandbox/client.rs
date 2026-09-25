//! HTTP to OpenSandbox: the lifecycle API, and execd inside a sandbox
//! through the server's proxy.
//!
//! Only port 9090 on the OpenSandbox host is used. execd is reached at the
//! endpoint `GET /v1/sandboxes/{id}/endpoints/44772?use_server_proxy=true`
//! returns, which OpenSandbox 0.2.3 gives without a scheme and without any
//! extra headers; headers it does return are sent on every execd request.

use super::Error;
use super::stream::{Output, Parser, preview};
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;
use url::Url;

/// The header OpenSandbox reads its API key from.
pub const API_KEY_HEADER: &str = "OPEN-SANDBOX-API-KEY";
/// The port execd listens on inside every sandbox.
pub const EXECD_PORT: u16 = 44772;
/// Stop reading a response body after this many bytes. Output past
/// [`super::stream::MAX_OUTPUT_BYTES`] is only counted, but an endless
/// stream must still end.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
/// Slack on top of a command's own timeout before the HTTP request gives up.
const REQUEST_SLACK: Duration = Duration::from_secs(30);

fn http() -> reqwest::Client {
    // Only fails if the TLS backend cannot initialise, which is a build
    // problem, not a runtime condition.
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("reqwest client builds with the rustls backend")
}

/// What a new sandbox is made from.
#[derive(Debug, Clone)]
pub struct CreateSpec {
    pub image: String,
    pub entrypoint: Vec<String>,
    pub timeout_secs: u64,
    pub cpu: String,
    pub memory: String,
    pub env: BTreeMap<String, String>,
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct Status {
    state: String,
}

#[derive(Debug, Deserialize)]
struct SandboxBody {
    id: String,
    status: Status,
}

#[derive(Debug, Deserialize)]
struct EndpointBody {
    endpoint: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

/// A created sandbox: its id and the state it was created in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    pub id: String,
    pub state: String,
}

/// The OpenSandbox lifecycle API.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: Url,
    api_key: Option<HeaderValue>,
}

impl Client {
    /// `api_key` is sent as [`API_KEY_HEADER`] on every request.
    pub fn new(base: Url, api_key: Option<HeaderValue>) -> Self {
        Self {
            http: http(),
            base,
            api_key,
        }
    }

    fn url(&self, segments: &[&str]) -> Url {
        let mut url = self.base.clone();
        // Cannot fail: the base is checked to be http(s) when configured.
        url.path_segments_mut()
            .expect("an http(s) URL has path segments")
            .pop_if_empty()
            .extend(segments);
        url
    }

    fn request(&self, method: Method, segments: &[&str]) -> RequestBuilder {
        let request = self
            .http
            .request(method, self.url(segments))
            .timeout(REQUEST_SLACK);
        match &self.api_key {
            Some(key) => request.header(API_KEY_HEADER, key),
            None => request,
        }
    }

    pub async fn create(&self, spec: &CreateSpec) -> Result<Created, Error> {
        let body = json!({
            "image": {"uri": spec.image},
            "entrypoint": spec.entrypoint,
            "timeout": spec.timeout_secs,
            "resourceLimits": {"cpu": spec.cpu, "memory": spec.memory},
            "env": spec.env,
            "metadata": spec.metadata,
        });
        let response = self
            .request(Method::POST, &["v1", "sandboxes"])
            .json(&body)
            .send()
            .await?;
        let created: SandboxBody = json_body("create sandbox", response).await?;
        Ok(Created {
            id: created.id,
            state: created.status.state,
        })
    }

    /// The sandbox's state, or `None` if the server does not know it.
    pub async fn state(&self, id: &str) -> Result<Option<String>, Error> {
        let response = self
            .request(Method::GET, &["v1", "sandboxes", id])
            .send()
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let body: SandboxBody = json_body("get sandbox", response).await?;
        Ok(Some(body.status.state))
    }

    /// Delete a sandbox. One that is already gone counts as deleted.
    pub async fn delete(&self, id: &str) -> Result<(), Error> {
        let response = self
            .request(Method::DELETE, &["v1", "sandboxes", id])
            .send()
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(());
        }
        checked("delete sandbox", response).await.map(drop)
    }

    /// Push the sandbox's expiry to `expires_at`, an RFC 3339 time. Returns
    /// false if the sandbox no longer exists.
    pub async fn renew(&self, id: &str, expires_at: &str) -> Result<bool, Error> {
        let response = self
            .request(Method::POST, &["v1", "sandboxes", id, "renew-expiration"])
            .json(&json!({"expiresAt": expires_at}))
            .send()
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(false);
        }
        checked("renew sandbox", response).await.map(|_| true)
    }

    /// execd inside the sandbox, through the server's proxy.
    pub async fn execd(&self, id: &str) -> Result<Execd, Error> {
        let port = EXECD_PORT.to_string();
        let response = self
            .request(Method::GET, &["v1", "sandboxes", id, "endpoints", &port])
            .query(&[("use_server_proxy", "true")])
            .send()
            .await?;
        let body: EndpointBody = json_body("get execd endpoint", response).await?;
        let base = if body.endpoint.contains("://") {
            body.endpoint
        } else {
            format!("{}://{}", self.base.scheme(), body.endpoint)
        };
        let base = Url::parse(&base)
            .map_err(|e| Error::Protocol(format!("execd endpoint `{base}`: {e}")))?;
        let mut headers = header_map(&body.headers)?;
        // The proxy is the lifecycle server, which checks the same key.
        if let Some(key) = &self.api_key {
            headers.insert(API_KEY_HEADER, key.clone());
        }
        Ok(Execd {
            http: self.http.clone(),
            base,
            headers,
        })
    }
}

/// execd in one sandbox.
#[derive(Clone)]
pub struct Execd {
    http: reqwest::Client,
    base: Url,
    headers: HeaderMap,
}

#[derive(Debug, Deserialize)]
struct SessionBody {
    session_id: String,
}

#[derive(Debug, Deserialize)]
struct ContextBody {
    id: String,
}

impl Execd {
    fn request(&self, method: Method, segments: &[&str], timeout: Duration) -> RequestBuilder {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .expect("an http(s) URL has path segments")
            .pop_if_empty()
            .extend(segments);
        self.http
            .request(method, url)
            .headers(self.headers.clone())
            .timeout(timeout + REQUEST_SLACK)
    }

    /// POST `body` and read the event stream it answers with.
    async fn stream(
        &self,
        what: &str,
        segments: &[&str],
        body: Value,
        timeout: Duration,
    ) -> Result<Output, Error> {
        let response = self
            .request(Method::POST, segments, timeout)
            .json(&body)
            .send()
            .await?;
        let response = checked(what, response).await?;
        let mut parser = Parser::default();
        let mut output = Output::default();
        let mut read = 0;
        let mut body = response.bytes_stream();
        while let Some(chunk) = body.next().await {
            let chunk = chunk?;
            read += chunk.len();
            for event in parser.push(&chunk)? {
                output.apply(event);
            }
            if read > MAX_BODY_BYTES {
                output.omitted += read - MAX_BODY_BYTES;
                output.error = Some(format!(
                    "stopped reading after {MAX_BODY_BYTES} bytes of output"
                ));
                return Ok(output);
            }
        }
        for event in parser.finish()? {
            output.apply(event);
        }
        Ok(output)
    }

    /// Run shell text once, in a fresh shell.
    pub async fn command(&self, command: &str, timeout: Duration) -> Result<Output, Error> {
        let body = json!({"command": command, "timeout": timeout.as_millis() as u64});
        self.stream("run command", &["command"], body, timeout)
            .await
    }

    /// A new persistent bash session.
    pub async fn create_session(&self) -> Result<String, Error> {
        let response = self
            .request(Method::POST, &["session"], Duration::ZERO)
            .json(&json!({}))
            .send()
            .await?;
        let body: SessionBody = json_body("create bash session", response).await?;
        Ok(body.session_id)
    }

    /// Run shell text in a bash session, which keeps its cwd and env.
    pub async fn run_in_session(
        &self,
        session: &str,
        command: &str,
        timeout: Duration,
    ) -> Result<Output, Error> {
        let body = json!({"command": command, "timeout": timeout.as_millis() as u64});
        self.stream(
            "run in bash session",
            &["session", session, "run"],
            body,
            timeout,
        )
        .await
    }

    /// A new code-interpreter context for `language`.
    pub async fn create_context(&self, language: &str) -> Result<String, Error> {
        let response = self
            .request(Method::POST, &["code", "context"], Duration::ZERO)
            .json(&json!({"language": language}))
            .send()
            .await?;
        let body: ContextBody = json_body("create code context", response).await?;
        Ok(body.id)
    }

    /// Run `code` in a code-interpreter context, which keeps its variables.
    pub async fn run_code(
        &self,
        context: &str,
        language: &str,
        code: &str,
        timeout: Duration,
    ) -> Result<Output, Error> {
        let body = json!({"context": {"id": context, "language": language}, "code": code});
        self.stream("run code", &["code"], body, timeout).await
    }

    /// Write `content` to `path`, creating or replacing the file.
    pub async fn upload(&self, path: &str, content: Vec<u8>) -> Result<(), Error> {
        let name = path.rsplit('/').next().unwrap_or(path).to_string();
        // execd reads the metadata part as a file: without a filename it
        // answers "metadata file is missing" (checked against 0.2.3).
        let metadata = reqwest::multipart::Part::text(json!({"path": path}).to_string())
            .file_name("metadata.json")
            .mime_str("application/json")?;
        let file = reqwest::multipart::Part::bytes(content)
            .file_name(name)
            .mime_str("application/octet-stream")?;
        let form = reqwest::multipart::Form::new()
            .part("metadata", metadata)
            .part("file", file);
        let response = self
            .request(Method::POST, &["files", "upload"], Duration::ZERO)
            .multipart(form)
            .send()
            .await?;
        checked("upload file", response).await.map(drop)
    }

    /// Up to `limit` bytes of the file at `path`, and whether there was more.
    pub async fn download(&self, path: &str, limit: usize) -> Result<(Vec<u8>, bool), Error> {
        let response = self
            .request(Method::GET, &["files", "download"], Duration::ZERO)
            .query(&[("path", path)])
            // One byte past the limit, to tell a file of exactly `limit`
            // bytes from a longer one.
            .header(reqwest::header::RANGE, format!("bytes=0-{limit}"))
            .send()
            .await?;
        // An empty file cannot satisfy any range.
        if response.status() == StatusCode::RANGE_NOT_SATISFIABLE {
            return Ok((Vec::new(), false));
        }
        let response = checked("download file", response).await?;
        let mut content = Vec::new();
        let mut body = response.bytes_stream();
        while let Some(chunk) = body.next().await {
            content.extend_from_slice(&chunk?);
            if content.len() > limit {
                break;
            }
        }
        let more = content.len() > limit;
        content.truncate(limit);
        Ok((content, more))
    }
}

/// Headers the endpoint says execd requests need.
fn header_map(headers: &BTreeMap<String, String>) -> Result<HeaderMap, Error> {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        match (HeaderName::try_from(name), HeaderValue::try_from(value)) {
            (Ok(name), Ok(value)) => map.insert(name, value),
            _ => {
                return Err(Error::Protocol(format!(
                    "execd endpoint header `{name}` is not valid"
                )));
            }
        };
    }
    Ok(map)
}

/// The response, if its status is a success; otherwise an error with a
/// preview of the body.
async fn checked(what: &str, response: Response) -> Result<Response, Error> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    Err(Error::Status {
        what: what.to_string(),
        status: status.as_u16(),
        body: preview(&body),
    })
}

async fn json_body<T: serde::de::DeserializeOwned>(
    what: &str,
    response: Response,
) -> Result<T, Error> {
    let text = checked(what, response).await?.text().await?;
    serde_json::from_str(&text).map_err(|e| {
        Error::Protocol(format!(
            "{what}: unexpected response ({e}): {}",
            preview(&text)
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_headers_are_checked() {
        let good = BTreeMap::from([("x-token".to_string(), "abc".to_string())]);
        assert_eq!(header_map(&good).unwrap()["x-token"], "abc");

        for (name, value) in [("bad name", "v"), ("x-ok", "line\nbreak")] {
            let bad = BTreeMap::from([(name.to_string(), value.to_string())]);
            assert!(matches!(header_map(&bad), Err(Error::Protocol(_))));
        }
    }
}
