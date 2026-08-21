mod dev_container_suggest;
pub mod disconnected_overlay;
mod remote_connections;
mod remote_servers;
pub mod sidebar_recent_projects;
mod ssh_config;

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::{DateTime, Utc};

use fs::Fs;

#[cfg(target_os = "windows")]
mod wsl_picker;

use remote::{RemoteConnectionOptions, remote_connection_identity, same_remote_connection_identity};
pub use remote_connection::{RemoteConnectionModal, connect, connect_with_modal};
pub use remote_connections::{navigate_to_positions, open_remote_project};

use disconnected_overlay::DisconnectedOverlay;
use editor::Editor;
use fuzzy_nucleo::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    Action, AnyElement, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    Subscription, Task, TaskExt, WeakEntity, Window, actions, px,
};

use picker::{
    Picker, PickerDelegate, ScrollBehavior,
    highlighted_match_with_paths::{HighlightedMatch, HighlightedMatchWithPaths},
};
use project::{Worktree, git_store::Repository};
pub use remote_connections::RemoteSettings;
pub use remote_servers::RemoteServerProjects;
use settings::{DefaultOpenBehavior, Settings, WorktreeId};
use workspace::ProjectGroupKey;

use dev_container::{DevContainerContext, find_devcontainer_configs};
use ui::{
    ButtonLike, ContextMenu, Divider, HighlightedLabel, KeyBinding, ListItem, ListItemSpacing,
    ListSubHeader, PopoverMenu, PopoverMenuHandle, TintColor, Tooltip, prelude::*,
};
use util::{ResultExt, paths::PathExt};
use workspace::{
    HistoryManager, ModalView, MultiWorkspace, OpenMode, OpenOptions, OpenVisible, PathList,
    ProjectFolder,
    ProjectFolderAssignment, ProjectFolderId, RecentWorkspace, SerializedWorkspaceLocation,
    Workspace, WorkspaceDb, WorkspaceId, notifications::DetachAndPromptErr,
    with_active_or_new_workspace,
};
use zed_actions::{OpenDevContainer, OpenRecent, OpenRemote};

actions!(
    recent_projects,
    [
        ToggleActionsMenu,
        RemoveSelected,
        AddToWorkspace,
        NewFolder,
        RenameFolder
    ]
);

#[derive(Clone, Debug)]
pub struct RecentProjectEntry {
    pub name: SharedString,
    pub full_path: SharedString,
    pub paths: Vec<PathBuf>,
    pub workspace_id: WorkspaceId,
    pub timestamp: DateTime<Utc>,
}

#[derive(Clone, Debug)]
struct OpenFolderEntry {
    worktree_id: WorktreeId,
    name: SharedString,
    path: PathBuf,
    branch: Option<SharedString>,
    is_active: bool,
    connection_options: Option<RemoteConnectionOptions>,
}

#[derive(Clone, Debug)]
enum ProjectPickerEntry {
    Header(SharedString),
    /// A currently open folder from the active workspace's "Current Folders" section.
    ///
    /// `index` points into `RecentProjectsDelegate::open_folders`, and `positions` stores the
    /// fuzzy-match highlight positions for rendering the folder name.
    OpenFolder {
        index: usize,
        positions: Vec<usize>,
    },
    /// A project group from the current window's "This Window" section.
    ///
    /// These entries come from `RecentProjectsDelegate::window_project_groups`, not from the
    /// recent-project database. Empty queries list every project group known to the current
    /// window; non-empty queries list matching project groups. Confirming one activates or loads
    /// that project group in the current window, while secondary confirm can move local project
    /// groups to a new window when multiple groups are available.
    ProjectGroup(StringMatch),
    /// A user-defined named folder that groups recent projects.
    Folder {
        folder_id: ProjectFolderId,
        name: SharedString,
    },
    /// A workspace from the recent-project database.
    ///
    /// The match's `candidate_id` indexes into `RecentProjectsDelegate::workspaces`. Confirming
    /// one opens that recent workspace in either the current window or a new window, depending on
    /// whether the picker was invoked for new-window behavior and whether this was a primary or
    /// secondary confirm.
    RecentProject(StringMatch),
    /// Creates a new user folder. Shown at the end of the modal list.
    NewFolder,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FolderEdit {
    Create,
    Rename(ProjectFolderId),
}

fn is_selectable_entry(entry: &ProjectPickerEntry) -> bool {
    matches!(
        entry,
        ProjectPickerEntry::OpenFolder { .. }
            | ProjectPickerEntry::ProjectGroup(_)
            | ProjectPickerEntry::Folder { .. }
            | ProjectPickerEntry::RecentProject(_)
            | ProjectPickerEntry::NewFolder
    )
}

fn recent_project_match(id: usize, matches: &HashMap<usize, StringMatch>) -> StringMatch {
    matches.get(&id).cloned().unwrap_or(StringMatch {
        candidate_id: id,
        score: 0.0,
        positions: Vec::new(),
        string: Default::default(),
    })
}

fn path_list_search_blob(paths: &PathList) -> String {
    let mut parts = Vec::new();
    for path in paths.ordered_paths() {
        let compact = path.compact();
        if let Some(name) = compact.file_name() {
            parts.push(name.to_string_lossy().into_owned());
        }
        parts.push(compact.to_string_lossy().into_owned());
    }
    parts.join(" ")
}

fn folder_identity_for_connection(host: Option<&RemoteConnectionOptions>) -> String {
    host.map(|options| remote_connection_identity(options).persistence_key())
        .unwrap_or_default()
}

fn folder_identity_for_group(key: &ProjectGroupKey) -> String {
    folder_identity_for_connection(key.host().as_ref())
}

#[derive(Clone, Debug)]
struct PendingFolderAssign {
    remote_identity: String,
    identity_paths: PathList,
}

impl PendingFolderAssign {
    fn from_workspace(workspace: &RecentWorkspace) -> Self {
        Self {
            remote_identity: workspace.project_folder_identity(),
            identity_paths: workspace.identity_paths.clone(),
        }
    }

