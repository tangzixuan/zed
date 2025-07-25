mod event_coalescer;

use crate::TelemetrySettings;
use anyhow::Result;
use clock::SystemClock;
use futures::channel::mpsc;
use futures::{Future, FutureExt, StreamExt};
use gpui::{App, AppContext as _, BackgroundExecutor, Task};
use http_client::{self, AsyncBody, HttpClient, HttpClientWithUrl, Method, Request};
use parking_lot::Mutex;
use regex::Regex;
use release_channel::ReleaseChannel;
use settings::{Settings, SettingsStore};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::File;
use std::io::Write;
use std::sync::LazyLock;
use std::time::Instant;
use std::{env, mem, path::PathBuf, sync::Arc, time::Duration};
use telemetry_events::{AssistantEventData, AssistantPhase, Event, EventRequestBody, EventWrapper};
use util::{ResultExt, TryFutureExt};
use worktree::{UpdatedEntriesSet, WorktreeId};

use self::event_coalescer::EventCoalescer;

pub struct Telemetry {
    clock: Arc<dyn SystemClock>,
    http_client: Arc<HttpClientWithUrl>,
    executor: BackgroundExecutor,
    state: Arc<Mutex<TelemetryState>>,
}

struct TelemetryState {
    settings: TelemetrySettings,
    system_id: Option<Arc<str>>,       // Per system
    installation_id: Option<Arc<str>>, // Per app installation (different for dev, nightly, preview, and stable)
    session_id: Option<String>,        // Per app launch
    metrics_id: Option<Arc<str>>,      // Per logged-in user
    release_channel: Option<&'static str>,
    architecture: &'static str,
    events_queue: Vec<EventWrapper>,
    flush_events_task: Option<Task<()>>,
    log_file: Option<File>,
    is_staff: Option<bool>,
    first_event_date_time: Option<Instant>,
    event_coalescer: EventCoalescer,
    max_queue_size: usize,
    worktrees_with_project_type_events_sent: HashSet<WorktreeId>,

    os_name: String,
    app_version: String,
    os_version: Option<String>,
}

#[cfg(debug_assertions)]
const MAX_QUEUE_LEN: usize = 5;

#[cfg(not(debug_assertions))]
const MAX_QUEUE_LEN: usize = 50;

#[cfg(debug_assertions)]
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);

#[cfg(not(debug_assertions))]
const FLUSH_INTERVAL: Duration = Duration::from_secs(60 * 5);
static ZED_CLIENT_CHECKSUM_SEED: LazyLock<Option<Vec<u8>>> = LazyLock::new(|| {
    option_env!("ZED_CLIENT_CHECKSUM_SEED")
        .map(|s| s.as_bytes().into())
        .or_else(|| {
            env::var("ZED_CLIENT_CHECKSUM_SEED")
                .ok()
                .map(|s| s.as_bytes().into())
        })
});

static DOTNET_PROJECT_FILES_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(global\.json|Directory\.Build\.props|.*\.(csproj|fsproj|vbproj|sln))$").unwrap()
});

pub fn os_name() -> String {
    #[cfg(target_os = "macos")]
    {
        "macOS".to_string()
    }
    #[cfg(target_os = "linux")]
    {
        format!("Linux {}", gpui::guess_compositor())
    }
    #[cfg(target_os = "freebsd")]
    {
        format!("FreeBSD {}", gpui::guess_compositor())
    }

    #[cfg(target_os = "windows")]
    {
        "Windows".to_string()
    }
}

