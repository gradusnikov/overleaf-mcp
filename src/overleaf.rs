//! Client for the web endpoints of Overleaf Community Edition that the browser editor uses.
//!
//! Overleaf CE has no public API, so this logs in as an ordinary user (session cookie + CSRF
//! token) and calls the same routes as the frontend. The project file tree, with the entity ids
//! needed to delete files or pick the root document, is only sent over the real-time socket, so
//! [`Overleaf::project_tree`] performs a short socket.io 0.9 long-polling handshake.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::header::{ACCEPT, HeaderValue, LOCATION};
use reqwest::multipart::{Form, Part};
use reqwest::{Client, RequestBuilder, Response, StatusCode, redirect};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

const CSRF_HEADER: &str = "X-Csrf-Token";
const SOCKET_TIMEOUT: Duration = Duration::from_secs(20);
const COMPILE_ATTEMPTS: usize = 10;
const COMPILE_RETRY_DELAY: Duration = Duration::from_millis(1500);

pub struct Overleaf {
    base: String,
    email: String,
    password: String,
    http: Client,
    /// CSRF token of the logged-in session; `None` means "log in before the next request".
    csrf: Mutex<Option<String>>,
    /// Output files of the last compile per project, so that they can be fetched later.
    last_compile: Mutex<HashMap<String, Vec<OutputFile>>>,
}

/// The project does not exist or was deleted; a fresh login would not help.
#[derive(Debug)]
pub struct ProjectNotFound;

impl std::fmt::Display for ProjectNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("project not found (check the id with list_projects)")
    }
}

