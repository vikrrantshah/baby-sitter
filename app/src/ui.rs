use crate::{
    gh::{self, ChildEnv},
    model::{
        self, ActiveSit, AgentKind, Config, Defaults, GateState, Outcome, PastSit, PrRef, PrRow,
        PrState, RepoListItem, RepoName, RepoScope, RepoSettings, SitState, StackMode, StateFilter,
        VerdictKind, WatchFilter,
    },
    models::{self, ModelOption},
    notifications,
    setup::{self, CheckSpec, CheckState, Fix, Section},
    sitter::{self, Registry, RepoLocks, SitEvent, SitHandle},
    store,
    theme::{accent, danger, divider, ink, muted, panel, raised, success, surface, warning},
};
use gpui_kit::{assets::IconName, component::StyledExt, prelude::FluentBuilder};
use gpui_kit::{
    component::{
        Disableable, Icon, Selectable, Sizable, Size, WindowExt,
        button::{Button, ButtonCustomVariant, ButtonGroup, ButtonVariants},
        input::{Input, InputEvent, InputState, Textarea, TextareaState},
        menu::{DropdownMenu, PopupMenuItem},
        notification::Notification,
        select::{Select, SelectDelegate, SelectEvent, SelectItem, SelectState},
        spinner::Spinner,
        switch::Switch,
        tooltip::Tooltip,
    },
    *,
};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap},
    path::PathBuf,
    time::{Duration, Instant},
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
    App,
    Prompts,
    Status,
    Repo(RepoName),
}
#[derive(Clone, Copy)]
enum Field {
    Timeout,
    DraftTimeout,
    Interval,
}

impl Field {
    fn default_value(self, defaults: &mut Defaults) -> &mut Duration {
        match self {
            Self::Timeout => &mut defaults.timeout,
            Self::DraftTimeout => &mut defaults.draft_timeout,
            Self::Interval => &mut defaults.interval,
        }
    }

    fn repo_value(self, settings: &mut RepoSettings) -> &mut Option<Duration> {
        match self {
            Self::Timeout => &mut settings.timeout,
            Self::DraftTimeout => &mut settings.draft_timeout,
            Self::Interval => &mut settings.interval,
        }
    }
}

#[derive(Clone)]
enum DurationTarget {
    App,
    Repo(RepoName),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RowAction {
    MarkReady,
    ConvertToDraft,
    Close,
    BabysitStack(StackMode),
    CopyLink,
}

impl RowAction {
    const ALL: [Self; 6] = [
        Self::MarkReady,
        Self::ConvertToDraft,
        Self::Close,
        Self::BabysitStack(StackMode::BottomUp),
        Self::BabysitStack(StackMode::TopDown),
        Self::CopyLink,
    ];

    fn menu_item(
        self,
        is_open: bool,
        is_draft: bool,
        merged: bool,
        stack_watched: bool,
    ) -> Option<&'static str> {
        match self {
            Self::MarkReady => (is_open && is_draft && !merged).then_some("Ready for review"),
            Self::ConvertToDraft => (is_open && !is_draft && !merged).then_some("Convert to draft"),
            Self::Close => (is_open && !merged).then_some("Close pull request"),
            Self::BabysitStack(mode) => stack_watched.then_some(match mode {
                StackMode::BottomUp => "Babysit stack bottom up",
                StackMode::TopDown => "Babysit stack top down",
            }),
            Self::CopyLink => Some("Copy link"),
        }
    }
}

fn pr_state_color(state: PrState) -> Hsla {
    match state {
        PrState::Draft => muted(),
        PrState::Ready => rgb(0x3fb950).into(),
        PrState::Queued => warning(),
        PrState::Merged => rgb(0x8250df).into(),
        PrState::Closed => danger(),
    }
}

#[derive(Clone, Copy)]
enum ClosedPr {
    AwaitingSearch,
    Confirmed,
}
#[derive(serde::Deserialize)]
struct PromptDefaults {
    review: String,
    ci: String,
    conflicts: String,
}

#[derive(Clone, Copy)]
enum PromptKind {
    Review,
    Ci,
    Conflicts,
}

impl PromptKind {
    const ALL: [Self; 3] = [Self::Review, Self::Ci, Self::Conflicts];

    fn index(self) -> usize {
        match self {
            Self::Review => 0,
            Self::Ci => 1,
            Self::Conflicts => 2,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Review => "Review threads",
            Self::Ci => "Failing CI",
            Self::Conflicts => "Merge conflicts",
        }
    }

    fn switch_label(self) -> &'static str {
        match self {
            Self::Review => "Custom review prompt",
            Self::Ci => "Custom CI fix prompt",
            Self::Conflicts => "Custom conflict prompt",
        }
    }

    fn value(self, prompts: &mut model::Prompts) -> &mut model::CustomPrompt {
        match self {
            Self::Review => &mut prompts.review,
            Self::Ci => &mut prompts.ci,
            Self::Conflicts => &mut prompts.conflicts,
        }
    }
}

struct PromptEditor {
    input: Entity<TextareaState>,
    default: String,
    error: Option<String>,
    _subscription: Subscription,
}

