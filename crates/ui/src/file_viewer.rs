//! Read-only workspace file preview over the transcript.
//!
//! Clicking a workspace link in an assistant reply opens the file here instead
//! of handing it to the platform. Content always comes from the chat's HOST
//! device over [`methods::READ_WORKSPACE_FILE`] — never from the UI process's
//! own filesystem — so a remote session shows the remote checkout's bytes and
//! a local one behaves identically.
//!
//! The viewer is deliberately small: one file at a time, no editing, no tabs.
//! Markdown renders through the transcript's own parser/renderer as a reading
//! preview; other UTF-8 text opens as a read-only editor (line numbers, syntax,
//! virtualized rows) without a caret or edits. Every refusal the host can
//! express (missing, outside the checkout, directory, binary, oversized) gets
//! its own deliberate state rather than a blank pane.

use std::{cell::Cell, rc::Rc, sync::Arc};

use gpui::{
    AnyElement, Context, CursorStyle, Entity, EventEmitter, ListAlignment, ListState, ScrollHandle,
    SharedString, StyledText, Task, Window, div, font, list, prelude::*, px,
};
use zeron_proto::{WorkspaceFileContent, WorkspaceFileRead};
use zeron_rpc::{RpcError, methods};
use zeron_syntax::{HighlightRequest, LanguageId};

use crate::composer::{FILE_MENTION_SCHEME, percent_decode_path};
use crate::icons::{self, icon};
use crate::markdown::parser::{BlockTree, parse_full};
use crate::markdown::render::{self, RenderOptions};
use crate::state::AppState;
use crate::theme::Theme;
use crate::transcript::MAX_CONTENT_WIDTH;

/// Horizontal gutter matching the transcript (`px-4 @3xl:px-12` → 48px).
const CHAT_GUTTER: f32 = 48.0;
/// Editor row metrics — same density as the checkout-diff line grid.
const EDITOR_LINE_HEIGHT: f32 = 21.0;
const EDITOR_GUTTER_MIN: f32 = 44.0;

// ---------------------------------------------------------------------------
// Link classification (shared by the transcript and the viewer itself)
// ---------------------------------------------------------------------------

/// What a clicked markdown link target means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkTarget {
    /// Open in the viewer: a checkout-relative path (or an absolute one the
    /// host still has to prove is inside the checkout).
    WorkspacePath(String),
    /// Hand to the platform exactly as before — `http(s)`, `mailto`, anything
    /// else carrying an explicit scheme.
    External,
}

/// Classify one raw markdown link target.
///
/// The `zeron-file:` rules are the composer's, reused rather than re-derived:
/// the scheme marks a percent-encoded workspace path, and a trailing `/` marks
/// a folder mention. Everything path-shaped is a workspace path; everything
/// with another explicit scheme stays external. A trailing `#fragment` is
/// dropped for the lookup — heading/line navigation is not part of v1.
pub fn classify_link(target: &str) -> LinkTarget {
    let target = target.trim();
    if let Some(encoded) = target.strip_prefix(FILE_MENTION_SCHEME) {
        // Composer mentions percent-encode `#`, so a bare one is a fragment.
        let encoded = strip_fragment(encoded);
        let decoded = percent_decode_path(encoded).unwrap_or_else(|| encoded.to_string());
        return workspace_path(&decoded);
    }
    if has_scheme(target) {
        return LinkTarget::External;
    }
    let path = strip_fragment(target);
    // Best effort: markdown targets escape spaces and friends, but a raw path
    // that is not valid percent-encoding is used as written.
    let decoded = percent_decode_path(path).unwrap_or_else(|| path.to_string());
    workspace_path(&decoded)
}

fn workspace_path(path: &str) -> LinkTarget {
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        return LinkTarget::External;
    }
    LinkTarget::WorkspacePath(path.to_string())
}

fn strip_fragment(target: &str) -> &str {
    target.split_once('#').map_or(target, |(head, _)| head)
}

