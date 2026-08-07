use crate::thread_metadata_store::{ThreadId, ThreadMetadata, ThreadMetadataStore};
use crate::threads_archive_view::format_history_entry_timestamp;
use crate::{
    Agent, AgentPanel, AgentPanelEvent, AgentThreadSource, ConversationView, NewThread,
    RemoveSelectedThread,
};
use agent::ThreadStore;
use fs::Fs;
use gpui::{
    Action, AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, ListState,
    SharedString, Subscription, Task, TaskExt, WeakEntity, Window, list, prelude::*, px,
};
use ui::{
    ContextMenu, ScrollAxes, Scrollbars, ThreadItem, Tooltip, WithScrollbar, prelude::*,
    right_click_menu,
};
use workspace::{
    CloseActiveItem, HideStatusItem, StatusItemView, Workspace,
    item::{Item, ItemEvent, ItemHandle},
};
use zed_actions::assistant::OpenAgentPage;

const HISTORY_WIDTH: f32 = 260.;

pub struct AgentPage {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    history_items: Vec<ThreadMetadata>,
    history_list_state: ListState,
    hovered_history_index: Option<usize>,
    selected_thread_id: Option<ThreadId>,
    conversation_view: Option<Entity<ConversationView>>,
    panel_subscribed: bool,
    _subscriptions: Vec<Subscription>,
}

impl AgentPage {
    pub fn new(
        _workspace: &Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let workspace_entity = cx.entity().clone();
        cx.new(|cx| {
            let focus_handle = cx.focus_handle();
            let mut subscriptions = Vec::new();

            if let Some(store) = ThreadMetadataStore::try_global(cx) {
                subscriptions.push(cx.observe(&store, |this: &mut Self, _, cx| {
                    this.refresh_history(cx);
                }));
            }

            let mut this = Self {
                workspace: workspace_entity.downgrade(),
                focus_handle,
                history_items: Vec::new(),
                history_list_state: ListState::new(0, gpui::ListAlignment::Top, px(1000.)),
                hovered_history_index: None,
                selected_thread_id: None,
                conversation_view: None,
                panel_subscribed: false,
                _subscriptions: subscriptions,
            };

            this.refresh_history(cx);

            // OpenAgentPage runs inside a Workspace update. Reading/updating the
            // workspace (via ensure_panel_subscription / open_thread) here would
            // panic with "cannot read Workspace while it is already being updated".
            cx.defer_in(window, |this, window, cx| {
                this.ensure_panel_subscription(window, cx);
                if this.conversation_view.is_none()
                    && let Some(first) = this.history_items.first().cloned()
                {
                    this.open_thread(&first, true, window, cx);
                }
            });

            this
        })
    }

    fn ensure_panel_subscription(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let Some(panel) = workspace.read(cx).panel::<AgentPanel>(cx) else {
            return;
        };

        if !self.panel_subscribed {
            self._subscriptions.push(cx.subscribe_in(
                &panel,
                window,
                |this: &mut Self, panel, event: &AgentPanelEvent, window, cx| match event {
                    AgentPanelEvent::ActiveViewChanged
                    | AgentPanelEvent::ActiveViewFocused
                    | AgentPanelEvent::EntryChanged => {
                        this.sync_from_panel(&panel, window, cx);
                    }
                    AgentPanelEvent::TerminalCloseRequested { .. }
                    | AgentPanelEvent::ThreadInteracted { .. } => {}
                },
            ));
            self.panel_subscribed = true;
        }
        self.sync_from_panel(&panel, window, cx);
    }

    pub fn open_or_toggle(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let existing = workspace
            .active_pane()
            .read(cx)
            .items()
            .find_map(|item| item.downcast::<AgentPage>());

        if let Some(existing) = existing {
            let is_active = workspace
                .active_item(cx)
                .and_then(|item| item.downcast::<AgentPage>())
                .is_some_and(|item| item.entity_id() == existing.entity_id());
            if is_active {
                let pane = workspace.active_pane().clone();
                pane.update(cx, |pane, cx| {
                    pane.close_active_item(&CloseActiveItem::default(), window, cx)
                        .detach();
                });
            } else {
                workspace.activate_item(&existing, true, true, window, cx);
            }
            return;
        }

        let existing_elsewhere = workspace.items_of_type::<AgentPage>(cx).next();
        if let Some(existing) = existing_elsewhere {
            workspace.activate_item(&existing, true, true, window, cx);
            return;
        }

        let page = AgentPage::new(workspace, window, cx);
        workspace.add_item_to_active_pane(Box::new(page), None, true, window, cx);
    }