struct RepoInputs {
    timeout: Entity<InputState>,
    draft_timeout: Entity<InputState>,
    interval: Entity<InputState>,
    setup_command: Option<Entity<InputState>>,
    timeout_error: Option<String>,
    draft_timeout_error: Option<String>,
    interval_error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl RepoInputs {
    fn input(&self, field: Field) -> &Entity<InputState> {
        match field {
            Field::Timeout => &self.timeout,
            Field::DraftTimeout => &self.draft_timeout,
            Field::Interval => &self.interval,
        }
    }

    fn error_mut(&mut self, field: Field) -> &mut Option<String> {
        match field {
            Field::Timeout => &mut self.timeout_error,
            Field::DraftTimeout => &mut self.draft_timeout_error,
            Field::Interval => &mut self.interval_error,
        }
    }
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
const ROW_INSET: f32 = 20.;
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
    setup: Load<Vec<(&'static CheckSpec, CheckState)>>,
    models: BTreeMap<AgentKind, Load<Vec<ModelOption>>>,
    titles: BTreeMap<PrRef, String>,
    /// Merged PRs dismissed while a GitHub search may still return an older snapshot.
    dismissed_merged: BTreeSet<PrRef>,
    closed_prs: BTreeMap<PrRef, ClosedPr>,
    org_select: Entity<SelectState<Vec<PickerChoice<String>>>>,
    org_filter: Option<String>,
    /// Owner login to its downloaded profile picture.
    avatars: BTreeMap<String, PathBuf>,
    scope: RepoScope,
    watch_filter: WatchFilter,
    state_filter: StateFilter,
    dashboard_filter: Entity<SelectState<Vec<PickerChoice<RepoName>>>>,
    search: Entity<InputState>,
    selected_filter: Option<RepoName>,
    app_inputs: RepoInputs,
    prompt_editors: Load<Vec<PromptEditor>>,
    prompt_task: Option<Task<()>>,
    clear_merged_input: Entity<InputState>,
    clear_merged_error: Option<String>,
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
    _auto_refresh: Task<()>,
    _permission_watch: Task<()>,
    pulse_epoch: Instant,
    repo_task: Option<Task<()>>,
    avatar_task: Option<Task<()>>,
    pr_task: Option<Task<()>>,
    setup_task: Option<Task<()>>,
    model_tasks: BTreeMap<AgentKind, Task<()>>,
    pr_action_tasks: BTreeMap<PrRef, Task<()>>,
    removal_tasks: BTreeMap<PrRef, Task<()>>,
    sweep_task: Option<Task<()>>,
    sweep_pending: bool,
}

impl AppView {
    /// One opacity for every pulsing icon this frame, from a single start time.
    fn pulse(&self) -> f32 {
        const PERIOD_MS: u128 = 2400;
        let phase = (self.pulse_epoch.elapsed().as_millis() % PERIOD_MS) as f32 / PERIOD_MS as f32;
        pulsating_between(0.35, 1.0)(phase)
    }
    fn effective_parents(&self, prs: &[PrRow]) -> BTreeMap<PrRef, PrRef> {
        model::effective_parents(model::stack_parents(prs), &self.config.stack_links)
    }

    fn record_stack_links(&mut self, prs: &[PrRow]) {
        model::prune_stack_links(
            &mut self.config.stack_links,
            &self.config.active,
            &self.config.history,
        );
        for (child, parent) in model::stack_parents(prs) {
            if self.config.active.contains_key(&child) || self.config.active.contains_key(&parent) {
                self.config.stack_links.insert(child, parent);
            }
        }
    }

    fn prune_stack_links(&mut self) {
        model::prune_stack_links(
            &mut self.config.stack_links,
            &self.config.active,
            &self.config.history,
        );
    }
    pub(crate) fn open_settings(&mut self, cx: &mut Context<Self>) {
        self.page = Page::Settings(SettingsPage::App);
        cx.notify();
    }
    pub(crate) fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.page = Page::PullRequests;
        window.focus(&self.search.focus_handle(cx), cx);
        cx.notify();
    }

    pub fn new(
        data: PathBuf,
        mut config: Config,
        env: ChildEnv,
        registry: Registry,
        locks: RepoLocks,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        sitter::sweep_trash(&data);
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
        let error = store::write_prompts(&data, &config.prompts)
            .err()
            .map(|e| format!("Could not write prompts: {e:#}"))
            .or(error);
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
            cx.observe_window_activation(window, |this: &mut Self, window, cx| {
                if window.is_window_active()
                    && this.page == Page::Settings(SettingsPage::Status)
                    && !matches!(this.setup, Load::Loading)
                {
                    this.load_setup(window, cx);
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
                            let title =
                                this.titles.get(&event.pr).cloned().unwrap_or_else(|| {
                                    format!("Pull request #{}", event.pr.number)
                                });
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
                            this.prune_stack_links();
                            if *outcome == Outcome::Merged
                                && (!this.config.app.auto_dismiss_merged
                                    || this.config.app.clear_merged_after.is_zero())
                            {
                                this.remove_worktree(event.pr.clone(), window, cx);
                            }
                            if *outcome == Outcome::Merged && this.config.app.hides_merged_filter()
                            {
                                this.states.insert(event.pr.clone(), event.state);
                                this.dismiss(event.pr, window, cx);
                                return;
                            }
                            this.save();
                        }
                        if let SitState::Running {
                            last_line: Some(line),
                            ..
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
        // Keeps countdowns and step durations current between output lines.
        let clock = cx.spawn_in(window, async move |view, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(30))
                    .await;
                if view
                    .update_in(cx, |this, window, cx| {
                        this.dismiss_merged_history(window, cx);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let auto_refresh = cx.spawn_in(window, async move |view, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(60))
                    .await;
                if view
                    .update_in(cx, |this, window, cx| {
                        if !this.loading_prs {
                            this.load_prs(window, cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        // macOS does not announce notification-setting changes, so the Status page polls them.
        let permission_watch = cx.spawn_in(window, async move |view, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let Ok(watching) = view.update(cx, |this, _| {
                    this.page == Page::Settings(SettingsPage::Status)
                        && matches!(this.setup, Load::Ready(_))
                }) else {
                    break;
                };
                if !watching {
                    continue;
                }
                let state = cx
                    .background_spawn(async { setup::recheck_notifications() })
                    .await;
                if view
                    .update(cx, |this, cx| {
                        if let Load::Ready(results) = &mut this.setup
                            && let Some((_, current)) = results
                                .iter_mut()
                                .find(|(spec, _)| spec.id == "notifications")
                            && *current != state
                        {
                            *current = state;
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let titles = config
            .history
            .iter()
            .filter(|(_, past)| !past.title.starts_with("Pull request #"))
            .map(|(pr, past)| (pr.clone(), past.title.clone()))
            .collect();
        let defaults = config.app.defaults.clone();
        let app_inputs = Self::duration_inputs(
            DurationTarget::App,
            [
                Some(defaults.timeout),
                Some(defaults.draft_timeout),
                Some(defaults.interval),
            ],
            None,
            window,
            cx,
        );
        let clear_merged_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(model::format_duration(config.app.clear_merged_after))
        });
        let clear_subscription = cx.subscribe_in(
            &clear_merged_input,
            window,
            |this: &mut Self, input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::Blur | InputEvent::PressEnter { .. }) {
                    let value = input.read(cx).value().to_string();
                    this.update_clear_merged_after(&value, window, cx);
                    cx.notify();
                }
            },
        );
        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search by number or title"));
        let search_subscription = cx.subscribe(&search, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
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
            setup: Load::Loading,
            models: BTreeMap::new(),
            titles,
            dismissed_merged: BTreeSet::new(),
            closed_prs: BTreeMap::new(),
            org_select,
            org_filter: None,
            avatars: BTreeMap::new(),
            scope: RepoScope::All,
            watch_filter: WatchFilter::default(),
            state_filter: StateFilter::default(),
            dashboard_filter,
            search,
            selected_filter: None,
            app_inputs,
            prompt_editors: Load::Loading,
            prompt_task: None,
            clear_merged_input,
            clear_merged_error: None,
            inputs: BTreeMap::new(),
            states: BTreeMap::new(),
            gates: BTreeMap::new(),
            handles: HashMap::new(),
            event_tx,
            error,
            refresh_error: None,
            loading_repos: false,
            loading_prs: false,
            _subscriptions: {
                let mut subscriptions = subscriptions;
                subscriptions.extend([clear_subscription, search_subscription]);
                subscriptions
            },
            _events: events,
            _clock: clock,
            _auto_refresh: auto_refresh,
            _permission_watch: permission_watch,
            pulse_epoch: Instant::now(),
            repo_task: None,
            avatar_task: None,
            pr_task: None,
            setup_task: None,
            model_tasks: BTreeMap::new(),
            pr_action_tasks: BTreeMap::new(),
            removal_tasks: BTreeMap::new(),
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
        this.load_prompt_defaults(window, cx);
        for (pr, sit) in this.config.active.clone() {
            this.resume(pr, sit);
        }
        this.dismiss_merged_history(window, cx);
        this.update_filter(window, cx);
        this.refresh(window, cx);
        this.load_setup(window, cx);
        this
    }
    fn save(&mut self) -> bool {
        match store::write_prompts(&self.data, &self.config.prompts)
            .and_then(|()| store::save(&self.data, &self.config))
        {
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
    fn load_models(&mut self, kind: AgentKind, window: &mut Window, cx: &mut Context<Self>) {
        if self.models.contains_key(&kind) {
            return;
        }
        self.models.insert(kind, Load::Loading);
        let env = self.env.clone();
        let task = cx.spawn_in(window, async move |view, cx| {
            let result = cx
                .background_spawn(async move {
                    models::list_models(kind, &env).map_err(|error| format!("{error:#}"))
                })
                .await;
            let _ = view.update_in(cx, |this, _, cx| {
                this.models.insert(
                    kind,
                    match result {
                        Ok(models) => Load::Ready(models),
                        Err(error) => Load::Error(error),
                    },
                );
                this.model_tasks.remove(&kind);
                cx.notify();
            });
        });
        self.model_tasks.insert(kind, task);
    }
    fn load_prompt_defaults(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let env = self.env.clone();
        self.prompt_task = Some(cx.spawn_in(window, async move |view, cx| {
            let result = cx
                .background_spawn(async move {
                    let result = (|| -> anyhow::Result<PromptDefaults> {
                        use anyhow::{Context as _, bail};
                        let cli = std::env::current_exe()?
                            .parent()
                            .context("executable has no parent")?
                            .join("baby-sitter");
                        let output = env
                            .command(cli)
                            .arg("--print-default-prompts")
                            .output()
                            .context("start baby-sitter")?;
                        if !output.status.success() {
                            bail!(
                                "baby-sitter exited {}: {}",
                                output.status,
                                String::from_utf8_lossy(&output.stderr).trim()
                            );
                        }
                        serde_json::from_slice(&output.stdout).context("parse default prompts")
                    })();
                    result.map_err(|error| format!("{error:#}"))
                })
                .await;
            let _ = view.update_in(cx, |this, window, cx| {
                this.prompt_editors = match result {
                    Err(error) => Load::Error(error),
                    Ok(defaults) => {
                        let defaults = [defaults.review, defaults.ci, defaults.conflicts];
                        let editors = PromptKind::ALL
                            .into_iter()
                            .zip(defaults)
                            .map(|(kind, default)| {
                                let value = kind
                                    .value(&mut this.config.prompts)
                                    .text
                                    .clone()
                                    .unwrap_or_else(|| default.clone());
                                let input = cx
                                    .new(|cx| TextareaState::new(window, cx).default_value(value));
                                let subscription = cx.subscribe(
                                    &input,
                                    |this: &mut Self, _, event: &InputEvent, cx| {
                                        if matches!(event, InputEvent::Change) {
                                            let _ = this;
                                            cx.notify();
                                        }
                                    },
                                );
                                PromptEditor {
                                    input,
                                    default,
                                    error: None,
                                    _subscription: subscription,
                                }
                            })
                            .collect();
                        Load::Ready(editors)
                    }
                };
                cx.notify();
            });
        }));
    }

    fn save_prompt(
        &mut self,
        kind: PromptKind,
        reset: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Load::Ready(editors) = &mut self.prompt_editors else {
            return;
        };
        let editor = &mut editors[kind.index()];
        let text = if reset {
            editor.default.clone()
        } else {
            editor.input.read(cx).value().to_string()
        };
        if text.trim().is_empty() {
            editor.error = Some("A prompt can't be empty. Use Reset to default.".into());
            cx.notify();
            return;
        }
        editor.error = None;
        let value = (!reset).then_some(text.clone());
        let previous = std::mem::replace(&mut kind.value(&mut self.config.prompts).text, value);
        if self.save() {
            if reset && let Load::Ready(editors) = &self.prompt_editors {
                editors[kind.index()]
                    .input
                    .update(cx, |input, cx| input.set_value(text, window, cx));
            }
        } else {
            kind.value(&mut self.config.prompts).text = previous;
            if let Err(error) = store::write_prompts(&self.data, &self.config.prompts) {
                self.error = Some(format!("Could not restore prompts: {error:#}"));
            }
        }
        cx.notify();
    }

    fn toggle_prompt(
        &mut self,
        kind: PromptKind,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prompt = kind.value(&mut self.config.prompts);
        let previous = prompt.enabled;
        prompt.enabled = enabled;
        if !self.save() {
            kind.value(&mut self.config.prompts).enabled = previous;
            if let Err(error) = store::write_prompts(&self.data, &self.config.prompts) {
                self.error = Some(format!("Could not restore prompts: {error:#}"));
            }
        } else if enabled {
            let prompt = kind.value(&mut self.config.prompts);
            if let Load::Ready(editors) = &mut self.prompt_editors {
                let editor = &mut editors[kind.index()];
                let text = prompt
                    .text
                    .clone()
                    .unwrap_or_else(|| editor.default.clone());
                editor.error = None;
                editor
                    .input
                    .update(cx, |input, cx| input.set_value(text, window, cx));
            }
        }
        cx.notify();
    }

    fn duration_inputs(
        target: DurationTarget,
        values: [Option<Duration>; 3],
        defaults: Option<Defaults>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> RepoInputs {
        fn make_input(
            value: Option<Duration>,
            default: Option<Duration>,
            window: &mut Window,
            cx: &mut Context<AppView>,
        ) -> Entity<InputState> {
            let value = value.map(model::format_duration).unwrap_or_default();
            let placeholder = default
                .map(|duration| format!("{} (app default)", model::format_duration(duration)))
                .unwrap_or_default();
            cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(value)
                    .placeholder(placeholder)
            })
        }

        let defaults = defaults
            .map(|defaults| {
                [
                    Some(defaults.timeout),
                    Some(defaults.draft_timeout),
                    Some(defaults.interval),
                ]
            })
            .unwrap_or([None; 3]);
        let timeout = make_input(values[0], defaults[0], window, cx);
        let draft_timeout = make_input(values[1], defaults[1], window, cx);
        let interval = make_input(values[2], defaults[2], window, cx);
        let mut subscriptions = Vec::new();
        for (input, field) in [
            (&timeout, Field::Timeout),
            (&draft_timeout, Field::DraftTimeout),
            (&interval, Field::Interval),
        ] {
            let target = target.clone();
            subscriptions.push(cx.subscribe_in(
                input,
                window,
                move |this: &mut Self, input, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::Blur | InputEvent::PressEnter { .. }) {
                        let value = input.read(cx).value().to_string();
                        this.update_duration(&target, field, &value, window, cx);
                        cx.notify();
                    }
                },
            ));
        }
        RepoInputs {
            timeout,
            draft_timeout,
            interval,
            setup_command: None,
            timeout_error: None,
            draft_timeout_error: None,
            interval_error: None,
            _subscriptions: subscriptions,
        }
    }

    fn add_inputs(
        &mut self,
        repo: RepoName,
        settings: &RepoSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut inputs = Self::duration_inputs(
            DurationTarget::Repo(repo.clone()),
            [settings.timeout, settings.draft_timeout, settings.interval],
            Some(self.config.app.defaults.clone()),
            window,
            cx,
        );
        let setup_command = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(settings.setup_command.clone().unwrap_or_default())
                .placeholder("pnpm install")
        });
        let target = repo.clone();
        inputs._subscriptions.push(cx.subscribe_in(
            &setup_command,
            window,
            move |this: &mut Self, input, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Blur | InputEvent::PressEnter { .. }) {
                    let value = input.read(cx).value().to_string();
                    this.update_setup_command(&target, &value);
                    cx.notify();
                }
            },
        ));
        inputs.setup_command = Some(setup_command);
        self.inputs.insert(repo, inputs);
    }

    fn update_setup_command(&mut self, repo: &RepoName, text: &str) {
        let value = model::normalize_setup_command(text);
        let previous = self.config.repos.get_mut(repo).and_then(|settings| {
            (settings.setup_command.as_deref() != value)
                .then(|| std::mem::replace(&mut settings.setup_command, value.map(str::to_owned)))
        });
        if let Some(previous) = previous
            && !self.save()
            && let Some(settings) = self.config.repos.get_mut(repo)
        {
            settings.setup_command = previous;
        }
    }

    fn update_duration(
        &mut self,
        target: &DurationTarget,
        field: Field,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = text.trim();
        let parsed = if text.is_empty() && matches!(target, DurationTarget::Repo(_)) {
            Ok(None)
        } else {
            model::parse_duration(text).and_then(|duration| {
                if matches!(field, Field::Interval) && duration.is_zero() {
                    Err("Interval must be greater than 0".into())
                } else {
                    Ok(Some(duration))
                }
            })
        };
        let error = match parsed {
            Ok(value) => {
                match target {
                    DurationTarget::App => {
                        let duration = value.expect("app defaults cannot be empty");
                        let current = field.default_value(&mut self.config.app.defaults);
                        let previous = std::mem::replace(current, duration);
                        if previous != duration {
                            if self.save() {
                                let placeholder =
                                    format!("{} (app default)", model::format_duration(duration));
                                for inputs in self.inputs.values() {
                                    inputs.input(field).update(cx, |input, cx| {
                                        input.set_placeholder(placeholder.clone(), window, cx);
                                    });
                                }
                            } else {
                                *field.default_value(&mut self.config.app.defaults) = previous;
                            }
                        }
                    }
                    DurationTarget::Repo(repo) => {
                        let previous = self.config.repos.get_mut(repo).and_then(|settings| {
                            let current = field.repo_value(settings);
                            (*current != value).then(|| std::mem::replace(current, value))
                        });
                        if let Some(previous) = previous
                            && !self.save()
                            && let Some(settings) = self.config.repos.get_mut(repo)
                        {
                            *field.repo_value(settings) = previous;
                        }
                    }
                }
                None
            }
            Err(error) => Some(error),
        };
        let inputs = match target {
            DurationTarget::App => Some(&mut self.app_inputs),
            DurationTarget::Repo(repo) => self.inputs.get_mut(repo),
        };
        if let Some(inputs) = inputs {
            *inputs.error_mut(field) = error;
        }
    }
    fn update_clear_merged_after(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = text.trim();
        let parsed = if text.is_empty() {
            Ok(Duration::ZERO)
        } else {
            model::parse_duration(text)
        };
        self.clear_merged_error = match parsed {
            Ok(duration) => {
                let previous = self.config.app.clear_merged_after;
                if previous != duration {
                    self.config.app.clear_merged_after = duration;
                    if self.save() {
                        if self.config.app.hides_merged_filter()
                            && self.state_filter == StateFilter::Merged
                        {
                            self.state_filter = StateFilter::All;
                        }
                        self.dismiss_merged_history(window, cx);
                    } else {
                        self.config.app.clear_merged_after = previous;
                    }
                }
                None
            }
            Err(error) => Some(error),
        };
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
    pub(crate) fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading_repos || self.loading_prs {
            return;
        }
        self.load_repos(window, cx);
        self.load_prs(window, cx);
    }
    fn load_repos(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading_repos {
            return;
        }
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
                        this.titles
                            .extend(prs.iter().map(|row| (row.pr.clone(), row.title.clone())));
                        let previous_links = this.config.stack_links.clone();
                        this.record_stack_links(&prs);
                        if this.config.stack_links != previous_links {
                            this.save();
                        }
                        this.closed_prs.retain(|pr, state| {
                            let found = prs.iter().any(|row| &row.pr == pr);
                            match (*state, found) {
                                (ClosedPr::AwaitingSearch, false) => {
                                    *state = ClosedPr::Confirmed;
                                    true
                                }
                                (ClosedPr::Confirmed, true) => false,
                                _ => true,
                            }
                        });
                        prs.retain(|row| {
                            !this.dismissed_merged.contains(&row.pr)
                                && !this.closed_prs.contains_key(&row.pr)
                        });
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
    fn run_pr_action(
        &mut self,
        pr: PrRef,
        action: gh::GhPrAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pr_action_tasks.contains_key(&pr) {
            return;
        }
        let env = self.env.clone();
        let task_pr = pr.clone();
        let task = cx.spawn_in(window, async move |view, cx| {
            let request_pr = task_pr.clone();
            let result = cx
                .background_spawn(async move { gh::pr_action(&env, &request_pr, action) })
                .await;
            let _ = view.update_in(cx, |this, window, cx| {
                this.pr_action_tasks.remove(&task_pr);
                match result {
                    Ok(()) => {
                        if let Load::Ready(rows) = &mut this.prs {
                            match action {
                                gh::GhPrAction::Ready | gh::GhPrAction::Draft => {
                                    if let Some(row) = rows.iter_mut().find(|row| row.pr == task_pr)
                                    {
                                        row.state = if matches!(action, gh::GhPrAction::Draft) {
                                            PrState::Draft
                                        } else {
                                            PrState::Ready
                                        };
                                    }
                                }
                                gh::GhPrAction::Close => {
                                    rows.retain(|row| row.pr != task_pr);
                                    this.closed_prs
                                        .insert(task_pr.clone(), ClosedPr::AwaitingSearch);
                                }
                            }
                        }
                        let message = match action {
                            gh::GhPrAction::Ready => {
                                format!("Marked #{} ready for review", task_pr.number)
                            }
                            gh::GhPrAction::Draft => {
                                format!("Converted #{} to draft", task_pr.number)
                            }
                            gh::GhPrAction::Close => format!("Closed #{}", task_pr.number),
                        };
                        window.push_notification(Notification::success(message), cx);
                        this.load_prs(window, cx);
                    }
                    Err(error) => {
                        let verb = match action {
                            gh::GhPrAction::Ready => "mark",
                            gh::GhPrAction::Draft => "convert",
                            gh::GhPrAction::Close => "close",
                        };
                        this.error = Some(format!("Could not {verb} #{}: {error}", task_pr.number));
                    }
                }
                cx.notify();
            });
        });
        self.pr_action_tasks.insert(pr, task);
        cx.notify();
    }

    fn ask_close(&mut self, pr: PrRef, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Close #{}? You can reopen it on GitHub.", pr.number),
            None,
            &["Close", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |view, cx| {
            if answer.await.ok() == Some(0) {
                let _ = view.update_in(cx, |this, window, cx| {
                    this.run_pr_action(pr, gh::GhPrAction::Close, window, cx);
                });
            }
        })
        .detach();
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
    fn resume(&mut self, pr: PrRef, sit: ActiveSit) {
        if self.handles.contains_key(&pr) {
            return;
        }
        let Some(settings) = self.config.repos.get(&pr.repo) else {
            return;
        };
        let settings = settings.resolve(&self.config.app.defaults);
        self.states.insert(pr.clone(), SitState::Preparing);
        self.gates.remove(&pr);
        let handle = sitter::spawn(
            pr.clone(),
            settings,
            sit,
            self.data.clone(),
            self.env.clone(),
            self.registry.clone(),
            self.locks.clone(),
            self.event_tx.clone(),
        );
        self.handles.insert(pr, handle);
    }
    fn start(&mut self, pr: PrRef, mode: StackMode, window: &mut Window, cx: &mut Context<Self>) {
        if self.removal_tasks.contains_key(&pr)
            || self.states.get(&pr).is_some_and(SitState::is_active)
        {
            return;
        }
        let Some(settings) = self.config.repos.get(&pr.repo) else {
            return;
        };
        let settings = settings.resolve(&self.config.app.defaults);
        let timeout = if matches!(&self.prs, Load::Ready(rows) if rows.iter().any(|row| row.pr == pr && row.state == PrState::Draft))
        {
            settings.draft_timeout
        } else {
            settings.timeout
        };
        let Some(deadline) = model::unix_now().checked_add(timeout.as_secs()) else {
            self.error = Some("Timeout is too large".into());
            cx.notify();
            return;
        };
        let previous = self.config.history.remove(&pr);
        let sit = ActiveSit {
            deadline,
            stack: mode,
        };
        self.config.active.insert(pr.clone(), sit);
        let previous_links = self.config.stack_links.clone();
        if let Load::Ready(rows) = &self.prs {
            let links = model::stack_parents(rows);
            for (child, parent) in links {
                if self.config.active.contains_key(&child)
                    || self.config.active.contains_key(&parent)
                {
                    self.config.stack_links.insert(child, parent);
                }
            }
        }
        if !self.save() {
            self.config.active.remove(&pr);
            self.config.stack_links = previous_links;
            if let Some(past) = previous {
                self.config.history.insert(pr, past);
            }
            cx.notify();
            return;
        }
        self.resume(pr, sit);
        self.update_filter(window, cx);
        cx.notify();
    }
    fn start_stack(
        &mut self,
        pr: &PrRef,
        mode: StackMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Load::Ready(rows) = &self.prs else {
            return;
        };
        let parents = self.effective_parents(rows);
        let tree = model::stack_tree(pr, &parents);
        if let Some(other) = tree
            .iter()
            .filter_map(|member| self.config.active.get(member))
            .find(|sit| sit.stack != mode)
        {
            let message = match other.stack {
                StackMode::BottomUp => "This stack is already babysat bottom up",
                StackMode::TopDown => "This stack is already babysat top down",
            };
            window.push_notification(Notification::error(message), cx);
            return;
        }
        let members = match mode {
            StackMode::BottomUp => model::stack_of(pr, &parents),
            StackMode::TopDown => tree,
        };
        let stack: Vec<_> = members
            .into_iter()
            .filter(|pr| {
                rows.iter().any(|row| {
                    row.pr == *pr
                        && matches!(row.state, PrState::Draft | PrState::Ready | PrState::Queued)
                })
            })
            .collect();
        let mut count = 0;
        for pr in stack {
            if !self.config.repos.contains_key(&pr.repo)
                || self.states.get(&pr).is_some_and(SitState::is_active)
            {
                continue;
            }
            self.start(pr.clone(), mode, window, cx);
            if self.states.get(&pr).is_some_and(SitState::is_active) {
                count += 1;
            }
        }
        window.push_notification(
            Notification::success(format!("Started {count} babysits in the stack")),
            cx,
        );
    }
    fn remove_worktree(&mut self, pr: PrRef, window: &mut Window, cx: &mut Context<Self>) {
        if self.removal_tasks.contains_key(&pr) {
            return;
        }
        let env = self.env.clone();
        let data = self.data.clone();
        let locks = self.locks.clone();
        let task_pr = pr.clone();
        let task = cx.spawn_in(window, async move |view, cx| {
            let removal_pr = task_pr.clone();
            let result = cx
                .background_spawn(async move {
                    sitter::remove_worktree(&env, &data, &locks, &removal_pr)
                })
                .await;
            let _ = view.update_in(cx, |this, _, cx| {
                this.removal_tasks.remove(&task_pr);
                if let Err(error) = result {
                    this.error = Some(format!(
                        "Could not remove the worktree for #{}: {error:#}",
                        task_pr.number
                    ));
                }
                cx.notify();
            });
        });
        self.removal_tasks.insert(pr, task);
    }

    fn dismiss(&mut self, pr: PrRef, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let merged = match self.states.get(&pr) {
            Some(SitState::Finished(outcome)) => *outcome == Outcome::Merged,
            _ => return true,
        };
        let previous = self.config.history.remove(&pr);
        let previous_links = self.config.stack_links.clone();
        self.prune_stack_links();
        if !self.save() {
            self.config.stack_links = previous_links;
            if let Some(past) = previous {
                self.config.history.insert(pr, past);
            }
            cx.notify();
            return false;
        }
        self.remove_worktree(pr.clone(), window, cx);
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
        true
    }
    fn dismiss_merged_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let due = model::merged_due(&self.config.history, &self.config.app, model::unix_now());
        for pr in due {
            if !self.dismiss(pr, window, cx) {
                break;
            }
        }
    }

    fn set_auto_dismiss_merged(
        &mut self,
        checked: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous = self.config.app.auto_dismiss_merged;
        if previous == checked {
            return;
        }
        self.config.app.auto_dismiss_merged = checked;
        if !self.save() {
            self.config.app.auto_dismiss_merged = previous;
        } else if checked {
            if self.config.app.hides_merged_filter() && self.state_filter == StateFilter::Merged {
                self.state_filter = StateFilter::All;
            }
            self.dismiss_merged_history(window, cx);
        }
        cx.notify();
    }
    fn cancel(&mut self, pr: PrRef, cx: &mut Context<Self>) {
        let Some(handle) = self.handles.get(&pr).cloned() else {
            return;
        };
        let old = self.config.active.remove(&pr);
        let previous_links = self.config.stack_links.clone();
        self.prune_stack_links();
        if !self.save() {
            self.config.stack_links = previous_links;
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
            Some(SitState::Running { last_line: Some(line), .. })
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
    fn ask_cancel_stack(
        &mut self,
        members: Vec<PrRef>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let acting = members.iter().any(|pr| {
            matches!(
                self.states.get(pr),
                Some(SitState::Running { last_line: Some(line), .. })
                    if model::parse_pass(line).is_some_and(|pass| pass.kind == VerdictKind::Act)
            )
        });
        if !acting {
            for pr in members {
                self.cancel(pr, cx);
            }
            return;
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            "Stop babysitting this stack?",
            Some("A change is in progress and may be half applied. Inspect the worktree before restarting."),
            &["Stop babysitting", "Keep running"],
            cx,
        );
        cx.spawn(async move |view, cx| {
            if answer.await.ok() == Some(0) {
                let _ = view.update(cx, |this, cx| {
                    for pr in members {
                        this.cancel(pr, cx);
                    }
                });
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
            Some(SitState::Running {
                last_line: None, ..
            }) => reading(Waiting, "Starting", Some("Taking the first snapshot")),
            Some(SitState::Running {
                last_line: Some(line),
                since,
            }) => match model::parse_pass(line) {
                Some(pass) => match pass.kind {
                    VerdictKind::Done => merged,
                    VerdictKind::Wait | VerdictKind::Act => {
                        let verb = if pass.kind == VerdictKind::Wait {
                            "Waiting"
                        } else {
                            "Working"
                        };
                        let mut result = reading(Waiting, verb, Some(pass.detail));
                        let seconds = since.elapsed().as_secs();
                        if seconds >= 60 {
                            result.detail =
                                Some(format!("{} · {}", pass.detail, format_minutes(seconds)));
                        }
                        result
                    }
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
    fn repo_row(&self, repo: &RepoName, last: bool, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.config.repos.contains_key(repo);
        let count = self.active_count(repo);
        let switch_repo = repo.clone();
        let settings_repo = repo.clone();
        let github_url = repo.url();
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
                "Turn on the repository to configure it"
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.page = Page::Settings(SettingsPage::Repo(settings_repo.clone()));
                cx.notify();
            }));
        div()
            .h(px(REPO_ROW_HEIGHT))
            .w_full()
            .px(px(ROW_INSET))
            .when(!last, |row| row.border_b_1().border_color(divider()))
            .flex()
            .items_center()
            .gap(px(20.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .child(
                        div()
                            .min_w_0()
                            .text_ellipsis()
                            .text_color(ink())
                            .child(repo.name.clone()),
                    )
                    .child(
                        icon_button(
                            SharedString::from(format!("github-{repo}")),
                            IconName::Github,
                            "Open on GitHub",
                            px(24.),
                        )
                        .flex_shrink_0()
                        .on_click(move |_, _, cx| {
                            cx.stop_propagation();
                            open_url(github_url.clone());
                        }),
                    ),
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
    fn settings_header(
        &self,
        title: &'static str,
        detail: Option<&'static str>,
        cx: &mut Context<Self>,
    ) -> Div {
        div()
            .flex_shrink_0()
            .child(
                div()
                    .h(px(50.))
                    .px(px(ROW_INSET))
                    .flex()
                    .items_center()
                    .child(
                        Button::new("back-home")
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .child(
                                        div()
                                            .relative()
                                            .top(px(-1.))
                                            .child(Icon::new(IconName::ChevronLeft).size(px(14.))),
                                    )
                                    .child("Home"),
                            )
                            .ghost()
                            .ml(px(-8.))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.page = Page::PullRequests;
                                cx.notify();
                            })),
                    ),
            )
            .child(header(title, detail).pt(px(2.)))
    }

    fn render_repos(&self, cx: &mut Context<Self>) -> (Div, Div) {
        let repo_counts = model::repo_counts(
            match &self.repos {
                Load::Ready(repos) => repos,
                _ => &[],
            },
            &self.config.repos,
            self.org_filter.as_deref(),
        );
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
                                    .filter_map(|index| items.get(index).map(|item| (index, item)))
                                    .map(|(index, item)| {
                                        let last = index + 1 == count;
                                        match item {
                                            RepoListItem::Org {
                                                name,
                                                watched,
                                                total,
                                            } => div()
                                                .h(px(REPO_ROW_HEIGHT))
                                                .w_full()
                                                .px(px(ROW_INSET))
                                                .bg(panel())
                                                .when(!last, |row| {
                                                    row.border_b_1().border_color(divider())
                                                })
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
                                                        .child(format!(
                                                            "{watched} of {total} watched"
                                                        )),
                                                )
                                                .into_any_element(),
                                            RepoListItem::Repo(repo) => {
                                                this.repo_row(repo, last, cx)
                                            }
                                        }
                                    })
                                    .collect::<Vec<_>>()
                            }),
                        )
                        .size_full(),
                    )
                }
            }
        };
        let scope = ButtonGroup::new("repo-scope")
            .with_size(Size::Medium)
            .outline()
            .children(
                RepoScope::ALL
                    .into_iter()
                    .zip(repo_counts)
                    .map(|(scope, (_, count))| {
                        let (id, label) = match scope {
                            RepoScope::All => ("scope-all", "All"),
                            RepoScope::Watched => ("scope-watched", "Watched"),
                        };
                        let selected = self.scope == scope;
                        Button::new(id).selected(selected).child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .text_color(if selected { accent() } else { muted() })
                                .when(selected, |d| d.font_semibold())
                                .child(label)
                                .when(count > 0, |d| d.child(count.to_string())),
                        )
                    }),
            )
            .on_click(cx.listener(|this, selected: &Vec<usize>, _, cx| {
                if let Some(scope) = selected
                    .first()
                    .and_then(|index| RepoScope::ALL.get(*index))
                {
                    this.scope = *scope;
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
            .justify_between()
            .gap(px(12.))
            .child(scope)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(header_picker(
                        &self.org_select,
                        "Filter by organization",
                        "All organizations",
                        Size::Medium,
                    ))
                    .child(
                        round_button_with(
                            "refresh-repos".into(),
                            refresh_icon(self.loading_repos),
                            "Refresh repositories",
                            raised(),
                            ink(),
                            cx,
                        )
                        .size(px(32.))
                        .rounded(px(16.))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.load_repos(window, cx);
                            cx.notify();
                        })),
                    ),
            );
        let body = div()
            .size_full()
            .flex()
            .flex_col()
            .child(filters)
            .child(div().flex_1().min_h_0().child(list));
        let header = self.settings_header(
            "Repositories",
            Some("Babysits run only in the repositories you watch."),
            cx,
        );
        (header, body)
    }
    fn render_prompts(&self, cx: &mut Context<Self>) -> (Div, Div) {
        let header = self
            .settings_header("Prompts", None, cx)
            .border_b_1()
            .border_color(divider());
        let mut content = div().px(px(ROW_INSET)).py(px(28.)).w_full().max_w(px(800.))
            .flex().flex_col().gap(px(24.))
            .child(div().text_size(px(13.)).line_height(relative(1.4)).text_color(muted())
                .child("The coding agent gets these instructions, followed by the review threads, failing checks, or conflicted files. Baby Sitter applies its safety checks regardless of the instructions. It never commits CI configuration changes and checks conflict resolutions before pushing."))
            .child(div().text_size(px(12.)).text_color(muted())
                .child("Changes apply to running babysits the next time they call the coding agent."));
        match &self.prompt_editors {
            Load::Loading => content = content.child("Loading prompts…"),
            Load::Error(error) => {
                content = content.child(
                    div()
                        .text_color(danger())
                        .child(format!("Could not load default prompts: {error}")),
                )
            }
            Load::Ready(editors) => {
                for kind in PromptKind::ALL {
                    let editor = &editors[kind.index()];
                    let prompt = match kind {
                        PromptKind::Review => &self.config.prompts.review,
                        PromptKind::Ci => &self.config.prompts.ci,
                        PromptKind::Conflicts => &self.config.prompts.conflicts,
                    };
                    let current = editor.input.read(cx).value();
                    let saved_text = prompt.text.as_deref().unwrap_or(&editor.default);
                    let dirty = current.as_ref() != saved_text;
                    let id = kind.index();
                    let switch = Switch::new(("custom-prompt", id))
                        .accessibility_label(kind.switch_label())
                        .checked(prompt.enabled)
                        .on_click(cx.listener(move |this, checked, window, cx| {
                            this.toggle_prompt(kind, *checked, window, cx);
                        }));
                    content = content.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(8.))
                            .child(
                                div()
                                    .font_semibold()
                                    .text_size(px(16.))
                                    .text_color(ink())
                                    .child(kind.title()),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(16.))
                                    .child(
                                        div()
                                            .flex_1()
                                            .font_semibold()
                                            .text_size(px(14.))
                                            .text_color(ink())
                                            .child(kind.switch_label()),
                                    )
                                    .child(switch),
                            )
                            .when(!prompt.enabled, |d| {
                                d.child(
                                    div()
                                        .text_size(px(12.))
                                        .text_color(muted())
                                        .child("Using the default prompt."),
                                )
                            })
                            .when(prompt.enabled, |d| {
                                d.when(prompt.text.is_none(), |d| {
                                    d.child(
                                        div()
                                            .text_size(px(12.))
                                            .text_color(muted())
                                            .child("Save to use this prompt."),
                                    )
                                })
                                .child(
                                    Textarea::new(&editor.input)
                                        .w_full()
                                        .h(px(200.))
                                        .aria_label(kind.title()),
                                )
                                .when_some(editor.error.clone(), |d, error| {
                                    d.child(
                                        div().text_size(px(12.)).text_color(danger()).child(error),
                                    )
                                })
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(8.))
                                        .child(
                                            Button::new(("save-prompt", id))
                                                .label("Save")
                                                .disabled(!dirty && prompt.text.is_some())
                                                .on_click(cx.listener(
                                                    move |this, _, window, cx| {
                                                        this.save_prompt(kind, false, window, cx)
                                                    },
                                                )),
                                        )
                                        .when(dirty, |d| {
                                            d.child(
                                                div()
                                                    .text_size(px(12.))
                                                    .text_color(muted())
                                                    .child("Unsaved changes"),
                                            )
                                        })
                                        .child(
                                            Button::new(("reset-prompt", id))
                                                .label("Reset to default")
                                                .outline()
                                                .disabled(prompt.text.is_none() && !dirty)
                                                .on_click(cx.listener(
                                                    move |this, _, window, cx| {
                                                        this.save_prompt(kind, true, window, cx)
                                                    },
                                                )),
                                        ),
                                )
                            }),
                    );
                }
            }
        }
        let body = div().size_full().child(
            div()
                .id("prompts-settings-form")
                .size_full()
                .overflow_y_scroll()
                .child(content),
        );
        (header, body)
    }

    fn agent_fields(&self, target: DurationTarget, cx: &mut Context<Self>) -> Div {
        let selected = match &target {
            DurationTarget::App => Some(self.config.app.defaults.agent),
            DurationTarget::Repo(repo) => self
                .config
                .repos
                .get(repo)
                .and_then(|settings| settings.agent),
        };
        let label = selected.map_or("App default", AgentKind::label);
        let app_default = self.config.app.defaults.agent;
        let kind = selected.unwrap_or(app_default);
        let model = match &target {
            DurationTarget::App => self.config.app.defaults.model.as_ref(),
            DurationTarget::Repo(repo) => self
                .config
                .repos
                .get(repo)
                .and_then(|settings| settings.model.as_ref()),
        };
        let options = match self.models.get(&kind) {
            Some(Load::Ready(options)) => options.clone(),
            _ => Vec::new(),
        };
        let inherits_app_model = match &target {
            DurationTarget::App => false,
            DurationTarget::Repo(_) => selected.is_none(),
        };
        let unset_label = if inherits_app_model {
            "App default"
        } else {
            "Harness default"
        };
        let model_label = model.map_or_else(
            || unset_label.to_owned(),
            |value| {
                options
                    .iter()
                    .find(|option| option.value == *value)
                    .map_or_else(|| value.clone(), |option| option.label.clone())
            },
        );
        let status = match self.models.get(&kind) {
            None | Some(Load::Loading) => Some("Loading models…".to_owned()),
            Some(Load::Error(error)) => Some(error.lines().next().unwrap_or(error).to_owned()),
            Some(Load::Ready(options)) if options.is_empty() => Some(
                if kind == AgentKind::Pi {
                    "No models. Sign in with `pi auth login`."
                } else {
                    "No models found"
                }
                .to_owned(),
            ),
            Some(Load::Ready(_)) => None,
        };
        let model_target = target.clone();
        let model_view = cx.entity().downgrade();
        let model_picker = Button::new("model-picker")
            .child(harness_choice(kind, model_label).flex_1())
            .dropdown_caret(true)
            .outline()
            .w_full()
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu
                    .min_w(px(SETTINGS_INPUT_WIDTH))
                    .max_h(px(320.))
                    .scrollable(true);
                let choices = std::iter::once((None, unset_label.to_owned())).chain(
                    options
                        .clone()
                        .into_iter()
                        .map(|option| (Some(option.value), option.label)),
                );
                for (value, label) in choices {
                    let view = model_view.clone();
                    let target = model_target.clone();
                    menu = menu.item(PopupMenuItem::new(label).on_click(move |_, window, cx| {
                        let value = value.clone();
                        let _ = view.update(cx, |this, cx| {
                            let previous = match &target {
                                DurationTarget::App => {
                                    std::mem::replace(&mut this.config.app.defaults.model, value)
                                }
                                DurationTarget::Repo(repo) => {
                                    let Some(settings) = this.config.repos.get_mut(repo) else {
                                        return;
                                    };
                                    std::mem::replace(&mut settings.model, value)
                                }
                            };
                            if !this.save() {
                                match &target {
                                    DurationTarget::App => {
                                        this.config.app.defaults.model = previous
                                    }
                                    DurationTarget::Repo(repo) => {
                                        if let Some(settings) = this.config.repos.get_mut(repo) {
                                            settings.model = previous;
                                        }
                                    }
                                }
                            } else {
                                this.load_setup(window, cx);
                            }
                            cx.notify();
                        });
                    }));
                }
                if let Some(status) = status.clone() {
                    menu = menu.item(PopupMenuItem::new(status).disabled(true));
                }
                menu
            });
        let view = cx.entity().downgrade();
        let is_repo = matches!(target, DurationTarget::Repo(_));
        let picker = Button::new("coding-agent")
            .child(harness_choice(selected.unwrap_or(app_default), label).flex_1())
            .dropdown_caret(true)
            .outline()
            .w_full()
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu.min_w(px(SETTINGS_INPUT_WIDTH));
                let choices = std::iter::once(None)
                    .filter(|_| is_repo)
                    .chain(AgentKind::ALL.into_iter().map(Some));
                for choice in choices {
                    let view = view.clone();
                    let target = target.clone();
                    let label = choice.map_or("App default", AgentKind::label);
                    let shown = choice.unwrap_or(app_default);
                    menu = menu.item(
                        PopupMenuItem::element(move |_, _| harness_choice(shown, label)).on_click(
                            move |_, window, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    let previous = match &target {
                                        DurationTarget::App => Some(std::mem::replace(
                                            &mut this.config.app.defaults.agent,
                                            choice.expect("app agent"),
                                        )),
                                        DurationTarget::Repo(repo) => {
                                            let Some(settings) = this.config.repos.get_mut(repo)
                                            else {
                                                return;
                                            };
                                            std::mem::replace(&mut settings.agent, choice)
                                        }
                                    };
                                    if !this.save() {
                                        match &target {
                                            DurationTarget::App => {
                                                this.config.app.defaults.agent =
                                                    previous.expect("app agent")
                                            }
                                            DurationTarget::Repo(repo) => {
                                                if let Some(settings) =
                                                    this.config.repos.get_mut(repo)
                                                {
                                                    settings.agent = previous;
                                                }
                                            }
                                        }
                                    } else {
                                        this.load_models(choice.unwrap_or(app_default), window, cx);
                                        this.load_setup(window, cx);
                                    }
                                    cx.notify();
                                });
                            },
                        ),
                    );
                }
                menu
            });
        div()
            .flex()
            .flex_col()
            .gap(px(20.))
            .child(settings_field(
                Icon::empty().path("icons/bot.svg"),
                "Agent harness",
                picker,
                div()
                    .text_color(muted())
                    .child("Answers review threads, fixes CI, and resolves conflicts."),
            ))
            .child(settings_field(
                Icon::empty().path("icons/brain.svg"),
                "Model",
                model_picker,
                div()
                    .text_color(muted())
                    .child("Models the selected harness offers."),
            ))
    }

    fn render_app_settings(&self, cx: &mut Context<Self>) -> (Div, Div) {
        let header = self
            .settings_header("App", None, cx)
            .border_b_1()
            .border_color(divider());
        let switch = Switch::new("auto-dismiss-merged")
            .accessibility_label("Auto-dismiss merged PRs")
            .checked(self.config.app.auto_dismiss_merged)
            .on_click(cx.listener(|this, checked, window, cx| {
                this.set_auto_dismiss_merged(*checked, window, cx);
            }));
        let body = div().size_full().child(
            div()
                .id("app-settings-form")
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
                        .gap(px(28.))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(20.))
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap(px(4.))
                                        .child(
                                            div()
                                                .font_semibold()
                                                .text_size(px(16.))
                                                .text_color(ink())
                                                .child("Defaults"),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(13.))
                                                .text_color(muted())
                                                .child("Repositories use these values unless they set their own."),
                                        ),
                                )
                                .child(self.agent_fields(DurationTarget::App, cx))
                                .child(duration_fields(&self.app_inputs)),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(10.))
                                .child(
                                    div()
                                        .font_semibold()
                                        .text_size(px(16.))
                                        .text_color(ink())
                                        .child("Merged PRs"),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(16.))
                                        .child(
                                            div()
                                                .flex_1()
                                                .font_semibold()
                                                .text_size(px(14.))
                                                .text_color(ink())
                                                .child("Auto-dismiss merged PRs"),
                                        )
                                        .child(switch),
                                )
                                .child(
                                    div()
                                        .text_size(px(12.))
                                        .line_height(relative(1.4))
                                        .text_color(muted())
                                        .child("Remove a merged PR's row and worktree after the delay below."),
                                )
                                .child(duration_field(
                                    Icon::empty().path("icons/timer.svg"),
                                    "Clear after",
                                    "A merged PR stays listed for this long before Baby Sitter clears it. For example, 1h waits one hour. 0s clears it at once.",
                                    &self.clear_merged_input,
                                    &self.clear_merged_error,
                                    !self.config.app.auto_dismiss_merged,
                                )),
                        ),
                ),
        );
        (header, body)
    }

    fn render_repo_settings(&self, repo: &RepoName, cx: &mut Context<Self>) -> (Div, Div) {
        let inputs = self
            .inputs
            .get(repo)
            .expect("watched repositories have settings inputs");
        let github_url = repo.url();
        let header = div()
            .flex_shrink_0()
            .border_b_1()
            .border_color(divider())
            .child(
                div()
                    .h(px(50.))
                    .px(px(ROW_INSET))
                    .flex()
                    .items_center()
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
                    ),
            )
            .child(
                drag_area()
                    .px(px(ROW_INSET))
                    .pt(px(2.))
                    .pb(px(20.))
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(4.))
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
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation();
                                    })
                                    .child(
                                        icon_button(
                                            "github-repo-title".into(),
                                            IconName::Github,
                                            "Open on GitHub",
                                            px(24.),
                                        )
                                        .flex_shrink_0()
                                        .on_click(
                                            move |_, _, cx| {
                                                cx.stop_propagation();
                                                open_url(github_url.clone());
                                            },
                                        ),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(muted())
                            .child("These settings apply to babysits you start from now on."),
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
                        .child(self.agent_fields(DurationTarget::Repo(repo.clone()), cx))
                        .child(duration_fields(inputs).child(duration_field(
                            Icon::empty().path("icons/square-terminal.svg"),
                            "Setup command",
                            "Baby Sitter runs this command after creating a new worktree and before the babysit starts. Leave it empty to skip this step.",
                            inputs
                                .setup_command
                                .as_ref()
                                .expect("watched repositories have setup inputs"),
                            &None,
                            false,
                        ))),
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
            self.selected_filter.as_ref(),
        )
        .into_iter()
        .map(|mut row| {
            if let Cow::Owned(row) = &mut row
                && let Some(title) = self.titles.get(&row.pr)
            {
                row.title.clone_from(title);
            }
            row
        })
        .filter(|row| !self.closed_prs.contains_key(&row.pr))
        .collect()
    }
    fn pr_row(
        &self,
        row: &PrRow,
        last: bool,
        rail: model::StackRail,
        parents: &BTreeMap<PrRef, PrRef>,
        group: Option<(&model::StackGroup, bool)>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let state = self.states.get(&row.pr);
        let reading = self.reading(&row.pr);
        let active = state.is_some_and(SitState::is_active);
        let next = group.is_some_and(|(group, _)| group.next == row.pr);
        let pr = row.pr.clone();
        let action = if active && group.is_some() {
            None
        } else if active {
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
                .disabled(self.removal_tasks.contains_key(&row.pr))
                .on_click(cx.listener(move |this, _, window, cx| {
                    let mode = match &this.prs {
                        Load::Ready(rows) => model::stack_mode_of(
                            &pr,
                            &this.effective_parents(rows),
                            &this.config.active,
                        )
                        .unwrap_or_default(),
                        _ => StackMode::BottomUp,
                    };
                    this.start(pr.clone(), mode, window, cx)
                })),
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
            .on_click(cx.listener(move |this, _, window, cx| {
                this.dismiss(pr.clone(), window, cx);
            }))
        });
        let time_left = (active && group.is_none())
            .then(|| self.config.active.get(&row.pr))
            .flatten()
            .map(|sit| time_left(sit.deadline.saturating_sub(model::unix_now())));
        let is_open = matches!(row.state, PrState::Draft | PrState::Ready | PrState::Queued)
            && matches!(&self.prs, Load::Ready(rows) if rows.iter().any(|item| item.pr == row.pr));
        let is_draft = row.state == PrState::Draft;
        let merged = reading.merged;
        let shows_merged = merged || row.state == PrState::Merged;
        let stack_watched = self.config.repos.contains_key(&row.pr.repo)
            && (parents.contains_key(&row.pr) || parents.values().any(|parent| parent == &row.pr));
        let menu_pr = row.pr.clone();
        let menu_url = row.url.clone();
        let view = cx.entity().downgrade();
        let menu = round_button_with(
            SharedString::from(format!("actions-{}", row.pr)),
            Icon::empty().path("icons/ellipsis.svg").size(px(14.)),
            "More actions",
            muted(),
            ink(),
            cx,
        )
        .disabled(self.pr_action_tasks.contains_key(&row.pr))
        .dropdown_menu(move |mut menu, _, _| {
            menu = menu.min_w(px(200.));
            for action in RowAction::ALL {
                if let Some(label) = action.menu_item(is_open, is_draft, merged, stack_watched) {
                    let pr = menu_pr.clone();
                    let url = menu_url.clone();
                    let view = view.clone();
                    let (icon, color) = match action {
                        RowAction::MarkReady => {
                            ("icons/git-pull-request.svg", pr_state_color(PrState::Ready))
                        }
                        RowAction::ConvertToDraft => (
                            "icons/git-pull-request-draft.svg",
                            pr_state_color(PrState::Draft),
                        ),
                        RowAction::Close => (
                            "icons/git-pull-request-closed.svg",
                            pr_state_color(PrState::Closed),
                        ),
                        RowAction::BabysitStack(_) => ("icons/stack-16.svg", muted()),
                        RowAction::CopyLink => ("icons/link.svg", muted()),
                    };
                    let item = PopupMenuItem::element(move |_, _| {
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .px(px(4.))
                            .py(px(6.))
                            .child(Icon::empty().path(icon).size(px(16.)).text_color(color))
                            .child(label)
                    });
                    menu = menu.item(item.on_click(move |_, window, cx| {
                        if action == RowAction::CopyLink {
                            copy_pr_link(&pr, &url, window, cx);
                        } else {
                            let _ = view.update(cx, |this, cx| match action {
                                RowAction::MarkReady => this.run_pr_action(
                                    pr.clone(),
                                    gh::GhPrAction::Ready,
                                    window,
                                    cx,
                                ),
                                RowAction::ConvertToDraft => this.run_pr_action(
                                    pr.clone(),
                                    gh::GhPrAction::Draft,
                                    window,
                                    cx,
                                ),
                                RowAction::Close => this.ask_close(pr.clone(), window, cx),
                                RowAction::BabysitStack(mode) => {
                                    this.start_stack(&pr, mode, window, cx)
                                }
                                RowAction::CopyLink => {}
                            });
                        }
                    }));
                }
            }
            menu
        });
        let url = row.url.clone();
        let open = round_button(
            SharedString::from(format!("open-{}", row.pr)),
            IconName::Github,
            "Open on GitHub",
            muted(),
            ink(),
            cx,
        )
        .on_click(move |_, _, _| open_url(url.clone()));
        let title = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .when(!shows_merged, |title| {
                let label = row.state.label();
                title.child(
                    div()
                        .id(SharedString::from(format!("state-{}", row.pr)))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .text_color(pr_state_color(row.state))
                        .child(
                            Icon::empty()
                                .path(match row.state {
                                    PrState::Draft => "icons/git-pull-request-draft.svg",
                                    PrState::Ready => "icons/git-pull-request.svg",
                                    PrState::Queued => "icons/git-merge-queue-16.svg",
                                    PrState::Merged => "icons/git-merge.svg",
                                    PrState::Closed => "icons/git-pull-request-closed.svg",
                                })
                                .size(px(16.)),
                        )
                        .tooltip(move |window, cx| Tooltip::new(label).build(window, cx)),
                )
            })
            .child(
                div()
                    .min_w_0()
                    .text_size(px(16.))
                    .line_height(relative(1.3))
                    .font_semibold()
                    .text_color(ink())
                    .child(row.title.clone()),
            );
        let place = if row.base.is_empty() || row.head.is_empty() {
            div()
                .flex_shrink_0()
                .text_size(px(13.))
                .text_color(muted())
                .child(row.pr.repo.to_string())
        } else {
            div()
                .min_w_0()
                .flex()
                .items_center()
                .gap(px(6.))
                .text_size(px(13.))
                .text_color(muted())
                .child(branch_chip(format!("head-{}", row.pr), &row.head).min_w_0())
                .when(!rail.up, |place| {
                    place
                        .child(div().flex_shrink_0().child("into"))
                        .child(branch_chip(format!("base-{}", row.pr), &row.base).flex_shrink_0())
                })
        };
        let copy_url = row.url.clone();
        let copy_pr = row.pr.clone();
        let path = sitter::worktree_path(&self.data, &row.pr);
        let worktree = path.exists().then(|| {
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
        let content = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(title)
            .child(
                div()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(16.))
                    .child(
                        div()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(place)
                            .child(
                                div()
                                    .id(SharedString::from(format!("copy-link-{}", row.pr)))
                                    .flex_shrink_0()
                                    .text_size(px(13.))
                                    .text_color(muted())
                                    .cursor_pointer()
                                    .hover(|style| style.text_color(ink()).underline())
                                    .tooltip(|window, cx| {
                                        Tooltip::new("Copy link").build(window, cx)
                                    })
                                    .on_click(move |_, window, cx| {
                                        copy_pr_link(&copy_pr, &copy_url, window, cx)
                                    })
                                    .child(format!("#{}", row.pr.number)),
                            ),
                    )
                    .child(gate_icons(
                        &row.pr,
                        &{
                            let merged = row.state == PrState::Merged
                                || matches!(
                                    self.states.get(&row.pr),
                                    Some(SitState::Finished(Outcome::Merged))
                                );
                            let mut gates = self.gates.get(&row.pr).copied().unwrap_or(if merged {
                                [GateState::Met; 4]
                            } else {
                                [GateState::Pending; 4]
                            });
                            let lands_through_bottom = parents.contains_key(&row.pr)
                                && model::stack_mode_of(&row.pr, parents, &self.config.active)
                                    == Some(StackMode::TopDown);
                            if row.state == PrState::Draft || lands_through_bottom {
                                gates[3] = GateState::Skipped;
                            }
                            gates
                        },
                        active.then(|| self.pulse()),
                    ))
                    .when_some(row.queue_position, |line, position| {
                        let color = pr_state_color(PrState::Queued);
                        line.child(
                            div()
                                .id(SharedString::from(format!("queue-position-{}", row.pr)))
                                .px(px(7.))
                                .py(px(2.))
                                .rounded_full()
                                .bg(color.opacity(0.15))
                                .text_color(color)
                                .text_size(px(12.))
                                .tooltip(move |window, cx| {
                                    Tooltip::new(format!("Position {position} in the merge queue"))
                                        .build(window, cx)
                                })
                                .child(format!("#{position} in queue")),
                        )
                    })
                    .when(next, |line| {
                        line.child(
                            div()
                                .px(px(7.))
                                .py(px(2.))
                                .rounded_full()
                                .bg(accent().opacity(0.15))
                                .text_color(accent())
                                .text_size(px(12.))
                                .child("Next to merge"),
                        )
                    })
                    .when_some(worktree, |line, worktree| line.child(worktree)),
            )
            .when(
                reading.verb.is_some() || reading.detail.is_some(),
                |content| {
                    content.child(
                        div()
                            .flex()
                            .items_start()
                            .gap(px(8.))
                            .when_some(reading.verb, |line, verb| {
                                line.child(
                                    div()
                                        .id(SharedString::from(format!("status-{}", row.pr)))
                                        .flex_shrink_0()
                                        .h(px(19.))
                                        .flex()
                                        .items_center()
                                        .child(status_badge(reading.badge))
                                        .tooltip(move |window, cx| {
                                            Tooltip::new(verb).build(window, cx)
                                        }),
                                )
                            })
                            .when_some(reading.detail.clone(), |line, detail| {
                                line.child(
                                    div()
                                        .min_w_0()
                                        .text_size(px(13.))
                                        .line_height(relative(1.45))
                                        .text_color(ink())
                                        .child(detail),
                                )
                            }),
                    )
                },
            );
        let inset = if group.is_some() {
            ROW_INSET - 9.
        } else {
            ROW_INSET
        };
        let rail_line = |segment: Div, upper: bool| {
            segment
                .absolute()
                .left(px(inset + 15.5))
                .w(px(3.))
                .bg(accent().opacity(0.7))
                .when_some(
                    group.filter(|(_, running)| *running),
                    |segment, (group, _)| {
                        let phase = (self.pulse_epoch.elapsed().as_millis() % 2400) as f32 / 2400.;
                        let phase = if group.mode == StackMode::TopDown {
                            1. - phase
                        } else {
                            phase
                        };
                        let visible = if upper { phase >= 0.5 } else { phase < 0.5 };
                        let local = if upper {
                            (phase - 0.5) * 2.
                        } else {
                            phase * 2.
                        };
                        segment.when(visible, |segment| {
                            segment.child(
                                div()
                                    .absolute()
                                    .left(px(-1.5))
                                    .top(relative(local))
                                    .mt(px(-3.))
                                    .size(px(6.))
                                    .rounded_full()
                                    .bg(accent().opacity((std::f32::consts::PI * phase).sin())),
                            )
                        })
                    },
                )
        };
        div()
            .relative()
            .px(px(inset))
            .flex_shrink_0()
            .py(px(18.))
            .when(!last, |item| item.border_b_1().border_color(divider()))
            .when(rail.up, |item| {
                item.child(rail_line(div(), true).top_0().h(px(18.)))
            })
            .when(rail.down, |item| {
                item.child(rail_line(div(), false).top(px(52.)).bottom_0())
            })
            .flex()
            .items_start()
            .gap(px(20.))
            .child(
                div().flex_shrink_0().size(px(34.)).child(match action {
                    Some(action) => action.into_any_element(),
                    None if shows_merged => {
                        let color = pr_state_color(PrState::Merged);
                        div()
                            .id(SharedString::from(format!("merged-{}", row.pr)))
                            .flex_shrink_0()
                            .size(px(34.))
                            .rounded_full()
                            .bg(color.opacity(0.18))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(color)
                            .child(Icon::empty().path("icons/git-merge.svg").size(px(16.)))
                            .tooltip(|window, cx| Tooltip::new("Merged").build(window, cx))
                            .into_any_element()
                    }
                    None if active && group.is_some() => div()
                        .id(SharedString::from(format!("stack-node-{}", row.pr)))
                        .size(px(34.))
                        .rounded_full()
                        .bg(if next {
                            accent().opacity(0.18)
                        } else {
                            raised()
                        })
                        .when(next, |node| node.border_2().border_color(accent()))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .size(px(8.))
                                .rounded_full()
                                .bg(accent().opacity(self.pulse())),
                        )
                        .tooltip(move |window, cx| {
                            Tooltip::new(if next {
                                "Merges next"
                            } else {
                                "Babysat with the stack"
                            })
                            .build(window, cx)
                        })
                        .into_any_element(),
                    None => div().size(px(34.)).into_any_element(),
                }),
            )
            .child(content)
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(12.))
                            .when_some(dismiss, |d, dismiss| d.child(dismiss))
                            .child(open)
                            .child(menu),
                    )
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
    fn render_dashboard(&self, fullscreen: bool, cx: &mut Context<Self>) -> (Div, Div) {
        let mut rows = self.dashboard_rows();
        let parents = match &self.prs {
            Load::Ready(prs) => self.effective_parents(prs),
            _ => self.effective_parents(&[]),
        };
        let watched = model::watched_prs(&rows, &self.states, &parents);
        let watch_counts = model::watch_counts(&rows, &watched);
        let query = self.search.read(cx).value().to_string();
        let searching = !query.trim().is_empty();
        let admitted = |row: &PrRow| {
            if searching {
                model::matches_search(row, &query)
            } else {
                self.watch_filter.admits(watched.contains(&row.pr))
                    && self.state_filter.admits(row, self.states.get(&row.pr))
            }
        };
        let whole_stacks: BTreeSet<PrRef> = if searching {
            BTreeSet::new()
        } else {
            let ordered = model::order_stacks(rows.clone(), &parents);
            model::stack_groups(&ordered, &parents, &self.config.active, &self.states)
                .into_iter()
                .map(|group| &ordered[group.rows])
                .filter(|members| members.iter().any(|row| admitted(row)))
                .flat_map(|members| members.iter().map(|row| row.pr.clone()))
                .collect()
        };
        rows.retain(|row| whole_stacks.contains(&row.pr) || admitted(row));
        let rows = model::order_stacks(rows, &parents);
        let list = match &self.prs {
            Load::Loading if rows.is_empty() => message("Loading pull requests…", None),
            Load::Error(error) if rows.is_empty() => {
                message("Could not load pull requests", Some(error.clone()))
                    .child(Self::retry_prs_button(cx))
            }
            _ if rows.is_empty() => {
                if searching {
                    message("No pull requests match the search", None)
                } else if self.state_filter == StateFilter::Drafts {
                    message("No draft pull requests", None)
                } else if self.state_filter == StateFilter::Merged {
                    message("No merged pull requests", None)
                } else if self.state_filter == StateFilter::Open {
                    message("No open pull requests", None)
                } else if self.watch_filter == WatchFilter::All && self.config.repos.is_empty() {
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
                    match self.watch_filter {
                        WatchFilter::Watched => message(
                            "Baby Sitter is not babysitting any pull requests",
                            Some("Press play on a pull request to start.".into()),
                        ),
                        WatchFilter::Unwatched => {
                            message("Baby Sitter is babysitting every listed pull request", None)
                        }
                        WatchFilter::All => message("No open pull requests", None),
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
                        .children({
                            let rails = model::stack_rails(&rows, &parents);
                            let groups = model::stack_groups(
                                &rows,
                                &parents,
                                &self.config.active,
                                &self.states,
                            );
                            let mut elements = Vec::new();
                            let mut index = 0;
                            for group in &groups {
                                while index < group.rows.start {
                                    elements.push(self.pr_row(
                                        &rows[index],
                                        index + 1 == rows.len(),
                                        rails[index],
                                        &parents,
                                        None,
                                        cx,
                                    ));
                                    index += 1;
                                }
                                let members: Vec<_> = model::stack_tree(&rows[index].pr, &parents)
                                    .into_iter()
                                    .filter(|pr| self.config.active.contains_key(pr))
                                    .collect();
                                let running = members
                                    .iter()
                                    .any(|pr| self.states.get(pr).is_some_and(SitState::is_active));
                                let count = group.rows.len();
                                let mode = match group.mode {
                                    StackMode::BottomUp => "bottom up",
                                    StackMode::TopDown => "top down",
                                };
                                let label = if group.merged > 0 {
                                    format!(
                                        "Stack of {count} · {mode} · {} of {count} merged",
                                        group.merged
                                    )
                                } else {
                                    format!("Stack of {count} · {mode}")
                                };
                                let stop = Button::new(SharedString::from(format!(
                                    "stop-stack-{}",
                                    rows[index].pr
                                )))
                                .custom(
                                    ButtonCustomVariant::new(cx)
                                        .color(muted())
                                        .foreground(danger())
                                        .hover(danger().opacity(0.3))
                                        .active(danger().opacity(0.45)),
                                )
                                .label("Stop")
                                .tooltip("Stop babysitting the stack")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.ask_cancel_stack(members.clone(), window, cx)
                                }));
                                let header = div()
                                    .h(px(40.))
                                    .px(px(12.))
                                    .flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .bg(accent().opacity(0.12))
                                    .text_color(accent())
                                    .child(Icon::empty().path("icons/stack-16.svg").size(px(16.)))
                                    .child(div().flex_1().font_semibold().child(label))
                                    .child(div().text_size(px(12.)).text_color(muted()).child(
                                        time_left(group.deadline.saturating_sub(model::unix_now())),
                                    ))
                                    .child(stop);
                                let mut card = div()
                                    .flex_shrink_0()
                                    .mx(px(8.))
                                    .my(px(6.))
                                    .rounded(px(10.))
                                    .border_1()
                                    .border_color(accent())
                                    .overflow_hidden()
                                    .flex()
                                    .flex_col()
                                    .child(header);
                                while index < group.rows.end {
                                    card = card.child(self.pr_row(
                                        &rows[index],
                                        index + 1 == group.rows.end,
                                        rails[index],
                                        &parents,
                                        Some((group, running)),
                                        cx,
                                    ));
                                    index += 1;
                                }
                                elements.push(card.into_any_element());
                            }
                            while index < rows.len() {
                                elements.push(self.pr_row(
                                    &rows[index],
                                    index + 1 == rows.len(),
                                    rails[index],
                                    &parents,
                                    None,
                                    cx,
                                ));
                                index += 1;
                            }
                            elements
                        }),
                )
            }
        };
        let header = div()
            .flex_shrink_0()
            .h(px(50.))
            .pl(px(if fullscreen { ROW_INSET } else { 86. }))
            .pr(px(ROW_INSET))
            .flex()
            .items_center()
            .gap(px(10.))
            .child(
                drag_area()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(pair_mark())
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_semibold()
                            .text_color(ink())
                            .child("Baby Sitter"),
                    ),
            )
            .child(header_picker(
                &self.dashboard_filter,
                "Filter by repository",
                "All repositories",
                Size::Medium,
            ))
            .child(
                round_button_with(
                    "refresh".into(),
                    refresh_icon(self.loading_repos || self.loading_prs),
                    "Refresh",
                    raised(),
                    ink(),
                    cx,
                )
                .size(px(32.))
                .rounded(px(16.))
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
                .size(px(32.))
                .rounded(px(16.))
                .on_click(cx.listener(|this, _, _, cx| this.open_settings(cx))),
            );
        let watch_filter = ButtonGroup::new("watch-filter")
            .with_size(Size::Medium)
            .outline()
            .children(watch_counts.into_iter().map(|(filter, count)| {
                let (id, label) = match filter {
                    WatchFilter::All => ("watch-filter-all", "All"),
                    WatchFilter::Watched => ("watch-filter-watched", "Watched"),
                    WatchFilter::Unwatched => ("watch-filter-unwatched", "Unwatched"),
                };
                let selected = self.watch_filter == filter;
                Button::new(id).selected(selected).child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .text_color(if selected { accent() } else { muted() })
                        .when(selected, |d| d.font_semibold())
                        .child(label)
                        .when(count > 0, |d| d.child(count.to_string())),
                )
            }))
            .on_click(cx.listener(|this, selected: &Vec<usize>, _, cx| {
                if let Some(filter) = selected
                    .first()
                    .and_then(|index| WatchFilter::ALL.get(*index))
                {
                    this.watch_filter = *filter;
                    cx.notify();
                }
            }));
        let state_filter = ButtonGroup::new("state-filter")
            .with_size(Size::Medium)
            .outline()
            .children(
                StateFilter::visible(self.config.app.hides_merged_filter())
                    .iter()
                    .copied()
                    .map(|filter| {
                        let (id, label) = match filter {
                            StateFilter::All => ("state-filter-all", "All"),
                            StateFilter::Open => ("state-filter-open", "Open"),
                            StateFilter::Drafts => ("state-filter-drafts", "Drafts"),
                            StateFilter::Merged => ("state-filter-merged", "Merged"),
                        };
                        let selected = self.state_filter == filter;
                        Button::new(id).selected(selected).child(
                            div()
                                .text_color(if selected { accent() } else { muted() })
                                .when(selected, |d| d.font_semibold())
                                .child(label),
                        )
                    }),
            )
            .on_click(cx.listener(|this, selected: &Vec<usize>, _, cx| {
                if let Some(filter) = selected.first().and_then(|index| {
                    StateFilter::visible(this.config.app.hides_merged_filter()).get(*index)
                }) {
                    this.state_filter = *filter;
                    cx.notify();
                }
            }));
        let filters = div()
            .flex_shrink_0()
            .px(px(ROW_INSET))
            .pt(px(12.))
            .pb(px(16.))
            .border_b_1()
            .border_color(divider())
            .flex()
            .items_center()
            .gap(px(12.))
            .child(
                div()
                    .when(searching, |group| group.opacity(0.45))
                    .child(watch_filter),
            )
            .child(
                div()
                    .when(searching, |group| group.opacity(0.45))
                    .child(state_filter),
            )
            .child(
                div().ml_auto().w(px(240.)).flex_shrink_0().child(
                    Input::new(&self.search)
                        .with_size(Size::Medium)
                        .cleanable(true)
                        .prefix(Icon::new(IconName::Search).text_color(muted())),
                ),
            );
        let body = div()
            .size_full()
            .flex()
            .flex_col()
            .child(filters)
            .child(div().flex_1().min_h_0().child(list));
        (header, body)
    }
    fn render_sidebar(&self, fullscreen: bool, cx: &mut Context<Self>) -> Div {
        div()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(panel())
            .border_r_1()
            .border_color(divider())
            .child(drag_area().h(px(if fullscreen { 0. } else { 52. })))
            .child(
                div()
                    .id("settings-home")
                    .when(fullscreen, |row| row.h(px(50.)))
                    .px(px(20.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .cursor_pointer()
                    .tooltip(|window, cx| Tooltip::new("Home").build(window, cx))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.page = Page::PullRequests;
                        cx.notify();
                    }))
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
                div()
                    .px(px(12.))
                    .pt(px(28.))
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .children(
                        [
                            ("nav-app", "App", IconName::Settings, SettingsPage::App),
                            (
                                "nav-repos",
                                "Repositories",
                                IconName::BookMarked,
                                SettingsPage::Repositories,
                            ),
                            (
                                "nav-prompts",
                                "Prompts",
                                IconName::MessageSquare,
                                SettingsPage::Prompts,
                            ),
                            (
                                "nav-status",
                                "Status",
                                IconName::Check,
                                SettingsPage::Status,
                            ),
                        ]
                        .into_iter()
                        .map(|(id, label, icon, page)| {
                            let selected = match &page {
                                SettingsPage::Repositories => matches!(
                                    self.page,
                                    Page::Settings(
                                        SettingsPage::Repositories | SettingsPage::Repo(_)
                                    )
                                ),
                                SettingsPage::App => {
                                    matches!(self.page, Page::Settings(SettingsPage::App))
                                }
                                SettingsPage::Prompts => {
                                    matches!(self.page, Page::Settings(SettingsPage::Prompts))
                                }
                                SettingsPage::Status => {
                                    matches!(self.page, Page::Settings(SettingsPage::Status))
                                }
                                SettingsPage::Repo(_) => false,
                            };
                            let color = if selected { accent() } else { muted() };
                            div()
                                .rounded(px(6.))
                                .when(selected, |d| d.bg(accent().opacity(0.12)))
                                .child(
                                    Button::new(id)
                                        .custom(
                                            ButtonCustomVariant::new(cx)
                                                .foreground(color)
                                                .hover(raised())
                                                .active(divider()),
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
                                                .child(Icon::new(icon).size_4().text_color(color))
                                                .child(label),
                                        )
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.page = Page::Settings(page.clone());
                                            cx.notify();
                                        })),
                                )
                        }),
                    ),
            )
    }
}

