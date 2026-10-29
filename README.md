# Chuggin

A Rust terminal tool for advancing software, document, and data projects with local and cloud models.
It keeps working, checking results, and refining the same project.

**Save the work. Test it. Keep improving it.**

## Install

Chuggin is currently tested on Linux. Install a recent stable Rust toolchain and
Git, and a tool-capable model through Ollama or a supported cloud provider.

Chuggin uses your configured Git `user.name` and `user.email` for commits,
including repository overrides of global settings. If either is missing,
interactive setup or resume asks for it and saves it in the project's Git
configuration before starting work. Noninteractive runs stop with setup
instructions. Use a GitHub verified email or your GitHub noreply email for
GitHub attribution; GitHub login is not required for local commits.

~~~sh
cargo install chuggin --locked
~~~

Or install from a checkout of this repository:

~~~sh
cargo install --path . --locked
~~~

This installs one Rust executable into Cargo's bin directory (normally
~/.cargo/bin). Make sure that directory is on PATH. Chuggin itself needs no Python runtime.

## Start

~~~sh
cd your-project
chuggin
~~~

The arrow-key menu highlights **Set up this project** or **Resume project**.
Model selection, Settings, the accepted goal, Progress, and Quit are also in
the menu. No flags are needed for everyday use.

## Full-screen observation

The interactive interface uses the terminal's alternate screen, with a large
splash screen, in-place settings and goal drafting, and a live run dashboard.
The dashboard shows the current task and stage, model output as it arrives,
tool actions, check output, recovery messages, and recent outcomes.

- **1–7 / Tab:** switch between live activity, model output, checks, the goal, local settings, the assistant, and messages.
- **M:** open direct messages with the looping model. The status row shows new messages and your availability.
- **Arrow keys / mouse wheel / Page Up / Page Down:** scroll without pausing work.
  Scrolling back to the bottom automatically resumes following live output.
- **Home:** oldest retained output. **End / F:** follow live output again.
- **/** searches the current output view; **Esc** clears the filter.
- **?** opens the keyboard guide.
- **P:** pause inside the current cycle, then press P again to continue where it left off.
  An active model response or tool operation finishes before the pause takes effect.
- **T:** retry a waiting provider now. While paused, queues the retry for resume.
- **Ctrl+C / Q:** finish the current cycle. **Ctrl+C again:** force stop.
- **R:** cancel a pending stop or resume directly after the run finishes.
  When the run finishes, Enter returns to the splash screen.

The side panel shows request counts, recent checkpoints and findings, context usage,
and generation speed. Token counts and speed come from the selected provider after each completed
response, not estimates during streaming. CPU, RAM, and process memory describe
the local Linux computer; they do not measure the remote server's GPU.

The display redraws at up to ten frames per second and keeps 2,000 output lines.
Its bounded event stream cannot block the agent; full diagnostic logs remain on
disk. Narrow windows hide the side panel, and resizing does not stop the run.
Shell settings and the previous terminal screen are restored on exit.
Terminal window titles start with the project folder, such as
`werd-ornith-35b · Chuggin`, to distinguish runs in the taskbar. Compatible terminals
restore the previous title when Chuggin exits.

Updating the executable does not change an already-running process. The new UI
appears the next time you launch Chuggin. Explicit `chuggin run` commands retain plain
output for scripts and redirected logs.

New installations have no model preselected. **Project model** opens a provider
selector, followed by the provider's live model catalog. Use arrow keys to choose,
**/** to search, **R** to refresh, **C** for the connection, and **Esc** to return to
providers. **PgUp/PgDn** pages through the list; **Alt+PgUp/PgDn** scrolls model
details. Refresh works without restarting or updating Chuggin. Each model shows
its pricing category and any context/output limits supplied by the catalog.
**A** shows unavailable, paid, or unconfirmed entries with an explanation; paid or
unconfirmed Zen/OpenRouter entries cannot be selected. Free pricing does not
promise unlimited access or remaining account allowance.

Configure all connections through **Global settings → Inference providers**:
Ollama, Groq, OpenAI (ChatGPT plan), OpenCode Zen, and OpenRouter use the same menu.
Ollama defaults to **http://localhost:11434**; enter a remote server address there.
**Add Ollama connection** saves another server with a name, such as “This computer”
or “GPU server”; both remain available in the provider selector. **Add custom
OpenAI-compatible connection** accepts a name, API base URL (including any `/v1`
or proxy path), and an optional API key. Each saved connection has its own live
model catalog, refresh, connection test, and edit menu. If a custom API does not
list models, use **M** or the manual model ID option in its model picker.
Renaming a connection preserves project selections. Removing one reports a
missing connection in projects using it; Chuggin never silently redirects work
to another server. Existing Ollama settings remain available.

Cloud-only and saved-connection projects do not need the legacy Ollama URL.
Connections are global, while project, setup, chat, and helper model choices
remain separate. Saved selections reference a stable connection ID and raw model
ID; credentials stay in separate protected files in the user's Chuggin config
directory, outside project configuration and request traces. Explicit
connection edits persist globally; backing out of a model picker does not change
model choices. Existing saved settings take precedence. Brave is optional and
disabled until configured.

### Sign in with ChatGPT

Choose **OpenAI · ChatGPT plan → Account → Continue with ChatGPT** in Inference
providers. Chuggin opens the official authorization page in your browser, then
receives its callback on a temporary localhost port. Eligible Plus/Pro users can
authorize their existing plan allowance; this does not provide additional usage.
After authorization, select a model from your account's live catalog. Models are
saved as `chatgpt/<model-id>`, and model calls use OpenAI's public Responses API.
No Codex installation, API key, or Python runtime is needed.

Credentials are stored separately from project files, in owner-only account
records under the global Chuggin configuration directory. Registrations remain
separate even when they share an email address. Tokens renew automatically;
authentication expiry, provider outages, and exhausted plan usage go through
normal provider recovery. Disconnect locally from the Account menu, and manage
or revoke Chuggin's authorization in ChatGPT settings. Tokens are never added to
model prompts or request traces.

