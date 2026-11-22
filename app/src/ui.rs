use crate::{
    gh::{self, ChildEnv},
    model::{
        self, Config, GateState, Outcome, PastSit, PrFilter, PrRef, PrRow, RepoListItem, RepoName,
        RepoScope, RepoSettings, SitState, VerdictKind,
    },
    sitter::{self, Registry, RepoLocks, SitEvent, SitHandle},
    store,
    theme::{accent, danger, divider, ink, muted, panel, raised, success, surface, warning},
};
use gpui_kit::{assets::IconName, component::StyledExt, prelude::FluentBuilder};
use gpui_kit::{
    component::{
        Disableable, Icon, Selectable, Sizable, Size,
        button::{Button, ButtonCustomVariant, ButtonGroup, ButtonVariants},
        input::{Input, InputEvent, InputState},
        select::{Select, SelectDelegate, SelectEvent, SelectItem, SelectState},
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
    Settings(SettingsPage),
}
#[derive(Clone, Eq, PartialEq)]
enum SettingsPage {
    Repositories,
    Repo(RepoName),
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

/// How a pull request row's status line reads.
struct Reading {
    /// The state's name, shown in the badge tooltip. `None` for a pull request
    /// that was never babysat.
    verb: Option<&'static str>,
    detail: Option<String>,
    /// The badge that ends the status line, shared with the gate tooltips.
    badge: GateState,
    merged: bool,
}

const SIDEBAR_WIDTH: f32 = 232.;
const ROW_INSET: f32 = 32.;
const REPO_ROW_HEIGHT: f32 = 56.;
const SETTINGS_LABEL_WIDTH: f32 = 120.;
const SETTINGS_COLUMN_GAP: f32 = 24.;
const SETTINGS_INPUT_WIDTH: f32 = 360.;

pub struct AppView {
    data: PathBuf,
    config: Config,
    env: ChildEnv,
    registry: Registry,
    locks: RepoLocks,
    page: Page,
    repos: Load<Vec<RepoName>>,
    prs: Load<Vec<PrRow>>,
    /// Merged PRs dismissed while a GitHub search may still return an older snapshot.
    dismissed_merged: BTreeSet<PrRef>,
    org_select: Entity<SelectState<Vec<PickerChoice<String>>>>,
    org_filter: Option<String>,
    /// Owner login to its downloaded profile picture.
    avatars: BTreeMap<String, PathBuf>,
    scope: RepoScope,
    pr_filter: PrFilter,
    dashboard_filter: Entity<SelectState<Vec<PickerChoice<RepoName>>>>,
    selected_filter: Option<RepoName>,
    inputs: BTreeMap<RepoName, RepoInputs>,
    states: BTreeMap<PrRef, SitState>,
    /// Each babysit's latest gate report; kept after it finishes to show what held it.
    gates: BTreeMap<PrRef, [GateState; 4]>,
    handles: HashMap<PrRef, SitHandle>,
    event_tx: async_channel::Sender<SitEvent>,
    error: Option<String>,
    /// A failed refresh; the previous list stays on screen.
    refresh_error: Option<String>,
    loading_repos: bool,
    loading_prs: bool,
    _subscriptions: Vec<Subscription>,
    _events: Task<()>,
    _clock: Task<()>,
    repo_task: Option<Task<()>>,
    avatar_task: Option<Task<()>>,
    pr_task: Option<Task<()>>,
    sweep_task: Option<Task<()>>,
    sweep_pending: bool,
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
        let dashboard_filter = cx.new(|cx| {
            SelectState::new(
                vec![PickerChoice::all("All repositories", IconName::BookMarked)],
                None,
                window,
                cx,
            )
        });
        let org_select = cx.new(|cx| {
            SelectState::new(
                vec![PickerChoice::all("All organizations", IconName::Building2)],
                None,
                window,
                cx,
            )
        });
        let window_id = window.window_handle().window_id();
        let subscriptions = vec![
            cx.subscribe(
                &org_select,
                |this: &mut Self, _, event: &SelectEvent<Vec<PickerChoice<String>>>, cx| {
                    let SelectEvent::Confirm(value) = event;
                    this.org_filter = value.clone().flatten();
                    cx.notify();
                },
            ),
            cx.subscribe(
                &dashboard_filter,
                |this: &mut Self, _, event: &SelectEvent<Vec<PickerChoice<RepoName>>>, cx| {
                    let SelectEvent::Confirm(value) = event;
                    this.selected_filter = value.clone().flatten();
                    cx.notify();
                },
            ),
            cx.observe_keystrokes(move |this: &mut Self, event, window, cx| {
                if window.window_handle().window_id() == window_id
                    && event.keystroke.key == "escape"
                    && matches!(this.page, Page::Settings(_))
                {
                    this.page = Page::PullRequests;
                    cx.notify();
                }
            }),
        ];
        let (event_tx, event_rx) = async_channel::unbounded::<SitEvent>();
        let events = cx.spawn_in(window, async move |view, cx| {
            while let Ok(event) = event_rx.recv().await {
                if view
                    .update_in(cx, |this, window, cx| {
                        if matches!(this.states.get(&event.pr), Some(SitState::Cancelling))
                            && !matches!(event.state, SitState::Finished(_))
                        {
                            return;
                        }
                        let finished = matches!(event.state, SitState::Finished(_));
                        if let SitState::Finished(outcome) = &event.state {
                            let title = match &this.prs {
                                Load::Ready(prs) => prs
                                    .iter()
                                    .find(|row| row.pr == event.pr)
                                    .map(|row| row.title.clone()),
                                _ => None,
                            }
                            .or_else(|| {
                                this.config
                                    .history
                                    .get(&event.pr)
                                    .map(|past| past.title.clone())
                            })
                            .unwrap_or_else(|| format!("Pull request #{}", event.pr.number));
                            this.handles.remove(&event.pr);
                            this.config.active.remove(&event.pr);
                            this.config.history.insert(
                                event.pr.clone(),
                                PastSit {
                                    title,
                                    outcome: outcome.clone(),
                                    finished_at: model::unix_now(),
                                },
                            );
                            this.save();
                        }
                        if let SitState::Running {
                            last_line: Some(line),
                        } = &event.state
                            && let Some(gates) = model::parse_gates(line)
                        {
                            this.gates.insert(event.pr, gates);
                            cx.notify();
                            return;
                        }
                        this.states.insert(event.pr, event.state);
                        if finished {
                            this.update_filter(window, cx);
                        }
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
            dismissed_merged: BTreeSet::new(),
            org_select,
            org_filter: None,
            avatars: BTreeMap::new(),
            scope: RepoScope::All,
            pr_filter: PrFilter::default(),
            dashboard_filter,
            selected_filter: None,
            inputs: BTreeMap::new(),
            states: BTreeMap::new(),
            gates: BTreeMap::new(),
            handles: HashMap::new(),
            event_tx,
            error,
            refresh_error: None,
            loading_repos: false,
            loading_prs: false,
            _subscriptions: subscriptions,
            _events: events,
            _clock: clock,
            repo_task: None,
            avatar_task: None,
            pr_task: None,
            sweep_task: None,
            sweep_pending: false,
        };
        for (pr, past) in &this.config.history {
            this.states
                .insert(pr.clone(), SitState::Finished(past.outcome.clone()));
        }
        for (repo, settings) in this.config.repos.clone() {
            this.add_inputs(repo, &settings, window, cx);
        }
        for (pr, deadline) in this.config.active.clone() {
            this.resume(pr, deadline);
        }
        this.update_filter(window, cx);
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
        if self.loading_repos || self.loading_prs {
            return;
        }
        self.load_repos(window, cx);
        self.load_prs(window, cx);
    }
    fn load_repos(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.repos, Load::Ready(_)) {
            self.repos = Load::Loading;
        }
        self.loading_repos = true;
        let env = self.env.clone();
        self.repo_task = Some(cx.spawn_in(window, async move |view, cx| {
            let result = cx
                .background_spawn(async move { gh::list_repos(&env).map_err(|e| format!("{e:#}")) })
                .await;
            let _ = view.update_in(cx, |this, window, cx| {
                this.loading_repos = false;
                match result {
                    Ok(repos) => {
                        this.refresh_error = None;
                        this.repos = Load::Ready(repos);
                        this.update_org_choices(window, cx);
                        this.load_avatars(window, cx);
                    }
                    Err(error) if matches!(this.repos, Load::Ready(_)) => {
                        this.refresh_error =
                            Some(format!("Could not refresh repositories: {error}"));
                    }
                    Err(error) => this.repos = Load::Error(error),
                }
                cx.notify();
            });
        }));
    }
    fn owners(&self) -> Vec<String> {
        let Load::Ready(repos) = &self.repos else {
            return Vec::new();
        };
        let mut owners: Vec<_> = repos
            .iter()
            .map(|repo| repo.owner.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        owners.sort_by_cached_key(|owner| owner.to_lowercase());
        owners
    }
    fn update_org_choices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let owners = self.owners();
        if self
            .org_filter
            .as_ref()
            .is_some_and(|selected| !owners.contains(selected))
        {
            self.org_filter = None;
        }
        let mut choices = vec![PickerChoice::all("All organizations", IconName::Building2)];
        choices.extend(owners.into_iter().map(|owner| PickerChoice {
            title: owner.clone().into(),
            mark: Mark::Avatar(self.avatars.get(&owner).cloned()),
            value: Some(owner),
        }));
        let selected = self.org_filter.clone();
        self.org_select.update(cx, |state, cx| {
            state.set_items(choices, window, cx);
            state.set_selected_value(&selected, window, cx);
            cx.notify();
        });
    }
    fn load_avatars(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let missing: Vec<_> = self
            .owners()
            .into_iter()
            .filter(|owner| !self.avatars.contains_key(owner))
            .collect();
        if missing.is_empty() {
            return;
        }
        let env = self.env.clone();
        let data = self.data.clone();
        self.avatar_task = Some(cx.spawn_in(window, async move |view, cx| {
            let fetched: Vec<_> = cx
                .background_spawn(async move {
                    missing
                        .into_iter()
                        .filter_map(|owner| {
                            gh::avatar(&env, &data, &owner).map(|path| (owner, path))
                        })
                        .collect()
                })
                .await;
            let _ = view.update_in(cx, |this, window, cx| {
                this.avatars.extend(fetched);
                this.update_org_choices(window, cx);
                this.update_filter(window, cx);
                cx.notify();
            });
        }));
    }
    fn load_prs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.prs, Load::Ready(_)) {
            self.prs = Load::Loading;
        }
        self.loading_prs = true;
        let env = self.env.clone();
        self.pr_task = Some(cx.spawn_in(window, async move |view, cx| {
            let result = cx
                .background_spawn(
                    async move { gh::list_my_prs(&env).map_err(|e| format!("{e:#}")) },
                )
                .await;
            let _ = view.update_in(cx, |this, window, cx| {
                this.loading_prs = false;
                match result {
                    Ok(mut prs) => {
                        this.refresh_error = None;
                        prs.retain(|row| !this.dismissed_merged.contains(&row.pr));
                        this.prs = Load::Ready(prs);
                        this.sweep_worktrees(window, cx);
                    }
                    Err(error) if matches!(this.prs, Load::Ready(_)) => {
                        this.refresh_error =
                            Some(format!("Could not refresh pull requests: {error}"));
                    }
                    Err(error) => this.prs = Load::Error(error),
                }
                this.update_filter(window, cx);
                cx.notify();
            });
        }));
    }
    fn sweep_worktrees(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sweep_task.is_some() {
            self.sweep_pending = true;
            return;
        }
        let Load::Ready(prs) = &self.prs else {
            return;
        };
        let mut keep: BTreeSet<_> = prs.iter().map(|row| row.pr.clone()).collect();
        keep.extend(
            self.states
                .iter()
                .filter(|(_, state)| state.is_active())
                .map(|(pr, _)| pr.clone()),
        );
        let env = self.env.clone();
        let data = self.data.clone();
        let locks = self.locks.clone();
        self.sweep_task = Some(cx.spawn_in(window, async move |view, cx| {
            cx.background_spawn(async move { sitter::sweep_merged(&env, &data, &locks, &keep) })
                .await;
            let _ = view.update_in(cx, |this, window, cx| {
                this.sweep_task = None;
                if std::mem::take(&mut this.sweep_pending) {
                    this.sweep_worktrees(window, cx);
                }
                cx.notify();
            });
        }));
    }
    fn update_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prs = match &self.prs {
            Load::Ready(prs) => prs.as_slice(),
            _ => &[],
        };
        let repos: BTreeSet<_> = model::dashboard_rows(
            prs,
            &self.config.history,
            &self.states,
            &self.config.repos,
            PrFilter::All,
            None,
        )
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
        let mut choices = vec![PickerChoice::all("All repositories", IconName::BookMarked)];
        choices.extend(repos.into_iter().map(|repo| PickerChoice {
            title: repo.to_string().into(),
            mark: Mark::Avatar(self.avatars.get(&repo.owner).cloned()),
            value: Some(repo),
        }));
        let selected = self.selected_filter.clone();
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
        self.gates.remove(&pr);
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
        let previous = self.config.history.remove(&pr);
        self.config.active.insert(pr.clone(), deadline);
        if !self.save() {
            self.config.active.remove(&pr);
            if let Some(past) = previous {
                self.config.history.insert(pr, past);
            }
            cx.notify();
            return;
        }
        self.resume(pr, deadline);
        self.update_filter(window, cx);
        cx.notify();
    }
    fn dismiss(&mut self, pr: PrRef, window: &mut Window, cx: &mut Context<Self>) {
        let merged = match self.states.get(&pr) {
            Some(SitState::Finished(outcome)) => *outcome == Outcome::Merged,
            _ => return,
        };
        let previous = self.config.history.remove(&pr);
        if !self.save() {
            if let Some(past) = previous {
                self.config.history.insert(pr, past);
            }
            cx.notify();
            return;
        }
        if merged {
            if let Load::Ready(prs) = &mut self.prs {
                prs.retain(|row| row.pr != pr);
            }
            self.dismissed_merged.insert(pr.clone());
        }
        self.states.remove(&pr);
        self.gates.remove(&pr);
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
        use GateState::{Blocked, Met, Pending, Waiting};
        let state = self.states.get(pr);
        let reading = |badge, verb, detail: Option<&str>| Reading {
            verb: Some(verb),
            detail: detail.map(str::to_owned),
            badge,
            merged: false,
        };
        let merged = Reading {
            verb: Some("Merged"),
            detail: Some("Merged".into()),
            badge: Met,
            merged: true,
        };
        match state {
            None => Reading {
                verb: None,
                detail: None,
                badge: Pending,
                merged: false,
            },
            Some(SitState::Preparing) => {
                reading(Waiting, "Preparing", Some("Setting up the worktree"))
            }
            Some(SitState::Running { last_line: None }) => {
                reading(Waiting, "Starting", Some("Taking the first snapshot"))
            }
            Some(SitState::Running {
                last_line: Some(line),
            }) => match model::parse_pass(line) {
                Some(pass) => match pass.kind {
                    VerdictKind::Done => merged,
                    VerdictKind::Wait => reading(Waiting, "Waiting", Some(pass.detail)),
                    VerdictKind::Act => reading(Waiting, "Working", Some(pass.detail)),
                    VerdictKind::Stop => reading(Blocked, "Stopping", Some(pass.detail)),
                },
                None => reading(
                    Waiting,
                    "Running",
                    Some(line.strip_prefix("baby-sitter: ").unwrap_or(line)),
                ),
            },
            Some(SitState::Cancelling) => reading(
                Pending,
                "Cancelling",
                Some("Waiting for the process to exit"),
            ),
            Some(SitState::Finished(Outcome::Merged)) => merged,
            Some(SitState::Finished(Outcome::TimedOut)) => reading(
                Waiting,
                "Timed out",
                Some("The deadline passed before the merge"),
            ),
            Some(SitState::Finished(Outcome::Stopped(reason))) => {
                reading(Blocked, "Stopped", Some(reason))
            }
            Some(SitState::Finished(Outcome::Cancelled)) => reading(Pending, "Cancelled", None),
            Some(SitState::Finished(Outcome::Failed(reason))) => {
                reading(Blocked, "Failed", Some(reason))
            }
        }
    }
    fn repo_row(&self, repo: &RepoName, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.config.repos.contains_key(repo);
        let count = self.active_count(repo);
        let switch_repo = repo.clone();
        let settings_repo = repo.clone();
        let switch = Switch::new(SharedString::from(format!("repo-{repo}")))
            .accessibility_label(format!("Watch {repo}"))
            .checked(selected)
            .disabled(count > 0)
            .on_click(cx.listener(move |this, checked, window, cx| {
                this.toggle_repo(switch_repo.clone(), *checked, window, cx)
            }));
        let settings = Button::new(SharedString::from(format!("settings-{repo}")))
            .icon(IconName::Settings)
            .accessibility_label(format!("Settings for {repo}"))
            .ghost()
            .disabled(!selected)
            .tooltip(if selected {
                "Settings"
            } else {
                "Turn on to configure"
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.page = Page::Settings(SettingsPage::Repo(settings_repo.clone()));
                cx.notify();
            }));
        div()
            .h(px(REPO_ROW_HEIGHT))
            .w_full()
            .px(px(ROW_INSET))
            .border_b_1()
            .border_color(divider())
            .flex()
            .items_center()
            .gap(px(20.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_ellipsis()
                    .text_color(ink())
                    .child(repo.name.clone()),
            )
            .when(count > 0, |row| {
                row.child(
                    div()
                        .flex_shrink_0()
                        .px(px(8.))
                        .py(px(3.))
                        .rounded(px(12.))
                        .bg(accent().opacity(0.12))
                        .text_size(px(12.))
                        .text_color(accent())
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
                );
                if items.is_empty() {
                    if self.scope == RepoScope::Watched
                        && !repos
                            .iter()
                            .any(|repo| self.config.repos.contains_key(repo))
                    {
                        message(
                            "No watched repositories",
                            Some(
                                "Turn on a repository under All to watch its pull requests.".into(),
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
                                            watched,
                                            total,
                                        } => div()
                                            .h(px(REPO_ROW_HEIGHT))
                                            .w_full()
                                            .px(px(ROW_INSET))
                                            .bg(panel())
                                            .border_b_1()
                                            .border_color(divider())
                                            .flex()
                                            .items_center()
                                            .gap(px(20.))
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .flex()
                                                    .items_center()
                                                    .gap(px(10.))
                                                    .child(avatar(this.avatars.get(name), 20.))
                                                    .child(
                                                        div()
                                                            .min_w_0()
                                                            .font_semibold()
                                                            .text_ellipsis()
                                                            .text_color(ink())
                                                            .child(name.clone()),
                                                    ),
                                            )
                                            .child(
                                                div()
                                                    .flex_shrink_0()
                                                    .text_size(px(12.))
                                                    .text_color(muted())
                                                    .child(format!("{watched} of {total} watched")),
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
                    .text_color(if selected { accent() } else { muted() })
                    .when(selected, |d| d.font_semibold())
                    .child(label),
            )
        };
        let scope = ButtonGroup::new("repo-scope")
            .with_size(Size::Medium)
            .outline()
            .child(segment("scope-all", "All", RepoScope::All))
            .child(segment("scope-watched", "Watched", RepoScope::Watched))
            .on_click(cx.listener(|this, selected: &Vec<usize>, _, cx| {
                if let Some(index) = selected.first() {
                    this.scope = if *index == 0 {
                        RepoScope::All
                    } else {
                        RepoScope::Watched
                    };
                    cx.notify();
                }
            }));
        let filters = div()
            .flex_shrink_0()
            .px(px(ROW_INSET))
            .pb(px(16.))
            .border_b_1()
            .border_color(divider())
            .flex()
            .items_center()
            .gap(px(12.))
            .child(header_picker(
                &self.org_select,
                "Filter by organization",
                "All organizations",
                Size::Medium,
            ))
            .child(scope);
        let body = div()
            .size_full()
            .flex()
            .flex_col()
            .child(filters)
            .child(div().flex_1().min_h_0().child(list));
        let header = div()
            .flex_shrink_0()
            .pt(px(20.))
            .child(
                div().px(px(ROW_INSET)).flex().items_start().child(
                    Button::new("back-to-pull-requests")
                        .icon(IconName::ChevronLeft)
                        .label("Pull requests")
                        .ghost()
                        .ml(px(-8.))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.page = Page::PullRequests;
                            cx.notify();
                        })),
                ),
            )
            .child(
                header(
                    "Repositories",
                    Some("Babysits run only in the repositories you watch."),
                )
                .pt(px(10.)),
            );
        (header, body)
    }
    fn render_repo_settings(&self, repo: &RepoName, cx: &mut Context<Self>) -> (Div, Div) {
        let inputs = self
            .inputs
            .get(repo)
            .expect("watched repositories have duration inputs");
        let field = |label: &'static str,
                     hint: &'static str,
                     input: &Entity<InputState>,
                     error: &Option<String>| {
            let note = match error {
                Some(error) => div().text_color(danger()).child(error.clone()),
                None => div().text_color(muted()).child(hint),
            };
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap(px(SETTINGS_COLUMN_GAP))
                        .child(
                            div()
                                .w(px(SETTINGS_LABEL_WIDTH))
                                .flex_shrink_0()
                                .text_size(px(14.))
                                .font_semibold()
                                .text_color(ink())
                                .child(label),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .max_w(px(SETTINGS_INPUT_WIDTH))
                                .child(Input::new(input)),
                        ),
                )
                .child(
                    note.ml(px(SETTINGS_LABEL_WIDTH + SETTINGS_COLUMN_GAP))
                        .max_w(px(SETTINGS_INPUT_WIDTH))
                        .text_size(px(12.))
                        .line_height(relative(1.4)),
                )
        };
        let header = div()
            .flex_shrink_0()
            .px(px(ROW_INSET))
            .pt(px(20.))
            .pb(px(20.))
            .border_b_1()
            .border_color(divider())
            .flex()
            .flex_col()
            .items_start()
            .gap(px(10.))
            .child(
                Button::new("back-to-repositories")
                    .icon(IconName::ChevronLeft)
                    .label("Repositories")
                    .ghost()
                    .ml(px(-8.))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.page = Page::Settings(SettingsPage::Repositories);
                        cx.notify();
                    })),
            )
            .child(
                drag_area()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(24.))
                            .line_height(relative(1.2))
                            .font_semibold()
                            .text_color(ink())
                            .child(repo.to_string()),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(muted())
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
                        .max_w(px(SETTINGS_LABEL_WIDTH
                            + SETTINGS_COLUMN_GAP
                            + SETTINGS_INPUT_WIDTH))
                        .flex()
                        .flex_col()
                        .gap(px(20.))
                        .child(field(
                            "Timeout",
                            "How long to wait for the merge, for example 2h.",
                            &inputs.timeout,
                            &inputs.timeout_error,
                        ))
                        .child(field(
                            "Interval",
                            "How long to wait between checks, for example 30s.",
                            &inputs.interval,
                            &inputs.interval_error,
                        )),
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
            &self.config.history,
            &self.states,
            &self.config.repos,
            self.pr_filter,
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
                round_button(
                    SharedString::from(format!("cancel-{pr}")),
                    IconName::Square,
                    if cancelling {
                        "Cancelling babysit"
                    } else {
                        "Cancel babysit"
                    },
                    muted(),
                    danger(),
                    cx,
                )
                .disabled(cancelling)
                .on_click(
                    cx.listener(move |this, _, window, cx| this.ask_cancel(pr.clone(), window, cx)),
                ),
            )
        } else if reading.merged || !self.config.repos.contains_key(&row.pr.repo) {
            None
        } else {
            Some(
                round_button(
                    SharedString::from(format!("start-{pr}")),
                    IconName::Play,
                    "Start babysit",
                    accent(),
                    accent(),
                    cx,
                )
                .on_click(
                    cx.listener(move |this, _, window, cx| this.start(pr.clone(), window, cx)),
                ),
            )
        };
        let dismiss = matches!(state, Some(SitState::Finished(_))).then(|| {
            let pr = row.pr.clone();
            round_button(
                SharedString::from(format!("dismiss-{pr}")),
                IconName::X,
                "Dismiss",
                muted(),
                ink(),
                cx,
            )
            .on_click(cx.listener(move |this, _, window, cx| this.dismiss(pr.clone(), window, cx)))
        });
        let time_left = active
            .then(|| self.config.active.get(&row.pr))
            .flatten()
            .map(|deadline| time_left(deadline.saturating_sub(model::unix_now())));
        let url = row.url.clone();
        let open = round_button(
            SharedString::from(format!("open-{}", row.pr)),
            IconName::Github,
            "Open on GitHub",
            muted(),
            ink(),
            cx,
        )
        .on_click(move |_, _, _| {
            let url = url.clone();
            std::thread::spawn(move || {
                let _ = std::process::Command::new("open").arg(url).status();
            });
        });
        let title = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(
                div()
                    .min_w_0()
                    .text_size(px(16.))
                    .line_height(relative(1.3))
                    .font_semibold()
                    .text_color(ink())
                    .child(row.title.clone()),
            )
            .when(row.is_draft, |title| {
                title.child(
                    div()
                        .id(SharedString::from(format!("draft-{}", row.pr)))
                        .flex_shrink_0()
                        .text_color(muted())
                        .child(Icon::new(IconName::GitPullRequestDraft).size(px(16.)))
                        .tooltip(|window, cx| Tooltip::new("Draft").build(window, cx)),
                )
            });
        let place = format!("{} #{}", row.pr.repo, row.pr.number);
        let gates = self.gates.get(&row.pr);
        let status = reading.verb.map(|verb| {
            div()
                .min_w_0()
                .flex()
                .items_start()
                .gap(px(8.))
                .text_size(px(13.))
                .line_height(relative(1.45))
                .when_some(gates, |line, gates| {
                    line.child(gate_icons(&row.pr, gates, active))
                })
                .when_some(reading.detail.clone(), |line, detail| {
                    line.child(div().min_w_0().text_color(ink()).child(detail))
                })
                .child(
                    div()
                        .id(SharedString::from(format!("status-{}", row.pr)))
                        .flex_shrink_0()
                        .h(px(19.))
                        .flex()
                        .items_center()
                        .child(status_badge(reading.badge))
                        .tooltip(move |window, cx| Tooltip::new(verb).build(window, cx)),
                )
        });
        let path = sitter::worktree_path(&self.data, &row.pr);
        let worktree = (!active && path.exists()).then(|| {
            let folder = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            div()
                .flex_shrink_0()
                .h(px(19.))
                .flex()
                .items_center()
                .gap(px(6.))
                .text_size(px(12.))
                .text_color(muted())
                .child(Icon::new(IconName::FolderGit2).size(px(14.)))
                .child(folder)
                .child(
                    icon_button(
                        SharedString::from(format!("reveal-{}", row.pr)),
                        IconName::FolderOpen,
                        "Reveal in Finder",
                        px(24.),
                    )
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
        let meta = (status.is_some() || worktree.is_some()).then(|| {
            div()
                .pt(px(6.))
                .flex()
                .items_start()
                .gap(px(16.))
                .when_some(status, |d, status| d.child(status))
                .when_some(worktree, |d, worktree| d.child(worktree))
        });
        let content = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(title)
            .child(div().text_size(px(13.)).text_color(muted()).child(place))
            .when_some(meta, |d, meta| d.child(meta));
        div()
            .px(px(ROW_INSET))
            .flex_shrink_0()
            .py(px(18.))
            .border_b_1()
            .border_color(divider())
            .flex()
            .items_start()
            .gap(px(20.))
            .child(content)
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .when_some(time_left, |d, left| {
                        d.child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(5.))
                                .text_size(px(12.))
                                .text_color(muted())
                                .child(Icon::empty().path("icons/timer.svg").size(px(13.)))
                                .child(left),
                        )
                    })
                    .map(|d| match (action, dismiss) {
                        // The rightmost slot holds the row's main action; a merged row's is Dismiss.
                        (None, Some(dismiss)) => d.child(open).child(dismiss),
                        (action, dismiss) => d
                            .when_some(dismiss, |d, dismiss| d.child(dismiss))
                            .child(open)
                            .child(match action {
                                Some(action) => action.into_any_element(),
                                None => div().size(px(34.)).into_any_element(),
                            }),
                    }),
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
                if self.pr_filter == PrFilter::All && self.config.repos.is_empty() {
                    message(
                        "No watched repositories",
                        Some("Watch a repository to see its pull requests.".into()),
                    )
                    .child(
                        Button::new("choose-repositories")
                            .label("Choose repositories")
                            .primary()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.page = Page::Settings(SettingsPage::Repositories);
                                cx.notify();
                            })),
                    )
                } else if self.selected_filter.is_some() {
                    message(
                        "No pull requests in this repository",
                        Some("Choose All repositories to see the rest.".into()),
                    )
                } else {
                    match self.pr_filter {
                        PrFilter::Watched => message(
                            "Nothing is being babysat",
                            Some("Press play on a pull request to start.".into()),
                        ),
                        PrFilter::Unwatched => {
                            message("Every listed pull request is being babysat", None)
                        }
                        PrFilter::Merged => message(
                            "No merged babysits",
                            Some(
                                "Babysat PRs show here after they merge, until you dismiss them."
                                    .into(),
                            ),
                        ),
                        PrFilter::All => message("No open pull requests", None),
                    }
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
        let header = div()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .child(drag_area().h(px(52.)))
            .child(
                div()
                    .px(px(ROW_INSET))
                    .pb(px(20.))
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(
                        drag_area()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .child(pair_mark())
                            .child(
                                div()
                                    .text_size(px(24.))
                                    .font_semibold()
                                    .text_color(ink())
                                    .child("Baby Sitter"),
                            ),
                    )
                    .child(
                        round_button_with(
                            "refresh".into(),
                            refresh_icon(self.loading_repos || self.loading_prs),
                            "Refresh",
                            raised(),
                            ink(),
                            cx,
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.refresh(window, cx);
                            cx.notify();
                        })),
                    )
                    .child(
                        round_button(
                            "settings".into(),
                            IconName::Settings,
                            "Settings",
                            raised(),
                            ink(),
                            cx,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.page = Page::Settings(SettingsPage::Repositories);
                            cx.notify();
                        })),
                    ),
            );
        let segment = |id: &'static str, label: &'static str, filter: PrFilter| {
            let selected = self.pr_filter == filter;
            Button::new(id).selected(selected).child(
                div()
                    .text_color(if selected { accent() } else { muted() })
                    .when(selected, |d| d.font_semibold())
                    .child(label),
            )
        };
        let filter = ButtonGroup::new("pr-filter")
            .with_size(Size::Medium)
            .outline()
            .child(segment("pr-filter-all", "All", PrFilter::All))
            .child(segment("pr-filter-watched", "Watched", PrFilter::Watched))
            .child(segment(
                "pr-filter-unwatched",
                "Unwatched",
                PrFilter::Unwatched,
            ))
            .child(segment("pr-filter-merged", "Merged", PrFilter::Merged))
            .on_click(cx.listener(|this, selected: &Vec<usize>, _, cx| {
                let filters = [
                    PrFilter::All,
                    PrFilter::Watched,
                    PrFilter::Unwatched,
                    PrFilter::Merged,
                ];
                if let Some(filter) = selected.first().and_then(|index| filters.get(*index)) {
                    this.pr_filter = *filter;
                    cx.notify();
                }
            }));
        let filters = div()
            .flex_shrink_0()
            .px(px(ROW_INSET))
            .pb(px(16.))
            .border_b_1()
            .border_color(divider())
            .flex()
            .items_center()
            .gap(px(12.))
            .child(header_picker(
                &self.dashboard_filter,
                "Filter by repository",
                "All repositories",
                Size::Medium,
            ))
            .child(filter);
        let body = div()
            .size_full()
            .flex()
            .flex_col()
            .child(filters)
            .child(div().flex_1().min_h_0().child(list));
        (header, body)
    }
    fn render_sidebar(&self, cx: &mut Context<Self>) -> Div {
        div()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(panel())
            .border_r_1()
            .border_color(divider())
            .child(drag_area().h(px(52.)))
            .child(
                div()
                    .px(px(20.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(pair_mark())
                    .child(
                        div()
                            .text_size(px(18.))
                            .font_semibold()
                            .text_color(ink())
                            .child("Baby Sitter"),
                    ),
            )
            .child(
                div().px(px(12.)).pt(px(28.)).child(
                    Button::new("nav-repos")
                        .custom(
                            ButtonCustomVariant::new(cx)
                                .foreground(ink())
                                .hover(raised())
                                .active(divider()),
                        )
                        .w_full()
                        .h(px(34.))
                        .accessibility_label("Repositories")
                        .child(
                            div()
                                .flex_1()
                                .flex()
                                .items_center()
                                .gap(px(12.))
                                .px(px(4.))
                                .text_size(px(14.))
                                .font_semibold()
                                .child(
                                    Icon::new(IconName::BookMarked)
                                        .size_4()
                                        .text_color(accent()),
                                )
                                .child("Repositories"),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.page = Page::Settings(SettingsPage::Repositories);
                            cx.notify();
                        })),
                ),
            )
    }
}

