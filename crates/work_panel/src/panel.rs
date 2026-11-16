use crate::{
    LIST_CONTEXT, chip, group_by_status, is_ready, pr_chip, priority_label,
    preview::{IssuePreview, ProjectPreview},
    section, start, worktree_chip,
};
use editor::{Editor, EditorEvent};
use gpui::{
    Action, App, AppContext as _, Bounds, ClipboardItem, Context, DismissEvent, Entity,
    EventEmitter, FocusHandle, Focusable, MouseButton, Pixels, Point, Render, Subscription, Task,
    WeakEntity, Window, actions, anchored, canvas, deferred, point, px,
};
use n8n_worktree_manager::{
    Manager,
    github::PrStatus,
    linear::{IssueRow, ProjectSummary, ProjectView},
    settings::Settings,
    system::RealSystem,
};
use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
use ui::{Checkbox, ContextMenu, ToggleState, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    StatusItemView, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(
    work_panel,
    [
        /// Opens or focuses the Work panel: your Linear issues and their worktrees.
        ToggleFocus,
        /// Opens the worktree of the selected issue, or creates it.
        StartWork,
        /// Shows the selected issue in the preview tab.
        OpenPreview,
        /// Shows the actions of the selected issue.
        ToggleActions,
        /// Opens the selected issue in Linear.
        OpenInLinear,
        /// Starts Claude Code about the selected issue, with no worktree.
        AskClaude,
        /// Copies the Linear branch name of the selected issue.
        CopyBranch,
        /// Copies the key and the title of the selected issue.
        CopyKeyAndTitle,
        /// Copies the Linear URL of the selected issue.
        CopyUrl,
    ]
);

/// Linear is asked again this often while Zed8 runs.
const REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// The worktrees' setup progress is read from meta.json this often.
const LOCAL_INTERVAL: Duration = Duration::from_secs(3);
/// The project search waits this long after the last key.
const SEARCH_DELAY: Duration = Duration::from_millis(300);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    UpNext,
    Project,
    Reviews,
}

struct ActionsMenu {
    menu: Entity<ContextMenu>,
    position: Point<Pixels>,
    _subscription: Subscription,
}