    /// Opens the Agent page without toggling it closed when already active.
    pub fn open(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let existing = workspace
            .active_pane()
            .read(cx)
            .items()
            .find_map(|item| item.downcast::<AgentPage>());
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            return existing;
        }

        let existing_elsewhere = workspace.items_of_type::<AgentPage>(cx).next();
        if let Some(existing) = existing_elsewhere {
            workspace.activate_item(&existing, true, true, window, cx);
            return existing;
        }

        let page = AgentPage::new(workspace, window, cx);
        workspace.add_item_to_active_pane(Box::new(page.clone()), None, true, window, cx);
        page
    }

    pub fn open_and_load_thread(
        workspace: &mut Workspace,
        agent: Agent,
        thread_id: ThreadId,
        work_dirs: Option<util::path_list::PathList>,
        title: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        Self::open(workspace, window, cx);
        if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
            panel.update(cx, |panel, cx| {
                panel.load_agent_thread(
                    agent,
                    thread_id,
                    work_dirs,
                    title,
                    true,
                    AgentThreadSource::AgentPanel,
                    window,
                    cx,
                );
            });
        }
    }

    fn refresh_history(&mut self, cx: &mut Context<Self>) {
        let Some(store) = ThreadMetadataStore::try_global(cx) else {
            self.history_items.clear();
            self.history_list_state.reset(0);
            self.hovered_history_index = None;
            cx.notify();
            return;
        };

        let mut items: Vec<_> = store
            .read(cx)
            .entries()
            .filter(|entry| !entry.archived)
            .cloned()
            .collect();
        items.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        self.history_list_state.reset(items.len());
        self.history_items = items;
        if let Some(ix) = self.hovered_history_index
            && ix >= self.history_items.len()
        {
            self.hovered_history_index = None;
        }
        cx.notify();
    }

    fn sync_from_panel(
        &mut self,
        panel: &Entity<AgentPanel>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let conversation_view = panel.read(cx).active_conversation_view().cloned();
        let selected_thread_id = conversation_view
            .as_ref()
            .map(|view| view.read(cx).thread_id);

        let changed = self.selected_thread_id != selected_thread_id
            || self.conversation_view.as_ref().map(|v| v.entity_id())
                != conversation_view.as_ref().map(|v| v.entity_id());

        if changed {
            self.selected_thread_id = selected_thread_id;
            self.conversation_view = conversation_view;
            if let Some(thread_id) = selected_thread_id
                && let Some(index) = self
                    .history_items
                    .iter()
                    .position(|item| item.thread_id == thread_id)
            {
                self.history_list_state.scroll_to_reveal_item(index);
            }
            cx.notify();
        }
    }

    fn open_thread(
        &mut self,
        metadata: &ThreadMetadata,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };

        self.ensure_panel_subscription(window, cx);

        // Do not reveal/focus the docked Agent panel; AgentPage is the UI surface.
        let Some(panel) = workspace.read(cx).panel::<AgentPanel>(cx) else {
            return;
        };

        panel.update(cx, |panel, cx| {
            panel.load_agent_thread(
                Agent::from(metadata.agent_id.clone()),
                metadata.thread_id,
                Some(metadata.folder_paths().clone()),
                metadata.title.clone(),
                focus,
                AgentThreadSource::AgentPanel,
                window,
                cx,
            );
        });
        self.sync_from_panel(&panel, window, cx);
    }

    fn new_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        self.ensure_panel_subscription(window, cx);
        let Some(panel) = workspace.read(cx).panel::<AgentPanel>(cx) else {
            return;
        };

        panel.update(cx, |panel, cx| {
            panel.activate_new_thread(true, AgentThreadSource::AgentPanel, window, cx);
        });
        self.sync_from_panel(&panel, window, cx);
    }

    fn remove_selected_thread(
        &mut self,
        _: &RemoveSelectedThread,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(thread_id) = self.selected_thread_id else {
            return;
        };
        let Some(metadata) = self
            .history_items
            .iter()
            .find(|item| item.thread_id == thread_id)
            .cloned()
        else {
            return;
        };
        self.delete_thread(metadata, window, cx);
    }

    fn delete_thread(
        &mut self,
        metadata: ThreadMetadata,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let thread_id = metadata.thread_id;
        let session_id = metadata.session_id.clone();
        let agent = Agent::from(metadata.agent_id.clone());

        let connection_store = self.workspace.upgrade().and_then(|workspace| {
            let panel = workspace.read(cx).panel::<AgentPanel>(cx)?;
            let connection_store = panel.read(cx).connection_store().clone();
            panel.update(cx, |panel, cx| {
                panel.remove_thread_without_activating_draft(thread_id, window, cx);
            });
            Some(connection_store)
        });

        if connection_store.is_none() {
            if let Some(store) = ThreadMetadataStore::try_global(cx) {
                store.update(cx, |store, cx| store.delete(thread_id, cx));
            }
        }

        if let Some(connection_store) = connection_store {
            let fs = <dyn Fs>::global(cx);
            let wait_for_connection = connection_store.update(cx, |store, cx| {
                store
                    .request_connection(agent.clone(), agent.server(fs, ThreadStore::global(cx)), cx)
                    .read(cx)
                    .wait_for_connection()
            });
            cx.spawn(async move |_this, cx| {
                crate::thread_worktree_archive::cleanup_thread_archived_worktrees(thread_id, cx)
                    .await;

                let state = wait_for_connection.await?;
                let delete_session = cx.update(|cx| {
                    if let Some(session_id) = &session_id {
                        if let Some(list) = state
                            .connection
                            .session_list(cx)
                            .filter(|list| list.supports_delete())
                        {
                            list.delete_session(session_id, cx)
                        } else {
                            Task::ready(Ok(()))
                        }
                    } else {
                        Task::ready(Ok(()))
                    }
                });
                delete_session.await
            })
            .detach_and_log_err(cx);
        } else {
            cx.spawn(async move |_this, cx| {
                crate::thread_worktree_archive::cleanup_thread_archived_worktrees(thread_id, cx)
                    .await;
            })
            .detach();
        }

        if self.selected_thread_id == Some(thread_id) {
            self.selected_thread_id = None;
            self.conversation_view = None;
        }
        cx.notify();
    }

    fn render_history_item(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(metadata) = self.history_items.get(index).cloned() else {
            return div().into_any_element();
        };

        let selected = Some(metadata.thread_id) == self.selected_thread_id;
        let is_hovered = self.hovered_history_index == Some(index);
        let title = metadata.display_title();
        let timestamp = format_history_entry_timestamp(metadata.updated_at);
        let icon = match Agent::from(metadata.agent_id.clone()) {
            Agent::NativeAgent => IconName::ZedAgent,
            Agent::Custom { .. } => IconName::Sparkle,
            #[cfg(any(test, feature = "test-support"))]
            Agent::Stub => IconName::ZedAgent,
        };
        let this = cx.weak_entity();
        let metadata_for_click = metadata.clone();
        let metadata_for_delete = metadata.clone();
        let focus_handle = self.focus_handle.clone();
        let context_menu_id = SharedString::from(format!(
            "agent-page-thread-menu-{}",
            metadata.thread_id.to_key_string()
        ));

        let thread_item = ThreadItem::new(
            SharedString::from(format!(
                "agent-page-thread-{}",
                metadata.thread_id.to_key_string()
            )),
            title,
        )
        .icon(icon)
        .timestamp(timestamp)
        .selected(selected)
        .hovered(is_hovered)
        .on_hover(cx.listener(move |this, is_hovered: &bool, _window, cx| {
            let previously_hovered = this.hovered_history_index;
            this.hovered_history_index = if *is_hovered {
                Some(index)
            } else {
                previously_hovered.filter(|&i| i != index)
            };
            if this.hovered_history_index != previously_hovered {
                cx.notify();
            }
        }))
        .action_slot(
            IconButton::new("delete-thread", IconName::Trash)
                .icon_size(IconSize::Small)
                .icon_color(Color::Muted)
                .tooltip({
                    let focus_handle = focus_handle.clone();
                    move |_window, cx| {
                        Tooltip::for_action_in(
                            "Delete Thread",
                            &RemoveSelectedThread,
                            &focus_handle,
                            cx,
                        )
                    }
                })
                .on_click({
                    let this = this.clone();
                    let metadata = metadata_for_delete.clone();
                    move |_, window, cx| {
                        this.update(cx, |this, cx| {
                            this.delete_thread(metadata.clone(), window, cx);
                        })
                        .ok();
                        cx.stop_propagation();
                    }
                }),
        )
        .on_click({
            let this = this.clone();
            move |_, window, cx| {
                this.update(cx, |this, cx| {
                    this.open_thread(&metadata_for_click, true, window, cx);
                })
                .ok();
            }
        });

        right_click_menu(context_menu_id)
            .trigger(move |_, _, _| thread_item)
            .menu({
                let this = this.clone();
                let metadata = metadata_for_delete;
                move |_window, cx| {
                    let this = this.clone();
                    let metadata = metadata.clone();
                    ContextMenu::build(_window, cx, move |menu, _window, _cx| {
                        menu.entry("Delete Thread", None, {
                            let this = this.clone();
                            let metadata = metadata.clone();
                            move |window, cx| {
                                this.update(cx, |this, cx| {
                                    this.delete_thread(metadata.clone(), window, cx);
                                })
                                .ok();
                            }
                        })
                    })
                }
            })
            .into_any_element()
    }
}