    fn from_project_group(key: &ProjectGroupKey) -> Self {
        Self {
            remote_identity: folder_identity_for_group(key),
            identity_paths: key.path_list().clone(),
        }
    }
}

fn populate_move_to_folder_menu(
    mut menu: ContextMenu,
    picker_entity: Entity<Picker<RecentProjectsDelegate>>,
    user_folders: &[ProjectFolder],
    current_folder_id: Option<ProjectFolderId>,
    pending: PendingFolderAssign,
) -> ContextMenu {
    if user_folders.is_empty() {
        return menu.entry("New Folder…", None, {
            let picker_entity = picker_entity.clone();
            let pending = pending.clone();
            move |window, cx| {
                picker_entity.update(cx, |picker, cx| {
                    picker
                        .delegate
                        .start_create_folder(Some(pending.clone()), window, cx);
                });
            }
        });
    }

    for folder in user_folders {
        if Some(folder.folder_id) == current_folder_id {
            continue;
        }
        let folder_id = folder.folder_id;
        let picker_entity = picker_entity.clone();
        let pending = pending.clone();
        menu = menu.entry(folder.name.clone(), None, move |window, cx| {
            picker_entity.update(cx, |picker, cx| {
                picker.delegate.assign_identity_to_folder(
                    folder_id,
                    pending.remote_identity.clone(),
                    pending.identity_paths.clone(),
                    window,
                    cx,
                );
            });
        });
    }

    menu = menu.separator().entry("New Folder…", None, {
        let picker_entity = picker_entity.clone();
        let pending = pending.clone();
        move |window, cx| {
            picker_entity.update(cx, |picker, cx| {
                picker
                    .delegate
                    .start_create_folder(Some(pending.clone()), window, cx);
            });
        }
    });

    if current_folder_id.is_some() {
        let picker_entity = picker_entity.clone();
        menu = menu.entry("Remove from Folder", None, move |window, cx| {
            picker_entity.update(cx, |picker, cx| {
                picker.delegate.unassign_identity_from_folder(
                    pending.remote_identity.clone(),
                    pending.identity_paths.clone(),
                    window,
                    cx,
                );
            });
        });
    }

    menu
}

fn move_to_folder_popover(
    id: impl Into<ElementId>,
    trigger_id: impl Into<ElementId>,
    menu_handle: PopoverMenuHandle<ContextMenu>,
    picker_entity: Entity<Picker<RecentProjectsDelegate>>,
    user_folders: Vec<ProjectFolder>,
    current_folder_id: Option<ProjectFolderId>,
    pending: PendingFolderAssign,
) -> PopoverMenu<ContextMenu> {
    PopoverMenu::new(id)
        .with_handle(menu_handle)
        .trigger(
            IconButton::new(trigger_id, IconName::FolderAdd)
                .icon_size(IconSize::Small)
                .tooltip(Tooltip::text("Move to Folder")),
        )
        .menu(move |window, cx| {
            let picker_entity = picker_entity.clone();
            let user_folders = user_folders.clone();
            let pending = pending.clone();
            Some(ContextMenu::build(window, cx, move |menu, _, _| {
                populate_move_to_folder_menu(
                    menu,
                    picker_entity.clone(),
                    &user_folders,
                    current_folder_id,
                    pending.clone(),
                )
            }))
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectPickerStyle {
    Modal,
    Popover,
}

pub async fn get_recent_projects(
    current_workspace_id: Option<WorkspaceId>,
    limit: Option<usize>,
    fs: Arc<dyn fs::Fs>,
    db: &WorkspaceDb,
) -> Vec<RecentProjectEntry> {
    let workspaces = db
        .recent_project_workspaces(fs.as_ref())
        .await
        .unwrap_or_default();

    let filtered: Vec<_> = workspaces
        .into_iter()
        .filter(|workspace| Some(workspace.workspace_id) != current_workspace_id)
        .filter(|workspace| matches!(workspace.location, SerializedWorkspaceLocation::Local))
        .collect();

    let mut all_paths: Vec<PathBuf> = filtered
        .iter()
        .flat_map(|workspace| workspace.identity_paths.paths().iter().cloned())
        .collect();
    all_paths.sort_unstable();
    all_paths.dedup();
    let path_details =
        util::disambiguate::compute_disambiguation_details(&all_paths, |path, detail| {
            project::path_suffix(path, detail)
        });
    let path_detail_map: std::collections::HashMap<PathBuf, usize> =
        all_paths.into_iter().zip(path_details).collect();

    let entries: Vec<RecentProjectEntry> = filtered
        .into_iter()
        .map(|workspace| {
            let paths: Vec<PathBuf> = workspace.paths.paths().to_vec();
            let ordered_paths: Vec<&PathBuf> = workspace.identity_paths.ordered_paths().collect();

            let name = ordered_paths
                .iter()
                .map(|p| {
                    let detail = path_detail_map.get(*p).copied().unwrap_or(0);
                    project::path_suffix(p, detail)
                })
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(", ");

            let full_path = ordered_paths
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join("\n");

            RecentProjectEntry {
                name: SharedString::from(name),
                full_path: SharedString::from(full_path),
                paths,
                workspace_id: workspace.workspace_id,
                timestamp: workspace.timestamp,
            }
        })
        .collect();

    match limit {
        Some(n) => entries.into_iter().take(n).collect(),
        None => entries,
    }
}

pub async fn delete_recent_project(workspace_id: WorkspaceId, db: &WorkspaceDb) {
    let _ = db.delete_workspace_by_id(workspace_id).await;
}

fn get_open_folders(workspace: &Workspace, cx: &App) -> Vec<OpenFolderEntry> {
    let project = workspace.project().read(cx);
    let connection_options = project.remote_connection_options(cx);
    let visible_worktrees: Vec<_> = project.visible_worktrees(cx).collect();

    if visible_worktrees.len() <= 1 {
        return Vec::new();
    }

    let active_worktree_id = if let Some(repo) = project.active_repository(cx) {
        let repo = repo.read(cx);
        let repo_path = &repo.work_directory_abs_path;
        project.visible_worktrees(cx).find_map(|worktree| {
            let worktree_path = worktree.read(cx).abs_path();
            (worktree_path == *repo_path || worktree_path.starts_with(repo_path.as_ref()))
                .then(|| worktree.read(cx).id())
        })
    } else {
        project
            .visible_worktrees(cx)
            .next()
            .map(|wt| wt.read(cx).id())
    };

    let mut all_paths: Vec<PathBuf> = visible_worktrees
        .iter()
        .map(|wt| wt.read(cx).abs_path().to_path_buf())
        .collect();
    all_paths.sort_unstable();
    all_paths.dedup();
    let path_details =
        util::disambiguate::compute_disambiguation_details(&all_paths, |path, detail| {
            project::path_suffix(path, detail)
        });
    let path_detail_map: std::collections::HashMap<PathBuf, usize> =
        all_paths.into_iter().zip(path_details).collect();

    let git_store = project.git_store().read(cx);
    let repositories: Vec<_> = git_store.repositories().values().cloned().collect();

    let mut entries: Vec<OpenFolderEntry> = visible_worktrees
        .into_iter()
        .map(|worktree| {
            let worktree_ref = worktree.read(cx);
            let worktree_id = worktree_ref.id();
            let path = worktree_ref.abs_path().to_path_buf();
            let detail = path_detail_map.get(&path).copied().unwrap_or(0);
            let name = SharedString::from(project::path_suffix(&path, detail));
            let branch = get_branch_for_worktree(worktree_ref, &repositories, cx);
            let is_active = active_worktree_id == Some(worktree_id);
            OpenFolderEntry {
                worktree_id,
                name,
                path,
                branch,
                is_active,
                connection_options: connection_options.clone(),
            }
        })
        .collect();

    entries.sort_by_key(|entry| entry.name.to_lowercase());
    entries
}

fn get_branch_for_worktree(
    worktree: &Worktree,
    repositories: &[Entity<Repository>],
    cx: &App,
) -> Option<SharedString> {
    let worktree_abs_path = worktree.abs_path();
    repositories
        .iter()
        .filter(|repo| {
            let repo_path = &repo.read(cx).work_directory_abs_path;
            *repo_path == worktree_abs_path || worktree_abs_path.starts_with(repo_path.as_ref())
        })
        .max_by_key(|repo| repo.read(cx).work_directory_abs_path.as_os_str().len())
        .and_then(|repo| {
            repo.read(cx)
                .branch
                .as_ref()
                .map(|branch| SharedString::from(branch.name().to_string()))
        })
}

pub(crate) fn default_open_in_new_window(cx: &App) -> bool {
    matches!(
        workspace::WorkspaceSettings::get_global(cx).default_open_behavior,
        DefaultOpenBehavior::NewWindow
    )
}

pub fn init(cx: &mut App) {
    #[cfg(target_os = "windows")]
    cx.on_action(|open_wsl: &zed_actions::wsl_actions::OpenFolderInWsl, cx| {
        let create_new_window = open_wsl
            .create_new_window
            .unwrap_or_else(|| default_open_in_new_window(cx));
        with_active_or_new_workspace(cx, move |workspace, window, cx| {
            use gpui::PathPromptOptions;
            use project::DirectoryLister;

            let paths = workspace.prompt_for_open_path(
                PathPromptOptions {
                    files: true,
                    directories: true,
                    multiple: false,
                    prompt: None,
                },
                DirectoryLister::Local(
                    workspace.project().clone(),
                    workspace.app_state().fs.clone(),
                ),
                window,
                cx,
            );

            let app_state = workspace.app_state().clone();
            let window_handle = window.window_handle().downcast::<MultiWorkspace>();

            cx.spawn_in(window, async move |workspace, cx| {
                use util::paths::SanitizedPath;

                let Some(paths) = paths.await.log_err().flatten() else {
                    return;
                };

                let wsl_path = paths
                    .iter()
                    .find_map(util::paths::WslPath::from_path);

                if let Some(util::paths::WslPath { distro, path }) = wsl_path {
                    use remote::WslConnectionOptions;

                    let connection_options = RemoteConnectionOptions::Wsl(WslConnectionOptions {
                        distro_name: distro.to_string(),
                        user: None,
                    });

                    let requesting_window = match create_new_window {
                        false => window_handle,
                        true => None,
                    };

                    let open_options = workspace::OpenOptions {
                        requesting_window,
                        ..Default::default()
                    };

                    open_remote_project(connection_options, vec![path.into()], app_state, open_options, cx).await.log_err();
                    return;
                }

                let paths = paths
                    .into_iter()
                    .filter_map(|path| SanitizedPath::new(&path).local_to_wsl())
                    .collect::<Vec<_>>();

                if paths.is_empty() {
                    let message = indoc::indoc! { r#"
                        Invalid path specified when trying to open a folder inside WSL.

                        Please note that Zed currently does not support opening network share folders inside wsl.
                    "#};

                    let _ = cx.prompt(gpui::PromptLevel::Critical, "Invalid path", Some(&message), &["OK"]).await;
                    return;
                }

                workspace.update_in(cx, |workspace, window, cx| {
                    workspace.toggle_modal(window, cx, |window, cx| {
                        crate::wsl_picker::WslOpenModal::new(paths, create_new_window, window, cx)
                    });
                }).log_err();
            })
            .detach();
        });
    });

    #[cfg(target_os = "windows")]
    cx.on_action(|open_wsl: &zed_actions::wsl_actions::OpenWsl, cx| {
        let create_new_window = open_wsl
            .create_new_window
            .unwrap_or_else(|| default_open_in_new_window(cx));
        with_active_or_new_workspace(cx, move |workspace, window, cx| {
            let handle = cx.entity().downgrade();
            let fs = workspace.project().read(cx).fs().clone();
            workspace.toggle_modal(window, cx, |window, cx| {
                RemoteServerProjects::wsl(create_new_window, fs, window, handle, cx)
            });
        });
    });

    #[cfg(target_os = "windows")]
    cx.on_action(|open_wsl: &remote::OpenWslPath, cx| {
        let open_wsl = open_wsl.clone();
        with_active_or_new_workspace(cx, move |workspace, window, cx| {
            let fs = workspace.project().read(cx).fs().clone();
            add_wsl_distro(fs, &open_wsl.distro, cx);
            let requesting_window =
                match workspace::WorkspaceSettings::get_global(cx).default_open_behavior {
                    DefaultOpenBehavior::ExistingWindow => {
                        window.window_handle().downcast::<MultiWorkspace>()
                    }
                    DefaultOpenBehavior::NewWindow => None,
                };
            let open_options = OpenOptions {
                requesting_window,
                ..Default::default()
            };

            let app_state = workspace.app_state().clone();

            cx.spawn_in(window, async move |_, cx| {
                open_remote_project(
                    RemoteConnectionOptions::Wsl(open_wsl.distro.clone()),
                    open_wsl.paths,
                    app_state,
                    open_options,
                    cx,
                )
                .await
            })
            .detach();
        });
    });

    cx.on_action(|open_recent: &OpenRecent, cx| {
        let create_new_window = open_recent.create_new_window;

        match cx
            .active_window()
            .and_then(|w| w.downcast::<MultiWorkspace>())
        {
            Some(multi_workspace) => {
                cx.defer(move |cx| {
                    multi_workspace
                        .update(cx, |multi_workspace, window, cx| {
                            let window_project_groups: Vec<ProjectGroupKey> =
                                multi_workspace.project_group_keys();

                            let workspace = multi_workspace.workspace().clone();
                            workspace.update(cx, |workspace, cx| {
                                let Some(recent_projects) =
                                    workspace.active_modal::<RecentProjects>(cx)
                                else {
                                    let focus_handle = workspace.focus_handle(cx);
                                    RecentProjects::open(
                                        workspace,
                                        create_new_window,
                                        window_project_groups,
                                        window,
                                        focus_handle,
                                        cx,
                                    );
                                    return;
                                };

                                recent_projects.update(cx, |recent_projects, cx| {
                                    recent_projects
                                        .picker
                                        .update(cx, |picker, cx| picker.cycle_selection(window, cx))
                                });
                            });
                        })
                        .log_err();
                });
            }
            None => {
                with_active_or_new_workspace(cx, move |workspace, window, cx| {
                    let Some(recent_projects) = workspace.active_modal::<RecentProjects>(cx) else {
                        let focus_handle = workspace.focus_handle(cx);
                        RecentProjects::open(
                            workspace,
                            create_new_window,
                            Vec::new(),
                            window,
                            focus_handle,
                            cx,
                        );
                        return;
                    };

                    recent_projects.update(cx, |recent_projects, cx| {
                        recent_projects
                            .picker
                            .update(cx, |picker, cx| picker.cycle_selection(window, cx))
                    });
                });
            }
        }
    });
    cx.on_action(|open_remote: &OpenRemote, cx| {
        let from_existing_connection = open_remote.from_existing_connection;
        let create_new_window = open_remote
            .create_new_window
            .unwrap_or_else(|| default_open_in_new_window(cx));
        with_active_or_new_workspace(cx, move |workspace, window, cx| {
            if from_existing_connection {
                cx.propagate();
                return;
            }
            let handle = cx.entity().downgrade();
            let fs = workspace.project().read(cx).fs().clone();
            workspace.toggle_modal(window, cx, |window, cx| {
                RemoteServerProjects::new(create_new_window, fs, window, handle, cx)
            })
        });
    });

    cx.observe_new(DisconnectedOverlay::register).detach();

    cx.on_action(|_: &OpenDevContainer, cx| {
        with_active_or_new_workspace(cx, move |workspace, window, cx| {
            if !workspace.project().read(cx).is_local() {
                cx.spawn_in(window, async move |_, cx| {
                    cx.prompt(
                        gpui::PromptLevel::Critical,
                        "Cannot open Dev Container from remote project",
                        None,
                        &["OK"],
                    )
                    .await
                    .ok();
                })
                .detach();
                return;
            }

            let fs = workspace.project().read(cx).fs().clone();
            let configs = find_devcontainer_configs(workspace, cx);
            let app_state = workspace.app_state().clone();
            let dev_container_context = DevContainerContext::from_workspace(workspace, cx);
            let handle = cx.entity().downgrade();
            workspace.toggle_modal(window, cx, |window, cx| {
                RemoteServerProjects::new_dev_container(
                    fs,
                    configs,
                    app_state,
                    dev_container_context,
                    window,
                    handle,
                    cx,
                )
            });
        });
    });

    // Subscribe to worktree additions to suggest opening the project in a dev container
    cx.observe_new(
        |workspace: &mut Workspace, window: Option<&mut Window>, cx: &mut Context<Workspace>| {
            let Some(window) = window else {
                return;
            };
            cx.subscribe_in(
                workspace.project(),
                window,
                move |workspace, project, event, window, cx| {
                    if let project::Event::WorktreeUpdatedEntries(worktree_id, updated_entries) =
                        event
                    {
                        dev_container_suggest::suggest_on_worktree_updated(
                            workspace,
                            *worktree_id,
                            updated_entries,
                            project,
                            window,
                            cx,
                        );
                    }
                },
            )
            .detach();
        },
    )
    .detach();
}

#[cfg(target_os = "windows")]
pub fn add_wsl_distro(
    fs: Arc<dyn project::Fs>,
    connection_options: &remote::WslConnectionOptions,
    cx: &App,
) {
    use gpui::ReadGlobal;
    use settings::SettingsStore;

    let distro_name = connection_options.distro_name.clone();
    let user = connection_options.user.clone();
    SettingsStore::global(cx).update_settings_file(fs, move |setting, _| {
        let connections = setting
            .remote
            .wsl_connections
            .get_or_insert(Default::default());

        if !connections
            .iter()
            .any(|conn| conn.distro_name == distro_name && conn.user == user)
        {
            use std::collections::BTreeSet;

            connections.push(settings::WslConnection {
                distro_name,
                user,
                projects: BTreeSet::new(),
            })
        }
    });
}

pub struct RecentProjects {
    pub picker: Entity<Picker<RecentProjectsDelegate>>,
    _subscriptions: Vec<Subscription>,
}

impl ModalView for RecentProjects {
    fn on_before_dismiss(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> workspace::DismissDecision {
        let submenu_focused = self.picker.update(cx, |picker, cx| {
            picker.delegate.has_another_open_menu(window, cx)
        });
        workspace::DismissDecision::Dismiss(!submenu_focused)
    }
}

impl RecentProjects {
    fn new(
        delegate: RecentProjectsDelegate,
        fs: Option<Arc<dyn Fs>>,
        rem_width: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let style = delegate.style;
        let picker = cx.new(|cx| {
            Picker::list(delegate, window, cx)
                .list_measure_all()
                .initial_width(rems(rem_width))
                .show_scrollbar(true)
        });

        let picker_focus_handle = picker.focus_handle(cx);
        picker.update(cx, |picker, _| {
            picker.delegate.focus_handle = picker_focus_handle;
        });

        let mut subscriptions = vec![cx.subscribe(&picker, |_, _, _, cx| cx.emit(DismissEvent))];

        if style == ProjectPickerStyle::Popover {
            let picker_focus = picker.focus_handle(cx);
            subscriptions.push(
                cx.on_focus_out(&picker_focus, window, |this, _, window, cx| {
                    let submenu_focused = this.picker.update(cx, |picker, cx| {
                        picker.delegate.actions_menu_handle.is_focused(window, cx)
                    });
                    if !submenu_focused {
                        cx.emit(DismissEvent);
                    }
                }),
            );
        }
        // We do not want to block the UI on a potentially lengthy call to DB, so we're gonna swap
        // out workspace locations once the future runs to completion.
        let db = WorkspaceDb::global(cx);
        cx.spawn_in(window, async move |this, cx| {
            let Some(fs) = fs else { return };
            let workspaces = db
                .recent_project_workspaces(fs.as_ref())
                .await
                .log_err()
                .unwrap_or_default();
            let folders = db.project_folders().log_err().unwrap_or_default();
            let assignments = db
                .project_folder_assignments()
                .log_err()
                .unwrap_or_default();
            this.update_in(cx, move |this, window, cx| {
                this.picker.update(cx, move |picker, cx| {
                    picker.delegate.set_workspaces(workspaces);
                    picker.delegate.set_folders(folders, assignments);
                    picker.update_matches(picker.query(cx), window, cx)
                })
            })
            .ok();
        })
        .detach();
        Self {
            picker,
            _subscriptions: subscriptions,
        }
    }

    pub fn open(
        workspace: &mut Workspace,
        create_new_window: Option<bool>,
        window_project_groups: Vec<ProjectGroupKey>,
        window: &mut Window,
        focus_handle: FocusHandle,
        cx: &mut Context<Workspace>,
    ) {
        let weak = cx.entity().downgrade();
        let open_folders = get_open_folders(workspace, cx);
        let fs = Some(workspace.app_state().fs.clone());

        let create_new_window = create_new_window.unwrap_or_else(|| default_open_in_new_window(cx));

        workspace.toggle_modal(window, cx, |window, cx| {
            let delegate = RecentProjectsDelegate::new(
                weak,
                create_new_window,
                focus_handle,
                open_folders,
                window_project_groups,
                ProjectPickerStyle::Modal,
            );

            Self::new(delegate, fs, 42., window, cx)
        })
    }

    pub fn popover(
        workspace: WeakEntity<Workspace>,
        window_project_groups: Vec<ProjectGroupKey>,
        create_new_window: Option<bool>,
        focus_handle: FocusHandle,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let (open_folders, fs) = workspace
            .upgrade()
            .map(|workspace| {
                let workspace = workspace.read(cx);
                (
                    get_open_folders(workspace, cx),
                    Some(workspace.app_state().fs.clone()),
                )
            })
            .unwrap_or_else(|| (Vec::new(), None));

        let create_new_window = create_new_window.unwrap_or_else(|| default_open_in_new_window(cx));

        cx.new(|cx| {
            let delegate = RecentProjectsDelegate::new(
                workspace,
                create_new_window,
                focus_handle,
                open_folders,
                window_project_groups,
                ProjectPickerStyle::Popover,
            );
            let list = Self::new(delegate, fs, 20., window, cx);
            list.picker.focus_handle(cx).focus(window, cx);
            list
        })
    }

    fn handle_toggle_open_menu(
        &mut self,
        _: &ToggleActionsMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.picker.update(cx, |picker, cx| {
            let menu_handle = &picker.delegate.actions_menu_handle;
            if menu_handle.is_deployed() {
                menu_handle.hide(cx);
            } else {
                menu_handle.show(window, cx);
            }
        });
    }

    fn handle_remove_selected(
        &mut self,
        _: &RemoveSelected,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.picker.update(cx, |picker, cx| {
            let ix = picker.delegate.selected_index;

            match picker.delegate.filtered_entries.get(ix) {
                Some(ProjectPickerEntry::OpenFolder { index, .. }) => {
                    if let Some(worktree_id) = picker
                        .delegate
                        .open_folders
                        .get(*index)
                        .map(|f| f.worktree_id)
                    {
                        RecentProjectsDelegate::remove_open_folder(picker, worktree_id, window, cx);
                    }
                }
                Some(ProjectPickerEntry::ProjectGroup(hit)) => {
                    if let Some(key) = picker
                        .delegate
                        .window_project_groups
                        .get(hit.candidate_id)
                        .cloned()
                    {
                        picker.delegate.remove_project_group(key, window, cx);
                        let query = picker.query(cx);
                        picker.update_matches(query, window, cx);
                    }
                }
                Some(ProjectPickerEntry::RecentProject(_)) => {
                    picker.delegate.delete_recent_project(ix, window, cx);
                }
                Some(ProjectPickerEntry::Folder { folder_id, .. }) => {
                    picker.delegate.delete_user_folder(*folder_id, window, cx);
                }
                _ => {}
            }
        });
    }

    fn handle_add_to_workspace(
        &mut self,
        _: &AddToWorkspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.picker.update(cx, |picker, cx| {
            let ix = picker.delegate.selected_index;

            if let Some(ProjectPickerEntry::RecentProject(hit)) =
                picker.delegate.filtered_entries.get(ix)
            {
                if let Some(workspace) = picker.delegate.workspaces.get(hit.candidate_id) {
                    if matches!(workspace.location, SerializedWorkspaceLocation::Local) {
                        let paths_to_add = workspace.paths.paths().to_vec();
                        picker
                            .delegate
                            .add_paths_to_project(paths_to_add, window, cx);
                    }
                }
            }
        });
    }

    fn handle_new_folder(&mut self, _: &NewFolder, window: &mut Window, cx: &mut Context<Self>) {
        self.picker.update(cx, |picker, cx| {
            picker.delegate.start_create_folder(None, window, cx);
            let query = picker.query(cx);
            picker.update_matches(query, window, cx);
            if let Some(ix) = picker.delegate.filtered_entries.iter().position(|entry| {
                matches!(entry, ProjectPickerEntry::Folder { folder_id, .. } if folder_id.0 < 0)
            }) {
                picker.set_selected_index(ix, None, false, window, cx);
            }
        });
    }

    fn handle_rename_folder(
        &mut self,
        _: &RenameFolder,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.picker.update(cx, |picker, cx| {
            let ix = picker.delegate.selected_index;
            if let Some(ProjectPickerEntry::Folder { folder_id, name }) =
                picker.delegate.filtered_entries.get(ix).cloned()
            {
                picker
                    .delegate
                    .start_rename_folder(folder_id, name, window, cx);
                let query = picker.query(cx);
                picker.update_matches(query, window, cx);
            }
        });
    }
}

impl EventEmitter<DismissEvent> for RecentProjects {}

impl Focusable for RecentProjects {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for RecentProjects {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("RecentProjects")
            .on_action(cx.listener(Self::handle_toggle_open_menu))
            .on_action(cx.listener(Self::handle_remove_selected))
            .on_action(cx.listener(Self::handle_add_to_workspace))
            .on_action(cx.listener(Self::handle_new_folder))
            .on_action(cx.listener(Self::handle_rename_folder))
            .child(self.picker.clone())
    }
}

pub struct RecentProjectsDelegate {
    workspace: WeakEntity<Workspace>,
    open_folders: Vec<OpenFolderEntry>,
    window_project_groups: Vec<ProjectGroupKey>,
    workspaces: Vec<RecentWorkspace>,
    folders: Vec<ProjectFolder>,
    assignments: Vec<ProjectFolderAssignment>,
    filtered_entries: Vec<ProjectPickerEntry>,
    selected_index: usize,
    render_paths: bool,
    create_new_window: bool,
    snap_selection_to_first_non_header_match: bool,
    focus_handle: FocusHandle,
    style: ProjectPickerStyle,
    actions_menu_handle: PopoverMenuHandle<ContextMenu>,
    move_to_folder_menu_handle: PopoverMenuHandle<ContextMenu>,
    footer_move_to_folder_menu_handle: PopoverMenuHandle<ContextMenu>,
    folder_name_editor: Option<Entity<Editor>>,
    folder_edit: Option<FolderEdit>,
    folder_edit_error: Option<SharedString>,
    pending_assign_after_create: Option<PendingFolderAssign>,
}

impl RecentProjectsDelegate {
    fn new(
        workspace: WeakEntity<Workspace>,
        create_new_window: bool,
        focus_handle: FocusHandle,
        open_folders: Vec<OpenFolderEntry>,
        window_project_groups: Vec<ProjectGroupKey>,
        style: ProjectPickerStyle,
    ) -> Self {
        let render_paths = style == ProjectPickerStyle::Modal;
        Self {
            workspace,
            open_folders,
            window_project_groups,
            workspaces: Vec::new(),
            folders: Vec::new(),
            assignments: Vec::new(),
            filtered_entries: Vec::new(),
            selected_index: 0,
            create_new_window,
            render_paths,
            snap_selection_to_first_non_header_match: true,
            focus_handle,
            style,
            actions_menu_handle: PopoverMenuHandle::default(),
            move_to_folder_menu_handle: PopoverMenuHandle::default(),
            footer_move_to_folder_menu_handle: PopoverMenuHandle::default(),
            folder_name_editor: None,
            folder_edit: None,
            folder_edit_error: None,
            pending_assign_after_create: None,
        }
    }

    pub fn set_workspaces(&mut self, workspaces: Vec<RecentWorkspace>) {
        self.workspaces = workspaces;
    }

    fn set_folders(
        &mut self,
        folders: Vec<ProjectFolder>,
        assignments: Vec<ProjectFolderAssignment>,
    ) {
        self.folders = folders;
        self.assignments = assignments;
    }

    fn is_editing_folder(&self, folder_id: ProjectFolderId) -> bool {
        self.folder_edit == Some(FolderEdit::Rename(folder_id))
    }

    fn is_creating_folder(&self) -> bool {
        self.folder_edit == Some(FolderEdit::Create)
    }

    fn folder_id_for_identity(
        &self,
        remote_identity: &str,
        identity_paths: &PathList,
    ) -> Option<ProjectFolderId> {
        self.assignments.iter().find_map(|assignment| {
            (assignment.remote_identity == remote_identity
                && assignment.identity_paths == *identity_paths)
                .then_some(assignment.folder_id)
        })
    }

    fn folder_id_for_workspace(&self, workspace: &RecentWorkspace) -> Option<ProjectFolderId> {
        self.folder_id_for_identity(
            &workspace.project_folder_identity(),
            &workspace.identity_paths,
        )
    }

    fn folder_id_for_project_group(&self, key: &ProjectGroupKey) -> Option<ProjectFolderId> {
        self.folder_id_for_identity(&folder_identity_for_group(key), key.path_list())
    }

    fn selected_folder_assign(&self) -> Option<PendingFolderAssign> {
        match self.filtered_entries.get(self.selected_index) {
            Some(ProjectPickerEntry::RecentProject(hit)) => self
                .workspaces
                .get(hit.candidate_id)
                .map(PendingFolderAssign::from_workspace),
            Some(ProjectPickerEntry::ProjectGroup(hit)) => self
                .window_project_groups
                .get(hit.candidate_id)
                .map(PendingFolderAssign::from_project_group),
            _ => None,
        }
    }

    fn is_draft_folder(&self, folder_id: ProjectFolderId) -> bool {
        self.is_creating_folder() && folder_id.0 < 0
    }

    fn ensure_folder_editor(&mut self, window: &mut Window, cx: &mut App) -> Entity<Editor> {
        self.folder_name_editor
            .get_or_insert_with(|| {
                cx.new(|cx| {
                    let mut editor = Editor::single_line(window, cx);
                    editor.set_placeholder_text("Folder name", window, cx);
                    editor
                })
            })
            .clone()
    }

    fn focus_folder_editor(&self, window: &mut Window, cx: &mut App) {
        if let Some(editor) = self.folder_name_editor.as_ref() {
            editor.focus_handle(cx).focus(window, cx);
        }
    }

    fn start_create_folder(
        &mut self,
        pending_assign: Option<PendingFolderAssign>,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        if self.style != ProjectPickerStyle::Modal {
            return;
        }
        self.pending_assign_after_create = pending_assign;
        self.folder_edit = Some(FolderEdit::Create);
        self.folder_edit_error = None;
        self.snap_selection_to_first_non_header_match = false;
        let editor = self.ensure_folder_editor(window, cx);
        editor.update(cx, |editor, cx| {
            editor.set_text("", window, cx);
            editor.select_all(&editor::actions::SelectAll, window, cx);
        });
        if let Some(ix) = self
            .filtered_entries
            .iter()
            .position(|entry| matches!(entry, ProjectPickerEntry::NewFolder))
        {
            self.filtered_entries[ix] = ProjectPickerEntry::Folder {
                folder_id: ProjectFolderId(-1),
                name: SharedString::from(""),
            };
            self.selected_index = ix;
        } else if !self
            .filtered_entries
            .iter()
            .any(|entry| matches!(entry, ProjectPickerEntry::Folder { folder_id, .. } if folder_id.0 < 0))
        {
            self.filtered_entries.push(ProjectPickerEntry::Folder {
                folder_id: ProjectFolderId(-1),
                name: SharedString::from(""),
            });
            self.selected_index = self.filtered_entries.len().saturating_sub(1);
        }
        self.focus_folder_editor(window, cx);
        cx.notify();
    }

    fn start_rename_folder(
        &mut self,
        folder_id: ProjectFolderId,
        name: SharedString,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        if self.style != ProjectPickerStyle::Modal || folder_id.0 < 0 {
            return;
        }
        self.pending_assign_after_create = None;
        self.folder_edit = Some(FolderEdit::Rename(folder_id));
        self.folder_edit_error = None;
        self.snap_selection_to_first_non_header_match = false;
        let editor = self.ensure_folder_editor(window, cx);
        editor.update(cx, |editor, cx| {
            editor.set_text(name, window, cx);
            editor.select_all(&editor::actions::SelectAll, window, cx);
        });
        self.focus_folder_editor(window, cx);
        cx.notify();
    }

    fn cancel_folder_edit(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let was_creating = self.is_creating_folder();
        self.folder_edit = None;
        self.folder_edit_error = None;
        self.pending_assign_after_create = None;
        if was_creating {
            self.filtered_entries
                .retain(|entry| !matches!(entry, ProjectPickerEntry::Folder { folder_id, .. } if folder_id.0 < 0));
            if self.style == ProjectPickerStyle::Modal
                && !self
                    .filtered_entries
                    .iter()
                    .any(|entry| matches!(entry, ProjectPickerEntry::NewFolder))
            {
                self.filtered_entries.push(ProjectPickerEntry::NewFolder);
            }
        }
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn commit_folder_edit(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(edit) = self.folder_edit else {
            return;
        };
        let name = self
            .folder_name_editor
            .as_ref()
            .map(|editor| editor.read(cx).text(cx))
            .unwrap_or_default();
        let name = name.trim().to_string();
        if name.is_empty() {
            self.cancel_folder_edit(window, cx);
            return;
        }
        if self.folders.iter().any(|folder| {
            folder.name.eq_ignore_ascii_case(&name)
                && match edit {
                    FolderEdit::Create => true,
                    FolderEdit::Rename(folder_id) => folder.folder_id != folder_id,
                }
        }) {
            self.folder_edit_error =
                Some(format!("A folder named \"{name}\" already exists").into());
            cx.notify();
            return;
        }

        let db = WorkspaceDb::global(cx);
        let pending_assign = self.pending_assign_after_create.take();
        self.folder_edit = None;
        self.folder_edit_error = None;
        cx.spawn_in(window, async move |this, cx| {
            let result = match edit {
                FolderEdit::Create => db.create_project_folder(name).await,
                FolderEdit::Rename(folder_id) => db.rename_project_folder(folder_id, name).await,
            };
            if let (Ok(folder), Some(pending)) = (&result, pending_assign) {
                db.assign_project_to_folder(
                    folder.folder_id,
                    pending.remote_identity,
                    pending.identity_paths,
                )
                .await
                .log_err();
            }
            let folders = db.project_folders().log_err().unwrap_or_default();
            let assignments = db
                .project_folder_assignments()
                .log_err()
                .unwrap_or_default();
            this.update_in(cx, |picker, window, cx| {
                if result.is_err() {
                    picker.delegate.folder_edit_error = Some("Could not save folder".into());
                }
                picker.delegate.set_folders(folders, assignments);
                picker.delegate.snap_selection_to_first_non_header_match = false;
                let query = picker.query(cx);
                picker.update_matches(query, window, cx);
                picker.focus_handle(cx).focus(window, cx);
            })
            .ok();
        })
        .detach();
    }

    fn delete_user_folder(
        &mut self,
        folder_id: ProjectFolderId,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        if folder_id.0 < 0 {
            self.cancel_folder_edit(window, cx);
            return;
        }
        let db = WorkspaceDb::global(cx);
        cx.spawn_in(window, async move |this, cx| {
            db.delete_project_folder(folder_id).await.log_err();
            let folders = db.project_folders().log_err().unwrap_or_default();
            let assignments = db
                .project_folder_assignments()
                .log_err()
                .unwrap_or_default();
            this.update_in(cx, |picker, window, cx| {
                picker.delegate.set_folders(folders, assignments);
                picker.delegate.snap_selection_to_first_non_header_match = false;
                let query = picker.query(cx);
                picker.update_matches(query, window, cx);
            })
            .ok();
        })
        .detach();
    }

    fn assign_identity_to_folder(
        &mut self,
        folder_id: ProjectFolderId,
        remote_identity: String,
        identity_paths: PathList,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        let db = WorkspaceDb::global(cx);
        cx.spawn_in(window, async move |this, cx| {
            db.assign_project_to_folder(folder_id, remote_identity, identity_paths)
                .await
                .log_err();
            let folders = db.project_folders().log_err().unwrap_or_default();
            let assignments = db
                .project_folder_assignments()
                .log_err()
                .unwrap_or_default();
            this.update_in(cx, |picker, window, cx| {
                picker.delegate.set_folders(folders, assignments);
                picker.delegate.snap_selection_to_first_non_header_match = false;
                let query = picker.query(cx);
                picker.update_matches(query, window, cx);
            })
            .ok();
        })
        .detach();
    }

    fn unassign_identity_from_folder(
        &mut self,
        remote_identity: String,
        identity_paths: PathList,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        let db = WorkspaceDb::global(cx);
        cx.spawn_in(window, async move |this, cx| {
            db.unassign_project_from_folder(remote_identity, identity_paths)
                .await
                .log_err();
            let folders = db.project_folders().log_err().unwrap_or_default();
            let assignments = db
                .project_folder_assignments()
                .log_err()
                .unwrap_or_default();
            this.update_in(cx, |picker, window, cx| {
                picker.delegate.set_folders(folders, assignments);
                picker.delegate.snap_selection_to_first_non_header_match = false;
                let query = picker.query(cx);
                picker.update_matches(query, window, cx);
            })
            .ok();
        })
        .detach();
    }

    fn filtered_entries_include_remote_project(&self) -> bool {
        self.filtered_entries
            .iter()
            .any(|entry| self.entry_is_remote_project(entry))
    }

    fn entry_is_remote_project(&self, entry: &ProjectPickerEntry) -> bool {
        match entry {
            ProjectPickerEntry::Header(_)
            | ProjectPickerEntry::Folder { .. }
            | ProjectPickerEntry::NewFolder => false,
            ProjectPickerEntry::OpenFolder { index, .. } => self
                .open_folders
                .get(*index)
                .is_some_and(|folder| folder.connection_options.is_some()),
            ProjectPickerEntry::ProjectGroup(hit) => self
                .window_project_groups
                .get(hit.candidate_id)
                .is_some_and(|key| key.host().is_some()),
            ProjectPickerEntry::RecentProject(hit) => self
                .workspaces
                .get(hit.candidate_id)
                .is_some_and(|workspace| {
                    matches!(workspace.location, SerializedWorkspaceLocation::Remote(_))
                }),
        }
    }
}
impl EventEmitter<DismissEvent> for RecentProjectsDelegate {}
impl PickerDelegate for RecentProjectsDelegate {
    type ListItem = AnyElement;

    fn name() -> &'static str {
        "recent projects"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search projects…".into()
    }

    fn match_count(&self) -> usize {
        self.filtered_entries.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn can_select(&self, ix: usize, _window: &mut Window, _cx: &mut Context<Picker<Self>>) -> bool {
        self.filtered_entries
            .get(ix)
            .is_some_and(is_selectable_entry)
    }

    fn has_another_open_menu(&self, window: &Window, cx: &App) -> bool {
        self.actions_menu_handle.is_focused(window, cx)
            || self.actions_menu_handle.is_deployed()
            || self.move_to_folder_menu_handle.is_focused(window, cx)
            || self.move_to_folder_menu_handle.is_deployed()
            || self.footer_move_to_folder_menu_handle.is_focused(window, cx)
            || self.footer_move_to_folder_menu_handle.is_deployed()
            || self.folder_edit.is_some()
            || self
                .folder_name_editor
                .as_ref()
                .is_some_and(|editor| editor.focus_handle(cx).is_focused(window))
    }

    fn update_matches(
        &mut self,
        query: String,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> gpui::Task<()> {
        let query = query.trim_start();
        let case = fuzzy_nucleo::Case::smart_if_uppercase_in(query);
        let is_empty_query = query.is_empty();

        let folder_matches = if self.open_folders.is_empty() {
            Vec::new()
        } else {
            let candidates: Vec<_> = self
                .open_folders
                .iter()
                .enumerate()
                .map(|(id, folder)| StringMatchCandidate::new(id, folder.name.as_ref()))
                .collect();

            match_strings(
                &candidates,
                query,
                case,
                fuzzy_nucleo::LengthPenalty::On,
                100,
            )
        };

        let project_group_candidates: Vec<_> = self
            .window_project_groups
            .iter()
            .enumerate()
            .map(|(id, key)| StringMatchCandidate::new(id, &path_list_search_blob(key.path_list())))
            .collect();

        let project_group_matches = match_strings(
            &project_group_candidates,
            query,
            case,
            fuzzy_nucleo::LengthPenalty::On,
            100,
        );

        // Match every recent workspace, including the currently open one. Assigned
        // open projects still need to appear under their folder when searching.
        let recent_candidates: Vec<_> = self
            .workspaces
            .iter()
            .enumerate()
            .map(|(id, workspace)| {
                StringMatchCandidate::new(id, &path_list_search_blob(&workspace.identity_paths))
            })
            .collect();

        let recent_matches = match_strings(
            &recent_candidates,
            query,
            case,
            fuzzy_nucleo::LengthPenalty::On,
            100,
        );

        let mut entries = Vec::new();

        if !self.open_folders.is_empty() {
            let matched_folders: Vec<_> = if is_empty_query {
                (0..self.open_folders.len())
                    .map(|i| (i, Vec::new()))
                    .collect()
            } else {
                folder_matches
                    .iter()
                    .map(|m| (m.candidate_id, m.positions.clone()))
                    .collect()
            };

            if !matched_folders.is_empty() {
                entries.push(ProjectPickerEntry::Header("Current Folders".into()));
                for (index, positions) in matched_folders {
                    entries.push(ProjectPickerEntry::OpenFolder { index, positions });
                }
            }
        }

        let has_projects_to_show = if is_empty_query {
            self.window_project_groups.iter().any(|key| self.folder_id_for_project_group(key).is_none())
        } else {
            project_group_matches.iter().any(|m| {
                self.window_project_groups
                    .get(m.candidate_id)
                    .is_some_and(|key| self.folder_id_for_project_group(key).is_none())
            })
        };

        if has_projects_to_show {
            entries.push(ProjectPickerEntry::Header("This Window".into()));

            if is_empty_query {
                for id in 0..self.window_project_groups.len() {
                    if self
                        .window_project_groups
                        .get(id)
                        .is_some_and(|key| self.folder_id_for_project_group(key).is_some())
                    {
                        continue;
                    }
                    entries.push(ProjectPickerEntry::ProjectGroup(StringMatch {
                        candidate_id: id,
                        score: 0.0,
                        positions: Vec::new(),
                        string: Default::default(),
                    }));
                }
            } else {
                for m in &project_group_matches {
                    if self
                        .window_project_groups
                        .get(m.candidate_id)
                        .is_some_and(|key| self.folder_id_for_project_group(key).is_some())
                    {
                        continue;
                    }
                    entries.push(ProjectPickerEntry::ProjectGroup(m.clone()));
                }
            }
        }

        let recent_by_id: HashMap<usize, StringMatch> = if is_empty_query {
            HashMap::default()
        } else {
            recent_matches
                .into_iter()
                .map(|m| (m.candidate_id, m))
                .collect()
        };
        let project_group_by_id: HashMap<usize, StringMatch> = if is_empty_query {
            HashMap::default()
        } else {
            project_group_matches
                .iter()
                .cloned()
                .map(|m| (m.candidate_id, m))
                .collect()
        };

        let valid_recent_ids: Vec<usize> = self
            .workspaces
            .iter()
            .enumerate()
            .filter(|(_, workspace)| self.is_valid_recent_candidate(workspace, cx))
            .map(|(id, _)| id)
            .collect();

        let matching_recent_ids: HashSet<usize> = if is_empty_query {
            self.workspaces.iter().enumerate().map(|(id, _)| id).collect()
        } else {
            recent_by_id.keys().copied().collect()
        };
        let matching_project_group_ids: HashSet<usize> = if is_empty_query {
            (0..self.window_project_groups.len()).collect()
        } else {
            project_group_by_id.keys().copied().collect()
        };

        let mut assigned_ids: HashSet<usize> = HashSet::default();
        let mut members_by_folder: HashMap<ProjectFolderId, Vec<(i64, usize)>> = HashMap::default();
        for assignment in &self.assignments {
            if let Some(workspace_id) = self.workspaces.iter().enumerate().find_map(|(id, workspace)| {
                (workspace.project_folder_identity() == assignment.remote_identity
                    && workspace.identity_paths == assignment.identity_paths)
                    .then_some(id)
            }) {
                assigned_ids.insert(workspace_id);
                members_by_folder
                    .entry(assignment.folder_id)
                    .or_default()
                    .push((assignment.position, workspace_id));
            }
        }
        for members in members_by_folder.values_mut() {
            members.sort_by_key(|(position, _)| *position);
        }

        let assigned_window_groups_by_folder: HashMap<ProjectFolderId, Vec<usize>> = {
            let mut grouped: HashMap<ProjectFolderId, Vec<usize>> = HashMap::default();
            for (group_ix, key) in self.window_project_groups.iter().enumerate() {
                let Some(folder_id) = self.folder_id_for_project_group(key) else {
                    continue;
                };
                let represented_by_workspace = self.workspaces.iter().any(|workspace| {
                    workspace.project_folder_identity() == folder_identity_for_group(key)
                        && workspace.identity_paths == *key.path_list()
                });
                if represented_by_workspace {
                    continue;
                }
                grouped.entry(folder_id).or_default().push(group_ix);
            }
            grouped
        };

        let folder_name_matches: HashSet<ProjectFolderId> = if is_empty_query {
            HashSet::default()
        } else {
            let folder_candidates: Vec<_> = self
                .folders
                .iter()
                .enumerate()
                .map(|(id, folder)| StringMatchCandidate::new(id, folder.name.as_str()))
                .collect();
            match_strings(
                &folder_candidates,
                query,
                case,
                fuzzy_nucleo::LengthPenalty::On,
                100,
            )
            .into_iter()
            .filter_map(|m| self.folders.get(m.candidate_id).map(|folder| folder.folder_id))
            .collect()
        };

        for folder in &self.folders {
            let members = members_by_folder
                .get(&folder.folder_id)
                .cloned()
                .unwrap_or_default();
            let window_group_members = assigned_window_groups_by_folder
                .get(&folder.folder_id)
                .cloned()
                .unwrap_or_default();
            let folder_name_matched =
                is_empty_query || folder_name_matches.contains(&folder.folder_id);
            let visible_members: Vec<usize> = members
                .into_iter()
                .map(|(_, workspace_id)| workspace_id)
                .filter(|workspace_id| {
                    is_empty_query
                        || folder_name_matched
                        || matching_recent_ids.contains(workspace_id)
                })
                .collect();
            let visible_window_groups: Vec<usize> = window_group_members
                .into_iter()
                .filter(|group_ix| {
                    is_empty_query
                        || folder_name_matched
                        || matching_project_group_ids.contains(group_ix)
                })
                .collect();

            if !is_empty_query
                && visible_members.is_empty()
                && visible_window_groups.is_empty()
                && !folder_name_matched
            {
                continue;
            }

            entries.push(ProjectPickerEntry::Folder {
                folder_id: folder.folder_id,
                name: folder.name.clone().into(),
            });
            for workspace_id in visible_members {
                entries.push(ProjectPickerEntry::RecentProject(recent_project_match(
                    workspace_id,
                    &recent_by_id,
                )));
            }
            for group_ix in visible_window_groups {
                entries.push(ProjectPickerEntry::ProjectGroup(recent_project_match(
                    group_ix,
                    &project_group_by_id,
                )));
            }
        }

        if self.is_creating_folder() {
            entries.push(ProjectPickerEntry::Folder {
                folder_id: ProjectFolderId(-1),
                name: SharedString::from(""),
            });
        }

        let ungrouped_ids: Vec<usize> = valid_recent_ids
            .iter()
            .copied()
            .filter(|id| !assigned_ids.contains(id))
            .filter(|id| is_empty_query || matching_recent_ids.contains(id))
            .collect();

        if !ungrouped_ids.is_empty() {
            entries.push(ProjectPickerEntry::Header("Ungrouped".into()));
            for workspace_id in ungrouped_ids {
                entries.push(ProjectPickerEntry::RecentProject(recent_project_match(
                    workspace_id,
                    &recent_by_id,
                )));
            }
        }

        if self.style == ProjectPickerStyle::Modal && is_empty_query && !self.is_creating_folder() {
            entries.push(ProjectPickerEntry::NewFolder);
        }

        self.filtered_entries = entries;

        if self.snap_selection_to_first_non_header_match {
            self.selected_index = self
                .filtered_entries
                .iter()
                .position(|e| !matches!(e, ProjectPickerEntry::Header(_)))
                .unwrap_or(0);
        }
        self.snap_selection_to_first_non_header_match = true;
        Task::ready(())
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        match self.filtered_entries.get(self.selected_index) {
            Some(ProjectPickerEntry::OpenFolder { index, .. }) => {
                let Some(folder) = self.open_folders.get(*index) else {
                    return;
                };
                let worktree_id = folder.worktree_id;
                if let Some(workspace) = self.workspace.upgrade() {
                    workspace.update(cx, |workspace, cx| {
                        let git_store = workspace.project().read(cx).git_store().clone();
                        git_store.update(cx, |git_store, cx| {
                            git_store.set_active_repo_for_worktree(worktree_id, cx);
                        });
                    });
                }
                cx.emit(DismissEvent);
            }
            Some(ProjectPickerEntry::ProjectGroup(selected_match)) => {
                let Some(key) = self.window_project_groups.get(selected_match.candidate_id) else {
                    return;
                };

                if secondary && key.host().is_none() && self.window_project_groups.len() >= 2 {
                    move_project_group_to_new_window(key, window, cx);
                    cx.emit(DismissEvent);
                    return;
                }

                let key = key.clone();
                if let Some(handle) = window.window_handle().downcast::<MultiWorkspace>() {
                    cx.defer(move |cx| {
                        // Try to activate an existing workspace for this project group
                        // first, so we preserve the actual worktree paths (which may
                        // differ from the main git worktree paths stored in the key).
                        if let Some(workspace) = handle
                            .update(cx, |multi_workspace, _window, cx| {
                                multi_workspace.last_active_workspace_for_group(&key, cx)
                            })
                            .log_err()
                            .flatten()
                        {
                            handle
                                .update(cx, |multi_workspace, window, cx| {
                                    multi_workspace.activate(workspace, None, window, cx);
                                })
                                .log_err();
                        } else {
                            let path_list = key.path_list().clone();
                            let host = key.host();
                            if let Some(task) = handle
                                .update(cx, |multi_workspace, window, cx| {
                                    let modal_workspace = multi_workspace.workspace().clone();
                                    multi_workspace.find_or_create_workspace(
                                        path_list,
                                        host,
                                        Some(key.clone()),
                                        move |options, window, cx| {
                                            connect_with_modal(
                                                &modal_workspace,
                                                options,
                                                window,
                                                cx,
                                            )
                                        },
                                        None,
                                        OpenMode::Activate,
                                        None,
                                        window,
                                        cx,
                                    )
                                })
                                .log_err()
                            {
                                task.detach_and_log_err(cx);
                            }
                        }
                    });
                }
                cx.emit(DismissEvent);
            }
            Some(ProjectPickerEntry::RecentProject(selected_match)) => {
                let candidate_id = selected_match.candidate_id;
                self.open_recent_projects(candidate_id, secondary, window, cx);
            }
            Some(ProjectPickerEntry::Folder { folder_id, name }) => {
                if self.is_creating_folder() || self.is_editing_folder(*folder_id) {
                    return;
                }
                let folder_id = *folder_id;
                let name = name.clone();
                if secondary {
                    self.start_rename_folder(folder_id, name, window, cx);
                    return;
                }
                let child_ix = self.selected_index + 1;
                match self.filtered_entries.get(child_ix) {
                    Some(ProjectPickerEntry::RecentProject(_) | ProjectPickerEntry::ProjectGroup(_)) => {
                        self.selected_index = child_ix;
                        self.confirm(secondary, window, cx);
                    }
                    _ => {}
                }
            }
            Some(ProjectPickerEntry::NewFolder) => {
                self.start_create_folder(None, window, cx);
            }
            _ => {}
        }
    }

    fn dismissed(&mut self, _window: &mut Window, _: &mut Context<Picker<Self>>) {}

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        let text = if self.workspaces.is_empty() && self.open_folders.is_empty() {
            "Recently opened projects will show up here".into()
        } else {
            "No matches".into()
        };
        Some(text)
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        match self.filtered_entries.get(ix)? {
            ProjectPickerEntry::Header(title) => Some(
                v_flex()
                    .w_full()
                    .gap_1()
                    .when(ix > 0, |this| this.mt_1().child(Divider::horizontal()))
                    .child(ListSubHeader::new(title.clone()).inset(true))
                    .into_any_element(),
            ),
            ProjectPickerEntry::Folder { folder_id, name } => {
                let folder_id = *folder_id;
                let name = name.clone();
                let editing = self.is_draft_folder(folder_id) || self.is_editing_folder(folder_id);
                let show_folder_actions = self.style == ProjectPickerStyle::Modal && !editing;
                let editor = self.folder_name_editor.clone();
                let error = self.folder_edit_error.clone();

                let secondary_actions = h_flex()
                    .gap_px()
                    .child(
                        IconButton::new(("rename-folder", folder_id.0 as usize), IconName::Pencil)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Rename Folder"))
                            .on_click({
                                let name = name.clone();
                                cx.listener(move |picker, _, window, cx| {
                                    cx.stop_propagation();
                                    window.prevent_default();
                                    picker.delegate.start_rename_folder(
                                        folder_id,
                                        name.clone(),
                                        window,
                                        cx,
                                    );
                                })
                            }),
                    )
                    .child(
                        IconButton::new(("delete-folder", folder_id.0 as usize), IconName::Close)
                            .icon_size(IconSize::Small)
                            .tooltip({
                                let focus_handle = self.focus_handle.clone();
                                move |_, cx| {
                                    Tooltip::for_action_in(
                                        "Remove Folder",
                                        &RemoveSelected,
                                        &focus_handle,
                                        cx,
                                    )
                                }
                            })
                            .on_click(cx.listener(move |picker, _, window, cx| {
                                cx.stop_propagation();
                                window.prevent_default();
                                picker.delegate.delete_user_folder(folder_id, window, cx);
                            })),
                    );

                Some(
                    v_flex()
                        .w_full()
                        .when(ix > 0, |this| this.mt_1().child(Divider::horizontal()))
                        .child(
                            ListItem::new(ix)
                                .toggle_state(selected)
                                .inset(true)
                                .spacing(ListItemSpacing::Sparse)
                                .child(if editing {
                                    v_flex()
                                        .w_full()
                                        .min_w_0()
                                        .capture_action(cx.listener(
                                            |picker, _: &menu::Confirm, window, cx| {
                                                picker.delegate.commit_folder_edit(window, cx);
                                            },
                                        ))
                                        .capture_action(cx.listener(
                                            |picker, _: &editor::actions::Newline, window, cx| {
                                                picker.delegate.commit_folder_edit(window, cx);
                                            },
                                        ))
                                        .capture_action(cx.listener(
                                            |picker, _: &editor::actions::Cancel, window, cx| {
                                                picker.delegate.cancel_folder_edit(window, cx);
                                            },
                                        ))
                                        .children(editor)
                                        .when_some(error, |this, error| {
                                            this.child(
                                                Label::new(error)
                                                    .size(LabelSize::Small)
                                                    .color(Color::Error),
                                            )
                                        })
                                        .into_any_element()
                                } else {
                                    Label::new(name)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .into_any_element()
                                })
                                .when(show_folder_actions, |this| {
                                    this.end_slot(secondary_actions)
                                        .when(!selected, |this| this.show_end_slot_on_hover())
                                }),
                        )
                        .into_any_element(),
                )
            }
            ProjectPickerEntry::NewFolder => Some(
                ListItem::new(ix)
                    .toggle_state(selected)
                    .inset(true)
                    .spacing(ListItemSpacing::Sparse)
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Icon::new(IconName::Plus)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new("New Folder")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    )
                    .into_any_element(),
            ),
            ProjectPickerEntry::OpenFolder { index, positions } => {
                let folder = self.open_folders.get(*index)?;
                let name = folder.name.clone();
                let path = folder.path.compact();
                let branch = folder.branch.clone();
                let is_active = folder.is_active;
                let worktree_id = folder.worktree_id;
                let positions = positions.clone();
                let show_path = self.style == ProjectPickerStyle::Modal;

                let secondary_actions = h_flex()
                    .gap_1()
                    .child(
                        IconButton::new(("remove-folder", worktree_id.to_usize()), IconName::Close)
                            .icon_size(IconSize::Small)
                            .tooltip({
                                let focus_handle = self.focus_handle.clone();
                                move |_, cx| {
                                    Tooltip::for_action_in(
                                        "Remove Folder from Project",
                                        &RemoveSelected,
                                        &focus_handle,
                                        cx,
                                    )
                                }
                            })
                            .on_click(cx.listener(move |picker, _, window, cx| {
                                RecentProjectsDelegate::remove_open_folder(
                                    picker,
                                    worktree_id,
                                    window,
                                    cx,
                                );
                            })),
                    )
                    .into_any_element();

                let icon = icon_for_remote_connection(folder.connection_options.as_ref());
                let show_icon = self.filtered_entries_include_remote_project();

                let tooltip_path: SharedString = path.to_string_lossy().to_string().into();
                let tooltip_branch = branch.clone();

                Some(
                    ListItem::new(ix)
                        .toggle_state(selected)
                        .inset(true)
                        .spacing(ListItemSpacing::Sparse)
                        .child(
                            h_flex()
                                .id("open_folder_item")
                                .w_full()
                                .min_w_0()
                                .gap_2p5()
                                .when(show_icon, |this| {
                                    this.child(Icon::new(icon).color(Color::Muted))
                                })
                                .child(
                                    v_flex()
                                        .min_w_0()
                                        .child(
                                            h_flex()
                                                .gap_1()
                                                .child(HighlightedLabel::new(
                                                    name.to_string(),
                                                    positions,
                                                ))
                                                .when_some(branch, |this, branch| {
                                                    this.child(
                                                        Label::new(branch)
                                                            .color(Color::Muted)
                                                            .truncate(),
                                                    )
                                                })
                                                .when(is_active, |this| {
                                                    this.child(
                                                        Icon::new(IconName::Check)
                                                            .size(IconSize::Small)
                                                            .color(Color::Accent),
                                                    )
                                                }),
                                        )
                                        .when(show_path, |this| {
                                            this.child(
                                                Label::new(path.to_string_lossy().to_string())
                                                    .size(LabelSize::Small)
                                                    .color(Color::Muted),
                                            )
                                        }),
                                )
                                .when(!show_path, |this| {
                                    this.tooltip(move |_, cx| {
                                        if let Some(branch) = tooltip_branch.clone() {
                                            Tooltip::with_meta(
                                                format!("{}/{}", name, branch),
                                                None,
                                                tooltip_path.clone(),
                                                cx,
                                            )
                                        } else {
                                            Tooltip::simple(tooltip_path.clone(), cx)
                                        }
                                    })
                                }),
                        )
                        .end_slot(secondary_actions)
                        .when(!selected, |this| this.show_end_slot_on_hover())
                        .into_any_element(),
                )
            }
            ProjectPickerEntry::ProjectGroup(hit) => {
                let key = self.window_project_groups.get(hit.candidate_id)?;
                let is_active = self.is_active_project_group(key, cx);
                let paths = key.path_list();
                let ordered_paths: Vec<_> = paths
                    .ordered_paths()
                    .map(|p| p.compact().to_string_lossy().to_string())
                    .collect();
                let tooltip_path: SharedString = ordered_paths.join("\n").into();
                let icon = icon_for_project_group(key);
                let show_icon = self.filtered_entries_include_remote_project();

                let mut path_start_offset = 0;
                let (match_labels, path_highlights): (Vec<_>, Vec<_>) = paths
                    .ordered_paths()
                    .map(|p| p.compact())
                    .map(|path| {
                        let highlighted_text =
                            highlights_for_path(path.as_ref(), &hit.positions, path_start_offset);
                        path_start_offset += highlighted_text.1.text.len();
                        highlighted_text
                    })
                    .unzip();

                let highlighted_match = HighlightedMatchWithPaths {
                    prefix: None,
                    match_label: HighlightedMatch::join(match_labels.into_iter().flatten(), ", "),
                    paths: path_highlights,
                    active: is_active,
                };

                let project_group_key = key.clone();
                let is_local = key.host().is_none();
                let has_multiple_groups = self.window_project_groups.len() >= 2;
                let show_move_to_folder = self.style == ProjectPickerStyle::Modal;
                let pending_assign = PendingFolderAssign::from_project_group(key);
                let current_folder_id = self.folder_id_for_project_group(key);
                let user_folders = self.folders.clone();
                let move_to_folder_menu_handle = self.move_to_folder_menu_handle.clone();
                let group_index = hit.candidate_id;
                let secondary_actions = h_flex()
                    .gap_0p5()
                    .when(show_move_to_folder, |this| {
                        this.child(move_to_folder_popover(
                            ("move-to-folder-group", group_index),
                            ("move-to-folder-group-trigger", group_index),
                            move_to_folder_menu_handle,
                            cx.entity(),
                            user_folders,
                            current_folder_id,
                            pending_assign,
                        ))
                    })
                    .when(is_local && has_multiple_groups, |this| {
                        this.child(
                            IconButton::new("move_to_new_window", IconName::ArrowUpRight)
                                .icon_size(IconSize::Small)
                                .tooltip({
                                    let focus_handle = self.focus_handle.clone();
                                    move |_, cx| {
                                        Tooltip::for_action_in(
                                            "Open in New Window",
                                            &menu::SecondaryConfirm,
                                            &focus_handle,
                                            cx,
                                        )
                                    }
                                })
                                .on_click({
                                    let project_group_key = project_group_key.clone();
                                    cx.listener(move |_picker, _, window, cx| {
                                        cx.stop_propagation();
                                        window.prevent_default();
                                        move_project_group_to_new_window(
                                            &project_group_key,
                                            window,
                                            cx,
                                        );
                                        cx.emit(DismissEvent);
                                    })
                                }),
                        )
                    })
                    .child(
                        IconButton::new("remove_open_project", IconName::Close)
                            .icon_size(IconSize::Small)
                            .tooltip({
                                let focus_handle = self.focus_handle.clone();
                                move |_, cx| {
                                    Tooltip::for_action_in(
                                        "Remove Project from Window",
                                        &RemoveSelected,
                                        &focus_handle,
                                        cx,
                                    )
                                }
                            })
                            .on_click({
                                cx.listener(move |picker, _, window, cx| {
                                    cx.stop_propagation();
                                    window.prevent_default();
                                    picker.delegate.remove_project_group(
                                        project_group_key.clone(),
                                        window,
                                        cx,
                                    );
                                    let query = picker.query(cx);
                                    picker.update_matches(query, window, cx);
                                })
                            }),
                    )
                    .into_any_element();

                Some(
                    ListItem::new(ix)
                        .inset(true)
                        .toggle_state(selected)
                        .spacing(ListItemSpacing::Sparse)
                        .child(
                            h_flex()
                                .id("open_project_info_container")
                                .w_full()
                                .min_w_0()
                                .gap_2p5()
                                .when(show_icon, |this| {
                                    this.child(Icon::new(icon).color(Color::Muted))
                                })
                                .child({
                                    let mut highlighted = highlighted_match;
                                    if !self.render_paths {
                                        highlighted.paths.clear();
                                    }
                                    highlighted.render(window, cx)
                                })
                                .tooltip(Tooltip::text(tooltip_path)),
                        )
                        .end_slot(secondary_actions)
                        .when(!selected, |this| this.show_end_slot_on_hover())
                        .into_any_element(),
                )
            }
            ProjectPickerEntry::RecentProject(hit) => {
                let workspace = self.workspaces.get(hit.candidate_id)?;
                let location = &workspace.location;
                let raw_paths = &workspace.paths;
                let identity_paths = &workspace.identity_paths;
                let is_local = matches!(location, SerializedWorkspaceLocation::Local);
                let paths_to_add = raw_paths.paths().to_vec();
                let ordered_paths: Vec<_> = identity_paths
                    .ordered_paths()
                    .map(|p| p.compact().to_string_lossy().to_string())
                    .collect();
                let tooltip_path: SharedString = match &location {
                    SerializedWorkspaceLocation::Remote(options) => {
                        let host = options.display_name();
                        if ordered_paths.len() == 1 {
                            format!("{} ({})", ordered_paths[0], host).into()
                        } else {
                            format!("{}\n({})", ordered_paths.join("\n"), host).into()
                        }
                    }
                    _ => ordered_paths.join("\n").into(),
                };

                let mut path_start_offset = 0;
                let (match_labels, paths): (Vec<_>, Vec<_>) = identity_paths
                    .ordered_paths()
                    .map(|p| p.compact())
                    .map(|path| {
                        let highlighted_text =
                            highlights_for_path(path.as_ref(), &hit.positions, path_start_offset);
                        path_start_offset += highlighted_text.1.text.len();
                        highlighted_text
                    })
                    .unzip();

                let tooltip_title = if paths.len() > 1 {
                    "Add Folders to this Project"
                } else {
                    "Add Folder to this Project"
                };

                let prefix = match &location {
                    SerializedWorkspaceLocation::Remote(options) => {
                        Some(SharedString::from(options.display_name()))
                    }
                    _ => None,
                };

                let highlighted_match = HighlightedMatchWithPaths {
                    prefix,
                    match_label: HighlightedMatch::join(match_labels.into_iter().flatten(), ", "),
                    paths,
                    active: false,
                };

                let focus_handle = self.focus_handle.clone();
                let secondary_confirm_tooltip = if self.create_new_window {
                    "Open Project in This Window"
                } else {
                    "Open Project in New Window"
                };
                let primary_confirm_tooltip = if self.create_new_window {
                    "Open Project in New Window"
                } else {
                    "Open Project in This Window"
                };
                let secondary_confirm_icon = if self.create_new_window {
                    IconName::ThisWindow
                } else {
                    IconName::ArrowUpRight
                };

                let workspace_index = hit.candidate_id;
                let current_folder_id = self.folder_id_for_workspace(workspace);
                let user_folders = self.folders.clone();
                let show_move_to_folder = self.style == ProjectPickerStyle::Modal;
                let move_to_folder_menu_handle = self.move_to_folder_menu_handle.clone();
                let pending_assign = PendingFolderAssign::from_workspace(workspace);

                let secondary_actions = h_flex()
                    .gap_px()
                    .when(is_local, |this| {
                        this.child(
                            IconButton::new("add_to_workspace", IconName::FolderInclude)
                                .icon_size(IconSize::Small)
                                .tooltip({
                                    let focus_handle = self.focus_handle.clone();
                                    move |_, cx| {
                                        Tooltip::with_meta_in(
                                            tooltip_title,
                                            Some(&AddToWorkspace),
                                            "As a multi-root folder",
                                            &focus_handle,
                                            cx,
                                        )
                                    }
                                })
                                .on_click({
                                    let paths_to_add = paths_to_add.clone();
                                    cx.listener(move |picker, _event, window, cx| {
                                        cx.stop_propagation();
                                        window.prevent_default();
                                        picker.delegate.add_paths_to_project(
                                            paths_to_add.clone(),
                                            window,
                                            cx,
                                        );
                                    })
                                }),
                        )
                    })
                    .when(show_move_to_folder, |this| {
                        this.child(move_to_folder_popover(
                            ("move-to-folder", workspace_index),
                            ("move-to-folder-trigger", workspace_index),
                            move_to_folder_menu_handle,
                            cx.entity(),
                            user_folders,
                            current_folder_id,
                            pending_assign,
                        ))
                    })
                    .child(
                        IconButton::new("alternate_open", secondary_confirm_icon)
                            .icon_size(IconSize::Small)
                            .tooltip({
                                move |_, cx| {
                                    Tooltip::for_action_in(
                                        secondary_confirm_tooltip,
                                        &menu::SecondaryConfirm,
                                        &focus_handle,
                                        cx,
                                    )
                                }
                            })
                            .on_click(cx.listener(move |this, _event, window, cx| {
                                cx.stop_propagation();
                                window.prevent_default();
                                this.delegate.set_selected_index(ix, window, cx);
                                this.delegate.confirm(true, window, cx);
                            })),
                    )
                    .child(
                        IconButton::new("delete", IconName::Close)
                            .icon_size(IconSize::Small)
                            .tooltip({
                                let focus_handle = self.focus_handle.clone();
                                move |_, cx| {
                                    Tooltip::for_action_in(
                                        "Remove from Recent Projects",
                                        &RemoveSelected,
                                        &focus_handle,
                                        cx,
                                    )
                                }
                            })
                            .on_click(cx.listener(move |this, _event, window, cx| {
                                cx.stop_propagation();
                                window.prevent_default();
                                this.delegate.delete_recent_project(ix, window, cx)
                            })),
                    )
                    .into_any_element();

                let icon = icon_for_remote_connection(match location {
                    SerializedWorkspaceLocation::Local => None,
                    SerializedWorkspaceLocation::Remote(options) => Some(options),
                });
                let show_icon = self.filtered_entries_include_remote_project();

                Some(
                    ListItem::new(ix)
                        .toggle_state(selected)
                        .inset(true)
                        .spacing(ListItemSpacing::Sparse)
                        .child(
                            h_flex()
                                .id("project_info_container")
                                .w_full()
                                .min_w_0()
                                .gap_2p5()
                                .flex_grow_1()
                                .when(show_icon, |this| {
                                    this.child(Icon::new(icon).color(Color::Muted))
                                })
                                .child({
                                    let mut highlighted = highlighted_match;
                                    if !self.render_paths {
                                        highlighted.paths.clear();
                                    }
                                    highlighted.render(window, cx)
                                })
                                .tooltip(move |_, cx| {
                                    Tooltip::with_meta(
                                        primary_confirm_tooltip,
                                        None,
                                        tooltip_path.clone(),
                                        cx,
                                    )
                                }),
                        )
                        .end_slot(secondary_actions)
                        .when(!selected, |this| this.show_end_slot_on_hover())
                        .into_any_element(),
                )
            }
        }
    }

    fn render_footer(&self, _: &mut Window, cx: &mut Context<Picker<Self>>) -> Option<AnyElement> {
        let focus_handle = self.focus_handle.clone();
        let popover_style = matches!(self.style, ProjectPickerStyle::Popover);

        let is_already_open_entry = matches!(
            self.filtered_entries.get(self.selected_index),
            Some(ProjectPickerEntry::OpenFolder { .. } | ProjectPickerEntry::ProjectGroup(_))
        );
        let is_folder_management_entry = matches!(
            self.filtered_entries.get(self.selected_index),
            Some(ProjectPickerEntry::Folder { .. } | ProjectPickerEntry::NewFolder)
        );

        let show_move_to_new_window = match self.filtered_entries.get(self.selected_index) {
            Some(ProjectPickerEntry::ProjectGroup(hit)) => {
                self.window_project_groups.len() >= 2
                    && self
                        .window_project_groups
                        .get(hit.candidate_id)
                        .is_some_and(|key| key.host().is_none())
            }
            _ => false,
        };

        if popover_style {
            return Some(
                v_flex()
                    .flex_1()
                    .p_1p5()
                    .gap_1()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child({
                        ButtonLike::new("open_local_folder")
                            .child(
                                h_flex()
                                    .w_full()
                                    .gap_1()
                                    .justify_between()
                                    .child(Label::new("Open Local Folders"))
                                    .child(KeyBinding::for_action_in(
                                        &workspace::Open {
                                            create_new_window: Some(self.create_new_window),
                                        },
                                        &focus_handle,
                                        cx,
                                    )),
                            )
                            .on_click({
                                let workspace = self.workspace.clone();
                                let create_new_window = self.create_new_window;
                                move |_, window, cx| {
                                    open_local_project(
                                        workspace.clone(),
                                        create_new_window,
                                        window,
                                        cx,
                                    );
                                }
                            })
                    })
                    .child(
                        ButtonLike::new("open_remote_folder")
                            .child(
                                h_flex()
                                    .w_full()
                                    .gap_1()
                                    .justify_between()
                                    .child(Label::new("Open Remote Folder"))
                                    .child(KeyBinding::for_action(
                                        &OpenRemote {
                                            from_existing_connection: false,
                                            create_new_window: Some(self.create_new_window),
                                        },
                                        cx,
                                    )),
                            )
                            .on_click({
                                let create_new_window = self.create_new_window;
                                move |_, window, cx| {
                                    window.dispatch_action(
                                        OpenRemote {
                                            from_existing_connection: false,
                                            create_new_window: Some(create_new_window),
                                        }
                                        .boxed_clone(),
                                        cx,
                                    )
                                }
                            }),
                    )
                    .into_any(),
            );
        }

        let selected_entry = self.filtered_entries.get(self.selected_index);
        let selected_folder_assign = self.selected_folder_assign();
        let selected_current_folder_id = selected_folder_assign.as_ref().and_then(|pending| {
            self.folder_id_for_identity(&pending.remote_identity, &pending.identity_paths)
        });

        let is_current_workspace_entry =
            if let Some(ProjectPickerEntry::ProjectGroup(hit)) = selected_entry {
                self.window_project_groups
                    .get(hit.candidate_id)
                    .is_some_and(|key| self.is_active_project_group(key, cx))
            } else {
                false
            };

        let secondary_footer_actions: Option<AnyElement> = match selected_entry {
            Some(ProjectPickerEntry::OpenFolder { .. }) => Some(
                Button::new("remove_selected", "Remove Folder")
                    .key_binding(KeyBinding::for_action_in(
                        &RemoveSelected,
                        &focus_handle,
                        cx,
                    ))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(RemoveSelected.boxed_clone(), cx)
                    })
                    .into_any_element(),
            ),
            Some(ProjectPickerEntry::ProjectGroup(_)) if !is_current_workspace_entry => Some(
                Button::new("remove_selected", "Remove from Window")
                    .key_binding(KeyBinding::for_action_in(
                        &RemoveSelected,
                        &focus_handle,
                        cx,
                    ))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(RemoveSelected.boxed_clone(), cx)
                    })
                    .into_any_element(),
            ),
            Some(ProjectPickerEntry::RecentProject(_)) => Some(
                Button::new("delete_recent", "Remove")
                    .key_binding(KeyBinding::for_action_in(
                        &RemoveSelected,
                        &focus_handle,
                        cx,
                    ))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(RemoveSelected.boxed_clone(), cx)
                    })
                    .into_any_element(),
            ),
            Some(ProjectPickerEntry::Folder { folder_id, .. }) if folder_id.0 >= 0 => Some(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("rename_folder", "Rename")
                            .key_binding(KeyBinding::for_action_in(
                                &RenameFolder,
                                &focus_handle,
                                cx,
                            ))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(RenameFolder.boxed_clone(), cx)
                            }),
                    )
                    .child(
                        Button::new("remove_folder", "Remove Folder")
                            .key_binding(KeyBinding::for_action_in(
                                &RemoveSelected,
                                &focus_handle,
                                cx,
                            ))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(RemoveSelected.boxed_clone(), cx)
                            }),
                    )
                    .into_any_element(),
            ),
            _ => None,
        };

        Some(
            h_flex()
                .flex_1()
                .p_1p5()
                .gap_1()
                .justify_end()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .when_some(
                    (self.style == ProjectPickerStyle::Modal)
                        .then(|| selected_folder_assign.clone())
                        .flatten()
                        .map(|pending| {
                            let current_folder_id = selected_current_folder_id;
                            let user_folders = self.folders.clone();
                            let picker_entity = cx.entity();
                            PopoverMenu::new("footer-move-to-folder")
                                .with_handle(self.footer_move_to_folder_menu_handle.clone())
                                .trigger(Button::new(
                                    "footer-move-to-folder-trigger",
                                    "Move to Folder",
                                ))
                                .menu(move |window, cx| {
                                    let picker_entity = picker_entity.clone();
                                    let user_folders = user_folders.clone();
                                    let pending = pending.clone();
                                    Some(ContextMenu::build(window, cx, move |menu, _, _| {
                                        populate_move_to_folder_menu(
                                            menu,
                                            picker_entity.clone(),
                                            &user_folders,
                                            current_folder_id,
                                            pending.clone(),
                                        )
                                    }))
                                })
                                .into_any_element()
                        }),
                    |this, actions| this.child(actions),
                )
                .when_some(secondary_footer_actions, |this, actions| {
                    this.child(actions)
                })
                .map(|this| {
                    if is_folder_management_entry {
                        this
                    } else if is_already_open_entry {
                        this.when(show_move_to_new_window, |this| {
                            this.child({
                                let window_project_groups = self.window_project_groups.clone();
                                let selected_index = self.selected_index;
                                let filtered_entries = self.filtered_entries.clone();
                                Button::new("move_to_new_window", "New Window")
                                    .key_binding(KeyBinding::for_action_in(
                                        &menu::SecondaryConfirm,
                                        &focus_handle,
                                        cx,
                                    ))
                                    .on_click(move |_, window, cx| {
                                        let key = match filtered_entries.get(selected_index) {
                                            Some(ProjectPickerEntry::ProjectGroup(hit)) => {
                                                window_project_groups.get(hit.candidate_id).cloned()
                                            }
                                            _ => None,
                                        };
                                        if let Some(key) = key {
                                            move_project_group_to_new_window(&key, window, cx);
                                        }
                                    })
                            })
                        })
                        .child(
                            Button::new("activate", "Activate")
                                .key_binding(KeyBinding::for_action_in(
                                    &menu::Confirm,
                                    &focus_handle,
                                    cx,
                                ))
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(menu::Confirm.boxed_clone(), cx)
                                }),
                        )
                    } else if self.create_new_window {
                        this.child(
                            Button::new("open_here", "This Window")
                                .key_binding(KeyBinding::for_action_in(
                                    &menu::SecondaryConfirm,
                                    &focus_handle,
                                    cx,
                                ))
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(menu::SecondaryConfirm.boxed_clone(), cx)
                                }),
                        )
                        .child(
                            Button::new("open_new_window", "Open")
                                .key_binding(KeyBinding::for_action_in(
                                    &menu::Confirm,
                                    &focus_handle,
                                    cx,
                                ))
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(menu::Confirm.boxed_clone(), cx)
                                }),
                        )
                    } else {
                        this.child(
                            Button::new("open_new_window", "New Window")
                                .key_binding(KeyBinding::for_action_in(
                                    &menu::SecondaryConfirm,
                                    &focus_handle,
                                    cx,
                                ))
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(menu::SecondaryConfirm.boxed_clone(), cx)
                                }),
                        )
                        .child(
                            Button::new("open_here", "Open")
                                .key_binding(KeyBinding::for_action_in(
                                    &menu::Confirm,
                                    &focus_handle,
                                    cx,
                                ))
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(menu::Confirm.boxed_clone(), cx)
                                }),
                        )
                    }
                })
                .child(Divider::vertical())
                .child(
                    PopoverMenu::new("actions-menu-popover")
                        .with_handle(self.actions_menu_handle.clone())
                        .anchor(gpui::Anchor::BottomRight)
                        .offset(gpui::Point {
                            x: px(0.0),
                            y: px(-2.0),
                        })
                        .trigger(
                            Button::new("actions-trigger", "Actions")
                                .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                                .key_binding(KeyBinding::for_action_in(
                                    &ToggleActionsMenu,
                                    &focus_handle,
                                    cx,
                                )),
                        )
                        .menu({
                            let focus_handle = focus_handle.clone();
                            let workspace_handle = self.workspace.clone();
                            let create_new_window = self.create_new_window;
                            let open_action = workspace::Open {
                                create_new_window: Some(create_new_window),
                            };
                            let show_add_to_workspace = match selected_entry {
                                Some(ProjectPickerEntry::RecentProject(hit)) => self
                                    .workspaces
                                    .get(hit.candidate_id)
                                    .map(|workspace| {
                                        matches!(
                                            workspace.location,
                                            SerializedWorkspaceLocation::Local
                                        )
                                    })
                                    .unwrap_or(false),
                                _ => false,
                            };
                            let selected_folder_assign = selected_folder_assign.clone();
                            let current_folder_id = selected_current_folder_id;
                            let user_folders = self.folders.clone();
                            let show_rename_folder = matches!(
                                selected_entry,
                                Some(ProjectPickerEntry::Folder { folder_id, .. })
                                    if folder_id.0 >= 0
                            );
                            let picker_entity = cx.entity();
                            let show_new_folder = self.style == ProjectPickerStyle::Modal;

                            move |window, cx| {
                                Some(ContextMenu::build(window, cx, {
                                    let focus_handle = focus_handle.clone();
                                    let workspace_handle = workspace_handle.clone();
                                    let open_action = open_action.clone();
                                    let picker_entity = picker_entity.clone();
                                    let user_folders = user_folders.clone();
                                    let selected_folder_assign = selected_folder_assign.clone();
                                    move |mut menu, _, _| {
                                        menu = menu.context(focus_handle);
                                        if show_new_folder {
                                            menu = menu.action("New Folder…", NewFolder.boxed_clone());
                                        }
                                        if let Some(pending) = selected_folder_assign.clone() {
                                            if show_new_folder {
                                                menu = menu.submenu("Move to Folder", {
                                                    let picker_entity = picker_entity.clone();
                                                    let user_folders = user_folders.clone();
                                                    move |menu, _, _| {
                                                        populate_move_to_folder_menu(
                                                            menu,
                                                            picker_entity.clone(),
                                                            &user_folders,
                                                            current_folder_id,
                                                            pending.clone(),
                                                        )
                                                    }
                                                });
                                            }
                                        }
                                        if show_rename_folder {
                                            menu = menu.action(
                                                "Rename Folder",
                                                RenameFolder.boxed_clone(),
                                            );
                                        }
                                        menu.when(show_new_folder, |menu| menu.separator())
                                            .when(show_add_to_workspace, |menu| {
                                                menu.action(
                                                    "Add Folder to this Project",
                                                    AddToWorkspace.boxed_clone(),
                                                )
                                                .separator()
                                            })
                                            .entry(
                                                "Open Local Folders",
                                                Some(open_action.boxed_clone()),
                                                {
                                                    let workspace_handle = workspace_handle.clone();
                                                    move |window, cx| {
                                                        open_local_project(
                                                            workspace_handle.clone(),
                                                            create_new_window,
                                                            window,
                                                            cx,
                                                        );
                                                    }
                                                },
                                            )
                                            .action(
                                                "Open Remote Folder",
                                                OpenRemote {
                                                    from_existing_connection: false,
                                                    create_new_window: Some(create_new_window),
                                                }
                                                .boxed_clone(),
                                            )
                                    }
                                }))
                            }
                        }),
                )
                .into_any(),
        )
    }
}