impl AppView {
    fn load_setup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.load_setup_with_authorization(false, window, cx);
    }

    fn load_setup_with_authorization(
        &mut self,
        request_authorization: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(self.setup, Load::Loading) && self.setup_task.is_some() {
            return;
        }
        self.setup = Load::Loading;
        self.models.retain(|_, load| match load {
            Load::Loading => true,
            Load::Ready(options) => !options.is_empty(),
            Load::Error(_) => false,
        });
        let env = self.env.clone();
        let config = self.config.clone();
        self.setup_task = Some(cx.spawn_in(window, async move |view, cx| {
            let results = cx
                .background_spawn(async move {
                    if request_authorization {
                        notifications::request_authorization();
                    }
                    setup::run_all(&env, &config)
                })
                .await;
            let _ = view.update_in(cx, |this, _, cx| {
                this.setup = Load::Ready(results);
                this.setup_task = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn status_banner(&self, cx: &mut Context<Self>) -> Option<Div> {
        let Load::Ready(results) = &self.setup else {
            return None;
        };
        let mut failures = setup::required_failures(results);
        let (first, _) = failures.next()?;
        let more = failures.count();
        let text = if more == 0 {
            format!("Baby Sitter isn't ready. {} needs attention.", first.label)
        } else {
            format!(
                "Baby Sitter isn't ready. {} and {more} more need attention.",
                first.label
            )
        };
        Some(
            div()
                .px(px(ROW_INSET))
                .py(px(6.))
                .flex()
                .items_center()
                .gap(px(12.))
                .bg(warning().opacity(0.12))
                .text_color(warning())
                .text_size(px(13.))
                .child(div().flex_1().min_w_0().text_ellipsis().child(text))
                .child(
                    Button::new("open-status")
                        .label("Open status")
                        .ghost()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.page = Page::Settings(SettingsPage::Status);
                            cx.notify();
                        })),
                ),
        )
    }

    fn render_status(&self, cx: &mut Context<Self>) -> (Div, Div) {
        let header = self
            .settings_header(
                "Status",
                Some("Baby Sitter needs these tools, sign-ins, and permissions on this Mac. The checks use the same PATH as babysits."),
                cx,
            )
            .child(
                div()
                    .px(px(ROW_INSET))
                    .pb(px(12.))
                    .flex()
                    .justify_end()
                    .child(
                        Button::new("check-again")
                            .label("Check again")
                            .outline()
                            .disabled(matches!(self.setup, Load::Loading))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.load_setup(window, cx)),
                            ),
                    ),
            );
        let mut body = div()
            .id("status-scroll")
            .size_full()
            .overflow_y_scroll()
            .px(px(ROW_INSET))
            .pb(px(24.));
        match &self.setup {
            Load::Loading => {
                body = body.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .py(px(20.))
                        .text_color(muted())
                        .child(Spinner::new().small())
                        .child("Checking tools and sign-ins…"),
                );
            }
            Load::Error(error) => {
                body = body.child(div().text_color(danger()).child(error.clone()));
            }
            Load::Ready(results) => {
                for (section, title) in [
                    (Section::Required, "Required"),
                    (Section::Permissions, "Permissions"),
                ] {
                    body = body.child(div().pt(px(20.)).pb(px(8.)).font_semibold().child(title));
                    if section == Section::Permissions {
                        body = body.child(
                            div().text_size(px(13.)).text_color(muted()).child(
                                "Baby Sitter needs no other macOS permissions. If macOS asks for Desktop, Documents, or Photos while a babysit runs, that request comes from the coding agent reading outside the checkout. Choose Don't Allow.",
                            ),
                        );
                    }
                    let section_rows: Vec<_> = results
                        .iter()
                        .filter(|(spec, _)| spec.section == section)
                        .collect();
                    let count = section_rows.len();
                    for (index, (spec, state)) in section_rows.into_iter().enumerate() {
                        let last = index + 1 == count;
                        let (detail, fix, badge) = match state {
                            CheckState::Ok(detail) => (detail.as_str(), None, GateState::Met),
                            CheckState::Missing { detail, fix } => (
                                detail.as_str(),
                                Some(fix.clone()),
                                if section == Section::Required {
                                    GateState::Blocked
                                } else {
                                    GateState::Pending
                                },
                            ),
                            CheckState::Failed(detail) => {
                                let fix = match spec.id {
                                    "gh" if detail.starts_with("Version ") => "brew upgrade gh",
                                    "gh-auth" if detail.contains("token lacks repo scope") => {
                                        "gh auth refresh -s repo,read:org"
                                    }
                                    _ if section == Section::Permissions => "",
                                    _ => "Check again",
                                };
                                (
                                    detail.as_str(),
                                    (!fix.is_empty()).then(|| Fix::Copy(fix.into())),
                                    GateState::Blocked,
                                )
                            }
                        };
                        let mut row = div()
                            .py(px(10.))
                            .when(!last, |row| row.border_b_1().border_color(divider()))
                            .flex()
                            .items_start()
                            .gap(px(10.))
                            .child(status_badge(badge))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap(px(3.))
                                    .child(div().font_semibold().child(spec.label))
                                    .child(
                                        div()
                                            .text_size(px(13.))
                                            .text_color(muted())
                                            .child(detail.to_owned()),
                                    ),
                            );
                        match fix {
                            Some(Fix::Copy(value)) => {
                                row = row.child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(6.))
                                        .child(
                                            div()
                                                .px(px(8.))
                                                .py(px(4.))
                                                .rounded(px(4.))
                                                .bg(raised())
                                                .text_size(px(12.))
                                                .font_family("monospace")
                                                .child(value.clone()),
                                        )
                                        .child(
                                            Button::new(SharedString::from(format!(
                                                "copy-{}",
                                                spec.id
                                            )))
                                            .label("Copy")
                                            .ghost()
                                            .on_click(cx.listener(move |_, _, window, cx| {
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    value.clone(),
                                                ));
                                                window.push_notification(
                                                    Notification::success("Copied fix"),
                                                    cx,
                                                );
                                            })),
                                        ),
                                );
                            }
                            Some(Fix::OpenSettings(url)) => {
                                row = row.child(
                                    Button::new(SharedString::from(format!(
                                        "settings-{}",
                                        spec.id
                                    )))
                                    .label("Open System Settings")
                                    .outline()
                                    .on_click(
                                        move |_, _, _| {
                                            std::thread::spawn(move || {
                                                let _ = std::process::Command::new("open")
                                                    .arg(url)
                                                    .status();
                                            });
                                        },
                                    ),
                                );
                            }
                            Some(Fix::AllowNotifications) => {
                                row = row.child(
                                    Button::new("allow-notifications")
                                        .label("Allow notifications")
                                        .outline()
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.load_setup_with_authorization(true, window, cx);
                                        })),
                                );
                            }
                            None => {}
                        }
                        body = body.child(row);
                    }
                }
            }
        }
        (header, div().size_full().child(body))
    }
}

