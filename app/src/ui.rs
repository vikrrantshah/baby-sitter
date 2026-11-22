use crate::{
    gh::{self, ChildEnv},
    model::{self, Config, Outcome, PrRef, PrRow, RepoName, RepoSettings, SitState},
    sitter::{self, Registry, RepoLocks, SitEvent, SitHandle},
    store,
};
use gpui_kit::{component::StyledExt, prelude::FluentBuilder};
use gpui_kit::{
    component::{
        Disableable, Sizable,
        button::{Button, ButtonVariants},
        checkbox::Checkbox,
        input::{Input, InputEvent, InputState},
        select::{Select, SelectEvent, SelectState},
    },
    *,
};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap},
    path::PathBuf,
};

enum Load<T> {
    Loading,
    Ready(T),
    Error(String),
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum Tab {
    Dashboard,
    Repos,
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

pub struct AppView {
    data: PathBuf,
    config: Config,
    env: ChildEnv,
    registry: Registry,
    locks: RepoLocks,
    tab: Tab,
    repos: Load<Vec<RepoName>>,
    prs: Load<Vec<PrRow>>,
    repo_filter: Entity<InputState>,
    dashboard_filter: Entity<SelectState<Vec<String>>>,
    selected_filter: Option<RepoName>,
    inputs: BTreeMap<RepoName, RepoInputs>,
    states: BTreeMap<PrRef, SitState>,
    handles: HashMap<PrRef, SitHandle>,
    event_tx: async_channel::Sender<SitEvent>,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
    _events: Task<()>,
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
        let subscriptions = vec![
            cx.subscribe(&repo_filter, |_: &mut Self, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
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
                        this.states.insert(event.pr, event.state);
                        cx.notify();
                    })
                    .is_err()
                {
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
            tab: Tab::Dashboard,
            repos: Load::Loading,
            prs: Load::Loading,
            repo_filter,
            dashboard_filter,
            selected_filter: None,
            inputs: BTreeMap::new(),
            states: BTreeMap::new(),
            handles: HashMap::new(),
            event_tx,
            error,
            _subscriptions: subscriptions,
            _events: events,
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
        cx.notify();
    }
    fn active_count(&self, repo: &RepoName) -> usize {
        self.states
            .iter()
            .filter(|(pr, state)| &pr.repo == repo && state.is_active())
            .count()
    }
    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.load_repos(cx);
        self.load_prs(window, cx);
    }
    fn load_repos(&mut self, cx: &mut Context<Self>) {
        self.repos = Load::Loading;
        let env = self.env.clone();
        self.repo_task = Some(cx.spawn(async move |view, cx| {
            let result = cx
                .background_spawn(async move { gh::list_repos(&env).map_err(|e| format!("{e:#}")) })
                .await;
            let _ = view.update(cx, |this, cx| {
                this.repos = match result {
                    Ok(repos) => Load::Ready(repos),
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
        let mut repos = BTreeSet::new();
        if let Load::Ready(prs) = &self.prs {
            repos.extend(prs.iter().map(|row| row.pr.repo.clone()));
        }
        repos.extend(self.states.keys().map(|pr| pr.repo.clone()));
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
        let action = matches!(self.states.get(&pr), Some(SitState::Running { last_line: Some(line) }) if line.starts_with('[') && !line.contains("] wait:") && !line.contains("] stop:") && !line.ends_with("] merged"));
        if !action {
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
    fn repo_row(&self, repo: &RepoName, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.config.repos.contains_key(repo);
        let count = self.active_count(repo);
        let checkbox_repo = repo.clone();
        let label = if count > 0 {
            format!("{repo}  ·  {count} babysitting")
        } else {
            repo.to_string()
        };
        let checkbox = Checkbox::new(SharedString::from(format!("repo-{repo}")))
            .label(label)
            .checked(selected)
            .disabled(count > 0)
            .on_change(cx.listener(move |this, checked, window, cx| {
                this.toggle_repo(checkbox_repo.clone(), *checked, window, cx)
            }));
        let mut row = div()
            .h(px(96.))
            .w_full()
            .px_4()
            .py_3()
            .border_b_1()
            .border_color(rgb(0xe5e9ef))
            .flex()
            .items_center()
            .gap_4()
            .child(div().flex_1().min_w_0().child(checkbox));
        if let Some(inputs) = self.inputs.get(repo) {
            let field =
                |label: &'static str, input: &Entity<InputState>, error: &Option<String>| {
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .w(px(185.))
                        .child(div().text_xs().text_color(rgb(0x667085)).child(label))
                        .child(Input::new(input).small())
                        .when_some(error.clone(), |d, e| {
                            d.child(div().text_xs().text_color(rgb(0xb42318)).child(e))
                        })
                };
            row = row
                .child(field("Timeout", &inputs.timeout, &inputs.timeout_error))
                .child(field("Interval", &inputs.interval, &inputs.interval_error));
        }
        row.into_any_element()
    }
    fn render_repos(&self, cx: &mut Context<Self>) -> AnyElement {
        let body = match &self.repos {
            Load::Loading => message("Loading repositories...", None),
            Load::Error(error) => {
                let retry = Button::new("retry-repos")
                    .label("Retry")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.load_repos(cx);
                        cx.notify();
                    }));
                message("Could not load repositories", Some(error.clone())).child(retry)
            }
            Load::Ready(repos) => {
                let filter = self.repo_filter.read(cx).value().to_lowercase();
                let mut filtered: Vec<_> = repos
                    .iter()
                    .filter(|repo| {
                        filter.is_empty() || repo.to_string().to_lowercase().contains(&filter)
                    })
                    .cloned()
                    .collect();
                filtered.sort_by(|a, b| {
                    self.config
                        .repos
                        .contains_key(b)
                        .cmp(&self.config.repos.contains_key(a))
                        .then_with(|| a.cmp(b))
                });
                if filtered.is_empty() {
                    message(
                        if filter.is_empty() {
                            "No repositories found"
                        } else {
                            "No repositories match the filter"
                        },
                        None,
                    )
                } else {
                    let count = filtered.len();
                    div().size_full().child(
                        uniform_list(
                            "repo-list",
                            count,
                            cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                                range
                                    .filter_map(|index| filtered.get(index))
                                    .map(|repo| this.repo_row(repo, cx))
                                    .collect::<Vec<_>>()
                            }),
                        )
                        .size_full(),
                    )
                }
            }
        };
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_end()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_xl().font_semibold().child("Repositories"))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(0x667085))
                                    .child("Choose where baby-sitter can run."),
                            ),
                    )
                    .child(div().w(px(280.)).child(Input::new(&self.repo_filter))),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .bg(rgb(0xffffff))
                    .border_1()
                    .border_color(rgb(0xe5e9ef))
                    .rounded_lg()
                    .child(body),
            )
            .into_any_element()
    }
    fn dashboard_rows(&self) -> Vec<Cow<'_, PrRow>> {
        let prs: &[PrRow] = match &self.prs {
            Load::Ready(rows) => rows,
            _ => &[],
        };
        let existing: BTreeSet<_> = prs.iter().map(|row| &row.pr).collect();
        let mut rows: Vec<_> = prs.iter().map(Cow::Borrowed).collect();
        rows.extend(
            self.states
                .keys()
                .filter(|pr| !existing.contains(*pr))
                .map(|pr| {
                    Cow::Owned(PrRow {
                        pr: pr.clone(),
                        title: format!("Pull request #{}", pr.number),
                        url: pr.url(),
                        is_draft: false,
                        updated_at: String::new(),
                    })
                }),
        );
        if let Some(filter) = &self.selected_filter {
            rows.retain(|row| &row.pr.repo == filter);
        }
        rows.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        rows
    }
    fn pr_row(&self, row: &PrRow, cx: &mut Context<Self>) -> AnyElement {
        let state = self.states.get(&row.pr);
        let (label, color, background, detail) = match state {
            None => ("Idle", 0x475467, 0xf2f4f7, None),
            Some(SitState::Preparing) => ("Preparing", 0x175cd3, 0xeff8ff, None),
            Some(SitState::Running { last_line }) => {
                ("Running", 0x175cd3, 0xeff8ff, last_line.clone())
            }
            Some(SitState::Cancelling) => ("Cancelling", 0xb54708, 0xfffaeb, None),
            Some(SitState::Finished(Outcome::Merged)) => ("Merged", 0x027a48, 0xecfdf3, None),
            Some(SitState::Finished(Outcome::TimedOut)) => ("Timed out", 0xb54708, 0xfffaeb, None),
            Some(SitState::Finished(Outcome::Stopped(reason))) => {
                ("Stopped", 0xb42318, 0xfef3f2, Some(reason.clone()))
            }
            Some(SitState::Finished(Outcome::Cancelled)) => ("Cancelled", 0x475467, 0xf2f4f7, None),
            Some(SitState::Finished(Outcome::Failed(reason))) => {
                ("Failed", 0xb42318, 0xfef3f2, Some(reason.clone()))
            }
        };
        let selected = self.config.repos.contains_key(&row.pr.repo);
        let active = state.is_some_and(SitState::is_active);
        let pr = row.pr.clone();
        let action = if active {
            Some(
                Button::new(SharedString::from(format!("cancel-{pr}")))
                    .label(if matches!(state, Some(SitState::Cancelling)) {
                        "Cancelling"
                    } else {
                        "Cancel"
                    })
                    .outline()
                    .disabled(matches!(state, Some(SitState::Cancelling)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.ask_cancel(pr.clone(), window, cx)
                    })),
            )
        } else if matches!(state, Some(SitState::Finished(Outcome::Merged))) {
            None
        } else {
            Some(
                Button::new(SharedString::from(format!("start-{pr}")))
                    .label("Start")
                    .primary()
                    .disabled(!selected)
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.start(pr.clone(), window, cx)),
                    ),
            )
        };
        let url = row.url.clone();
        let title_id = SharedString::from(format!("title-{}", row.pr));
        let title = div()
            .id(title_id)
            .cursor_pointer()
            .text_color(rgb(0x175cd3))
            .font_semibold()
            .child(row.title.clone())
            .on_click(move |_, _, _| {
                let url = url.clone();
                std::thread::spawn(move || {
                    let _ = std::process::Command::new("open").arg(url).status();
                });
            });
        let mut content = div()
            .flex()
            .flex_col()
            .gap_2()
            .flex_1()
            .min_w_0()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(title)
                    .when(row.is_draft, |d| d.child(chip("Draft", 0x475467, 0xf2f4f7))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x667085))
                            .child(row.pr.to_string()),
                    )
                    .child(chip(label, color, background)),
            )
            .when_some(detail, |d, text| {
                d.child(div().text_sm().text_color(rgb(0x344054)).child(text))
            })
            .when(!selected, |d| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x667085))
                        .child(format!("Select {} in Repos", row.pr.repo)),
                )
            });
        let path = sitter::worktree_path(&self.data, &row.pr);
        if !active && path.exists() {
            let path_string = path.display().to_string();
            let reveal_id = SharedString::from(format!("reveal-{}", row.pr));
            content = content.child(
                div()
                    .flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_xs().text_color(rgb(0x667085)).child(path_string))
                    .child(Button::new(reveal_id).label("Reveal").small().on_click(
                        move |_, _, _| {
                            let path = path.clone();
                            std::thread::spawn(move || {
                                let _ = std::process::Command::new("open")
                                    .arg("-R")
                                    .arg(path)
                                    .status();
                            });
                        },
                    )),
            );
        }
        div()
            .p_4()
            .border_b_1()
            .border_color(rgb(0xe5e9ef))
            .flex()
            .items_start()
            .gap_4()
            .child(content)
            .when_some(action, |row, action| row.child(action))
            .into_any_element()
    }
    fn retry_prs_button(cx: &mut Context<Self>) -> Button {
        Button::new("retry-prs")
            .label("Retry")
            .on_click(cx.listener(|this, _, window, cx| {
                this.load_prs(window, cx);
                cx.notify();
            }))
    }
    fn render_dashboard(&self, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.dashboard_rows();
        let list = match &self.prs {
            Load::Loading if rows.is_empty() => message("Loading pull requests...", None),
            Load::Error(error) if rows.is_empty() => {
                message("Could not load pull requests", Some(error.clone()))
                    .child(Self::retry_prs_button(cx))
            }
            _ if rows.is_empty() => message(
                if self.selected_filter.is_some() {
                    "No pull requests match this repository"
                } else {
                    "No open pull requests"
                },
                None,
            ),
            _ => {
                let mut body = div().size_full().flex().flex_col();
                match &self.prs {
                    Load::Loading => body = body.child(message("Loading pull requests...", None)),
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
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_end()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_xl().font_semibold().child("Pull requests"))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(0x667085))
                                    .child("Your open pull requests and babysits."),
                            ),
                    )
                    .child(div().w(px(260.)).child(
                        Select::new(&self.dashboard_filter).placeholder("All repositories"),
                    )),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .bg(rgb(0xffffff))
                    .border_1()
                    .border_color(rgb(0xe5e9ef))
                    .rounded_lg()
                    .child(list),
            )
            .into_any_element()
    }
}

