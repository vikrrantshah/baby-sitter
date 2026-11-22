use crate::{
    gh::{self, ChildEnv},
    model::{
        self, Config, Outcome, PrRef, PrRow, RepoListItem, RepoName, RepoScope, RepoSettings,
        STEPS, SitState, VerdictKind,
    },
    sitter::{self, Registry, RepoLocks, SitEvent, SitHandle},
    store,
    theme::{coral, dusk, glow, housing, mint, moon, night, rail},
};
use gpui_kit::{component::StyledExt, prelude::FluentBuilder};
use gpui_kit::{
    component::{
        Disableable, Selectable, Sizable,
        button::{Button, ButtonCustomVariant, ButtonGroup, ButtonVariants},
        input::{Input, InputEvent, InputState},
        select::{Select, SelectEvent, SelectState},
        switch::Switch,
        tooltip::Tooltip,
    },
    *,
};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap},
    path::PathBuf,
    time::Duration,
};

enum Load<T> {
    Loading,
    Ready(T),
    Error(String),
}
#[derive(Clone, Eq, PartialEq)]
enum Page {
    PullRequests,
    Repositories,
    RepoSettings(RepoName),
}
#[derive(Clone, Copy)]
enum Field {
    Timeout,
    Interval,
}
struct RepoInputs {
    timeout: Entity<InputState>,
    interval: Entity<InputState>,
    timeout_error: Option<String>,
    interval_error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

/// How a pull request row reads: the meter level and the status sentence.
struct Reading {
    /// Lit meter segments, counted from the bottom.
    level: usize,
    tip: Hsla,
    breathe: bool,
    merged: bool,
    verb: Option<&'static str>,
    detail: Option<String>,
}

const SIDEBAR_WIDTH: f32 = 232.;
const ROW_INSET: f32 = 32.;
const REPO_ROW_HEIGHT: f32 = 56.;

pub struct AppView {
    data: PathBuf,
    config: Config,
    env: ChildEnv,
    registry: Registry,
    locks: RepoLocks,
    page: Page,
    repos: Load<Vec<RepoName>>,
    prs: Load<Vec<PrRow>>,
    repo_filter: Entity<InputState>,
    org_select: Entity<SelectState<Vec<String>>>,
    org_filter: Option<String>,
    scope: RepoScope,
    dashboard_filter: Entity<SelectState<Vec<String>>>,
    selected_filter: Option<RepoName>,
    inputs: BTreeMap<RepoName, RepoInputs>,
    states: BTreeMap<PrRef, SitState>,
    /// The checklist step each babysit's latest pass reached.
    progress: BTreeMap<PrRef, usize>,
    handles: HashMap<PrRef, SitHandle>,
    event_tx: async_channel::Sender<SitEvent>,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
    _events: Task<()>,
    _clock: Task<()>,
    repo_task: Option<Task<()>>,
    pr_task: Option<Task<()>>,
}

impl AppView {
    pub fn new(
        data: PathBuf,
        mut config: Config,
        env: ChildEnv,
        registry: Registry,
        locks: RepoLocks,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let before = config.active.len();
        config
            .active
            .retain(|pr, _| config.repos.contains_key(&pr.repo));
        let error = if before != config.active.len() {
            store::save(&data, &config)
                .err()
                .map(|e| format!("Could not save configuration: {e:#}"))
        } else {
            None
        };
        let repo_filter =
            cx.new(|cx| InputState::new(window, cx).placeholder("Filter repositories"));
        let dashboard_filter =
            cx.new(|cx| SelectState::new(vec!["All repositories".to_string()], None, window, cx));
        let org_select =
            cx.new(|cx| SelectState::new(vec!["All organizations".to_string()], None, window, cx));
        let subscriptions = vec![
            cx.subscribe(&repo_filter, |_: &mut Self, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.subscribe(
                &org_select,
                |this: &mut Self, _, event: &SelectEvent<Vec<String>>, cx| {
                    let SelectEvent::Confirm(value) = event;
                    this.org_filter = value
                        .as_ref()
                        .filter(|name| name.as_str() != "All organizations")
                        .cloned();
                    cx.notify();
                },
            ),
            cx.subscribe(
                &dashboard_filter,
                |this: &mut Self, _, event: &SelectEvent<Vec<String>>, cx| {
                    let SelectEvent::Confirm(value) = event;
                    this.selected_filter = value.as_ref().and_then(|s| s.parse().ok());
                    cx.notify();
                },
            ),
        ];
        let (event_tx, event_rx) = async_channel::unbounded::<SitEvent>();
        let events = cx.spawn(async move |view, cx| {
            while let Ok(event) = event_rx.recv().await {
                if view
                    .update(cx, |this, cx| {
                        if matches!(this.states.get(&event.pr), Some(SitState::Cancelling))
                            && !matches!(event.state, SitState::Finished(_))
                        {
                            return;
                        }
                        if matches!(event.state, SitState::Finished(_)) {
                            this.handles.remove(&event.pr);
                            if this.config.active.remove(&event.pr).is_some() {
                                this.save();
                            }
                        }
                        if let SitState::Running {
                            last_line: Some(line),
                        } = &event.state
                            && let Some(step) = model::parse_pass(line).and_then(|pass| pass.step)
                        {
                            this.progress.insert(event.pr.clone(), step);
                        }
                        this.states.insert(event.pr, event.state);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        // Keeps each babysit's time left current between its own output lines.
        let clock = cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(30))
                    .await;
                if view.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });
        let mut this = Self {
            data,
            config,
            env,
            registry,
            locks,
            page: Page::PullRequests,
            repos: Load::Loading,
            prs: Load::Loading,
            repo_filter,
            org_select,
            org_filter: None,
            scope: RepoScope::All,
            dashboard_filter,
            selected_filter: None,
            inputs: BTreeMap::new(),
            states: BTreeMap::new(),
            progress: BTreeMap::new(),
            handles: HashMap::new(),
            event_tx,
            error,
            _subscriptions: subscriptions,
            _events: events,
            _clock: clock,
            repo_task: None,
            pr_task: None,
        };
        for (repo, settings) in this.config.repos.clone() {
            this.add_inputs(repo, &settings, window, cx);
        }
        for (pr, deadline) in this.config.active.clone() {
            this.resume(pr, deadline);
        }
        this.refresh(window, cx);
        this
    }
    fn save(&mut self) -> bool {
        match store::save(&self.data, &self.config) {
            Ok(()) => {
                self.error = None;
                true
            }
            Err(error) => {
                self.error = Some(format!("Could not save configuration: {error:#}"));
                false
            }
        }
    }
    fn add_inputs(
        &mut self,
        repo: RepoName,
        settings: &RepoSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let timeout = cx.new(|cx| {
            InputState::new(window, cx).default_value(model::format_duration(settings.timeout))
        });
        let interval = cx.new(|cx| {
            InputState::new(window, cx).default_value(model::format_duration(settings.interval))
        });
        let mut subscriptions = Vec::new();
        for (input, field) in [(&timeout, Field::Timeout), (&interval, Field::Interval)] {
            let repo = repo.clone();
            subscriptions.push(cx.subscribe_in(
                input,
                window,
                move |this: &mut Self, input, event: &InputEvent, _window, cx| {
                    if matches!(event, InputEvent::Blur | InputEvent::PressEnter { .. }) {
                        let value = input.read(cx).value().to_string();
                        this.update_duration(&repo, field, &value);
                        cx.notify();
                    }
                },
            ));
        }
        self.inputs.insert(
            repo,
            RepoInputs {
                timeout,
                interval,
                timeout_error: None,
                interval_error: None,
                _subscriptions: subscriptions,
            },
        );
    }
    fn update_duration(&mut self, repo: &RepoName, field: Field, text: &str) {
        let parsed = model::parse_duration(text).and_then(|duration| {
            if matches!(field, Field::Interval) && duration.is_zero() {
                Err("Interval must be greater than 0".into())
            } else {
                Ok(duration)
            }
        });
        match parsed {
            Ok(duration) => {
                let previous = self.config.repos.get_mut(repo).and_then(|settings| {
                    let value = match field {
                        Field::Timeout => &mut settings.timeout,
                        Field::Interval => &mut settings.interval,
                    };
                    if *value == duration {
                        None
                    } else {
                        Some(std::mem::replace(value, duration))
                    }
                });
                if let Some(previous) = previous
                    && !self.save()
                    && let Some(settings) = self.config.repos.get_mut(repo)
                {
                    match field {
                        Field::Timeout => settings.timeout = previous,
                        Field::Interval => settings.interval = previous,
                    }
                }
                if let Some(inputs) = self.inputs.get_mut(repo) {
                    match field {
                        Field::Timeout => inputs.timeout_error = None,
                        Field::Interval => inputs.interval_error = None,
                    }
                }
            }
            Err(error) => {
                if let Some(inputs) = self.inputs.get_mut(repo) {
                    match field {
                        Field::Timeout => inputs.timeout_error = Some(error),
                        Field::Interval => inputs.interval_error = Some(error),
                    }
                }
            }
        }
    }
    fn toggle_repo(
        &mut self,
        repo: RepoName,
        checked: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active_count(&repo) > 0 {
            return;
        }
        let previous = if checked {
            self.config
                .repos
                .insert(repo.clone(), RepoSettings::default())
        } else {
            self.config.repos.remove(&repo)
        };
        if !self.save() {
            match previous {
                Some(settings) => {
                    self.config.repos.insert(repo, settings);
                }
                None => {
                    self.config.repos.remove(&repo);
                }
            }
            return;
        }
        if checked {
            self.add_inputs(repo, &RepoSettings::default(), window, cx);
        } else {
            self.inputs.remove(&repo);
        }
        self.update_filter(window, cx);
        cx.notify();
    }
    fn active_count(&self, repo: &RepoName) -> usize {
        self.states
            .iter()
            .filter(|(pr, state)| &pr.repo == repo && state.is_active())
            .count()
    }
    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.load_repos(window, cx);
        self.load_prs(window, cx);
    }
    fn load_repos(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.repos = Load::Loading;
        let env = self.env.clone();
        self.repo_task = Some(cx.spawn_in(window, async move |view, cx| {
            let result = cx
                .background_spawn(async move { gh::list_repos(&env).map_err(|e| format!("{e:#}")) })
                .await;
            let _ = view.update_in(cx, |this, window, cx| {
                this.repos = match result {
                    Ok(repos) => {
                        let mut owners: Vec<_> = repos
                            .iter()
                            .map(|repo| repo.owner.as_str())
                            .collect::<BTreeSet<_>>()
                            .into_iter()
                            .collect();
                        owners.sort_by_cached_key(|owner| owner.to_lowercase());
                        if this
                            .org_filter
                            .as_deref()
                            .is_some_and(|selected| !owners.contains(&selected))
                        {
                            this.org_filter = None;
                        }
                        let mut choices = vec!["All organizations".to_owned()];
                        choices.extend(owners.into_iter().map(str::to_owned));
                        let selected = this
                            .org_filter
                            .clone()
                            .unwrap_or_else(|| "All organizations".into());
                        this.org_select.update(cx, |state, cx| {
                            state.set_items(choices, window, cx);
                            state.set_selected_value(&selected, window, cx);
                            cx.notify();
                        });
                        Load::Ready(repos)
                    }
                    Err(error) => Load::Error(error),
                };
                cx.notify();
            });
        }));
    }
    fn load_prs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.prs = Load::Loading;
        let env = self.env.clone();
        self.pr_task = Some(cx.spawn_in(window, async move |view, cx| {
            let result = cx
                .background_spawn(
                    async move { gh::list_my_prs(&env).map_err(|e| format!("{e:#}")) },
                )
                .await;
            let _ = view.update_in(cx, |this, window, cx| {
                this.prs = match result {
                    Ok(prs) => Load::Ready(prs),
                    Err(error) => Load::Error(error),
                };
                this.update_filter(window, cx);
                cx.notify();
            });
        }));
    }
    fn update_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prs = match &self.prs {
            Load::Ready(prs) => prs.as_slice(),
            _ => &[],
        };
        let repos: BTreeSet<_> =
            model::dashboard_rows(prs, self.states.keys(), &self.config.repos, None)
                .into_iter()
                .map(|row| row.pr.repo.clone())
                .collect();
        if self
            .selected_filter
            .as_ref()
            .is_some_and(|repo| !repos.contains(repo))
        {
            self.selected_filter = None;
        }
        let mut choices = vec!["All repositories".to_owned()];
        choices.extend(repos.into_iter().map(|repo| repo.to_string()));
        let selected = self
            .selected_filter
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "All repositories".into());
        self.dashboard_filter.update(cx, |state, cx| {
            state.set_items(choices, window, cx);
            state.set_selected_value(&selected, window, cx);
            cx.notify();
        });
    }
    fn resume(&mut self, pr: PrRef, deadline: u64) {
        if self.handles.contains_key(&pr) {
            return;
        }
        let Some(settings) = self.config.repos.get(&pr.repo).cloned() else {
            return;
        };
        self.states.insert(pr.clone(), SitState::Preparing);
        self.progress.remove(&pr);
        let handle = sitter::spawn(
            pr.clone(),
            settings,
            deadline,
            self.data.clone(),
            self.env.clone(),
            self.registry.clone(),
            self.locks.clone(),
            self.event_tx.clone(),
        );
        self.handles.insert(pr, handle);
    }
    fn start(&mut self, pr: PrRef, window: &mut Window, cx: &mut Context<Self>) {
        if self.states.get(&pr).is_some_and(SitState::is_active) {
            return;
        }
        let Some(settings) = self.config.repos.get(&pr.repo) else {
            return;
        };
        let Some(deadline) = model::unix_now().checked_add(settings.timeout.as_secs()) else {
            self.error = Some("Timeout is too large".into());
            cx.notify();
            return;
        };
        self.config.active.insert(pr.clone(), deadline);
        if !self.save() {
            self.config.active.remove(&pr);
            cx.notify();
            return;
        }
        self.resume(pr, deadline);
        self.update_filter(window, cx);
        cx.notify();
    }
    fn cancel(&mut self, pr: PrRef, cx: &mut Context<Self>) {
        let Some(handle) = self.handles.get(&pr).cloned() else {
            return;
        };
        let old = self.config.active.remove(&pr);
        if !self.save() {
            if let Some(deadline) = old {
                self.config.active.insert(pr, deadline);
            }
            cx.notify();
            return;
        }
        self.states.insert(pr, SitState::Cancelling);
        handle.cancel();
        cx.notify();
    }
    fn ask_cancel(&mut self, pr: PrRef, window: &mut Window, cx: &mut Context<Self>) {
        let acting = matches!(
            self.states.get(&pr),
            Some(SitState::Running { last_line: Some(line) })
                if model::parse_pass(line).is_some_and(|pass| pass.kind == VerdictKind::Act)
        );
        if !acting {
            self.cancel(pr, cx);
            return;
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            "An action may be half applied if you cancel.",
            Some("Inspect the worktree before restarting."),
            &["Cancel babysit", "Keep running"],
            cx,
        );
        cx.spawn(async move |view, cx| {
            if answer.await.ok() == Some(0) {
                let _ = view.update(cx, |this, cx| this.cancel(pr, cx));
            }
        })
        .detach();
    }
    fn reading(&self, pr: &PrRef) -> Reading {
        let state = self.states.get(pr);
        let reached = self.progress.get(pr).map_or(0, |step| step + 1);
        let reading = |tip, breathe, verb, detail: Option<&str>| Reading {
            level: reached.max(1),
            tip,
            breathe,
            merged: false,
            verb: Some(verb),
            detail: detail.map(str::to_owned),
        };
        let merged = Reading {
            level: STEPS.len(),
            tip: mint(),
            breathe: false,
            merged: true,
            verb: Some("Merged"),
            detail: None,
        };
        match state {
            None => Reading {
                level: 0,
                tip: rail(),
                breathe: false,
                merged: false,
                verb: None,
                detail: None,
            },
            Some(SitState::Preparing) => {
                reading(moon(), true, "Preparing", Some("Setting up the worktree"))
            }
            Some(SitState::Running { last_line: None }) => {
                reading(moon(), true, "Starting", Some("Taking the first snapshot"))
            }
            Some(SitState::Running {
                last_line: Some(line),
            }) => match model::parse_pass(line) {
                Some(pass) => match pass.kind {
                    VerdictKind::Done => merged,
                    VerdictKind::Wait => reading(glow(), true, "Waiting", Some(pass.detail)),
                    VerdictKind::Act => reading(moon(), true, "Working", Some(pass.detail)),
                    VerdictKind::Stop => reading(coral(), false, "Stopping", Some(pass.detail)),
                },
                None => reading(
                    moon(),
                    true,
                    "Running",
                    Some(line.strip_prefix("baby-sitter: ").unwrap_or(line)),
                ),
            },
            Some(SitState::Cancelling) => reading(
                dusk(),
                false,
                "Cancelling",
                Some("Waiting for the process to exit"),
            ),
            Some(SitState::Finished(Outcome::Merged)) => merged,
            Some(SitState::Finished(Outcome::TimedOut)) => reading(
                glow(),
                false,
                "Timed out",
                Some("The deadline passed before the merge"),
            ),
            Some(SitState::Finished(Outcome::Stopped(reason))) => {
                reading(coral(), false, "Stopped", Some(reason))
            }
            Some(SitState::Finished(Outcome::Cancelled)) => {
                reading(dusk(), false, "Cancelled", None)
            }
            Some(SitState::Finished(Outcome::Failed(reason))) => {
                reading(coral(), false, "Failed", Some(reason))
            }
        }
    }
    fn needs_you(&self) -> usize {
        self.states
            .values()
            .filter(|state| {
                matches!(
                    state,
                    SitState::Finished(
                        Outcome::Stopped(_) | Outcome::Failed(_) | Outcome::TimedOut
                    )
                )
            })
            .count()
    }
    fn repo_row(&self, repo: &RepoName, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.config.repos.contains_key(repo);
        let count = self.active_count(repo);
        let switch_repo = repo.clone();
        let settings_repo = repo.clone();
        let switch = Switch::new(SharedString::from(format!("repo-{repo}")))
            .accessibility_label(format!("Allow {repo}"))
            .checked(selected)
            .disabled(count > 0)
            .on_click(cx.listener(move |this, checked, window, cx| {
                this.toggle_repo(switch_repo.clone(), *checked, window, cx)
            }));
        let settings = Button::new(SharedString::from(format!("settings-{repo}")))
            .label("Settings")
            .accessibility_label(format!("Settings for {repo}"))
            .ghost()
            .small()
            .disabled(!selected)
            .when(!selected, |button| button.tooltip("Turn on to configure"))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.page = Page::RepoSettings(settings_repo.clone());
                cx.notify();
            }));
        div()
            .h(px(REPO_ROW_HEIGHT))
            .w_full()
            .px(px(ROW_INSET))
            .border_b_1()
            .border_color(rail())
            .flex()
            .items_center()
            .gap(px(20.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_ellipsis()
                    .text_color(moon())
                    .child(repo.name.clone()),
            )
            .when(count > 0, |row| {
                row.child(
                    div()
                        .flex_shrink_0()
                        .px(px(8.))
                        .py(px(3.))
                        .rounded(px(12.))
                        .bg(glow().opacity(0.12))
                        .text_size(px(12.))
                        .text_color(glow())
                        .child(if count == 1 {
                            "1 babysit running".to_owned()
                        } else {
                            format!("{count} babysits running")
                        }),
                )
            })
            .child(switch)
            .child(settings)
            .into_any_element()
    }
    fn render_repos(&self, cx: &mut Context<Self>) -> (Div, Div) {
        let list = match &self.repos {
            Load::Loading => message("Loading repositories…", None),
            Load::Error(error) => {
                let retry = Button::new("retry-repos")
                    .label("Retry")
                    .outline()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.load_repos(window, cx);
                        cx.notify();
                    }));
                message("Could not load repositories", Some(error.clone())).child(retry)
            }
            Load::Ready(repos) if repos.is_empty() => message(
                "No repositories found",
                Some("Your GitHub account has no repositories it can push to.".into()),
            ),
            Load::Ready(repos) => {
                let items = model::repo_list(
                    repos,
                    &self.config.repos,
                    self.org_filter.as_deref(),
                    self.scope,
                    self.repo_filter.read(cx).value().as_ref(),
                );
                if items.is_empty() {
                    if self.scope == RepoScope::Allowed
                        && !repos
                            .iter()
                            .any(|repo| self.config.repos.contains_key(repo))
                    {
                        message(
                            "No repositories allowed",
                            Some(
                                "Turn on a repository under All to babysit its pull requests."
                                    .into(),
                            ),
                        )
                    } else {
                        message(
                            "No repositories match the filters",
                            Some("Clear a filter to see more.".into()),
                        )
                    }
                } else {
                    let count = items.len();
                    div().size_full().child(
                        uniform_list(
                            "repo-list",
                            count,
                            cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                                range
                                    .filter_map(|index| items.get(index))
                                    .map(|item| match item {
                                        RepoListItem::Org {
                                            name,
                                            allowed,
                                            total,
                                        } => div()
                                            .h(px(REPO_ROW_HEIGHT))
                                            .w_full()
                                            .px(px(ROW_INSET))
                                            .bg(housing())
                                            .border_b_1()
                                            .border_color(rail())
                                            .flex()
                                            .items_center()
                                            .gap(px(20.))
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .font_semibold()
                                                    .text_ellipsis()
                                                    .text_color(moon())
                                                    .child(name.clone()),
                                            )
                                            .child(
                                                div()
                                                    .flex_shrink_0()
                                                    .text_size(px(12.))
                                                    .text_color(dusk())
                                                    .child(format!("{allowed} of {total} allowed")),
                                            )
                                            .into_any_element(),
                                        RepoListItem::Repo(repo) => this.repo_row(repo, cx),
                                    })
                                    .collect::<Vec<_>>()
                            }),
                        )
                        .size_full(),
                    )
                }
            }
        };
        let segment = |id: &'static str, label: &'static str, scope: RepoScope| {
            let selected = self.scope == scope;
            Button::new(id).selected(selected).child(
                div()
                    .text_color(if selected { glow() } else { dusk() })
                    .when(selected, |d| d.font_semibold())
                    .child(label),
            )
        };
        let scope = ButtonGroup::new("repo-scope")
            .small()
            .outline()
            .child(segment("scope-all", "All", RepoScope::All))
            .child(segment("scope-allowed", "Allowed", RepoScope::Allowed))
            .on_click(cx.listener(|this, selected: &Vec<usize>, _, cx| {
                if let Some(index) = selected.first() {
                    this.scope = if *index == 0 {
                        RepoScope::All
                    } else {
                        RepoScope::Allowed
                    };
                    cx.notify();
                }
            }));
        let filters = div()
            .flex_shrink_0()
            .px(px(ROW_INSET))
            .py(px(16.))
            .border_b_1()
            .border_color(rail())
            .flex()
            .items_center()
            .gap(px(12.))
            .child(
                div().w(px(190.)).flex_shrink_0().child(
                    Select::new(&self.org_select)
                        .small()
                        .accessibility_label("Filter by organization")
                        .placeholder("All organizations"),
                ),
            )
            .child(scope)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(&self.repo_filter).small()),
            );
        let body = div()
            .size_full()
            .flex()
            .flex_col()
            .child(filters)
            .child(div().flex_1().min_h_0().child(list));
        (
            header(
                "Repositories",
                Some("Babysits run only in the repositories you allow."),
            ),
            body,
        )
    }
    fn render_repo_settings(&self, repo: &RepoName, cx: &mut Context<Self>) -> (Div, Div) {
        let inputs = self
            .inputs
            .get(repo)
            .expect("allowed repositories have duration inputs");
        let field = |label: &'static str, input: &Entity<InputState>, error: &Option<String>| {
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(
                    div()
                        .text_size(px(13.))
                        .font_semibold()
                        .text_color(moon())
                        .child(label),
                )
                .child(Input::new(input))
                .when_some(error.clone(), |field, error| {
                    field.child(div().text_size(px(12.)).text_color(coral()).child(error))
                })
        };
        let header = div()
            .flex_shrink_0()
            .px(px(ROW_INSET))
            .pt(px(28.))
            .pb(px(20.))
            .border_b_1()
            .border_color(rail())
            .flex()
            .flex_col()
            .items_start()
            .gap(px(12.))
            .child(
                Button::new("back-to-repositories")
                    .label("‹ Repositories")
                    .ghost()
                    .small()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.page = Page::Repositories;
                        cx.notify();
                    })),
            )
            .child(
                drag_area()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .text_size(px(24.))
                            .font_semibold()
                            .text_color(moon())
                            .child(repo.to_string()),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(dusk())
                            .child("Applies to babysits you start from now on."),
                    ),
            );
        let body = div().size_full().child(
            div()
                .id("repo-settings-form")
                .size_full()
                .overflow_y_scroll()
                .child(
                    div()
                        .px(px(ROW_INSET))
                        .py(px(28.))
                        .w_full()
                        .max_w(px(420.))
                        .flex()
                        .flex_col()
                        .gap(px(24.))
                        .child(field("Timeout", &inputs.timeout, &inputs.timeout_error))
                        .child(field("Interval", &inputs.interval, &inputs.interval_error))
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(dusk())
                                .child("Use s, m, or h, for example 2h or 30s."),
                        ),
                ),
        );
        (header, body)
    }
    fn dashboard_rows(&self) -> Vec<Cow<'_, PrRow>> {
        let prs: &[PrRow] = match &self.prs {
            Load::Ready(rows) => rows,
            _ => &[],
        };
        model::dashboard_rows(
            prs,
            self.states.keys(),
            &self.config.repos,
            self.selected_filter.as_ref(),
        )
    }
    fn pr_row(&self, row: &PrRow, cx: &mut Context<Self>) -> AnyElement {
        let state = self.states.get(&row.pr);
        let reading = self.reading(&row.pr);
        let active = state.is_some_and(SitState::is_active);
        let pr = row.pr.clone();
        let action = if active {
            let cancelling = matches!(state, Some(SitState::Cancelling));
            Some(
                Button::new(SharedString::from(format!("cancel-{pr}")))
                    .label(if cancelling { "Cancelling" } else { "Cancel" })
                    .outline()
                    .disabled(cancelling)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.ask_cancel(pr.clone(), window, cx)
                    })),
            )
        } else if reading.merged {
            None
        } else {
            Some(
                Button::new(SharedString::from(format!("start-{pr}")))
                    .label("Start")
                    .primary()
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.start(pr.clone(), window, cx)),
                    ),
            )
        };
        let time_left = active
            .then(|| self.config.active.get(&row.pr))
            .flatten()
            .map(|deadline| time_left(deadline.saturating_sub(model::unix_now())));
        let url = row.url.clone();
        let title = div()
            .id(SharedString::from(format!("title-{}", row.pr)))
            .cursor_pointer()
            .text_size(px(16.))
            .line_height(relative(1.3))
            .font_semibold()
            .text_color(moon())
            .hover(|style| style.text_color(glow()))
            .child(row.title.clone())
            .on_click(move |_, _, _| {
                let url = url.clone();
                std::thread::spawn(move || {
                    let _ = std::process::Command::new("open").arg(url).status();
                });
            });
        let place = if row.is_draft {
            format!("{} #{}, draft", row.pr.repo, row.pr.number)
        } else {
            format!("{} #{}", row.pr.repo, row.pr.number)
        };
        let status = reading.verb.map(|verb| {
            div()
                .flex()
                .gap(px(8.))
                .text_size(px(13.))
                .line_height(relative(1.45))
                .child(
                    div()
                        .flex_shrink_0()
                        .font_medium()
                        .text_color(if reading.merged { mint() } else { reading.tip })
                        .child(verb),
                )
                .when_some(reading.detail.clone(), |line, detail| {
                    line.child(div().min_w_0().text_color(moon()).child(detail))
                })
        });
        let path = sitter::worktree_path(&self.data, &row.pr);
        let worktree = (!active && path.exists()).then(|| {
            let shown = path.display().to_string();
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .text_size(px(12.))
                .text_color(dusk())
                .child(div().min_w_0().text_ellipsis().child(shown))
                .child(
                    Button::new(SharedString::from(format!("reveal-{}", row.pr)))
                        .label("Reveal in Finder")
                        .ghost()
                        .xsmall()
                        .on_click(move |_, _, _| {
                            let path = path.clone();
                            std::thread::spawn(move || {
                                let _ = std::process::Command::new("open")
                                    .arg("-R")
                                    .arg(path)
                                    .status();
                            });
                        }),
                )
        });
        let content = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(title)
            .child(div().text_size(px(13.)).text_color(dusk()).child(place))
            .when_some(status, |d, status| d.child(div().pt(px(6.)).child(status)))
            .when_some(worktree, |d, worktree| d.child(worktree));
        div()
            .px(px(ROW_INSET))
            .flex_shrink_0()
            .py(px(18.))
            .border_b_1()
            .border_color(rail())
            .flex()
            .items_start()
            .gap(px(20.))
            .child(meter(&row.pr, &reading))
            .child(content)
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(16.))
                    .when_some(time_left, |d, left| {
                        d.child(div().text_size(px(12.)).text_color(dusk()).child(left))
                    })
                    .when_some(action, |d, action| d.child(action)),
            )
            .into_any_element()
    }
    fn retry_prs_button(cx: &mut Context<Self>) -> Button {
        Button::new("retry-prs")
            .label("Retry")
            .outline()
            .on_click(cx.listener(|this, _, window, cx| {
                this.load_prs(window, cx);
                cx.notify();
            }))
    }
    fn render_dashboard(&self, cx: &mut Context<Self>) -> (Div, Div) {
        let rows = self.dashboard_rows();
        let list = match &self.prs {
            Load::Loading if rows.is_empty() => message("Loading pull requests…", None),
            Load::Error(error) if rows.is_empty() => {
                message("Could not load pull requests", Some(error.clone()))
                    .child(Self::retry_prs_button(cx))
            }
            _ if rows.is_empty() => {
                if self.config.repos.is_empty() {
                    message(
                        "No repositories allowed",
                        Some("Allow a repository to see its pull requests.".into()),
                    )
                    .child(
                        Button::new("choose-repositories")
                            .label("Choose repositories")
                            .primary()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.page = Page::Repositories;
                                cx.notify();
                            })),
                    )
                } else if self.selected_filter.is_some() {
                    message(
                        "No pull requests in this repository",
                        Some("Choose All repositories to see the rest.".into()),
                    )
                } else {
                    message(
                        "No open pull requests",
                        Some(
                            "None of your allowed repositories has an open pull request from you."
                                .into(),
                        ),
                    )
                }
            }
            _ => {
                let mut body = div().size_full().flex().flex_col();
                match &self.prs {
                    Load::Loading => body = body.child(message("Loading pull requests…", None)),
                    Load::Error(error) => {
                        body = body.child(
                            message("Could not load pull requests", Some(error.clone()))
                                .child(Self::retry_prs_button(cx)),
                        );
                    }
                    Load::Ready(_) => {}
                }
                body.child(
                    div()
                        .id("pr-list")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .children(rows.iter().map(|row| self.pr_row(row, cx))),
                )
            }
        };
        let header = header("Pull requests", None).child(
            div()
                .w(px(240.))
                .child(Select::new(&self.dashboard_filter).placeholder("All repositories")),
        );
        (header, list)
    }
    fn render_sidebar(&self, cx: &mut Context<Self>) -> Div {
        let running = self.states.values().filter(|s| s.is_active()).count();
        let needs_you = self.needs_you();
        let watching = match running {
            0 => "No babysits running.".to_owned(),
            1 => "Babysitting 1 pull request.".to_owned(),
            n => format!("Babysitting {n} pull requests."),
        };
        let nav = |page: Page, id: &'static str, label: &'static str, cx: &mut Context<Self>| {
            let selected = matches!(
                (&self.page, &page),
                (Page::PullRequests, Page::PullRequests)
                    | (
                        Page::Repositories | Page::RepoSettings(_),
                        Page::Repositories
                    )
            );
            Button::new(id)
                .custom(
                    ButtonCustomVariant::new(cx)
                        .foreground(if selected { moon() } else { dusk() })
                        .hover(rgb(0x222440).into())
                        .active(rail()),
                )
                .w_full()
                .h(px(34.))
                .accessibility_label(label)
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .gap(px(12.))
                        .px(px(4.))
                        .text_size(px(14.))
                        .when(selected, |d| d.font_semibold())
                        .child(led(if selected { glow() } else { rail() }))
                        .child(label),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.page = page.clone();
                    cx.notify();
                }))
        };
        div()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(housing())
            .border_r_1()
            .border_color(rail())
            .child(drag_area().h(px(52.)))
            .child(
                div()
                    .px(px(20.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(moon_mark())
                    .child(
                        div()
                            .text_size(px(18.))
                            .font_semibold()
                            .text_color(moon())
                            .child("baby-sitter"),
                    ),
            )
            .child(
                div()
                    .px(px(20.))
                    .pt(px(20.))
                    .text_size(px(14.))
                    .line_height(relative(1.5))
                    .text_color(dusk())
                    .child(watching)
                    .when(needs_you > 0, |d| {
                        d.child(div().text_color(coral()).child(if needs_you == 1 {
                            "1 needs you.".to_owned()
                        } else {
                            format!("{needs_you} need you.")
                        }))
                    }),
            )
            .child(
                div()
                    .px(px(12.))
                    .pt(px(28.))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(nav(Page::PullRequests, "nav-prs", "Pull requests", cx))
                    .child(nav(Page::Repositories, "nav-repos", "Repositories", cx)),
            )
            .child(div().flex_1())
            .child(
                div().p(px(20.)).child(
                    Button::new("refresh")
                        .label("Refresh")
                        .outline()
                        .w_full()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.refresh(window, cx);
                            cx.notify();
                        })),
                ),
            )
    }
}