impl std::error::Error for ProjectNotFound {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EntityKind {
    Doc,
    File,
    Folder,
}

impl EntityKind {
    fn route(self) -> &'static str {
        match self {
            EntityKind::Doc => "doc",
            EntityKind::File => "file",
            EntityKind::Folder => "folder",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Entity {
    pub path: String,
    pub kind: EntityKind,
    pub id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectTree {
    pub name: String,
    pub permissions: String,
    pub root_doc: Option<String>,
    #[serde(skip)]
    pub root_folder_id: String,
    #[serde(skip)]
    pub root_doc_id: Option<String>,
    pub entities: Vec<Entity>,
}

impl ProjectTree {
    pub fn find(&self, path: &str) -> Option<&Entity> {
        let path = normalize_path(path);
        self.entities.iter().find(|e| e.path == path)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct OutputFile {
    pub path: String,
    #[serde(skip)]
    pub url: String,
}

#[derive(Debug, Serialize)]
pub struct CompileResult {
    pub status: String,
    pub output_files: Vec<String>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub bad_boxes: usize,
    pub log_tail: String,
}

/// Strips leading slashes so that paths compare the same way as in the file tree.
pub fn normalize_path(path: &str) -> String {
    path.trim().trim_start_matches('/').to_string()
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Extracts `content` of `<meta name="{name}" content="...">` from a page.
fn meta_content(html: &str, name: &str) -> Option<String> {
    let at = html.find(&format!("name=\"{name}\""))?;
    let start = html[..at].rfind('<')?;
    let end = at + html[at..].find('>')?;
    let tag = &html[start..end];
    let value = tag.split("content=\"").nth(1)?.split('"').next()?;
    Some(
        value
            .replace("&amp;", "&")
            .replace("&quot;", "\"")
            .replace("&#x27;", "'"),
    )
}

async fn error_text(resp: Response) -> String {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let body: String = body.chars().take(500).collect();
    format!("HTTP {status}: {body}")
}

impl Overleaf {
    pub fn new(base: &str, email: String, password: String) -> Result<Self> {
        let http = Client::builder()
            .cookie_store(true)
            .redirect(redirect::Policy::none())
            .timeout(Duration::from_secs(600))
            .user_agent(concat!("overleaf-mcp/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            email,
            password,
            http,
            csrf: Mutex::new(None),
            last_compile: Mutex::new(HashMap::new()),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    async fn page_csrf(&self, path: &str) -> Result<String> {
        let resp = self.http.get(self.url(path)).send().await?;
        if !resp.status().is_success() {
            bail!("GET {path}: {}", error_text(resp).await);
        }
        let html = resp.text().await?;
        meta_content(&html, "ol-csrfToken").ok_or_else(|| anyhow!("no CSRF token on {path}"))
    }

    async fn login(&self) -> Result<String> {
        let resp = self.http.get(self.url("/login")).send().await?;
        let csrf = if resp.status().is_redirection() {
            // the cookie jar still holds a valid session
            return self.page_csrf("/project").await;
        } else if resp.status().is_success() {
            let html = resp.text().await?;
            meta_content(&html, "ol-csrfToken").ok_or_else(|| anyhow!("no CSRF token on /login"))?
        } else {
            bail!("GET /login: {}", error_text(resp).await);
        };
        let resp = self
            .http
            .post(self.url("/login"))
            .header(CSRF_HEADER, &csrf)
            .header(ACCEPT, "application/json")
            .json(&json!({ "email": self.email, "password": self.password }))
            .send()
            .await?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() || body.get("redir").is_none() {
            let message = body
                .pointer("/message/text")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| body.to_string());
            bail!("Overleaf login as {} failed (HTTP {status}): {message}", self.email);
        }
        tracing::info!(email = %self.email, "logged in to Overleaf");
        // the session is regenerated on login, so take the token of the new session
        self.page_csrf("/project").await
    }

    async fn csrf_token(&self) -> Result<String> {
        let mut guard = self.csrf.lock().await;
        if let Some(token) = guard.as_ref() {
            return Ok(token.clone());
        }
        let token = self.login().await?;
        *guard = Some(token.clone());
        Ok(token)
    }

    async fn invalidate_session(&self) {
        *self.csrf.lock().await = None;
    }

    fn lost_session(resp: &Response) -> bool {
        match resp.status() {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => true,
            s if s.is_redirection() => resp
                .headers()
                .get(LOCATION)
                .and_then(|l| l.to_str().ok())
                .is_some_and(|l| l.contains("/login")),
            _ => false,
        }
    }

    /// Sends a request built by `build`, logging in first and once more if the session expired.
    async fn send<F>(&self, build: F) -> Result<Response>
    where
        F: Fn(&Client, &str) -> RequestBuilder,
    {
        for attempt in 0..2 {
            let csrf = self.csrf_token().await?;
            let resp = build(&self.http, &csrf)
                .header(CSRF_HEADER, &csrf)
                .header(ACCEPT, HeaderValue::from_static("application/json, */*"))
                .send()
                .await?;
            if attempt == 0 && Self::lost_session(&resp) {
                tracing::info!(status = %resp.status(), "Overleaf session lost, logging in again");
                self.invalidate_session().await;
                continue;
            }
            return Ok(resp);
        }
        unreachable!()
    }

    async fn send_ok<F>(&self, what: &str, build: F) -> Result<Response>
    where
        F: Fn(&Client, &str) -> RequestBuilder,
    {
        let resp = self.send(build).await?;
        if !resp.status().is_success() {
            bail!("{what}: {}", error_text(resp).await);
        }
        Ok(resp)
    }

    async fn json_ok<F>(&self, what: &str, build: F) -> Result<Value>
    where
        F: Fn(&Client, &str) -> RequestBuilder,
    {
        let resp = self.send_ok(what, build).await?;
        let text = resp.text().await?;
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).with_context(|| format!("{what}: unexpected response"))
    }

    pub async fn list_projects(&self, search: Option<&str>, include_archived: bool) -> Result<Value> {
        let mut filters = json!({});
        if let Some(search) = search.filter(|s| !s.is_empty()) {
            filters["search"] = json!(search);
        }
        let body = json!({
            "filters": filters,
            "page": { "size": 200 },
            "sort": { "by": "lastUpdated", "order": "desc" },
        });
        let page = self
            .json_ok("list projects", |c, _| c.post(self.url("/api/project")).json(&body))
            .await?;
        let projects = page["projects"].as_array().cloned().unwrap_or_default();
        let list: Vec<Value> = projects
            .into_iter()
            .filter(|p| p["trashed"] != json!(true))
            .filter(|p| include_archived || p["archived"] != json!(true))
            .map(|p| {
                json!({
                    "id": p["id"],
                    "name": p["name"],
                    "access": p["accessLevel"],
                    "owner": p["owner"]["email"],
                    "last_updated": p["lastUpdated"],
                    "archived": p["archived"],
                })
            })
            .collect();
        Ok(json!({ "total": page["totalSize"], "projects": list }))
    }

    pub async fn create_project(&self, name: &str, example: bool) -> Result<String> {
        let body = json!({ "projectName": name, "template": if example { "example" } else { "none" } });
        let resp = self
            .json_ok("create project", |c, _| c.post(self.url("/project/new")).json(&body))
            .await?;
        resp["project_id"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("create project: no project_id in {resp}"))
    }

    /// Fetches the project file tree from the real-time service (socket.io 0.9, xhr-polling).
    pub async fn project_tree(&self, project_id: &str) -> Result<ProjectTree> {
        match self.join_project(project_id).await {
            Ok(tree) => Ok(tree),
            Err(e) if e.is::<ProjectNotFound>() => Err(e),
            Err(e) => {
                // most likely an expired session: the socket handshake does not redirect
                tracing::info!(error = %e, "joinProject failed, retrying after a fresh login");
                self.invalidate_session().await;
                self.join_project(project_id).await
            }
        }
    }

    async fn join_project(&self, project_id: &str) -> Result<ProjectTree> {
        self.csrf_token().await?;
        let query = [("projectId", project_id.to_string())];
        let resp = self
            .http
            .get(self.url("/socket.io/1/"))
            .query(&query)
            .query(&[("t", now_millis().to_string())])
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("socket.io handshake: {}", error_text(resp).await);
        }
        let handshake = resp.text().await?;
        let sid = handshake
            .split(':')
            .next()
            .filter(|s| !s.is_empty() && handshake.contains("xhr-polling"))
            .ok_or_else(|| anyhow!("socket.io handshake: unexpected answer {handshake:?}"))?
            .to_string();
        let poll_url = self.url(&format!("/socket.io/1/xhr-polling/{sid}"));
        let result = self.poll_join_response(&poll_url, &query).await;
        let _ = self
            .http
            .get(&poll_url)
            .query(&query)
            .query(&[("disconnect", "1")])
            .send()
            .await;
        let (project, permissions) = result?;
        parse_tree(&project, permissions)
    }

    async fn poll_join_response(&self, poll_url: &str, query: &[(&str, String)]) -> Result<(Value, String)> {
        let deadline = Instant::now() + SOCKET_TIMEOUT;
        while Instant::now() < deadline {
            let resp = self
                .http
                .get(poll_url)
                .query(query)
                .query(&[("t", now_millis().to_string())])
                .send()
                .await?;
            if !resp.status().is_success() {
                bail!("socket.io poll: {}", error_text(resp).await);
            }
            let payload = resp.text().await?;
            for frame in split_frames(&payload) {
                let mut parts = frame.splitn(4, ':');
                let kind = parts.next().unwrap_or_default();
                match kind {
                    "2" => {
                        let _ = self.http.post(poll_url).query(query).body("2::").send().await;
                    }
                    "5" => {
                        let data = parts.nth(2).unwrap_or_default();
                        let event: Value = serde_json::from_str(data).unwrap_or(Value::Null);
                        match event["name"].as_str() {
                            Some("joinProjectResponse") => {
                                let args = &event["args"][0];
                                let permissions = args["permissionsLevel"].as_str().unwrap_or("unknown").to_string();
                                return Ok((args["project"].clone(), permissions));
                            }
                            Some("connectionRejected") => {
                                if event["args"][0]["code"] == json!("ProjectNotFound") {
                                    return Err(ProjectNotFound.into());
                                }
                                bail!("access to the project was rejected: {}", event["args"][0])
                            }
                            _ => {}
                        }
                    }
                    "0" => bail!("socket.io: server closed the connection"),
                    "7" => bail!("socket.io error: {frame}"),
                    _ => {}
                }
            }
        }
        bail!("socket.io: no joinProjectResponse within {SOCKET_TIMEOUT:?}")
    }

    pub async fn read_doc(&self, project_id: &str, doc_id: &str) -> Result<String> {
        let resp = self
            .send_ok("read document", |c, _| {
                c.get(self.url(&format!("/Project/{project_id}/doc/{doc_id}/download")))
            })
            .await?;
        Ok(resp.text().await?)
    }

    pub async fn read_file(&self, project_id: &str, file_id: &str) -> Result<Vec<u8>> {
        let resp = self
            .send_ok("read file", |c, _| {
                c.get(self.url(&format!("/Project/{project_id}/file/{file_id}")))
            })
            .await?;
        Ok(resp.bytes().await?.to_vec())
    }

    /// Creates or replaces the file at `path`, creating missing folders.
    pub async fn write_file(
        &self,
        project_id: &str,
        root_folder_id: &str,
        path: &str,
        content: Vec<u8>,
    ) -> Result<Value> {
        let path = normalize_path(path);
        let name = path.rsplit('/').next().unwrap_or_default().to_string();
        if name.is_empty() {
            bail!("invalid file path {path:?}");
        }
        let nested = path.contains('/');
        let resp = self
            .send(|c, _| {
                let mut form = Form::new().text("name", name.clone());
                if nested {
                    form = form.text("relativePath", path.clone());
                }
                let part = Part::bytes(content.clone()).file_name(name.clone());
                c.post(self.url(&format!("/Project/{project_id}/upload")))
                    .query(&[("folder_id", root_folder_id)])
                    .multipart(form.part("qqfile", part))
            })
            .await?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() || body["success"] != json!(true) {
            bail!("upload {path}: HTTP {status}: {body}");
        }
        Ok(json!({ "path": path, "type": body["entity_type"], "id": body["entity_id"] }))
    }

    pub async fn delete_entity(&self, project_id: &str, entity: &Entity) -> Result<()> {
        let route = entity.kind.route();
        let id = &entity.id;
        self.send_ok("delete", |c, _| {
            c.delete(self.url(&format!("/project/{project_id}/{route}/{id}")))
        })
        .await?;
        Ok(())
    }

    pub async fn set_root_doc(&self, project_id: &str, doc_id: &str) -> Result<()> {
        let body = json!({ "rootDocId": doc_id });
        self.send_ok("set main document", |c, _| {
            c.post(self.url(&format!("/project/{project_id}/settings"))).json(&body)
        })
        .await?;
        Ok(())
    }

    pub async fn compile(
        &self,
        project_id: &str,
        root_doc_id: Option<&str>,
        draft: bool,
        stop_on_first_error: bool,
    ) -> Result<CompileResult> {
        let mut body = json!({
            "check": "silent",
            "draft": draft,
            "incrementalCompilesEnabled": true,
            "stopOnFirstError": stop_on_first_error,
        });
        if let Some(id) = root_doc_id {
            body["rootDoc_id"] = json!(id);
        }
        // Overleaf refuses a compile within a second of the previous one, and while one is running
        let mut attempts = 0;
        let resp = loop {
            let resp = self
                .json_ok("compile", |c, _| {
                    c.post(self.url(&format!("/project/{project_id}/compile")))
                        .query(&[("auto_compile", "false"), ("file_line_errors", "true")])
                        .json(&body)
                })
                .await?;
            let busy = matches!(
                resp["status"].as_str(),
                Some("too-recently-compiled" | "compile-in-progress")
            );
            attempts += 1;
            if !busy || attempts == COMPILE_ATTEMPTS {
                break resp;
            }
            tokio::time::sleep(COMPILE_RETRY_DELAY).await;
        };
        let status = resp["status"].as_str().unwrap_or("unknown").to_string();
        let files: Vec<OutputFile> = resp["outputFiles"]
            .as_array()
            .map(|files| {
                files
                    .iter()
                    .filter_map(|f| {
                        Some(OutputFile {
                            path: f["path"].as_str()?.to_string(),
                            url: f["url"].as_str()?.to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let log = match files.iter().find(|f| f.path == "output.log") {
            Some(f) => String::from_utf8_lossy(&self.fetch_output(&f.url).await?).into_owned(),
            None => String::new(),
        };
        let mut result = parse_log(&log);
        result.status = status;
        result.output_files = files.iter().map(|f| f.path.clone()).collect();
        if let Some(problems) = resp.get("validationProblems").filter(|v| !v.is_null()) {
            result.errors.insert(0, format!("validation problems: {problems}"));
        }
        // a refused compile has no outputs; keep those of the previous one
        if !files.is_empty() {
            self.last_compile.lock().await.insert(project_id.to_string(), files);
        }
        Ok(result)
    }

    async fn fetch_output(&self, url: &str) -> Result<Vec<u8>> {
        let url = if url.starts_with("http") {
            url.to_string()
        } else {
            self.url(url)
        };
        let resp = self.send_ok("fetch compile output", |c, _| c.get(&url)).await?;
        Ok(resp.bytes().await?.to_vec())
    }

    /// Returns an output file (`output.pdf`, `output.log`, ...) of the last compile of the project.
    pub async fn compile_output(&self, project_id: &str, file: &str) -> Result<Vec<u8>> {
        let url = {
            let compiles = self.last_compile.lock().await;
            let files = compiles
                .get(project_id)
                .ok_or_else(|| anyhow!("the project was not compiled by this server yet; run compile first"))?;
            let found = files.iter().find(|f| f.path == file).ok_or_else(|| {
                let names: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
                anyhow!("{file} is not among the output files of the last compile: {names:?}")
            })?;
            found.url.clone()
        };
        self.fetch_output(&url).await
    }

    pub async fn invite(&self, project_id: &str, email: &str, read_only: bool) -> Result<Value> {
        let body = json!({ "email": email, "privileges": if read_only { "readOnly" } else { "readAndWrite" } });
        self.json_ok("invite collaborator", |c, _| {
            c.post(self.url(&format!("/project/{project_id}/invite"))).json(&body)
        })
        .await
    }

    /// Makes the collaborator with this e-mail address the owner; Overleaf keeps the previous owner as a
    /// collaborator. Overleaf only transfers to users who are already collaborators (unless an admin asks).
    pub async fn transfer_ownership(&self, project_id: &str, email: &str) -> Result<Value> {
        let members = self
            .json_ok("list project members", |c, _| {
                c.get(self.url(&format!("/project/{project_id}/members")))
            })
            .await?;
        let email = email.trim();
        let member = members["members"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|m| m["email"].as_str().is_some_and(|e| e.eq_ignore_ascii_case(email)))
            .ok_or_else(|| {
                anyhow!(
                    "{email} is not a collaborator of this project. Only the owner (access \"owner\" in \
                     list_projects) can transfer a project, and only to a collaborator: invite them with \
                     invite_collaborator and transfer the project after they have accepted"
                )
            })?;
        let user_id = member["_id"].as_str().context("project member without an id")?;
        let body = json!({ "user_id": user_id });
        self.send_ok("transfer ownership", |c, _| {
            c.post(self.url(&format!("/project/{project_id}/transfer-ownership")))
                .json(&body)
        })
        .await?;
        Ok(json!({ "project_id": project_id, "new_owner": member["email"] }))
    }

    /// Accepts all pending invitations to other users' projects.
    pub async fn accept_invites(&self) -> Result<Vec<Value>> {
        let notifications = self
            .json_ok("list notifications", |c, _| c.get(self.url("/notifications")))
            .await?;
        let mut accepted = Vec::new();
        for n in notifications.as_array().into_iter().flatten() {
            if n["templateKey"] != json!("notification_project_invite") {
                continue;
            }
            let opts = &n["messageOpts"];
            let (Some(project_id), Some(token)) = (opts["projectId"].as_str(), opts["token"].as_str()) else {
                continue;
            };
            let resp = self
                .send(|c, _| c.post(self.url(&format!("/project/{project_id}/invite/token/{token}/accept"))))
                .await?;
            let ok = resp.status().is_success() || resp.status().is_redirection();
            if ok && let Some(id) = n["_id"].as_str() {
                let _ = self
                    .send(|c, _| c.delete(self.url(&format!("/notifications/{id}"))))
                    .await;
            }
            accepted.push(json!({
                "project_id": project_id,
                "project": opts["projectName"],
                "from": opts["userName"],
                "accepted": ok,
            }));
        }
        Ok(accepted)
    }
}

/// Splits a socket.io 0.9 payload; several messages are framed as `\u{fffd}len\u{fffd}msg...`.
fn split_frames(payload: &str) -> Vec<&str> {
    if payload.starts_with('\u{fffd}') {
        payload.split('\u{fffd}').skip(1).skip(1).step_by(2).collect()
    } else {
        vec![payload]
    }
}

fn parse_tree(project: &Value, permissions: String) -> Result<ProjectTree> {
    let root = project["rootFolder"]
        .get(0)
        .ok_or_else(|| anyhow!("joinProject response without rootFolder"))?;
    let root_folder_id = root["_id"]
        .as_str()
        .ok_or_else(|| anyhow!("root folder without _id"))?
        .to_string();
    let mut entities = Vec::new();
    walk_folder(root, "", &mut entities);
    entities.sort_by(|a, b| a.path.cmp(&b.path));
    let root_doc_id = project["rootDoc_id"].as_str().map(str::to_string);
    let root_doc = root_doc_id
        .as_ref()
        .and_then(|id| entities.iter().find(|e| &e.id == id))
        .map(|e| e.path.clone());
    Ok(ProjectTree {
        name: project["name"].as_str().unwrap_or_default().to_string(),
        permissions,
        root_doc,
        root_folder_id,
        root_doc_id,
        entities,
    })
}

fn walk_folder(folder: &Value, prefix: &str, out: &mut Vec<Entity>) {
    let children = [
        ("docs", EntityKind::Doc),
        ("fileRefs", EntityKind::File),
        ("folders", EntityKind::Folder),
    ];
    for (key, kind) in children {
        for child in folder[key].as_array().into_iter().flatten() {
            let (Some(name), Some(id)) = (child["name"].as_str(), child["_id"].as_str()) else {
                continue;
            };
            let path = format!("{prefix}{name}");
            if kind == EntityKind::Folder {
                walk_folder(child, &format!("{path}/"), out);
            }
            out.push(Entity {
                path,
                kind,
                id: id.to_string(),
            });
        }
    }
}

/// TeX breaks log lines at `max_print_line` characters (79 by default).
const TEX_LOG_WIDTH: usize = 79;

/// Joins the log lines TeX broke at [`TEX_LOG_WIDTH`] back into whole lines. A line that is an exact
/// multiple of the width ends with an empty line in the log, which is absorbed.
fn unwrap_log(log: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut continued = false;
    for line in log.lines() {
        match lines.last_mut() {
            Some(last) if continued => last.push_str(line),
            _ => lines.push(line.to_string()),
        }
        continued = line.chars().count() == TEX_LOG_WIDTH;
    }
    lines
}

/// Picks errors, LaTeX/package warnings and the number of over/underfull boxes out of a TeX log.
fn parse_log(log: &str) -> CompileResult {
    const MAX: usize = 30;
    let lines = unwrap_log(log);
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut bad_boxes = 0;
    for (i, line) in lines.iter().enumerate() {
        let file_line_error = line
            .split_once(':')
            .and_then(|(_, rest)| rest.split_once(": "))
            .is_some_and(|(num, _)| !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()));
        if line.starts_with('!') || (file_line_error && !line.starts_with(' ')) {
            if errors.len() < MAX {
                // TeX prints the offending source line as "l.<n> ..." shortly after the message
                let context = lines[i + 1..lines.len().min(i + 8)]
                    .iter()
                    .find(|l| l.starts_with("l."))
                    .map(|l| format!(" [{}]", l.trim()))
                    .unwrap_or_default();
                errors.push(format!("{}{}", line.trim(), context));
            }
        } else if line.contains("Warning:") {
            if warnings.len() < MAX {
                warnings.push(line.trim().to_string());
            }
        } else if line.starts_with("Overfull \\") || line.starts_with("Underfull \\") {
            bad_boxes += 1;
        }
    }
    let raw: Vec<&str> = log.lines().collect();
    let tail_start = raw.len().saturating_sub(25);
    CompileResult {
        status: String::new(),
        output_files: Vec::new(),
        errors,
        warnings,
        bad_boxes,
        log_tail: raw[tail_start..].join("\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_is_extracted() {
        let html = r#"<head><meta name="ol-csrfToken" content="abc-123"><meta name="x" content="y"></head>"#;
        assert_eq!(meta_content(html, "ol-csrfToken").as_deref(), Some("abc-123"));
    }

    #[test]
    fn frames_are_split() {
        assert_eq!(split_frames("1::"), vec!["1::"]);
        let payload = "\u{fffd}3\u{fffd}1::\u{fffd}9\u{fffd}5:::{\"a\":1}";
        assert_eq!(split_frames(payload), vec!["1::", "5:::{\"a\":1}"]);
    }

    #[test]
    fn tree_is_flattened() {
        let project = json!({
            "name": "p",
            "rootDoc_id": "d1",
            "rootFolder": [{
                "_id": "root",
                "docs": [{ "_id": "d1", "name": "main.tex" }],
                "fileRefs": [{ "_id": "f1", "name": "logo.png" }],
                "folders": [{ "_id": "s", "name": "sec", "docs": [{ "_id": "d2", "name": "a.tex" }], "fileRefs": [], "folders": [] }],
            }],
        });
        let tree = parse_tree(&project, "owner".into()).unwrap();
        assert_eq!(tree.root_folder_id, "root");
        assert_eq!(tree.root_doc.as_deref(), Some("main.tex"));
        assert_eq!(tree.find("/sec/a.tex").unwrap().id, "d2");
        assert_eq!(tree.find("sec").unwrap().kind, EntityKind::Folder);
    }

    #[test]
    fn log_errors_are_found() {
        let log = "This is pdfTeX\n./main.tex:5: Undefined control sequence.\nl.5 \\foo\n\nLaTeX Warning: Reference `x' on page 1 undefined.\nOverfull \\hbox (1.0pt too wide)\n! Emergency stop.\n";
        let r = parse_log(log);
        assert_eq!(
            r.errors,
            vec![
                "./main.tex:5: Undefined control sequence. [l.5 \\foo]",
                "! Emergency stop."
            ]
        );
        assert_eq!(r.warnings.len(), 1);
        assert_eq!(r.bad_boxes, 1);
    }

    #[test]
    fn wrapped_log_lines_are_joined() {
        // from a real TeX Live 2017 log: the message continues on the next line(s) after 79 characters
        let log = "!pdfTeX error: pdflatex (file /var/lib/overleaf/data/compiles/6ade4c0de193f861a\n\
                   aec4540-6ade485bd42a28a7053304d9/seq.pdf): reading image file failed\n \
                   ==> Fatal error occurred, no output PDF file produced!\n";
        assert_eq!(
            parse_log(log).errors,
            vec![
                "!pdfTeX error: pdflatex (file /var/lib/overleaf/data/compiles/6ade4c0de193f861a\
                 aec4540-6ade485bd42a28a7053304d9/seq.pdf): reading image file failed"
            ]
        );
        let long = "x".repeat(2 * TEX_LOG_WIDTH);
        let wrapped = format!("{}\n{}\n\nnext", &long[..TEX_LOG_WIDTH], &long[TEX_LOG_WIDTH..]);
        assert_eq!(unwrap_log(&wrapped), vec![long.as_str(), "next"]);
        let wrapped = format!("{}\nshort\n\nnext", &long[..TEX_LOG_WIDTH]);
        assert_eq!(unwrap_log(&wrapped).len(), 3);
    }
}