fn icon_for_project_group(key: &ProjectGroupKey) -> IconName {
    let host = key.host();
    icon_for_remote_connection(host.as_ref())
}

pub(crate) fn icon_for_remote_connection(options: Option<&RemoteConnectionOptions>) -> IconName {
    match options {
        None => IconName::Screen,
        Some(options) => match options {
            RemoteConnectionOptions::Ssh(_) => IconName::Server,
            RemoteConnectionOptions::Wsl(_) => IconName::Linux,
            RemoteConnectionOptions::Docker(_) => IconName::Box,
            #[cfg(any(test, feature = "test-support"))]
            RemoteConnectionOptions::Mock(_) => IconName::Server,
        },
    }
}

// Compute the highlighted text for the name and path
pub(crate) fn highlights_for_path(
    path: &Path,
    match_positions: &Vec<usize>,
    path_start_offset: usize,
) -> (Option<HighlightedMatch>, HighlightedMatch) {
    let path_string = path.to_string_lossy();
    let path_text = path_string.to_string();
    let path_byte_len = path_text.len();
    // Get the subset of match highlight positions that line up with the given path.
    // Also adjusts them to start at the path start
    let path_positions = match_positions
        .iter()
        .copied()
        .skip_while(|position| *position < path_start_offset)
        .take_while(|position| *position < path_start_offset + path_byte_len)
        .map(|position| position - path_start_offset)
        .collect::<Vec<_>>();

    // Again subset the highlight positions to just those that line up with the file_name
    // again adjusted to the start of the file_name
    let file_name_text_and_positions = path.file_name().map(|file_name| {
        let file_name_text = file_name.to_string_lossy().into_owned();
        let file_name_start_byte = path_byte_len - file_name_text.len();
        let highlight_positions = path_positions
            .iter()
            .copied()
            .skip_while(|position| *position < file_name_start_byte)
            .take_while(|position| *position < file_name_start_byte + file_name_text.len())
            .map(|position| position - file_name_start_byte)
            .collect::<Vec<_>>();
        HighlightedMatch {
            text: file_name_text,
            highlight_positions,
            color: Color::Default,
        }
    });

    (
        file_name_text_and_positions,
        HighlightedMatch {
            text: path_text,
            highlight_positions: path_positions,
            color: Color::Default,
        },
    )
}