fn chip(label: &'static str, color: u32, background: u32) -> Div {
    div()
        .px_2()
        .py_1()
        .rounded_full()
        .text_xs()
        .text_color(rgb(color))
        .bg(rgb(background))
        .child(label)
}
fn message(title: &'static str, detail: Option<String>) -> Div {
    div()
        .p_6()
        .flex()
        .flex_col()
        .gap_2()
        .text_sm()
        .text_color(rgb(0x667085))
        .child(div().font_semibold().text_color(rgb(0x344054)).child(title))
        .when_some(detail, |d, text| d.child(text))
}
impl Render for AppView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let nav = |tab: Tab,
                   id: &'static str,
                   label: &'static str,
                   selected: bool,
                   cx: &mut Context<Self>| {
            let button =
                Button::new(id)
                    .label(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.tab = tab;
                        cx.notify();
                    }));
            if selected {
                button.primary()
            } else {
                button.ghost()
            }
        };
        div()
            .size_full()
            .bg(rgb(0xf6f8fb))
            .text_color(rgb(0x101828))
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(70.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_6()
                    .bg(rgb(0xffffff))
                    .border_b_1()
                    .border_color(rgb(0xe5e9ef))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_6()
                            .child(div().font_bold().text_lg().child("baby-sitter"))
                            .child(
                                div()
                                    .flex()
                                    .gap_1()
                                    .child(nav(
                                        Tab::Dashboard,
                                        "dashboard-tab",
                                        "Dashboard",
                                        self.tab == Tab::Dashboard,
                                        cx,
                                    ))
                                    .child(nav(
                                        Tab::Repos,
                                        "repos-tab",
                                        "Repos",
                                        self.tab == Tab::Repos,
                                        cx,
                                    )),
                            ),
                    )
                    .child(Button::new("refresh").label("Refresh").outline().on_click(
                        cx.listener(|this, _, window, cx| {
                            this.refresh(window, cx);
                            cx.notify();
                        }),
                    )),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_6()
                    .when_some(self.error.clone(), |d, error| {
                        d.child(
                            div()
                                .p_3()
                                .rounded_md()
                                .bg(rgb(0xfef3f2))
                                .text_color(rgb(0xb42318))
                                .text_sm()
                                .child(error),
                        )
                    })
                    .child(match self.tab {
                        Tab::Dashboard => self.render_dashboard(cx),
                        Tab::Repos => self.render_repos(cx),
                    }),
            )
    }
}