/// Note: This might do blocking IO! Only call from background threads
pub fn os_version() -> String {
    #[cfg(target_os = "macos")]
    {
        use cocoa::base::nil;
        use cocoa::foundation::NSProcessInfo;

        unsafe {
            let process_info = cocoa::foundation::NSProcessInfo::processInfo(nil);
            let version = process_info.operatingSystemVersion();
            gpui::SemanticVersion::new(
                version.majorVersion as usize,
                version.minorVersion as usize,
                version.patchVersion as usize,
            )
            .to_string()
        }
    }
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        use std::path::Path;

        let content = if let Ok(file) = std::fs::read_to_string(&Path::new("/etc/os-release")) {
            file
        } else if let Ok(file) = std::fs::read_to_string(&Path::new("/usr/lib/os-release")) {
            file
        } else if let Ok(file) = std::fs::read_to_string(&Path::new("/var/run/os-release")) {
            file
        } else {
            log::error!(
                "Failed to load /etc/os-release, /usr/lib/os-release, or /var/run/os-release"
            );
            "".to_string()
        };
        let mut name = "unknown";
        let mut version = "unknown";

        for line in content.lines() {
            match line.split_once('=') {
                Some(("ID", val)) => name = val.trim_matches('"'),
                Some(("VERSION_ID", val)) => version = val.trim_matches('"'),
                _ => {}
            }
        }

        format!("{} {}", name, version)
    }

    #[cfg(target_os = "windows")]
    {
        let mut info = unsafe { std::mem::zeroed() };
        let status = unsafe { windows::Wdk::System::SystemServices::RtlGetVersion(&mut info) };
        if status.is_ok() {
            gpui::SemanticVersion::new(
                info.dwMajorVersion as _,
                info.dwMinorVersion as _,
                info.dwBuildNumber as _,
            )
            .to_string()
        } else {
            "unknown".to_string()
        }
    }
}

impl Telemetry {
    pub fn new(
        clock: Arc<dyn SystemClock>,
        client: Arc<HttpClientWithUrl>,
        cx: &mut App,
    ) -> Arc<Self> {
        let release_channel =
            ReleaseChannel::try_global(cx).map(|release_channel| release_channel.display_name());

        TelemetrySettings::register(cx);

        let state = Arc::new(Mutex::new(TelemetryState {
            settings: *TelemetrySettings::get_global(cx),
            architecture: env::consts::ARCH,
            release_channel,
            system_id: None,
            installation_id: None,
            session_id: None,
            metrics_id: None,
            events_queue: Vec::new(),
            flush_events_task: None,
            log_file: None,
            is_staff: None,
            first_event_date_time: None,
            event_coalescer: EventCoalescer::new(clock.clone()),
            max_queue_size: MAX_QUEUE_LEN,
            worktrees_with_project_type_events_sent: HashSet::new(),

            os_version: None,
            os_name: os_name(),
            app_version: release_channel::AppVersion::global(cx).to_string(),
        }));
        Self::log_file_path();

        cx.background_spawn({
            let state = state.clone();
            let os_version = os_version();
            state.lock().os_version = Some(os_version);
            async move {
                if let Some(tempfile) = File::create(Self::log_file_path()).log_err() {
                    state.lock().log_file = Some(tempfile);
                }
            }
        })
        .detach();

        cx.observe_global::<SettingsStore>({
            let state = state.clone();

            move |cx| {
                let mut state = state.lock();
                state.settings = *TelemetrySettings::get_global(cx);
            }
        })
        .detach();

        let this = Arc::new(Self {
            clock,
            http_client: client,
            executor: cx.background_executor().clone(),
            state,
        });

        let (tx, mut rx) = mpsc::unbounded();
        ::telemetry::init(tx);

        cx.background_spawn({
            let this = Arc::downgrade(&this);
            async move {
                while let Some(event) = rx.next().await {
                    let Some(state) = this.upgrade() else { break };
                    state.report_event(Event::Flexible(event))
                }
            }
        })
        .detach();

        // We should only ever have one instance of Telemetry, leak the subscription to keep it alive
        // rather than store in TelemetryState, complicating spawn as subscriptions are not Send
        std::mem::forget(cx.on_app_quit({
            let this = this.clone();
            move |_| this.shutdown_telemetry()
        }));

        this
    }

    #[cfg(any(test, feature = "test-support"))]
    fn shutdown_telemetry(self: &Arc<Self>) -> impl Future<Output = ()> + use<> {
        Task::ready(())
    }

    // Skip calling this function in tests.
    // TestAppContext ends up calling this function on shutdown and it panics when trying to find the TelemetrySettings
    #[cfg(not(any(test, feature = "test-support")))]
    fn shutdown_telemetry(self: &Arc<Self>) -> impl Future<Output = ()> + use<> {
        telemetry::event!("App Closed");
        // TODO: close final edit period and make sure it's sent
        Task::ready(())
    }

    pub fn log_file_path() -> PathBuf {
        paths::logs_dir().join("telemetry.log")
    }

    pub fn has_checksum_seed(&self) -> bool {
        ZED_CLIENT_CHECKSUM_SEED.is_some()
    }