fn move_project_group_to_new_window(key: &ProjectGroupKey, window: &mut Window, cx: &mut App) {
    if let Some(handle) = window.window_handle().downcast::<MultiWorkspace>() {
        let key = key.clone();
        cx.defer(move |cx| {
            handle
                .update(cx, |multi_workspace, window, cx| {
                    multi_workspace
                        .open_project_group_in_new_window(&key, window, cx)
                        .detach_and_log_err(cx);
                })
                .log_err();
        });
    }
}

fn open_local_project(
    workspace: WeakEntity<Workspace>,
    create_new_window: bool,
    window: &mut Window,
    cx: &mut App,
) {
    use gpui::PathPromptOptions;
    use project::DirectoryLister;

    let Some(workspace) = workspace.upgrade() else {
        return;
    };

    let paths = workspace.update(cx, |workspace, cx| {
        workspace.prompt_for_open_path(
            PathPromptOptions {
                files: true,
                directories: true,
                multiple: true,
                prompt: None,
            },
            DirectoryLister::Local(
                workspace.project().clone(),
                workspace.app_state().fs.clone(),
            ),
            window,
            cx,
        )
    });

    let multi_workspace_handle = window.window_handle().downcast::<MultiWorkspace>();
    window
        .spawn(cx, async move |cx| {
            let Some(paths) = paths.await.log_err().flatten() else {
                return;
            };
            if !create_new_window {
                if let Some(handle) = multi_workspace_handle {
                    if let Some(task) = handle
                        .update(cx, |multi_workspace, window, cx| {
                            multi_workspace.open_project(paths, OpenMode::Activate, window, cx)
                        })
                        .log_err()
                    {
                        task.await.log_err();
                    }
                    return;
                }
            }
            if let Some(task) = workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.open_workspace_for_paths(OpenMode::NewWindow, paths, window, cx)
                })
                .log_err()
            {
                task.await.log_err();
            }
        })
        .detach();
}