/// The page title row. Its empty space drags the window, since the title bar is hidden.
fn header(title: &'static str, detail: Option<&'static str>) -> Div {
    div()
        .flex_shrink_0()
        .px(px(ROW_INSET))
        .pt(px(36.))
        .pb(px(20.))
        .flex()
        .items_end()
        .gap(px(16.))
        .border_b_1()
        .border_color(rail())
        .child(
            drag_area()
                .flex_1()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(
                    div()
                        .text_size(px(24.))
                        .font_semibold()
                        .text_color(moon())
                        .child(title),
                )
                .when_some(detail, |d, detail| {
                    d.child(div().text_size(px(13.)).text_color(dusk()).child(detail))
                }),
        )
}
fn drag_area() -> Div {
    div().on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
}
fn time_left(seconds: u64) -> String {
    let minutes = seconds / 60;
    match (minutes / 60, minutes % 60) {
        (0, 0) => "Under a minute left".to_owned(),
        (0, m) => format!("{m}m left"),
        (h, 0) => format!("{h}h left"),
        (h, m) => format!("{h}h {m}m left"),
    }
}
fn led(color: Hsla) -> Div {
    div().size(px(7.)).rounded_full().bg(color)
}
/// A crescent: a lit disc with a housing-colored disc over its upper right.
fn moon_mark() -> Div {
    div()
        .relative()
        .size(px(18.))
        .child(div().absolute().size_full().rounded_full().bg(glow()))
        .child(
            div()
                .absolute()
                .top(px(-4.))
                .left(px(6.))
                .size(px(16.))
                .rounded_full()
                .bg(housing()),
        )
}
/// A baby monitor's level meter: one segment per checklist step, lit up to the
/// step the latest pass reached, read from the bottom.
fn meter(pr: &PrRef, reading: &Reading) -> AnyElement {
    let segments = (0..STEPS.len()).rev().map(|index| {
        let segment = div().w(px(20.)).h(px(5.)).rounded(px(1.));
        let position = index + 1;
        if reading.merged {
            segment.bg(mint()).into_any_element()
        } else if position < reading.level {
            segment.bg(glow()).into_any_element()
        } else if position == reading.level && reading.breathe {
            segment
                .bg(reading.tip)
                .with_animation(
                    SharedString::from(format!("breathe-{pr}")),
                    Animation::new(Duration::from_millis(2400))
                        .repeat()
                        .with_easing(pulsating_between(0.3, 1.0)),
                    |segment, delta| segment.opacity(delta),
                )
                .into_any_element()
        } else if position == reading.level {
            segment.bg(reading.tip).into_any_element()
        } else {
            segment.bg(rail()).into_any_element()
        }
    });
    let tooltip: Option<SharedString> = if reading.merged {
        Some("Merged".into())
    } else {
        reading.level.checked_sub(1).map(|index| {
            format!(
                "Step {} of {}: {}",
                reading.level,
                STEPS.len(),
                STEPS[index]
            )
            .into()
        })
    };
    div()
        .id(SharedString::from(format!("meter-{pr}")))
        .flex_shrink_0()
        .pt(px(3.))
        .flex()
        .flex_col()
        .gap(px(3.))
        .children(segments)
        .when_some(tooltip, |d, text| {
            d.tooltip(move |window, cx| Tooltip::new(text.clone()).build(window, cx))
        })
        .into_any_element()
}
fn message(title: &'static str, detail: Option<String>) -> Div {
    div()
        .px(px(ROW_INSET))
        .py(px(28.))
        .flex()
        .flex_col()
        .items_start()
        .gap(px(8.))
        .max_w(px(620.))
        .text_size(px(14.))
        .text_color(dusk())
        .child(
            div()
                .text_size(px(16.))
                .font_semibold()
                .text_color(moon())
                .child(title),
        )
        .when_some(detail, |d, text| d.child(text))
}
impl Render for AppView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if matches!(
            &self.page,
            Page::RepoSettings(repo) if !self.config.repos.contains_key(repo)
        ) {
            self.page = Page::Repositories;
        }
        let (header, body) = match &self.page {
            Page::PullRequests => self.render_dashboard(cx),
            Page::Repositories => self.render_repos(cx),
            Page::RepoSettings(repo) => self.render_repo_settings(repo, cx),
        };
        div()
            .size_full()
            .flex()
            .bg(night())
            .text_color(moon())
            .child(self.render_sidebar(cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(header)
                    .when_some(self.error.clone(), |d, error| {
                        d.child(
                            div()
                                .mx(px(ROW_INSET))
                                .mt(px(16.))
                                .px(px(14.))
                                .py(px(10.))
                                .rounded(px(6.))
                                .bg(coral().opacity(0.14))
                                .text_color(coral())
                                .text_size(px(13.))
                                .child(error),
                        )
                    })
                    .child(div().flex_1().min_h_0().child(body)),
            )
    }
}