impl EventEmitter<ItemEvent> for AgentPage {}

impl Focusable for AgentPage {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.conversation_view
            .as_ref()
            .map(|view| view.focus_handle(cx))
            .unwrap_or_else(|| self.focus_handle.clone())
    }
}

impl Item for AgentPage {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Agent".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ZedAssistant))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Agent Page Opened")
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for AgentPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let history_count = self.history_items.len();

        h_flex()
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .key_context("AgentPage")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &NewThread, window, cx| {
                this.new_thread(window, cx);
            }))
            .on_action(cx.listener(Self::remove_selected_thread))
            .child(
                v_flex()
                    .w(px(HISTORY_WIDTH))
                    .h_full()
                    .flex_shrink_0()
                    .border_r_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().panel_background)
                    .child(
                        h_flex()
                            .w_full()
                            .px_2()
                            .py_1p5()
                            .gap_1()
                            .border_b_1()
                            .border_color(cx.theme().colors().border_variant)
                            .child(
                                Label::new("History")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(div().flex_1())
                            .child(
                                IconButton::new("agent-page-new-thread", IconName::Plus)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::for_action_title("New Thread", &NewThread))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.new_thread(window, cx);
                                    })),
                            ),
                    )
                    .child(
                        div().id("agent-page-history").flex_1().size_full().map(|this| {
                            if history_count == 0 {
                                this.child(
                                    v_flex()
                                        .size_full()
                                        .items_center()
                                        .justify_center()
                                        .gap_2()
                                        .px_4()
                                        .child(
                                            Label::new("No threads yet")
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                        )
                                        .child(
                                            Button::new("agent-page-empty-new", "New Thread")
                                                .style(ButtonStyle::Filled)
                                                .label_size(LabelSize::Small)
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.new_thread(window, cx);
                                                })),
                                        ),
                                )
                            } else {
                                this.child(
                                    v_flex()
                                        .flex_1()
                                        .size_full()
                                        .overflow_hidden()
                                        .child(
                                            list(
                                                self.history_list_state.clone(),
                                                cx.processor(Self::render_history_item),
                                            )
                                            .flex_1()
                                            .size_full(),
                                        )
                                        .custom_scrollbars(
                                            Scrollbars::new(ScrollAxes::Vertical)
                                                .tracked_scroll_handle(&self.history_list_state),
                                            window,
                                            cx,
                                        ),
                                )
                            }
                        }),
                    ),
            )
            .child(
                v_flex().flex_1().h_full().min_w_0().map(|this| {
                    if let Some(conversation_view) = self.conversation_view.clone() {
                        this.child(conversation_view)
                    } else {
                        this.child(
                            v_flex()
                                .size_full()
                                .items_center()
                                .justify_center()
                                .gap_2()
                                .child(
                                    Label::new("Select a thread or start a new one")
                                        .color(Color::Muted),
                                )
                                .child(
                                    Button::new("agent-page-start", "New Thread")
                                        .style(ButtonStyle::Filled)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.new_thread(window, cx);
                                        })),
                                ),
                        )
                    }
                }),
            )
    }
}

pub struct AgentPageButton {
    active: bool,
}

impl AgentPageButton {
    pub fn new() -> Self {
        Self { active: false }
    }
}

impl Default for AgentPageButton {
    fn default() -> Self {
        Self::new()
    }
}

impl Render for AgentPageButton {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        IconButton::new("status-bar-open-agent-page", IconName::ZedAssistant)
            .icon_size(IconSize::Small)
            .toggle_state(self.active)
            .aria_label("Open Agent")
            .tooltip(Tooltip::for_action_title("Open Agent", &OpenAgentPage))
            .on_click(|_, window, cx| {
                window.dispatch_action(OpenAgentPage.boxed_clone(), cx);
            })
    }
}

impl StatusItemView for AgentPageButton {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let active = active_pane_item
            .and_then(|item| item.downcast::<AgentPage>())
            .is_some();
        if self.active != active {
            self.active = active;
            cx.notify();
        }
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}