pub struct WorkPanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    position: DockPosition,
    /// Made on a background thread, because loading the settings starts a
    /// process.
    manager: Option<Arc<Manager>>,
    rows: Vec<IssueRow>,
    /// When Linear answered last.
    synced_at: Option<Instant>,
    refreshing: bool,
    error: Option<String>,
    tab: Tab,
    filter: Entity<Editor>,
    /// The key of the selected issue.
    selected: Option<String>,
    /// Where the selected row was drawn last, for the actions menu.
    selected_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    actions_menu: Option<ActionsMenu>,
    /// The PR of each worktree, by worktree name.
    prs: HashMap<String, PrStatus>,
    my_projects: Option<Vec<ProjectSummary>>,
    picker_open: bool,
    search: Entity<Editor>,
    /// The projects that match the search text, mine first.
    search_results: Option<Vec<ProjectSummary>>,
    project: Option<(ProjectView, Vec<IssueRow>)>,
    project_loading: bool,
    show_finished: bool,
    /// The PRs you review or wrote, for the Reviews tab.
    reviews: Option<Vec<crate::reviews::ReviewRow>>,
    reviews_error: Option<String>,
    _reviews_task: Task<()>,
    _refresh_task: Task<()>,
    _local_task: Task<()>,
    _search_task: Task<()>,
    _project_task: Task<()>,
    _pr_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl WorkPanel {
    pub fn new(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Filter issues", window, cx);
            editor
        });
        let search = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search projects", window, cx);
            editor
        });
        let subscriptions = vec![
            cx.subscribe(&filter, |_, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    cx.notify();
                }
            }),
            cx.subscribe(&search, |panel, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    panel.search_projects(cx);
                }
            }),
        ];
        let refresh_task = cx.spawn(async move |this, cx| {
            let (manager, rows) = cx
                .background_spawn(async move {
                    let manager = Arc::new(Manager::new(Settings::load(), Arc::new(RealSystem)));
                    let rows = manager.cached_issues();
                    (manager, rows)
                })
                .await;
            let updated = this.update(cx, |panel, cx| {
                panel.manager = Some(manager);
                match rows {
                    Ok(rows) => panel.set_rows(rows, cx),
                    Err(error) => panel.error = Some(format!("{error:#}")),
                }
                panel._local_task = panel.poll_local(cx);
                cx.notify();
            });
            if updated.is_err() {
                return;
            }
            loop {
                let Ok(task) = this.update(cx, |panel, cx| panel.refresh(cx)) else {
                    return;
                };
                task.await;
                cx.background_executor().timer(REFRESH_INTERVAL).await;
            }
        });
        Self {
            workspace,
            focus_handle: cx.focus_handle(),
            position: DockPosition::Right,
            manager: None,
            rows: Vec::new(),
            synced_at: None,
            refreshing: false,
            error: None,
            tab: Tab::UpNext,
            filter,
            selected: None,
            selected_bounds: Rc::new(Cell::new(None)),
            actions_menu: None,
            prs: HashMap::new(),
            my_projects: None,
            picker_open: false,
            search,
            search_results: None,
            project: None,
            project_loading: false,
            show_finished: false,
            reviews: None,
            reviews_error: None,
            _reviews_task: Task::ready(()),
            _refresh_task: refresh_task,
            _local_task: Task::ready(()),
            _search_task: Task::ready(()),
            _project_task: Task::ready(()),
            _pr_task: Task::ready(()),
            _subscriptions: subscriptions,
        }
    }

    /// "Linear: synced 1m ago", for the status bar.
    pub fn sync_text(&self) -> String {
        if self.refreshing {
            return "Linear: syncing…".to_string();
        }
        if self.error.is_some() && self.synced_at.is_none() {
            return "Linear: not synced".to_string();
        }
        match self.synced_at {
            Some(at) => match at.elapsed().as_secs() / 60 {
                0 => "Linear: synced just now".to_string(),
                minutes => format!("Linear: synced {minutes}m ago"),
            },
            None => "Linear: from the cache".to_string(),
        }
    }

    fn set_rows(&mut self, rows: Vec<IssueRow>, cx: &mut Context<Self>) {
        self.rows = rows;
        if self.selected.is_none() {
            self.selected = self.visible_rows(cx).first().map(|row| row.issue.identifier.clone());
        }
    }

    /// Asks Linear for your open issues and joins them with the worktrees.
    pub fn refresh(&mut self, cx: &mut Context<Self>) -> Task<()> {
        let Some(manager) = self.manager.clone() else {
            return Task::ready(());
        };
        if self.refreshing {
            return Task::ready(());
        }
        self.refreshing = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let rows = cx
                .background_spawn(async move { manager.issues(&manager.linear()) })
                .await;
            this.update(cx, |panel, cx| {
                panel.refreshing = false;
                match rows {
                    Ok(rows) => {
                        panel.set_rows(rows, cx);
                        panel.synced_at = Some(Instant::now());
                        panel.error = None;
                        panel.load_prs(cx);
                        panel.load_reviews(cx);
                    }
                    Err(error) => panel.error = Some(format!("{error:#}")),
                }
                cx.notify();
            })
            .ok();
        })
    }

    /// Reads the setup progress of the worktrees from meta.json, so a row
    /// shows "setting up 3/6" while `provision` runs.
    fn poll_local(&mut self, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(LOCAL_INTERVAL).await;
                let Ok(Some(manager)) = this.read_with(cx, |panel, _| panel.manager.clone()) else {
                    return;
                };
                let rows = cx
                    .background_spawn(async move { manager.cached_issues() })
                    .await;
                let updated = this.update(cx, |panel, cx| {
                    if let Ok(rows) = rows
                        && !rows.is_empty()
                        && rows != panel.rows
                    {
                        panel.set_rows(rows, cx);
                    }
                    // Also moves "synced 1m ago" along.
                    cx.notify();
                });
                if updated.is_err() {
                    return;
                }
            }
        })
    }

    /// The PR and CI state of every worktree in the lists.
    fn load_prs(&mut self, cx: &mut Context<Self>) {
        let Some(manager) = self.manager.clone() else {
            return;
        };
        let mut names: HashSet<String> =
            self.rows.iter().filter_map(|row| row.worktree.clone()).collect();
        if let Some((_, rows)) = &self.project {
            names.extend(rows.iter().filter_map(|row| row.worktree.clone()));
        }
        self._pr_task = cx.spawn(async move |this, cx| {
            let prs = cx
                .background_spawn(async move {
                    names
                        .into_iter()
                        .filter_map(|name| {
                            let pr = manager.pr_status(&name).log_err().flatten()?;
                            Some((name, pr))
                        })
                        .collect::<HashMap<_, _>>()
                })
                .await;
            this.update(cx, |panel, cx| {
                panel.prs.extend(prs);
                cx.notify();
            })
            .ok();
        });
    }

    /// The rows of the open tab, in the order they show.
    fn visible_rows(&self, cx: &App) -> Vec<IssueRow> {
        match self.tab {
            Tab::UpNext => {
                let filter = self.filter.read(cx).text(cx).trim().to_lowercase();
                group_by_status(&self.rows)
                    .into_iter()
                    .flat_map(|(_, rows)| rows)
                    .filter(|row| filter.is_empty() || matches_filter(row, &filter))
                    .collect()
            }
            Tab::Project => self
                .project_groups()
                .into_iter()
                .flat_map(|(_, rows)| rows)
                .collect(),
            Tab::Reviews => Vec::new(),
        }
    }

    /// The visible issues of the project, by milestone. Issues without a
    /// milestone come last.
    fn project_groups(&self) -> Vec<(String, Vec<IssueRow>)> {
        let Some((project, rows)) = &self.project else {
            return Vec::new();
        };
        let mut groups: Vec<(Option<String>, String, Vec<IssueRow>)> = project
            .milestones
            .iter()
            .map(|milestone| {
                let title = format!("{} · {:.0}%", milestone.name, milestone.percent);
                (Some(milestone.name.clone()), title, Vec::new())
            })
            .collect();
        groups.push((None, "No milestone".to_string(), Vec::new()));
        for (issue, row) in project.issues.iter().zip(rows) {
            if issue.finished && !self.show_finished {
                continue;
            }
            if let Some((_, _, group)) = groups.iter_mut().find(|(name, _, _)| *name == issue.milestone) {
                group.push(row.clone());
            }
        }
        groups
            .into_iter()
            .filter(|(_, _, rows)| !rows.is_empty())
            .map(|(_, title, rows)| (title, rows))
            .collect()
    }

    fn selected_row(&self, cx: &App) -> Option<IssueRow> {
        let key = self.selected.as_ref()?;
        self.visible_rows(cx)
            .into_iter()
            .find(|row| &row.issue.identifier == key)
    }

    fn select_offset(&mut self, offset: isize, cx: &mut Context<Self>) {
        let rows = self.visible_rows(cx);
        if rows.is_empty() {
            return;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|key| rows.iter().position(|row| &row.issue.identifier == key));
        let next = match current {
            Some(ix) => ix.saturating_add_signed(offset).min(rows.len() - 1),
            None => 0,
        };
        self.selected = rows.get(next).map(|row| row.issue.identifier.clone());
        cx.notify();
    }

    fn start_work(&mut self, _: &StartWork, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(manager), Some(row)) = (self.manager.clone(), self.selected_row(cx)) else {
            return;
        };
        // Up next lists only the issues assigned to you.
        let mine = self
            .rows
            .iter()
            .any(|mine| mine.issue.identifier == row.issue.identifier);
        start::start_work(self.workspace.clone(), manager, row, mine, window, cx);
    }

    fn open_preview(&mut self, _: &OpenPreview, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(manager), Some(row)) = (self.manager.clone(), self.selected_row(cx)) else {
            return;
        };
        let pr = row.worktree.as_ref().and_then(|name| self.prs.get(name).cloned());
        let handle = self.workspace.clone();
        self.workspace
            .update(cx, |workspace, cx| {
                let existing = workspace.items_of_type::<IssuePreview>(cx).next();
                match existing {
                    Some(preview) => {
                        preview.update(cx, |preview, cx| preview.show(row, pr, cx));
                        workspace.activate_item(&preview, true, false, window, cx);
                    }
                    None => {
                        let preview = cx.new(|cx| IssuePreview::new(row, pr, manager, handle, cx));
                        workspace.add_item_to_active_pane(Box::new(preview), None, false, window, cx);
                    }
                }
            })
            .log_err();
    }

    fn open_in_linear(&mut self, _: &OpenInLinear, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.selected_row(cx) {
            cx.open_url(&row.issue.url);
        }
    }

    fn ask_claude(&mut self, _: &AskClaude, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.selected_row(cx) {
            start::ask_claude(self.workspace.clone(), &row, window, cx);
        }
    }

    fn copy(&mut self, text: Option<String>, cx: &mut Context<Self>) {
        if let Some(text) = text {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn toggle_actions(&mut self, _: &ToggleActions, window: &mut Window, cx: &mut Context<Self>) {
        if self.actions_menu.take().is_some() {
            cx.notify();
            return;
        }
        let position = self
            .selected_bounds
            .get()
            .map(|bounds| bounds.bottom_left() + point(px(14.), px(-4.)))
            .unwrap_or_else(|| point(px(0.), px(0.)));
        self.deploy_actions(position, window, cx);
    }

    fn deploy_actions(&mut self, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.selected_row(cx) else {
            return;
        };
        let focus = self.focus_handle.clone();
        let start_label = worktree_label(&row);
        let menu = ContextMenu::build(window, cx, |menu, _, _| {
            menu.context(focus)
                .header(format!("{} · ⌘K", row.issue.identifier))
                .action(start_label, Box::new(StartWork))
                .action("Open preview", Box::new(OpenPreview))
                .action("Open in Linear", Box::new(OpenInLinear))
                .action("Ask Claude about it, no worktree", Box::new(AskClaude))
                .separator()
                .action("Copy branch name", Box::new(CopyBranch))
                .action("Copy key and title", Box::new(CopyKeyAndTitle))
                .action("Copy URL", Box::new(CopyUrl))
        });
        window.focus(&menu.focus_handle(cx), cx);
        let subscription = cx.subscribe_in(&menu, window, |panel, _, _: &DismissEvent, window, cx| {
            panel.actions_menu = None;
            window.focus(&panel.focus_handle, cx);
            cx.notify();
        });
        self.actions_menu = Some(ActionsMenu {
            menu,
            position,
            _subscription: subscription,
        });
        cx.notify();
    }

    /// The PRs of the Reviews tab, from GitHub and the review rounds.
    fn load_reviews(&mut self, cx: &mut Context<Self>) {
        let Some(manager) = self.manager.clone() else {
            return;
        };
        self._reviews_task = cx.spawn(async move |this, cx| {
            let rows = cx
                .background_spawn(async move {
                    let worktrees: Vec<(String, String)> = manager
                        .list()
                        .unwrap_or_default()
                        .into_iter()
                        .map(|meta| (meta.name, meta.branch))
                        .collect();
                    let reviews_dir = util::paths::home_dir().join("workspace").join("n8n-reviews");
                    crate::reviews::load(&reviews_dir, &worktrees)
                })
                .await;
            this.update(cx, |panel, cx| {
                match rows {
                    Ok(rows) => {
                        panel.reviews = Some(rows);
                        panel.reviews_error = None;
                    }
                    Err(error) => panel.reviews_error = Some(format!("{error:#}")),
                }
                cx.notify();
            })
            .ok();
        });
    }

    fn waiting_reviews(&self) -> usize {
        self.reviews
            .as_ref()
            .map(|rows| rows.iter().filter(|row| row.group == crate::reviews::ReviewGroup::WaitsOnYou).count())
            .unwrap_or_default()
    }

    fn open_review(&mut self, row: &crate::reviews::ReviewRow, window: &mut Window, cx: &mut Context<Self>) {
        let Some(manager) = self.manager.clone() else {
            return;
        };
        match row.worktree.clone() {
            Some(name) => start::open_worktree(self.workspace.clone(), manager, name, false, false, window, cx),
            None => start::start_review(self.workspace.clone(), manager, row.number, row.branch.clone(), window, cx),
        }
    }

    fn render_reviews(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        use crate::reviews::ReviewGroup;
        let mut elements = Vec::new();
        if let Some(error) = &self.reviews_error {
            elements.push(
                div()
                    .px_3()
                    .py_1()
                    .child(Label::new(error.clone()).size(LabelSize::Small).color(Color::Error))
                    .into_any_element(),
            );
        }
        let Some(rows) = &self.reviews else {
            elements.push(section("Loading the PRs…").into_any_element());
            return elements;
        };
        let hover = cx.theme().colors().ghost_element_hover;
        for group in [ReviewGroup::WaitsOnYou, ReviewGroup::WaitsOnAuthor, ReviewGroup::Yours] {
            let group_rows: Vec<&crate::reviews::ReviewRow> = rows.iter().filter(|row| row.group == group).collect();
            elements.push(section(format!("{} · {}", group.title(), group_rows.len())).into_any_element());
            if group_rows.is_empty() {
                elements.push(
                    div()
                        .px_3()
                        .pb_1p5()
                        .child(Label::new("Nothing here").size(LabelSize::XSmall).color(Color::Muted))
                        .into_any_element(),
                );
            }
            for row in group_rows {
                let open_row = row.clone();
                let url = row.url.clone();
                let label = if row.worktree.is_some() { "Open review" } else { "Start review" };
                let dot = match group {
                    ReviewGroup::WaitsOnYou => Color::Warning,
                    ReviewGroup::WaitsOnAuthor => Color::Muted,
                    ReviewGroup::Yours => Color::Success,
                };
                elements.push(
                    v_flex()
                        .id(("review-row", row.number as usize))
                        .mx_1p5()
                        .px_1p5()
                        .py_1()
                        .gap_0p5()
                        .rounded_md()
                        .hover(move |style| style.bg(hover))
                        .child(
                            h_flex()
                                .gap_1p5()
                                .child(Label::new(format!("#{}", row.number)).size(LabelSize::Small).weight(gpui::FontWeight::SEMIBOLD))
                                .child(div().flex_1().min_w_0().child(Label::new(row.title.clone()).size(LabelSize::Small).color(Color::Muted).truncate()))
                                .when(group != ReviewGroup::Yours, |line| {
                                    line.child(Label::new(row.author.clone()).size(LabelSize::XSmall).color(Color::Muted))
                                }),
                        )
                        .child(
                            h_flex()
                                .gap_1p5()
                                .child(Icon::new(IconName::Circle).size(IconSize::XSmall).color(dot))
                                .child(div().flex_1().min_w_0().child(Label::new(row.state.clone()).size(LabelSize::XSmall).color(Color::Muted).truncate()))
                                .child(
                                    IconButton::new(("review-on-github", row.number as usize), IconName::ArrowUpRight)
                                        .icon_size(IconSize::XSmall)
                                        .icon_color(Color::Muted)
                                        .tooltip(ui::Tooltip::text("Opens the PR on GitHub"))
                                        .on_click(move |_, _, cx| cx.open_url(&url)),
                                )
                                .child(
                                    Button::new(("review-open", row.number as usize), label)
                                        .style(ButtonStyle::Outlined)
                                        .label_size(LabelSize::XSmall)
                                        .on_click(cx.listener(move |panel, _, window, cx| panel.open_review(&open_row, window, cx))),
                                ),
                        )
                        .into_any_element(),
                );
            }
        }
        elements
    }

    fn show_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tab = tab;
        self.picker_open = false;
        if tab == Tab::Reviews {
            self.load_reviews(cx);
        }
        if tab == Tab::Project && self.my_projects.is_none() {
            self.load_my_projects(cx);
        }
        self.selected = self.visible_rows(cx).first().map(|row| row.issue.identifier.clone());
        cx.notify();
    }

    fn load_my_projects(&mut self, cx: &mut Context<Self>) {
        let Some(manager) = self.manager.clone() else {
            return;
        };
        self._search_task = cx.spawn(async move |this, cx| {
            let projects = cx
                .background_spawn(async move { manager.linear().projects(None) })
                .await;
            this.update(cx, |panel, cx| {
                match projects {
                    Ok(projects) => {
                        // The first of my projects shows until you pick another.
                        if panel.project.is_none()
                            && let Some(first) = projects.first()
                        {
                            panel.select_project(first.id.clone(), false, cx);
                        }
                        panel.my_projects = Some(projects);
                    }
                    Err(error) => panel.error = Some(format!("{error:#}")),
                }
                cx.notify();
            })
            .ok();
        });
    }

    /// Searches all projects of the workspace, a moment after the last key.
    fn search_projects(&mut self, cx: &mut Context<Self>) {
        let text = self.search.read(cx).text(cx).trim().to_string();
        if text.is_empty() {
            self.search_results = None;
            self._search_task = Task::ready(());
            cx.notify();
            return;
        }
        let Some(manager) = self.manager.clone() else {
            return;
        };
        self._search_task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DELAY).await;
            let projects = cx
                .background_spawn(async move { manager.linear().projects(Some(&text)) })
                .await;
            this.update(cx, |panel, cx| {
                match projects {
                    Ok(projects) => panel.search_results = Some(projects),
                    Err(error) => panel.error = Some(format!("{error:#}")),
                }
                cx.notify();
            })
            .ok();
        });
    }

    /// Loads a project into the Project tab. With `open_tab`, it also shows
    /// the project in its tab of the center pane.
    fn select_project(&mut self, id: String, open_tab: bool, cx: &mut Context<Self>) {
        let Some(manager) = self.manager.clone() else {
            return;
        };
        self.picker_open = false;
        self.search_results = None;
        self.project_loading = true;
        cx.notify();
        self._project_task = cx.spawn(async move |this, cx| {
            let project = cx
                .background_spawn(async move { manager.project(&manager.linear(), &id) })
                .await;
            this.update(cx, |panel, cx| {
                panel.project_loading = false;
                match project {
                    Ok(project) => {
                        panel.project = Some(project);
                        if panel.tab == Tab::Project {
                            panel.selected =
                                panel.visible_rows(cx).first().map(|row| row.issue.identifier.clone());
                        }
                        panel.load_prs(cx);
                        if open_tab {
                            panel.open_project_tab(cx);
                        }
                    }
                    Err(error) => panel.error = Some(format!("{error:#}")),
                }
                cx.notify();
            })
            .ok();
        });
    }

    /// Shows the project in its tab of the center pane, without focus.
    fn open_project_tab(&mut self, cx: &mut Context<Self>) {
        let Some((project, rows)) = self.project.clone() else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let Some(window) = window_of(&workspace, cx) else {
            return;
        };
        cx.defer(move |cx| {
            window
                .update(cx, |_, window, cx| {
                    workspace.update(cx, |workspace, cx| {
                        let existing = workspace.items_of_type::<ProjectPreview>(cx).next();
                        match existing {
                            Some(preview) => {
                                preview.update(cx, |preview, cx| preview.show(project, rows, cx));
                                workspace.activate_item(&preview, true, false, window, cx);
                            }
                            None => {
                                let preview = cx.new(|cx| ProjectPreview::new(project, rows, cx));
                                workspace.add_item_to_active_pane(Box::new(preview), None, false, window, cx);
                            }
                        }
                    })
                })
                .log_err();
        });
    }

    fn toggle_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.picker_open = !self.picker_open;
        if self.picker_open {
            self.search.update(cx, |search, cx| search.clear(window, cx));
            self.search_results = None;
            window.focus(&self.search.focus_handle(cx), cx);
        }
        cx.notify();
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tab_button = |id: &'static str, label: &'static str, tab: Tab, cx: &mut Context<Self>| {
            let selected = self.tab == tab;
            Button::new(id, label)
                .label_size(LabelSize::Small)
                .style(if selected { ButtonStyle::Outlined } else { ButtonStyle::Subtle })
                .when(!selected, |button| button.color(Color::Muted))
                .on_click(cx.listener(move |panel, _, _, cx| panel.show_tab(tab, cx)))
        };
        h_flex()
            .px_2p5()
            .py_2()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                div()
                    .mr_auto()
                    .child(Label::new("Work").weight(gpui::FontWeight::SEMIBOLD)),
            )
            .child(tab_button("work-panel-tab-up-next", "Up next", Tab::UpNext, cx))
            .child(tab_button("work-panel-tab-project", "Project", Tab::Project, cx))
            .child(
                h_flex()
                    .child(tab_button("work-panel-tab-reviews", "Reviews", Tab::Reviews, cx))
                    .when(self.waiting_reviews() > 0, |tab| {
                        tab.child(
                            div()
                                .ml_0p5()
                                .px_1()
                                .rounded_full()
                                .bg(cx.theme().status().warning.opacity(0.25))
                                .child(Label::new(self.waiting_reviews().to_string()).size(LabelSize::XSmall).color(Color::Warning)),
                        )
                    }),
            )
    }

    fn input_box(&self, editor: &Entity<Editor>, cx: &App) -> Div {
        h_flex()
            .gap_2()
            .px_2p5()
            .py_1p5()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().editor_background)
            .child(
                Icon::new(IconName::MagnifyingGlass)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(div().flex_1().child(editor.clone()))
    }

    fn render_row(&self, ix: usize, row: &IssueRow, in_project: bool, cx: &mut Context<Self>) -> AnyElement {
        let key = row.issue.identifier.clone();
        let selected = self.selected.as_deref() == Some(key.as_str());
        let finished = in_project
            && self
                .project
                .as_ref()
                .and_then(|(project, _)| {
                    project
                        .issues
                        .iter()
                        .find(|issue| issue.issue.identifier == key)
                        .map(|issue| issue.finished)
                })
                .unwrap_or(false);
        let right_chip = if in_project {
            if is_ready(&row.issue.status) {
                Some(chip("Ready for pick up", Some(Color::Warning), cx))
            } else {
                Some(chip(row.issue.status.clone(), None, cx))
            }
        } else if matches!(row.issue.priority, 1 | 2) {
            Some(chip(priority_label(row.issue.priority), Some(Color::Error), cx))
        } else {
            row.issue.project.clone().map(|project| chip(project, None, cx))
        };
        let mut chips: Vec<AnyElement> = Vec::new();
        if let Some(worktree) = worktree_chip(row, cx) {
            chips.push(worktree.into_any_element());
        }
        if let Some(pr) = row.worktree.as_ref().and_then(|name| self.prs.get(name)) {
            chips.push(pr_chip(pr, cx).into_any_element());
        }
        let click_key = key.clone();
        let menu_key = key.clone();
        let button_key = key.clone();
        let bounds = self.selected_bounds.clone();
        v_flex()
            .id(("work-panel-issue", ix))
            .group("work-panel-row")
            .relative()
            .mx_1p5()
            .px_3()
            .py_2()
            .gap_1()
            .rounded_md()
            .cursor_pointer()
            .when(selected, |this| this.bg(cx.theme().colors().element_selected))
            .when(!selected, |this| {
                this.hover(|style| style.bg(cx.theme().colors().element_hover))
            })
            .on_click(cx.listener(move |panel, _, window, cx| {
                panel.selected = Some(click_key.clone());
                window.focus(&panel.focus_handle, cx);
                panel.open_preview(&OpenPreview, window, cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |panel, event: &gpui::MouseDownEvent, window, cx| {
                    panel.selected = Some(menu_key.clone());
                    window.focus(&panel.focus_handle, cx);
                    panel.deploy_actions(event.position, window, cx);
                }),
            )
            .child(
                div()
                    .absolute()
                    .top_1p5()
                    .right_2()
                    .invisible()
                    .group_hover("work-panel-row", |style| style.visible())
                    .child(
                        Button::new(("work-panel-row-worktree", ix), worktree_label(row))
                            .label_size(LabelSize::XSmall)
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(move |panel, _, window, cx| {
                                panel.selected = Some(button_key.clone());
                                panel.start_work(&StartWork, window, cx);
                                cx.stop_propagation();
                            })),
                    ),
            )
            .when(selected, |this| {
                this.child(
                    canvas(
                        move |row_bounds, _, _| bounds.set(Some(row_bounds)),
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .size_full(),
                )
            })
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Label::new(key)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .buffer_font(cx),
                    )
                    .when_some(right_chip, |this, right_chip| {
                        this.child(div().ml_auto().child(right_chip))
                    }),
            )
            .child(
                Label::new(row.issue.title.clone())
                    .size(LabelSize::Small)
                    .when(finished, |label| label.color(Color::Muted)),
            )
            .when(!chips.is_empty(), |this| {
                this.child(h_flex().gap_1p5().flex_wrap().children(chips))
            })
            .into_any_element()
    }

    fn render_up_next(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let filter = self.filter.read(cx).text(cx).trim().to_lowercase();
        let mut elements = Vec::new();
        let mut ix = 0;
        for (status, rows) in group_by_status(&self.rows) {
            let rows: Vec<IssueRow> = rows
                .into_iter()
                .filter(|row| filter.is_empty() || matches_filter(row, &filter))
                .collect();
            elements.push(section(format!("{status} · {}", rows.len())).into_any_element());
            if rows.is_empty() {
                elements.push(
                    div()
                        .px_3()
                        .pb_1p5()
                        .child(
                            Label::new(format!("Nothing {}", status.to_lowercase()))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .into_any_element(),
                );
            }
            for row in &rows {
                elements.push(self.render_row(ix, row, false, cx));
                ix += 1;
            }
        }
        elements
    }

    fn render_project_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut elements = Vec::new();
        if self.project_loading {
            elements.push(section("Loading the project…").into_any_element());
            return elements;
        }
        if self.project.is_none() {
            let text = if self.my_projects.is_some() {
                "Pick a project above."
            } else {
                "Loading your projects…"
            };
            elements.push(section(text).into_any_element());
            return elements;
        }
        let mut ix = 10_000;
        for (title, rows) in self.project_groups() {
            elements.push(section(title).into_any_element());
            for row in &rows {
                elements.push(self.render_row(ix, row, true, cx));
                ix += 1;
            }
        }
        elements.push(
            div()
                .px_3()
                .py_2()
                .child(
                    Checkbox::new("work-panel-show-finished", ToggleState::from(self.show_finished))
                        .label("Show finished issues")
                        .label_size(LabelSize::XSmall)
                        .on_click(cx.listener(|panel, state: &ToggleState, _, cx| {
                            panel.show_finished = state.selected();
                            cx.notify();
                        })),
                )
                .into_any_element(),
        );
        elements
    }

    fn render_picker_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let name = self
            .project
            .as_ref()
            .map(|(project, _)| project.name.clone())
            .unwrap_or_else(|| "Pick a project".to_string());
        let border = if self.picker_open {
            cx.theme().colors().border_focused
        } else {
            cx.theme().colors().border
        };
        div().p_2p5().child(
            h_flex()
                .id("work-panel-project-picker")
                .gap_2()
                .px_2p5()
                .py_1p5()
                .rounded_md()
                .border_1()
                .border_color(border)
                .bg(cx.theme().colors().editor_background)
                .cursor_pointer()
                .on_click(cx.listener(|panel, _, window, cx| panel.toggle_picker(window, cx)))
                .child(Label::new(format!("▣ {name}")).size(LabelSize::Small))
                .child(
                    div()
                        .ml_auto()
                        .child(Label::new("▾").size(LabelSize::Small).color(Color::Muted)),
                ),
        )
    }

    fn render_picker_rows(
        &self,
        id: &'static str,
        projects: &[ProjectSummary],
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let current = self.project.as_ref().map(|(project, _)| project.summary.id.clone());
        projects
            .iter()
            .enumerate()
            .map(|(ix, project)| {
                let project_id = project.id.clone();
                let note = if project.mine {
                    if project.lead_is_me { "lead" } else { "member" }.to_string()
                } else {
                    project.team.clone().unwrap_or_default()
                };
                h_flex()
                    .id((id, ix))
                    .mx_1p5()
                    .px_2()
                    .py_1p5()
                    .gap_2()
                    .rounded_sm()
                    .cursor_pointer()
                    .when(current.as_ref() == Some(&project.id), |this| {
                        this.bg(cx.theme().colors().element_selected)
                    })
                    .hover(|style| style.bg(cx.theme().colors().element_hover))
                    .on_click(cx.listener(move |panel, _, window, cx| {
                        panel.select_project(project_id.clone(), true, cx);
                        window.focus(&panel.focus_handle, cx);
                    }))
                    .child(Label::new(format!("▣ {}", project.name)).size(LabelSize::Small))
                    .child(
                        div()
                            .ml_auto()
                            .child(Label::new(note).size(LabelSize::XSmall).color(Color::Muted)),
                    )
                    .into_any_element()
            })
            .collect()
    }

    fn render_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list: Vec<AnyElement> = Vec::new();
        match &self.search_results {
            Some(results) => {
                let (mine, others): (Vec<_>, Vec<_>) =
                    results.iter().cloned().partition(|project| project.mine);
                if results.is_empty() {
                    list.push(section("No project matches").into_any_element());
                }
                if !mine.is_empty() {
                    list.push(section("My projects").into_any_element());
                    list.extend(self.render_picker_rows("work-panel-picker-mine", &mine, cx));
                }
                if !others.is_empty() {
                    list.push(section("Other projects in Linear").into_any_element());
                    list.extend(self.render_picker_rows("work-panel-picker-other", &others, cx));
                }
            }
            None => {
                list.push(section("My projects").into_any_element());
                match &self.my_projects {
                    Some(projects) => {
                        list.extend(self.render_picker_rows("work-panel-picker-mine", projects, cx))
                    }
                    None => list.push(
                        div()
                            .px_3()
                            .child(Label::new("Loading…").size(LabelSize::XSmall).color(Color::Muted))
                            .into_any_element(),
                    ),
                }
            }
        }
        deferred(
            v_flex()
                .id("work-panel-picker")
                .occlude()
                .absolute()
                .top(px(92.))
                .left_2p5()
                .right_2p5()
                .max_h(px(420.))
                .overflow_y_scroll()
                .pb_1p5()
                .elevation_3(cx)
                .child(div().p_2().child(self.input_box(&self.search, cx)))
                .children(list)
                .child(
                    div().px_3().pt_1p5().child(
                        Label::new("My projects come first. Typing searches all projects of the workspace.")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
                ),
        )
        .with_priority(1)
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let hint = |text: &'static str| Label::new(text).size(LabelSize::XSmall).color(Color::Muted);
        h_flex()
            .px_3()
            .py_2()
            .gap_2p5()
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .child(hint("↵ Open or create worktree"))
            .child(hint("Space preview"))
            .child(hint("⌘K actions"))
    }
}