    pub fn start(
        self: &Arc<Self>,
        system_id: Option<String>,
        installation_id: Option<String>,
        session_id: String,
        cx: &App,
    ) {
        let mut state = self.state.lock();
        state.system_id = system_id.map(|id| id.into());
        state.installation_id = installation_id.map(|id| id.into());
        state.session_id = Some(session_id);
        state.app_version = release_channel::AppVersion::global(cx).to_string();
        state.os_name = os_name();
    }

    pub fn metrics_enabled(self: &Arc<Self>) -> bool {
        let state = self.state.lock();
        let enabled = state.settings.metrics;
        drop(state);
        enabled
    }

    pub fn set_authenticated_user_info(
        self: &Arc<Self>,
        metrics_id: Option<String>,
        is_staff: bool,
    ) {
        let mut state = self.state.lock();

        if !state.settings.metrics {
            return;
        }

        let metrics_id: Option<Arc<str>> = metrics_id.map(|id| id.into());
        state.metrics_id.clone_from(&metrics_id);
        state.is_staff = Some(is_staff);
        drop(state);
    }

    pub fn report_assistant_event(self: &Arc<Self>, event: AssistantEventData) {
        let event_type = match event.phase {
            AssistantPhase::Response => "Assistant Responded",
            AssistantPhase::Invoked => "Assistant Invoked",
            AssistantPhase::Accepted => "Assistant Response Accepted",
            AssistantPhase::Rejected => "Assistant Response Rejected",
        };

        telemetry::event!(
            event_type,
            conversation_id = event.conversation_id,
            kind = event.kind,
            phase = event.phase,
            message_id = event.message_id,
            model = event.model,
            model_provider = event.model_provider,
            response_latency = event.response_latency,
            error_message = event.error_message,
            language_name = event.language_name,
        );
    }

    pub fn log_edit_event(self: &Arc<Self>, environment: &'static str, is_via_ssh: bool) {
        let mut state = self.state.lock();
        let period_data = state.event_coalescer.log_event(environment);
        drop(state);

        if let Some((start, end, environment)) = period_data {
            let duration = end
                .saturating_duration_since(start)
                .min(Duration::from_secs(60 * 60 * 24))
                .as_millis() as i64;

            telemetry::event!(
                "Editor Edited",
                duration = duration,
                environment = environment,
                is_via_ssh = is_via_ssh
            );
        }
    }

    pub fn report_discovered_project_type_events(
        self: &Arc<Self>,
        worktree_id: WorktreeId,
        updated_entries_set: &UpdatedEntriesSet,
    ) {
        let Some(project_types) = self.detect_project_types(worktree_id, updated_entries_set)
        else {
            return;
        };

        for project_type in project_types {
            telemetry::event!("Project Opened", project_type = project_type);
        }
    }

    fn detect_project_types(
        self: &Arc<Self>,
        worktree_id: WorktreeId,
        updated_entries_set: &UpdatedEntriesSet,
    ) -> Option<Vec<String>> {
        let mut state = self.state.lock();

        if state
            .worktrees_with_project_type_events_sent
            .contains(&worktree_id)
        {
            return None;
        }

        let mut project_types: HashSet<&str> = HashSet::new();

        for (path, _, _) in updated_entries_set.iter() {
            let Some(file_name) = path.file_name().and_then(|f| f.to_str()) else {
                continue;
            };

            let project_type = if file_name == "pnpm-lock.yaml" {
                Some("pnpm")
            } else if file_name == "yarn.lock" {
                Some("yarn")
            } else if file_name == "package.json" {
                Some("node")
            } else if DOTNET_PROJECT_FILES_REGEX.is_match(file_name) {
                Some("dotnet")
            } else {
                None
            };

            if let Some(project_type) = project_type {
                project_types.insert(project_type);
            };
        }

        if !project_types.is_empty() {
            state
                .worktrees_with_project_type_events_sent
                .insert(worktree_id);
        }

        let mut project_types: Vec<_> = project_types.into_iter().map(String::from).collect();
        project_types.sort();
        Some(project_types)
    }