/// An agent harness's logo next to its name, for the picker and its menu.
fn harness_choice(kind: AgentKind, label: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .child(img(kind.logo()).size(px(16.)).flex_shrink_0())
        .child(label.into())
}

fn duration_field(
    icon: Icon,
    label: &'static str,
    hint: &'static str,
    input: &Entity<InputState>,
    error: &Option<String>,
    disabled: bool,
) -> Div {
    let note = match error {
        Some(error) => div().text_color(danger()).child(error.clone()),
        None => div().text_color(muted()).child(hint),
    };
    settings_field(icon, label, Input::new(input).disabled(disabled), note)
}

/// A settings row: an icon and label in the label column, the control in the input column,
/// and a note under the control.
fn settings_field(icon: Icon, label: &'static str, control: impl IntoElement, note: Div) -> Div {
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
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .text_size(px(14.))
                        .font_semibold()
                        .text_color(ink())
                        .child(icon.size(px(14.)).text_color(muted()))
                        .child(label),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .max_w(px(SETTINGS_INPUT_WIDTH))
                        .child(control),
                ),
        )
        .child(
            note.ml(px(SETTINGS_LABEL_WIDTH + SETTINGS_COLUMN_GAP))
                .max_w(px(SETTINGS_INPUT_WIDTH))
                .text_size(px(12.))
                .line_height(relative(1.4)),
        )
}