/// True for an RFC 3986 URI scheme. The only one-letter `x:` form that remains
/// a path is an actual absolute Windows drive path (`C:\…` or `C:/…`);
/// `x:payload` is a valid URI and stays external.
fn has_scheme(target: &str) -> bool {
    let Some((scheme, _)) = target.split_once(':') else {
        return false;
    };
    if is_windows_drive_path(target) {
        return false;
    }
    scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

fn is_windows_drive_path(target: &str) -> bool {
    let bytes = target.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
}

// ---------------------------------------------------------------------------
// Viewer state (pure — no gpui, so the request rules are directly testable)
// ---------------------------------------------------------------------------

/// What the body renders right now.
#[derive(Debug, Clone, PartialEq)]
enum ViewerBody {
    Loading,
    /// Shared so unrelated shell renders clone one pointer, not the parsed
    /// document tree.
    Markdown(Arc<BlockTree>),
    Text(SharedString),
    /// A deliberate non-content state: refusal, error, or empty file.
    Notice {
        title: SharedString,
        detail: SharedString,
    },
}

/// The viewer's request/result core. A monotonic request id makes a slow first
/// response unable to overwrite a later click, and the chat id scopes a result
/// to the session that asked for it.
#[derive(Debug)]
struct ViewerCore {
    chat_id: String,
    /// Path as clicked — what the header shows until the host resolves one.
    path: SharedString,
    request: u64,
    body: ViewerBody,
}

impl ViewerCore {
    fn new(chat_id: String, path: String, request: u64) -> Self {
        Self {
            chat_id,
            path: path.into(),
            request,
            body: ViewerBody::Loading,
        }
    }

    /// Apply a response, or ignore it as stale. Stale means a newer click has
    /// already replaced this request — the newest file always wins regardless
    /// of which host response lands first.
    fn apply(&mut self, request: u64, outcome: Result<WorkspaceFileRead, RpcError>) -> bool {
        if request != self.request {
            return false;
        }
        let (path, body) = match outcome {
            Ok(read) => {
                let body = body_for(&read.path, read.content);
                (SharedString::from(read.path), body)
            }
            Err(error) => (self.path.clone(), rpc_notice(&error)),
        };
        self.path = path;
        self.body = body;
        true
    }
}

/// Render form for one successful host reply.
fn body_for(path: &str, content: WorkspaceFileContent) -> ViewerBody {
    let notice = |title: &str, detail: String| ViewerBody::Notice {
        title: title.to_string().into(),
        detail: detail.into(),
    };
    match content {
        WorkspaceFileContent::Text { text } if text.is_empty() => {
            notice("Empty file", "This file has no content.".into())
        }
        WorkspaceFileContent::Text { text } if is_markdown(path) => {
            ViewerBody::Markdown(Arc::new(parse_full(&text)))
        }
        WorkspaceFileContent::Text { text } => ViewerBody::Text(text.into()),
        WorkspaceFileContent::NotFound => notice(
            "File not found",
            "It may have been moved or deleted since the message was written.".into(),
        ),
        WorkspaceFileContent::OutsideWorkspace => notice(
            "Outside this session's workspace",
            "Only files inside the session's folder can be previewed.".into(),
        ),
        WorkspaceFileContent::Directory => notice(
            "That's a folder",
            "The viewer opens files, not directories.".into(),
        ),
        WorkspaceFileContent::NotPreviewable => notice(
            "Can't preview this file",
            "It appears to be binary or unsupported text, so there is nothing readable to show."
                .into(),
        ),
        WorkspaceFileContent::TooLarge { byte_len, limit } => notice(
            "File is too large to preview",
            format!(
                "{} exceeds the {} preview limit, so nothing is shown rather than a partial file.",
                format_bytes(byte_len),
                format_bytes(limit)
            ),
        ),
        WorkspaceFileContent::PermissionDenied => notice(
            "Permission denied",
            "The session's device could not read this file.".into(),
        ),
    }
}

fn is_markdown(path: &str) -> bool {
    let extension = path
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase());
    matches!(extension.as_deref(), Some("md") | Some("markdown"))
}

fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    match bytes {
        b if b >= MIB => format!("{:.1} MiB", b as f64 / MIB as f64),
        b if b >= KIB => format!("{} KiB", b / KIB),
        b => format!("{b} B"),
    }
}