impl RecentProjectsDelegate {
    fn open_recent_projects(
        &mut self,
        candidate_id: usize,
        secondary: bool,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let Some(candidate_workspace) = self.workspaces.get(candidate_id) else {
            return;
        };

        let replace_current_window = self.create_new_window == secondary;
        let candidate_workspace_id = candidate_workspace.workspace_id;
        let candidate_workspace_location = candidate_workspace.location.clone();
        let candidate_workspace_paths = candidate_workspace.paths.clone();

        workspace.update(cx, |workspace, cx| {
            if workspace.database_id() == Some(candidate_workspace_id) {
                return;
            }
            match candidate_workspace_location {
                SerializedWorkspaceLocation::Local => {
                    let paths = candidate_workspace_paths.paths().to_vec();
                    if replace_current_window {
                        if let Some(handle) = window.window_handle().downcast::<MultiWorkspace>() {
                            cx.defer(move |cx| {
                                if let Some(task) = handle
                                    .update(cx, |multi_workspace, window, cx| {
                                        multi_workspace.open_project(
                                            paths,
                                            OpenMode::Activate,
                                            window,
                                            cx,
                                        )
                                    })
                                    .log_err()
                                {
                                    task.detach_and_log_err(cx);
                                }
                            });
                        }
                        return;
                    } else {
                        workspace
                            .open_workspace_for_paths(OpenMode::NewWindow, paths, window, cx)
                            .detach_and_prompt_err(
                                "Failed to open project",
                                window,
                                cx,
                                |_, _, _| None,
                            );
                    }
                }
                SerializedWorkspaceLocation::Remote(mut connection) => {
                    let app_state = workspace.app_state().clone();
                    let replace_window = if replace_current_window {
                        window.window_handle().downcast::<MultiWorkspace>()
                    } else {
                        None
                    };
                    let open_options = OpenOptions {
                        requesting_window: replace_window,
                        ..Default::default()
                    };
                    if let RemoteConnectionOptions::Ssh(connection) = &mut connection {
                        RemoteSettings::get_global(cx)
                            .fill_connection_options_from_settings(connection);
                    };
                    let paths = candidate_workspace_paths.paths().to_vec();
                    cx.spawn_in(window, async move |_, cx| {
                        open_remote_project(connection.clone(), paths, app_state, open_options, cx)
                            .await
                    })
                    .detach_and_prompt_err(
                        "Failed to open project",
                        window,
                        cx,
                        |_, _, _| None,
                    );
                }
            }
        });
        cx.emit(DismissEvent);
    }