    fn report_event(self: &Arc<Self>, event: Event) {
        let mut state = self.state.lock();
        // RUST_LOG=telemetry=trace to debug telemetry events
        log::trace!(target: "telemetry", "{:?}", event);

        if !state.settings.metrics {
            return;
        }

        if state.flush_events_task.is_none() {
            let this = self.clone();
            state.flush_events_task = Some(self.executor.spawn(async move {
                this.executor.timer(FLUSH_INTERVAL).await;
                this.flush_events().detach();
            }));
        }

        let date_time = self.clock.utc_now();

        let milliseconds_since_first_event = match state.first_event_date_time {
            Some(first_event_date_time) => date_time
                .saturating_duration_since(first_event_date_time)
                .min(Duration::from_secs(60 * 60 * 24))
                .as_millis() as i64,
            None => {
                state.first_event_date_time = Some(date_time);
                0
            }
        };

        let signed_in = state.metrics_id.is_some();
        state.events_queue.push(EventWrapper {
            signed_in,
            milliseconds_since_first_event,
            event,
        });

        if state.installation_id.is_some() && state.events_queue.len() >= state.max_queue_size {
            drop(state);
            self.flush_events().detach();
        }
    }

    pub fn metrics_id(self: &Arc<Self>) -> Option<Arc<str>> {
        self.state.lock().metrics_id.clone()
    }

    pub fn system_id(self: &Arc<Self>) -> Option<Arc<str>> {
        self.state.lock().system_id.clone()
    }

    pub fn installation_id(self: &Arc<Self>) -> Option<Arc<str>> {
        self.state.lock().installation_id.clone()
    }

    pub fn is_staff(self: &Arc<Self>) -> Option<bool> {
        self.state.lock().is_staff
    }

    fn build_request(
        self: &Arc<Self>,
        // We take in the JSON bytes buffer so we can reuse the existing allocation.
        mut json_bytes: Vec<u8>,
        event_request: &EventRequestBody,
    ) -> Result<Request<AsyncBody>> {
        json_bytes.clear();
        serde_json::to_writer(&mut json_bytes, event_request)?;

        let checksum = calculate_json_checksum(&json_bytes).unwrap_or_default();

        Ok(Request::builder()
            .method(Method::POST)
            .uri(
                self.http_client
                    .build_zed_api_url("/telemetry/events", &[])?
                    .as_ref(),
            )
            .header("Content-Type", "application/json")
            .header("x-zed-checksum", checksum)
            .body(json_bytes.into())?)
    }

    pub fn flush_events(self: &Arc<Self>) -> Task<()> {
        let mut state = self.state.lock();
        state.first_event_date_time = None;
        let events = mem::take(&mut state.events_queue);
        state.flush_events_task.take();
        drop(state);
        if events.is_empty() {
            return Task::ready(());
        }

        let this = self.clone();
        self.executor.spawn(
            async move {
                let mut json_bytes = Vec::new();

                if let Some(file) = &mut this.state.lock().log_file {
                    for event in &events {
                        json_bytes.clear();
                        serde_json::to_writer(&mut json_bytes, event)?;
                        file.write_all(&json_bytes)?;
                        file.write_all(b"\n")?;
                    }
                }

                let request_body = {
                    let state = this.state.lock();

                    EventRequestBody {
                        system_id: state.system_id.as_deref().map(Into::into),
                        installation_id: state.installation_id.as_deref().map(Into::into),
                        session_id: state.session_id.clone(),
                        metrics_id: state.metrics_id.as_deref().map(Into::into),
                        is_staff: state.is_staff,
                        app_version: state.app_version.clone(),
                        os_name: state.os_name.clone(),
                        os_version: state.os_version.clone(),
                        architecture: state.architecture.to_string(),

                        release_channel: state.release_channel.map(Into::into),
                        events,
                    }
                };

                let request = this.build_request(json_bytes, &request_body)?;
                let response = this.http_client.send(request).await?;
                if response.status() != 200 {
                    log::error!("Failed to send events: HTTP {:?}", response.status());
                }
                anyhow::Ok(())
            }
            .log_err()
            .map(|_| ()),
        )
    }
}