/// A failed call, translated. `UnknownMethod` is the version-skew case: an
/// older host daemon has no `ReadWorkspaceFile`, and the answer is to update
/// that device — never to fall back to reading the path locally, which would
/// show a different machine's file under the same name.
fn rpc_notice(error: &RpcError) -> ViewerBody {
    let unknown_method = matches!(error, RpcError::UnknownMethod(_))
        || matches!(error, RpcError::Failed(message) if message.contains("unknown method"));
    let (title, detail) = if unknown_method {
        (
            "This session's device runs an older zeron",
            "Update that device to preview its files.".to_string(),
        )
    } else {
        match error {
            RpcError::Transport(_) | RpcError::Closed => (
                "The session's device is unreachable",
                "Reconnect to the host to preview its files.".to_string(),
            ),
            other => ("Couldn't open this file", other.to_string()),
        }
    };
    ViewerBody::Notice {
        title: title.to_string().into(),
        detail: detail.into(),
    }
}

// ---------------------------------------------------------------------------
// The entity
// ---------------------------------------------------------------------------

pub enum FileViewerEvent {
    /// The user dismissed the viewer (close icon).
    Closed,
}

pub struct FileViewer {
    state: Entity<AppState>,
    open: Option<ViewerCore>,
    /// Monotonic across the viewer's lifetime, so ids are never reused after a
    /// close/reopen and a response from before the close can't be applied.
    next_request: u64,
    scroll: ScrollHandle,
    task: Option<Task<()>>,
    /// Height of the glass chrome (status strip + composer) the viewer's own
    /// bottom edge runs behind, written by the shell each frame. The file's
    /// content is padded and faded across it, so a line is never parked
    /// unreadably under the composer while still scrolling past it.
    chrome_inset: Cell<f32>,
    /// Virtualized source buffer (non-markdown). `None` while loading,
    /// previewing markdown, or showing a notice.
    editor: Option<EditorBuffer>,
    list: ListState,
    highlight_task: Option<Task<()>>,
}

/// One opened source file, split into editor rows.
struct EditorBuffer {
    request: u64,
    lines: Arc<Vec<SharedString>>,
    highlight: Option<Arc<zeron_syntax::HighlightedDocument>>,
}

impl EventEmitter<FileViewerEvent> for FileViewer {}

