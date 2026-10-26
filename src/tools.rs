//! MCP tool definitions, built with the `rmcp` tool macros.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ResourceContents, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::json;

use crate::lines::LineFilter;
use crate::overleaf::{Entity, EntityKind, Overleaf, TREE_TTL, normalize_path};

const INSTRUCTIONS: &str = "Tools for an Overleaf (ShareLaTeX) server. Projects are identified by the id from \
list_projects or create_project, files by their path inside the project (e.g. main.tex, chapters/intro.tex). \
The server acts as one Overleaf user: it sees its own projects and projects shared with it (call accept_invites \
after someone shares a project with it, invite_collaborator to give a person access to a project it created, and \
transfer_ownership to hand such a project over to a person who has accepted the invitation). \
Typical loop: write_file/edit_file, then compile and fix the reported errors; fetch the PDF with \
get_compile_output. Edits are applied like an upload, so people with the project open see them immediately.";

#[derive(Clone)]
pub struct OverleafMcp {
    overleaf: Arc<Overleaf>,
    tool_router: ToolRouter<Self>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ListProjectsArgs {
    /// Only projects whose name contains this text.
    pub search: Option<String>,
    /// Include archived projects (default false).
    pub include_archived: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct CreateProjectArgs {
    /// Project name; Overleaf appends a number if the name is taken.
    pub name: String,
    /// Start from Overleaf's example project instead of a minimal main.tex (default false).
    pub example: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProjectArgs {
    /// Project id (24 hex characters).
    pub project_id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct PathArgs {
    /// Project id (24 hex characters).
    pub project_id: String,
    /// Path of the file inside the project, e.g. "chapters/intro.tex".
    pub path: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct WriteFileArgs {
    /// Project id (24 hex characters).
    pub project_id: String,
    /// Path of the file inside the project; missing folders are created.
    pub path: String,
    /// New file content (UTF-8 text, or base64 when `base64` is true).
    pub content: String,
    /// Set for binary files such as images: `content` is then base64-encoded.
    pub base64: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct EditFileArgs {
    /// Project id (24 hex characters).
    pub project_id: String,
    /// Path of the text file inside the project.
    pub path: String,
    /// Exact text to replace; must occur exactly once unless `replace_all` is set.
    pub old_string: String,
    /// Replacement text.
    pub new_string: String,
    /// Replace every occurrence (default false).
    pub replace_all: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct CompileArgs {
    /// Project id (24 hex characters).
    pub project_id: String,
    /// Main .tex file to compile (default: the project's main document).
    pub main_file: Option<String>,
    /// Draft mode: faster, images are replaced by boxes (default false).
    pub draft: Option<bool>,
    /// Stop at the first error (default false).
    pub stop_on_first_error: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct CompileOutputArgs {
    /// Project id (24 hex characters).
    pub project_id: String,
    /// Output file name from the compile result (default "output.pdf"), e.g. "output.log" or "output.blg".
    pub file: Option<String>,
    /// Text files only: return just the lines matching this regular expression, e.g.
    /// "^!|\\.tex:\\d+:|Warning" for errors and warnings; prefix "(?i)" to ignore case.
    pub pattern: Option<String>,
    /// Text files only: lines of context before and after each match (default 0, at most 20).
    pub context: Option<u32>,
    /// Text files only: skip this many of the selected lines; negative counts from the end (-50 = last 50).
    pub offset: Option<i64>,
    /// Text files only: maximum number of lines returned (default 400, at most 5000).
    pub limit: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct InviteArgs {
    /// Project id (24 hex characters).
    pub project_id: String,
    /// E-mail address of the person to invite.
    pub email: String,
    /// Give read-only access instead of edit access (default false).
    pub read_only: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TransferOwnershipArgs {
    /// Project id (24 hex characters).
    pub project_id: String,
    /// E-mail address of the new owner: a registered user who is already a collaborator of the project.
    pub email: String,
}

fn check_project_id(id: &str) -> Result<()> {
    if id.len() == 24 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        bail!("invalid project id {id:?}: expected 24 hex characters")
    }
}

fn to_result(result: Result<Vec<ContentBlock>>) -> CallToolResult {
    match result {
        Ok(content) => CallToolResult::success(content),
        Err(e) => {
            tracing::warn!(error = format!("{e:#}"), "tool failed");
            CallToolResult::error(vec![ContentBlock::text(format!("{e:#}"))])
        }
    }
}

fn json_block(value: impl serde::Serialize) -> Result<Vec<ContentBlock>> {
    Ok(vec![ContentBlock::text(serde_json::to_string_pretty(&value)?)])
}

fn image_mime(path: &str) -> Option<&'static str> {
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// Content of a project file as Overleaf stores it.
enum FileContent {
    Text(String),
    Bytes(Vec<u8>),
}

fn blob(uri: String, mime: &str, bytes: &[u8]) -> ContentBlock {
    ContentBlock::resource(ResourceContents::BlobResourceContents {
        uri,
        mime_type: Some(mime.to_string()),
        blob: BASE64.encode(bytes),
        meta: None,
    })
}

impl OverleafMcp {
    pub fn new(overleaf: Arc<Overleaf>) -> Self {
        Self {
            overleaf,
            tool_router: Self::tool_router(),
        }
    }

    /// Looks `path` up in the cached file tree, fetching the tree again when the path is not in it.
    async fn entity(&self, project_id: &str, path: &str) -> Result<Entity> {
        let started = Instant::now();
        let tree = self.overleaf.project_tree(project_id, TREE_TTL).await?;
        if let Some(entity) = tree.find(path) {
            return Ok(entity.clone());
        }
        let tree = if tree.fetched >= started {
            tree
        } else {
            self.overleaf.project_tree(project_id, Duration::ZERO).await?
        };
        tree.find(path)
            .cloned()
            .ok_or_else(|| anyhow!("no file {path:?} in the project"))
    }

    async fn read_entity(&self, project_id: &str, entity: &Entity) -> Result<FileContent> {
        match entity.kind {
            EntityKind::Doc => Ok(FileContent::Text(self.overleaf.read_doc(project_id, &entity.id).await?)),
            EntityKind::File => Ok(FileContent::Bytes(
                self.overleaf.read_file(project_id, &entity.id).await?,
            )),
            EntityKind::Folder => bail!("{} is a folder; use list_files", entity.path),
        }
    }

    /// Reads the file at `path`. The cached tree may be out of date when someone else replaced or deleted the
    /// file, so a failed read is tried once more with a fresh tree.
    async fn read(&self, project_id: &str, path: &str) -> Result<(Entity, FileContent)> {
        let entity = self.entity(project_id, path).await?;
        let error = match self.read_entity(project_id, &entity).await {
            Ok(content) => return Ok((entity, content)),
            Err(e) if entity.kind == EntityKind::Folder => return Err(e),
            Err(e) => e,
        };
        let tree = self.overleaf.project_tree(project_id, Duration::ZERO).await?;
        match tree.find(path) {
            Some(fresh) if fresh.id != entity.id => {
                let content = self.read_entity(project_id, fresh).await?;
                Ok((fresh.clone(), content))
            }
            Some(_) => Err(error),
            None => bail!("no file {path:?} in the project"),
        }
    }

    async fn read_text(&self, project_id: &str, path: &str) -> Result<String> {
        match self.read(project_id, path).await? {
            (_, FileContent::Text(text)) => Ok(text),
            (_, FileContent::Bytes(bytes)) => String::from_utf8(bytes).map_err(|_| anyhow!("{path} is a binary file")),
        }
    }

    async fn do_read_file(&self, a: PathArgs) -> Result<Vec<ContentBlock>> {
        check_project_id(&a.project_id)?;
        let (entity, bytes) = match self.read(&a.project_id, &a.path).await? {
            (_, FileContent::Text(text)) => return Ok(vec![ContentBlock::text(text)]),
            (entity, FileContent::Bytes(bytes)) => (entity, bytes),
        };
        if let Some(mime) = image_mime(&entity.path) {
            return Ok(vec![ContentBlock::image(BASE64.encode(&bytes), mime)]);
        }
        match String::from_utf8(bytes) {
            Ok(text) => Ok(vec![ContentBlock::text(text)]),
            Err(e) => {
                let uri = format!("overleaf://{}/{}", a.project_id, entity.path);
                Ok(vec![blob(uri, "application/octet-stream", e.as_bytes())])
            }
        }
    }

    async fn do_write_file(&self, a: WriteFileArgs) -> Result<Vec<ContentBlock>> {
        check_project_id(&a.project_id)?;
        let content = if a.base64.unwrap_or(false) {
            BASE64
                .decode(a.content.trim())
                .map_err(|e| anyhow!("content is not valid base64: {e}"))?
        } else {
            a.content.into_bytes()
        };
        let tree = self.overleaf.project_tree(&a.project_id, TREE_TTL).await?;
        let written = self
            .overleaf
            .write_file(&a.project_id, &tree.root_folder_id, &a.path, content)
            .await?;
        json_block(written)
    }

    async fn do_edit_file(&self, a: EditFileArgs) -> Result<Vec<ContentBlock>> {
        check_project_id(&a.project_id)?;
        if a.old_string.is_empty() {
            bail!("old_string must not be empty");
        }
        let text = self.read_text(&a.project_id, &a.path).await?;
        let count = text.matches(&a.old_string).count();
        let replace_all = a.replace_all.unwrap_or(false);
        if count == 0 {
            bail!("old_string was not found in {}", a.path);
        }
        if count > 1 && !replace_all {
            bail!(
                "old_string occurs {count} times in {}; add context or set replace_all",
                a.path
            );
        }
        let updated = if replace_all {
            text.replace(&a.old_string, &a.new_string)
        } else {
            text.replacen(&a.old_string, &a.new_string, 1)
        };
        let tree = self.overleaf.project_tree(&a.project_id, TREE_TTL).await?;
        self.overleaf
            .write_file(&a.project_id, &tree.root_folder_id, &a.path, updated.into_bytes())
            .await?;
        json_block(json!({ "path": normalize_path(&a.path), "replacements": if replace_all { count } else { 1 } }))
    }

    async fn do_delete_file(&self, a: PathArgs) -> Result<Vec<ContentBlock>> {
        check_project_id(&a.project_id)?;
        let entity = self.entity(&a.project_id, &a.path).await?;
        self.overleaf.delete_entity(&a.project_id, &entity).await?;
        json_block(json!({ "deleted": entity.path, "type": entity.kind }))
    }

    async fn do_set_main_file(&self, a: PathArgs) -> Result<Vec<ContentBlock>> {
        check_project_id(&a.project_id)?;
        let entity = self.entity(&a.project_id, &a.path).await?;
        if entity.kind != EntityKind::Doc {
            bail!("{} is not an editable document", a.path);
        }
        self.overleaf.set_root_doc(&a.project_id, &entity.id).await?;
        json_block(json!({ "main_file": entity.path }))
    }

    async fn do_compile(&self, a: CompileArgs) -> Result<Vec<ContentBlock>> {
        check_project_id(&a.project_id)?;
        // without a main file Overleaf compiles the project's main document
        let root_doc_id = match a.main_file.as_deref() {
            Some(path) => {
                let entity = self.entity(&a.project_id, path).await?;
                if entity.kind != EntityKind::Doc {
                    bail!("{path} is not a .tex document");
                }
                Some(entity.id)
            }
            None => None,
        };
        let result = self
            .overleaf
            .compile(
                &a.project_id,
                root_doc_id.as_deref(),
                a.draft.unwrap_or(false),
                a.stop_on_first_error.unwrap_or(false),
            )
            .await?;
        json_block(result)
    }

    async fn do_compile_output(&self, a: CompileOutputArgs) -> Result<Vec<ContentBlock>> {
        check_project_id(&a.project_id)?;
        let file = a.file.unwrap_or_else(|| "output.pdf".to_string());
        let filter = LineFilter::new(a.pattern.as_deref(), a.context, a.offset, a.limit)?;
        let filtering = filter.pattern.is_some() || a.context.is_some() || a.offset.is_some() || a.limit.is_some();
        let bytes = self.overleaf.compile_output(&a.project_id, &file).await?;
        let uri = format!("overleaf://{}/output/{file}", a.project_id);
        let binary = file.ends_with(".pdf") || file.ends_with(".gz") || image_mime(&file).is_some();
        if binary && filtering {
            bail!("pattern, context, offset and limit apply to text files only, not {file}");
        }
        if file.ends_with(".pdf") {
            return Ok(vec![
                ContentBlock::text(format!("{file}: {} bytes", bytes.len())),
                blob(uri, "application/pdf", &bytes),
            ]);
        }
        if let Some(mime) = image_mime(&file) {
            return Ok(vec![ContentBlock::image(BASE64.encode(&bytes), mime)]);
        }
        if binary {
            return Ok(vec![blob(uri, "application/octet-stream", &bytes)]);
        }
        // TeX writes 8-bit input bytes to the log unchanged, so logs are often not valid UTF-8
        let text = String::from_utf8_lossy(&bytes);
        Ok(vec![ContentBlock::text(filter.apply(&file, &text))])
    }
}

#[tool_router]
impl OverleafMcp {
    #[tool(
        description = "List the Overleaf projects this server's user owns or has access to.",
        annotations(read_only_hint = true)
    )]
    async fn list_projects(&self, Parameters(a): Parameters<ListProjectsArgs>) -> CallToolResult {
        let result = self
            .overleaf
            .list_projects(a.search.as_deref(), a.include_archived.unwrap_or(false))
            .await;
        to_result(result.and_then(json_block))
    }

    #[tool(
        description = "Create a new project with a minimal main.tex (or Overleaf's example project). Returns its id."
    )]
    async fn create_project(&self, Parameters(a): Parameters<CreateProjectArgs>) -> CallToolResult {
        let result = self.overleaf.create_project(&a.name, a.example.unwrap_or(false)).await;
        to_result(result.and_then(|id| json_block(json!({ "project_id": id }))))
    }

    #[tool(
        description = "List all files and folders of a project with their type (doc = editable text, file = binary/uploaded) and the main document. Files carry a hash, the SHA-1 that `git hash-object` gives for their content, so unchanged files need not be read again.",
        annotations(read_only_hint = true)
    )]
    async fn list_files(&self, Parameters(a): Parameters<ProjectArgs>) -> CallToolResult {
        let result = async {
            check_project_id(&a.project_id)?;
            json_block(self.overleaf.project_tree(&a.project_id, Duration::ZERO).await?)
        };
        to_result(result.await)
    }

    #[tool(
        description = "Download a whole project as a zip archive (all files in their folders). Use it to copy a project instead of reading the files one by one.",
        annotations(read_only_hint = true)
    )]
    async fn download_project(&self, Parameters(a): Parameters<ProjectArgs>) -> CallToolResult {
        let result = async {
            check_project_id(&a.project_id)?;
            let zip = self.overleaf.download_zip(&a.project_id).await?;
            let uri = format!("overleaf://{}/project.zip", a.project_id);
            Ok(vec![
                ContentBlock::text(format!("project.zip: {} bytes", zip.len())),
                blob(uri, "application/zip", &zip),
            ])
        };
        to_result(result.await)
    }

    #[tool(
        description = "Read a file of a project. Text is returned as is, images as images.",
        annotations(read_only_hint = true)
    )]
    async fn read_file(&self, Parameters(a): Parameters<PathArgs>) -> CallToolResult {
        to_result(self.do_read_file(a).await)
    }

    #[tool(
        description = "Create or overwrite a file in a project (missing folders are created). For binary files pass base64 content and set base64=true."
    )]
    async fn write_file(&self, Parameters(a): Parameters<WriteFileArgs>) -> CallToolResult {
        to_result(self.do_write_file(a).await)
    }

    #[tool(
        description = "Replace an exact piece of text in a text file of a project. old_string must match exactly once unless replace_all is set."
    )]
    async fn edit_file(&self, Parameters(a): Parameters<EditFileArgs>) -> CallToolResult {
        to_result(self.do_edit_file(a).await)
    }

    #[tool(
        description = "Delete a file or folder (with its content) from a project.",
        annotations(destructive_hint = true)
    )]
    async fn delete_file(&self, Parameters(a): Parameters<PathArgs>) -> CallToolResult {
        to_result(self.do_delete_file(a).await)
    }

    #[tool(description = "Make a .tex document the project's main document (the one compiled by default).")]
    async fn set_main_file(&self, Parameters(a): Parameters<PathArgs>) -> CallToolResult {
        to_result(self.do_set_main_file(a).await)
    }

    #[tool(
        description = "Compile a project to PDF. Returns the status (success, failure, error, timedout, ...), LaTeX errors with source lines, warnings, the number of over/underfull boxes, the tail of the log and the output file names."
    )]
    async fn compile(&self, Parameters(a): Parameters<CompileArgs>) -> CallToolResult {
        to_result(self.do_compile(a).await)
    }

    #[tool(
        description = "Fetch an output file of the last compile of a project: the PDF (default), or a text file such as \
output.log (TeX log) or output.blg (BibTeX log). Text is returned with line numbers, at most 400 lines unless \
limit is raised; use pattern (regex) with context to find errors and warnings, and offset to page \
(negative offset = tail).",
        annotations(read_only_hint = true)
    )]
    async fn get_compile_output(&self, Parameters(a): Parameters<CompileOutputArgs>) -> CallToolResult {
        to_result(self.do_compile_output(a).await)
    }

    #[tool(
        description = "Invite a person by e-mail to a project, with edit or read-only access. They receive an invitation e-mail."
    )]
    async fn invite_collaborator(&self, Parameters(a): Parameters<InviteArgs>) -> CallToolResult {
        let result = async {
            check_project_id(&a.project_id)?;
            json_block(
                self.overleaf
                    .invite(&a.project_id, &a.email, a.read_only.unwrap_or(false))
                    .await?,
            )
        };
        to_result(result.await)
    }

    #[tool(
        description = "Make another user the owner of a project owned by this server's user. The new owner must already \
be a collaborator: invite them with invite_collaborator and transfer after they have accepted. This server's user stays \
a collaborator; only the new owner can give the project back. Overleaf e-mails both users.",
        annotations(destructive_hint = true)
    )]
    async fn transfer_ownership(&self, Parameters(a): Parameters<TransferOwnershipArgs>) -> CallToolResult {
        let result = async {
            check_project_id(&a.project_id)?;
            json_block(self.overleaf.transfer_ownership(&a.project_id, &a.email).await?)
        };
        to_result(result.await)
    }

    #[tool(description = "Accept all pending invitations to projects that other users shared with this server's user.")]
    async fn accept_invites(&self) -> CallToolResult {
        let result = self.overleaf.accept_invites().await;
        to_result(result.and_then(|accepted| json_block(json!({ "accepted": accepted }))))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for OverleafMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("overleaf-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}