    fn add_paths_to_project(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let open_paths_task = workspace.update(cx, |workspace, cx| {
            workspace.open_paths(
                paths,
                OpenOptions {
                    visible: Some(OpenVisible::All),
                    ..Default::default()
                },
                None,
                window,
                cx,
            )
        });
        cx.spawn_in(window, async move |picker, cx| {
            let _result = open_paths_task.await;
            picker
                .update_in(cx, |picker, window, cx| {
                    let Some(workspace) = picker.delegate.workspace.upgrade() else {
                        return;
                    };
                    picker.delegate.open_folders = get_open_folders(workspace.read(cx), cx);
                    let query = picker.query(cx);
                    picker.update_matches(query, window, cx);
                })
                .ok();
        })
        .detach();
    }

    /// Returns the new selection index after the entry at `deleted_index`
    /// is removed.
    ///
    /// - Prefers the nearest entry matching `prefer_section` so the user
    ///   stays in the same section they were navigating.
    /// - Falls back to any other selectable entry so the picker doesn't
    ///   land on a header.
    fn replacement_index_after_deletion(
        &self,
        deleted_index: usize,
        prefer_previous: bool,
        prefer_section: fn(&ProjectPickerEntry) -> bool,
    ) -> Option<usize> {
        let replacement_index = |matches_entry: fn(&ProjectPickerEntry) -> bool| {
            let next_index = self
                .filtered_entries
                .iter()
                .enumerate()
                .skip(deleted_index)
                .find_map(|(index, entry)| matches_entry(entry).then_some(index));
            let previous_index = self
                .filtered_entries
                .iter()
                .enumerate()
                .take(deleted_index.min(self.filtered_entries.len()))
                .rev()
                .find_map(|(index, entry)| matches_entry(entry).then_some(index));

            if prefer_previous {
                previous_index.or(next_index)
            } else {
                next_index.or(previous_index)
            }
        };

        replacement_index(prefer_section).or_else(|| replacement_index(is_selectable_entry))
    }

    fn update_picker_after_recent_project_deletion(
        picker: &mut Picker<Self>,
        deleted_index: usize,
        workspaces: Vec<RecentWorkspace>,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        let prefer_previous = picker.is_scrolled_to_end() == Some(true);
        picker.delegate.set_workspaces(workspaces);
        picker.delegate.snap_selection_to_first_non_header_match = false;
        picker.update_matches_with_options(
            picker.query(cx),
            ScrollBehavior::PreserveOffset,
            window,
            cx,
        );
        if let Some(replacement_index) = picker.delegate.replacement_index_after_deletion(
            deleted_index,
            prefer_previous,
            |entry| matches!(entry, ProjectPickerEntry::RecentProject(_)),
        ) {
            picker.set_selected_index(replacement_index, None, false, window, cx);
        }
    }

    fn delete_recent_project(
        &self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        if let Some(ProjectPickerEntry::RecentProject(selected_match)) =
            self.filtered_entries.get(ix)
        {
            let Some(recent_workspace) = self.workspaces.get(selected_match.candidate_id).cloned()
            else {
                return;
            };
            let fs = self
                .workspace
                .upgrade()
                .map(|ws| ws.read(cx).app_state().fs.clone());
            let db = WorkspaceDb::global(cx);
            cx.spawn_in(window, async move |this, cx| {
                let Some(fs) = fs else { return };
                let deleted_workspace_ids = db
                    .delete_recent_workspace_group(&recent_workspace)
                    .await
                    .log_err()
                    .unwrap_or_default();
                let workspaces = db
                    .recent_project_workspaces(fs.as_ref())
                    .await
                    .unwrap_or_default();
                this.update_in(cx, move |picker, window, cx| {
                    Self::update_picker_after_recent_project_deletion(
                        picker, ix, workspaces, window, cx,
                    );
                    // After deleting a project, we want to update the history manager to reflect the change.
                    // But we do not emit a update event when user opens a project, because it's handled in `workspace::load_workspace`.
                    if let Some(history_manager) = HistoryManager::global(cx) {
                        history_manager.update(cx, |this, cx| {
                            for workspace_id in &deleted_workspace_ids {
                                this.delete_history(*workspace_id, cx);
                            }
                        });
                    }
                })
                .ok();
            })
            .detach();
        }
    }

    fn remove_open_folder(
        picker: &mut Picker<Self>,
        worktree_id: WorktreeId,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        let Some(workspace) = picker.delegate.workspace.upgrade() else {
            return;
        };

        let old_key = workspace.read(cx).project_group_key(cx);
        workspace.update(cx, |workspace, cx| {
            let project = workspace.project().clone();
            project.update(cx, |project, cx| {
                project.remove_worktree(worktree_id, cx);
            });
        });

        let new_key = workspace.read(cx).project_group_key(cx);
        if let Some(entry) = picker
            .delegate
            .window_project_groups
            .iter_mut()
            .find(|key| **key == old_key)
        {
            *entry = new_key;
        }

        picker.delegate.open_folders = get_open_folders(workspace.read(cx), cx);
        let query = picker.query(cx);
        picker.update_matches(query, window, cx);
    }

    fn remove_project_group(
        &mut self,
        key: ProjectGroupKey,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        if let Some(handle) = window.window_handle().downcast::<MultiWorkspace>() {
            let key_for_remove = key.clone();
            cx.defer(move |cx| {
                handle
                    .update(cx, |multi_workspace, window, cx| {
                        multi_workspace
                            .remove_project_group(&key_for_remove, window, cx)
                            .detach_and_log_err(cx);
                    })
                    .log_err();
            });
        }

        self.window_project_groups.retain(|k| k != &key);
    }

    fn is_current_workspace(
        &self,
        workspace_id: WorkspaceId,
        cx: &mut Context<Picker<Self>>,
    ) -> bool {
        if let Some(workspace) = self.workspace.upgrade() {
            let workspace = workspace.read(cx);
            if Some(workspace_id) == workspace.database_id() {
                return true;
            }
        }

        false
    }

    fn is_active_project_group(&self, key: &ProjectGroupKey, cx: &App) -> bool {
        if let Some(workspace) = self.workspace.upgrade() {
            return workspace.read(cx).project_group_key(cx) == *key;
        }
        false
    }

    fn is_in_current_window_groups(&self, workspace: &RecentWorkspace) -> bool {
        self.window_project_groups
            .iter()
            .any(|key| key.matches(&workspace.project_group_key()))
    }

    fn is_open_folder(&self, workspace: &RecentWorkspace) -> bool {
        if self.open_folders.is_empty() {
            return false;
        }

        let workspace_host = match &workspace.location {
            SerializedWorkspaceLocation::Local => None,
            SerializedWorkspaceLocation::Remote(options) => Some(options),
        };

        for workspace_path in workspace.paths.paths() {
            for open_folder in &self.open_folders {
                if workspace_path == &open_folder.path
                    && same_remote_connection_identity(
                        workspace_host,
                        open_folder.connection_options.as_ref(),
                    )
                {
                    return true;
                }
            }
        }

        false
    }

    fn is_valid_recent_candidate(
        &self,
        workspace: &RecentWorkspace,
        cx: &mut Context<Picker<Self>>,
    ) -> bool {
        !self.is_current_workspace(workspace.workspace_id, cx)
            && !self.is_in_current_window_groups(workspace)
            && !self.is_open_folder(workspace)
    }
}

#[cfg(test)]
mod tests {
    use gpui::{TestAppContext, UpdateGlobal, VisualTestContext};