fn duration_fields(inputs: &RepoInputs) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(20.))
        .child(duration_field(
            Icon::empty().path("icons/timer.svg"),
            "Timeout",
            "Wait this long for the merge, for example 2h.",
            &inputs.timeout,
            &inputs.timeout_error,
            false,
        ))
        .child(duration_field(
            Icon::new(IconName::GitPullRequestDraft),
            "Draft timeout",
            "Babysit a draft pull request for this long.",
            &inputs.draft_timeout,
            &inputs.draft_timeout_error,
            false,
        ))
        .child(duration_field(
            Icon::new(IconName::RefreshCw),
            "Interval",
            "Wait this long between checks, for example 30s.",
            &inputs.interval,
            &inputs.interval_error,
            false,
        ))
}

/// A branch name in a gray chip, for the head and base of a row's PR.
fn branch_chip(id: String, name: &str) -> Stateful<Div> {
    let gray = muted();
    let full = SharedString::from(name.to_owned());
    div()
        .id(SharedString::from(id))
        .px(px(6.))
        .py(px(1.))
        .rounded(px(6.))
        .bg(gray.opacity(0.14))
        .text_color(gray)
        .font_family("Menlo")
        .text_size(px(12.))
        .text_ellipsis()
        .whitespace_nowrap()
        .overflow_hidden()
        .tooltip(move |window, cx| Tooltip::new(full.clone()).build(window, cx))
        .child(name.to_owned())
}

