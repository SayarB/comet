//! Quick-open palette: search the selected chat's checkout and open a file
//! in the existing viewer.

use std::time::Duration;

use gpui::{
    AnyElement, Context, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString,
    Subscription, Task, Window, div, prelude::*, px,
};
use zeron_proto::FileSearchMatch;
use zeron_rpc::{RpcError, methods};

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::icons;
use crate::popover::{self, MenuKey};
use crate::state::AppState;
use crate::theme::Theme;

/// Drop directories — the file viewer cannot preview them.
pub fn openable_files(matches: Vec<FileSearchMatch>) -> Vec<FileSearchMatch> {
    matches.into_iter().filter(|entry| !entry.is_dir).collect()
}

fn search_error_message(err: &RpcError) -> SharedString {
    match err {
        RpcError::UnknownMethod(_) => {
            "The session's device runs an older zeron — update it to search its files".into()
        }
        RpcError::Transport(_) | RpcError::Closed => "The session's device is unreachable".into(),
        RpcError::BadParams(_) | RpcError::Failed(_) => "File search failed".into(),
    }
}

pub enum FileOpenEvent {
    Open(String),
    Dismissed,
}

pub struct FileOpenPalette {
    state: Entity<AppState>,
    input: Entity<ComposerInput>,
    results: Vec<FileSearchMatch>,
    active: Option<usize>,
    loading: bool,
    error: Option<SharedString>,
    query: String,
    request: u64,
    task: Option<Task<()>>,
    focus: FocusHandle,
    focus_pending: bool,
    _input_events: Subscription,
}

impl EventEmitter<FileOpenEvent> for FileOpenPalette {}