/// A round icon-only button: tinted with `tint` at rest, filled further with `hover` under the pointer.
fn round_button(
    id: SharedString,
    icon: IconName,
    label: &'static str,
    tint: Hsla,
    hover: Hsla,
    cx: &App,
) -> Button {
    round_button_with(id, Icon::new(icon).size(px(14.)), label, tint, hover, cx)
}
fn round_button_with(
    id: SharedString,
    child: impl IntoElement,
    label: &'static str,
    tint: Hsla,
    hover: Hsla,
    cx: &App,
) -> Button {
    Button::new(id)
        .custom(
            ButtonCustomVariant::new(cx)
                .color(tint)
                .foreground(hover)
                .hover(hover.opacity(0.3))
                .active(hover.opacity(0.45)),
        )
        .rounded(px(17.))
        .w(px(34.))
        .h(px(34.))
        .p_0()
        .accessibility_label(label)
        .tooltip(label)
        .child(child)
}
/// The refresh icon, turning while a repository or pull request load is in flight.
fn refresh_icon(spinning: bool) -> AnyElement {
    let icon = Icon::new(IconName::RefreshCw).size(px(14.));
    if spinning {
        icon.with_animation(
            "refresh-spin",
            Animation::new(Duration::from_millis(900)).repeat(),
            |icon, delta| icon.transform(Transformation::rotate(percentage(delta))),
        )
        .into_any_element()
    } else {
        icon.into_any_element()
    }
}
/// A borderless round button showing only an icon, for secondary row actions.
fn icon_button(id: SharedString, icon: IconName, label: &'static str, size: Pixels) -> Button {
    Button::new(id)
        .ghost()
        .rounded(size / 2.)
        .w(size)
        .h(size)
        .p_0()
        .accessibility_label(label)
        .tooltip(label)
        .child(Icon::new(icon).size(px(14.)))
}