fn open_url(url: String) {
    std::thread::spawn(move || {
        let _ = std::process::Command::new("open").arg(url).status();
    });
}

fn copy_pr_link(pr: &PrRef, url: &str, window: &mut Window, cx: &mut App) {
    cx.write_to_clipboard(ClipboardItem::new_string(url.to_owned()));
    window.push_notification(
        Notification::success(format!("Copied link to {} #{}", pr.repo, pr.number)),
        cx,
    );
}

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
fn format_minutes(seconds: u64) -> String {
    let minutes = seconds / 60;
    match (minutes / 60, minutes % 60) {
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h {m}m"),
    }
}
fn time_left(seconds: u64) -> String {
    if seconds < 60 {
        "Under a minute left".to_owned()
    } else {
        format!("{} left", format_minutes(seconds))
    }
}
/// A header picker choice is an organization or repository with its owner's
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
/// The merge requirements in display order, each with its index in the CLI's
/// `[gates] reviews checks branch merge` report and its own icon.
const GATE_ICONS: [(usize, IconName, &str); 4] = [
    (0, IconName::MessageSquare, "Reviews"),
    (2, IconName::GitBranch, "Branch up to date"),
    (1, IconName::ListChecks, "Checks"),
    (3, IconName::GitMerge, "Merge"),
];
/// One icon per merge requirement, colored by its state and named in its tooltip.
/// A waiting requirement pulses while the babysit runs, at `pulse` opacity, so every
/// pulsing icon shares one phase.
fn gate_icons(pr: &PrRef, gates: &[GateState; 4], pulse: Option<f32>) -> Div {
    div()
        .flex_shrink_0()
        .h(px(19.))
        .flex()
        .items_center()
        .gap(px(6.))
        .children(
            GATE_ICONS
                .iter()
                .filter(|&&(index, _, _)| gates[index] != GateState::Skipped)
                .map(|&(index, icon, name)| {
                    let state = gates[index];
                    let (color, word) = state_style(state);
                    let label = SharedString::from(format!("{name}: {word}"));
                    let id = SharedString::from(format!("gate-{pr}-{name}"));
                    let color = match pulse {
                        Some(opacity) if state == GateState::Waiting => color.opacity(opacity),
                        _ => color,
                    };
                    let mark = Icon::new(icon).size(px(15.)).text_color(color);
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
                }),
        )
}
/// The color and word for a state, shared by gate icons, tooltips, and status badges.
fn state_style(state: GateState) -> (Hsla, &'static str) {
    match state {
        GateState::Met => (success(), "done"),
        GateState::Waiting => (warning(), "waiting"),
        GateState::Blocked => (danger(), "blocked"),
        GateState::Pending => (muted().opacity(0.45), "not started"),
        GateState::Skipped => (muted().opacity(0.45), "skipped"),
    }
}
/// A filled circle has a check, dash, or cross. A hollow ring means not started.
fn status_badge(state: GateState) -> Div {
    let (color, _) = state_style(state);
    let badge = div()
        .size(px(14.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center();
    let icon = match state {
        GateState::Met => IconName::Check,
        GateState::Waiting => IconName::Minus,
        GateState::Blocked => IconName::X,
        GateState::Pending | GateState::Skipped => {
            return badge.child(
                div()
                    .size(px(12.))
                    .rounded_full()
                    .border_2()
                    .border_color(color),
            );
        }
    };
    badge.child(Icon::new(icon).size(px(14.)).text_color(color))
}
/// Draws the caretaker and child mark, a large accent disc overlapped by a smaller one.
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let running = |pr: &PrRef| self.states.get(pr).is_some_and(SitState::is_active);
        let waiting_gate = self.states.iter().any(|(pr, state)| {
            state.is_active()
                && self
                    .gates
                    .get(pr)
                    .is_some_and(|gates| gates.contains(&GateState::Waiting))
        });
        let stack_flowing = match &self.prs {
            Load::Ready(prs) => {
                let parents = self.effective_parents(prs);
                let rows = model::order_stacks(self.dashboard_rows(), &parents);
                model::stack_groups(&rows, &parents, &self.config.active, &self.states)
                    .iter()
                    .any(|group| rows[group.rows.clone()].iter().any(|row| running(&row.pr)))
            }
            _ => false,
        };
        if self.page == Page::PullRequests && (waiting_gate || stack_flowing) {
            window.request_animation_frame();
        }
        if matches!(
            &self.page,
            Page::Settings(SettingsPage::Repo(repo)) if !self.config.repos.contains_key(repo)
        ) {
            self.page = Page::Settings(SettingsPage::Repositories);
        }
        let model_kind = match &self.page {
            Page::Settings(SettingsPage::App) => Some(self.config.app.defaults.agent),
            Page::Settings(SettingsPage::Repo(repo)) => self
                .config
                .repos
                .get(repo)
                .map(|settings| settings.agent.unwrap_or(self.config.app.defaults.agent)),
            _ => None,
        };
        if let Some(kind) = model_kind {
            self.load_models(kind, window, cx);
        }
        let (header, body) = match &self.page {
            Page::PullRequests => self.render_dashboard(window.is_fullscreen(), cx),
            Page::Settings(SettingsPage::Repositories) => self.render_repos(cx),
            Page::Settings(SettingsPage::App) => self.render_app_settings(cx),
            Page::Settings(SettingsPage::Prompts) => self.render_prompts(cx),
            Page::Settings(SettingsPage::Status) => self.render_status(cx),
            Page::Settings(SettingsPage::Repo(repo)) => self.render_repo_settings(repo, cx),
        };
        div()
            .size_full()
            .flex()
            .bg(surface())
            .text_color(ink())
            .when(matches!(self.page, Page::Settings(_)), |d| {
                d.child(self.render_sidebar(window.is_fullscreen(), cx))
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(header)
                    .when(matches!(self.page, Page::PullRequests), |d| {
                        d.when_some(self.status_banner(cx), |d, banner| d.child(banner))
                    })
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

#[cfg(test)]
mod row_action_tests {
    use super::{RowAction, StackMode};

    #[test]
    fn menu_matches_pull_request_state() {
        let labels = |open, draft, merged| {
            RowAction::ALL
                .into_iter()
                .filter_map(|action| action.menu_item(open, draft, merged, false))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            labels(true, true, false),
            ["Ready for review", "Close pull request", "Copy link"]
        );
        assert_eq!(
            labels(true, false, false),
            ["Convert to draft", "Close pull request", "Copy link"]
        );
        assert_eq!(labels(false, false, true), ["Copy link"]);
        assert_eq!(labels(false, false, false), ["Copy link"]);
        let labels: Vec<_> = RowAction::ALL
            .into_iter()
            .filter_map(|action| action.menu_item(true, false, false, true))
            .collect();
        assert_eq!(
            labels,
            [
                "Convert to draft",
                "Close pull request",
                "Babysit stack bottom up",
                "Babysit stack top down",
                "Copy link"
            ]
        );
        for mode in [StackMode::BottomUp, StackMode::TopDown] {
            assert_eq!(
                RowAction::BabysitStack(mode).menu_item(true, false, false, false),
                None
            );
        }
    }
}