impl FileOpenPalette {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| ComposerInput::with_context("Search files", "PaletteSearch", cx));
        let input_events = cx.subscribe(&input, |this, _, event, cx| {
            if matches!(
                event,
                ComposerInputEvent::Edited | ComposerInputEvent::CursorMoved
            ) {
                this.on_query(cx);
            }
        });
        let mut page = Self {
            state,
            input,
            results: Vec::new(),
            active: None,
            loading: false,
            error: None,
            query: String::new(),
            request: 0,
            task: None,
            focus: cx.focus_handle(),
            focus_pending: true,
            _input_events: input_events,
        };
        page.search(cx);
        page
    }

    fn on_query(&mut self, cx: &mut Context<Self>) {
        let query = self.input.read(cx).text().to_string();
        if query == self.query {
            return;
        }
        self.query = query;
        self.search(cx);
    }

    fn search(&mut self, cx: &mut Context<Self>) {
        self.request = self.request.wrapping_add(1);
        let request = self.request;
        self.task = None;
        self.error = None;

        let Some(chat) = self.state.read(cx).selected_chat_row() else {
            self.loading = false;
            self.results.clear();
            self.active = None;
            self.error = Some("Open a session to search its files.".into());
            cx.notify();
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.loading = false;
            self.results.clear();
            self.active = None;
            self.error = Some("Engine not connected".into());
            cx.notify();
            return;
        };

        self.loading = true;
        let query = self.query.clone();
        let params = serde_json::json!({
            "query": query,
            "chatId": chat.id,
            "targetDeviceId": chat.device_id,
        });
        self.task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(80))
                .await;
            let mut result = engine
                .client()
                .call(methods::SEARCH_FILES, params.clone())
                .await;
            if matches!(result, Err(RpcError::Transport(_)) | Err(RpcError::Closed)) {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                result = engine.client().call(methods::SEARCH_FILES, params).await;
            }
            this.update(cx, |page, cx| {
                if page.request != request {
                    return;
                }
                page.loading = false;
                match result {
                    Ok(value) => match serde_json::from_value::<Vec<FileSearchMatch>>(value) {
                        Ok(matches) => {
                            page.error = None;
                            page.results = openable_files(matches);
                            page.active = (!page.results.is_empty()).then_some(0);
                        }
                        Err(err) => {
                            page.results.clear();
                            page.active = None;
                            page.error = Some(err.to_string().into());
                        }
                    },
                    Err(err) => {
                        page.results.clear();
                        page.active = None;
                        page.error = Some(search_error_message(&err));
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn select_active(&mut self, cx: &mut Context<Self>) {
        let Some(index) = self.active else {
            return;
        };
        let Some(path) = self.results.get(index).map(|entry| entry.path.clone()) else {
            return;
        };
        cx.emit(FileOpenEvent::Open(path));
    }

    fn on_key(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        let key = popover::classify_key(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        );
        match key {
            MenuKey::Escape => cx.emit(FileOpenEvent::Dismissed),
            MenuKey::Up | MenuKey::Down => {
                let delta = if key == MenuKey::Up { -1 } else { 1 };
                self.active = popover::menu_step(self.active, self.results.len(), delta);
                cx.notify();
            }
            MenuKey::Enter | MenuKey::ModEnter => self.select_active(cx),
            MenuKey::Backspace | MenuKey::Other => return,
        }
        cx.stop_propagation();
    }

    pub fn render_overlay(
        &mut self,
        viewport: gpui::Size<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if std::mem::take(&mut self.focus_pending) {
            let handle = self.input.focus_handle(cx);
            window.focus(&handle, cx);
        }
        let theme = Theme::of(cx).clone();
        let input = self.input.clone();
        let query_empty = self.query.is_empty();
        let loading = self.loading && self.results.is_empty();

        let body: AnyElement = if loading {
            div()
                .px(px(8.0))
                .py(px(6.0))
                .child(popover::skeleton_rows(
                    "file-open-skeleton",
                    &theme,
                    5,
                    cx.entity_id(),
                    cx,
                ))
                .into_any_element()
        } else if let Some(error) = self.error.clone() {
            popover::error_row(&theme, error.as_ref())
                .px(px(14.0))
                .py(px(12.0))
                .into_any_element()
        } else if self.results.is_empty() {
            div()
                .px(px(14.0))
                .py(px(16.0))
                .text_size(px(12.5))
                .text_color(theme.text_faint)
                .child(SharedString::from(if query_empty {
                    "No files available"
                } else {
                    "No matching files"
                }))
                .into_any_element()
        } else {
            let mut list = div()
                .id("file-open-results")
                .max_h(px(320.0))
                .overflow_y_scroll()
                .py(px(4.0));
            for (index, result) in self.results.iter().enumerate() {
                let selected = self.active == Some(index);
                let path = result.path.clone();
                list = list.child(
                    popover::menu_row(&theme, selected, format!("file-open-result-{index}"))
                        .id(("file-open-result", index))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.active = Some(index);
                            this.select_active(cx);
                        }))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .child(
                                    icons::icon(icons::DOCUMENT)
                                        .size(px(14.0))
                                        .text_color(theme.text_muted),
                                )
                                .child(
                                    div()
                                        .min_w_0()
                                        .flex_1()
                                        .truncate()
                                        .text_size(px(13.0))
                                        .text_color(theme.text)
                                        .child(SharedString::from(path)),
                                ),
                        ),
                );
            }
            list.into_any_element()
        };

        let card = div()
            .id("file-open-palette")
            .w(px(520.0))
            .rounded(px(14.0))
            .border_1()
            .border_color(crate::theme::hairline(0.10))
            .bg(if theme.is_glass() {
                theme.glass_overlay()
            } else {
                theme.surface_overlay
            })
            .shadow_lg()
            .overflow_hidden()
            .flex()
            .flex_col()
            .text_color(theme.text)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event, _, cx| this.on_key(event, cx)))
            .on_mouse_down_out(cx.listener(|_this, _, _, cx| {
                cx.emit(FileOpenEvent::Dismissed);
            }))
            .child(
                div()
                    .px(px(14.0))
                    .py(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .border_b_1()
                    .border_color(crate::theme::hairline(0.08))
                    .child(
                        icons::icon(icons::DOCUMENT)
                            .size(px(16.0))
                            .text_color(theme.text_muted),
                    )
                    .child(div().flex_1().min_w_0().child(input)),
            )
            .child(body)
            .into_any_element();

        popover::modal_glass("file-open-dialog", viewport, card, 14.0)
    }
}

impl Render for FileOpenPalette {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, is_dir: bool) -> FileSearchMatch {
        FileSearchMatch {
            path: path.into(),
            is_dir,
        }
    }

    #[test]
    fn openable_files_drop_directories() {
        let rows = openable_files(vec![
            entry("src", true),
            entry("src/main.rs", false),
            entry("README.md", false),
            entry("docs", true),
        ]);
        assert_eq!(
            rows.iter().map(|row| row.path.as_str()).collect::<Vec<_>>(),
            vec!["src/main.rs", "README.md"]
        );
        assert!(openable_files(Vec::new()).is_empty());
        assert!(openable_files(vec![entry("src", true)]).is_empty());
    }
}