pub fn calculate_json_checksum(json: &impl AsRef<[u8]>) -> Option<String> {
    let Some(checksum_seed) = &*ZED_CLIENT_CHECKSUM_SEED else {
        return None;
    };

    let mut summer = Sha256::new();
    summer.update(checksum_seed);
    summer.update(json);
    summer.update(checksum_seed);
    let mut checksum = String::new();
    for byte in summer.finalize().as_slice() {
        use std::fmt::Write;
        write!(&mut checksum, "{:02x}", byte).unwrap();
    }

    Some(checksum)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clock::FakeSystemClock;
    use gpui::TestAppContext;
    use http_client::FakeHttpClient;
    use std::collections::HashMap;
    use telemetry_events::FlexibleEvent;
    use worktree::{PathChange, ProjectEntryId, WorktreeId};

    #[gpui::test]
    fn test_telemetry_flush_on_max_queue_size(cx: &mut TestAppContext) {
        init_test(cx);
        let clock = Arc::new(FakeSystemClock::new());
        let http = FakeHttpClient::with_200_response();
        let system_id = Some("system_id".to_string());
        let installation_id = Some("installation_id".to_string());
        let session_id = "session_id".to_string();

        cx.update(|cx| {
            let telemetry = Telemetry::new(clock.clone(), http, cx);

            telemetry.state.lock().max_queue_size = 4;
            telemetry.start(system_id, installation_id, session_id, cx);

            assert!(is_empty_state(&telemetry));

            let first_date_time = clock.utc_now();
            let event_properties = HashMap::from_iter([(
                "test_key".to_string(),
                serde_json::Value::String("test_value".to_string()),
            )]);

            let event = FlexibleEvent {
                event_type: "test".to_string(),
                event_properties,
            };

            telemetry.report_event(Event::Flexible(event.clone()));
            assert_eq!(telemetry.state.lock().events_queue.len(), 1);
            assert!(telemetry.state.lock().flush_events_task.is_some());
            assert_eq!(
                telemetry.state.lock().first_event_date_time,
                Some(first_date_time)
            );

            clock.advance(Duration::from_millis(100));

            telemetry.report_event(Event::Flexible(event.clone()));
            assert_eq!(telemetry.state.lock().events_queue.len(), 2);
            assert!(telemetry.state.lock().flush_events_task.is_some());
            assert_eq!(
                telemetry.state.lock().first_event_date_time,
                Some(first_date_time)
            );

            clock.advance(Duration::from_millis(100));

            telemetry.report_event(Event::Flexible(event.clone()));
            assert_eq!(telemetry.state.lock().events_queue.len(), 3);
            assert!(telemetry.state.lock().flush_events_task.is_some());
            assert_eq!(
                telemetry.state.lock().first_event_date_time,
                Some(first_date_time)
            );

            clock.advance(Duration::from_millis(100));

            // Adding a 4th event should cause a flush
            telemetry.report_event(Event::Flexible(event));
            assert!(is_empty_state(&telemetry));
        });
    }

    #[gpui::test]
    async fn test_telemetry_flush_on_flush_interval(
        executor: BackgroundExecutor,
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let clock = Arc::new(FakeSystemClock::new());
        let http = FakeHttpClient::with_200_response();
        let system_id = Some("system_id".to_string());
        let installation_id = Some("installation_id".to_string());
        let session_id = "session_id".to_string();

        cx.update(|cx| {
            let telemetry = Telemetry::new(clock.clone(), http, cx);
            telemetry.state.lock().max_queue_size = 4;
            telemetry.start(system_id, installation_id, session_id, cx);

            assert!(is_empty_state(&telemetry));
            let first_date_time = clock.utc_now();

            let event_properties = HashMap::from_iter([(
                "test_key".to_string(),
                serde_json::Value::String("test_value".to_string()),
            )]);

            let event = FlexibleEvent {
                event_type: "test".to_string(),
                event_properties,
            };

            telemetry.report_event(Event::Flexible(event));
            assert_eq!(telemetry.state.lock().events_queue.len(), 1);
            assert!(telemetry.state.lock().flush_events_task.is_some());
            assert_eq!(
                telemetry.state.lock().first_event_date_time,
                Some(first_date_time)
            );

            let duration = Duration::from_millis(1);

            // Test 1 millisecond before the flush interval limit is met
            executor.advance_clock(FLUSH_INTERVAL - duration);

            assert!(!is_empty_state(&telemetry));

            // Test the exact moment the flush interval limit is met
            executor.advance_clock(duration);

            assert!(is_empty_state(&telemetry));
        });
    }

    #[gpui::test]
    fn test_project_discovery_does_not_double_report(cx: &mut gpui::TestAppContext) {
        init_test(cx);

        let clock = Arc::new(FakeSystemClock::new());
        let http = FakeHttpClient::with_200_response();
        let telemetry = cx.update(|cx| Telemetry::new(clock.clone(), http, cx));
        let worktree_id = 1;

        // Scan of empty worktree finds nothing
        test_project_discovery_helper(telemetry.clone(), vec![], Some(vec![]), worktree_id);

        // Files added, second scan of worktree 1 finds project type
        test_project_discovery_helper(
            telemetry.clone(),
            vec!["package.json"],
            Some(vec!["node"]),
            worktree_id,
        );

        // Third scan of worktree does not double report, as we already reported
        test_project_discovery_helper(telemetry.clone(), vec!["package.json"], None, worktree_id);
    }

    #[gpui::test]
    fn test_pnpm_project_discovery(cx: &mut gpui::TestAppContext) {
        init_test(cx);

        let clock = Arc::new(FakeSystemClock::new());
        let http = FakeHttpClient::with_200_response();
        let telemetry = cx.update(|cx| Telemetry::new(clock.clone(), http, cx));

        test_project_discovery_helper(
            telemetry.clone(),
            vec!["package.json", "pnpm-lock.yaml"],
            Some(vec!["node", "pnpm"]),
            1,
        );
    }

    #[gpui::test]
    fn test_yarn_project_discovery(cx: &mut gpui::TestAppContext) {
        init_test(cx);

        let clock = Arc::new(FakeSystemClock::new());
        let http = FakeHttpClient::with_200_response();
        let telemetry = cx.update(|cx| Telemetry::new(clock.clone(), http, cx));

        test_project_discovery_helper(
            telemetry.clone(),
            vec!["package.json", "yarn.lock"],
            Some(vec!["node", "yarn"]),
            1,
        );
    }

    #[gpui::test]
    fn test_dotnet_project_discovery(cx: &mut gpui::TestAppContext) {
        init_test(cx);

        let clock = Arc::new(FakeSystemClock::new());
        let http = FakeHttpClient::with_200_response();
        let telemetry = cx.update(|cx| Telemetry::new(clock.clone(), http, cx));

        // Using different worktrees, as production code blocks from reporting a
        // project type for the same worktree multiple times

        test_project_discovery_helper(
            telemetry.clone().clone(),
            vec!["global.json"],
            Some(vec!["dotnet"]),
            1,
        );
        test_project_discovery_helper(
            telemetry.clone(),
            vec!["Directory.Build.props"],
            Some(vec!["dotnet"]),
            2,
        );
        test_project_discovery_helper(
            telemetry.clone(),
            vec!["file.csproj"],
            Some(vec!["dotnet"]),
            3,
        );
        test_project_discovery_helper(
            telemetry.clone(),
            vec!["file.fsproj"],
            Some(vec!["dotnet"]),
            4,
        );
        test_project_discovery_helper(
            telemetry.clone(),
            vec!["file.vbproj"],
            Some(vec!["dotnet"]),
            5,
        );
        test_project_discovery_helper(telemetry.clone(), vec!["file.sln"], Some(vec!["dotnet"]), 6);

        // Each worktree should only send a single project type event, even when
        // encountering multiple files associated with that project type
        test_project_discovery_helper(
            telemetry,
            vec!["global.json", "Directory.Build.props"],
            Some(vec!["dotnet"]),
            7,
        );
    }

    // TODO:
    // Test settings
    // Update FakeHTTPClient to keep track of the number of requests and assert on it

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    fn is_empty_state(telemetry: &Telemetry) -> bool {
        telemetry.state.lock().events_queue.is_empty()
            && telemetry.state.lock().flush_events_task.is_none()
            && telemetry.state.lock().first_event_date_time.is_none()
    }

    fn test_project_discovery_helper(
        telemetry: Arc<Telemetry>,
        file_paths: Vec<&str>,
        expected_project_types: Option<Vec<&str>>,
        worktree_id_num: usize,
    ) {
        let worktree_id = WorktreeId::from_usize(worktree_id_num);
        let entries: Vec<_> = file_paths
            .into_iter()
            .enumerate()
            .map(|(i, path)| {
                (
                    Arc::from(std::path::Path::new(path)),
                    ProjectEntryId::from_proto(i as u64 + 1),
                    PathChange::Added,
                )
            })
            .collect();
        let updated_entries: UpdatedEntriesSet = Arc::from(entries.as_slice());

        let detected_project_types = telemetry.detect_project_types(worktree_id, &updated_entries);

        let expected_project_types =
            expected_project_types.map(|types| types.iter().map(|&t| t.to_string()).collect());

        assert_eq!(detected_project_types, expected_project_types);
    }
}