ChatGPT plan requests are stateless and stream their output. Complete tool calls
are released only after a successful completed response. The conversation,
client-side tools, and saved progress remain in Chuggin. Thinking options are
read from the account model catalog. OpenAI controls response length for this
connection: Chuggin does not send its response-token cap or sampling overrides.
See the [official Sign in with ChatGPT documentation](https://developers.openai.com/siwc/quickstart)
for plan eligibility and usage controls.

Setup offers guided goals for rewriting software in Rust, building new software,
and research. The questionnaires produce an editable goal in a fixed format,
without calling a model. Rust rewrites record where the original software can be
found, including local files, archives or ISOs, a URL, or a request to locate it.
You can also write a goal directly, or discuss it with an AI that can inspect the
project using read-only tools. The AI proposes a goal for you to accept, edit, or
reject; its conversation and drafts persist across restarts. Accepting a goal
does not start the loop.

Choose a validation command, then Resume. Empty projects get an initial Git commit. For existing
files without commits, setup offers to commit project files while respecting
Git ignores and excluding Chuggin's state.

First Ctrl-C requests a stop after the current cycle. Press R to cancel that
request, or press R on the finished screen to continue from saved progress.
Second Ctrl-C stops immediately. Subsequent runs resume the persistent working project.
During setup, a single Ctrl-C exits.

For shorter interruptions, press **P** in the observation view. **PAUSING** means
the current model response or tool operation is finishing; **PAUSED IN CYCLE**
means no further model requests or tools will start until you press P again.
This preserves the current cycle, conversation, and pending tool calls without
starting a new session. Already-started commands may still finish; pausing does
not suspend operating-system processes. The run timer stops while the cycle is
held. Pressing P again while a pause is pending cancels it. Ctrl+C or Q releases
the held continuation and saves without launching new work. Other holds, such as
active hours or operator editing, remain independent of the manual pause.

Choose **Project settings → Run duration** from the welcome screen to set a project-specific duration
in hours (fractional hours work; 0 means unlimited). The observation view shows
the time remaining. When the limit is reached, Chuggin finishes the current cycle,
saves its outcome, and stops before starting another. This is a soft limit, so
the run can exceed the chosen duration by the remaining cycle time. Resuming
starts a new timer. The setting persists as `run_duration_seconds` in `chuggin.json`
and also applies to command-line runs.

## No goals, only chaos

Setup first offers **Work toward a goal** or **No goals, only chaos**. The latter
starts a persistent conversation with tools and an environment, without an
assigned objective. It skips goal drafting and a mandatory validation command.
The name is a label in the interface; the model receives neutral environment and
tool instructions rather than a request to act chaotically.
The saved mode identifier is `type2`; older `chaos` identifiers remain readable.
Mode identifiers are omitted from model-facing startup, resume, helper, and
diagnostic messages. Use a neutral workspace directory name for an experiment:
paths and local configuration remain discoverable when using the host computer.
The private-computer option below keeps Chuggin's configuration outside the
model's filesystem.

Every response boundary schedules another opportunity to generate, using the
saved conversation. Text-only responses, silence, and no file changes do not end
the session. There is no deliberate delay between cycles. Human pause, stop,
active hours, timers, and provider backoff still apply. The existing repeated-text,
repeated-thinking, and action-loop detectors and recovery remain enabled.

Tasks are optional records of activities the model chooses. Finishing one never
ends the session. With no configured checks, completion is recorded as a model
report and validation reads **No checks configured**, never a passing result.
If checks are explicitly configured, their existing completion gates apply.
This mode does not initialize Git, request a commit identity, create recovery
snapshots or commits, or restrict branches. Existing repositories and their history
are left intact. Conversation, tool results, and activity state persist normally;
file hashes detect changes independently of Git. Hash records are not file backups.
The model can use Git through shell commands if it chooses. Mode is fixed
for a saved session to avoid silently mixing its history with goal-driven
instructions; start in a new directory to choose another mode.

### Self-profile experiment

Unassigned sessions can optionally keep a profile written by the model itself.
Choose **Project settings → More project settings → Self-profile experiment**
(also available through observation tab **5 Settings**). The variants are:

- **Baseline:** the existing unassigned conversation, with no profile tools.
- **Editable personality:** a blank list of tendencies the model may choose and
  revise. No initial traits are assigned.
- **Personality + interests/questions:** blank tendencies, interests, open
  questions, intentions and reflections. The model may choose activities without
  waiting for instructions, change its mind, or remain quiet. There is no required
  hobby, productivity target or outcome.

Existing and new sessions start on Baseline until you choose otherwise. Each
enabled variant keeps a separate profile and immutable revision history. The
settings menu can show the current profile and its revisions, including when
Baseline is selected. `read_self_profile` and `update_self_profile` let the model
inspect and revise its own entries, with a reason and optional evidence. Stale
writes are rejected and retries do not create duplicate revisions.

The profile is fallible model-authored data, kept separate from human directions
and tool permissions. Its current snapshot is provided on startup and after a
context handoff; updates append only when its revision changes, preserving the
conversation prefix for provider caching. There is no separate inference agent
or extra model request for profile maintenance. Repetition protection, pause,
active hours and provider recovery continue to apply.

Switching variants preserves ordinary conversation, files and previous tool
results. For comparisons starting from the same conditions, use separate fresh
session directories rather than switching an established conversation.

### Private Linux computer (Podman)

New unassigned sessions offer two choices: **Private computer (Podman)** or
**This computer · direct access**. Choosing the private computer automatically
checks prerequisites, downloads/builds the desktop and verifies resource limits;
there is no separate check or preparation step. It gives the model its own
persistent Debian installation,
starting in `/root`, with an Xfce desktop. It can use system directories and
install additional applications. The default desktop already includes LibreOffice
Writer, Calc, Impress and Draw; GIMP; Inkscape; Audacity; Blender; Atril for PDFs;
Mousepad; Xarchiver; and Parole, alongside Chromium and the file manager. Fonts,
emoji, spellcheck, archive formats and desktop settings are included. Application
inventory exposes what is installed without assigning an activity to the model.
Host project files are not imported.
Existing host sessions retain their environment; start a new session directory
to use the private computer.

Podman is a separate, user-installed prerequisite. If it is absent, Chuggin
shows installation instructions for your OS and **Check again**. It installs
nothing and never falls back to running model commands on the host. This backend
currently requires Linux, rootless Podman, and working cgroup v2 resource limits.
For example, Debian, Ubuntu and Linux Mint use `sudo apt install podman`;
[Podman's installation guide](https://podman.io/docs/installation) covers other systems
and rootless prerequisites. The macOS/Windows CLI alone is insufficient.

**Copy install command to clipboard** copies just the command for pasting into
another terminal with Ctrl+Shift+V. Keep Chuggin open while pasting. Copying runs
nothing. When a desktop clipboard is unavailable, Chuggin can request clipboard
access through the terminal and reports that the request is unconfirmed.

Resume automatically completes setup if the private computer is not ready.
Chuggin creates the installation and verifies CPU, memory and process limits
itself, using defaults of 4 CPUs, 4 GiB RAM and public web access. Changing models
or editing settings does not require a prepared computer. Host configuration
commands are never silently run.

Setup problems show the actual error and copyable repair commands; missing Podman
shows its installation command. Successful setup continues without extra menus.
Build output streams in the terminal and is saved in the session's
`computer/build.log`. **Project settings → Project tools → Open desktop** or the highlighted,
clickable **F4 Open desktop** button at the top of the observation view opens a
local, token-protected noVNC viewer in a separate browser tab, without pausing
the model. **Alt+D** also opens it. The desktop screen offers **Open browser again**
and **Copy desktop link** if your browser does not appear. Reopening reuses the same
viewer address while Chuggin stays open. **Project settings → More project settings → Computer environment → Advanced computer
settings** holds resource limits and optional setup checks. The model has
`capture_screenshot` and `desktop_action` tools for its private display; inspecting
pixels requires a vision-capable inference model. Neither the human desktop nor
its display socket is shared. Initial rendering is software-only, with no GPU
passthrough. The desktop image uses distribution packages, including websockify;
their own runtime dependencies are confined to the computer.

Shell, file, image and desktop tools execute inside the container, including
tools used by Chat, MCP, and investigation helpers. Only a read-only copy of the
Rust tool executable/libraries and a dedicated proxy socket directory are mounted.
Host home directories, project/config files, provider credentials, container
management sockets and devices are not shared. External harnesses' **own** shell
or file tools remain the responsibility of those harnesses.

The container has no direct network connection. Optional public-web access goes
through an HTTP/HTTPS proxy that rejects loopback, LAN, link-local and other
nonpublic destinations before connecting. Inference and Brave credentials remain
in the host controller. Container-local services bypass the proxy through
`NO_PROXY=localhost,127.0.0.1,::1`, so applications can test their own local servers.
This does not expose the host's loopback interface. Applications that cannot use an HTTP proxy may not have
Internet access. **Computer resource limits** controls CPU, memory and public-web
access; start/prepare applies the settings and verifies enforcement before tools
are allowed. Defaults are 4 CPUs, 4 GiB RAM and 512 processes. Container storage
shares your host disk; this version has no hard disk quota. Containers share the
host kernel and do not provide a VM's separate-kernel isolation.

The private installation and its files survive restarts. Pausing the model does
not suspend its desktop or already-started applications. Use **Shut down private
computer · keep its files** to stop it after finishing the loop and commands;
resume/prepare starts it again. Conversation history and file-hash observations
stay outside the container; these records are not backups of its contents.
New installations reuse a desktop image identified by its embedded build assets;
application-only updates do not rebuild that image. Existing installations receive
missing desktop applications on their next idle setup/resume, using their current
container and files. A running controller must stop before installation can begin.
Setup streams progress and saves `computer/desktop-apps.log` when packages are
needed; it preserves existing configuration files and refuses package removals.
It does not perform a full system upgrade. Completion is recorded outside the
computer, so later starts respect applications the model deliberately removes.

Audio applications have a private virtual output and loopback input for editing,
playback and recording inside the computer. No host microphone, speaker or sound
device is shared. The noVNC desktop viewer currently displays video only; it does
not stream that private audio to the human browser. Desktop applications share
one private session bus and clipboard. CPU/memory limits and software rendering
remain configurable separately from the installed applications.

Alongside tool-routing, isolation-policy and missing-dependency tests, a real
rootless Podman integration test exercises setup, private file edits, screenshots,
pointer controls and blocked host/local-network access. It is opt-in and requires
Podman and the desktop image; the ordinary test suite uses simulated providers.

### Empty provider replies

In goal mode, an empty reply preserves the complete conversation. The next
request ends with a direct continuation after the cycle's context updates.
Repeated empty replies use increasing delays capped at 15 minutes; they do not
trigger an automatic conversation reset or replay tools. Raw replies remain in
diagnostic history. Context-capacity handoffs and repetition recovery are separate
mechanisms. Unassigned sessions continue to allow deliberate silence.

### Sampling profiles

**Settings → Loop sampling** offers Provider default, Focused (temperature 0.4),
Exploratory (0.9), Wildcard (1.1), Custom, and Legacy. Presets change only
temperature; their values are starting points, not universal recommendations.
New Chaos sessions select Exploratory where the provider supports it, otherwise
Provider default. Established projects retain Legacy behavior until changed.
Provider default sends no sampling overrides, including for structured replies.

Custom exposes supported temperature, top-p, top-k, min-p, repetition, presence,
frequency penalty, and seed controls. Type **default** to remove one override,
or reset all. Provider support differs; unavailable controls are explained rather
than silently dropped. A model change with incompatible loop sampling offers an
explicit reset to provider defaults before saving. Thinking effort remains an
independent setting.

Profiles are saved per project and apply at the next request without interrupting
an active response. Setup/chat controls are in More project settings; helpers
have independent sampling in their settings. Global sampling defaults are under
Global settings. Watchdogs and command diagnostics retain their normal behavior.
Request traces record the selected profile and actual resolved overrides.

## Live project settings

Tab **5 Settings** in the observation view opens the provider/model picker and edits request
timeout in minutes (0 means unlimited), and run duration in hours (0 means
unlimited). Use arrows to select, Enter to edit/save, Ctrl+U to clear, and Esc
to cancel. Changes persist only in this project’s `chuggin.json`.

Model and request-timeout changes are snapshotted at the next model call, including
a retry; the active call finishes with its original settings. The panel shows the
current/last request model separately from the next selected model. Timer edits
apply immediately against active elapsed time since this run started, excluding
time held by P. Shortening the duration below elapsed time requests a stop after
the current cycle; extending it allows additional time. Resuming a stopped run
starts a new timer.

**Investigation helpers** in tab 5 opens the same project settings as
**Settings → This project · Investigation helpers**. Enable helpers, choose their
model, and set their response allowance there. These permissions remain in human
Settings; Chat and MCP cannot turn on helpers or authorize another provider.

## Investigation helpers

Helpers are optional and disabled by default. The main model can ask one to
investigate a focused question with fresh context, then use its findings in the
existing working conversation. Helpers can read project files, search, inspect
saved evidence and logs, and use enabled web tools. They cannot edit files, run
commands, launch other helpers, or decide whether work is complete. Their reports
are advisory, and main work remains saved even when an investigation is inconclusive.

Choose **Use project model** to use the project's connection, or explicitly
select a model from Ollama, Groq, OpenAI, OpenCode Zen, or OpenRouter. Ollama uses the
project's server; cloud models use their provider's connection and limits.
Groq keeps its shared request/token budgets. Investigations run sequentially
with main work. Separate-provider parallel jobs are not enabled by this feature.
Helper traffic passes through the same provider recovery and quota handling as
other requests. It does not bypass Groq waits or silently switch providers.

The default allowance is 20 model responses per investigation, adjustable in
Settings. Reaching it preserves available findings and returns control to the
main model. Pause and stop apply at safe operation boundaries. The live view
shows investigation activity; inputs, tool evidence, and results persist under
`.chuggin/agents/`. Saved results can be retrieved without repeating an investigation.

Model and allowance changes apply to the next investigation; an existing job
keeps its chosen route and allowance. Shared helper settings are defaults for new
projects. Existing projects stay disabled until you enable them explicitly.

## Images and screenshots

Vision-capable models can use `view_image` to inspect PNG, JPEG, WebP, GIF
(first frame), or BMP files in the project. Chuggin saves a separate image
snapshot and sends its actual pixels to the model. Conversations and diagnostic
logs retain artifact references instead of large base64 payloads. The latest
three images remain in the working context; older snapshots can be inspected
again by their returned `image_id`.

`capture_screenshot` can list visible windows and capture an explicitly selected
window on Linux X11. It requires ImageMagick's `import`; window discovery and
title selection also require `wmctrl`. The model can launch an app with
`run_command`, inspect its window while that command runs, and close it with
the command controls afterward. Desktop capture requires an explicit request;
there is no automatic capture of the active window or unrelated applications.
Headless sessions can still inspect images generated by the project's own tools.

Ollama's model capabilities determine whether images are supported. Supported
Groq vision models receive image content through the OpenAI-compatible API;
their images count toward the shared provider quota. A model without vision
receives an explicit failed-observation message and continues working with text.
Chat, investigation helpers, and external MCP clients can also inspect images.

## Missing-tool requests

Models can call `request_tool` with a missing capability, a concrete use case,
and an explanation of why current tools are insufficient. Suggestions persist
in this project's `.chuggin/tool-requests.json`, with duplicate capability names
grouped together. Open **Project settings → Project tools → Tool requests** to
read them, mark them reviewed, dismiss them, or reopen them.

Requests never install tools, pause the loop, or submit GitHub issues. The detail
view provides text you can review and share manually. Check it for private
project information before sharing it publicly.

## Active hours

Open **Settings → This project · Active hours** (also available in tab 5).
Choose Always allowed, custom hours, or the shared default under Global settings.
Set opening and closing times, weekdays, and a named timezone. Overnight windows
belong to the day they open; daylight-saving changes follow that timezone.
Existing projects remain Always allowed until you opt in.

At closing time choose either:

- **Finish current call:** finish the model response or tool action, then pause
  before the next operation. Tools returned by that response wait for reopening.
- **Finish current cycle:** continue through checks and saving, then pause before
  another cycle. This can run well past the closing time. Provider backoff outside
  hours parks the unfinished cycle instead of retrying throughout the day.

These hours control **only the autonomous loop**. Chat and inspection remain
available. Already-started processes may finish, and Ollama may retain its model
in memory; the schedule does not unload a shared server's models.

You can resume outside hours: choose **Run until the next scheduled stop**,
**Run one cycle now**, or **Wait for active hours**. This leaves your saved schedule
unchanged. A scheduled reopening never cancels a manual pause or another session's
editing hold. Time spent paused does not consume the run-duration timer.

Keep Chuggin open for scheduled work; it does not install a system scheduler or
wake a sleeping computer. Closing the last viewer unexpectedly requests a pause
after a one-minute reconnect grace period and the current operation. Reconnecting
does not silently clear that manual pause.

## Messages and availability

**Messages with the loop** on Home, **M**, or tab **7 Messages** opens a mailbox
with the autonomous model itself. Tab **6 Assistant** remains a separate
interactive assistant. Sending mail never resumes work or replaces a goal or
nudge. The loop sees a count notification at the next request boundary and must
call `read_messages` to fetch bodies. It can reply or initiate mail with
`send_message`; it does not have to wait for you to answer.

Messages and read receipts survive restarts outside the private computer.
**Enter** sends, **Alt+Enter** inserts a newline, **Ctrl+R** selects a message to
reply to, **F2** loads older messages, and **F3** returns to the latest page.
Receipts mean fetched by the model or displayed in an active, focused Messages
view, rather than agreement or understanding. A background or away terminal
does not silently read mail. Terminals without focus reports require recent
deliberate input; hovering the mouse is insufficient. Exact and lightly reformatted duplicate model
messages return `already_sent`, including after restarting; changed facts and
new human input allow a new message. This is conservative text matching, not
semantic paraphrase detection. There is no arbitrary cap on unanswered mail.

Availability uses host computer idle time, with **Away after five minutes** by
default. Private desktop activity, generation, and commands do not make the
human active. **F5** in Messages or **Global settings → Presence** changes the
threshold or selects Automatic, Active, Away, or Busy. Unsupported or stale OS
idle information is shown as Unknown. This checks idle/lock status only; it does
not collect keys, window titles, clipboard contents, or screenshots. Availability
is advisory and never pauses the loop or guarantees a response.

## General computer tools and notes

The private Linux computer provides tools for more than editing source files:

- `computer_info` inventories installed applications and capabilities, with actual
  resource limits. `application_command` resolves an application to literal argv
  for the existing managed command tool; it does not launch it by itself.
- `browser` controls visible private Chromium and returns page text and numbered
  controls, so it works with text-only models. It supports navigation, forms,
  clicks, keyboard input, dialogs, and download inspection. Controls are tied to
  a page snapshot; stale or already-attempted actions require a fresh read.
  Pages and downloads remain untrusted observations. The same public-web proxy
  and container isolation apply. Cross-origin frames need separate navigation.
- `inspect_document` extracts paginated PDF, office, or text content, PDF page
  renders, image dimensions, and audio/video metadata. Format limitations and
  continuation fields are reported. A spreadsheet shared-string preview is not
  a complete workbook reader.
- `notebook` stores optional titled, tagged notes outside the computer. Notes
  have stable IDs, search, paginated reads, retained revisions, and version
  checks before replacement. They are not goals or automatically injected into
  context. The model chooses whether to use them.

Private computers include Chromium, Poppler and FFmpeg as part of the desktop
bundle. Updating Chuggin does not change a running computer; idle setup/resume
adds any missing bundle packages while preserving its files. Browser page interaction may continue while
an unrelated finite command runs; ordinary file mutations still follow the
command ownership rules. The separate assistant must acquire editing ownership
before controlling the browser, so it cannot race the loop.

## Project chat

Open **Project assistant** from Home, or tab **6 Assistant** during a run.
Opening Chat does not start the loop. Ask about progress, request an edit, change
the overall goal, or set a temporary nudge. The chat model can inspect files,
search saved history, use configured web tools, run commands and checks, save or
restore checkpoints, and control the loop. These operator controls are not given
to the autonomous model.

**Enter** sends; **Esc / Ctrl+C** cancel the chat reply; **Ctrl+N** starts a new
chat; **F2** expands tool details. Tab returns to the observation views. The latest
chat loads after restarting Chuggin. Chat and loop histories are separate and
persist under the project's state directory. Older chat can be retrieved through
the history tools even when it no longer fits in the model's working context.

Chat uses the project model unless you set **This project · Chat model** in
Settings. Requests sharing an Ollama endpoint are serialized; an interactive
request gets the next free slot within its controller. The current response is
allowed to finish. Connection-wide fairness across independent controllers is
best effort; this is not a server-wide scheduler.

Inspection leaves the loop running. Before an edit or command, Chat waits for a
safe boundary and takes exclusive editing ownership. It keeps that ownership
through its checks and repairs. The loop receives a concise change notice and
reconsiders any stale, unexecuted tools before continuing. Existing commands are
observable and can be explicitly stopped; a long command is not killed just to
let Chat edit. Human Git staging and unfinished files are preserved.

## External AI access (MCP)

**Settings → This project · External AI access (MCP)** shows a project-specific
configuration to copy into another AI application's MCP settings. The server is
part of the same Rust binary, using the official Rust MCP SDK and local stdio:

~~~json
{
  "mcpServers": {
    "chuggin": {
      "command": "chuggin",
      "args": ["mcp", "--project", "/absolute/path/to/project"]
    }
  }
}
~~~

The project argument may instead name its configuration file. Connecting is an
explicit opt-in for that project and never starts the loop. TUI, scripted runs,
and MCP share one local controller and checkout lock. Local IPC currently requires
Unix; Linux is the tested platform. No separate daemon installation is needed.

The external model calls `open_operator_session`, then uses that session ID on
the same operator tools as Chat. Mutating calls include a unique `operation_id`;
retries reuse that ID and the same arguments. `operation_status` retrieves a
recorded outcome. Long commands return handles for inspection, input, and stop.
Status is also exposed as a read-only MCP resource. Credentials and connection
trust remain in human Settings rather than operator tool results.

If an external harness uses its **own** edit or shell tools, it must acquire
`begin_edit`, wait until granted, and retain ownership until all its writers
finish; then call `end_edit`. Chuggin cannot coordinate tools that bypass this
agreement. This is cooperative coordination, not an operating-system sandbox.

Interrupted actions are recorded as uncertain and are never automatically
replayed. Interrupted editing retains a recovery hold. Review files and logs,
confirm any external writers have stopped, then use the recovery tools to release
that hold. Restoring a snapshot requires a current preview and retains a backup;
restoration refuses to overwrite human staging.

## One conversation, continuous refinement

Chuggin works directly in **your project folder, on its current branch**. Open
that folder in your editor or run the project's normal commands to see the actual
work. New runs do not create a hidden developing checkout.

Recovery saves and normal commits serve different purposes. Each changed cycle
creates a recovery snapshot without moving your branch or changing your Git
staging. Completed tasks whose configured checks pass can create normal commits
with descriptive task summaries. Failing checks, model errors, and cycle boundaries
do not discard edits. Unfinished work stays visible and recoverable.

The agent uses **one conversation across tasks, cycles, and restarts**. Its system
instructions, main goal, and tool definitions stay stable. New task directions,
tool results, and check feedback are appended to the same history. This keeps
useful reasoning available and gives Ollama the opportunity to reuse a cached
prompt prefix instead of processing a different conversation for each activity.
Actual cache reuse depends on the server, model, and available context.
Tool availability follows project capabilities and enabled settings. Enabling or
disabling helpers updates their availability on the next main-model request.

Within that conversation the agent can:

- Inspect the current files and choose a useful next result with `set_task`.
- Plan, edit, run checks, and repair in any order that the evidence warrants.
- Examine its changes for missed problems using the same tools and history.
- Call `finish_task` with a factual summary when it believes the task is done.

`set_task` records the task's objective, desired outcomes, and likely relevant
files. These are planning hints, not file allowlists or rigid acceptance rules.
`finish_task` expresses completion intent. Chuggin marks the task complete only
when the configured checks pass; failures are appended to the conversation and
the task remains unfinished for repair. **Every result still preserves the files.**
Tasks have persistent IDs and completed-task records. Completion is tied to the
active task; repeating an old completion does not end another work interval.
The agent receives guidance to select useful next work. Research and other tasks
can complete without file edits; backend, document, and non-code work are valid.
There is no mandatory separate planning agent or reviewer, and no review veto.
An ordinary prose response can end a work interval without marking the task done.

At each cycle boundary Chuggin runs configured checks, even if the agent already
ran them, creates a recovery save when there are changes, and records the results.
The last passing revision advances only when all configured checks pass and the
project files remain unchanged during those checks. Commands that modify files
leave a saved but unverified checkpoint for another validation pass. The next
cycle continues from those files and the same conversation.

Chuggin does not rebuild an attempt from an older passing snapshot. The agent
can make targeted corrections. Whole-project recovery is a user action under
**Progress → Browse recovery saves / restore files**. Preview the changes before
restoring; Chuggin saves the current files first and retains Git history. Restored
files need validation again.

**Progress → Commit current changes** lets you explicitly commit unfinished work,
including staged and unstaged project changes. Automatic task commits defer when
you have staged project changes, preserving your staging while useful work and
recovery saves continue. Normal commits honor your Git hooks and signing settings;
a commit failure is reported without discarding the work. Pause with P before
making manual edits or running a build that needs stable inputs. Running local
commands can still finish. On resume, changes outside recorded model actions are
reported and affected files must be re-read before file-tool edits.

A branch switch or unfinished Git merge/rebase stops further agent mutations.
Return to the original branch, or explicitly select **Progress → Use current
branch** after finishing the Git operation. A checkout lock prevents two Chuggin
instances using different configurations from writing into the same checkout.

Conversation recovery happens for reported context pressure, repeated request
failures, or sustained repeated tool actions, not simply because a task or cycle ended or a text-size estimate was
exceeded. Before a handoff, Chuggin archives the full conversation. It preserves
the stable instructions and goal, then supplies the current task, checkpoint,
progress notes, and latest feedback. A token-pressure handoff also retains recent
messages. This is a deterministic handoff, without an extra summary-model request,
and never resets project files. The interface distinguishes checkpoints,
unresolved failures, and the last revision whose configured checks passed.
A passing check is evidence about that check, not proof of project completeness.

The agent can use `save_progress_note` for a factual handoff: what changed,
what it checked, outstanding problems, and a useful next action. Recent actions
and failures are recorded too. These notes work with any language or artifact
format and remain advisory; current files and observed validation take precedence.
Notes are stored in full, with replacement or append support. Handoffs include a
6KB excerpt; `read_progress_note` retrieves the full note through byte pagination.
Task evidence distinguishes current validation from incidental tool errors and
older task notes. It records the file state checked, so a past passing result
does not describe files changed afterward. The working model can search and read
saved conversation history after a handoff instead of repeating earlier research.

## Tools and recovery

The agent has the same inspection, editing, execution, and task tools throughout
the working conversation, plus a bounded inventory of project files. Rust
declarations and module reachability supplement the inventory when present. Other
formats use file reading and text search. Instructions describe project outcomes
and evidence rather than assuming a language or test framework.
Setup asks for a validation command appropriate to the project; it only suggests
`cargo test` when a Cargo manifest exists. Documents and data can use linters,
consistency checkers, or custom scripts. A validation command is still required.

The model can list files, search literal text within selected paths, read numbered line ranges, replace
a file, make an exact targeted edit, and run configured checks. Long reads include
continuation positions. Unique-match edits prevent accidental broad replacement.
File size alone does not block reads or edits. Very long lines can be retrieved
exactly using `read_file` byte pagination. Edits and writes replace files atomically,
preserve existing permissions, and report `changed: false` if the bytes are identical.
Structured tool results expose fields directly instead of nesting escaped JSON.
Searches include surrounding lines, pagination, and an explicit completeness
indicator. Command status can wait for new output or completion instead of rapidly
polling the same tail. Plans can be updated while commands run; edits and new
execution still wait until commands using the project have finished.
Edits can address any relevant project file, including supporting work omitted from
the original plan. Chuggin configuration, runtime state, and Git control files remain
protected.
Investigation and repairs use the same native tools and conversation. Reading
several files does not trigger a separate patch-generation stage. If the agent
tries to finish while validation is failing, it receives the failure evidence
and continues in place; claims never substitute for changes on disk.

Validation is project-defined. There is no mandatory Rust regression-writing
stage, test-name gate, or language-specific acceptance rule. The agent can add and
run suitable tests and investigate missing checks using the normal tools.

The working conversation uses native tool calls. Legacy local/Groq sampling uses temperature 0.4; explicit profiles and provider defaults are selectable. Task metadata
is recorded through tools; ordinary work does not require a staged JSON report.
Setup goal drafting still uses structured output and formatting repair.
Repeated JSON fields and fenced code are allowed. Sustained repeated words,
phrases, or sentence blocks in prose interrupt the stream. Chuggin retains the
completed conversation and retries up to twice, removing the repetitive tail.
Only ordinary prose before the repetition may be reused; interrupted tool calls,
thinking, and partial JSON are never replayed as completed actions. Structured
responses are regenerated whole. Generation-limit interruptions also request a
shorter complete response. Each failure and continuation request is logged.
Goal-drafting failures preserve the pitch and offer Retry, Settings, or Back.

A separate reasoning detector compares substantial prose and separate provider
thinking across responses and restarts. It scans the entire prose, normalizes punctuation and case, and compares
five-word patterns to catch small variations. Four matching responses trigger
feedback; four more matching responses trigger recovery. Short progress labels,
fenced code, JSON tool arguments, and genuine file changes do not count as a
reasoning loop. Existing transcripts are checked on cold resume, so restarting
does not blindly continue an already-repetitive conversation.

The action detector recognizes repeating sequences of up to 2,048 actions and
tracks previously read content by line or byte range. It can detect whole-file
rereads even if their order or page boundaries change. First-time inspections,
changed contents, and genuinely new evidence are allowed; a sustained sequence
of already-seen evidence triggers feedback before recovery. Command side effects
are not inferred from identical stdout.

Recovery archives the conversation and retains the current goal, active nudge,
task, checkpoints, observed check results, failures, log IDs, and running command
IDs. Repetitive narration, cached reasoning and completed progress-note calls
are quarantined rather than copied back into the next context. Unrelated
observations and explicitly structured episode notes remain available. Recovery
neither pauses the run, stops a command, nor rolls back files. Healthy
conversations continue unchanged until recovery is warranted. Short repeated
text-only replies are checked too: eight matching normalized replies within a
16-response window receive feedback; continued recurrence triggers a handoff.
Silence is allowed in unassigned sessions, and tool-attached progress labels do
not count as idle narration. New observations clear short-reply suspicion.

### Context handoffs and durable memory

Capacity handling first tries clearing older bulky completed tool results while
retaining the ordered conversation, recent results and pending tool calls. The
original payloads remain retrievable through immutable `read_history` references.
It accepts this lighter compaction only when estimated input usage, including
tool schemas and memory, leaves sufficient room for another response. Otherwise,
it builds a structured handoff with recent complete exchanges selected by a
token estimate proportional to the configured context window, rather than a
fixed number of messages. Estimates guide selection; they do not impose a new
context guard or pause the run. Protected instructions or pending calls can
exceed a selection budget, and diagnostics report that condition.

Previously delivered human operator directions and explicitly fetched mailbox
messages have their own source-linked memory, separate from model-authored
notes. Compaction never reads unread mailbox bodies or private operator chats.
Existing sessions backfill delivered records from their loop archives once.
Explicit goal replacements are marked as historical; ordinary notes retain their
original scope. Recent directions are prioritized in previews, and full exact
records remain available through `read_context_memory`.

`save_progress_note` accepts optional `observations`, `decisions`, `reasons`,
`open_questions`, and `attempts` arrays. These structured entries survive
handoffs as unverified model-authored memory, with source references; they do
not assign a new goal or prove a result. Freeform notes remain available through
`read_progress_note`. The memory reader pages the `human` or `episode` section; an optional `record_id`
selects one exact entry instead of scanning the whole ledger. Pass the returned `revision` with each `next_offset` continuation, restarting at
zero if records changed. No extra summary-model calls are made. Between
compactions, the conversation remains append-only for inference caching.

Each handoff saves `context-compaction-N.json` beside its immutable conversation
archive, reporting estimated sizes, retained exchanges, archived evidence,
repetition quarantine, and any protected overage. Its live event explains whether
older tool payloads were cleared or a new context was assembled.

Recovery events appear in live output and `prose-recovery-N.json`, `thinking-recovery-N.json`,
`action-recovery-N.json`, and `repetition-context-reset-N.json` cycle artifacts.
Bounded detector observations and cumulative intervention counts persist across
restarts; fingerprints and content coverage stay out of the model's context.
Asynchronous commands count once when they actually finish, including completion
through monitoring, status polling, or end-of-cycle draining. Polls do not count
as new command executions.

Version 0.7.1 adds a separate **command repetition assessment**. It tracks model-issued
commands, configured-check requests, and compiler diagnostics against the observed
project files, task ID, and outcome status. Eight repeated executions with unchanged
observed files produce a brief question about what another execution should establish.
After sixteen, a fresh conversation with the selected model assesses the evidence.
These are assessment thresholds, not command limits: commands are not blocked or
silently replaced with cached results. Checks run by Chuggin at cycle boundaries
do not count toward this detector.

The diagnostic runs sequentially and can read files, inspect the inventory, search,
and retrieve command logs or progress notes. It cannot execute commands, edit files,
complete tasks, or reject checkpoints. It sees the goal, active task, recent commands
and inspections, project snapshot, and check feedback instead of the entire repetitive
conversation. It reports **productive**, **stalled**, or **uncertain**, with a reason,
next action, and expected new evidence. Its opinion is advisory. For a reported stall,
Chuggin archives the repetitive conversation and resumes from saved work with that
advice. Productive or uncertain assessments keep the main conversation intact.
An inconclusive or failed assessment also returns to normal work; it does not pause
the project. Provider waits retain the existing backoff and stop/timer behavior.

Each assessment has up to five inspection rounds followed by a final reporting round,
up to eight read-only tool requests per reply, and at most 2,048 output tokens per
response. Existing transport/provider retries still apply. Continued repetition is reassessed
after another sixteen executions, or sixty-four when judged productive. Changed
project files, task identity, command arguments, or outcome status reset the streak.
Optional `run_command.reason` explains deliberate trials or polling. Identical files
and exit codes alone cannot establish unchanged external state or ignored artifacts;
the diagnostic must consider that uncertainty. It can still make mistakes.

Successful repeated command output gets a shorter, explicitly labeled excerpt with
the full log ID. Full output remains on disk and retrievable. **Diagnose** appears
in the observation view, and `command-repetition-*` and `command-diagnostic-*` cycle
artifacts record the evidence, read-only actions, report, or failure. Recovery state
survives cycles and restarts. Healthy runs do not pay for a diagnostic on every cycle.

## Reading the observation view

**Live** combines model text, command summaries, tool notices and checkpoints.
**Model** shows only streamed model text; **Checks** shows commands and validation.
Provider-separated reasoning has a muted, italic **Thinking** section; regular
output has a **Response** section. Switching channels starts a new section even
when the provider supplies no whitespace. Reasoning-like prose sent as ordinary
content remains Response; Chuggin does not guess its channel from its wording.

Live activity is saved separately in `.chuggin/live-events.jsonl`. On restart,
Live and Model restore recent output, including reasoning which is deliberately
absent from the model's working conversation. Older installations recover recent
thinking, responses and tool notices from their saved provider traces once.
A New session divider separates restored history from new activity. Startup reads
a bounded recent window into the normal 2,000-entry scrollback; the full display
log, conversation archives and command logs remain on disk. Historical commands
show their notices and full-log paths rather than reconstructed live panels.
Restoring the display does not execute tools, add anything to model context,
or increment the new session's counters.

Context shows the input token count last reported for the main loop. On resume,
the saved count is labeled **Saved response** until a fresh report arrives. When
no measurement is available, it shows **Awaiting usage report** instead of zero.
Helper and watchdog requests do not replace this count. It describes the last
measured request, rather than estimating the current conversation's size.

Command output is collapsed by default. Recognized Rust, pytest, Jest/Vitest and TAP
summaries show passed/total counts, failures and skipped tests. Multiple Rust suites
are aggregated without counting individual test lines twice. Other commands show
exit status and output-line counts instead of guessed test totals. A few error or
slow-test messages remain visible even when collapsed.

Press **E** in Live or Checks to expand/collapse command output. The expanded view
retains a bounded recent excerpt and shows the full log path; full diagnostic logs
are unchanged on disk. Searching also searches retained command details. Summaries
are computed locally, without extra model requests, and never alter what the agent
sees or how verification is evaluated. Repeated identical stage headings are omitted.

When output is quiet, a small moving ASCII indicator appears at the bottom of the output
box. Its arrow travels behind each bracket and reverses direction. It labels waiting for a response, a running command, or provider waiting;
it indicates an active harness, not a completion percentage or proof of progress.
It stops when the run is paused.

## Execution, compiler diagnostics, and symbols

The working agent supplies exactly one **run_command** input: `argv`, an executable
and literal argument array such as `["cargo", "test", "unicode"]`, or `script`, a
shell program string for pipelines, redirection and multiline commands. `script`
explicitly runs `/bin/sh -c`; `argv` has no implicit shell. Commands run in the
persistent workspace, or inside the private Linux computer when configured.
If the model supplies a valid JSON-encoded argument array, Chuggin decodes it and
reports `arguments_normalized: true`. Malformed arrays are rejected without
execution, with instructions to supply a real array or the explicit `script` field.
Starting with **0.8.0**, commands return within about one second. If unfinished,
they return `running: true` and a `command_id`. **command_status** inspects or waits
briefly for that same process; **command_input** sends stdin text (including an
explicit newline when needed) or closes stdin; **stop_command** terminates the
process group with a recorded reason. Input reports how many bytes were accepted,
so callers can retry only the remainder if a pipe is full. These are pipe-backed
sessions, not full PTYs. Completed responses include the exit status, bounded
output tail, and full log ID. The default `mode: "finite"` covers builds, tests,
scripts and commands that modify files. The agent can inspect or plan while finite
work runs; further workspace writes and validation wait for it to finish.
Use `mode: "background"` with a `reason` to keep a needed application or server
open during development. Background mode does not turn an unfinished build, test
or file mutation into safe completed work. Launch the application directly rather
than a shell command ending with `&`: Chuggin cleans up process-group descendants
when their parent command exits and reports `descendant_cleanup_reason`.

A fresh, read-only **watchdog** inspects long-running processes. The default first
review is after 120 seconds, configurable in observation Settings as **Command
first review (seconds)**. It applies to newly started commands. For compatibility,
existing check/tool `timeout_seconds` values now request an earlier first review
(the smaller of that value and `command_review_seconds`), not an automatic kill.
The watchdog sees purpose, current task, recent output, elapsed time, time since
output, and Linux process-tree counters where available. It can inspect source
and full logs, but cannot edit or run additional commands. Quiet output, high CPU,
and runtime alone are not grounds for termination.

A productive or uncertain verdict lets the existing process continue, with another
review in five minutes. A stalled verdict terminates it and returns the reason and
suggested repair to the working agent. If the watchdog is unavailable or cannot
produce a valid report, the process keeps running and review is retried later.
Watchdog requests have a two-minute per-request deadline and no provider retry
loop; at most six requests are made per review. A command that finishes during
inspection keeps its actual exit result. Reviews run sequentially between working
model calls or while waiting for a command; they do not interrupt an in-flight
model request or require parallel Ollama inference.

Both model-requested commands and final checkpoint checks use this mechanism.
Sessions belong to the controller and retain their command IDs across model turns,
cycles, pauses, run timers and ordinary Stop/Resume. Ending a work interval returns
control promptly while commands remain observable. Finite work must finish before
checkpoint verification; running, interrupted and unexecuted checks never count
as passing. Intentionally open background apps can remain while development and
configured validation continue.

An explicit controller close can clean up its loop-owned commands with
`cleanup_commands: true` and a recorded reason. An idle controller upgrade uses
this cleanup before starting the new version. Operator-owned or external commands
must finish before that close. The terminal shows shutdown progress and waits for
the previous controller to release its project locks before opening the replacement.
Startup errors show output from the current attempt; earlier run logs stay on disk.
Ordinary soft stops and automatic idle-close attempts
retain live sessions. Force stop terminates active process groups. Command records
and resume notices explain intentional cleanup so it is not mistaken for an
unexplained application crash. Processes are not reattached after the controller
exits. Files and command logs are retained. Session metadata
is saved beside logs as `*.session.json`; `watchdog-*/decision.json` and diagnostic
artifacts record each assessment. The observation sidebar shows command elapsed
time and time until its next review.
**read_command_log** retrieves the full log in chunks, including logs from earlier
cycles. Use the complete returned ID, such as `cycle-000003/command-2-0.log`, so
continued conversations retrieve the original evidence even after command numbers
repeat. Legacy IDs without a cycle prefix refer only to the current cycle.
Commands and checks share
the live output view and process-group cleanup on completion or explicit termination.
Background applications remain owned and observable until targeted stop or
explicit controller cleanup.

**compiler_diagnostics** is a Rust helper that requires a Cargo manifest. It runs
Cargo check for all targets and returns grouped errors/warnings, source locations,
nearby code, and compiler suggestions. It does
not execute tests. Command success does not substitute for the configured final
checks. Command-produced edits remain part of the persistent project and are
saved at the next checkpoint.
Like configured checks, commands execute with the user's OS permissions. The
visible-folder workflow is cooperative and does not isolate arbitrary external
processes. The model is instructed to keep commands within its task and leave Git
commits/state to Chuggin.

**lookup_symbol** inspects Rust files. It finds types,
functions, private/public methods, and name-based reference candidates, with
paths, line numbers, source snippets, and pagination. It parses Rust syntax, so
comments and string contents do not appear as references. This is not a language
server: it does not resolve types, expand macros, or filter inactive cfg branches.
The loop advertises these Rust tools when the project has a Cargo manifest;
other projects use the general file, search, and command tools. They are not
required validation steps for other projects.

## Optional Brave search and page reading

Open **Settings → Brave API key → Enter or replace key**. Entry is masked.
Saving a key enables the web tools; the toggle can disable them without deleting
it. **Test Brave connection** performs a real search and reports errors.

The key is stored separately in ~/.config/chuggin/brave.key (or the corresponding
XDG directory) with owner-only permissions. It is not included in project
configuration, prompts, or request logs. It is sent only to Brave's search API.

The agent can use **web_search** for five sourced snippets,
and **read_web_page** for numbered, paginated public HTTPS text and links.
Responses are bounded and cached within a cycle. Page reading does not execute
JavaScript; private/local addresses and binary downloads are rejected. External
content is labelled untrusted and cannot authorize tools or change the goal.
Search queries and returned evidence are logged, so prompts direct agents to use
public API names/errors and never private code or secrets. Brave billing and
quotas apply to searches; keys are optional and web tools default to disabled.

## Settings and history

Global settings: ~/.config/chuggin/settings.json, or
$XDG_CONFIG_HOME/chuggin/settings.json. Project-local chuggin.json contains the accepted
goal, repository, checks, and state location. Project overrides take precedence;
shared settings are reloaded when starting a run.

The welcome screen has four entries: **Resume project** (or **Set up this
project**), **Project settings**, **Global settings**, and **Quit**. Esc or Q also quits.
After first-time setup, a success screen confirms that settings are saved and
returns to the welcome screen after three seconds; Enter skips the countdown.
Setup does not start the loop.

**Project settings** contains the goal or session mode, project model, run
duration, and active hours. **Loop settings** holds thinking, context and response
limits, request timeout, sampling, and optional goal completion. **More project
settings** contains setup and chat models, their thinking settings, investigation
helpers, computer environment, and command review. **Project tools** holds
progress and recovery, the project assistant, messages with the loop, the private
desktop when configured, tool requests, and MCP connection information.

**Global settings** contains shared provider connections and defaults; existing
project overrides take precedence. Both settings menus work before setup.
Preferences saved before setup live in `.chuggin/project-draft-settings.json`
until setup is accepted; saving preferences does not start the loop or initialize
a Git repository.
Context and response limits offer presets and a
Custom option. Request settings apply to the next request without interrupting
the current one.

The setup model can differ from the loop model: use a larger model to investigate
and define the goal, then a smaller model for ongoing work. An unset setup or chat
model uses the loop model. Global settings supply defaults for new projects;
project settings take precedence.

Thinking is configured separately for setup, the loop, chat, and helpers. Each
defaults to **Model default**, which omits a reasoning override from requests.
The selector discovers the exact supported values from Ollama's `/api/show` and
OpenRouter's model metadata, or the authorized ChatGPT account catalog. Groq options use documented model-specific mappings,
because its model list does not advertise reasoning levels. Unknown models and
providers offer Model default until their controls can be verified. Where the
provider advertises them, choices include on/off, effort levels, or a reasoning
token budget. Explicit unsupported choices produce an error rather than silently
changing their meaning. Reasoning tokens share the provider's response allowance
and can increase latency and usage.

Defaults: 32,768 context tokens, 4,096 output tokens, Model default thinking, and up to
48 work steps per cycle, with bounded request recovery within each step.
The context setting supports million-token windows and has no model-independent
cap beyond its 32-bit token count. Choose a window supported by your model and
provider; the response limit must be smaller than that window. For cloud models,
this setting controls when Chuggin refreshes context; it does not increase the
provider's actual capacity. Larger settings do not fill the window in advance.
Conversations are not reset at an estimated byte threshold. After repeated failed
request recovery or actual context pressure, work can continue in a refreshed
conversation with the current files, main goal, task, and recorded failures.
Reaching the step budget proceeds to checks and a saved checkpoint.
It does not discard unfinished edits or pause the project. Model requests default
to a 30-minute total timeout; set it to 0 for no request deadline. Connection
establishment remains bounded to ten seconds.
Command reviews are separate from model-request timeouts. A timeout or connection failure retries
the same model request once, retaining completed edits and validation instead of
restarting earlier stages. If the retry fails, the existing files are still
retained for continued work.
Request settings and failed attempts are recorded alongside the response traces.

Cloud provider limits use a separate recovery path. Legacy session/weekly limits,
HTTP 429 responses, and temporary HTTP 5xx outages preserve the exact working
conversation and retry after waiting. `Retry-After` seconds or HTTP dates take
precedence; without that header, delays increase from 1 to 2, 4, 8, then 15 minutes.
The dashboard shows **Waiting for provider** and a countdown. Provider errors do
not trigger conversation compaction or count as reasoning failures. Limit errors
inside an otherwise successful Ollama stream are handled too.

Credit/payment exhaustion and authentication/access errors use the same waiting
policy, even without a supplied reset time. After reaching the 15-minute backoff
cap, Chuggin keeps retrying every 15 minutes indefinitely; there is no attempt limit.
An explicit longer `Retry-After` is still honored. Work and conversation stay intact
while you resolve account issues, credits reset, or you select another model.
Chuggin never purchases credits or changes your billing plan. Set Run duration to
0 for an unlimited run; explicit stop requests and configured run timers still apply.
An unrecognized quota message returned with HTTP 429
still receives the bounded-frequency retry policy; a reset time cannot be inferred
reliably from every provider's prose.

Pending waits persist in `.chuggin/provider-wait.json`, so restarting does not bypass
the cooldown. Once you have fixed the connection or account issue, press **T** in
the observation view to skip the current wait and try now, including a wait restored
after restarting. This requests one retry; another provider failure returns to the
normal backoff policy. T does not unpause a held cycle; the retry waits for P to resume.
Changing the selected model/server releases its old wait. A soft stop
or the run timer ends a provider wait promptly, then runs checks and saves work;
time waiting counts toward the run duration unless you explicitly pause with P.
Each provider failure and chosen retry delay is recorded in the cycle's
`provider-error-NNN.json` files.

`.chuggin/state.json` records the visible `working_workspace`, current branch,
`branch_head` (normal Git history), `working_ref` (latest recovery save),
`last_validated_tree`, last passing recovery reference, current task, completion
records, pending commit explanation, and recent outcomes. Full evidence remains
in cycle artifacts. `.chuggin/conversation.json` retains the conversation across
tasks, cycles, and restarts.

Recovery snapshots use project-scoped `refs/chuggin/autosaves/...` references.
They preserve failing and unfinished work without appearing in ordinary branch
history or a normal branch push. Backups must include the Git objects and recovery
refs plus Chuggin's state; a normal clone alone is not a complete recovery backup.
For an explicitly chosen Git worktree, the shared Git directory is also required.
Snapshots exclude ignored files and Chuggin's runtime/configuration paths. Existing
ignored files remain in your working folder. Runtime paths are also added to
Git’s local exclude file, keeping logs out of ordinary staging without editing
your project’s tracked ignore rules.

**Upgrading an existing project:** choose Resume to prepare a migration preview.
The old developing workspace and the visible folder are reconciled in a separate
preparation directory. Nothing is copied over the visible folder until you apply
that preview. If both folders changed a file, choose **Review conflicting files**
to compare versions and select the whole file from Chuggin or your original folder. Both
originals remain backed up; choices only change the preview. The screen explains
why upgrading from the older separate-folder workflow needs this review and
recommends keeping Chuggin’s latest work. **Ask AI to recommend a version** uses
your configured project model for a bounded, read-only review. It explains its
choice for you to approve, or asks for closer review when neither whole file is
a safe choice. Provider errors return to the menu; no file choice is applied.
Once conflicts are resolved, **Move files and resume** applies the result.
Combining parts manually is also possible in the displayed preview folder.
**Start review over (if files changed)** re-reads both folders and starts a fresh
file comparison. Use it after editing either folder during the review, or to redo
your file choices. Previous choices remain backed up but are not reused.
Restarting the review leaves project files and development history unchanged.
Existing commits keep their IDs, authors, dates, and messages. Both input
snapshots, staging backups, the old workspace, conversation, and diagnostic history
remain available. A running or paused worker must stop before migration.

If the original folder has changes selected for a future Git commit, the menu
explains that selection separately. You can clear the old selection while keeping
the updated files, or explicitly preserve it. Its backup remains available.
For maintenance/automation, `chuggin migrate`
prints the preview and `chuggin migrate --apply` applies it; unattended runs never
silently migrate. Interrupted application is journaled and can resume; intervening
file or staging changes stop automatic recovery rather than being overwritten.
Version 0.12 uses state schema 4; older binaries cannot resume that state. Once
migration succeeds, later launches resume directly; this is a one-time move per
project, not a step repeated on every launch or version update.

Migration also records its actual changes relative to the agent's previous files
in `.chuggin/migration-handoff.json`. If those files changed, the first work
interval receives a one-time note listing affected files, including original
versions kept in place of Chuggin's work, additions, removals, and merges. The note
asks for focused inspection and appropriate checks, followed by ordinary work.
It survives an interrupted interval and conversation recovery, then retires at
the next completed cycle boundary. Concrete unresolved findings remain ordinary
follow-up work. Keeping the agent's files unchanged adds no extra review request.

Each cycle-NNNNNN directory records the evidence for that interval:

- Effective configuration, prompt version, and task information.
- Exact model requests and raw streamed responses, including server-provided
  token counts, timings, and interrupted output.
- Tool results, progress notes, context recovery, and request errors.
- Full check logs, validation results, completion requests, and checkpoint outcome.

The working conversation and current files survive task and cycle boundaries. The short
state summary rotates; full cycle folders remain. Artifact and build output
cleanup is not yet automatic.

## Build and test

All implementation and tests are Rust. Installation is one executable.

~~~sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --release
install -m 755 target/release/chuggin ~/.local/bin/chuggin
~~~

Git and the target project's build/test tools must be on PATH. Ollama can run on
another machine. Advanced automation commands are available through help.

This is an experimental harness, not proof of product completeness. Check quality
and model judgment matter; a saved checkpoint does not prove arbitrary behavior
correct. Requests have no byte-based context cutoff; the configured context and output token
limits are sent to Ollama. Large requests can still exceed the model's context capacity.
Checks run with the user's permissions. Worktrees isolate Git changes, not code
execution.

## Harness design references

The current design emphasizes persistent work, useful tool feedback, and
observable verification, informed by [Anthropic's context engineering guidance](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents)
and [tool design guidance](https://www.anthropic.com/engineering/writing-tools-for-agents).
Structured goal drafts follow [Ollama's schema support](https://docs.ollama.com/capabilities/structured-outputs).
Search uses the [Brave Web Search API](https://api-dashboard.search.brave.com/api-reference/web/search/get).

The working model can still miss defects or invent a concern. Saved checkpoints
and passing tests do not establish completion of a broad product goal. Compare
actual changes, unresolved findings, and validation results across longer runs
before treating harness changes as a performance improvement.

## Nudges

Press **N** in the observation view, including while paused, to give the model a
persistent temporary priority within the project goal. Enter saves it; Esc closes
the editor. Ctrl+U clears the draft, Ctrl+D cancels the active nudge, and Ctrl+R
reopens the latest closed nudge. One nudge is active at a time; replacing one
preserves its history. Adding a nudge while paused does not resume work.

The next model call receives the update without discarding conversation history
or interrupting a request or command already underway. A nudge can span multiple
tasks. The model calls `finish_nudge` with a summary and evidence, then returns to
the overall goal. This records the model's completion claim, not independent
verification. The Goal tab shows requests, completion reports, and history; reopen
a nudge if more work is needed. State lives in `.chuggin/nudges.json` separately
from worker state so concurrent UI and model updates cannot overwrite one another.

## Allowing goal completion

In the observation view, open **5 Settings**, select **Allow goal completion**,
and set it to **on**. It defaults to off and persists for this project. The next
model call receives a tool to mark the overall goal complete when fully achieved
and verified. There is no urgency prompt, reduced budget, or new work stage.
Without that tool call, the loop continues normally.

Completion saves the working tree and the model's summary and evidence, then
pauses with **Model reports project complete**. Configured check results are
recorded alongside the claim; the claim itself is not independent verification.
The saved completion prevents automatic continuation after restarting. Press
**R** from the paused observation view to explicitly reopen work, or turn the
setting off and resume to return to indefinite looping. Reports remain in the
cycle artifacts. Turning the setting off also rejects outstanding completion
calls; running commands must finish before the model can report completion.

## Cloud inference

**Project model** offers Ollama, Groq, OpenCode Zen, and OpenRouter. Selecting a
provider loads its model catalog; it does not generate a response. Model choices
are explicit: adding a connection does not change an existing project's model or
the shared default. Chat and investigation helpers can select these providers too.

### OpenCode Zen free models

Choose **Project model → OpenCode Zen**. No account or API key is needed for the
free models offered by this connection. Saved model names start with `zen/`.
The picker includes tool-capable models whose catalog lists free pricing.
For Space Bunny, select `zen/space-bunny-free`.

Some free models are temporary previews. Availability, capacity, and provider
limits can change, so a free model is not a guarantee of unlimited throughput.
Chuggin waits and retries rate/capacity errors while retaining your selected model.
It never switches to a paid Zen model.
Zen publishes its API for other coding agents. Its
[service terms](https://opencode.ai/legal/terms-of-service) still apply, including
restrictions on quota circumvention and broad wording about unattended processes.
Free API access should not be taken as an assurance of unlimited 24/7 use.

### OpenRouter free models

Add your key under **Settings → Global settings → OpenRouter**, then choose
**Project model → OpenRouter**. The key is masked during entry and stored privately
as `openrouter.key` alongside the shared settings. It is excluded from project
configuration and request artifacts. The catalog check lists models without
generating a response; it does not verify your remaining inference allowance.

Saved names start with `openrouter/`. The picker includes only tool-capable models
whose catalog lists free input and output pricing. Requests retain this free-only
policy and do not silently fall back to paid models. Free-tier requests may wait
for rate limits or available capacity; retries use provider backoff.
Space Bunny is listed as `openrouter/stealth/space-bunny-alpha`; its model page
currently gives an October 5, 2026 preview end date. See the live
[model page](https://openrouter.ai/stealth/space-bunny-alpha) for availability.

Your context window and response limit remain project settings. Choosing a cloud
model does not reduce local-model settings or expand them automatically. Provider
context and output limits still apply to that model.

### Groq free-tier inference

Choose **Project model → Groq** from Home. Add the API key under
**Settings → Global settings → Groq connection and limits**. The model list comes
from your Groq account. Project models use names such as
`groq/openai/gpt-oss-120b`; the `groq/` prefix selects Groq, while existing model
names continue to use Ollama. Chat can select a Groq model independently with the
same prefix. Credentials live in the private global `groq.key` file, never in
project configurations or model request artifacts. Connections use Groq's official
HTTPS endpoint and do not follow redirects.

The default shared budget is 24 requests/minute, 900 requests/day, 7,200
tokens/minute, and 180,000 tokens/day. These defaults leave headroom below the
published free limits for GPT-OSS and Qwen as of September 2026. Verify your actual
account allowances on [Groq's Limits page](https://console.groq.com/settings/limits).
Limits and the 1,024-token maximum response cap are editable in the same menu;
optional separate input/output minute limits are supported too.

Every inference call—including chat, recovery and watchdog requests—must reserve
estimated input plus maximum output before sending. The budget is shared across
projects and models on this machine, locked across processes, and persisted across
restarts. Actual reported usage replaces reservations; interrupted or unreported
requests retain their reservations. Daily accounting uses a conservative rolling
24-hour window. Minute token capacity refills continuously; Chuggin waits for the
next request to fit instead of always waiting a full minute. The dashboard labels
this **Waiting for Groq budget**, separately from provider-error retries.
Successful response headers can tighten admission, and an unexpected
429 also establishes a shared cooldown. Manual retry rechecks this budget rather
than bypassing it. A watchdog skips unavailable inference instead of waiting
indefinitely while reviewing a command. Other agents can pause/stop normally during
quota waits. Model-list connection tests do not invoke inference.

Free-tier throughput is mostly constrained by **tokens**, not request counts.
Once the daily budget is used, the loop waits for capacity to return; this is
expected and survives restarting Chuggin. Waiting cannot make an oversized request
fit. Groq requests therefore keep the system instructions and initial goal, drop
older complete exchanges when necessary, and explicitly shorten large tool results.
The latest exchange stays paired, and pre-shortening conversations are archived.
If the required context still cannot fit, Chuggin reports the problem without sending
it. This smaller working context applies only to Groq.

Token counting is an estimate with headroom and upward calibration from actual
usage, not a provider tokenizer guarantee. Other apps/machines using your Groq
organization are only reflected in provider headers, so 429 recovery remains a
fallback. Cached tokens are conservatively counted locally. The local limits are
not an account-wide billing cap and do not automatically enable paid usage.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