/// The page title row. Its empty space drags the window, since the title bar is hidden.
fn header(title: &'static str, detail: Option<&'static str>) -> Div {
    div()
        .flex_shrink_0()
        .px(px(ROW_INSET))
        .pt(px(36.))
        .pb(px(20.))
        .flex()
        .items_start()
        .gap(px(16.))
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
                        .text_color(ink())
                        .child(title),
                )
                .when_some(detail, |d, detail| {
                    d.child(div().text_size(px(13.)).text_color(muted()).child(detail))
                }),
        )
}
/// A filter picker with a fixed width for the page header or dashboard filters.
fn header_picker<D>(
    state: &Entity<SelectState<D>>,
    label: &'static str,
    placeholder: &'static str,
    size: Size,
) -> Div
where
    D: SelectDelegate,
    <D::Item as SelectItem>::Value: PartialEq + Clone,
{
    div().w(px(240.)).flex_shrink_0().child(
        Select::new(state)
            .with_size(size)
            .accessibility_label(label)
            .placeholder(placeholder),
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
/// One choice in a header picker: an organization or repository with its owner's
/// profile picture, or the "All …" choice with an icon.
#[derive(Clone)]
struct PickerChoice<V> {
    /// `None` is the "All …" choice.
    value: Option<V>,
    title: SharedString,
    mark: Mark,
}
#[derive(Clone)]
enum Mark {
    Icon(IconName),
    Avatar(Option<PathBuf>),
}
impl<V> PickerChoice<V> {
    fn all(title: &'static str, icon: IconName) -> Self {
        Self {
            value: None,
            title: title.into(),
            mark: Mark::Icon(icon),
        }
    }
    fn row(&self) -> Div {
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .min_w_0()
            .child(match &self.mark {
                Mark::Avatar(path) => avatar(path.as_ref(), 18.),
                Mark::Icon(icon) => Icon::new(*icon)
                    .size(px(18.))
                    .text_color(muted())
                    .into_any_element(),
            })
            .child(div().min_w_0().text_ellipsis().child(self.title.clone()))
    }
}
impl<V: Clone + PartialEq + 'static> SelectItem for PickerChoice<V> {
    type Value = Option<V>;
    fn title(&self) -> SharedString {
        self.title.clone()
    }
    fn display_title(&self) -> Option<AnyElement> {
        Some(self.row().into_any_element())
    }
    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        self.row()
    }
    fn value(&self) -> &Self::Value {
        &self.value
    }
}
/// A profile picture, or a blank tile of the same size until it downloads.
fn avatar(path: Option<&PathBuf>, size: f32) -> AnyElement {
    match path {
        Some(path) => img(path.clone())
            .size(px(size))
            .flex_shrink_0()
            .rounded(px(4.))
            .into_any_element(),
        None => div()
            .size(px(size))
            .flex_shrink_0()
            .rounded(px(4.))
            .bg(divider())
            .into_any_element(),
    }
}
/// The merge requirements in the order the CLI reports them, each with its own icon.
const GATE_ICONS: [(IconName, &str); 4] = [
    (IconName::MessageSquare, "Reviews"),
    (IconName::ListChecks, "Checks"),
    (IconName::GitBranch, "Branch up to date"),
    (IconName::GitMerge, "Merge"),
];
/// One icon per merge requirement, colored by its state and named in its tooltip.
/// A waiting requirement pulses while the babysit runs.
fn gate_icons(pr: &PrRef, gates: &[GateState; 4], live: bool) -> Div {
    div()
        .flex_shrink_0()
        .h(px(19.))
        .flex()
        .items_center()
        .gap(px(6.))
        .children(GATE_ICONS.iter().zip(gates).map(|((icon, name), state)| {
            let (color, word) = state_style(*state);
            let label = SharedString::from(format!("{name}: {word}"));
            let state = *state;
            let id = SharedString::from(format!("gate-{pr}-{name}"));
            let mark = Icon::new(*icon).size(px(15.)).text_color(color);
            let mark = if live && state == GateState::Waiting {
                mark.with_animation(
                    id.clone(),
                    Animation::new(Duration::from_millis(2400))
                        .repeat()
                        .with_easing(pulsating_between(0.35, 1.0)),
                    move |mark, delta| mark.text_color(color.opacity(delta)),
                )
                .into_any_element()
            } else {
                mark.into_any_element()
            };
            div().id(id).child(mark).tooltip(move |window, cx| {
                let label = label.clone();
                Tooltip::element(move |_, _| {
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .child(status_badge(state))
                        .child(label.clone())
                })
                .build(window, cx)
            })
        }))
}
/// The color and word for a state, shared by gate icons, tooltips, and status badges.
fn state_style(state: GateState) -> (Hsla, &'static str) {
    match state {
        GateState::Met => (success(), "done"),
        GateState::Waiting => (warning(), "waiting"),
        GateState::Blocked => (danger(), "blocked"),
        GateState::Pending => (muted().opacity(0.45), "not started"),
    }
}
/// A filled circle holding a check, dash, or cross; a hollow ring when not started.
fn status_badge(state: GateState) -> Div {
    let (color, _) = state_style(state);
    let badge = div()
        .size(px(14.))
        .flex_shrink_0()
        .rounded_full()
        .flex()
        .items_center()
        .justify_center();
    let glyph = match state {
        GateState::Met => IconName::Check,
        GateState::Waiting => IconName::Minus,
        GateState::Blocked => IconName::X,
        GateState::Pending => return badge.border_2().border_color(color),
    };
    badge
        .bg(color)
        .child(Icon::new(glyph).size(px(10.)).text_color(panel()))
}
/// The caretaker and the child: a lit disc holding a smaller light one.
fn pair_mark() -> Div {
    div()
        .relative()
        .w(px(22.))
        .h(px(20.))
        .flex_shrink_0()
        .child(
            div()
                .absolute()
                .top(px(1.))
                .left_0()
                .size(px(16.))
                .rounded_full()
                .bg(accent()),
        )
        .child(
            div()
                .absolute()
                .top(px(7.))
                .left(px(9.))
                .size(px(13.))
                .rounded_full()
                .border_2()
                .border_color(panel())
                .bg(ink()),
        )
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
        .text_color(muted())
        .child(
            div()
                .text_size(px(16.))
                .font_semibold()
                .text_color(ink())
                .child(title),
        )
        .when_some(detail, |d, text| d.child(text))
}
impl Render for AppView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if matches!(
            &self.page,
            Page::Settings(SettingsPage::Repo(repo)) if !self.config.repos.contains_key(repo)
        ) {
            self.page = Page::Settings(SettingsPage::Repositories);
        }
        let (header, body) = match &self.page {
            Page::PullRequests => self.render_dashboard(cx),
            Page::Settings(SettingsPage::Repositories) => self.render_repos(cx),
            Page::Settings(SettingsPage::Repo(repo)) => self.render_repo_settings(repo, cx),
        };
        div()
            .size_full()
            .flex()
            .bg(surface())
            .text_color(ink())
            .when(matches!(self.page, Page::Settings(_)), |d| {
                d.child(self.render_sidebar(cx))
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(header)
                    .when_some(
                        self.error.clone().or(self.refresh_error.clone()),
                        |d, error| {
                            d.child(
                                div()
                                    .mx(px(ROW_INSET))
                                    .mt(px(16.))
                                    .px(px(14.))
                                    .py(px(10.))
                                    .rounded(px(6.))
                                    .bg(danger().opacity(0.14))
                                    .text_color(danger())
                                    .text_size(px(13.))
                                    .child(error),
                            )
                        },
                    )
                    .child(div().flex_1().min_h_0().child(body)),
            )
    }
}