fn worktree_label(row: &IssueRow) -> &'static str {
    if row.worktree.is_some() {
        "Open worktree"
    } else {
        "Create worktree"
    }
}

/// The window that shows `workspace`.
fn window_of(
    workspace: &Entity<Workspace>,
    cx: &App,
) -> Option<gpui::WindowHandle<workspace::MultiWorkspace>> {
    cx.windows().into_iter().find_map(|window| {
        let multi = window.downcast::<workspace::MultiWorkspace>()?;
        let shows = multi
            .read(cx)
            .ok()?
            .workspaces()
            .any(|shown| shown == workspace);
        shows.then_some(multi)
    })
}

fn matches_filter(row: &IssueRow, filter: &str) -> bool {
    row.issue.identifier.to_lowercase().contains(filter)
        || row.issue.title.to_lowercase().contains(filter)
        || row
            .issue
            .project
            .as_ref()
            .is_some_and(|project| project.to_lowercase().contains(filter))
}

impl Render for WorkPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.tab {
            Tab::UpNext => self.render_up_next(cx),
            Tab::Project => self.render_project_rows(cx),
            Tab::Reviews => self.render_reviews(cx),
        };
        let top = match self.tab {
            Tab::UpNext => div()
                .p_2p5()
                .child(self.input_box(&self.filter, cx))
                .into_any_element(),
            Tab::Project => self.render_picker_button(cx).into_any_element(),
            Tab::Reviews => div().into_any_element(),
        };
        v_flex()
            .key_context("WorkPanel")
            .relative()
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(self.render_header(cx))
            .child(top)
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .px_3()
                        .py_1()
                        .child(Label::new(error).size(LabelSize::Small).color(Color::Error)),
                )
            })
            .child(
                v_flex()
                    .id("work-panel-list")
                    .key_context(LIST_CONTEXT)
                    .track_focus(&self.focus_handle)
                    .on_action(cx.listener(|panel, _: &menu::SelectNext, _, cx| panel.select_offset(1, cx)))
                    .on_action(cx.listener(|panel, _: &menu::SelectPrevious, _, cx| {
                        panel.select_offset(-1, cx)
                    }))
                    .on_action(cx.listener(Self::start_work))
                    .on_action(cx.listener(Self::open_preview))
                    .on_action(cx.listener(Self::toggle_actions))
                    .on_action(cx.listener(Self::open_in_linear))
                    .on_action(cx.listener(Self::ask_claude))
                    .on_action(cx.listener(|panel, _: &CopyBranch, _, cx| {
                        let text = panel.selected_row(cx).and_then(|row| row.issue.branch_name);
                        panel.copy(text, cx);
                    }))
                    .on_action(cx.listener(|panel, _: &CopyKeyAndTitle, _, cx| {
                        let text = panel
                            .selected_row(cx)
                            .map(|row| format!("{} {}", row.issue.identifier, row.issue.title));
                        panel.copy(text, cx);
                    }))
                    .on_action(cx.listener(|panel, _: &CopyUrl, _, cx| {
                        let text = panel.selected_row(cx).map(|row| row.issue.url);
                        panel.copy(text, cx);
                    }))
                    .flex_1()
                    .overflow_y_scroll()
                    .pb_2()
                    .children(body),
            )
            .child(self.render_footer(cx))
            .when(self.tab == Tab::Project && self.picker_open, |this| {
                this.child(self.render_picker(cx))
            })
            .children(self.actions_menu.as_ref().map(|actions| {
                deferred(
                    anchored()
                        .position(actions.position)
                        .anchor(gpui::Anchor::TopLeft)
                        .child(actions.menu.clone()),
                )
                .with_priority(2)
            }))
    }
}

impl Focusable for WorkPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for WorkPanel {}

impl Panel for WorkPanel {
    fn persistent_name() -> &'static str {
        "WorkPanel"
    }

    fn panel_key() -> &'static str {
        "WorkPanel"
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(360.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::GitBranchPlus)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Work")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        21
    }
}

/// "Linear: synced 1m ago" on the right of the status bar. A click asks
/// Linear again.
pub struct LinearStatus {
    panel: Entity<WorkPanel>,
    _observe: Subscription,
}

impl LinearStatus {
    pub fn new(panel: Entity<WorkPanel>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&panel, |_, _, cx| cx.notify());
        Self {
            panel,
            _observe: observe,
        }
    }
}

impl Render for LinearStatus {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let text = self.panel.read(cx).sync_text();
        let panel = self.panel.clone();
        Button::new("work-panel-linear-status", text)
            .label_size(LabelSize::Small)
            .color(Color::Muted)
            .style(ButtonStyle::Transparent)
            .tooltip(Tooltip::text("Ask Linear again"))
            .on_click(move |_, _, cx| {
                panel.update(cx, |panel, cx| panel.refresh(cx).detach());
            })
    }
}

impl StatusItemView for LinearStatus {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn workspace::ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        None
    }
}