    use serde_json::json;
    use settings::SettingsStore;
    use util::path;
    use workspace::{AppState, PathList, open_paths};

    use super::*;

    // Test picker for the empty query:
    //
    //   [0] Header("Current Folders")
    //   [1] OpenFolder(0)
    //   [2] OpenFolder(1)
    //   [3] Header("This Window")
    //   [4] ProjectGroup(0)
    //   [5] ProjectGroup(1)
    //   [6] Header("Ungrouped")
    //   [7..=26] RecentProject(0..=19)
    //   [27] NewFolder
    //
    const RECENT_PROJECT_COUNT: usize = 20;
    const FIRST_RECENT_PROJECT: usize = 7;
    const LAST_RECENT_PROJECT: usize = FIRST_RECENT_PROJECT + RECENT_PROJECT_COUNT - 1;

    fn open_folder(index: usize) -> OpenFolderEntry {
        OpenFolderEntry {
            worktree_id: WorktreeId::from_usize(index),
            name: format!("project-folder-{index}").into(),
            path: PathBuf::from(format!("/current/project-folder-{index}")),
            branch: None,
            is_active: false,
            connection_options: None,
        }
    }

    fn project_group(index: usize) -> ProjectGroupKey {
        ProjectGroupKey::new(
            None,
            PathList::new(&[PathBuf::from(format!("/this-window/project-{index}"))]),
        )
    }

    fn remote_project_group(index: usize) -> ProjectGroupKey {
        ProjectGroupKey::new(
            Some(RemoteConnectionOptions::Mock(
                remote::MockConnectionOptions { id: index as u64 },
            )),
            PathList::new(&[PathBuf::from(format!(
                "/this-window/remote-project-{index}"
            ))]),
        )
    }

    fn recent_workspace(index: usize) -> RecentWorkspace {
        let paths = PathList::new(&[PathBuf::from(format!("/recent/project-{index:02}"))]);
        RecentWorkspace {
            workspace_id: WorkspaceId::from_i64(index as i64),
            location: SerializedWorkspaceLocation::Local,
            paths: paths.clone(),
            identity_paths: paths,
            timestamp: Utc::now(),
        }
    }

    fn recent_workspaces() -> Vec<RecentWorkspace> {
        (0..RECENT_PROJECT_COUNT).map(recent_workspace).collect()
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }

    fn build_picker(
        cx: &mut TestAppContext,
    ) -> (
        Entity<Picker<RecentProjectsDelegate>>,
        &mut VisualTestContext,
    ) {
        init_test(cx);
        let (picker, cx) = cx.add_window_view(|window, cx| {
            let mut delegate = RecentProjectsDelegate::new(
                WeakEntity::new_invalid(),
                false,
                cx.focus_handle(),
                vec![open_folder(0), open_folder(1)],
                vec![project_group(0), project_group(1)],
                ProjectPickerStyle::Modal,
            );
            delegate.set_workspaces(recent_workspaces());
            Picker::list(delegate, window, cx)
                .list_measure_all()
                .show_scrollbar(true)
                .max_height(Rems::from_pixels(px(240.0), window))
        });
        draw(cx);
        (picker, cx)
    }

    fn scroll_to_and_select(
        picker: &Entity<Picker<RecentProjectsDelegate>>,
        cx: &mut VisualTestContext,
        index: usize,
    ) -> usize {
        picker.update_in(cx, |picker, window, cx| {
            picker.set_selected_index(index, None, true, window, cx);
        });
        draw(cx);
        picker.update(cx, |picker, _| picker.logical_scroll_top_index())
    }

    fn delete_recent_project_in_picker(
        picker: &Entity<Picker<RecentProjectsDelegate>>,
        cx: &mut VisualTestContext,
        index: usize,
    ) {
        picker.update_in(cx, |picker, window, cx| {
            let Some(ProjectPickerEntry::RecentProject(hit)) =
                picker.delegate.filtered_entries.get(index)
            else {
                panic!("expected entry at {index} to be a recent project");
            };
            let mut workspaces = picker.delegate.workspaces.clone();
            workspaces.remove(hit.candidate_id);
            RecentProjectsDelegate::update_picker_after_recent_project_deletion(
                picker, index, workspaces, window, cx,
            );
        });
    }

    #[track_caller]
    fn assert_scroll_top_is(
        picker: &Entity<Picker<RecentProjectsDelegate>>,
        cx: &mut VisualTestContext,
        expected: usize,
        phase: &str,
    ) {
        picker.update(cx, |picker, _| {
            assert_eq!(
                picker.logical_scroll_top_index(),
                expected,
                "scroll top should remain at {expected} ({phase})"
            );
            assert_selected_entry_is_recent_project(picker);
        });
    }

    #[track_caller]
    fn assert_selected_entry_is_recent_project(picker: &Picker<RecentProjectsDelegate>) {
        assert!(matches!(
            picker
                .delegate
                .filtered_entries
                .get(picker.delegate.selected_index),
            Some(ProjectPickerEntry::RecentProject(_))
        ));
    }

    #[gpui::test]
    fn this_window_project_icons_use_each_project_group_host(cx: &mut TestAppContext) {
        init_test(cx);

        let mut delegate = RecentProjectsDelegate::new(
            WeakEntity::new_invalid(),
            false,
            cx.update(|cx| cx.focus_handle()),
            Vec::new(),
            vec![project_group(0), remote_project_group(1)],
            ProjectPickerStyle::Modal,
        );
        delegate.filtered_entries = vec![
            ProjectPickerEntry::ProjectGroup(StringMatch {
                candidate_id: 0,
                score: 0.0,
                positions: Vec::new(),
                string: Default::default(),
            }),
            ProjectPickerEntry::ProjectGroup(StringMatch {
                candidate_id: 1,
                score: 0.0,
                positions: Vec::new(),
                string: Default::default(),
            }),
        ];

        assert!(!delegate.entry_is_remote_project(&delegate.filtered_entries[0]));
        assert!(delegate.entry_is_remote_project(&delegate.filtered_entries[1]));
        assert!(delegate.filtered_entries_include_remote_project());
        assert_eq!(
            icon_for_project_group(&delegate.window_project_groups[0]),
            IconName::Screen
        );
        assert_eq!(
            icon_for_project_group(&delegate.window_project_groups[1]),
            IconName::Server
        );
    }

    #[gpui::test]
    fn is_open_folder_distinguishes_local_and_remote(cx: &mut TestAppContext) {
        init_test(cx);

        let shared_path = PathBuf::from("/repo");
        let local_open_folder = OpenFolderEntry {
            worktree_id: WorktreeId::from_usize(0),
            name: "repo".into(),
            path: shared_path.clone(),
            branch: None,
            is_active: false,
            connection_options: None,
        };

        let delegate = RecentProjectsDelegate::new(
            WeakEntity::new_invalid(),
            false,
            cx.update(|cx| cx.focus_handle()),
            vec![local_open_folder],
            Vec::new(),
            ProjectPickerStyle::Modal,
        );

        let paths = PathList::new(&[shared_path]);
        let local_workspace = RecentWorkspace {
            workspace_id: WorkspaceId::from_i64(1),
            location: SerializedWorkspaceLocation::Local,
            paths: paths.clone(),
            identity_paths: paths.clone(),
            timestamp: Utc::now(),
        };
        let remote_workspace = RecentWorkspace {
            workspace_id: WorkspaceId::from_i64(2),
            location: SerializedWorkspaceLocation::Remote(RemoteConnectionOptions::Mock(
                remote::MockConnectionOptions { id: 0 },
            )),
            paths: paths.clone(),
            identity_paths: paths,
            timestamp: Utc::now(),
        };

        // A local open folder should hide only the matching local recent
        // project, not a remote checkout that shares the same path.
        assert!(delegate.is_open_folder(&local_workspace));
        assert!(!delegate.is_open_folder(&remote_workspace));
    }

    #[gpui::test]
    fn deleting_top_recent_project_preserves_scroll_position(cx: &mut TestAppContext) {
        let target = FIRST_RECENT_PROJECT;
        let (picker, cx) = build_picker(cx);
        let scroll_top = scroll_to_and_select(&picker, cx, target);
        assert!(
            scroll_top > 0,
            "test should start scrolled away from the top"
        );

        delete_recent_project_in_picker(&picker, cx, target);
        assert_scroll_top_is(&picker, cx, scroll_top, "after delete");

        // The picker re-runs layout on the next frame; the scroll position
        // must still be preserved after that redraw.
        draw(cx);
        assert_scroll_top_is(&picker, cx, scroll_top, "after redraw");
    }

    #[gpui::test]
    fn deleting_middle_recent_project_preserves_scroll_position(cx: &mut TestAppContext) {
        let target = FIRST_RECENT_PROJECT + RECENT_PROJECT_COUNT / 2;
        let (picker, cx) = build_picker(cx);
        let scroll_top = scroll_to_and_select(&picker, cx, target);
        assert!(
            scroll_top > 0,
            "test should start scrolled away from the top"
        );

        delete_recent_project_in_picker(&picker, cx, target);
        assert_scroll_top_is(&picker, cx, scroll_top, "after delete");

        draw(cx);
        assert_scroll_top_is(&picker, cx, scroll_top, "after redraw");
    }

    #[gpui::test]
    fn deleting_last_recent_project_preserves_scroll_position(cx: &mut TestAppContext) {
        let target = LAST_RECENT_PROJECT;
        let (picker, cx) = build_picker(cx);
        scroll_to_and_select(&picker, cx, target + 1);

        picker.update(cx, |picker, _| {
            assert_eq!(
                picker.is_scrolled_to_end(),
                Some(true),
                "selecting the last entry should leave the picker pinned to the bottom"
            );
        });

        delete_recent_project_in_picker(&picker, cx, target);
        picker.update(cx, |picker, _| {
            assert_eq!(
                picker.is_scrolled_to_end(),
                Some(true),
                "picker should remain pinned to the bottom (after delete)"
            );
        });

        draw(cx);
        picker.update(cx, |picker, _| {
            assert_eq!(
                picker.is_scrolled_to_end(),
                Some(true),
                "picker should remain pinned to the bottom (after redraw)"
            );
        });
    }

    fn assign_first_recent_to_folder(
        picker: &Entity<Picker<RecentProjectsDelegate>>,
        cx: &mut VisualTestContext,
        folder_name: &str,
    ) -> ProjectFolderId {
        picker.update_in(cx, |picker, window, cx| {
            let workspace = picker.delegate.workspaces[0].clone();
            let folder_id = ProjectFolderId(1);
            picker.delegate.set_folders(
                vec![ProjectFolder {
                    folder_id,
                    name: folder_name.to_string(),
                    position: 0,
                }],
                vec![ProjectFolderAssignment {
                    folder_id,
                    remote_identity: workspace.project_folder_identity(),
                    identity_paths: workspace.identity_paths.clone(),
                    position: 0,
                }],
            );
            picker.delegate.snap_selection_to_first_non_header_match = false;
            let query = picker.query(cx);
            picker.update_matches(query, window, cx);
            folder_id
        })
    }

    #[gpui::test]
    fn recent_projects_are_grouped_under_user_folders(cx: &mut TestAppContext) {
        let (picker, cx) = build_picker(cx);
        assign_first_recent_to_folder(&picker, cx, "Work");

        picker.update(cx, |picker, _| {
            let entries = &picker.delegate.filtered_entries;
            let work_ix = entries
                .iter()
                .position(|entry| {
                    matches!(entry, ProjectPickerEntry::Folder { name, .. } if name == "Work")
                })
                .expect("Work folder should be listed");
            assert!(
                matches!(
                    entries.get(work_ix + 1),
                    Some(ProjectPickerEntry::RecentProject(hit)) if hit.candidate_id == 0
                ),
                "assigned project should appear under its folder"
            );
            let ungrouped_ix = entries
                .iter()
                .position(|entry| matches!(entry, ProjectPickerEntry::Header(title) if title == "Ungrouped"))
                .expect("Ungrouped section should remain");
            assert!(work_ix < ungrouped_ix, "folders should appear before Ungrouped");
            assert!(
                !entries[ungrouped_ix..]
                    .iter()
                    .any(|entry| matches!(entry, ProjectPickerEntry::RecentProject(hit) if hit.candidate_id == 0)),
                "assigned project should not remain in Ungrouped"
            );
            assert!(
                entries
                    .iter()
                    .any(|entry| matches!(entry, ProjectPickerEntry::NewFolder)),
                "modal picker should keep a New Folder row"
            );
        });
    }

    #[gpui::test]
    fn searching_grouped_projects_keeps_folder_headers(cx: &mut TestAppContext) {
        let (picker, cx) = build_picker(cx);
        assign_first_recent_to_folder(&picker, cx, "Work");

        picker.update_in(cx, |picker, window, cx| {
            picker.update_matches("project-00".into(), window, cx);
        });

        picker.update(cx, |picker, _| {
            let entries = &picker.delegate.filtered_entries;
            assert!(
                entries.iter().any(|entry| {
                    matches!(entry, ProjectPickerEntry::Folder { name, .. } if name == "Work")
                }),
                "matching a project should still show its folder header"
            );
            assert!(
                entries
                    .iter()
                    .any(|entry| matches!(entry, ProjectPickerEntry::RecentProject(hit) if hit.candidate_id == 0)),
                "matching project should remain visible"
            );
            assert!(
                !entries
                    .iter()
                    .any(|entry| matches!(entry, ProjectPickerEntry::NewFolder)),
                "New Folder should be hidden while searching"
            );
        });
    }

    #[gpui::test]
    fn deleting_user_folder_returns_projects_to_ungrouped(cx: &mut TestAppContext) {
        let (picker, cx) = build_picker(cx);
        let folder_id = assign_first_recent_to_folder(&picker, cx, "Work");

        picker.update_in(cx, |picker, window, cx| {
            picker.delegate.folders.clear();
            picker.delegate.assignments.clear();
            picker.delegate.snap_selection_to_first_non_header_match = false;
            let query = picker.query(cx);
            picker.update_matches(query, window, cx);
            let _ = folder_id;
        });

        picker.update(cx, |picker, _| {
            let entries = &picker.delegate.filtered_entries;
            assert!(
                !entries
                    .iter()
                    .any(|entry| matches!(entry, ProjectPickerEntry::Folder { name, .. } if name == "Work"))
            );
            let ungrouped_ix = entries
                .iter()
                .position(|entry| matches!(entry, ProjectPickerEntry::Header(title) if title == "Ungrouped"))
                .expect("Ungrouped section should exist");
            assert!(
                entries[ungrouped_ix..]
                    .iter()
                    .any(|entry| matches!(entry, ProjectPickerEntry::RecentProject(hit) if hit.candidate_id == 0)),
                "unassigned project should return to Ungrouped"
            );
        });
    }

    #[gpui::test]
    fn assigned_open_project_stays_under_folder_not_ungrouped(cx: &mut TestAppContext) {
        let (picker, cx) = build_picker(cx);
        picker.update_in(cx, |picker, window, cx| {
            let mut workspaces = recent_workspaces();
            let window_paths = PathList::new(&[PathBuf::from("/this-window/project-0")]);
            workspaces.push(RecentWorkspace {
                workspace_id: WorkspaceId::from_i64(99),
                location: SerializedWorkspaceLocation::Local,
                paths: window_paths.clone(),
                identity_paths: window_paths.clone(),
                timestamp: Utc::now(),
            });
            picker.delegate.set_workspaces(workspaces);
            let folder_id = ProjectFolderId(1);
            picker.delegate.set_folders(
                vec![ProjectFolder {
                    folder_id,
                    name: "test".into(),
                    position: 0,
                }],
                vec![ProjectFolderAssignment {
                    folder_id,
                    remote_identity: String::new(),
                    identity_paths: window_paths,
                    position: 0,
                }],
            );
            picker.delegate.snap_selection_to_first_non_header_match = false;
            picker.update_matches("".into(), window, cx);
        });

        picker.update(cx, |picker, _| {
            let entries = &picker.delegate.filtered_entries;
            let folder_ix = entries
                .iter()
                .position(|entry| {
                    matches!(entry, ProjectPickerEntry::Folder { name, .. } if name == "test")
                })
                .expect("test folder should be listed");
            assert!(
                matches!(
                    entries.get(folder_ix + 1),
                    Some(ProjectPickerEntry::RecentProject(hit)) if hit.candidate_id == RECENT_PROJECT_COUNT
                ),
                "currently open assigned project should appear under its folder"
            );
            assert!(
                !entries.iter().any(|entry| {
                    matches!(entry, ProjectPickerEntry::ProjectGroup(hit) if hit.candidate_id == 0)
                }),
                "assigned This Window project should not stay in This Window"
            );
            if let Some(ungrouped_ix) = entries.iter().position(|entry| {
                matches!(entry, ProjectPickerEntry::Header(title) if title == "Ungrouped")
            }) {
                assert!(
                    !entries[ungrouped_ix..].iter().any(|entry| {
                        matches!(
                            entry,
                            ProjectPickerEntry::RecentProject(hit)
                                if hit.candidate_id == RECENT_PROJECT_COUNT
                        )
                    }),
                    "assigned project should not remain in Ungrouped"
                );
            }
        });
    }

