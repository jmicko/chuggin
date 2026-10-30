//! A resumable preferences walkthrough. These pages write drafts only: runnable
//! configuration, Git changes and computer preparation follow final confirmation.
use super::*;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Page {
    #[default]
    Mode,
    Model,
    Computer,
    Personality,
    Thinking,
    Sampling,
    Context,
    Output,
    Helpers,
    HelperModel,
    HelperBudget,
    ActiveHours,
    Duration,
    Timeout,
    Extras,
    SetupModel,
    SetupThinking,
    SetupSampling,
    ChatModel,
    ChatThinking,
    ChatSampling,
    HelperThinking,
    HelperSampling,
    Completion,
    CommandReview,
    Responses,
    Retry,
    Web,
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Progress {
    schema_version: u32,
    last_mode: Option<crate::activity::Mode>,
    page: Page,
    advanced: bool,
    reviewed: bool,
}
impl Default for Progress {
    fn default() -> Self {
        Self {
            schema_version: 1,
            last_mode: None,
            page: Page::Mode,
            advanced: true,
            reviewed: false,
        }
    }
}
impl Progress {
    fn sync_mode(&mut self, mode: crate::activity::Mode) {
        if self.last_mode != Some(mode) {
            self.last_mode = Some(mode);
            self.page = Page::Mode;
            self.reviewed = false;
        }
    }
}
fn progress_path(root: &Path) -> PathBuf {
    root.join(".chuggin/setup-wizard.json")
}
fn progress(root: &Path) -> Result<Progress> {
    let value = match fs::read(progress_path(root)) {
        Ok(bytes) => serde_json::from_slice::<Progress>(&bytes)
            .context("Could not read saved setup progress")?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Progress::default(),
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(
        value.schema_version == 1,
        "Saved setup progress uses an unsupported version; project option drafts were retained"
    );
    Ok(value)
}
fn save_progress(root: &Path, value: &Progress) -> Result<()> {
    save(&progress_path(root), value)
}

pub(super) fn clear_progress(root: &Path) -> Result<()> {
    match fs::remove_file(progress_path(root)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn pages(config: &runner::Config, advanced: bool) -> Vec<Page> {
    use Page::*;
    let mut pages = vec![Mode, Model];
    if config.activity_mode.is_chaos() {
        pages.extend([Computer, Personality]);
    }
    pages.extend([Thinking, Sampling, Context, Output, Helpers]);
    if config.helpers.enabled {
        pages.extend([HelperModel, HelperBudget]);
    }
    pages.extend([ActiveHours, Duration, Timeout, Extras]);
    if advanced {
        if !config.activity_mode.is_chaos() {
            pages.extend([SetupModel, SetupThinking, SetupSampling]);
        }
        pages.extend([ChatModel, ChatThinking, ChatSampling]);
        if config.helpers.enabled {
            pages.extend([HelperThinking, HelperSampling]);
        }
        if !config.activity_mode.is_chaos() {
            pages.push(Completion);
        }
        pages.extend([CommandReview, Responses]);
        if !config.activity_mode.is_chaos() {
            pages.push(Retry);
        }
        pages.push(Web);
    }
    pages
}

pub(super) fn progress_total(root: &Path) -> Result<usize> {
    Ok(pages(&draft_config(root)?, progress(root)?.advanced).len())
}

impl Page {
    fn title(self) -> &'static str {
        match self {
            Self::Mode => "Session mode",
            Self::Model => "Loop model",
            Self::Computer => "Computer environment",
            Self::Personality => "Personality",
            Self::Thinking => "Loop thinking",
            Self::Sampling => "Loop sampling",
            Self::Context => "Context window",
            Self::Output => "Response limit",
            Self::Helpers => "Investigation helpers",
            Self::HelperModel => "Helper model",
            Self::HelperBudget => "Helper response allowance",
            Self::ActiveHours => "Active hours",
            Self::Duration => "Run duration",
            Self::Timeout => "Request timeout",
            Self::Extras => "Additional options",
            Self::SetupModel => "Setup model",
            Self::SetupThinking => "Setup thinking",
            Self::SetupSampling => "Setup sampling",
            Self::ChatModel => "Chat model",
            Self::ChatThinking => "Chat thinking",
            Self::ChatSampling => "Chat sampling",
            Self::HelperThinking => "Helper thinking",
            Self::HelperSampling => "Helper sampling",
            Self::Completion => "Goal completion",
            Self::CommandReview => "Command review",
            Self::Responses => "Responses per cycle",
            Self::Retry => "Cycle retry delay",
            Self::Web => "Web tools",
        }
    }
    fn explanation(self) -> &'static str {
        match self {
            Self::Mode => {
                "Work toward a goal refines a project against an objective and validation checks. No goals, only chaos keeps an unassigned conversation going. This choice is fixed after setup; saved drafts can still be changed here."
            }
            Self::Model => {
                "This is the model used by the ongoing loop. A shared default is a starting choice, and you can select another model or provider for this project. Catalog selection does not generate a response."
            }
            Self::Computer => {
                "Choose where the model's tools run. A private computer has a persistent Linux desktop isolated from your home, project files and credentials. It requires Podman. Direct access runs with your account's access in this project. Nothing is prepared or started until you confirm the completed setup."
            }
            Self::Personality => {
                "Baseline keeps the existing unassigned behavior. Editable personality lets the model choose its own tendencies. Personality + interests/questions also keeps self-chosen interests, questions, intentions and reflections. Profiles start blank, remain model-authored, and retain revision history. This does not assign hobbies or a goal."
            }
            Self::Thinking => {
                "Model default lets the provider choose its normal reasoning behavior; it does not force thinking off. Change this to inspect thinking controls advertised by the selected model. Only metadata is requested."
            }
            Self::Sampling => {
                "Sampling controls token variation independently of thinking. Presets are starting points, not guarantees of quality. Model default sends no override. Explicit changes offer only controls supported by the selected provider."
            }
            Self::Context => {
                "The context window is the conversation capacity requested from the provider. Larger settings do not fill it in advance. Use a size supported by the selected model; long histories can use more memory and take longer to process."
            }
            Self::Output => {
                "This allowance covers one generated response, including thinking where the provider counts it. It must fit inside the context window. Provider-specific limits still apply."
            }
            Self::Helpers => {
                "Helpers are optional, fresh-context investigations selected by the main model. They can inspect evidence but cannot edit files or run commands. Enabling them permits extra model requests through the configured connection and its existing usage controls."
            }
            Self::HelperModel => {
                "Use the loop model to share its inference connection, or select a separate investigation model/provider. Helpers keep their own conversation. Provider charges and account limits still apply."
            }
            Self::HelperBudget => {
                "The allowance is the maximum number of model responses in one helper investigation, including tool-use rounds. Findings are retained when the allowance is reached. This does not cap the main loop."
            }
            Self::ActiveHours => {
                "Active hours control when the loop can run. Chat remains available. You can manually resume outside the schedule, and choose whether closing time finishes the current call or the full cycle."
            }
            Self::Duration => {
                "Choose the length of each run. At the limit Chuggin finishes the current cycle and saves before stopping. Zero means unlimited. Resuming starts a new timer."
            }
            Self::Timeout => {
                "This is the deadline for one inference request. Larger/slower models may need a longer deadline. Zero means no request deadline. Provider errors use the normal recovery and backoff controls."
            }
            Self::Extras => {
                "The common options are ready. Next walks through the remaining applicable settings, including assistant models, helper controls and command budgets. You can instead keep their displayed defaults and skip these pages. All choices remain editable later in Project settings."
            }
            Self::SetupModel => {
                "Goal drafting can use a different model from the ongoing loop. A larger model can help shape the goal without changing the loop model. Using the loop model is the default."
            }
            Self::SetupThinking => {
                "Thinking settings for goal drafting are independent of the ongoing loop. Model default preserves the selected setup model's normal behavior."
            }
            Self::SetupSampling => {
                "Sampling settings for goal drafting are independent of the ongoing loop. Changing them does not change the project's loop sampler."
            }
            Self::ChatModel => {
                "The project assistant can use the loop model or a separate model/provider. This does not change the model performing the continuous work."
            }
            Self::ChatThinking => {
                "Thinking settings for the project assistant are independent of the ongoing loop. Model default preserves the chat model's normal behavior."
            }
            Self::ChatSampling => {
                "Sampling settings for the project assistant are independent of the ongoing loop."
            }
            Self::HelperThinking => {
                "Helpers have their own reasoning preference. Model default preserves their selected model's normal behavior and does not turn thinking off."
            }
            Self::HelperSampling => {
                "Helpers have their own sampling preference. The main loop retains its sampler."
            }
            Self::Completion => {
                "Continuous refinement keeps looping after completed tasks. Allowing goal completion adds a tool the model may use when it believes the whole goal is met. It does not tell the model to rush or finish early."
            }
            Self::CommandReview => {
                "Choose the first review point for long-running commands, in seconds. This is a review interval, not a deadline that automatically terminates the process. The watchdog can inspect work that takes longer."
            }
            Self::Responses => {
                "This is the number of main-model responses available in one cycle, including tool-use rounds. Reaching it saves current work and continues on a later cycle; it is not a project-wide iteration limit."
            }
            Self::Retry => {
                "This is the delay between goal-workflow attempts. Provider outages and rate limits use their own persistent recovery and backoff handling. Unassigned sessions continue between responses without this delay."
            }
            Self::Web => {
                "Web search and browsing use your shared Brave configuration. Its API key and enable switch belong in Global settings. This project walkthrough preserves those shared settings and never changes them automatically."
            }
        }
    }
    fn value(self, c: &runner::Config, draft: &Value, progress: &Progress) -> String {
        let model = |name: &str| {
            if name.is_empty() {
                "Use loop model".into()
            } else {
                crate::model_picker::display_model(name)
            }
        };
        match self {
            Self::Mode => c.activity_mode.label().into(),
            Self::Model => {
                if c.model.is_empty() {
                    "Not selected — choose a loop model".into()
                } else {
                    model(&c.model)
                }
            }
            Self::Computer => {
                if draft.get("execution").is_none() {
                    crate::sandbox::Settings::private().label().into()
                } else {
                    c.execution.label().into()
                }
            }
            Self::Personality => c.self_profile.label().into(),
            Self::Thinking => c.reasoning.label(),
            Self::Sampling => c.sampling.label(),
            Self::Context => format!("{} tokens", token_count(c.context_tokens)),
            Self::Output => format!("{} tokens", token_count(c.output_tokens)),
            Self::Helpers => if c.helpers.enabled {
                "Enabled"
            } else {
                "Disabled"
            }
            .into(),
            Self::HelperModel => model(&c.helpers.model),
            Self::HelperBudget => {
                format!("{} model responses per investigation", c.helpers.max_calls)
            }
            Self::ActiveHours => c
                .active_hours
                .at(chrono::Utc::now())
                .map(|s| s.description)
                .unwrap_or_else(|e| format!("Schedule needs attention: {e}")),
            Self::Duration => {
                if c.run_duration_seconds == 0 {
                    "Unlimited".into()
                } else {
                    format!("{} hours", c.run_duration_seconds as f64 / 3600.0)
                }
            }
            Self::Timeout => {
                if c.request_timeout_seconds == 0 {
                    "Unlimited".into()
                } else {
                    format!("{} minutes", c.request_timeout_seconds as f64 / 60.0)
                }
            }
            Self::Extras => if progress.advanced {
                "Additional options will follow"
            } else {
                "Keep remaining defaults"
            }
            .into(),
            Self::SetupModel => model(&c.setup_model),
            Self::SetupThinking => c.setup_reasoning.label(),
            Self::SetupSampling => c.setup_sampling.label(),
            Self::ChatModel => model(&c.chat_model),
            Self::ChatThinking => c.chat_reasoning.label(),
            Self::ChatSampling => c.chat_sampling.label(),
            Self::HelperThinking => c.helpers.reasoning.label(),
            Self::HelperSampling => c.helpers.sampling.label(),
            Self::Completion => if c.allow_goal_completion {
                "Model may mark the overall goal complete"
            } else {
                "Keep the loop running"
            }
            .into(),
            Self::CommandReview => format!("{} seconds", c.command_review_seconds),
            Self::Responses => format!("{} model responses", c.implementation_calls),
            Self::Retry => format!("{} seconds", c.retry_seconds),
            Self::Web => "Uses shared Brave configuration".into(),
        }
    }
    fn actions(self) -> Vec<String> {
        let options: &[&str] = match self {
            Self::Mode => &["Work toward a goal", "No goals, only chaos"],
            Self::Computer => &["Private computer (Podman)", "This computer · direct access"],
            Self::Personality => &[
                "Baseline · no self-profile",
                "Editable personality",
                "Personality + interests/questions",
            ],
            Self::Helpers => &[
                "Enable investigation helpers",
                "Disable investigation helpers",
            ],
            Self::Completion => &[
                "Keep the loop running",
                "Allow the model to mark the overall goal complete",
            ],
            Self::Extras => &["Show additional options…"],
            Self::Web => &[],
            _ => &["Change…"],
        };
        options.iter().map(|v| (*v).to_owned()).collect()
    }
}

fn current_patch(page: Page, c: &runner::Config, draft: &Value) -> Value {
    match page {
        Page::Mode => {
            if c.activity_mode.is_chaos() {
                json!({"activity_mode":c.activity_mode,"allow_goal_completion":false})
            } else {
                json!({"activity_mode":c.activity_mode,"execution":crate::sandbox::Settings::Host})
            }
        }
        Page::Model => json!({"model":c.model,"ollama_url":c.ollama_url}),
        Page::Computer => {
            json!({"execution":if draft.get("execution").is_none(){crate::sandbox::Settings::private()}else{c.execution.clone()}})
        }
        Page::Personality => json!({"self_profile":c.self_profile}),
        Page::Thinking => json!({"reasoning":c.reasoning}),
        Page::Sampling => json!({"sampling":c.sampling}),
        Page::Context => json!({"context_tokens":c.context_tokens}),
        Page::Output => json!({"output_tokens":c.output_tokens}),
        Page::Helpers
        | Page::HelperModel
        | Page::HelperBudget
        | Page::HelperThinking
        | Page::HelperSampling => json!({"helpers":c.helpers}),
        Page::ActiveHours => json!({"active_hours":c.active_hours}),
        Page::Duration => json!({"run_duration_seconds":c.run_duration_seconds}),
        Page::Timeout => json!({"request_timeout_seconds":c.request_timeout_seconds}),
        Page::SetupModel => json!({"setup_model":c.setup_model}),
        Page::SetupThinking => json!({"setup_reasoning":c.setup_reasoning}),
        Page::SetupSampling => json!({"setup_sampling":c.setup_sampling}),
        Page::ChatModel => json!({"chat_model":c.chat_model}),
        Page::ChatThinking => json!({"chat_reasoning":c.chat_reasoning}),
        Page::ChatSampling => json!({"chat_sampling":c.chat_sampling}),
        Page::Completion => json!({"allow_goal_completion":c.allow_goal_completion}),
        Page::CommandReview => json!({"command_review_seconds":c.command_review_seconds}),
        Page::Responses => json!({"implementation_calls":c.implementation_calls}),
        Page::Retry => json!({"retry_seconds":c.retry_seconds}),
        Page::Extras | Page::Web => json!({}),
    }
}

fn role_model(root: &Path, c: &runner::Config, field: &str) -> Result<()> {
    let (name, sampling, title) = match field {
        "setup_model" => (
            &c.setup_model,
            &c.setup_sampling,
            "Setup model · this project",
        ),
        "chat_model" => (&c.chat_model, &c.chat_sampling, "Chat model · this project"),
        _ => (
            &c.helpers.model,
            &c.helpers.sampling,
            "Helper model · this project",
        ),
    };
    let Some(selection) = role_model_choice(name, &c.model, &c.ollama_url, title)? else {
        return Ok(());
    };
    let effective = if selection.model.is_empty() {
        &c.model
    } else {
        &selection.model
    };
    let Some(next) = sampling_for_model(
        sampling,
        effective,
        selection.local_url.as_deref().unwrap_or(&c.ollama_url),
    )?
    else {
        return Ok(());
    };
    let mut patch = json!({});
    if field == "helpers" {
        let mut helpers = c.helpers.clone();
        helpers.model = selection.model;
        helpers.sampling = next;
        patch["helpers"] = json!(helpers);
    } else {
        patch[field] = json!(selection.model);
        patch[if field == "setup_model" {
            "setup_sampling"
        } else {
            "chat_sampling"
        }] = json!(next);
    }
    if let Some(url) = selection.local_url {
        patch["ollama_url"] = json!(url);
    }
    save_draft_patch(root, patch)
}
fn thinking(root: &Path, c: &runner::Config, page: Page) -> Result<()> {
    let (current, name, key) = match page {
        Page::SetupThinking => (
            &c.setup_reasoning,
            if c.setup_model.is_empty() {
                &c.model
            } else {
                &c.setup_model
            },
            "setup_reasoning",
        ),
        Page::ChatThinking => (
            &c.chat_reasoning,
            if c.chat_model.is_empty() {
                &c.model
            } else {
                &c.chat_model
            },
            "chat_reasoning",
        ),
        Page::HelperThinking => (
            &c.helpers.reasoning,
            if c.helpers.model.is_empty() {
                &c.model
            } else {
                &c.helpers.model
            },
            "helpers",
        ),
        _ => (&c.reasoning, &c.model, "reasoning"),
    };
    if let Some(next) =
        thinking_choice(current, name, &c.ollama_url, c.output_tokens, page.title())?
    {
        let mut patch = json!({});
        if key == "helpers" {
            let mut helpers = c.helpers.clone();
            helpers.reasoning = next;
            patch[key] = json!(helpers);
        } else {
            patch[key] = json!(next);
        }
        save_draft_patch(root, patch)?;
    }
    Ok(())
}
fn sampling(root: &Path, c: &runner::Config, page: Page) -> Result<()> {
    let (current, name, key) = match page {
        Page::SetupSampling => (
            &c.setup_sampling,
            if c.setup_model.is_empty() {
                &c.model
            } else {
                &c.setup_model
            },
            "setup_sampling",
        ),
        Page::ChatSampling => (
            &c.chat_sampling,
            if c.chat_model.is_empty() {
                &c.model
            } else {
                &c.chat_model
            },
            "chat_sampling",
        ),
        Page::HelperSampling => (
            &c.helpers.sampling,
            if c.helpers.model.is_empty() {
                &c.model
            } else {
                &c.helpers.model
            },
            "helpers",
        ),
        _ => (&c.sampling, &c.model, "sampling"),
    };
    if let Some(next) = sampling_choice(current, name, &c.ollama_url, page.title())? {
        let mut patch = json!({});
        if key == "helpers" {
            let mut helpers = c.helpers.clone();
            helpers.sampling = next;
            patch[key] = json!(helpers);
        } else {
            patch[key] = json!(next);
        }
        save_draft_patch(root, patch)?;
    }
    Ok(())
}
fn change(root: &Path, page: Page, choice: usize, c: &runner::Config) -> Result<()> {
    use Page::*;
    match page {
        Mode => save_draft_patch(
            root,
            if choice == 0 {
                json!({"activity_mode":"goal","execution":crate::sandbox::Settings::Host})
            } else {
                json!({"activity_mode":"type2","allow_goal_completion":false})
            },
        )?,
        Model => choose_project_model(None)?,
        Computer => save_draft_patch(
            root,
            json!({"execution":if choice==0{crate::sandbox::Settings::private()}else{crate::sandbox::Settings::Host}}),
        )?,
        Personality => {
            let variants = [
                crate::self_profile::Variant::Baseline,
                crate::self_profile::Variant::Personality,
                crate::self_profile::Variant::SelfDirected,
            ];
            save_draft_patch(root, json!({"self_profile":variants[choice]}))?;
        }
        Thinking | SetupThinking | ChatThinking | HelperThinking => thinking(root, c, page)?,
        Sampling | SetupSampling | ChatSampling | HelperSampling => sampling(root, c, page)?,
        Context | Output => {
            let context = page == Context;
            let Some(tokens) = token_selector(
                page.title(),
                if context {
                    c.context_tokens
                } else {
                    c.output_tokens
                },
                if context {
                    runner::MIN_CONTEXT_TOKENS
                } else {
                    256
                },
                if context {
                    u32::MAX
                } else {
                    c.context_tokens - 1
                },
                context,
            )?
            else {
                return Ok(());
            };
            save_draft_patch(
                root,
                setting_patch(c, if context { 5 } else { 6 }, &tokens.to_string())?,
            )?;
        }
        Helpers => {
            let mut helpers = c.helpers.clone();
            helpers.enabled = choice == 0;
            save_draft_patch(root, json!({"helpers":helpers}))?;
        }
        HelperModel => role_model(root, c, "helpers")?,
        HelperBudget => {
            let mut helpers = c.helpers.clone();
            helpers.max_calls = number("Model responses per helper", helpers.max_calls, 1, 1000)?;
            save_draft_patch(root, json!({"helpers":helpers}))?;
        }
        ActiveHours => project_active_hours_menu(None)?,
        Duration => project_run_duration(None)?,
        Timeout => {
            let text = ask(
                "Request timeout (minutes; 0 unlimited)",
                &(c.request_timeout_seconds as f64 / 60.0).to_string(),
            )?;
            save_draft_patch(root, setting_patch(c, 1, &text)?)?;
        }
        SetupModel => role_model(root, c, "setup_model")?,
        ChatModel => role_model(root, c, "chat_model")?,
        Completion => save_draft_patch(root, json!({"allow_goal_completion":choice==1}))?,
        CommandReview => {
            let seconds = number(
                "First command review (seconds)",
                c.command_review_seconds as u32,
                1,
                86400,
            )?;
            save_draft_patch(root, json!({"command_review_seconds":seconds}))?;
        }
        Responses => {
            let count = number(
                "Main-model responses per cycle",
                c.implementation_calls,
                1,
                100,
            )?;
            save_draft_patch(root, json!({"implementation_calls":count}))?;
        }
        Retry => {
            let seconds = number(
                "Cycle retry delay (seconds)",
                c.retry_seconds as u32,
                1,
                3600,
            )?;
            save_draft_patch(root, json!({"retry_seconds":seconds}))?;
        }
        Extras | Web => {}
    }
    Ok(())
}

/// Return false when the user saves/exits. Existing global model defaults never
/// skip the model page; an unassigned session always gets a personality page.
pub(super) fn configure(root: &Path) -> Result<bool> {
    let mut state = progress(root)?;
    state.sync_mode(draft_config(root)?.activity_mode);
    save_progress(root, &state)?;
    if state.reviewed {
        return Ok(true);
    }
    loop {
        let mut c = draft_config(root)?;
        state.sync_mode(c.activity_mode);
        let mut draft = draft_settings(root)?;
        let plan = pages(&c, state.advanced);
        let index = plan.iter().position(|p| *p == state.page).unwrap_or(0);
        state.page = plan[index];
        save_progress(root, &state)?;
        if state.page == Page::Sampling
            && c.activity_mode.is_chaos()
            && draft.get("sampling").is_none()
            && matches!(c.sampling, crate::sampling::Settings::Legacy)
            && !c.model.is_empty()
        {
            let config = c.clone();
            let default = crate::ui::busy(
                "Checking default sampling controls (no generation)",
                move || Ok(chaos_sampling_default(&config)),
            )?;
            save_draft_patch(root, json!({"sampling":default}))?;
            c = draft_config(root)?;
            draft = draft_settings(root)?;
        }
        let mut choices = vec!["Next".to_string()];
        let actions = if state.page == Page::Extras && state.advanced {
            vec!["Skip additional options · keep current values".into()]
        } else {
            state.page.actions()
        };
        choices.extend(actions.iter().cloned());
        choices.extend(["Back".into(), "Save and exit".into()]);
        let back = actions.len() + 1;
        let current = if state.page == Page::Web {
            format!(
                "Brave web tools: {} · API key {}",
                if settings()?.web_enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                if crate::web_tools::key()?.is_some() {
                    "configured"
                } else {
                    "not configured"
                }
            )
        } else {
            state.page.value(&c, &draft, &state)
        };
        let selected = crate::ui::wizard_step(
            state.page.title(),
            index + 1,
            plan.len() + if c.activity_mode.is_chaos() { 1 } else { 3 },
            &current,
            state.page.explanation(),
            &choices,
            0,
        )?;
        match selected {
            None => {
                save_progress(root, &state)?;
                return Ok(false);
            }
            Some(choice) if choice == choices.len() - 1 => {
                save_progress(root, &state)?;
                return Ok(false);
            }
            Some(choice) if choice == back => {
                if index == 0 {
                    return Ok(false);
                }
                state.page = plan[index - 1];
            }
            Some(0) => {
                if state.page == Page::Model && c.model.is_empty() {
                    choose_project_model(None)?;
                    continue;
                }
                save_draft_patch(root, current_patch(state.page, &c, &draft))?;
                if let Some(next) = plan.get(index + 1) {
                    state.page = *next;
                } else {
                    state.reviewed = true;
                    save_progress(root, &state)?;
                    return Ok(true);
                }
            }
            Some(choice) if state.page == Page::Extras && choice == 1 => {
                if state.advanced {
                    state.advanced = false;
                    state.reviewed = true;
                    save_progress(root, &state)?;
                    return Ok(true);
                } else {
                    state.advanced = true;
                    state.page = if c.activity_mode.is_chaos() {
                        Page::ChatModel
                    } else {
                        Page::SetupModel
                    };
                }
            }
            Some(choice) => {
                if let Err(error) = change(root, state.page, choice - 1, &c) {
                    crate::ui::show(
                        "Could not update this option",
                        &format!(
                            "{error:#}\n\nYour setup draft is saved. You can change the option again or continue with its current value."
                        ),
                    )?;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changing_mode_reopens_reviewed_preferences_and_normalizes_the_computer() {
        let root = tempfile::tempdir().unwrap();
        let mut c = fixture(root.path(), true, false);
        let mut state = Progress {
            last_mode: Some(crate::activity::Mode::Chaos),
            page: Page::Extras,
            reviewed: true,
            ..Progress::default()
        };
        save_progress(root.path(), &state).unwrap();
        state = progress(root.path()).unwrap();
        state.sync_mode(c.activity_mode);
        assert!(
            state.reviewed,
            "An unchanged mode preserves accepted option pages"
        );
        c.activity_mode = crate::activity::Mode::Goal;
        c.execution = crate::sandbox::Settings::private();
        state.sync_mode(c.activity_mode);
        assert_eq!(state.page, Page::Mode);
        assert!(!state.reviewed);
        assert_eq!(
            current_patch(Page::Mode, &c, &json!({}))["execution"]["kind"],
            "host"
        );
        assert!(pages(&c, false).contains(&Page::Model));
        assert!(!pages(&c, false).contains(&Page::Personality));
        c.activity_mode = crate::activity::Mode::Chaos;
        state.sync_mode(c.activity_mode);
        assert!(pages(&c, false).contains(&Page::Personality));
        assert!(pages(&c, false).contains(&Page::Computer));
    }
    fn fixture(root: &Path, chaos: bool, helpers: bool) -> runner::Config {
        let defaults = Settings {
            model: "preset-loop-model".into(),
            ..Default::default()
        };
        draft_config_with(
            root,
            &defaults,
            &json!({"activity_mode":if chaos{"type2"}else{"goal"},"helpers":{"enabled":helpers}}),
        )
        .unwrap()
    }
    #[test]
    fn preset_models_and_unassigned_personality_are_always_pages() {
        let root = tempfile::tempdir().unwrap();
        for chaos in [false, true] {
            for helpers in [false, true] {
                let c = fixture(root.path(), chaos, helpers);
                let plan = pages(&c, false);
                assert_eq!(plan[0], Page::Mode);
                assert_eq!(plan[1], Page::Model);
                assert_eq!(plan.contains(&Page::Personality), chaos);
                assert_eq!(plan.contains(&Page::Computer), chaos);
                assert_eq!(plan.contains(&Page::HelperModel), helpers);
                assert_eq!(plan.contains(&Page::HelperBudget), helpers);
                assert_eq!(plan.last(), Some(&Page::Extras));
            }
        }
    }
    #[test]
    fn all_applicable_controls_are_reviewed_by_default_and_can_be_skipped() {
        assert!(Progress::default().advanced);
        let root = tempfile::tempdir().unwrap();
        for chaos in [false, true] {
            let c = fixture(root.path(), chaos, true);
            let basic = pages(&c, false);
            let advanced = pages(&c, true);
            assert!(advanced.starts_with(&basic));
            assert!(!basic.contains(&Page::SetupModel));
            assert_eq!(advanced.contains(&Page::SetupModel), !chaos);
            assert_eq!(advanced.contains(&Page::SetupThinking), !chaos);
            assert_eq!(advanced.contains(&Page::Retry), !chaos);
            assert!(advanced.contains(&Page::ChatThinking));
            assert!(advanced.contains(&Page::HelperSampling));
            assert_eq!(advanced.contains(&Page::Completion), !chaos);
            assert_eq!(advanced.last(), Some(&Page::Web));
            for page in advanced {
                assert!(!page.title().is_empty());
                assert!(!page.explanation().is_empty());
            }
        }
    }
    #[test]
    fn progress_resume_and_cleanup_preserve_preferences_without_creating_a_project() {
        let root = tempfile::tempdir().unwrap();
        let drafts = json!({"model":"preset-loop-model","activity_mode":"type2","self_profile":"self_directed"});
        save(&draft_settings_path(root.path()), &drafts).unwrap();
        let state = Progress {
            page: Page::Personality,
            advanced: true,
            ..Default::default()
        };
        save_progress(root.path(), &state).unwrap();
        let resumed = progress(root.path()).unwrap();
        assert_eq!(resumed.page, Page::Personality);
        assert!(resumed.advanced);
        assert!(!resumed.reviewed);
        clear_progress(root.path()).unwrap();
        assert_eq!(draft_settings(root.path()).unwrap(), drafts);
        assert!(!root.path().join("chuggin.json").exists());
        assert!(!root.path().join(".chuggin/state.json").exists());
        assert!(!root.path().join(".chuggin/computer").exists());
    }
    #[test]
    fn accepted_defaults_are_explicit_and_mode_changes_cannot_leave_private_goal_projects() {
        let root = tempfile::tempdir().unwrap();
        let c = fixture(root.path(), true, false);
        assert_eq!(
            current_patch(Page::Computer, &c, &json!({}))["execution"]["kind"],
            "podman"
        );
        assert_eq!(
            current_patch(Page::Personality, &c, &json!({}))["self_profile"],
            "baseline"
        );
        assert_eq!(
            current_patch(Page::Model, &c, &json!({}))["model"],
            "preset-loop-model"
        );
        let mut goal = c;
        goal.activity_mode = crate::activity::Mode::Goal;
        goal.execution = crate::sandbox::Settings::private();
        assert_eq!(
            current_patch(Page::Mode, &goal, &json!({}))["execution"]["kind"],
            "host"
        );
    }
}