impl FileViewer {
    pub fn new(state: Entity<AppState>, _cx: &mut Context<Self>) -> Self {
        Self {
            state,
            open: None,
            next_request: 0,
            scroll: ScrollHandle::new(),
            task: None,
            chrome_inset: Cell::new(0.0),
            editor: None,
            list: ListState::new(0, ListAlignment::Top, px(512.0)),
            highlight_task: None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// Report the chrome height this frame (see [`Self::chrome_inset`]). Takes
    /// `&self` so the shell can set it while laying the overlay out, without a
    /// notify that would only schedule the frame it is already building.
    pub fn set_chrome_inset(&self, height: f32) {
        self.chrome_inset.set(height.max(0.0));
    }

    /// Open (or replace) the previewed file. The request targets the selected
    /// chat's host device; with no chat selected there is no workspace to read
    /// from and the click is ignored.
    pub fn open(&mut self, path: String, cx: &mut Context<Self>) {
        let Some((chat_id, device_id)) = self
            .state
            .read(cx)
            .selected_chat_row()
            .map(|chat| (chat.id.clone(), chat.device_id.clone()))
        else {
            return;
        };
        self.next_request += 1;
        let request = self.next_request;
        self.open = Some(ViewerCore::new(chat_id.clone(), path.clone(), request));
        self.scroll.set_offset(gpui::Point::default());
        self.clear_editor();

        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.apply(
                request,
                Err(RpcError::Failed("engine unavailable".into())),
                cx,
            );
            return;
        };
        // The host device always rides along: the engine compares it against
        // its own id, so a local chat skips the relay and a remote one is
        // forwarded to the machine that actually holds the checkout.
        let params = serde_json::json!({
            "chatId": chat_id,
            "path": path,
            "targetDeviceId": device_id,
        });
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call_as::<WorkspaceFileRead>(methods::READ_WORKSPACE_FILE, params)
                .await;
            this.update(cx, |viewer, cx| viewer.apply(request, result, cx))
                .ok();
        }));
        cx.notify();
    }

    fn apply(
        &mut self,
        request: u64,
        result: Result<WorkspaceFileRead, RpcError>,
        cx: &mut Context<Self>,
    ) {
        if let Some(core) = self.open.as_mut()
            && core.apply(request, result)
        {
            self.refresh_editor(cx);
            cx.notify();
        }
    }

    /// Dismiss the viewer without emitting — the shell's own close paths.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        if self.open.take().is_some() {
            self.task = None;
            self.clear_editor();
            cx.notify();
        }
    }

    /// Drop a file that belongs to a session the user has left: a preview from
    /// chat A must never be presented as part of chat B's workspace.
    pub fn clear_if_not_chat(&mut self, chat_id: Option<&str>, cx: &mut Context<Self>) {
        let stale = self
            .open
            .as_ref()
            .is_some_and(|core| Some(core.chat_id.as_str()) != chat_id);
        if stale {
            self.clear(cx);
        }
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.clear(cx);
        cx.emit(FileViewerEvent::Closed);
    }

    /// Route a link clicked inside the previewed markdown: workspace paths
    /// replace the current file, everything else keeps platform behavior.
    fn open_link(&mut self, target: &str, cx: &mut Context<Self>) {
        match classify_link(target) {
            LinkTarget::WorkspacePath(path) => self.open(path, cx),
            LinkTarget::External => cx.open_url(target),
        }
    }

    fn clear_editor(&mut self) {
        self.editor = None;
        self.highlight_task = None;
        self.list.reset(0);
    }

    fn refresh_editor(&mut self, cx: &mut Context<Self>) {
        let Some(core) = self.open.as_ref() else {
            self.clear_editor();
            return;
        };
        let ViewerBody::Text(text) = &core.body else {
            self.clear_editor();
            return;
        };
        let request = core.request;
        if self
            .editor
            .as_ref()
            .is_some_and(|buffer| buffer.request == request)
        {
            return;
        }
        let path = core.path.to_string();
        let text = text.clone();
        let lines = Arc::new(split_editor_lines(&text));
        self.list.reset(lines.len() + 1);
        self.editor = Some(EditorBuffer {
            request,
            lines,
            highlight: None,
        });
        self.kick_highlight(path, text, request, cx);
    }

    fn kick_highlight(
        &mut self,
        path: String,
        text: SharedString,
        request: u64,
        cx: &mut Context<Self>,
    ) {
        if zeron_syntax::language_for_path(&path).is_none() {
            return;
        }
        self.highlight_task = Some(cx.spawn(async move |this, cx| {
            let document = cx
                .background_executor()
                .spawn(async move {
                    zeron_syntax::highlight(HighlightRequest {
                        source: &text,
                        path: Some(&path),
                        fence_tag: None,
                    })
                    .ok()
                    .map(Arc::new)
                })
                .await;
            this.update(cx, |viewer, cx| {
                if let Some(buffer) = viewer.editor.as_mut()
                    && buffer.request == request
                {
                    buffer.highlight = document;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn render_editor_row(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(buffer) = self.editor.as_ref() else {
            return gpui::Empty.into_any_element();
        };
        let line_count = buffer.lines.len();
        if ix >= line_count {
            return div()
                .h(px(16.0 + self.chrome_inset.get()))
                .into_any_element();
        }
        let theme = Theme::of(cx).clone();
        let line = buffer.lines[ix].clone();
        let spans = buffer
            .highlight
            .as_ref()
            .and_then(|document| document.lines.get(ix))
            .cloned()
            .unwrap_or_default();
        let request = buffer.request;
        let gutter_w = gutter_px(line_count);
        let mono = font(theme.font_mono.clone());
        let runs =
            render::runs_for_syntax_line_with_plain(&line, &spans, &mono, theme.text, &theme);
        let styled = StyledText::new(line.clone()).with_runs(runs);
        let code = if line.is_empty() {
            div().into_any_element()
        } else {
            render::selectable_styled_text(
                format!("file-viewer-{request}:{ix}").into(),
                styled,
                line,
                &theme,
            )
        };
        div()
            .h(px(EDITOR_LINE_HEIGHT))
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .hover(|s| s.bg(crate::theme::ink(0.04)))
            .child(
                div()
                    .w(px(gutter_w))
                    .flex_none()
                    .h_full()
                    .flex()
                    .justify_end()
                    .items_center()
                    .pr(px(10.0))
                    .cursor(CursorStyle::Arrow)
                    .font_family(theme.font_mono.clone())
                    .text_size(px(11.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(format!("{}", ix + 1))),
            )
            .child(
                div()
                    .w(px(1.0))
                    .h_full()
                    .flex_none()
                    .bg(theme.border.opacity(0.65)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .pl(px(12.0))
                    .pr(px(12.0))
                    .font_family(theme.font_mono.clone())
                    .text_size(px(render::CODE_TEXT_SIZE))
                    .line_height(px(EDITOR_LINE_HEIGHT))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child(code),
            )
            .into_any_element()
    }

    fn render_header(
        &self,
        path: &SharedString,
        body: &ViewerBody,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let (dir, name) = split_display_path(path);
        let kind = match body {
            ViewerBody::Markdown(_) => Some("Markdown"),
            ViewerBody::Text(_) => Some(language_label(path)),
            ViewerBody::Loading | ViewerBody::Notice { .. } => None,
        };
        let lines = self.editor.as_ref().map(|buffer| buffer.lines.len());
        div()
            .flex_none()
            .h(px(40.0))
            .w_full()
            .px(px(12.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .cursor(CursorStyle::Arrow)
            .bg(crate::theme::ink(0.03))
            .border_b_1()
            .border_color(theme.border)
            .child(
                icon(icons::DOCUMENT)
                    .size(px(14.0))
                    .text_color(theme.text_faint),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_baseline()
                    .gap(px(6.0))
                    .overflow_hidden()
                    .when_some(dir, |el, dir| {
                        el.child(
                            div()
                                .overflow_hidden()
                                .text_size(px(11.5))
                                .text_color(theme.text_faint)
                                .child(SharedString::from(dir.to_string())),
                        )
                    })
                    .child(
                        div()
                            .overflow_hidden()
                            .text_size(px(13.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(SharedString::from(name.to_string())),
                    ),
            )
            .children(kind.map(|kind| header_chip(&theme, kind)))
            .children(lines.map(|n| {
                header_chip(
                    &theme,
                    if n == 1 {
                        "1 line".to_string()
                    } else {
                        format!("{n} lines")
                    },
                )
            }))
            .child(header_chip(&theme, "Read-only"))
            .child(
                div()
                    .id("file-viewer-close")
                    .size(px(22.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(crate::theme::ink(0.09)))
                    .on_click(cx.listener(|this, _, _, cx| this.close(cx)))
                    .child(
                        icon(icons::CLOSE)
                            .size(px(12.0))
                            .text_color(theme.text_muted),
                    ),
            )
            .into_any_element()
    }

    /// Ramp the file's text out across the glass chrome it scrolls behind.
    /// A painted overlay cannot do this job: the composer is translucent, so
    /// "what is behind the window" has no paintable color — the fade has to be
    /// per-glyph, the same primitive the transcript rides. Gated on the scroll
    /// handle so a file resting at its end (its trailing pad already filling
    /// the band) shows no ramp at all.
    fn fade_under_chrome(&self, content: impl IntoElement, inset: f32) -> AnyElement {
        crate::edge_fade::edge_faded(Theme::TRANSCRIPT_FADE_BAND, false, true, content)
            .band_bottom((inset - Theme::STATUS_STRIP_HEIGHT).max(1.0))
            .fade_overflow_y(&self.scroll)
            .into_any_element()
    }

    fn render_body(
        &mut self,
        body: &ViewerBody,
        request: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        // Everything below the file's text lives behind the glass chrome, so
        // padding keeps the end of the file scrollable into view and the
        // centered states stay centered in the part the user can actually see.
        let inset = self.chrome_inset.get();
        match body {
            ViewerBody::Loading => {
                let view = cx.entity_id();
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .pb(px(inset))
                    .child(crate::loaders::gradient_spinner(
                        "file-viewer-loading",
                        &theme,
                        3.0,
                        view,
                        cx,
                    ))
                    .into_any_element()
            }
            ViewerBody::Markdown(tree) => {
                let entity = cx.weak_entity();
                let opts = RenderOptions {
                    tufte: crate::appearance::markdown_serif(cx),
                    select_code: true,
                    select_pad_x: CHAT_GUTTER,
                    on_link: Some(Rc::new(move |target: &str, _window, cx| {
                        let target = target.to_string();
                        entity
                            .update(cx, |viewer, cx| viewer.open_link(&target, cx))
                            .ok();
                    })),
                    ..RenderOptions::settled(format!("file-viewer-{request}").into())
                };
                self.fade_under_chrome(
                    div()
                        .id("file-viewer-markdown")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(&self.scroll)
                        .cursor(CursorStyle::IBeam)
                        .px(px(4.0))
                        .pt(px(16.0))
                        .pb(px(16.0 + inset))
                        .child(render::render_tree(tree, &opts, &theme, window, &|_| None)),
                    inset,
                )
            }
            // Virtualized source view: line numbers, paint-only syntax, a
            // per-row hover wash — a read-only editor, not a blob of
            // monospace. Visible rows only materialize so a 512 KiB file
            // of one-character lines stays cheap.
            ViewerBody::Text(_) => crate::edge_fade::edge_faded(
                Theme::TRANSCRIPT_FADE_BAND,
                false,
                true,
                div()
                    .id("file-viewer-text")
                    .size_full()
                    .bg(crate::theme::ink(0.025))
                    .cursor(CursorStyle::IBeam)
                    .child(
                        list(self.list.clone(), cx.processor(Self::render_editor_row))
                            .size_full()
                            .with_sizing_behavior(gpui::ListSizingBehavior::Auto),
                    ),
            )
            .band_bottom((inset - Theme::STATUS_STRIP_HEIGHT).max(1.0))
            .into_any_element(),
            ViewerBody::Notice { title, detail } => div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(6.0))
                .px(px(32.0))
                .pb(px(inset))
                .child(
                    icon(icons::INFO_CIRCLE)
                        .size(px(20.0))
                        .text_color(theme.text_faint),
                )
                .child(
                    div()
                        .text_size(px(14.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(title.clone()),
                )
                .child(
                    div()
                        .max_w(px(420.0))
                        .text_size(px(12.5))
                        .text_center()
                        .text_color(theme.text_muted)
                        .child(detail.clone()),
                )
                .into_any_element(),
        }
    }
}

fn header_chip(theme: &Theme, label: impl Into<SharedString>) -> AnyElement {
    div()
        .flex_none()
        .h(px(20.0))
        .px(px(7.0))
        .rounded(px(5.0))
        .bg(crate::theme::ink(0.07))
        .flex()
        .items_center()
        .text_size(px(10.5))
        .text_color(theme.text_muted)
        .child(label.into())
        .into_any_element()
}

fn split_display_path(path: &str) -> (Option<&str>, &str) {
    let path = path.trim_end_matches(['/', '\\']);
    if path.is_empty() {
        return (None, path);
    }
    path.rsplit_once('/')
        .or_else(|| path.rsplit_once('\\'))
        .map(|(dir, name)| (Some(dir), name))
        .unwrap_or((None, path))
}

fn split_editor_lines(text: &str) -> Vec<SharedString> {
    text.split('\n')
        .map(|line| SharedString::from(line.to_string()))
        .collect()
}

fn gutter_px(line_count: usize) -> f32 {
    let digits = ((line_count.max(1) as f32).log10().floor() as usize) + 1;
    (18.0 + digits as f32 * 7.5).max(EDITOR_GUTTER_MIN)
}

fn language_label(path: &str) -> &'static str {
    use LanguageId::*;
    match zeron_syntax::language_for_path(path) {
        Some(Rust) => "Rust",
        Some(JavaScript) => "JavaScript",
        Some(Jsx) => "JSX",
        Some(TypeScript) => "TypeScript",
        Some(Tsx) => "TSX",
        Some(Python) => "Python",
        Some(Go) => "Go",
        Some(Json) => "JSON",
        Some(Jsonc) => "JSONC",
        Some(Bash) => "Shell",
        Some(Toml) => "TOML",
        Some(Markdown) => "Markdown",
        Some(Html) => "HTML",
        Some(Css) => "CSS",
        Some(Yaml) => "YAML",
        Some(C) => "C",
        Some(Cpp) => "C++",
        Some(CSharp) => "C#",
        Some(Java) => "Java",
        Some(Kotlin) => "Kotlin",
        Some(Swift) => "Swift",
        Some(Ruby) => "Ruby",
        Some(Php) => "PHP",
        Some(Sql) => "SQL",
        Some(Lua) => "Lua",
        Some(Dockerfile) => "Dockerfile",
        Some(Nix) => "Nix",
        Some(Make) => "Make",
        None => "Plain Text",
    }
}

impl Render for FileViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let Some((path, body, request)) = self
            .open
            .as_ref()
            .map(|core| (core.path.clone(), core.body.clone(), core.request))
        else {
            return div().into_any_element();
        };
        let header = self.render_header(&path, &body, cx);
        let article = matches!(body, ViewerBody::Markdown(_));
        let body = self.render_body(&body, request, window, cx);
        let pane = if article {
            div()
                .flex_1()
                .min_h_0()
                .w_full()
                .flex()
                .justify_center()
                .px(px(CHAT_GUTTER))
                .child(
                    div()
                        .h_full()
                        .w_full()
                        .max_w(px(MAX_CONTENT_WIDTH))
                        .min_w_0()
                        .child(body),
                )
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_h_0()
                .w_full()
                .child(body)
                .into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg)
            .border_t_1()
            .border_color(theme.border)
            .id("file-viewer")
            .occlude()
            .cursor(CursorStyle::IBeam)
            // FIRST child, so it paints first: drop the transcript's
            // selection registry before this file's text re-registers.
            // Otherwise a drag hits the hidden chat rows whose glyph
            // bounds still occupy these window coordinates.
            .child(render::selection_frame_reset())
            .child(header)
            .child(pane)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::WORKSPACE_FILE_PREVIEW_LIMIT;

    fn read(path: &str, content: WorkspaceFileContent) -> WorkspaceFileRead {
        WorkspaceFileRead {
            path: path.into(),
            content,
        }
    }

    fn text(body: &str) -> WorkspaceFileContent {
        WorkspaceFileContent::Text { text: body.into() }
    }

    #[test]
    fn workspace_links_are_classified_and_decoded() {
        assert_eq!(
            classify_link("zeron-file:crates/ui/src/composer.rs"),
            LinkTarget::WorkspacePath("crates/ui/src/composer.rs".into())
        );
        // The composer's percent encoding round-trips back to a real path.
        assert_eq!(
            classify_link("zeron-file:src/a%20file%23%5Bx%5D.rs"),
            LinkTarget::WorkspacePath("src/a file#[x].rs".into())
        );
        // A folder mention keeps its path; the host answers `directory`.
        assert_eq!(
            classify_link("zeron-file:src/components/"),
            LinkTarget::WorkspacePath("src/components".into())
        );
        // Plain markdown link shapes.
        assert_eq!(
            classify_link("docs/plan.md"),
            LinkTarget::WorkspacePath("docs/plan.md".into())
        );
        assert_eq!(
            classify_link("./docs/plan.md"),
            LinkTarget::WorkspacePath("./docs/plan.md".into())
        );
        assert_eq!(
            classify_link("/Users/me/repo/docs/plan.md"),
            LinkTarget::WorkspacePath("/Users/me/repo/docs/plan.md".into())
        );
        // Fragments are for heading navigation, which v1 does not do.
        assert_eq!(
            classify_link("docs/plan.md#goals"),
            LinkTarget::WorkspacePath("docs/plan.md".into())
        );
    }

    #[test]
    fn explicit_schemes_stay_external() {
        for target in [
            "https://example.com/a",
            "http://example.com",
            "mailto:me@example.com",
            "x:payload",
            "a:",
            "z+ext:value",
            "vscode://file/tmp/x.md",
            "file:///etc/passwd",
            "C:drive-relative",
            "#in-page-anchor",
        ] {
            assert_eq!(
                classify_link(target),
                LinkTarget::External,
                "{target} must keep external behavior"
            );
        }
    }

    #[test]
    fn only_absolute_windows_drive_forms_bypass_uri_classification() {
        assert_eq!(
            classify_link(r"C:\repo\docs\plan.md"),
            LinkTarget::WorkspacePath(r"C:\repo\docs\plan.md".into())
        );
        assert_eq!(
            classify_link("d:/repo/docs/plan.md"),
            LinkTarget::WorkspacePath("d:/repo/docs/plan.md".into())
        );
        assert_eq!(classify_link("c:relative"), LinkTarget::External);
        assert_eq!(classify_link("x:payload"), LinkTarget::External);
    }

    #[test]
    fn markdown_and_plain_text_map_to_different_bodies() {
        assert!(matches!(
            body_for("docs/plan.md", text("# Plan")),
            ViewerBody::Markdown(_)
        ));
        assert!(matches!(
            body_for("docs/PLAN.MARKDOWN", text("# Plan")),
            ViewerBody::Markdown(_)
        ));
        assert!(matches!(
            body_for("src/lib.rs", text("fn main() {}")),
            ViewerBody::Text(_)
        ));
    }

    #[test]
    fn editor_lines_keep_a_trailing_blank() {
        assert_eq!(split_editor_lines("fn main() {}").len(), 1);
        assert_eq!(split_editor_lines("a\nb").len(), 2);
        assert_eq!(split_editor_lines("a\nb\n").len(), 3);
        assert_eq!(
            split_display_path("crates/ui/src/lib.rs"),
            (Some("crates/ui/src"), "lib.rs")
        );
        assert_eq!(split_display_path("README"), (None, "README"));
        assert_eq!(language_label("src/lib.rs"), "Rust");
        assert_eq!(language_label("notes.txt"), "Plain Text");
        assert!(gutter_px(9) <= gutter_px(100));
    }

    #[test]
    fn markdown_body_clones_share_the_parsed_tree() {
        let body = body_for("docs/plan.md", text("# Plan\n- one\n"));
        let cloned = body.clone();
        let (ViewerBody::Markdown(tree), ViewerBody::Markdown(cloned_tree)) = (&body, &cloned)
        else {
            panic!("expected markdown bodies");
        };
        assert!(Arc::ptr_eq(tree, cloned_tree));
    }

    #[test]
    fn many_short_lines_remain_one_plain_text_payload() {
        let source = "x\n".repeat(WORKSPACE_FILE_PREVIEW_LIMIT as usize / 2);
        let ViewerBody::Text(rendered) = body_for("many-lines.txt", text(&source)) else {
            panic!("expected plain text");
        };
        assert_eq!(rendered.len(), source.len());
        assert_eq!(rendered.matches('\n').count(), source.matches('\n').count());
    }

    #[test]
    fn every_host_refusal_has_its_own_notice() {
        let states = [
            WorkspaceFileContent::NotFound,
            WorkspaceFileContent::OutsideWorkspace,
            WorkspaceFileContent::Directory,
            WorkspaceFileContent::NotPreviewable,
            WorkspaceFileContent::TooLarge {
                byte_len: WORKSPACE_FILE_PREVIEW_LIMIT + 1,
                limit: WORKSPACE_FILE_PREVIEW_LIMIT,
            },
            WorkspaceFileContent::PermissionDenied,
        ];
        let mut titles = Vec::new();
        for state in states {
            match body_for("docs/plan.md", state) {
                ViewerBody::Notice { title, .. } => titles.push(title.to_string()),
                other => panic!("expected a notice, got {other:?}"),
            }
        }
        let unique: std::collections::HashSet<_> = titles.iter().collect();
        assert_eq!(unique.len(), titles.len(), "states must not share a notice");

        // The size refusal has to say the content is missing on purpose.
        let ViewerBody::Notice { detail, .. } = body_for(
            "huge.md",
            WorkspaceFileContent::TooLarge {
                byte_len: 900_000,
                limit: WORKSPACE_FILE_PREVIEW_LIMIT,
            },
        ) else {
            panic!("expected a notice");
        };
        assert!(detail.contains("512 KiB"), "{detail}");
        assert!(detail.contains("partial"), "{detail}");
    }

    /// A slow first request must never overwrite the file the user asked for
    /// second, even when its response lands last.
    #[test]
    fn a_stale_response_cannot_replace_a_newer_file() {
        let first = 1;
        let second = 2;
        let opened = ViewerCore::new("chat".into(), "a.md".into(), first);
        assert_eq!(opened.request, first);
        // The second click replaces the request before A's response lands.
        let mut core = ViewerCore::new("chat".into(), "b.md".into(), second);

        assert!(core.apply(second, Ok(read("b.md", text("# B")))));
        assert!(
            !core.apply(first, Ok(read("a.md", text("# A")))),
            "the older request is dropped"
        );
        assert_eq!(core.path, "b.md");
        let ViewerBody::Markdown(tree) = &core.body else {
            panic!("expected B's markdown, got {:?}", core.body);
        };
        assert!(!tree.is_empty());
    }

    #[test]
    fn version_skew_and_transport_failures_read_differently() {
        let skew = rpc_notice(&RpcError::Failed(
            "unknown method: ReadWorkspaceFile".into(),
        ));
        let ViewerBody::Notice { title, .. } = skew else {
            panic!("expected a notice");
        };
        assert!(title.contains("older zeron"), "{title}");

        let offline = rpc_notice(&RpcError::Closed);
        let ViewerBody::Notice { title, .. } = offline else {
            panic!("expected a notice");
        };
        assert!(title.contains("unreachable"), "{title}");
    }
}