    #[gpui::test]
    fn assigned_this_window_project_appears_under_folder(cx: &mut TestAppContext) {
        let (picker, cx) = build_picker(cx);
        picker.update_in(cx, |picker, window, cx| {
            let folder_id = ProjectFolderId(1);
            let window_paths = PathList::new(&[PathBuf::from("/this-window/project-0")]);
            picker.delegate.set_folders(
                vec![ProjectFolder {
                    folder_id,
                    name: "test".into(),
                    position: 0,
                }],
                vec![ProjectFolderAssignment {
                    folder_id,
                    remote_identity: String::new(),
                    identity_paths: window_paths,
                    position: 0,
                }],
            );
            picker.delegate.snap_selection_to_first_non_header_match = false;
            picker.update_matches("".into(), window, cx);
        });

        picker.update(cx, |picker, _| {
            let entries = &picker.delegate.filtered_entries;
            let folder_ix = entries
                .iter()
                .position(|entry| {
                    matches!(entry, ProjectPickerEntry::Folder { name, .. } if name == "test")
                })
                .expect("test folder should be listed");
            assert!(
                matches!(
                    entries.get(folder_ix + 1),
                    Some(ProjectPickerEntry::ProjectGroup(hit)) if hit.candidate_id == 0
                ),
                "This Window project should appear under its folder"
            );
            assert_eq!(
                entries
                    .iter()
                    .filter(|entry| {
                        matches!(entry, ProjectPickerEntry::ProjectGroup(hit) if hit.candidate_id == 0)
                    })
                    .count(),
                1,
                "assigned This Window project should not also remain under This Window"
            );
        });
    }

    #[gpui::test]
    fn searching_matches_folder_names_and_project_basenames(cx: &mut TestAppContext) {
        let (picker, cx) = build_picker(cx);
        assign_first_recent_to_folder(&picker, cx, "Work");

        picker.update_in(cx, |picker, window, cx| {
            picker.update_matches("Work".into(), window, cx);
        });
        picker.update(cx, |picker, _| {
            let entries = &picker.delegate.filtered_entries;
            assert!(
                entries.iter().any(|entry| {
                    matches!(entry, ProjectPickerEntry::Folder { name, .. } if name == "Work")
                }),
                "folder name search should show the folder"
            );
            assert!(
                entries
                    .iter()
                    .any(|entry| matches!(entry, ProjectPickerEntry::RecentProject(hit) if hit.candidate_id == 0)),
                "matching a folder name should keep its members visible"
            );
            assert!(
                !entries
                    .iter()
                    .any(|entry| matches!(entry, ProjectPickerEntry::Header(title) if title == "Ungrouped")),
                "unrelated Ungrouped recents should be hidden when searching a folder name"
            );
        });

        picker.update_in(cx, |picker, window, cx| {
            picker.update_matches("project-00".into(), window, cx);
        });
        picker.update(cx, |picker, _| {
            let entries = &picker.delegate.filtered_entries;
            assert!(
                entries.iter().any(|entry| {
                    matches!(entry, ProjectPickerEntry::Folder { name, .. } if name == "Work")
                }),
                "basename search should still show the parent folder"
            );
            assert!(
                entries
                    .iter()
                    .any(|entry| matches!(entry, ProjectPickerEntry::RecentProject(hit) if hit.candidate_id == 0)),
                "basename search should find the assigned project"
            );
        });

        picker.update_in(cx, |picker, window, cx| {
            picker.update_matches("no-such-project".into(), window, cx);
        });
        picker.update(cx, |picker, _| {
            let entries = &picker.delegate.filtered_entries;
            assert!(
                !entries.iter().any(|entry| {
                    matches!(entry, ProjectPickerEntry::Folder { name, .. } if name == "Work")
                }),
                "unrelated search should hide folders with no matching members"
            );
        });
    }

    #[gpui::test]
    async fn test_open_dev_container_action_with_single_config(cx: &mut TestAppContext) {
        let app_state = init_test(cx);

        app_state
            .fs
            .as_fake()
            .insert_tree(
                path!("/project"),
                json!({
                    ".devcontainer": {
                        "devcontainer.json": "{}"
                    },
                    "src": {
                        "main.rs": "fn main() {}"
                    }
                }),
            )
            .await;

        // Open a file path (not a directory) so that the worktree root is a
        // file. This means `active_project_directory` returns `None`, which
        // causes `DevContainerContext::from_workspace` to return `None`,
        // preventing `open_dev_container` from spawning real I/O (docker
        // commands, shell environment loading) that is incompatible with the
        // test scheduler. The modal is still created and the re-entrancy
        // guard that this test validates is still exercised.
        cx.update(|cx| {
            open_paths(
                &[PathBuf::from(path!("/project/src/main.rs"))],
                app_state,
                workspace::OpenOptions::default(),
                cx,
            )
        })
        .await
        .unwrap();

        assert_eq!(cx.update(|cx| cx.windows().len()), 1);
        let multi_workspace = cx.update(|cx| cx.windows()[0].downcast::<MultiWorkspace>().unwrap());

        cx.run_until_parked();

        // This dispatch triggers with_active_or_new_workspace -> MultiWorkspace::update
        // -> Workspace::update -> toggle_modal -> new_dev_container.
        // Before the fix, this panicked with "cannot read workspace::Workspace while
        // it is already being updated" because new_dev_container and open_dev_container
        // tried to read the Workspace entity through a WeakEntity handle while it was
        // already leased by the outer update.
        cx.dispatch_action(*multi_workspace, OpenDevContainer);

        multi_workspace
            .update(cx, |multi_workspace, _, cx| {
                let modal = multi_workspace
                    .workspace()
                    .read(cx)
                    .active_modal::<RemoteServerProjects>(cx);
                assert!(
                    modal.is_some(),
                    "Dev container modal should be open after dispatching OpenDevContainer"
                );
            })
            .unwrap();
    }

    #[gpui::test]
    async fn test_open_dev_container_action_with_multiple_configs(cx: &mut TestAppContext) {
        let app_state = init_test(cx);

        app_state
            .fs
            .as_fake()
            .insert_tree(
                path!("/project"),
                json!({
                    ".devcontainer": {
                        "rust": {
                            "devcontainer.json": "{}"
                        },
                        "python": {
                            "devcontainer.json": "{}"
                        }
                    },
                    "src": {
                        "main.rs": "fn main() {}"
                    }
                }),
            )
            .await;

        cx.update(|cx| {
            open_paths(
                &[PathBuf::from(path!("/project"))],
                app_state,
                workspace::OpenOptions::default(),
                cx,
            )
        })
        .await
        .unwrap();

        assert_eq!(cx.update(|cx| cx.windows().len()), 1);
        let multi_workspace = cx.update(|cx| cx.windows()[0].downcast::<MultiWorkspace>().unwrap());

        cx.run_until_parked();

        cx.dispatch_action(*multi_workspace, OpenDevContainer);

        multi_workspace
            .update(cx, |multi_workspace, _, cx| {
                let modal = multi_workspace
                    .workspace()
                    .read(cx)
                    .active_modal::<RemoteServerProjects>(cx);
                assert!(
                    modal.is_some(),
                    "Dev container modal should be open after dispatching OpenDevContainer with multiple configs"
                );
            })
            .unwrap();
    }

    #[gpui::test]
    async fn test_open_local_project_reuses_multi_workspace_window(cx: &mut TestAppContext) {
        let app_state = init_test(cx);

        // Disable system path prompts so the injected mock is used.
        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.workspace.use_system_path_prompts = Some(false);
                });
            });
        });

        app_state
            .fs
            .as_fake()
            .insert_tree(
                path!("/initial-project"),
                json!({ "src": { "main.rs": "" } }),
            )
            .await;
        app_state
            .fs
            .as_fake()
            .insert_tree(path!("/new-project"), json!({ "lib": { "mod.rs": "" } }))
            .await;

        cx.update(|cx| {
            open_paths(
                &[PathBuf::from(path!("/initial-project"))],
                app_state.clone(),
                workspace::OpenOptions::default(),
                cx,
            )
        })
        .await
        .unwrap();

        let initial_window_count = cx.update(|cx| cx.windows().len());
        assert_eq!(initial_window_count, 1);

        let multi_workspace = cx.update(|cx| cx.windows()[0].downcast::<MultiWorkspace>().unwrap());
        cx.run_until_parked();

        let workspace = multi_workspace
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();

        // Set up the prompt mock to return the new project path.
        workspace.update(cx, |workspace, _cx| {
            workspace.set_prompt_for_open_path(Box::new(|_, _, _, _| {
                let (tx, rx) = futures::channel::oneshot::channel();
                tx.send(Some(vec![PathBuf::from(path!("/new-project"))]))
                    .ok();
                rx
            }));
        });

        // Call open_local_project with create_new_window: false.
        let weak_workspace = workspace.downgrade();
        multi_workspace
            .update(cx, |_, window, cx| {
                open_local_project(weak_workspace, false, window, cx);
            })
            .unwrap();

        cx.run_until_parked();

        // Should NOT have opened a new window.
        let final_window_count = cx.update(|cx| cx.windows().len());
        assert_eq!(
            final_window_count, initial_window_count,
            "open_local_project with create_new_window=false should reuse the current multi-workspace window"
        );
    }

    #[gpui::test]
    async fn test_open_local_project_new_window_creates_new_window(cx: &mut TestAppContext) {
        let app_state = init_test(cx);

        // Disable system path prompts so the injected mock is used.
        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.workspace.use_system_path_prompts = Some(false);
                });
            });
        });

        app_state
            .fs
            .as_fake()
            .insert_tree(
                path!("/initial-project"),
                json!({ "src": { "main.rs": "" } }),
            )
            .await;
        app_state
            .fs
            .as_fake()
            .insert_tree(path!("/new-project"), json!({ "lib": { "mod.rs": "" } }))
            .await;

        cx.update(|cx| {
            open_paths(
                &[PathBuf::from(path!("/initial-project"))],
                app_state.clone(),
                workspace::OpenOptions::default(),
                cx,
            )
        })
        .await
        .unwrap();

        let initial_window_count = cx.update(|cx| cx.windows().len());
        assert_eq!(initial_window_count, 1);

        let multi_workspace = cx.update(|cx| cx.windows()[0].downcast::<MultiWorkspace>().unwrap());
        cx.run_until_parked();

        let workspace = multi_workspace
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();

        // Set up the prompt mock to return the new project path.
        workspace.update(cx, |workspace, _cx| {
            workspace.set_prompt_for_open_path(Box::new(|_, _, _, _| {
                let (tx, rx) = futures::channel::oneshot::channel();
                tx.send(Some(vec![PathBuf::from(path!("/new-project"))]))
                    .ok();
                rx
            }));
        });

        // Call open_local_project with create_new_window: true.
        let weak_workspace = workspace.downgrade();
        multi_workspace
            .update(cx, |_, window, cx| {
                open_local_project(weak_workspace, true, window, cx);
            })
            .unwrap();

        cx.run_until_parked();

        // Should have opened a new window.
        let final_window_count = cx.update(|cx| cx.windows().len());
        assert_eq!(
            final_window_count,
            initial_window_count + 1,
            "open_local_project with create_new_window=true should open a new window"
        );
    }

    fn init_test(cx: &mut TestAppContext) -> Arc<AppState> {
        cx.update(|cx| {
            let state = AppState::test(cx);
            crate::init(cx);
            editor::init(cx);
            state
        })
    }

    #[gpui::test]
    async fn test_remote_project_group_confirm_does_not_create_local_workspace(
        cx: &mut TestAppContext,
    ) {
        // Regression test: confirming a ProjectGroup entry with a remote host
        // should call find_or_create_workspace with the host, not
        // find_or_create_local_workspace.
        let app_state = init_test(cx);
        app_state
            .fs
            .as_fake()
            .insert_tree("/local", json!({}))
            .await;

        cx.update(|cx| {
            open_paths(
                &[PathBuf::from("/local")],
                app_state,
                workspace::OpenOptions::default(),
                cx,
            )
        })
        .await
        .unwrap();

        cx.run_until_parked();

        let mw = cx.update(|cx| cx.windows()[0].downcast::<MultiWorkspace>().unwrap());
        let remote_key = remote_project_group(1);

        // Get workspace info via WindowHandle::read_with (returns Result)
        let (workspace, groups, fh) = mw
            .read_with(cx, |mw, _cx| {
                let ws = mw.workspace().clone();
                (
                    ws.clone(),
                    mw.project_group_keys(),
                    ws.read(_cx).focus_handle(_cx),
                )
            })
            .unwrap();

        let mut augmented_groups = groups.clone();
        augmented_groups.push(remote_key.clone());

        // Create the popover (same as the title bar does)
        let popover: Entity<RecentProjects> = cx.update(|cx| {
            let window = cx.windows()[0];
            window
                .update(cx, |_, window, cx| {
                    RecentProjects::popover(
                        workspace.downgrade(),
                        augmented_groups,
                        Some(false),
                        fh,
                        window,
                        cx,
                    )
                })
                .unwrap()
        });

        cx.run_until_parked();

        // Get the picker from the popover
        let picker: Entity<Picker<RecentProjectsDelegate>> = cx.update(|cx| {
            let window = cx.windows()[0];
            window
                .update(cx, |_, _window, cx| popover.read(cx).picker.clone())
                .unwrap()
        });

        cx.run_until_parked();

        // Find the remote project group entry index via Entity::read_with (no unwrap)
        let filtered = picker.read_with(cx, |p, _| p.delegate.filtered_entries.clone());
        let remote_idx = filtered
            .iter()
            .position(|entry| {
                matches!(entry, ProjectPickerEntry::ProjectGroup(m) if m.candidate_id == groups.len())
            })
            .expect("remote project group entry should exist");

        // Select and confirm the remote entry via Entity::update
        let _ = cx.update(|cx| {
            let window = cx.windows()[0];
            window.update(cx, |_, window, cx| {
                picker.update(cx, |picker, cx| {
                    picker.delegate.set_selected_index(remote_idx, window, cx);
                    picker.delegate.confirm(false, window, cx);
                });
            })
        });

        cx.run_until_parked();

        // Verify no local workspace was created for the remote paths
        let has_local = mw
            .read_with(cx, |mw, cx| {
                mw.workspace_for_paths(remote_key.path_list(), None, cx)
                    .is_some()
            })
            .unwrap();
        assert!(
            !has_local,
            "remote project group confirm should not create a local workspace"
        );
    }

    #[gpui::test]
    async fn test_remove_open_folder_rekeys_this_window_group(cx: &mut TestAppContext) {
        // Regression test: removing a folder from the active project while the
        // picker is open must update the "This Window" group so it no longer
        // lists the removed folder.
        let app_state = init_test(cx);
        app_state
            .fs
            .as_fake()
            .insert_tree(path!("/a"), json!({ "1.txt": "" }))
            .await;
        app_state
            .fs
            .as_fake()
            .insert_tree(path!("/b"), json!({ "2.txt": "" }))
            .await;

        cx.update(|cx| {
            open_paths(
                &[PathBuf::from(path!("/a")), PathBuf::from(path!("/b"))],
                app_state,
                workspace::OpenOptions::default(),
                cx,
            )
        })
        .await
        .unwrap();
        cx.run_until_parked();

        let mw = cx.update(|cx| cx.windows()[0].downcast::<MultiWorkspace>().unwrap());
        let (workspace, active_key, fh) = mw
            .read_with(cx, |mw, cx| {
                let ws = mw.workspace().clone();
                (
                    ws.clone(),
                    ws.read(cx).project_group_key(cx),
                    ws.read(cx).focus_handle(cx),
                )
            })
            .unwrap();

        assert_eq!(
            active_key.path_list().paths().len(),
            2,
            "group should span both folders before removal"
        );
        let groups = vec![active_key];

        let popover: Entity<RecentProjects> = cx.update(|cx| {
            let window = cx.windows()[0];
            window
                .update(cx, |_, window, cx| {
                    RecentProjects::popover(
                        workspace.downgrade(),
                        groups,
                        Some(false),
                        fh,
                        window,
                        cx,
                    )
                })
                .unwrap()
        });
        cx.run_until_parked();

        let picker: Entity<Picker<RecentProjectsDelegate>> = cx.update(|cx| {
            let window = cx.windows()[0];
            window
                .update(cx, |_, _window, cx| popover.read(cx).picker.clone())
                .unwrap()
        });
        cx.run_until_parked();

        let a_worktree_id = workspace
            .read_with(cx, |workspace, cx| {
                workspace
                    .project()
                    .read(cx)
                    .visible_worktrees(cx)
                    .find(|wt| wt.read(cx).abs_path().ends_with("a"))
                    .map(|wt| wt.read(cx).id())
            })
            .expect("a worktree should exist");

        cx.update(|cx| {
            let window = cx.windows()[0];
            window
                .update(cx, |_, window, cx| {
                    picker.update(cx, |picker, cx| {
                        RecentProjectsDelegate::remove_open_folder(
                            picker,
                            a_worktree_id,
                            window,
                            cx,
                        );
                    });
                })
                .unwrap();
        });
        cx.run_until_parked();

        let groups_after = picker.read_with(cx, |picker, _| {
            picker.delegate.window_project_groups.clone()
        });
        assert!(
            !groups_after.iter().any(|key| key
                .path_list()
                .paths()
                .iter()
                .any(|path| path.ends_with("a"))),
            "the removed folder should no longer appear in any This Window group, got {groups_after:?}"
        );
    }
}
