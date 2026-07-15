use crate::atspi_tree::{
    focused_element_summary, list_accessible_apps, perform_action as invoke_accessibility_action,
    set_element_value, snapshot_tree, AccessibilityAction, AccessibilityNode, AccessibleAppSummary,
    Bounds, FocusedElementSummary, ValueSetInvocation,
};
use crate::capture_transform::{
    CaptureTransform, DesktopRect, ScreenshotArtifact, ScreenshotArtifactCache, WindowSnapshot,
};
use crate::diagnostics::{doctor_report, setup_accessibility_report, DoctorReport, SetupReport};
use crate::gnome_extension::{setup_window_targeting_report, WindowTargetingSetupReport};
use crate::remote_desktop::{
    click as portal_click, drag as portal_drag, keysyms_for_text, press_keycode_chord,
    scroll as portal_scroll, start_portal_keyboard_session, start_portal_pointer_session,
    type_text_with_keysyms, PointerButton, PortalKeyboardSession, PortalPointerSession,
    ScrollDirection,
};
use crate::screenshot::{
    capture_screenshot_raw, prepare_screenshot_payload, RawScreenshotCapture, ScreenshotCapture,
    ScreenshotEncodingPolicy, ScreenshotPayloadOptions, MAX_SCREENSHOT_JPEG_QUALITY,
    MIN_SCREENSHOT_JPEG_QUALITY,
};
use crate::windowing::registry;
use crate::windows::{
    focus_window_target, focused_window, list_windows, resolve_window_target,
    window_permission_hint, WindowFocusResult, WindowInfo, WindowTarget,
    GNOME_SHELL_INTROSPECT_BACKEND,
};
use anyhow::Result;
use rmcp::{
    handler::server::wrapper::{Json, Parameters},
    model::{CallToolResult, Content},
    schemars::JsonSchema,
    tool, tool_handler, tool_router, ErrorData, ServerHandler, ServiceExt,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    env,
    future::Future,
    os::unix::net::{UnixDatagram, UnixStream},
    path::PathBuf,
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{Child as TokioChild, Command as TokioCommand},
    time::{sleep, timeout},
};
use zbus::{Connection as ZbusConnection, Proxy as ZbusProxy};

const YDOTOOL_TIMEOUT: Duration = Duration::from_secs(10);
const XDOTOOL_TIMEOUT: Duration = Duration::from_secs(10);
const YDOTOOL_TYPE_CHARS_PER_SECOND: u64 = 20;
const KDE_CLIPBOARD_DBUS_TIMEOUT: Duration = Duration::from_secs(3);
const KDE_KLIPPER_SERVICE: &str = "org.kde.klipper";
const KDE_KLIPPER_PATH: &str = "/klipper";
const KDE_KLIPPER_INTERFACE: &str = "org.kde.klipper.klipper";

#[derive(Clone, Default)]
pub struct ComputerUseLinux {
    last_nodes: Arc<Mutex<Vec<AccessibilityNode>>>,
    portal_pointer_session: Arc<Mutex<Option<PortalPointerSession>>>,
    portal_keyboard_session: Arc<Mutex<Option<PortalKeyboardSession>>>,
    /// Lazily-created uinput absolute pointer (preferred coordinate backend).
    abs_pointer: Arc<Mutex<Option<crate::abs_pointer::AbsPointer>>>,
    portal_keyboard_init_lock: Arc<tokio::sync::Mutex<()>>,
    clipboard_lock: Arc<tokio::sync::Mutex<()>>,
    /// Persistent X11 clipboard owner. Keeping the arboard handle alive avoids
    /// losing restored contents on desktops without a clipboard manager.
    x11_clipboard: Arc<Mutex<Option<arboard::Clipboard>>>,
    /// Raw multi-target X11 owner used to restore the user's original
    /// clipboard formats after the temporary paste transaction.
    x11_raw_clipboard: Arc<Mutex<Option<X11RawClipboard>>>,
    /// Cached logical desktop size (union of monitors) from the most recent
    /// full-frame capture; used for off-screen window/coordinate warnings.
    desktop_size: Arc<Mutex<Option<(u32, u32)>>>,
    /// Immutable recent screenshots and their capture-to-desktop transforms.
    /// Multiple IDs remain usable until their bounded TTL expires.
    screenshot_artifacts: Arc<Mutex<ScreenshotArtifactCache>>,
    visual_targets: Arc<Mutex<VisualTargetCache>>,
    /// Mapping from cached AT-SPI logical coordinates to the physical X11
    /// window geometry used by coordinate input fallbacks.
    last_atspi_coordinates: Arc<Mutex<Option<AtspiCoordinateMap>>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct AtspiCoordinateMap {
    logical_x: i32,
    logical_y: i32,
    physical_x: i32,
    physical_y: i32,
    scale: f64,
}

const VISUAL_TARGET_LIMIT: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
struct ImageBounds {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

impl ImageBounds {
    fn center(self) -> (i32, i32) {
        (
            self.x.saturating_add((self.width / 2) as i32),
            self.y.saturating_add((self.height / 2) as i32),
        )
    }
}

#[derive(Debug, Clone)]
struct VisualTarget {
    target_id: String,
    screenshot_id: String,
    bounds: ImageBounds,
    recognized_text: String,
    role: Option<String>,
}

#[derive(Debug, Default)]
struct VisualTargetCache {
    targets: VecDeque<VisualTarget>,
}

impl VisualTargetCache {
    fn insert(&mut self, target: VisualTarget) {
        self.targets
            .retain(|existing| existing.target_id != target.target_id);
        self.targets.push_front(target);
        self.targets.truncate(VISUAL_TARGET_LIMIT);
    }

    fn get(&self, target_id: &str) -> Option<VisualTarget> {
        self.targets
            .iter()
            .find(|target| target.target_id == target_id)
            .cloned()
    }
}

#[tool_router]
impl ComputerUseLinux {
    #[tool(
        name = "doctor",
        description = "Report Linux Computer Use desktop integration readiness.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn doctor(&self) -> Json<DoctorReport> {
        Json(doctor_report())
    }

    #[tool(
        name = "setup_accessibility",
        description = "Enable GNOME accessibility through gsettings so Linux Computer Use can read AT-SPI trees.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn setup_accessibility(&self) -> Json<SetupReport> {
        Json(setup_accessibility_report())
    }

    #[tool(
        name = "setup_window_targeting",
        description = "Install and enable the optional GNOME Shell extension used for exact window list/focus targeting when GNOME blocks native introspection.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn setup_window_targeting(&self) -> Json<WindowTargetingSetupReport> {
        Json(setup_window_targeting_report().await)
    }

    #[tool(
        name = "list_apps",
        description = "List running Linux desktop app candidates visible to the Computer Use backend.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn list_apps(&self) -> Json<ListAppsOutput> {
        let (accessible_apps, accessibility_error) = match list_accessible_apps(50).await {
            Ok(apps) => (apps, None),
            Err(error) => (Vec::new(), Some(format!("{error:#}"))),
        };

        Json(ListAppsOutput {
            apps: list_process_apps(),
            accessible_apps,
            accessibility_error,
            note: "Linux Computer Use lists process candidates plus AT-SPI application roots when accessibility is enabled.".to_string(),
        })
    }

    #[tool(
        name = "list_windows",
        description = "List compositor windows with title, app id, class, focus state, client type, and known bounds.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn list_windows(&self) -> Json<ListWindowsOutput> {
        Json(window_list_output().await)
    }

    #[tool(
        name = "focused_window",
        description = "Return the compositor window that currently has keyboard focus.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn focused_window(&self) -> Json<FocusedWindowOutput> {
        match focused_window().await {
            Ok(window) => {
                let backend = window_backend(window.as_ref().into_iter());
                Json(FocusedWindowOutput {
                    backend,
                    focused_window: window,
                    error: None,
                    permissions_hint: None,
                    message:
                        "Focused window query completed through the available compositor window backend."
                            .to_string(),
                })
            }
            Err(error) => {
                let error = format!("{error:#}");
                Json(FocusedWindowOutput {
                    backend: GNOME_SHELL_INTROSPECT_BACKEND.to_string(),
                    focused_window: None,
                    permissions_hint: window_permission_hint(&error),
                    error: Some(error),
                    message: "Focused window query failed; targeted keyboard input is unavailable until window introspection works.".to_string(),
                })
            }
        }
    }

    #[tool(
        name = "activate_window",
        description = "Focus a Linux desktop window by window_id, pid, app_id, wm_class, title, or terminal selectors when the compositor permits it.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn activate_window(
        &self,
        Parameters(params): Parameters<ActivateWindowParams>,
    ) -> Json<ActivateWindowOutput> {
        let target = params.into_target();
        let received = Some(serde_json::json!(target.clone()));
        match focus_window_target(&target).await {
            Ok(focus) => {
                let ok = focus_satisfies_target(&focus, &target);
                Json(ActivateWindowOutput {
                    ok,
                    implemented: true,
                    backend: focus.backend.clone(),
                    focus: Some(focus),
                    error: None,
                    permissions_hint: None,
                    received,
                })
            }
            Err(error) => {
                let error = format!("{error:#}");
                Json(ActivateWindowOutput {
                    ok: false,
                    implemented: true,
                    backend: GNOME_SHELL_INTROSPECT_BACKEND.to_string(),
                    focus: None,
                    permissions_hint: window_permission_hint(&error),
                    error: Some(error),
                    received,
                })
            }
        }
    }

    #[tool(
        name = "get_app_state",
        description = "Start an app use session if needed, then get accessibility state plus a size-bounded screenshot for a Linux app. Describe the required payload using max_bytes, max_width, and max_height; the backend preserves PNG when possible and compresses automatically when necessary.",
        output_schema = rmcp::handler::server::tool::schema_for_type::<GetAppStateOutput>(),
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn get_app_state(
        &self,
        Parameters(params): Parameters<GetAppStateParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let verbose = params.verbose.unwrap_or(false);
        let diagnostics = doctor_report();
        let (window_context, window_error, window_permissions_hint) =
            self.resolve_window_context(&params).await;
        let max_nodes = params.max_nodes.unwrap_or(120).clamp(1, 500);
        let max_depth = params.max_depth.unwrap_or(12).min(12);
        let include_screenshot = params.include_screenshot.unwrap_or(true);
        let screenshot_options = params.screenshot_options();
        let window_target_requested = params.window_target().has_target();
        let app_filter = self
            .resolve_accessibility_app_filter(&params, window_context.as_ref())
            .await;
        let (screenshot, screenshot_error) = if include_screenshot {
            if window_target_requested && window_context.is_none() {
                (
                    None,
                    Some(
                        "targeted get_app_state could not resolve the requested window; refusing to return an unrelated full-desktop screenshot"
                            .to_string(),
                    ),
                )
            } else {
                match capture_screenshot_raw().await {
                    Ok(raw) => {
                        self.cache_desktop_size(raw.width, raw.height);
                        let layout_fingerprint =
                            self.monitor_layout_fingerprint(raw.width, raw.height).await;
                        let prepared = prepare_get_app_state_capture(
                            raw,
                            window_context.as_ref().filter(|_| window_target_requested),
                            screenshot_options,
                        );
                        match prepared {
                            Ok((capture, window)) => {
                                self.cache_screenshot_artifact(
                                    &capture,
                                    layout_fingerprint,
                                    window,
                                );
                                (Some(capture), None)
                            }
                            Err(error) => (None, Some(error)),
                        }
                    }
                    Err(error) => (None, Some(format!("{error:#}"))),
                }
            }
        } else {
            (None, None)
        };
        let (accessibility_tree, accessibility_tree_raw_count, accessibility_error) = if diagnostics
            .readiness
            .can_build_accessibility_tree
        {
            let target_pid = window_context.as_ref().and_then(|window| window.pid);
            match snapshot_tree(app_filter.as_deref(), target_pid, max_nodes, max_depth).await {
                Ok(nodes) => {
                    let raw_count = nodes.len();
                    (compact_accessibility_tree(nodes), raw_count, None)
                }
                Err(error) => (Vec::new(), 0, Some(format!("{error:#}"))),
            }
        } else {
            (
                    Vec::new(),
                    0,
                    Some(
                        "AT-SPI accessibility is unavailable. Continue with screenshot and coordinate actions when readiness reports an input backend; follow readiness.recommended_next_step if element-aware actions are required."
                            .to_string(),
                    ),
                )
        };
        if accessibility_error.is_none() {
            self.cache_nodes_for_window(&accessibility_tree, window_context.as_ref());
        } else {
            self.clear_cached_nodes();
        }
        let mut message = if let Some(error) = &accessibility_error {
            format!("MCP registration is working, but AT-SPI tree extraction failed: {error}")
        } else if let Some(capture) = &screenshot {
            format!(
                "MCP registration, screenshot capture, and AT-SPI tree extraction are working. Captured {} accessibility nodes (compacted from {}) and a screenshot through {}.",
                accessibility_tree.len(),
                accessibility_tree_raw_count,
                capture.source
            )
        } else if let Some(error) = &screenshot_error {
            format!(
                "MCP registration and AT-SPI tree extraction are working. Captured {} accessibility nodes (compacted from {}). Screenshot capture failed: {error}",
                accessibility_tree.len(),
                accessibility_tree_raw_count,
            )
        } else {
            format!(
                "MCP registration and AT-SPI tree extraction are working. Captured {} accessibility nodes (compacted from {}). Screenshot capture was not requested.",
                accessibility_tree.len(),
                accessibility_tree_raw_count,
            )
        };
        if let Some(window) = &window_context {
            message.push_str(&format!(
                " Window target resolved to window_id {}.",
                window.window_id
            ));
        } else if let Some(error) = &window_error {
            message.push_str(&format!(" Window target resolution failed: {error}"));
        }

        // Full diagnostics are huge (portal/process dumps); emit them only on
        // request. The compact readiness block always travels, and failures get
        // a pointer to verbose=true instead of an automatic dump.
        let readiness = diagnostics.readiness.clone();
        let include_full = verbose;
        if !include_full
            && (accessibility_error.is_some()
                || screenshot_error.is_some()
                || window_error.is_some())
        {
            message.push_str(" Pass verbose=true for full diagnostics.");
        }
        get_app_state_call_result(
            GetAppStateOutput {
                app_name_or_bundle_identifier: params.app_name_or_bundle_identifier,
                window_context,
                window_error,
                window_permissions_hint,
                backend: "linux-atspi".to_string(),
                screenshot: None,
                screenshot_error,
                accessibility_tree,
                accessibility_tree_raw_count,
                accessibility_error,
                readiness,
                diagnostics: include_full.then_some(diagnostics),
                message,
            },
            screenshot,
        )
    }

    #[tool(
        name = "screenshot",
        description = "Capture a viewable, size-bounded image with a unique screenshot_id. Describe the required payload using max_bytes, max_width, and max_height; PNG is preserved when possible and the backend compresses automatically when necessary. A targeted window is strictly cropped unless full_screen=true; crop failures never fall back to the full desktop.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn screenshot(
        &self,
        Parameters(params): Parameters<ScreenshotParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.capture_screenshot_tool(params, ScreenshotEncodingPolicy::Adaptive)
            .await
    }

    #[tool(
        name = "screenshot_compressed",
        description = "Capture a size-bounded JPEG at the exact numeric quality requested. The backend may reduce dimensions to satisfy max_bytes but never changes quality. A targeted window is strictly cropped unless full_screen=true.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn screenshot_compressed(
        &self,
        Parameters(params): Parameters<ScreenshotCompressedParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if !(MIN_SCREENSHOT_JPEG_QUALITY..=MAX_SCREENSHOT_JPEG_QUALITY).contains(&params.quality) {
            return Err(ErrorData::invalid_params(
                format!(
                    "quality must be an integer from {MIN_SCREENSHOT_JPEG_QUALITY} to {MAX_SCREENSHOT_JPEG_QUALITY}"
                ),
                None,
            ));
        }
        self.capture_screenshot_tool(
            params.screenshot,
            ScreenshotEncodingPolicy::Jpeg {
                quality: params.quality,
            },
        )
        .await
    }

    #[tool(
        name = "locate_text",
        description = "Locate visible text inside an immutable screenshot. Returns screenshot-space bounds and target_id values; use target_id for verified actions instead of manually converting coordinates.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn locate_text(
        &self,
        Parameters(params): Parameters<LocateTextParams>,
    ) -> Json<LocateVisualOutput> {
        Json(
            self.locate_visual(&params.screenshot_id, &params.text, None)
                .await,
        )
    }

    #[tool(
        name = "locate_control",
        description = "Locate a visible control by role and text inside an immutable screenshot. OCR fallback reports role_inferred=true instead of claiming accessibility evidence.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn locate_control(
        &self,
        Parameters(params): Parameters<LocateControlParams>,
    ) -> Json<LocateVisualOutput> {
        Json(
            self.locate_visual(&params.screenshot_id, &params.text, Some(params.role))
                .await,
        )
    }

    async fn locate_visual(
        &self,
        screenshot_id: &str,
        text: &str,
        role: Option<String>,
    ) -> LocateVisualOutput {
        let query = normalize_visual_text(text);
        if query.is_empty() {
            return LocateVisualOutput {
                ok: false,
                screenshot_id: screenshot_id.to_string(),
                backend: None,
                matches: Vec::new(),
                message: "text must contain at least one non-whitespace character".to_string(),
            };
        }
        let artifact = match self.screenshot_artifact(screenshot_id) {
            Ok(artifact) => artifact,
            Err(message) => {
                return LocateVisualOutput {
                    ok: false,
                    screenshot_id: screenshot_id.to_string(),
                    backend: None,
                    matches: Vec::new(),
                    message,
                };
            }
        };
        let ocr =
            match crate::ocr::recognize(&artifact.capture.data_url, &artifact.capture.mime_type)
                .await
            {
                Ok(ocr) => ocr,
                Err(error) => {
                    return LocateVisualOutput {
                        ok: false,
                        screenshot_id: screenshot_id.to_string(),
                        backend: None,
                        matches: Vec::new(),
                        message: format!("OCR failed: {error:#}"),
                    };
                }
            };

        let mut matches = ocr
            .observations
            .into_iter()
            .filter(|observation| normalize_visual_text(&observation.text).contains(&query))
            .filter_map(|observation| {
                let raw_bounds = ImageBounds {
                    x: i32::try_from(observation.bounds.x).ok()?,
                    y: i32::try_from(observation.bounds.y).ok()?,
                    width: observation.bounds.width,
                    height: observation.bounds.height,
                };
                let bounds = expand_visual_bounds(
                    raw_bounds,
                    artifact.capture.width,
                    artifact.capture.height,
                    role.as_deref(),
                )?;
                let target_id = new_visual_target_id().ok()?;
                let target = VisualTarget {
                    target_id: target_id.clone(),
                    screenshot_id: screenshot_id.to_string(),
                    bounds,
                    recognized_text: observation.text.clone(),
                    role: role.clone(),
                };
                self.visual_targets.lock().ok()?.insert(target);
                let (center_x, center_y) = bounds.center();
                Some(LocatedVisualMatch {
                    target_id,
                    screenshot_id: screenshot_id.to_string(),
                    bounds,
                    center_x,
                    center_y,
                    confidence: observation.confidence,
                    source: if role.is_some() {
                        "ocr_text_region".to_string()
                    } else {
                        "ocr".to_string()
                    },
                    recognized_text: observation.text,
                    role: role.clone(),
                    role_inferred: role.is_some(),
                })
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| {
            right
                .confidence
                .total_cmp(&left.confidence)
                .then_with(|| left.bounds.y.cmp(&right.bounds.y))
                .then_with(|| left.bounds.x.cmp(&right.bounds.x))
        });
        matches.truncate(20);
        let message = match matches.len() {
            0 => format!("No OCR text matched {text:?}. Capture a clearer or larger screenshot."),
            1 => "Found one unique visual target.".to_string(),
            count => format!(
                "Found {count} candidates. Do not choose by coordinates alone; inspect recognized_text and confidence."
            ),
        };
        LocateVisualOutput {
            ok: true,
            screenshot_id: screenshot_id.to_string(),
            backend: Some(ocr.backend),
            matches,
            message,
        }
    }

    #[tool(
        name = "click_target",
        description = "Click a target_id returned by locate_text or locate_control and require visible target-region change. The action is never automatically repeated after dispatch.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn click_target(
        &self,
        Parameters(params): Parameters<ClickTargetParams>,
    ) -> Json<ClickVerificationOutput> {
        Json(
            self.click_visual_target(ClickAndVerifyParams {
                target_id: params.target_id,
                button: params.button,
                click_count: params.click_count,
                expect_text_present: None,
                expect_text_absent: None,
                expect_region_changed: Some(true),
                expect_focused_editable: None,
                timeout_ms: None,
            })
            .await,
        )
    }

    #[tool(
        name = "click_and_verify",
        description = "Click a target_id and verify all requested postconditions using pointer position, screenshot-region change, OCR text, and focused-editable state. If dispatch occurred but verification fails, the backend returns dispatched_unverified and never clicks again automatically.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn click_and_verify(
        &self,
        Parameters(params): Parameters<ClickAndVerifyParams>,
    ) -> Json<ClickVerificationOutput> {
        Json(self.click_visual_target(params).await)
    }

    async fn click_visual_target(&self, params: ClickAndVerifyParams) -> ClickVerificationOutput {
        let target = match self.visual_target(&params.target_id) {
            Ok(target) => target,
            Err(message) => return rejected_click_verification(&params.target_id, message),
        };
        let artifact = match self.screenshot_artifact(&target.screenshot_id) {
            Ok(artifact) => artifact,
            Err(message) => return rejected_click_verification(&params.target_id, message),
        };
        let (center_x, center_y) = target.bounds.center();
        let right = target
            .bounds
            .x
            .saturating_add(target.bounds.width.saturating_sub(1) as i32);
        let bottom = target
            .bounds
            .y
            .saturating_add(target.bounds.height.saturating_sub(1) as i32);
        let mapped = match self
            .resolve_screenshot_points(
                &target.screenshot_id,
                &[
                    (center_x, center_y),
                    (target.bounds.x, target.bounds.y),
                    (right, bottom),
                ],
            )
            .await
        {
            Ok(points) => points,
            Err(message) => return rejected_click_verification(&params.target_id, message),
        };
        let received = Some(serde_json::json!(params.clone()));
        let action = self
            .execute_click(
                ClickParams {
                    x: Some(mapped[0].0),
                    y: Some(mapped[0].1),
                    button: params.button.clone(),
                    click_count: params.click_count,
                    ..Default::default()
                },
                received,
            )
            .await
            .0;
        if !action.dispatched {
            return ClickVerificationOutput {
                ok: false,
                stage: "rejected".to_string(),
                dispatched: false,
                landed: action.landed,
                verified: action.verified,
                target_id: params.target_id,
                screenshot_id: Some(target.screenshot_id),
                pointer_arrived: None,
                actual_pointer_x: None,
                actual_pointer_y: None,
                region_change_score: None,
                region_changed: None,
                expected_text_present: None,
                expected_text_absent: None,
                focused_editable: None,
                evidence_screenshot_id: None,
                message: action.message,
            };
        }

        let actual_pointer = if self.is_x11_session() {
            query_x11_pointer_position().ok()
        } else {
            None
        };
        let pointer_arrived = actual_pointer.map(|(x, y)| {
            let left = mapped[1].0.min(mapped[2].0);
            let top = mapped[1].1.min(mapped[2].1);
            let right = mapped[1].0.max(mapped[2].0);
            let bottom = mapped[1].1.max(mapped[2].1);
            x >= left && x <= right && y >= top && y <= bottom
        });
        let require_region = params.expect_region_changed.unwrap_or(
            params.expect_text_present.is_none()
                && params.expect_text_absent.is_none()
                && params.expect_focused_editable.is_none(),
        );
        let timeout_ms = params.timeout_ms.unwrap_or(2000).clamp(250, 5000);
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut last_score = None;
        let mut last_region_changed = None;
        let mut last_text_present = None;
        let mut last_text_absent = None;
        let mut last_focused_editable = None;
        let mut evidence_screenshot_id = None;

        loop {
            tokio::time::sleep(Duration::from_millis(180)).await;
            let evidence = match self.capture_evidence_for_artifact(&artifact).await {
                Ok(capture) => capture,
                Err(error) => {
                    if Instant::now() >= deadline {
                        return dispatched_unverified(
                            &params,
                            &target,
                            pointer_arrived,
                            actual_pointer,
                            last_score,
                            last_region_changed,
                            last_text_present,
                            last_text_absent,
                            last_focused_editable,
                            evidence_screenshot_id,
                            format!(
                                "Post-click capture failed: {error}. The click was not repeated."
                            ),
                        );
                    }
                    continue;
                }
            };
            evidence_screenshot_id = Some(evidence.screenshot_id.clone());
            let region = crate::action_verification::PixelRegion {
                x: target.bounds.x.max(0) as u32,
                y: target.bounds.y.max(0) as u32,
                width: target.bounds.width,
                height: target.bounds.height,
            };
            if let Ok(score) = crate::action_verification::region_change_score(
                &artifact.capture.data_url,
                &evidence.data_url,
                region,
            ) {
                last_score = Some(score);
                last_region_changed = Some(score >= 0.02);
            }

            if params.expect_text_present.is_some() || params.expect_text_absent.is_some() {
                if let Ok(result) =
                    crate::ocr::recognize(&evidence.data_url, &evidence.mime_type).await
                {
                    let visible_text = normalize_visual_text(
                        &result
                            .observations
                            .iter()
                            .map(|observation| observation.text.as_str())
                            .collect::<Vec<_>>()
                            .join(" "),
                    );
                    last_text_present = params
                        .expect_text_present
                        .as_ref()
                        .map(|expected| visible_text.contains(&normalize_visual_text(expected)));
                    last_text_absent = params
                        .expect_text_absent
                        .as_ref()
                        .map(|expected| !visible_text.contains(&normalize_visual_text(expected)));
                }
            }
            if params.expect_focused_editable.is_some() {
                last_focused_editable =
                    timeout(Duration::from_millis(1500), focused_element_summary(None))
                        .await
                        .ok()
                        .and_then(std::result::Result::ok)
                        .flatten()
                        .map(|element| element.editable);
            }

            let region_ok = if params.expect_region_changed.is_some() || require_region {
                last_region_changed == Some(require_region)
            } else {
                true
            };
            let text_present_ok =
                params.expect_text_present.is_none() || last_text_present == Some(true);
            let text_absent_ok =
                params.expect_text_absent.is_none() || last_text_absent == Some(true);
            let focus_ok = match params.expect_focused_editable {
                Some(expected) => last_focused_editable == Some(expected),
                None => true,
            };
            let pointer_ok = pointer_arrived != Some(false);
            if region_ok && text_present_ok && text_absent_ok && focus_ok && pointer_ok {
                return ClickVerificationOutput {
                    ok: true,
                    stage: "verified".to_string(),
                    dispatched: true,
                    landed: Some(true),
                    verified: true,
                    target_id: params.target_id,
                    screenshot_id: Some(target.screenshot_id),
                    pointer_arrived,
                    actual_pointer_x: actual_pointer.map(|point| point.0),
                    actual_pointer_y: actual_pointer.map(|point| point.1),
                    region_change_score: last_score,
                    region_changed: last_region_changed,
                    expected_text_present: last_text_present,
                    expected_text_absent: last_text_absent,
                    focused_editable: last_focused_editable,
                    evidence_screenshot_id,
                    message: format!(
                        "Click verified for {:?}{}.",
                        target.recognized_text,
                        target
                            .role
                            .as_deref()
                            .map(|role| format!(" ({role})"))
                            .unwrap_or_default()
                    ),
                };
            }
            if Instant::now() >= deadline {
                return dispatched_unverified(
                    &params,
                    &target,
                    pointer_arrived,
                    actual_pointer,
                    last_score,
                    last_region_changed,
                    last_text_present,
                    last_text_absent,
                    last_focused_editable,
                    evidence_screenshot_id,
                    "The click was dispatched but the requested evidence did not converge before timeout. The click was not repeated. Capture and locate again before deciding whether another click is safe."
                        .to_string(),
                );
            }
        }
    }

    async fn capture_screenshot_tool(
        &self,
        params: ScreenshotParams,
        encoding: ScreenshotEncodingPolicy,
    ) -> Result<CallToolResult, ErrorData> {
        let target = params.window_target();

        // When targeting a window, raise it first (so it isn't occluded) and
        // resolve its bounds so we can crop to just that window. A targeted
        // capture is strict: failure to identify usable bounds must not turn
        // into an apparently successful full-desktop screenshot.
        let mut crop: Option<crate::windowing::WindowBounds> = None;
        let mut window_label: Option<String> = None;
        let mut captured_window: Option<WindowSnapshot> = None;
        if let Some(target) = &target {
            if params.raise_window.unwrap_or(true) {
                let focus = focus_window_target(target).await.map_err(|error| {
                    ErrorData::internal_error(
                        format!("targeted screenshot could not raise the window: {error:#}"),
                        None,
                    )
                })?;
                if !focus_satisfies_target(&focus, target) {
                    return Err(ErrorData::internal_error(
                        format!(
                            "targeted screenshot could not verify focus for window_id {}",
                            focus.requested_window.window_id
                        ),
                        None,
                    ));
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            if !params.full_screen.unwrap_or(false) {
                let windows = list_windows().await.map_err(|error| {
                    ErrorData::internal_error(
                        format!("targeted screenshot could not list windows: {error:#}"),
                        None,
                    )
                })?;
                let window = resolve_window_target(&windows, target).map_err(|error| {
                    ErrorData::internal_error(
                        format!("targeted screenshot could not resolve the window: {error:#}"),
                        None,
                    )
                })?;
                if !window_bounds_match_capture_space(window) {
                    return Err(ErrorData::internal_error(
                        format!(
                            "targeted screenshot cannot safely crop window_id {} from backend {:?}: its bounds are not guaranteed to use physical screenshot pixels. Use full_screen=true or a capture-space X11 window backend.",
                            window.window_id, window.backend
                        ),
                        None,
                    ));
                }
                crop = Some(window.bounds.clone().ok_or_else(|| {
                    ErrorData::internal_error(
                        format!(
                            "targeted screenshot window_id {} has no bounds",
                            window.window_id
                        ),
                        None,
                    )
                })?);
                window_label = window.title.clone();
                captured_window = window_snapshot(window);
            }
        }

        let raw_capture = capture_screenshot_raw()
            .await
            .map_err(|e| ErrorData::internal_error(format!("screenshot failed: {e}"), None))?;
        self.cache_desktop_size(raw_capture.width, raw_capture.height);
        let layout_fingerprint = self
            .monitor_layout_fingerprint(raw_capture.width, raw_capture.height)
            .await;

        // Warn when the target window extends past the visible desktop. The
        // strict crop below returns only the visible intersection and reports
        // its actual origin and dimensions in the coordinate metadata.
        let off_screen_note = match crop.as_ref() {
            Some(bounds) => self.off_screen_note_for_bounds(bounds).await,
            None => None,
        };

        let crop_rect = crop
            .as_ref()
            .map(|bounds| {
                window_crop_rect_for_capture(bounds, raw_capture.width, raw_capture.height)
            })
            .transpose()
            .map_err(|error| {
                ErrorData::internal_error(format!("targeted screenshot crop failed: {error}"), None)
            })?;

        let (capture, cropped) = match crop_rect {
            Some(rect) => {
                let (bytes, width, height) =
                    crop_image_to_png(&raw_capture.bytes, rect.x, rect.y, rect.width, rect.height)
                        .map_err(|error| {
                            ErrorData::internal_error(
                                format!("targeted screenshot crop failed: {error}"),
                                None,
                            )
                        })?;
                if width != rect.width || height != rect.height {
                    return Err(ErrorData::internal_error(
                        format!(
                            "targeted screenshot crop returned unexpected dimensions {width}x{height}; expected {}x{}",
                            rect.width, rect.height
                        ),
                        None,
                    ));
                }
                (
                    RawScreenshotCapture {
                        bytes,
                        source: raw_capture.source.clone(),
                        width,
                        height,
                    },
                    true,
                )
            }
            None => (raw_capture, false),
        };
        let mut capture = prepare_screenshot_payload(capture, params.screenshot_options(encoding))
            .map_err(|e| {
                ErrorData::internal_error(format!("screenshot resize failed: {e}"), None)
            })?;
        if cropped {
            if let Some(rect) = crop_rect {
                capture.coordinate_origin_x = rect.x;
                capture.coordinate_origin_y = rect.y;
            }
            capture.cropped_to_window = true;
            capture.target_window_id = captured_window.map(|window| window.window_id);
        }
        self.cache_screenshot_artifact(&capture, layout_fingerprint, captured_window);

        let mut caption = serde_json::json!({
            "screenshot_id": capture.screenshot_id.clone(),
            "width": capture.width,
            "height": capture.height,
            "coordinate_width": capture.coordinate_width,
            "coordinate_height": capture.coordinate_height,
            "coordinate_origin_x": capture.coordinate_origin_x,
            "coordinate_origin_y": capture.coordinate_origin_y,
            "resized": capture.resized,
            "bytes": capture.bytes,
            "original_bytes": capture.original_bytes,
            "max_bytes": capture.max_bytes,
            "mime_type": capture.mime_type,
            "source": capture.source,
            "cropped_to_window": capture.cropped_to_window,
            "target_window_id": capture.target_window_id,
            "window_title": window_label,
            "coordinate_space": "screenshot",
            "coordinate_usage": "Pass image x/y and this screenshot_id to click_screenshot, scroll_screenshot, or drag_screenshot. Never copy image pixels into a desktop-coordinate tool.",
        });
        if let Some(note) = off_screen_note {
            caption["window_off_screen"] = serde_json::json!(true);
            caption["off_screen_note"] = serde_json::json!(note);
        }
        Ok(CallToolResult::success(vec![
            Content::image(data_url_payload(&capture.data_url), capture.mime_type),
            Content::text(caption.to_string()),
        ]))
    }

    /// Lazily create the uinput absolute pointer, sizing its ABS range to the
    /// logical desktop (the portal screenshot dimensions). Returns `false` if it
    /// can't be created or is disabled via `CU_DISABLE_ABS_POINTER` (or the
    /// Codex embedded-build alias).
    async fn ensure_abs_pointer(&self) -> bool {
        if env_flag_enabled_any(&[
            "CU_DISABLE_ABS_POINTER",
            "CODEX_COMPUTER_USE_DISABLE_ABS_POINTER",
        ]) {
            return false;
        }
        if self
            .abs_pointer
            .lock()
            .map(|g| g.is_some())
            .unwrap_or(false)
        {
            return true;
        }
        let Ok(cap) = capture_screenshot_raw().await else {
            return false;
        };
        self.cache_desktop_size(cap.width, cap.height);
        match tokio::task::spawn_blocking(move || {
            crate::abs_pointer::AbsPointer::create(cap.width as i32, cap.height as i32)
        })
        .await
        {
            Ok(Ok(pointer)) => {
                if let Ok(mut guard) = self.abs_pointer.lock() {
                    *guard = Some(pointer);
                    return true;
                }
                false
            }
            _ => false,
        }
    }

    /// Try a coordinate click through the absolute uinput pointer. `Some(ok)` if
    /// the backend was used; `None` to fall through to portal / ydotool.
    async fn try_abs_click(
        &self,
        x: i32,
        y: i32,
        button: Option<&str>,
        count: u32,
    ) -> Option<bool> {
        if !self.ensure_abs_pointer().await {
            return None;
        }
        let btn = crate::abs_pointer::PointerButton::from_name(button);
        let abs_pointer = Arc::clone(&self.abs_pointer);
        tokio::task::spawn_blocking(move || {
            let mut guard = abs_pointer.lock().ok()?;
            let pointer = guard.as_mut()?;
            Some(pointer.click(x, y, btn, count).is_ok())
        })
        .await
        .ok()
        .flatten()
    }

    #[tool(
        name = "click",
        description = "Click an accessibility element by index or semantic selector. Use click_screenshot for a point selected from an image, or click_desktop for an already-physical desktop point.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn click(
        &self,
        Parameters(params): Parameters<SemanticClickParams>,
    ) -> Json<ActionOutput> {
        let received = Some(serde_json::json!(params.clone()));
        self.execute_click(params.into(), received).await
    }

    #[tool(
        name = "click_desktop",
        description = "Click an explicit physical desktop pixel. Coordinates must not come from a resized or cropped screenshot; use click_screenshot for image pixels.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn click_desktop(
        &self,
        Parameters(params): Parameters<DesktopClickParams>,
    ) -> Json<ActionOutput> {
        let received = Some(serde_json::json!(params.clone()));
        self.execute_click(params.into(), received).await
    }

    #[tool(
        name = "click_screenshot",
        description = "Click a pixel from the exact screenshot identified by screenshot_id. The backend validates capture age, monitor layout, and target-window geometry before converting the point to desktop pixels.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn click_screenshot(
        &self,
        Parameters(params): Parameters<ScreenshotClickParams>,
    ) -> Json<ActionOutput> {
        let received = Some(serde_json::json!(params.clone()));
        let points = match self
            .resolve_screenshot_points(&params.screenshot_id, &[(params.x, params.y)])
            .await
        {
            Ok(points) => points,
            Err(message) => return Json(rejected_action("click", message, received)),
        };
        let (x, y) = points[0];
        let mut output = self
            .execute_click(
                ClickParams {
                    x: Some(x),
                    y: Some(y),
                    button: params.button,
                    click_count: params.click_count,
                    ..Default::default()
                },
                received,
            )
            .await;
        if output.0.dispatched && self.is_x11_session() {
            if let Ok((actual_x, actual_y)) = query_x11_pointer_position() {
                let arrived = actual_x == x && actual_y == y;
                output.0.landed = Some(arrived);
                output.0.verified = true;
                output.0.ok &= arrived;
                output.0.message.push_str(&format!(
                    " Pointer verification: expected {x},{y}, actual {actual_x},{actual_y}."
                ));
            }
        }
        output
    }

    async fn execute_click(
        &self,
        mut params: ClickParams,
        received: Option<serde_json::Value>,
    ) -> Json<ActionOutput> {
        // Raise the target window first (if specified) so the click lands on the
        // intended app rather than whatever is stacked on top at that pixel.
        let window_target = params.window_target();
        if params.relative == Some(true) && window_target.is_none() {
            return Json(ActionOutput {
                dispatched: false,
                landed: None,
                verified: false,
                ok: false,
                implemented: true,
                action: "click".to_string(),
                message: "Relative coordinate clicks require a window target.".to_string(),
                received,
            });
        }
        if let Some(target) = window_target {
            let focus = match self.focus_target_for_input(&target).await {
                Ok(focus) => focus,
                Err(message) => {
                    return Json(ActionOutput {
                        dispatched: false,
                        landed: None,
                        verified: false,
                        ok: false,
                        implemented: true,
                        action: "click".to_string(),
                        message,
                        received,
                    });
                }
            };
            tokio::time::sleep(Duration::from_millis(120)).await;
            // Window-relative coordinates: translate by the window's top-left so
            // the agent can click the pixel it saw in a window-cropped screenshot.
            if params.relative == Some(true) {
                let Some(focus) = focus.as_ref() else {
                    return Json(ActionOutput {
                        dispatched: false,
                        landed: None,
                        verified: false,
                        ok: false,
                        implemented: true,
                        action: "click".to_string(),
                        message: "Relative coordinate clicks require verified target-window focus."
                            .to_string(),
                        received,
                    });
                };
                if let Err(message) = apply_window_relative_click_coordinates(&mut params, focus) {
                    return Json(ActionOutput {
                        dispatched: false,
                        landed: None,
                        verified: false,
                        ok: false,
                        implemented: true,
                        action: "click".to_string(),
                        message,
                        received,
                    });
                }
            }
        }
        let target = match self.resolve_click_target(&params) {
            Ok(target) => target,
            Err(message) => {
                return Json(ActionOutput {
                    dispatched: false,
                    landed: None,
                    verified: false,
                    ok: false,
                    implemented: true,
                    action: "click".to_string(),
                    message,
                    received,
                });
            }
        };
        let mut native_action_fallback_note = None;
        let (x, y) = match target {
            ClickTarget::Coordinates(x, y) => (x, y),
            ClickTarget::PrimaryAction {
                object_ref,
                action_name,
                action_index,
                fallback_coordinates,
            } => {
                let action_label = action_name
                    .as_deref()
                    .filter(|name| !name.is_empty())
                    .map(|name| format!(" ({name})"))
                    .unwrap_or_default();
                let action_index = action_index.to_string();
                match invoke_accessibility_action(&object_ref, Some(&action_index)).await {
                    Ok(invocation) if invocation.ok => {
                        return Json(ActionOutput {
                            dispatched: true,
                            landed: Some(true),
                            verified: true,
                            ok: true,
                            implemented: true,
                            action: "click".to_string(),
                            message: format!(
                                "Invoked the primary AT-SPI action{action_label}; no coordinate conversion was needed."
                            ),
                            received,
                        });
                    }
                    Ok(_) => {
                        let Some(point) = fallback_coordinates else {
                            return Json(ActionOutput {
                                dispatched: false,
                                landed: Some(false),
                                verified: true,
                                ok: false,
                                implemented: true,
                                action: "click".to_string(),
                                message: format!(
                                    "The primary AT-SPI action{action_label} returned false and no coordinate fallback is available."
                                ),
                                received,
                            });
                        };
                        native_action_fallback_note = Some(format!(
                            "The primary AT-SPI action{action_label} returned false, so the backend used its coordinate fallback."
                        ));
                        point
                    }
                    Err(error) => {
                        let Some(point) = fallback_coordinates else {
                            return Json(ActionOutput {
                                dispatched: false,
                                landed: None,
                                verified: false,
                                ok: false,
                                implemented: true,
                                action: "click".to_string(),
                                message: error.to_string(),
                                received,
                            });
                        };
                        native_action_fallback_note = Some(format!(
                            "The primary AT-SPI action{action_label} failed ({}), so the backend used its coordinate fallback.",
                            first_line(&error.to_string())
                        ));
                        point
                    }
                }
            }
        };
        let button = mouse_button_code(params.button.as_deref());
        let click_count = params.click_count.unwrap_or(1).clamp(1, 10).to_string();
        // After the native X11 xdotool path, prefer the uinput absolute pointer. Unlike ydotool's
        // relative-only device (faked `--absolute` via pin-to-corner + relative
        // move, which acceleration + fractional scaling distort) and unlike the
        // portal (per-monitor coordinate scaling + an approval dialog), the
        // absolute pointer lands exactly at the screenshot pixel.
        // Off-screen coordinates "succeed" at the uinput layer while landing on
        // no visible pixel — surface that instead of a silent no-op.
        let off_screen_note = self.off_screen_note_for_point(x, y).await;
        let mut coordinate_feedback = ActionFeedback::default();
        if let Some(note) = native_action_fallback_note {
            coordinate_feedback.notes.push(note);
        }
        if let Some(note) = off_screen_note {
            coordinate_feedback.merge(ActionFeedback::failed_landing(note));
        }
        if self.should_prefer_xdotool_input_backend() {
            let result = run_xdotool(&xdotool_click_args(
                x,
                y,
                params.button.as_deref(),
                params.click_count.unwrap_or(1).clamp(1, 10),
            ))
            .await
            .map(|output| vec![output]);
            return Json(with_action_feedback(
                action_result_for_backend("click", result, received, "xdotool"),
                coordinate_feedback,
            ));
        }
        if self
            .try_abs_click(
                x,
                y,
                params.button.as_deref(),
                params.click_count.unwrap_or(1).clamp(1, 10),
            )
            .await
            == Some(true)
        {
            return Json(with_action_feedback(
                ActionOutput {
                    dispatched: true,
                    landed: None,
                    verified: false,
                    ok: true,
                    implemented: true,
                    action: "click".to_string(),
                    message: "Action sent through the uinput absolute pointer.".to_string(),
                    received,
                },
                coordinate_feedback.clone(),
            ));
        }
        if let Some(session) = self.cached_portal_pointer_session() {
            match portal_click(
                &session,
                x,
                y,
                PointerButton::from_name(params.button.as_deref()),
                params.click_count.unwrap_or(1).clamp(1, 10),
            )
            .await
            {
                Ok(()) => {
                    return Json(with_action_feedback(
                        ActionOutput {
                            dispatched: true,
                            landed: None,
                            verified: false,
                            ok: true,
                            implemented: true,
                            action: "click".to_string(),
                            message: "Action sent through the remote desktop portal.".to_string(),
                            received,
                        },
                        coordinate_feedback.clone(),
                    ));
                }
                Err(_) => self.clear_portal_pointer_session(),
            }
        } else if self.should_prefer_portal_pointer_backend() {
            match self.ensure_portal_pointer_session().await {
                Ok(Some(session)) => match portal_click(
                    &session,
                    x,
                    y,
                    PointerButton::from_name(params.button.as_deref()),
                    params.click_count.unwrap_or(1).clamp(1, 10),
                )
                .await
                {
                    Ok(()) => {
                        return Json(with_action_feedback(
                            ActionOutput {
                                dispatched: true,
                                landed: None,
                                verified: false,
                                ok: true,
                                implemented: true,
                                action: "click".to_string(),
                                message: "Action sent through the remote desktop portal."
                                    .to_string(),
                                received,
                            },
                            coordinate_feedback.clone(),
                        ));
                    }
                    Err(_) => self.clear_portal_pointer_session(),
                },
                Ok(None) => {}
                Err(_) => {}
            }
        }
        let result = run_ydotool_sequence(&[
            absolute_mousemove_args(x, y),
            vec![
                "click".to_string(),
                "--repeat".to_string(),
                click_count,
                button,
            ],
        ])
        .await;
        Json(with_action_feedback(
            action_result("click", result, received),
            coordinate_feedback,
        ))
    }

    #[tool(
        name = "perform_action",
        description = "Invoke an accessibility action exposed by an element selected by index, identifier, or semantic selector. Defaults to the primary action unless action is provided.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn perform_action(
        &self,
        Parameters(params): Parameters<ActionParams>,
    ) -> Json<ActionOutput> {
        let requested_action = requested_or_primary_action(params.action.as_deref());
        self.perform_element_action(&params, Some(requested_action))
            .await
    }

    #[tool(
        name = "set_value",
        description = "Set the value of a settable accessibility element selected by index, identifier, or semantic selector.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn set_value(
        &self,
        Parameters(params): Parameters<SetValueParams>,
    ) -> Json<ActionOutput> {
        let received = Some(serde_json::json!(params.clone()));
        let object_ref = match self.resolve_object_ref(
            params.element_index,
            params.element_identifier.as_deref(),
            &params.selector(),
            ElementResolvePurpose::SetValue,
        ) {
            Ok(object_ref) => object_ref,
            Err(message) => {
                return Json(ActionOutput {
                    dispatched: false,
                    landed: None,
                    verified: false,
                    ok: false,
                    implemented: true,
                    action: "set_value".to_string(),
                    message,
                    received,
                });
            }
        };

        match set_element_value(&object_ref, &params.value).await {
            Ok(ValueSetInvocation::Numeric { value }) => Json(ActionOutput {
                dispatched: true,
                landed: Some(true),
                verified: true,
                ok: true,
                implemented: true,
                action: "set_value".to_string(),
                message: format!("AT-SPI numeric value set to {value}."),
                received,
            }),
            Ok(ValueSetInvocation::EditableText) => Json(ActionOutput {
                dispatched: true,
                landed: Some(true),
                verified: true,
                ok: true,
                implemented: true,
                action: "set_value".to_string(),
                message: "AT-SPI editable text contents set.".to_string(),
                received,
            }),
            Err(error) => Json(ActionOutput {
                dispatched: false,
                landed: None,
                verified: false,
                ok: false,
                implemented: true,
                action: "set_value".to_string(),
                message: error.to_string(),
                received,
            }),
        }
    }

    #[tool(
        name = "scroll",
        description = "Scroll at a cached accessibility element, or at the current pointer when element_index is omitted. Use scroll_screenshot or scroll_desktop for explicit visual points.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn scroll(
        &self,
        Parameters(params): Parameters<SemanticScrollParams>,
    ) -> Json<ActionOutput> {
        let received = Some(serde_json::json!(params.clone()));
        self.execute_scroll(params.into(), received).await
    }

    #[tool(
        name = "scroll_desktop",
        description = "Scroll at an explicit physical desktop pixel. Use scroll_screenshot when the point came from an image.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn scroll_desktop(
        &self,
        Parameters(params): Parameters<DesktopScrollParams>,
    ) -> Json<ActionOutput> {
        let received = Some(serde_json::json!(params.clone()));
        self.execute_scroll(params.into(), received).await
    }

    #[tool(
        name = "scroll_screenshot",
        description = "Scroll at a pixel from the exact screenshot identified by screenshot_id after validating the capture context.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn scroll_screenshot(
        &self,
        Parameters(params): Parameters<ScreenshotScrollParams>,
    ) -> Json<ActionOutput> {
        let received = Some(serde_json::json!(params.clone()));
        let points = match self
            .resolve_screenshot_points(&params.screenshot_id, &[(params.x, params.y)])
            .await
        {
            Ok(points) => points,
            Err(message) => return Json(rejected_action("scroll", message, received)),
        };
        let (x, y) = points[0];
        self.execute_scroll(
            ScrollParams {
                element_index: None,
                x: Some(x),
                y: Some(y),
                direction: params.direction,
                pages: params.pages,
                window_id: None,
                pid: None,
                app_id: None,
                wm_class: None,
                window_title: None,
                relative: None,
            },
            received,
        )
        .await
    }

    async fn execute_scroll(
        &self,
        mut params: ScrollParams,
        received: Option<serde_json::Value>,
    ) -> Json<ActionOutput> {
        let units = ((params.pages.unwrap_or(1.0).abs().max(0.1) * 5.0).round() as i32).max(1);
        // Raise/focus the target window first (parity with click) so wheel
        // events land on the intended app.
        let window_target = params.window_target();
        if params.relative == Some(true) && window_target.is_none() {
            return Json(ActionOutput {
                dispatched: false,
                landed: None,
                verified: false,
                ok: false,
                implemented: true,
                action: "scroll".to_string(),
                message: "Relative scroll coordinates require a window target.".to_string(),
                received,
            });
        }
        if let Some(target) = window_target {
            let focus = match self.focus_target_for_input(&target).await {
                Ok(focus) => focus,
                Err(message) => {
                    return Json(ActionOutput {
                        dispatched: false,
                        landed: None,
                        verified: false,
                        ok: false,
                        implemented: true,
                        action: "scroll".to_string(),
                        message,
                        received,
                    });
                }
            };
            tokio::time::sleep(Duration::from_millis(120)).await;
            if params.relative == Some(true) {
                let Some(focus) = focus.as_ref() else {
                    return Json(ActionOutput {
                        dispatched: false,
                        landed: None,
                        verified: false,
                        ok: false,
                        implemented: true,
                        action: "scroll".to_string(),
                        message:
                            "Relative scroll coordinates require verified target-window focus."
                                .to_string(),
                        received,
                    });
                };
                if let Err(message) = apply_window_relative_scroll_coordinates(&mut params, focus) {
                    return Json(ActionOutput {
                        dispatched: false,
                        landed: None,
                        verified: false,
                        ok: false,
                        implemented: true,
                        action: "scroll".to_string(),
                        message,
                        received,
                    });
                }
            } else if params.x.is_none() && params.y.is_none() && params.element_index.is_none() {
                // A window target without a point would otherwise scroll
                // whatever happens to sit under the pointer: focusing does not
                // move the cursor, and the wheel path never repositions it.
                // Default to the centre of the resolved target window.
                let Some(focus) = focus.as_ref() else {
                    return Json(ActionOutput {
                        dispatched: false,
                        landed: None,
                        verified: false,
                        ok: false,
                        implemented: true,
                        action: "scroll".to_string(),
                        message: "Window-targeted scroll requires verified target-window focus."
                            .to_string(),
                        received,
                    });
                };
                if let Err(message) = apply_window_center_scroll_point(&mut params, focus) {
                    return Json(ActionOutput {
                        dispatched: false,
                        landed: None,
                        verified: false,
                        ok: false,
                        implemented: true,
                        action: "scroll".to_string(),
                        message,
                        received,
                    });
                }
            }
        }
        let target_point =
            match self.resolve_optional_target_point(params.x, params.y, params.element_index) {
                Ok(point) => point,
                Err(message) => {
                    return Json(ActionOutput {
                        dispatched: false,
                        landed: None,
                        verified: false,
                        ok: false,
                        implemented: true,
                        action: "scroll".to_string(),
                        message,
                        received,
                    });
                }
            };
        let direction = match params.direction.to_ascii_lowercase().as_str() {
            "up" => ScrollDirection::Up,
            "down" => ScrollDirection::Down,
            "left" => ScrollDirection::Left,
            "right" => ScrollDirection::Right,
            _ => {
                return Json(ActionOutput {
                    dispatched: false,
                    landed: None,
                    verified: false,
                    ok: false,
                    implemented: true,
                    action: "scroll".to_string(),
                    message: "Unsupported scroll direction; expected up, down, left, or right."
                        .to_string(),
                    received,
                });
            }
        };
        let off_screen_note = match target_point {
            Some((x, y)) => self.off_screen_note_for_point(x, y).await,
            None => None,
        };
        let off_screen_feedback = off_screen_note
            .map(ActionFeedback::failed_landing)
            .unwrap_or_default();

        if self.should_prefer_xdotool_input_backend() {
            let result = run_xdotool(&xdotool_scroll_args(
                target_point,
                params.direction.as_str(),
                units,
            ))
            .await
            .map(|output| vec![output]);
            return Json(with_action_feedback(
                action_result_for_backend("scroll", result, received, "xdotool"),
                off_screen_feedback,
            ));
        }

        if let Some(session) = self.cached_portal_pointer_session() {
            match portal_scroll(&session, target_point, direction, units).await {
                Ok(()) => {
                    return Json(with_action_feedback(
                        ActionOutput {
                            dispatched: true,
                            landed: None,
                            verified: false,
                            ok: true,
                            implemented: true,
                            action: "scroll".to_string(),
                            message: "Action sent through the remote desktop portal.".to_string(),
                            received,
                        },
                        off_screen_feedback.clone(),
                    ));
                }
                Err(_) => self.clear_portal_pointer_session(),
            }
        } else if self.should_prefer_portal_pointer_backend() {
            match self.ensure_portal_pointer_session().await {
                Ok(Some(session)) => {
                    match portal_scroll(&session, target_point, direction, units).await {
                        Ok(()) => {
                            return Json(with_action_feedback(
                                ActionOutput {
                                    dispatched: true,
                                    landed: None,
                                    verified: false,
                                    ok: true,
                                    implemented: true,
                                    action: "scroll".to_string(),
                                    message: "Action sent through the remote desktop portal."
                                        .to_string(),
                                    received,
                                },
                                off_screen_feedback.clone(),
                            ));
                        }
                        Err(_) => self.clear_portal_pointer_session(),
                    }
                }
                Ok(None) => {}
                Err(_) => {}
            }
        }
        let (dx, dy) = match params.direction.to_ascii_lowercase().as_str() {
            "up" => (0, units),
            "down" => (0, -units),
            "left" => (units, 0),
            "right" => (-units, 0),
            _ => {
                return Json(ActionOutput {
                    dispatched: false,
                    landed: None,
                    verified: false,
                    ok: false,
                    implemented: true,
                    action: "scroll".to_string(),
                    message: "Unsupported scroll direction; expected up, down, left, or right."
                        .to_string(),
                    received,
                });
            }
        };
        let mut sequence = Vec::new();
        if let Some((x, y)) = target_point {
            sequence.push(absolute_mousemove_args(x, y));
        }
        sequence.push(wheel_mousemove_args(dx, dy));
        let result = run_ydotool_sequence(&sequence).await;
        Json(with_action_feedback(
            action_result("scroll", result, received),
            off_screen_feedback,
        ))
    }

    #[tool(
        name = "drag_desktop",
        description = "Drag between two explicit physical desktop pixels. Use drag_screenshot when either point came from an image.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn drag_desktop(
        &self,
        Parameters(params): Parameters<DesktopDragParams>,
    ) -> Json<ActionOutput> {
        let received = Some(serde_json::json!(params.clone()));
        self.execute_drag(
            DragParams {
                start_x: params.start_x,
                start_y: params.start_y,
                end_x: params.end_x,
                end_y: params.end_y,
            },
            received,
        )
        .await
    }

    #[tool(
        name = "drag_screenshot",
        description = "Drag between two pixels from the exact screenshot identified by screenshot_id after validating the capture context.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn drag_screenshot(
        &self,
        Parameters(params): Parameters<ScreenshotDragParams>,
    ) -> Json<ActionOutput> {
        let received = Some(serde_json::json!(params.clone()));
        let points = match self
            .resolve_screenshot_points(
                &params.screenshot_id,
                &[
                    (params.start_x, params.start_y),
                    (params.end_x, params.end_y),
                ],
            )
            .await
        {
            Ok(points) => points,
            Err(message) => return Json(rejected_action("drag", message, received)),
        };
        self.execute_drag(
            DragParams {
                start_x: points[0].0,
                start_y: points[0].1,
                end_x: points[1].0,
                end_y: points[1].1,
            },
            received,
        )
        .await
    }

    async fn execute_drag(
        &self,
        params: DragParams,
        received: Option<serde_json::Value>,
    ) -> Json<ActionOutput> {
        let mut drag_feedback = ActionFeedback::default();
        if let Some(note) = self
            .off_screen_note_for_point(params.start_x, params.start_y)
            .await
        {
            drag_feedback.merge(ActionFeedback::failed_landing(format!(
                "Drag start: {note}"
            )));
        }
        if let Some(note) = self
            .off_screen_note_for_point(params.end_x, params.end_y)
            .await
        {
            drag_feedback.merge(ActionFeedback::failed_landing(format!("Drag end: {note}")));
        }
        if self.should_prefer_xdotool_input_backend() {
            let result = run_xdotool(&xdotool_drag_args(
                params.start_x,
                params.start_y,
                params.end_x,
                params.end_y,
            ))
            .await
            .map(|output| vec![output]);
            return Json(with_action_feedback(
                action_result_for_backend("drag", result, received, "xdotool"),
                drag_feedback,
            ));
        }
        // After the native X11 xdotool path, prefer the uinput absolute pointer.
        if self.ensure_abs_pointer().await {
            let abs_pointer = Arc::clone(&self.abs_pointer);
            let dragged = tokio::task::spawn_blocking(move || {
                if let Ok(mut guard) = abs_pointer.lock() {
                    guard.as_mut().map(|p| {
                        p.drag(
                            (params.start_x, params.start_y),
                            (params.end_x, params.end_y),
                            crate::abs_pointer::PointerButton::Left,
                        )
                        .is_ok()
                    })
                } else {
                    None
                }
            })
            .await
            .ok()
            .flatten();
            if dragged == Some(true) {
                return Json(with_action_feedback(
                    ActionOutput {
                        dispatched: true,
                        landed: None,
                        verified: false,
                        ok: true,
                        implemented: true,
                        action: "drag".to_string(),
                        message: "Action sent through the uinput absolute pointer.".to_string(),
                        received,
                    },
                    drag_feedback.clone(),
                ));
            }
        }
        if let Some(session) = self.cached_portal_pointer_session() {
            match portal_drag(
                &session,
                params.start_x,
                params.start_y,
                params.end_x,
                params.end_y,
            )
            .await
            {
                Ok(()) => {
                    return Json(with_action_feedback(
                        ActionOutput {
                            dispatched: true,
                            landed: None,
                            verified: false,
                            ok: true,
                            implemented: true,
                            action: "drag".to_string(),
                            message: "Action sent through the remote desktop portal.".to_string(),
                            received,
                        },
                        drag_feedback.clone(),
                    ));
                }
                Err(_) => self.clear_portal_pointer_session(),
            }
        } else if self.should_prefer_portal_pointer_backend() {
            match self.ensure_portal_pointer_session().await {
                Ok(Some(session)) => match portal_drag(
                    &session,
                    params.start_x,
                    params.start_y,
                    params.end_x,
                    params.end_y,
                )
                .await
                {
                    Ok(()) => {
                        return Json(with_action_feedback(
                            ActionOutput {
                                dispatched: true,
                                landed: None,
                                verified: false,
                                ok: true,
                                implemented: true,
                                action: "drag".to_string(),
                                message: "Action sent through the remote desktop portal."
                                    .to_string(),
                                received,
                            },
                            drag_feedback.clone(),
                        ));
                    }
                    Err(_) => self.clear_portal_pointer_session(),
                },
                Ok(None) => {}
                Err(_) => {}
            }
        }
        let result = run_ydotool_sequence(&[
            absolute_mousemove_args(params.start_x, params.start_y),
            vec!["click".to_string(), "0x40".to_string()],
            absolute_mousemove_args(params.end_x, params.end_y),
            vec!["click".to_string(), "0x80".to_string()],
        ])
        .await;
        Json(with_action_feedback(
            action_result("drag", result, received),
            drag_feedback,
        ))
    }

    #[tool(
        name = "press_key",
        description = "Press a key or key-combination on the keyboard, optionally after focusing a target window or terminal selector. Key grammar (case-insensitive; hyphens/spaces ignored): combos join with '+', e.g. Ctrl+L or Ctrl+Shift+T. Modifiers: ctrl/control, alt/option, shift, meta/super/cmd/command. Named keys: enter/return, escape/esc, tab, backspace, delete/del, space, home, end, pageup, pagedown, arrowleft/left, arrowright/right, arrowup/up, arrowdown/down, f1-f12. Plus single US letters a-z and digits 0-9. Anything else returns an error (never silently dropped). Note: compositor-level shortcuts (e.g. Super+Up) may be consumed by GNOME before reaching the app.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn press_key(
        &self,
        Parameters(params): Parameters<PressKeyParams>,
    ) -> Json<ActionOutput> {
        let received = Some(serde_json::json!(params.clone()));
        let focus = match self.focus_target_for_input(&params.window_target()).await {
            Ok(focus) => focus,
            Err(message) => {
                return Json(ActionOutput {
                    dispatched: false,
                    landed: None,
                    verified: false,
                    ok: false,
                    implemented: true,
                    action: "press_key".to_string(),
                    message,
                    received,
                });
            }
        };
        let Some(key_events) = key_sequence(&params.key) else {
            return Json(ActionOutput {
                dispatched: false,
                landed: None,
                verified: false,
                ok: false,
                implemented: true,
                action: "press_key".to_string(),
                message: "Unsupported key. Use names like Enter, Escape, Tab, ArrowLeft, Super, Ctrl+L, or a single US keyboard letter/digit.".to_string(),
                received,
            });
        };
        if self.should_prefer_xdotool_input_backend() {
            let key = xdotool_key_chord(&params.key)
                .expect("xdotool and ydotool key grammars must stay aligned");
            let result = run_xdotool(&["key".to_string(), "--clearmodifiers".to_string(), key])
                .await
                .map(|output| vec![output]);
            let mut output = action_result_with_focus_for_backend(
                "press_key",
                result,
                received,
                focus.clone(),
                "xdotool",
            );
            if output.ok && focus.is_some() {
                let feedback = self.input_landing_feedback(focus.as_ref(), false).await;
                output = with_action_feedback(output, feedback);
            }
            return Json(output);
        }
        let mut args = vec!["key".to_string()];
        args.extend(key_events);
        let result = run_ydotool(&args).await.map(|output| vec![output]);
        let mut output = action_result_with_focus("press_key", result, received, focus.clone());
        if output.ok && focus.is_some() {
            let feedback = self.input_landing_feedback(focus.as_ref(), false).await;
            output = with_action_feedback(output, feedback);
        }
        Json(output)
    }

    #[tool(
        name = "type_text",
        description = "Type literal text using keyboard input, optionally after focusing a target window or terminal selector.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn type_text(
        &self,
        Parameters(params): Parameters<TypeTextParams>,
    ) -> Json<ActionOutput> {
        let received = Some(serde_json::json!(params.clone()));
        let window_target = params.window_target();
        let focus = match self.focus_target_for_input(&window_target).await {
            Ok(focus) => focus,
            Err(message) => {
                return Json(ActionOutput {
                    dispatched: false,
                    landed: None,
                    verified: false,
                    ok: false,
                    implemented: true,
                    action: "type_text".to_string(),
                    message,
                    received,
                });
            }
        };
        let mut x11_clipboard_fallback_note = None;
        if self.should_prefer_x11_clipboard_text_backend() {
            let terminal_target = window_target.has_terminal_target()
                || focus.as_ref().is_some_and(|focus| {
                    window_uses_terminal_paste(&focus.requested_window)
                        || focus
                            .focused_window
                            .as_ref()
                            .is_some_and(window_uses_terminal_paste)
                });
            let _clipboard_guard = self.clipboard_lock.lock().await;
            match run_x11_clipboard_paste_text(
                &self.x11_clipboard,
                &self.x11_raw_clipboard,
                &params.text,
                terminal_target,
            )
            .await
            {
                Ok(message) => {
                    let feedback = self.input_landing_feedback(focus.as_ref(), true).await;
                    return Json(with_action_feedback(
                        successful_action_with_focus("type_text", &message, received, focus),
                        feedback,
                    ));
                }
                Err(error) => {
                    if !error.can_fallback_to_xdotool {
                        if error.dispatched {
                            return Json(with_focus_context(
                                ActionOutput {
                                    dispatched: true,
                                    landed: None,
                                    verified: false,
                                    ok: false,
                                    implemented: true,
                                    action: "type_text".to_string(),
                                    message: error.message,
                                    received,
                                },
                                focus,
                            ));
                        }
                        return Json(action_result_with_focus(
                            "type_text",
                            Err(error.message),
                            received,
                            focus,
                        ));
                    }
                    x11_clipboard_fallback_note = Some(format!(
                        "X11 clipboard paste was unavailable before Ctrl+V was sent ({}), so the backend used xdotool key-by-key typing.",
                        first_line(&error.message)
                    ));
                }
            }
        }
        if self.should_prefer_xdotool_input_backend() {
            let result = run_xdotool(&[
                "type".to_string(),
                "--clearmodifiers".to_string(),
                "--delay".to_string(),
                "1".to_string(),
                "--".to_string(),
                params.text.clone(),
            ])
            .await
            .map(|output| vec![output]);
            let mut output = action_result_with_focus_for_backend(
                "type_text",
                result,
                received,
                focus.clone(),
                "xdotool",
            );
            if output.ok && focus.is_some() {
                let feedback = self.input_landing_feedback(focus.as_ref(), true).await;
                output = with_action_feedback(output, feedback);
            }
            if let Some(note) = x11_clipboard_fallback_note {
                output = with_notes(output, [note]);
            }
            return Json(output);
        }
        if self.should_prefer_kde_clipboard_text_backend() {
            match self.ensure_portal_keyboard_session().await {
                Ok(Some(session)) => {
                    let _clipboard_guard = self.clipboard_lock.lock().await;
                    match run_kde_clipboard_paste_text(&session, &params.text).await {
                        Ok(message) => {
                            let feedback = self.input_landing_feedback(focus.as_ref(), true).await;
                            return Json(with_action_feedback(
                                successful_action_with_focus(
                                    "type_text",
                                    &message,
                                    received,
                                    focus,
                                ),
                                feedback,
                            ));
                        }
                        Err(error) => {
                            if error.clear_portal_keyboard_session {
                                self.clear_portal_keyboard_session();
                            }
                            if !error.can_fallback_to_ydotool {
                                return Json(action_result_with_focus(
                                    "type_text",
                                    Err(error.message),
                                    received,
                                    focus,
                                ));
                            }
                        }
                    }
                }
                Ok(None) => {}
                Err(_) => {}
            }
        }
        if self.should_prefer_portal_keyboard_backend() {
            if let Ok(keysyms) = keysyms_for_text(&params.text) {
                match self.ensure_portal_keyboard_session().await {
                    Ok(Some(session)) => match type_text_with_keysyms(&session, &keysyms).await {
                        Ok(()) => {
                            let feedback = self.input_landing_feedback(focus.as_ref(), true).await;
                            return Json(with_action_feedback(
                                successful_action_with_focus(
                                    "type_text",
                                    "Action sent through the remote desktop portal.",
                                    received,
                                    focus,
                                ),
                                feedback,
                            ));
                        }
                        Err(error) => {
                            self.clear_portal_keyboard_session();
                            return Json(action_result_with_focus(
                                "type_text",
                                Err(format!("{error:#}")),
                                received,
                                focus,
                            ));
                        }
                    },
                    Ok(None) => {}
                    Err(_) => {}
                }
            }
        }
        let result = run_ydotool_type_text(&params.text)
            .await
            .map(|output| vec![output]);
        let mut output = action_result_with_focus("type_text", result, received, focus.clone());
        if output.ok && focus.is_some() {
            let feedback = self.input_landing_feedback(focus.as_ref(), true).await;
            output = with_action_feedback(output, feedback);
        }
        Json(output)
    }

    #[tool(
        name = "move_window",
        description = "Move a window to a new desktop position (frame top-left in desktop coordinates). Useful to recover windows that are partially off-screen. Requires the computer-use-linux GNOME Shell extension.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn move_window(
        &self,
        Parameters(params): Parameters<MoveWindowParams>,
    ) -> Json<WindowGeometryOutput> {
        let received = Some(serde_json::json!(params.clone()));
        let target = params.target.clone().into_target();
        self.window_geometry_op(received, &target, |window_id| async move {
            crate::windowing::backends::gnome::move_extension_window(window_id, params.x, params.y)
                .await
        })
        .await
    }

    #[tool(
        name = "resize_window",
        description = "Resize a window to a new frame width/height in desktop pixels, unmaximizing it first if needed. Useful to fit a window fully on-screen. Requires the computer-use-linux GNOME Shell extension.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn resize_window(
        &self,
        Parameters(params): Parameters<ResizeWindowParams>,
    ) -> Json<WindowGeometryOutput> {
        let received = Some(serde_json::json!(params.clone()));
        let target = params.target.clone().into_target();
        self.window_geometry_op(received, &target, |window_id| async move {
            crate::windowing::backends::gnome::resize_extension_window(
                window_id,
                params.width,
                params.height,
            )
            .await
        })
        .await
    }
}

#[tool_handler(
    name = "codex-computer-use-linux",
    // NOTE: keep in lockstep with Cargo.toml + package.json on every release.
    // The rmcp tool_handler macro only accepts a string literal here, so this
    // can't be env!("CARGO_PKG_VERSION"); the MCP safety check (CI) fails the
    // build if it drifts from the Cargo version.
    version = "0.3.1-linux-alpha1",
    /* Historical instructions retained in this comment until the next plugin
     * cache-breaking release; the active contract follows below.
    instructions = "Begin every turn that uses Computer Use by calling get_app_state with only the needed app/window selectors; keep the first call on the default PNG and omit format/quality. Tool arguments must always be strict JSON. Enum values such as jpeg and png are JSON strings: use {\"format\":\"jpeg\",\"quality\":70}, never a bare value such as {\"format\":jpeg}. If a tool call reports a JSON argument parse error, retry it once with strict JSON instead of ending the task. If diagnostics report disabled accessibility on GNOME, call setup_accessibility before asking the user to retry. If AT-SPI is unavailable on a non-GNOME desktop such as Deepin, do not repeatedly call setup_accessibility; continue with screenshots and coordinate input when readiness confirms an input backend. Use list_windows/focused_window before targeted keyboard input when window introspection is available. If diagnostics report windowing.can_list_windows=false on GNOME, call setup_window_targeting to install the optional GNOME Shell extension backend, then ask the user to log out and back in if the setup report says a shell reload is required. This Linux backend can capture size-bounded screenshots through GNOME Shell, the Codex GNOME Shell extension, or XDG Desktop Portal, read AT-SPI trees with action/value metadata, invoke native AT-SPI actions, set AT-SPI values or editable text, list/focus compositor windows through registered Linux window backends when the session permits it, attach best-effort terminal tty/process metadata to terminal windows, send exact window focus plus pointer/keyboard input through xdotool on X11, paste exact X11 text through a verified system-clipboard transaction, send coordinate or element-targeted input through the Wayland remote desktop portal when available, and use ydotool as a fallback. Action results separate dispatched (the backend sent the action), landed (target-level evidence, or null when unknown), and verified (whether the landing conclusion has conclusive post-action evidence); treat landed=false as failure even when dispatched=true. Screenshot results include a unique screenshot_id, width/height for the returned image, coordinate_width/coordinate_height, coordinate origin, and scale metadata. For every point selected visually from a returned image, pass coordinate_space=\"screenshot\" and the screenshot_id from that same image to click, scroll, or drag so resize and crop offsets are converted automatically; missing and stale IDs are rejected, and desktop is only for already-physical desktop pixels. A later image replaces the cached mapping, so coordinates and screenshot_id must always come from the same result. A targeted screenshot without full_screen=true is strict: target resolution, bounds, and crop failures return errors rather than silently returning the whole desktop. Request more detail with max_width, max_height, max_bytes, a strict JSON format string, quality, or a smaller target/crop instead of relying on unbounded screenshots. Tools with readOnlyHint=false may mutate local desktop or application state; hosts should require approval for actions that can submit, delete, send, purchase, or overwrite data. For element-targeted actions, prefer element_index from the latest get_app_state result; click, perform_action, and set_value can also use semantic role/name/text/states selectors when the target is unique. Plain left clicks prefer native AT-SPI actions, and scaled X11 coordinate fallbacks are aligned to the physical target window. type_text and press_key accept optional window_id, pid, app_id, wm_class, title, tty, terminal_pid, terminal_command, or terminal_cwd selectors and refuse targeted input if focus cannot be verified. After targeted keyboard input, results append focused-element feedback from AT-SPI (role, name, editable); a conclusively non-editable target returns landed=false and ok=false instead of reporting a false success. Screenshot, click, and input results warn when the target window or coordinate is partially or fully off-screen; use move_window/resize_window (GNOME Shell extension backend) to bring a window fully on-screen before retrying. scroll accepts the same window targeting and relative coordinates as click. get_app_state returns a compact readiness block by default; pass verbose=true for the full diagnostics dump. Electron apps expose no AT-SPI tree unless launched with --force-renderer-accessibility."
     */
    instructions = "Begin a Computer Use task with get_app_state using only the required app or window selectors. Ordinary screenshots accept max_bytes, max_width, and max_height only: the backend prefers PNG and compresses automatically. Use screenshot_compressed only when JPEG with an explicit numeric quality is required. Never convert image coordinates manually or pass them to a desktop-coordinate tool. Use click_screenshot, scroll_screenshot, and drag_screenshot with the screenshot_id from the exact image; the backend rejects expired captures and changed monitor or window geometry. Use click_desktop, scroll_desktop, and drag_desktop only for coordinates already known to be physical desktop pixels. Prefer accessibility element_index or semantic selectors when available. A targeted screenshot is strictly cropped and never falls back to the full desktop. Tool arguments must be strict JSON. Do not invoke xdotool or ydotool through a shell; all input must go through these Computer Use tools. Treat dispatched, landed, and verified as separate states and stop when a dispatched action cannot be verified. get_app_state returns compact diagnostics by default; use verbose=true only when diagnosing integration failures. Electron applications require --force-renderer-accessibility for a useful AT-SPI tree.",
)]
impl ServerHandler for ComputerUseLinux {}

pub async fn serve_mcp() -> Result<()> {
    ComputerUseLinux::default()
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct ListAppsOutput {
    apps: Vec<AppCandidate>,
    accessible_apps: Vec<AccessibleAppSummary>,
    accessibility_error: Option<String>,
    note: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct ListWindowsOutput {
    backend: String,
    windows: Vec<WindowInfo>,
    error: Option<String>,
    permissions_hint: Option<String>,
    note: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct FocusedWindowOutput {
    backend: String,
    focused_window: Option<WindowInfo>,
    error: Option<String>,
    permissions_hint: Option<String>,
    message: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct ActivateWindowParams {
    #[serde(default)]
    window_id: Option<u64>,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    tty: Option<String>,
    #[serde(default)]
    terminal_pid: Option<u32>,
    #[serde(default)]
    terminal_command: Option<String>,
    #[serde(default)]
    terminal_cwd: Option<String>,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    wm_class: Option<String>,
    #[serde(default)]
    title: Option<String>,
}

impl ActivateWindowParams {
    fn into_target(self) -> WindowTarget {
        WindowTarget {
            window_id: self.window_id,
            pid: self.pid,
            tty: self.tty,
            terminal_pid: self.terminal_pid,
            terminal_command: self.terminal_command,
            terminal_cwd: self.terminal_cwd,
            app_id: self.app_id,
            wm_class: self.wm_class,
            title: self.title,
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct ActivateWindowOutput {
    ok: bool,
    implemented: bool,
    backend: String,
    focus: Option<WindowFocusResult>,
    error: Option<String>,
    permissions_hint: Option<String>,
    // Echo of the request for debugging. `serde_json::Value` has no fixed JSON
    // schema, which strict MCP clients (Claude Code) reject in `outputSchema` —
    // and one invalid tool fails the whole tool list. Keep it in the runtime
    // response (serde) but omit it from the generated schema.
    #[schemars(skip)]
    received: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct MoveWindowParams {
    #[serde(flatten)]
    target: ActivateWindowParams,
    /// New frame-left in desktop coordinates.
    x: i32,
    /// New frame-top in desktop coordinates.
    y: i32,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct ResizeWindowParams {
    #[serde(flatten)]
    target: ActivateWindowParams,
    /// New frame width in desktop pixels.
    width: i32,
    /// New frame height in desktop pixels.
    height: i32,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct WindowGeometryOutput {
    ok: bool,
    implemented: bool,
    backend: String,
    /// Post-operation window info (compositor-final geometry).
    window: Option<WindowInfo>,
    message: String,
    permissions_hint: Option<String>,
    #[schemars(skip)]
    received: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct AppCandidate {
    name: String,
    pid: u32,
    command: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct GetAppStateParams {
    #[serde(default)]
    app_name_or_bundle_identifier: Option<String>,
    #[serde(default)]
    window_id: Option<u64>,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    tty: Option<String>,
    #[serde(default)]
    terminal_pid: Option<u32>,
    #[serde(default)]
    terminal_command: Option<String>,
    #[serde(default)]
    terminal_cwd: Option<String>,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    wm_class: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    max_nodes: Option<usize>,
    #[serde(default)]
    max_depth: Option<u32>,
    #[serde(default)]
    include_screenshot: Option<bool>,
    /// Maximum returned screenshot width in pixels (default 1920, hard-capped).
    #[serde(default)]
    max_width: Option<u32>,
    /// Maximum returned screenshot height in pixels (default 1920, hard-capped).
    #[serde(default)]
    max_height: Option<u32>,
    /// Maximum returned screenshot image bytes before base64 (default 2 MiB, hard-capped).
    #[serde(default)]
    max_bytes: Option<usize>,
    /// Include the full diagnostics report (large). Default false: only the
    /// compact readiness block is returned.
    #[serde(default)]
    verbose: Option<bool>,
}

impl GetAppStateParams {
    fn window_target(&self) -> WindowTarget {
        WindowTarget {
            window_id: self.window_id,
            pid: self.pid,
            tty: self.tty.clone(),
            terminal_pid: self.terminal_pid,
            terminal_command: self.terminal_command.clone(),
            terminal_cwd: self.terminal_cwd.clone(),
            app_id: self.app_id.clone(),
            wm_class: self.wm_class.clone(),
            title: self.title.clone(),
        }
    }

    fn screenshot_options(&self) -> ScreenshotPayloadOptions {
        ScreenshotPayloadOptions {
            max_width: self.max_width,
            max_height: self.max_height,
            max_bytes: self.max_bytes,
            encoding: ScreenshotEncodingPolicy::Adaptive,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct ScreenshotParams {
    #[serde(default)]
    window_id: Option<u64>,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    wm_class: Option<String>,
    #[serde(default)]
    title: Option<String>,
    /// Raise the targeted window before capture (default true). Ignored without
    /// a window target.
    #[serde(default)]
    raise_window: Option<bool>,
    /// Capture the whole desktop even when a window is targeted (default false).
    #[serde(default)]
    full_screen: Option<bool>,
    /// Maximum returned screenshot width in pixels (default 1920, hard-capped).
    #[serde(default)]
    max_width: Option<u32>,
    /// Maximum returned screenshot height in pixels (default 1920, hard-capped).
    #[serde(default)]
    max_height: Option<u32>,
    /// Maximum returned screenshot image bytes before base64 (default 2 MiB, hard-capped).
    #[serde(default)]
    max_bytes: Option<usize>,
}

impl ScreenshotParams {
    fn window_target(&self) -> Option<WindowTarget> {
        if self.window_id.is_none()
            && self.pid.is_none()
            && self.app_id.is_none()
            && self.wm_class.is_none()
            && self.title.is_none()
        {
            return None;
        }
        Some(WindowTarget {
            window_id: self.window_id,
            pid: self.pid,
            tty: None,
            terminal_pid: None,
            terminal_command: None,
            terminal_cwd: None,
            app_id: self.app_id.clone(),
            wm_class: self.wm_class.clone(),
            title: self.title.clone(),
        })
    }

    fn screenshot_options(&self, encoding: ScreenshotEncodingPolicy) -> ScreenshotPayloadOptions {
        ScreenshotPayloadOptions {
            max_width: self.max_width,
            max_height: self.max_height,
            max_bytes: self.max_bytes,
            encoding,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct ScreenshotCompressedParams {
    #[serde(flatten)]
    screenshot: ScreenshotParams,
    /// JPEG quality. The output is always JPEG; dimensions are reduced when
    /// necessary to satisfy max_bytes without changing this value.
    #[schemars(range(min = 1, max = 95))]
    quality: u8,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct LocateTextParams {
    screenshot_id: String,
    text: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct LocateControlParams {
    screenshot_id: String,
    role: String,
    text: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct LocatedVisualMatch {
    target_id: String,
    screenshot_id: String,
    bounds: ImageBounds,
    center_x: i32,
    center_y: i32,
    confidence: f64,
    source: String,
    recognized_text: String,
    role: Option<String>,
    role_inferred: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct LocateVisualOutput {
    ok: bool,
    screenshot_id: String,
    backend: Option<String>,
    matches: Vec<LocatedVisualMatch>,
    message: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct ClickTargetParams {
    target_id: String,
    #[serde(default)]
    button: Option<String>,
    #[serde(default)]
    click_count: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct ClickAndVerifyParams {
    target_id: String,
    #[serde(default)]
    button: Option<String>,
    #[serde(default)]
    click_count: Option<u32>,
    #[serde(default)]
    expect_text_present: Option<String>,
    #[serde(default)]
    expect_text_absent: Option<String>,
    #[serde(default)]
    expect_region_changed: Option<bool>,
    #[serde(default)]
    expect_focused_editable: Option<bool>,
    /// Total verification window. Clamped to 250-5000 ms.
    #[serde(default)]
    #[schemars(range(min = 250, max = 5000))]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct ClickVerificationOutput {
    ok: bool,
    stage: String,
    dispatched: bool,
    landed: Option<bool>,
    verified: bool,
    target_id: String,
    screenshot_id: Option<String>,
    pointer_arrived: Option<bool>,
    actual_pointer_x: Option<i32>,
    actual_pointer_y: Option<i32>,
    region_change_score: Option<f64>,
    region_changed: Option<bool>,
    expected_text_present: Option<bool>,
    expected_text_absent: Option<bool>,
    focused_editable: Option<bool>,
    evidence_screenshot_id: Option<String>,
    message: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct GetAppStateOutput {
    app_name_or_bundle_identifier: Option<String>,
    window_context: Option<WindowInfo>,
    window_error: Option<String>,
    window_permissions_hint: Option<String>,
    backend: String,
    screenshot: Option<ScreenshotCapture>,
    screenshot_error: Option<String>,
    accessibility_tree: Vec<AccessibilityNode>,
    accessibility_tree_raw_count: usize,
    accessibility_error: Option<String>,
    /// Compact readiness summary (always present).
    readiness: crate::diagnostics::ReadinessReport,
    /// Full diagnostics; populated only when verbose=true.
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnostics: Option<DoctorReport>,
    message: String,
}

fn get_app_state_call_result(
    mut output: GetAppStateOutput,
    screenshot: Option<ScreenshotCapture>,
) -> Result<CallToolResult, ErrorData> {
    output.screenshot = screenshot;
    let image = output.screenshot.as_ref().map(|capture| {
        Content::image(
            data_url_payload(&capture.data_url),
            capture.mime_type.clone(),
        )
    });
    let structured_content = serde_json::to_value(&output).map_err(|error| {
        ErrorData::internal_error(
            format!("failed to serialize get_app_state output: {error}"),
            None,
        )
    })?;
    let mut result = CallToolResult::structured(structured_content);
    if let Some(image) = image {
        result.content.insert(0, image);
    }
    Ok(result)
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct SemanticClickParams {
    #[serde(default)]
    element_index: Option<u32>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    states: Vec<String>,
    #[serde(default)]
    button: Option<String>,
    #[serde(default)]
    click_count: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct DesktopClickParams {
    x: i32,
    y: i32,
    #[serde(default)]
    button: Option<String>,
    #[serde(default)]
    click_count: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct ScreenshotClickParams {
    screenshot_id: String,
    x: i32,
    y: i32,
    #[serde(default)]
    button: Option<String>,
    #[serde(default)]
    click_count: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct ClickParams {
    #[serde(default)]
    element_index: Option<u32>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    states: Vec<String>,
    #[serde(default)]
    x: Option<i32>,
    #[serde(default)]
    y: Option<i32>,
    #[serde(default)]
    button: Option<String>,
    #[serde(default)]
    click_count: Option<u32>,
    // Optional window target: when set, the window is raised/focused before the
    // click so a coordinate click reliably lands on the intended app rather than
    // whatever window happens to be stacked on top at that pixel.
    #[serde(default)]
    window_id: Option<u64>,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    wm_class: Option<String>,
    #[serde(default)]
    window_title: Option<String>,
    /// Interpret explicit physical desktop `x`/`y` as relative to the targeted
    /// window's top-left corner. Internal compatibility path only.
    #[serde(default)]
    relative: Option<bool>,
}

impl From<SemanticClickParams> for ClickParams {
    fn from(params: SemanticClickParams) -> Self {
        Self {
            element_index: params.element_index,
            role: params.role,
            name: params.name,
            text: params.text,
            states: params.states,
            button: params.button,
            click_count: params.click_count,
            ..Default::default()
        }
    }
}

impl From<DesktopClickParams> for ClickParams {
    fn from(params: DesktopClickParams) -> Self {
        Self {
            x: Some(params.x),
            y: Some(params.y),
            button: params.button,
            click_count: params.click_count,
            ..Default::default()
        }
    }
}

impl ClickParams {
    /// A window target if any window-identifying field was supplied.
    fn window_target(&self) -> Option<WindowTarget> {
        if self.window_id.is_none()
            && self.pid.is_none()
            && self.app_id.is_none()
            && self.wm_class.is_none()
            && self.window_title.is_none()
        {
            return None;
        }
        Some(WindowTarget {
            window_id: self.window_id,
            pid: self.pid,
            tty: None,
            terminal_pid: None,
            terminal_command: None,
            terminal_cwd: None,
            app_id: self.app_id.clone(),
            wm_class: self.wm_class.clone(),
            title: self.window_title.clone(),
        })
    }

    fn selector(&self) -> ElementSelector<'_> {
        ElementSelector {
            role: self.role.as_deref(),
            name: self.name.as_deref(),
            text: self.text.as_deref(),
            states: &self.states,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct ActionParams {
    #[serde(default)]
    element_index: Option<u32>,
    #[serde(default)]
    element_identifier: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    states: Vec<String>,
    #[serde(default)]
    action: Option<String>,
}

impl ActionParams {
    fn selector(&self) -> ElementSelector<'_> {
        ElementSelector {
            role: self.role.as_deref(),
            name: self.name.as_deref(),
            text: self.text.as_deref(),
            states: &self.states,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct SetValueParams {
    #[serde(default)]
    element_index: Option<u32>,
    #[serde(default)]
    element_identifier: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    states: Vec<String>,
    value: String,
}

impl SetValueParams {
    fn selector(&self) -> ElementSelector<'_> {
        ElementSelector {
            role: self.role.as_deref(),
            name: self.name.as_deref(),
            text: self.text.as_deref(),
            states: &self.states,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct SemanticScrollParams {
    #[serde(default)]
    element_index: Option<u32>,
    direction: String,
    #[serde(default)]
    pages: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct DesktopScrollParams {
    x: i32,
    y: i32,
    direction: String,
    #[serde(default)]
    pages: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct ScreenshotScrollParams {
    screenshot_id: String,
    x: i32,
    y: i32,
    direction: String,
    #[serde(default)]
    pages: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ScrollParams {
    #[serde(default)]
    element_index: Option<u32>,
    #[serde(default)]
    x: Option<i32>,
    #[serde(default)]
    y: Option<i32>,
    direction: String,
    #[serde(default)]
    pages: Option<f64>,
    // Optional window target (parity with click): the window is raised/focused
    // before scrolling so the wheel events land on the intended app.
    #[serde(default)]
    window_id: Option<u64>,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    wm_class: Option<String>,
    #[serde(default)]
    window_title: Option<String>,
    /// Interpret explicit physical desktop `x`/`y` as relative to the targeted
    /// window's top-left corner. Internal compatibility path only.
    #[serde(default)]
    relative: Option<bool>,
}

impl From<SemanticScrollParams> for ScrollParams {
    fn from(params: SemanticScrollParams) -> Self {
        Self {
            element_index: params.element_index,
            x: None,
            y: None,
            direction: params.direction,
            pages: params.pages,
            window_id: None,
            pid: None,
            app_id: None,
            wm_class: None,
            window_title: None,
            relative: None,
        }
    }
}

impl From<DesktopScrollParams> for ScrollParams {
    fn from(params: DesktopScrollParams) -> Self {
        Self {
            element_index: None,
            x: Some(params.x),
            y: Some(params.y),
            direction: params.direction,
            pages: params.pages,
            window_id: None,
            pid: None,
            app_id: None,
            wm_class: None,
            window_title: None,
            relative: None,
        }
    }
}

impl ScrollParams {
    /// A window target if any window-identifying field was supplied.
    fn window_target(&self) -> Option<WindowTarget> {
        if self.window_id.is_none()
            && self.pid.is_none()
            && self.app_id.is_none()
            && self.wm_class.is_none()
            && self.window_title.is_none()
        {
            return None;
        }
        Some(WindowTarget {
            window_id: self.window_id,
            pid: self.pid,
            tty: None,
            terminal_pid: None,
            terminal_command: None,
            terminal_cwd: None,
            app_id: self.app_id.clone(),
            wm_class: self.wm_class.clone(),
            title: self.window_title.clone(),
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct DesktopDragParams {
    start_x: i32,
    start_y: i32,
    end_x: i32,
    end_y: i32,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct ScreenshotDragParams {
    screenshot_id: String,
    start_x: i32,
    start_y: i32,
    end_x: i32,
    end_y: i32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct DragParams {
    start_x: i32,
    start_y: i32,
    end_x: i32,
    end_y: i32,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct PressKeyParams {
    key: String,
    #[serde(default)]
    window_id: Option<u64>,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    tty: Option<String>,
    #[serde(default)]
    terminal_pid: Option<u32>,
    #[serde(default)]
    terminal_command: Option<String>,
    #[serde(default)]
    terminal_cwd: Option<String>,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    wm_class: Option<String>,
    #[serde(default)]
    title: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct TypeTextParams {
    text: String,
    #[serde(default)]
    window_id: Option<u64>,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    tty: Option<String>,
    #[serde(default)]
    terminal_pid: Option<u32>,
    #[serde(default)]
    terminal_command: Option<String>,
    #[serde(default)]
    terminal_cwd: Option<String>,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    wm_class: Option<String>,
    #[serde(default)]
    title: Option<String>,
}

impl PressKeyParams {
    fn window_target(&self) -> WindowTarget {
        WindowTarget {
            window_id: self.window_id,
            pid: self.pid,
            tty: self.tty.clone(),
            terminal_pid: self.terminal_pid,
            terminal_command: self.terminal_command.clone(),
            terminal_cwd: self.terminal_cwd.clone(),
            app_id: self.app_id.clone(),
            wm_class: self.wm_class.clone(),
            title: self.title.clone(),
        }
    }
}

impl TypeTextParams {
    fn window_target(&self) -> WindowTarget {
        WindowTarget {
            window_id: self.window_id,
            pid: self.pid,
            tty: self.tty.clone(),
            terminal_pid: self.terminal_pid,
            terminal_command: self.terminal_command.clone(),
            terminal_cwd: self.terminal_cwd.clone(),
            app_id: self.app_id.clone(),
            wm_class: self.wm_class.clone(),
            title: self.title.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct ActionOutput {
    /// Whether the selected input backend accepted and completed dispatching
    /// the requested events. This is deliberately separate from target-level
    /// landing and post-action verification.
    dispatched: bool,
    /// Target-level landing evidence. `None` means the desktop could not
    /// establish whether the dispatched input reached a suitable target.
    landed: Option<bool>,
    /// Whether `landed` is backed by a conclusive post-dispatch probe.
    verified: bool,
    ok: bool,
    implemented: bool,
    action: String,
    message: String,
    // See ActivateWindowOutput: kept in the response, omitted from the schema
    // because `serde_json::Value` produces a non-object schema strict MCP
    // clients reject.
    #[schemars(skip)]
    received: Option<serde_json::Value>,
}

impl ComputerUseLinux {
    fn is_x11_session(&self) -> bool {
        crate::diagnostics::hydrate_session_bus_env();
        env::var("XDG_SESSION_TYPE")
            .ok()
            .is_some_and(|value| value.eq_ignore_ascii_case("x11"))
            || (env::var_os("DISPLAY").is_some() && env::var_os("WAYLAND_DISPLAY").is_none())
    }

    fn is_wayland_session(&self) -> bool {
        crate::diagnostics::hydrate_session_bus_env();
        env::var("XDG_SESSION_TYPE")
            .ok()
            .is_some_and(|value| value.eq_ignore_ascii_case("wayland"))
    }

    fn should_prefer_xdotool_input_backend(&self) -> bool {
        self.is_x11_session()
            && !env_flag_enabled_any(&[
                "COMPUTER_USE_LINUX_FORCE_YDOTOOL_POINTER",
                "CODEX_COMPUTER_USE_FORCE_YDOTOOL_POINTER",
                "COMPUTER_USE_LINUX_FORCE_YDOTOOL_KEYBOARD",
                "CODEX_COMPUTER_USE_FORCE_YDOTOOL_KEYBOARD",
            ])
    }

    fn should_prefer_x11_clipboard_text_backend(&self) -> bool {
        let forced_ydotool_keyboard = env_flag_enabled_any(&[
            "COMPUTER_USE_LINUX_FORCE_YDOTOOL_KEYBOARD",
            "CODEX_COMPUTER_USE_FORCE_YDOTOOL_KEYBOARD",
        ]);
        prefer_x11_clipboard_text_backend(self.is_x11_session(), forced_ydotool_keyboard)
    }

    // The Wayland remote-desktop portal is now a *fallback* for input: when a
    // working `ydotoold` socket is present we prefer ydotool, because it injects
    // input without a permission prompt. GNOME refuses to persist remote-desktop
    // grants (`org.freedesktop.portal.Error: Remote desktop sessions cannot
    // persist`), so the portal would otherwise re-prompt on every new session.
    // `COMPUTER_USE_LINUX_FORCE_YDOTOOL_*=1` always uses ydotool;
    // `COMPUTER_USE_LINUX_FORCE_PORTAL_*=1` always uses the portal. The
    // `CODEX_COMPUTER_USE_*` names are accepted for the embedded Codex app
    // bundle so downstream can share this source without local string patches.
    fn should_prefer_portal_pointer_backend(&self) -> bool {
        if env_flag_enabled_any(&[
            "COMPUTER_USE_LINUX_FORCE_YDOTOOL_POINTER",
            "CODEX_COMPUTER_USE_FORCE_YDOTOOL_POINTER",
        ]) {
            return false;
        }
        if env_flag_enabled_any(&[
            "COMPUTER_USE_LINUX_FORCE_PORTAL_POINTER",
            "CODEX_COMPUTER_USE_FORCE_PORTAL_POINTER",
        ]) {
            return self.is_wayland_session();
        }
        self.is_wayland_session() && ydotool_socket().is_none()
    }

    fn should_prefer_portal_keyboard_backend(&self) -> bool {
        if env_flag_enabled_any(&[
            "COMPUTER_USE_LINUX_FORCE_YDOTOOL_KEYBOARD",
            "CODEX_COMPUTER_USE_FORCE_YDOTOOL_KEYBOARD",
        ]) {
            return false;
        }
        if env_flag_enabled_any(&[
            "COMPUTER_USE_LINUX_FORCE_PORTAL_KEYBOARD",
            "CODEX_COMPUTER_USE_FORCE_PORTAL_KEYBOARD",
        ]) {
            return self.is_wayland_session() && !self.is_kde_wayland_session();
        }
        self.is_wayland_session() && !self.is_kde_wayland_session() && ydotool_socket().is_none()
    }

    fn should_prefer_kde_clipboard_text_backend(&self) -> bool {
        !env_flag_enabled_any(&[
            "COMPUTER_USE_LINUX_FORCE_YDOTOOL_KEYBOARD",
            "CODEX_COMPUTER_USE_FORCE_YDOTOOL_KEYBOARD",
        ]) && self.is_kde_wayland_session()
    }

    fn is_kde_wayland_session(&self) -> bool {
        self.is_wayland_session()
            && (env_contains("XDG_CURRENT_DESKTOP", "kde")
                || env_contains("DESKTOP_SESSION", "plasma"))
    }

    fn cached_portal_pointer_session(&self) -> Option<PortalPointerSession> {
        self.portal_pointer_session
            .lock()
            .ok()
            .and_then(|cached| cached.clone())
    }

    fn clear_portal_pointer_session(&self) {
        if let Ok(mut cached) = self.portal_pointer_session.lock() {
            *cached = None;
        }
    }

    fn cached_portal_keyboard_session(&self) -> Option<PortalKeyboardSession> {
        self.portal_keyboard_session
            .lock()
            .ok()
            .and_then(|cached| cached.clone())
    }

    fn clear_portal_keyboard_session(&self) {
        if let Ok(mut cached) = self.portal_keyboard_session.lock() {
            *cached = None;
        }
    }

    async fn ensure_portal_pointer_session(&self) -> Result<Option<PortalPointerSession>> {
        if !self.should_prefer_portal_pointer_backend() {
            return Ok(None);
        }
        if let Some(session) = self.cached_portal_pointer_session() {
            return Ok(Some(session));
        }

        let session = start_portal_pointer_session().await?;
        if let Ok(mut cached) = self.portal_pointer_session.lock() {
            *cached = Some(session.clone());
        }
        Ok(Some(session))
    }

    async fn ensure_portal_keyboard_session(&self) -> Result<Option<PortalKeyboardSession>> {
        if env_flag_enabled_any(&[
            "COMPUTER_USE_LINUX_FORCE_YDOTOOL_KEYBOARD",
            "CODEX_COMPUTER_USE_FORCE_YDOTOOL_KEYBOARD",
        ]) || !self.is_wayland_session()
        {
            return Ok(None);
        }
        if let Some(session) = self.cached_portal_keyboard_session() {
            return Ok(Some(session));
        }

        let _guard = self.portal_keyboard_init_lock.lock().await;
        if let Some(session) = self.cached_portal_keyboard_session() {
            return Ok(Some(session));
        }

        let session = start_portal_keyboard_session().await?;
        if let Ok(mut cached) = self.portal_keyboard_session.lock() {
            *cached = Some(session.clone());
        }
        Ok(Some(session))
    }

    async fn resolve_window_context(
        &self,
        params: &GetAppStateParams,
    ) -> (Option<WindowInfo>, Option<String>, Option<String>) {
        let target = params.window_target();
        if !target.has_target() {
            return (None, None, None);
        }

        match list_windows().await {
            Ok(windows) => match resolve_window_target(&windows, &target) {
                Ok(window) => (Some(window.clone()), None, None),
                Err(error) => (None, Some(format!("{error:#}")), None),
            },
            Err(error) => {
                let error = format!("{error:#}");
                let hint = window_permission_hint(&error);
                (None, Some(error), hint)
            }
        }
    }

    async fn resolve_accessibility_app_filter(
        &self,
        params: &GetAppStateParams,
        window_context: Option<&WindowInfo>,
    ) -> Option<String> {
        if let Some(explicit) = trimmed_nonempty(params.app_name_or_bundle_identifier.as_deref()) {
            return Some(explicit.to_string());
        }

        let target_pid = window_context.and_then(|window| window.pid).or(params.pid);
        let candidates = accessibility_filter_candidates(window_context);

        if let Some(target_pid) = target_pid {
            if let Ok(apps) = list_accessible_apps(200).await {
                if let Some(object_ref) =
                    select_accessibility_object_ref(&apps, target_pid, &candidates)
                {
                    return Some(object_ref);
                }
            }
        }

        candidates.into_iter().next()
    }

    async fn focus_target_for_input(
        &self,
        target: &WindowTarget,
    ) -> std::result::Result<Option<WindowFocusResult>, String> {
        if !target.has_target() {
            return Ok(None);
        }

        let focus = focus_window_target(target).await.map_err(|error| {
            let error = format!("{error:#}");
            if let Some(hint) = window_permission_hint(&error) {
                format!("Did not send input because the target window could not be focused: {error}. {hint}")
            } else {
                format!("Did not send input because the target window could not be focused: {error}")
            }
        })?;

        if focus_satisfies_target(&focus, target) {
            Ok(Some(focus))
        } else {
            let required = if target.requires_exact_focus() {
                "exact target-window focus"
            } else {
                "app-level focus"
            };
            Err(format!(
                "Did not send input because {required} verification failed after activating the target window. Focus result: requested window_id {}, focused window_id {:?}.",
                focus.requested_window.window_id,
                focus.focused_window.as_ref().map(|window| window.window_id)
            ))
        }
    }

    fn cache_desktop_size(&self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        if let Ok(mut guard) = self.desktop_size.lock() {
            *guard = Some((width, height));
        }
    }

    fn cache_screenshot_artifact(
        &self,
        capture: &ScreenshotCapture,
        monitor_layout_fingerprint: String,
        window: Option<WindowSnapshot>,
    ) {
        let Ok(transform) = CaptureTransform::from_capture(
            capture,
            monitor_layout_fingerprint,
            window,
            Instant::now(),
        ) else {
            return;
        };
        if let Ok(mut guard) = self.screenshot_artifacts.lock() {
            guard.insert(ScreenshotArtifact {
                capture: capture.clone(),
                transform,
            });
        }
    }

    fn screenshot_artifact(
        &self,
        screenshot_id: &str,
    ) -> std::result::Result<ScreenshotArtifact, String> {
        self.screenshot_artifacts
            .lock()
            .map_err(|_| "Screenshot artifact cache is unavailable.".to_string())?
            .get(screenshot_id, Instant::now())
    }

    fn visual_target(&self, target_id: &str) -> std::result::Result<VisualTarget, String> {
        self.visual_targets
            .lock()
            .map_err(|_| "Visual target cache is unavailable.".to_string())?
            .get(target_id)
            .ok_or_else(|| {
                format!("Unknown target_id {target_id:?}. Run locate_text or locate_control again.")
            })
    }

    async fn capture_evidence_for_artifact(
        &self,
        artifact: &ScreenshotArtifact,
    ) -> std::result::Result<ScreenshotCapture, String> {
        let raw = capture_screenshot_raw()
            .await
            .map_err(|error| format!("verification screenshot failed: {error:#}"))?;
        self.cache_desktop_size(raw.width, raw.height);
        let layout_fingerprint = self.monitor_layout_fingerprint(raw.width, raw.height).await;
        let rect = artifact.transform.desktop_rect;
        let bounds = crate::windowing::WindowBounds {
            x: Some(rect.x),
            y: Some(rect.y),
            width: rect.width,
            height: rect.height,
        };
        let crop = window_crop_rect_for_capture(&bounds, raw.width, raw.height)?;
        let (bytes, width, height) =
            crop_image_to_png(&raw.bytes, crop.x, crop.y, crop.width, crop.height)
                .map_err(|error| format!("verification crop failed: {error:#}"))?;
        let mut capture = prepare_screenshot_payload(
            RawScreenshotCapture {
                bytes,
                source: raw.source,
                width,
                height,
            },
            ScreenshotPayloadOptions {
                max_width: Some(artifact.capture.width),
                max_height: Some(artifact.capture.height),
                max_bytes: Some(4 * 1024 * 1024),
                encoding: ScreenshotEncodingPolicy::Adaptive,
            },
        )
        .map_err(|error| format!("verification resize failed: {error:#}"))?;
        capture.coordinate_origin_x = crop.x;
        capture.coordinate_origin_y = crop.y;
        capture.cropped_to_window = artifact.transform.window.is_some();
        capture.target_window_id = artifact.transform.window.map(|window| window.window_id);
        self.cache_screenshot_artifact(&capture, layout_fingerprint, artifact.transform.window);
        Ok(capture)
    }

    async fn resolve_screenshot_points(
        &self,
        screenshot_id: &str,
        points: &[(i32, i32)],
    ) -> std::result::Result<Vec<(i32, i32)>, String> {
        let artifact = self.screenshot_artifact(screenshot_id)?;
        if let Some(window) = artifact.transform.window {
            let target = WindowTarget {
                window_id: Some(window.window_id),
                ..Default::default()
            };
            self.focus_target_for_input(&target).await?;
            tokio::time::sleep(Duration::from_millis(120)).await;
        }

        let (desktop_width, desktop_height) = self
            .desktop_size
            .lock()
            .map_err(|_| "Desktop geometry cache is unavailable.".to_string())?
            .ok_or_else(|| {
                "Desktop capture geometry is unavailable; capture a new screenshot.".to_string()
            })?;
        let current_layout = self
            .monitor_layout_fingerprint(desktop_width, desktop_height)
            .await;
        let current_window = match artifact.transform.window {
            Some(expected) => {
                let windows = list_windows().await.map_err(|error| {
                    format!("STALE_SCREENSHOT: cannot verify window: {error:#}")
                })?;
                windows
                    .iter()
                    .find(|window| window.window_id == expected.window_id)
                    .and_then(window_snapshot)
            }
            None => None,
        };
        artifact
            .transform
            .validate_context(&current_layout, current_window, Instant::now())?;
        points
            .iter()
            .map(|(x, y)| artifact.transform.map_pixel(*x, *y))
            .collect()
    }

    async fn monitor_layout_fingerprint(&self, capture_width: u32, capture_height: u32) -> String {
        match crate::windowing::backends::gnome::extension_monitor_layout().await {
            Ok(mut monitors) if !monitors.is_empty() => {
                monitors.sort_by_key(|monitor| monitor.index);
                let entries = monitors
                    .iter()
                    .map(|monitor| {
                        format!(
                            "{}:{},{},{}x{}@{:.4}:{}",
                            monitor.index,
                            monitor.x,
                            monitor.y,
                            monitor.width,
                            monitor.height,
                            monitor.scale,
                            monitor.primary
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("|");
                format!("gnome:{capture_width}x{capture_height}:{entries}")
            }
            _ => {
                if env::var_os("DISPLAY").is_some() {
                    if let Ok(Ok(output)) = timeout(
                        Duration::from_secs(2),
                        TokioCommand::new("xrandr")
                            .arg("--listmonitors")
                            .stdin(Stdio::null())
                            .output(),
                    )
                    .await
                    {
                        if output.status.success() {
                            let layout = String::from_utf8_lossy(&output.stdout)
                                .lines()
                                .map(str::trim)
                                .filter(|line| !line.is_empty())
                                .collect::<Vec<_>>()
                                .join("|");
                            if !layout.is_empty() {
                                return format!("xrandr:{capture_width}x{capture_height}:{layout}");
                            }
                        }
                    }
                }
                format!("capture:{capture_width}x{capture_height}")
            }
        }
    }

    /// COORDINATE SPACES: window bounds (list_windows / extension frame rects)
    /// and the extension monitor layout are in LOGICAL pixels, while click/
    /// scroll coordinates and screenshot captures are in PHYSICAL capture
    /// pixels. On fractionally-scaled displays the two differ, so each check
    /// below only ever compares values from the same space.
    ///
    /// Logical monitor rectangles from the GNOME Shell extension, for checks
    /// against logical window bounds. None when the extension is unavailable.
    async fn logical_monitor_rects(&self) -> Option<Vec<(i32, i32, i32, i32)>> {
        let monitors = crate::windowing::backends::gnome::extension_monitor_layout()
            .await
            .ok()?;
        (!monitors.is_empty()).then(|| {
            monitors
                .iter()
                .map(|m| (m.x, m.y, m.width, m.height))
                .collect()
        })
    }

    /// Physical capture-space desktop rectangle (union of monitors as captured
    /// by the screenshot pipeline), for checks against click coordinates.
    /// Best-effort; None disables the check.
    async fn capture_space_rect(&self) -> Option<(i32, i32, i32, i32)> {
        let cached = self.desktop_size.lock().ok().and_then(|guard| *guard);
        if let Some((w, h)) = cached {
            return Some((0, 0, w as i32, h as i32));
        }
        // One-time prime: a full-frame capture reveals the desktop size when
        // no prior capture is available.
        let raw = capture_screenshot_raw().await.ok()?;
        self.cache_desktop_size(raw.width, raw.height);
        (raw.width > 0 && raw.height > 0).then_some((0, 0, raw.width as i32, raw.height as i32))
    }

    /// Warn when a targeted window pokes outside every monitor: clicks and
    /// screenshots silently truncate to visible pixels there, which reads as
    /// "success" while landing nowhere.
    async fn off_screen_note_for_bounds(
        &self,
        bounds: &crate::windowing::WindowBounds,
    ) -> Option<String> {
        let (x, y) = bounds.x.zip(bounds.y)?;
        if bounds.width == 0 || bounds.height == 0 {
            return None;
        }
        // Window bounds are logical pixels: prefer the extension's logical
        // monitor layout (same space). The physical capture rect is a safe
        // fallback — on scaled displays it is at least as large as the logical
        // union, so it can only under-warn, never false-positive.
        let rects = match self.logical_monitor_rects().await {
            Some(rects) => rects,
            None => vec![self.capture_space_rect().await?],
        };
        let (w, h) = (bounds.width as i64, bounds.height as i64);
        let window_area = w * h;
        let mut visible_area = 0_i64;
        for (mx, my, mw, mh) in &rects {
            let ix = (x as i64).max(*mx as i64);
            let iy = (y as i64).max(*my as i64);
            let ix2 = (x as i64 + w).min(*mx as i64 + *mw as i64);
            let iy2 = (y as i64 + h).min(*my as i64 + *mh as i64);
            if ix2 > ix && iy2 > iy {
                // Overlapping monitors are rare; treating them as additive keeps
                // this a cheap best-effort heuristic.
                visible_area += (ix2 - ix) * (iy2 - iy);
            }
        }
        let visible_pct = (visible_area.min(window_area) * 100) / window_area.max(1);
        if visible_pct >= 100 {
            return None;
        }
        Some(format!(
            "WARNING: the target window (bounds {x},{y} {w}x{h}) is only ~{visible_pct}% on-screen; off-screen regions are missing from screenshots and unreachable by coordinate input. Use move_window/resize_window to bring it fully on-screen."
        ))
    }

    /// Warn when a click/scroll coordinate is outside the captured desktop.
    /// Click coordinates are physical capture-space pixels, so compare ONLY
    /// against the capture rect — the extension's logical layout is a
    /// different space on scaled displays and would false-positive.
    async fn off_screen_note_for_point(&self, x: i32, y: i32) -> Option<String> {
        let (mx, my, mw, mh) = self.capture_space_rect().await?;
        let visible = x >= mx && y >= my && x < mx.saturating_add(mw) && y < my.saturating_add(mh);
        if visible {
            return None;
        }
        Some(format!(
            "WARNING: coordinate {x},{y} is outside the captured desktop ({mw}x{mh}); the input landed on no visible pixel."
        ))
    }

    /// Post-input feedback: which AT-SPI element holds keyboard focus in the
    /// target app, and whether it is editable. Guards against the blind-typing
    /// trap where verified *window* focus still sends keystrokes nowhere.
    async fn focused_element_feedback(
        &self,
        focus: Option<&WindowFocusResult>,
        expects_editable: bool,
    ) -> ActionFeedback {
        let Some(focus) = focus else {
            return ActionFeedback::default();
        };
        let pid = focus
            .focused_window
            .as_ref()
            .and_then(|window| window.pid)
            .or(focus.requested_window.pid);
        match timeout(Duration::from_millis(1500), focused_element_summary(pid)).await {
            Ok(Ok(Some(element))) => focused_element_assessment(&element, expects_editable),
            Ok(Ok(None)) => ActionFeedback::unverified(
                "WARNING: AT-SPI reports no focused element in the target app — the input may have landed nowhere. If this is an Electron app, launch it with --force-renderer-accessibility to expose its UI tree."
                    .to_string(),
            ),
            Ok(Err(error)) => ActionFeedback::unverified(format!(
                "Focused-element feedback unavailable ({}).",
                first_line(&format!("{error:#}"))
            )),
            Err(_) => ActionFeedback::unverified(
                "Focused-element feedback unavailable (AT-SPI probe timed out).",
            ),
        }
    }

    /// Shared move/resize plumbing: resolve the window target, run the GNOME
    /// Shell extension operation, then re-query bounds to report the result.
    async fn window_geometry_op<F, Fut>(
        &self,
        received: Option<serde_json::Value>,
        target: &WindowTarget,
        op: F,
    ) -> Json<WindowGeometryOutput>
    where
        F: FnOnce(u64) -> Fut,
        Fut: Future<Output = Result<String>>,
    {
        let windows = match list_windows().await {
            Ok(windows) => windows,
            Err(error) => {
                let error = format!("{error:#}");
                return Json(WindowGeometryOutput {
                    ok: false,
                    implemented: true,
                    backend: crate::windowing::GNOME_SHELL_EXTENSION_BACKEND.to_string(),
                    window: None,
                    message: format!("Window listing failed: {error}"),
                    permissions_hint: window_permission_hint(&error),
                    received,
                });
            }
        };
        let window_id = match resolve_window_target(&windows, target) {
            Ok(window) => window.window_id,
            Err(error) => {
                return Json(WindowGeometryOutput {
                    ok: false,
                    implemented: true,
                    backend: crate::windowing::GNOME_SHELL_EXTENSION_BACKEND.to_string(),
                    window: None,
                    message: format!("{error:#}"),
                    permissions_hint: None,
                    received,
                });
            }
        };
        match op(window_id).await {
            Ok(message) => {
                // Re-query so the caller sees the compositor-final geometry
                // (tiling constraints, minimum sizes, etc. may adjust it).
                let window = list_windows().await.ok().and_then(|windows| {
                    windows
                        .into_iter()
                        .find(|window| window.window_id == window_id)
                });
                let mut message = message;
                if let Some(bounds) = window.as_ref().and_then(|window| window.bounds.as_ref()) {
                    if let Some(note) = self.off_screen_note_for_bounds(bounds).await {
                        message = format!("{message} {note}");
                    }
                }
                Json(WindowGeometryOutput {
                    ok: true,
                    implemented: true,
                    backend: crate::windowing::GNOME_SHELL_EXTENSION_BACKEND.to_string(),
                    window,
                    message,
                    permissions_hint: None,
                    received,
                })
            }
            Err(error) => {
                let error = format!("{error:#}");
                Json(WindowGeometryOutput {
                    ok: false,
                    implemented: true,
                    backend: crate::windowing::GNOME_SHELL_EXTENSION_BACKEND.to_string(),
                    window: None,
                    permissions_hint: window_permission_hint(&error),
                    message: error,
                    received,
                })
            }
        }
    }

    /// Notes appended after targeted keyboard input: off-screen window warning
    /// plus focused-element feedback.
    async fn input_landing_feedback(
        &self,
        focus: Option<&WindowFocusResult>,
        expects_editable: bool,
    ) -> ActionFeedback {
        let mut feedback = ActionFeedback::default();
        if let Some(focus) = focus {
            let bounds = focus
                .focused_window
                .as_ref()
                .and_then(|window| window.bounds.as_ref())
                .or(focus.requested_window.bounds.as_ref());
            if let Some(bounds) = bounds {
                if let Some(note) = self.off_screen_note_for_bounds(bounds).await {
                    feedback.notes.push(note);
                }
            }
        }
        feedback.merge(self.focused_element_feedback(focus, expects_editable).await);
        feedback
    }

    fn cache_nodes(&self, nodes: &[AccessibilityNode]) {
        if let Ok(mut cached) = self.last_nodes.lock() {
            cached.clear();
            cached.extend_from_slice(nodes);
        }
        if let Ok(mut mapping) = self.last_atspi_coordinates.lock() {
            *mapping = None;
        }
    }

    fn cache_nodes_for_window(&self, nodes: &[AccessibilityNode], window: Option<&WindowInfo>) {
        self.cache_nodes(nodes);
        let coordinate_map = window.and_then(|window| atspi_coordinate_map(nodes, window));
        if let Ok(mut mapping) = self.last_atspi_coordinates.lock() {
            *mapping = coordinate_map;
        }
    }

    fn clear_cached_nodes(&self) {
        if let Ok(mut cached) = self.last_nodes.lock() {
            cached.clear();
        }
        if let Ok(mut mapping) = self.last_atspi_coordinates.lock() {
            *mapping = None;
        }
    }

    fn resolve_optional_target_point(
        &self,
        x: Option<i32>,
        y: Option<i32>,
        element_index: Option<u32>,
    ) -> std::result::Result<Option<(i32, i32)>, String> {
        match (x.zip(y), element_index) {
            (Some(point), _) => Ok(Some(point)),
            (None, Some(index)) => self
                .center_for_cached_node(index)
                .map(Some)
                .ok_or_else(|| {
                    format!(
                        "No clickable bounds cached for element_index {index}. Call get_app_state first and choose a node with positive width and height."
                    )
                }),
            (None, None) => Ok(None),
        }
    }

    fn resolve_click_target(
        &self,
        params: &ClickParams,
    ) -> std::result::Result<ClickTarget, String> {
        if let Some((x, y)) = params.x.zip(params.y) {
            return Ok(ClickTarget::Coordinates(x, y));
        }

        let selector = params.selector();
        let node = self.resolve_cached_node(
            params.element_index,
            &selector,
            ElementResolvePurpose::Click,
        )?;

        let fallback_coordinates = node
            .bounds
            .as_ref()
            .and_then(bounds_center)
            .map(|point| self.atspi_point_to_desktop(point));
        if is_plain_left_click(params.button.as_deref(), params.click_count) {
            if let Some(action) = primary_action(node.actions.as_slice()) {
                return Ok(ClickTarget::PrimaryAction {
                    object_ref: node.object_ref.clone(),
                    action_name: Some(action.name.clone()),
                    action_index: action.index,
                    fallback_coordinates,
                });
            }
        }

        if let Some((x, y)) = fallback_coordinates {
            return Ok(ClickTarget::Coordinates(x, y));
        }

        if !is_plain_left_click(params.button.as_deref(), params.click_count) {
            return Err(format!(
                "No clickable bounds cached for element_index {}. Call get_app_state first and choose a node with positive width and height.",
                node.index
            ));
        }

        Err(format!(
            "No clickable bounds cached for element_index {}, and the element exposes no primary AT-SPI action.",
            node.index
        ))
    }

    fn center_for_cached_node(&self, element_index: u32) -> Option<(i32, i32)> {
        let cached = self.last_nodes.lock().ok()?;
        let node = cached.iter().find(|node| node.index == element_index)?;
        bounds_center(node.bounds.as_ref()?).map(|point| self.atspi_point_to_desktop(point))
    }

    fn atspi_point_to_desktop(&self, point: (i32, i32)) -> (i32, i32) {
        let mapping = self
            .last_atspi_coordinates
            .lock()
            .ok()
            .and_then(|mapping| *mapping);
        mapping
            .and_then(|mapping| map_atspi_point(mapping, point))
            .unwrap_or(point)
    }

    fn resolve_object_ref(
        &self,
        element_index: Option<u32>,
        element_identifier: Option<&str>,
        selector: &ElementSelector<'_>,
        purpose: ElementResolvePurpose,
    ) -> std::result::Result<String, String> {
        if let Some(element_identifier) = element_identifier
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Ok(element_identifier.to_string());
        }

        self.resolve_cached_node(element_index, selector, purpose)
            .map(|node| node.object_ref)
    }

    fn resolve_cached_node(
        &self,
        element_index: Option<u32>,
        selector: &ElementSelector<'_>,
        purpose: ElementResolvePurpose,
    ) -> std::result::Result<AccessibilityNode, String> {
        let cached = self.last_nodes.lock().map_err(|_| {
            "Could not read cached accessibility nodes. Call get_app_state and retry.".to_string()
        })?;

        if let Some(element_index) = element_index {
            return cached
                .iter()
                .find(|node| node.index == element_index)
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "No cached accessibility node for element_index {element_index}. Call get_app_state first."
                    )
                });
        }

        if selector.is_empty() {
            return Err(
                "Pass element_index, element_identifier, or a semantic selector such as role/name/text/states from the latest get_app_state result."
                    .to_string(),
            );
        }

        resolve_semantic_node(cached.as_slice(), selector, purpose)
    }

    async fn perform_element_action(
        &self,
        params: &ActionParams,
        requested_action: Option<&str>,
    ) -> Json<ActionOutput> {
        let received = Some(serde_json::json!(params.clone()));
        let object_ref = match self.resolve_object_ref(
            params.element_index,
            params.element_identifier.as_deref(),
            &params.selector(),
            ElementResolvePurpose::Action,
        ) {
            Ok(object_ref) => object_ref,
            Err(message) => {
                return Json(ActionOutput {
                    dispatched: false,
                    landed: None,
                    verified: false,
                    ok: false,
                    implemented: true,
                    action: "perform_action".to_string(),
                    message,
                    received,
                });
            }
        };

        match invoke_accessibility_action(&object_ref, requested_action).await {
            Ok(invocation) => Json(ActionOutput {
                dispatched: invocation.ok,
                landed: Some(invocation.ok),
                verified: true,
                ok: invocation.ok,
                implemented: true,
                action: "perform_action".to_string(),
                message: if invocation.ok {
                    format!(
                        "AT-SPI action {} ({}) invoked.",
                        invocation.action_index,
                        invocation
                            .action_name
                            .as_deref()
                            .filter(|name| !name.is_empty())
                            .unwrap_or("unnamed")
                    )
                } else {
                    format!(
                        "AT-SPI action {} ({}) returned false.",
                        invocation.action_index,
                        invocation
                            .action_name
                            .as_deref()
                            .filter(|name| !name.is_empty())
                            .unwrap_or("unnamed")
                    )
                },
                received,
            }),
            Err(error) => Json(ActionOutput {
                dispatched: false,
                landed: None,
                verified: false,
                ok: false,
                implemented: true,
                action: "perform_action".to_string(),
                message: error.to_string(),
                received,
            }),
        }
    }
}

#[derive(Debug)]
enum ClickTarget {
    Coordinates(i32, i32),
    PrimaryAction {
        object_ref: String,
        action_name: Option<String>,
        action_index: i32,
        fallback_coordinates: Option<(i32, i32)>,
    },
}

#[derive(Debug, Clone, Copy)]
enum ElementResolvePurpose {
    Click,
    Action,
    SetValue,
}

#[derive(Debug, Clone, Copy, Default)]
struct ElementSelector<'a> {
    role: Option<&'a str>,
    name: Option<&'a str>,
    text: Option<&'a str>,
    states: &'a [String],
}

impl ElementSelector<'_> {
    fn is_empty(&self) -> bool {
        [self.role, self.name, self.text]
            .into_iter()
            .all(|value| value.map(str::trim).is_none_or(str::is_empty))
            && self.states.iter().all(|value| value.trim().is_empty())
    }
}

fn resolve_semantic_node(
    nodes: &[AccessibilityNode],
    selector: &ElementSelector<'_>,
    purpose: ElementResolvePurpose,
) -> std::result::Result<AccessibilityNode, String> {
    let mut matches = nodes
        .iter()
        .filter(|node| node_matches_selector(node, selector))
        .collect::<Vec<_>>();

    if matches.is_empty() {
        return Err(format!(
            "No cached accessibility node matched semantic selector {}. Call get_app_state first or pass element_index.",
            describe_selector(selector)
        ));
    }

    if let Some(node) =
        unique_preferred_node(&matches, |node| node_matches_resolve_purpose(node, purpose))
    {
        return Ok(node.clone());
    }

    let useful_matches = matches
        .iter()
        .copied()
        .filter(|node| node_matches_resolve_purpose(node, purpose))
        .collect::<Vec<_>>();
    if !useful_matches.is_empty() {
        matches = useful_matches;
    }

    if let Some(node) = unique_preferred_node(&matches, node_is_showing) {
        return Ok(node.clone());
    }

    let visible_matches = matches
        .iter()
        .copied()
        .filter(|node| node_is_showing(node))
        .collect::<Vec<_>>();
    if !visible_matches.is_empty() {
        matches = visible_matches;
    }

    if matches.len() == 1 {
        return Ok(matches[0].clone());
    }

    Err(format!(
        "Semantic selector {} matched multiple cached nodes: {}. Pass element_index or add more selector fields.",
        describe_selector(selector),
        describe_matching_nodes(&matches),
    ))
}

fn unique_preferred_node<'a>(
    nodes: &[&'a AccessibilityNode],
    predicate: impl Fn(&AccessibilityNode) -> bool,
) -> Option<&'a AccessibilityNode> {
    let mut matches = nodes.iter().copied().filter(|node| predicate(node));
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

fn node_matches_selector(node: &AccessibilityNode, selector: &ElementSelector<'_>) -> bool {
    selector
        .role
        .is_none_or(|role| normalized_contains(Some(node.role.as_str()), role))
        && selector
            .name
            .is_none_or(|name| normalized_contains(node.name.as_deref(), name))
        && selector.text.is_none_or(|text| {
            normalized_contains(
                node.text
                    .as_ref()
                    .and_then(|value| value.content.as_deref()),
                text,
            ) || normalized_contains(node.name.as_deref(), text)
                || normalized_contains(node.description.as_deref(), text)
        })
        && selector
            .states
            .iter()
            .filter(|state| !state.trim().is_empty())
            .all(|state| {
                node.states
                    .iter()
                    .any(|node_state| normalized_equals(node_state, state))
            })
}

fn node_matches_resolve_purpose(node: &AccessibilityNode, purpose: ElementResolvePurpose) -> bool {
    match purpose {
        ElementResolvePurpose::Click => {
            node.bounds.as_ref().and_then(bounds_center).is_some()
                || primary_action_name(&node.actions).is_some()
        }
        ElementResolvePurpose::Action => !node.actions.is_empty(),
        ElementResolvePurpose::SetValue => node.supports_editable_text || node.value.is_some(),
    }
}

fn node_is_showing(node: &AccessibilityNode) -> bool {
    node.states
        .iter()
        .any(|state| normalized_equals(state, "showing"))
        && node
            .states
            .iter()
            .any(|state| normalized_equals(state, "visible"))
}

fn normalized_equals(actual: &str, expected: &str) -> bool {
    normalize_text(actual) == normalize_text(expected)
}

fn normalized_contains(actual: Option<&str>, expected: &str) -> bool {
    let expected = normalize_text(expected);
    !expected.is_empty()
        && actual
            .map(normalize_text)
            .is_some_and(|actual| actual.contains(&expected))
}

fn normalize_text(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn describe_selector(selector: &ElementSelector<'_>) -> String {
    let mut parts = Vec::new();
    if let Some(role) = selector.role.map(str::trim).filter(|role| !role.is_empty()) {
        parts.push(format!("role={role:?}"));
    }
    if let Some(name) = selector.name.map(str::trim).filter(|name| !name.is_empty()) {
        parts.push(format!("name={name:?}"));
    }
    if let Some(text) = selector.text.map(str::trim).filter(|text| !text.is_empty()) {
        parts.push(format!("text={text:?}"));
    }
    let states = selector
        .states
        .iter()
        .map(|state| state.trim())
        .filter(|state| !state.is_empty())
        .collect::<Vec<_>>();
    if !states.is_empty() {
        parts.push(format!("states={states:?}"));
    }
    if parts.is_empty() {
        "<empty>".to_string()
    } else {
        parts.join(", ")
    }
}

fn describe_matching_nodes(nodes: &[&AccessibilityNode]) -> String {
    nodes
        .iter()
        .take(8)
        .map(|node| {
            format!(
                "element_index {} role={:?} name={:?}",
                node.index, node.role, node.name
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn is_plain_left_click(button: Option<&str>, click_count: Option<u32>) -> bool {
    let button = button.unwrap_or("left");
    let click_count = click_count.unwrap_or(1);
    matches!(button.to_ascii_lowercase().as_str(), "left" | "primary") && click_count == 1
}

fn requested_or_primary_action(action: Option<&str>) -> &str {
    match action.map(str::trim).filter(|value| !value.is_empty()) {
        Some(action) => action,
        None => "0",
    }
}

fn primary_action(actions: &[AccessibilityAction]) -> Option<&AccessibilityAction> {
    actions.first()
}

fn primary_action_name(actions: &[AccessibilityAction]) -> Option<String> {
    primary_action(actions).map(|action| action.name.clone())
}

fn bounds_center(bounds: &Bounds) -> Option<(i32, i32)> {
    if bounds.width <= 0 || bounds.height <= 0 {
        return None;
    }
    if bounds.x <= i32::MIN / 2 || bounds.y <= i32::MIN / 2 {
        return None;
    }
    Some((
        bounds.x.checked_add(bounds.width / 2)?,
        bounds.y.checked_add(bounds.height / 2)?,
    ))
}

fn normalize_visual_text(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

fn expand_visual_bounds(
    bounds: ImageBounds,
    image_width: u32,
    image_height: u32,
    role: Option<&str>,
) -> Option<ImageBounds> {
    if bounds.width == 0 || bounds.height == 0 || image_width == 0 || image_height == 0 {
        return None;
    }
    let (pad_x, pad_y) = match role.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("textbox" | "text" | "entry" | "input") => (12_i64, 8_i64),
        Some("button" | "push button" | "menu item") => (10_i64, 6_i64),
        Some(_) => (8_i64, 6_i64),
        None => (0_i64, 0_i64),
    };
    let left = (i64::from(bounds.x) - pad_x).max(0);
    let top = (i64::from(bounds.y) - pad_y).max(0);
    let right = (i64::from(bounds.x) + i64::from(bounds.width) + pad_x).min(i64::from(image_width));
    let bottom =
        (i64::from(bounds.y) + i64::from(bounds.height) + pad_y).min(i64::from(image_height));
    (right > left && bottom > top).then_some(ImageBounds {
        x: i32::try_from(left).ok()?,
        y: i32::try_from(top).ok()?,
        width: u32::try_from(right - left).ok()?,
        height: u32::try_from(bottom - top).ok()?,
    })
}

fn new_visual_target_id() -> Result<String> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random)?;
    let mut id = String::from("target-");
    id.reserve(random.len() * 2);
    for byte in random {
        id.push(HEX[(byte >> 4) as usize] as char);
        id.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Ok(id)
}

/// Build a best-effort logical AT-SPI -> physical X11 transform by aligning
/// the app's top-level accessible frame with the window geometry reported by
/// the X11 window backend. Chromium exposes AT-SPI bounds in device-independent
/// pixels when desktop scaling is enabled, while xdotool and pointer injection
/// use physical pixels.
fn atspi_coordinate_map(
    nodes: &[AccessibilityNode],
    window: &WindowInfo,
) -> Option<AtspiCoordinateMap> {
    if !matches!(
        window.backend.as_str(),
        crate::windowing::XDOTOOL_BACKEND | crate::windowing::I3_BACKEND
    ) {
        return None;
    }
    let physical = window.bounds.as_ref()?;
    let physical_x = physical.x?;
    let physical_y = physical.y?;
    if physical.width == 0 || physical.height == 0 {
        return None;
    }

    let usable = |node: &&AccessibilityNode| {
        let Some(bounds) = node.bounds.as_ref() else {
            return false;
        };
        bounds.x > i32::MIN / 2 && bounds.y > i32::MIN / 2 && bounds.width > 0 && bounds.height > 0
    };
    let area = |node: &&AccessibilityNode| {
        let bounds = node.bounds.as_ref().expect("filtered bounds");
        i64::from(bounds.width) * i64::from(bounds.height)
    };
    let logical = nodes
        .iter()
        .filter(usable)
        .filter(|node| {
            matches!(
                node.role.trim().to_ascii_lowercase().as_str(),
                "frame" | "window" | "dialog"
            )
        })
        .max_by_key(area)
        .or_else(|| {
            nodes
                .iter()
                .filter(usable)
                .filter(|node| node.depth <= 1)
                .max_by_key(area)
        })?
        .bounds
        .as_ref()?;

    let scale_x = f64::from(physical.width) / f64::from(logical.width);
    let scale_y = f64::from(physical.height) / f64::from(logical.height);
    let scale = scale_x.min(scale_y);
    let ratio_spread = (scale_x - scale_y).abs() / scale_x.max(scale_y);
    if !scale.is_finite() || !(0.5..=4.0).contains(&scale) || ratio_spread > 0.20 {
        return None;
    }

    Some(AtspiCoordinateMap {
        logical_x: logical.x,
        logical_y: logical.y,
        physical_x,
        physical_y,
        scale,
    })
}

fn map_atspi_point(mapping: AtspiCoordinateMap, point: (i32, i32)) -> Option<(i32, i32)> {
    let x = f64::from(mapping.physical_x)
        + f64::from(point.0.checked_sub(mapping.logical_x)?) * mapping.scale;
    let y = f64::from(mapping.physical_y)
        + f64::from(point.1.checked_sub(mapping.logical_y)?) * mapping.scale;
    if !x.is_finite()
        || !y.is_finite()
        || x < f64::from(i32::MIN)
        || x > f64::from(i32::MAX)
        || y < f64::from(i32::MIN)
        || y > f64::from(i32::MAX)
    {
        return None;
    }
    Some((x.round() as i32, y.round() as i32))
}

fn compact_accessibility_tree(nodes: Vec<AccessibilityNode>) -> Vec<AccessibilityNode> {
    if nodes.is_empty() {
        return nodes;
    }

    let keep = nodes
        .iter()
        .map(should_keep_accessibility_node)
        .collect::<Vec<_>>();
    let mut old_to_new = vec![None; nodes.len()];
    let mut compacted = Vec::new();

    for (old_index, node) in nodes.iter().enumerate() {
        if !keep[old_index] {
            continue;
        }

        let mut compacted_node = node.clone();
        compacted_node.index = compacted.len() as u32;
        compacted_node.parent_index = nearest_kept_parent(&keep, &nodes, old_index);
        old_to_new[old_index] = Some(compacted_node.index);
        compacted.push(compacted_node);
    }

    for node in &mut compacted {
        node.parent_index = node
            .parent_index
            .and_then(|old_parent| old_to_new.get(old_parent as usize).copied().flatten());
    }

    let child_counts = compacted.iter().filter_map(|node| node.parent_index).fold(
        vec![0_i32; compacted.len()],
        |mut counts, parent_index| {
            counts[parent_index as usize] += 1;
            counts
        },
    );

    for (index, node) in compacted.iter_mut().enumerate() {
        node.child_count = child_counts[index];
    }

    compacted
}

fn nearest_kept_parent(
    keep: &[bool],
    nodes: &[AccessibilityNode],
    old_index: usize,
) -> Option<u32> {
    let mut parent = nodes[old_index].parent_index;
    while let Some(parent_index) = parent {
        let parent_usize = parent_index as usize;
        if keep.get(parent_usize).copied().unwrap_or(false) {
            return Some(parent_index);
        }
        parent = nodes.get(parent_usize).and_then(|node| node.parent_index);
    }
    None
}

fn should_keep_accessibility_node(node: &AccessibilityNode) -> bool {
    if node.depth <= 1 {
        return true;
    }

    if is_actionable_accessibility_node(node) || has_meaningful_node_copy(node) {
        return true;
    }

    matches!(
        node.role.as_str(),
        "page tab" | "menu item" | "menu" | "list item" | "tree item"
    ) && !is_sentinel_or_missing_bounds(node.bounds.as_ref())
}

fn is_actionable_accessibility_node(node: &AccessibilityNode) -> bool {
    !node.actions.is_empty() || node.supports_editable_text || node.value.is_some()
}

fn has_meaningful_node_copy(node: &AccessibilityNode) -> bool {
    has_non_empty_text(node.name.as_deref())
        || has_non_empty_text(node.description.as_deref())
        || has_non_empty_text(node.text.as_ref().and_then(|text| text.content.as_deref()))
}

fn has_non_empty_text(value: Option<&str>) -> bool {
    value.map(str::trim).is_some_and(|value| !value.is_empty())
}

fn is_sentinel_or_missing_bounds(bounds: Option<&Bounds>) -> bool {
    bounds.is_none()
}

fn select_accessibility_object_ref(
    apps: &[AccessibleAppSummary],
    target_pid: u32,
    candidates: &[String],
) -> Option<String> {
    let mut pid_matches = apps.iter().filter(|app| app.pid == Some(target_pid));
    let first = pid_matches.next()?;
    let second = pid_matches.next();

    if second.is_none() {
        return Some(first.object_ref.clone());
    }

    let lowered_candidates = candidates
        .iter()
        .map(|candidate| candidate.to_ascii_lowercase())
        .collect::<Vec<_>>();

    apps.iter()
        .filter(|app| app.pid == Some(target_pid))
        .find(|app| {
            let name = app.name.as_deref().unwrap_or_default().to_ascii_lowercase();
            lowered_candidates
                .iter()
                .any(|candidate| !candidate.is_empty() && name.contains(candidate))
        })
        .map(|app| app.object_ref.clone())
        .or_else(|| Some(first.object_ref.clone()))
}

fn accessibility_filter_candidates(window_context: Option<&WindowInfo>) -> Vec<String> {
    let Some(window) = window_context else {
        return Vec::new();
    };

    let mut candidates = Vec::new();
    push_candidate(&mut candidates, window.title.as_deref());
    push_candidate(&mut candidates, window.wm_class.as_deref());

    if let Some(app_id) = trimmed_nonempty(window.app_id.as_deref()) {
        if !app_id.starts_with("window:") {
            push_candidate(&mut candidates, Some(app_id));
            if let Some(stripped) = app_id.strip_suffix(".desktop") {
                push_candidate(&mut candidates, Some(stripped));
                let normalized = stripped.replace(['-', '_', '.'], " ");
                push_candidate(&mut candidates, Some(normalized.as_str()));
            } else {
                let normalized = app_id.replace(['-', '_', '.'], " ");
                push_candidate(&mut candidates, Some(normalized.as_str()));
            }
        }
    }

    candidates
}

fn push_candidate(candidates: &mut Vec<String>, value: Option<&str>) {
    let Some(value) = trimmed_nonempty(value) else {
        return;
    };

    if !candidates.iter().any(|candidate| candidate == value) {
        candidates.push(value.to_string());
    }
}

fn trimmed_nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn env_contains(key: &str, needle: &str) -> bool {
    env::var(key)
        .ok()
        .is_some_and(|value| value.to_ascii_lowercase().contains(needle))
}

/// True when an environment variable is set to `"1"` (an explicit on switch).
fn env_flag_enabled(key: &str) -> bool {
    env::var(key).ok().as_deref() == Some("1")
}

fn env_flag_enabled_any(keys: &[&str]) -> bool {
    keys.iter().any(|key| env_flag_enabled(key))
}

/// Return the base64 payload of a `data:` URL (or the original string if bare).
fn data_url_payload(data_url: &str) -> String {
    data_url
        .split_once(',')
        .map(|(_, payload)| payload)
        .unwrap_or(data_url)
        .to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WindowCropRect {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

fn window_bounds_match_capture_space(window: &WindowInfo) -> bool {
    matches!(
        window.backend.as_str(),
        crate::windowing::XDOTOOL_BACKEND | crate::windowing::I3_BACKEND
    )
}

fn window_snapshot(window: &WindowInfo) -> Option<WindowSnapshot> {
    let bounds = window.bounds.as_ref()?;
    let frame_rect = DesktopRect {
        x: bounds.x?,
        y: bounds.y?,
        width: bounds.width,
        height: bounds.height,
    };
    (frame_rect.width > 0 && frame_rect.height > 0).then_some(WindowSnapshot {
        window_id: window.window_id,
        frame_rect,
        client_rect: None,
        client_insets: None,
    })
}

fn prepare_get_app_state_capture(
    raw: RawScreenshotCapture,
    window: Option<&WindowInfo>,
    options: ScreenshotPayloadOptions,
) -> std::result::Result<(ScreenshotCapture, Option<WindowSnapshot>), String> {
    let Some(window) = window else {
        return prepare_screenshot_payload(raw, options)
            .map(|capture| (capture, None))
            .map_err(|error| format!("screenshot resize failed: {error:#}"));
    };
    if !window_bounds_match_capture_space(window) {
        return Err(format!(
            "targeted get_app_state cannot safely crop window_id {} from backend {:?}: its bounds are not physical screenshot pixels",
            window.window_id, window.backend
        ));
    }
    let snapshot = window_snapshot(window).ok_or_else(|| {
        format!(
            "targeted get_app_state window_id {} has incomplete or empty bounds",
            window.window_id
        )
    })?;
    let rect = window_crop_rect_for_capture(
        window
            .bounds
            .as_ref()
            .expect("window snapshot requires bounds"),
        raw.width,
        raw.height,
    )?;
    let (bytes, width, height) =
        crop_image_to_png(&raw.bytes, rect.x, rect.y, rect.width, rect.height)
            .map_err(|error| format!("targeted get_app_state crop failed: {error:#}"))?;
    if width != rect.width || height != rect.height {
        return Err(format!(
            "targeted get_app_state crop returned {width}x{height}; expected {}x{}",
            rect.width, rect.height
        ));
    }
    let mut capture = prepare_screenshot_payload(
        RawScreenshotCapture {
            bytes,
            source: raw.source,
            width,
            height,
        },
        options,
    )
    .map_err(|error| format!("targeted get_app_state resize failed: {error:#}"))?;
    capture.coordinate_origin_x = rect.x;
    capture.coordinate_origin_y = rect.y;
    capture.cropped_to_window = true;
    capture.target_window_id = Some(window.window_id);
    Ok((capture, Some(snapshot)))
}

/// Intersect window bounds with the pixels present in a full-desktop capture.
/// The returned origin is both the PNG crop origin and the physical desktop
/// origin represented by pixel (0, 0) in the cropped result. Raw captures do
/// not currently expose a virtual-desktop origin, so their pixel canvas is
/// defined as `[0, width) x [0, height)`; backends with a non-zero canvas
/// origin must add that metadata before their negative global coordinates can
/// be translated rather than clipped.
fn window_crop_rect_for_capture(
    bounds: &crate::windowing::WindowBounds,
    capture_width: u32,
    capture_height: u32,
) -> std::result::Result<WindowCropRect, String> {
    let x = bounds
        .x
        .ok_or_else(|| "target window bounds have no x origin".to_string())?;
    let y = bounds
        .y
        .ok_or_else(|| "target window bounds have no y origin".to_string())?;
    if bounds.width == 0 || bounds.height == 0 {
        return Err("target window bounds must be non-empty".to_string());
    }
    if capture_width == 0 || capture_height == 0 {
        return Err("desktop screenshot has invalid zero-sized bounds".to_string());
    }

    let left = i64::from(x);
    let top = i64::from(y);
    let right = left + i64::from(bounds.width);
    let bottom = top + i64::from(bounds.height);
    let visible_left = left.max(0);
    let visible_top = top.max(0);
    let visible_right = right.min(i64::from(capture_width));
    let visible_bottom = bottom.min(i64::from(capture_height));
    if visible_left >= visible_right || visible_top >= visible_bottom {
        return Err(format!(
            "target window bounds ({x},{y} {}x{}) are completely outside the desktop screenshot ({}x{})",
            bounds.width, bounds.height, capture_width, capture_height
        ));
    }

    Ok(WindowCropRect {
        x: i32::try_from(visible_left)
            .map_err(|_| "visible crop x origin exceeds desktop coordinates".to_string())?,
        y: i32::try_from(visible_top)
            .map_err(|_| "visible crop y origin exceeds desktop coordinates".to_string())?,
        width: u32::try_from(visible_right - visible_left)
            .map_err(|_| "visible crop width exceeds screenshot coordinates".to_string())?,
        height: u32::try_from(visible_bottom - visible_top)
            .map_err(|_| "visible crop height exceeds screenshot coordinates".to_string())?,
    })
}

fn apply_window_relative_click_coordinates(
    params: &mut ClickParams,
    focus: &WindowFocusResult,
) -> std::result::Result<(), String> {
    let (relative_x, relative_y) = params
        .x
        .zip(params.y)
        .ok_or_else(|| "Relative coordinate clicks require both x and y.".to_string())?;
    let bounds = focus
        .focused_window
        .as_ref()
        .and_then(|window| window.bounds.as_ref())
        .or(focus.requested_window.bounds.as_ref())
        .ok_or_else(|| {
            "Relative coordinate clicks require resolved target-window bounds.".to_string()
        })?;
    if bounds.width == 0 || bounds.height == 0 {
        return Err(
            "Relative coordinate clicks require non-empty target-window bounds.".to_string(),
        );
    }
    if relative_x < 0 || relative_y < 0 {
        return Err("Relative click coordinates must be inside target-window bounds.".to_string());
    }
    if relative_x as u32 >= bounds.width || relative_y as u32 >= bounds.height {
        return Err("Relative click coordinates must be inside target-window bounds.".to_string());
    }
    let (origin_x, origin_y) = bounds.x.zip(bounds.y).ok_or_else(|| {
        "Relative coordinate clicks require target-window bounds with an origin.".to_string()
    })?;
    let x = origin_x
        .checked_add(relative_x)
        .ok_or_else(|| "Relative click x coordinate overflowed.".to_string())?;
    let y = origin_y
        .checked_add(relative_y)
        .ok_or_else(|| "Relative click y coordinate overflowed.".to_string())?;
    params.x = Some(x);
    params.y = Some(y);
    Ok(())
}

/// Point a window-targeted scroll at the centre of the resolved window when
/// the caller supplied no coordinates. Without this the wheel events land on
/// whatever is under the current pointer position.
fn apply_window_center_scroll_point(
    params: &mut ScrollParams,
    focus: &WindowFocusResult,
) -> std::result::Result<(), String> {
    let bounds = focus
        .focused_window
        .as_ref()
        .and_then(|window| window.bounds.as_ref())
        .or(focus.requested_window.bounds.as_ref())
        .ok_or_else(|| {
            "Window-targeted scroll requires resolved target-window bounds; pass x/y explicitly."
                .to_string()
        })?;
    if bounds.width == 0 || bounds.height == 0 {
        return Err(
            "Window-targeted scroll requires non-empty target-window bounds; pass x/y explicitly."
                .to_string(),
        );
    }
    let (origin_x, origin_y) = bounds.x.zip(bounds.y).ok_or_else(|| {
        "Window-targeted scroll requires target-window bounds with an origin; pass x/y explicitly."
            .to_string()
    })?;
    params.x = Some(origin_x.saturating_add((bounds.width / 2) as i32));
    params.y = Some(origin_y.saturating_add((bounds.height / 2) as i32));
    Ok(())
}

fn apply_window_relative_scroll_coordinates(
    params: &mut ScrollParams,
    focus: &WindowFocusResult,
) -> std::result::Result<(), String> {
    let (relative_x, relative_y) = params
        .x
        .zip(params.y)
        .ok_or_else(|| "Relative scroll coordinates require both x and y.".to_string())?;
    let bounds = focus
        .focused_window
        .as_ref()
        .and_then(|window| window.bounds.as_ref())
        .or(focus.requested_window.bounds.as_ref())
        .ok_or_else(|| {
            "Relative scroll coordinates require resolved target-window bounds.".to_string()
        })?;
    if bounds.width == 0 || bounds.height == 0 {
        return Err(
            "Relative scroll coordinates require non-empty target-window bounds.".to_string(),
        );
    }
    if relative_x < 0
        || relative_y < 0
        || relative_x as u32 >= bounds.width
        || relative_y as u32 >= bounds.height
    {
        return Err("Relative scroll coordinates must be inside target-window bounds.".to_string());
    }
    let (origin_x, origin_y) = bounds.x.zip(bounds.y).ok_or_else(|| {
        "Relative scroll coordinates require target-window bounds with an origin.".to_string()
    })?;
    params.x = Some(origin_x.saturating_add(relative_x));
    params.y = Some(origin_y.saturating_add(relative_y));
    Ok(())
}

/// Crop any supported screenshot image to an already-validated rectangle,
/// returning a normalized PNG and cropped dimensions. Invalid or out-of-range
/// rectangles are rejected instead of being silently clamped.
fn crop_image_to_png(
    raw: &[u8],
    x: i32,
    y: i32,
    w: u32,
    h: u32,
) -> std::result::Result<(Vec<u8>, u32, u32), String> {
    use std::io::Cursor;
    let img = image::load_from_memory(raw).map_err(|e| format!("decode screenshot image: {e}"))?;
    let (iw, ih) = (img.width(), img.height());
    if x < 0 || y < 0 || w == 0 || h == 0 {
        return Err("crop rectangle must have a non-negative origin and non-zero size".into());
    }
    let x = x as u32;
    let y = y as u32;
    let right = x
        .checked_add(w)
        .ok_or_else(|| "crop rectangle x range overflowed".to_string())?;
    let bottom = y
        .checked_add(h)
        .ok_or_else(|| "crop rectangle y range overflowed".to_string())?;
    if x >= iw || y >= ih || right > iw || bottom > ih {
        return Err(format!(
            "crop rectangle ({x},{y} {w}x{h}) is outside image ({iw}x{ih})"
        ));
    }
    let sub = img.crop_imm(x, y, w, h);
    let mut out = Vec::new();
    sub.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
        .map_err(|e| format!("encode png: {e}"))?;
    Ok((out, w, h))
}

fn action_result(
    action: &str,
    result: std::result::Result<Vec<Output>, String>,
    received: Option<serde_json::Value>,
) -> ActionOutput {
    action_result_for_backend(action, result, received, "ydotool")
}

fn rejected_action(
    action: &str,
    message: String,
    received: Option<serde_json::Value>,
) -> ActionOutput {
    ActionOutput {
        dispatched: false,
        landed: None,
        verified: false,
        ok: false,
        implemented: true,
        action: action.to_string(),
        message,
        received,
    }
}

fn rejected_click_verification(target_id: &str, message: String) -> ClickVerificationOutput {
    ClickVerificationOutput {
        ok: false,
        stage: "rejected".to_string(),
        dispatched: false,
        landed: None,
        verified: false,
        target_id: target_id.to_string(),
        screenshot_id: None,
        pointer_arrived: None,
        actual_pointer_x: None,
        actual_pointer_y: None,
        region_change_score: None,
        region_changed: None,
        expected_text_present: None,
        expected_text_absent: None,
        focused_editable: None,
        evidence_screenshot_id: None,
        message,
    }
}

#[allow(clippy::too_many_arguments)]
fn dispatched_unverified(
    params: &ClickAndVerifyParams,
    target: &VisualTarget,
    pointer_arrived: Option<bool>,
    actual_pointer: Option<(i32, i32)>,
    region_change_score: Option<f64>,
    region_changed: Option<bool>,
    expected_text_present: Option<bool>,
    expected_text_absent: Option<bool>,
    focused_editable: Option<bool>,
    evidence_screenshot_id: Option<String>,
    message: String,
) -> ClickVerificationOutput {
    ClickVerificationOutput {
        ok: false,
        stage: "dispatched_unverified".to_string(),
        dispatched: true,
        landed: pointer_arrived,
        verified: false,
        target_id: params.target_id.clone(),
        screenshot_id: Some(target.screenshot_id.clone()),
        pointer_arrived,
        actual_pointer_x: actual_pointer.map(|point| point.0),
        actual_pointer_y: actual_pointer.map(|point| point.1),
        region_change_score,
        region_changed,
        expected_text_present,
        expected_text_absent,
        focused_editable,
        evidence_screenshot_id,
        message,
    }
}

fn query_x11_pointer_position() -> std::result::Result<(i32, i32), String> {
    use x11rb::{connection::Connection, protocol::xproto::ConnectionExt};
    let (connection, screen_index) =
        x11rb::connect(None).map_err(|error| format!("connect to X11: {error}"))?;
    let screen = connection
        .setup()
        .roots
        .get(screen_index)
        .ok_or_else(|| "X11 screen index is invalid".to_string())?;
    let reply = connection
        .query_pointer(screen.root)
        .map_err(|error| format!("query X11 pointer: {error}"))?
        .reply()
        .map_err(|error| format!("read X11 pointer reply: {error}"))?;
    Ok((i32::from(reply.root_x), i32::from(reply.root_y)))
}

fn action_result_for_backend(
    action: &str,
    result: std::result::Result<Vec<Output>, String>,
    received: Option<serde_json::Value>,
    backend: &str,
) -> ActionOutput {
    match result {
        Ok(_) => ActionOutput {
            dispatched: true,
            landed: None,
            verified: false,
            ok: true,
            implemented: true,
            action: action.to_string(),
            message: format!("Action sent through {backend}."),
            received,
        },
        Err(message) => ActionOutput {
            dispatched: false,
            landed: None,
            verified: false,
            ok: false,
            implemented: true,
            action: action.to_string(),
            message,
            received,
        },
    }
}

fn action_result_with_focus(
    action: &str,
    result: std::result::Result<Vec<Output>, String>,
    received: Option<serde_json::Value>,
    focus: Option<WindowFocusResult>,
) -> ActionOutput {
    with_focus_context(action_result(action, result, received), focus)
}

fn action_result_with_focus_for_backend(
    action: &str,
    result: std::result::Result<Vec<Output>, String>,
    received: Option<serde_json::Value>,
    focus: Option<WindowFocusResult>,
    backend: &str,
) -> ActionOutput {
    with_focus_context(
        action_result_for_backend(action, result, received, backend),
        focus,
    )
}

fn successful_action_with_focus(
    action: &str,
    message: &str,
    received: Option<serde_json::Value>,
    focus: Option<WindowFocusResult>,
) -> ActionOutput {
    with_focus_context(
        ActionOutput {
            dispatched: true,
            landed: None,
            verified: false,
            ok: true,
            implemented: true,
            action: action.to_string(),
            message: message.to_string(),
            received,
        },
        focus,
    )
}

fn with_focus_context(mut output: ActionOutput, focus: Option<WindowFocusResult>) -> ActionOutput {
    if output.dispatched {
        if let Some(focus) = focus {
            let verification = if focus.exact_window_focused {
                "exact window-focus"
            } else {
                "app-level focus"
            };
            output.message = format!(
                "{} Target window_id {} was focused with {verification} verification before input.",
                output.message, focus.requested_window.window_id,
            );
        }
    }
    output
}

#[derive(Debug, Clone, Default)]
struct ActionFeedback {
    notes: Vec<String>,
    landed: Option<bool>,
    verified: bool,
}

impl ActionFeedback {
    fn unverified(note: impl Into<String>) -> Self {
        Self {
            notes: vec![note.into()],
            landed: None,
            verified: false,
        }
    }

    fn failed_landing(note: impl Into<String>) -> Self {
        Self {
            notes: vec![note.into()],
            landed: Some(false),
            verified: true,
        }
    }

    fn merge(&mut self, other: Self) {
        self.notes.extend(other.notes);
        if other.landed.is_some() {
            self.landed = other.landed;
        }
        self.verified |= other.verified;
    }
}

fn focused_element_assessment(
    element: &FocusedElementSummary,
    expects_editable: bool,
) -> ActionFeedback {
    let note = describe_focused_element(element, expects_editable);
    if expects_editable && !element.editable {
        ActionFeedback {
            notes: vec![note],
            landed: Some(false),
            verified: true,
        }
    } else {
        ActionFeedback::unverified(note)
    }
}

fn describe_focused_element(element: &FocusedElementSummary, expects_editable: bool) -> String {
    let name = element
        .name
        .as_deref()
        .filter(|name| !name.is_empty())
        .map(|name| format!(" \"{name}\""))
        .unwrap_or_default();
    if element.editable {
        format!("Focused element: {}{name} (editable).", element.role)
    } else if expects_editable {
        format!(
            "WARNING: focused element is {}{name}, which is not editable — the typed text likely went nowhere. Click the intended input first or use set_value.",
            element.role
        )
    } else {
        format!("Focused element: {}{name} (not editable).", element.role)
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or(text)
}

/// Append supplemental notes (off-screen or focused-element feedback) to an
/// action result message without changing ok/implemented semantics.
fn with_notes(mut output: ActionOutput, notes: impl IntoIterator<Item = String>) -> ActionOutput {
    for note in notes {
        output.message = format!("{} {note}", output.message);
    }
    output
}

/// Apply target-level post-dispatch evidence. A conclusively wrong landing is
/// a failed action even though the backend successfully sent its events.
fn with_action_feedback(mut output: ActionOutput, feedback: ActionFeedback) -> ActionOutput {
    output = with_notes(output, feedback.notes);
    if output.dispatched {
        output.landed = feedback.landed;
        output.verified = feedback.verified;
        if output.landed == Some(false) {
            output.ok = false;
        }
    }
    output
}

fn focus_satisfies_target(focus: &WindowFocusResult, target: &WindowTarget) -> bool {
    if target.requires_exact_focus() {
        focus.exact_window_focused
    } else {
        focus.exact_window_focused || focus.app_focused
    }
}

async fn window_list_output() -> ListWindowsOutput {
    match list_windows().await {
        Ok(windows) => {
            let backend = window_backend(windows.iter());
            let note = registry::list_note(&backend);
            ListWindowsOutput {
                backend,
                windows,
                error: None,
                permissions_hint: None,
                note: note.to_string(),
            }
        }
        Err(error) => {
            let error = format!("{error:#}");
            ListWindowsOutput {
                backend: GNOME_SHELL_INTROSPECT_BACKEND.to_string(),
                windows: Vec::new(),
                permissions_hint: window_permission_hint(&error),
                error: Some(error),
                note: "Window listing failed, so targeted keyboard input cannot safely focus or verify a target window."
                    .to_string(),
            }
        }
    }
}

fn window_backend<'a>(windows: impl Iterator<Item = &'a WindowInfo>) -> String {
    windows
        .map(|window| window.backend.clone())
        .next()
        .unwrap_or_else(|| GNOME_SHELL_INTROSPECT_BACKEND.to_string())
}

fn absolute_mousemove_args(x: i32, y: i32) -> Vec<String> {
    vec![
        "mousemove".to_string(),
        "--absolute".to_string(),
        "--".to_string(),
        x.to_string(),
        y.to_string(),
    ]
}

fn wheel_mousemove_args(dx: i32, dy: i32) -> Vec<String> {
    vec![
        "mousemove".to_string(),
        "--wheel".to_string(),
        "--".to_string(),
        dx.to_string(),
        dy.to_string(),
    ]
}

fn xdotool_click_args(x: i32, y: i32, button: Option<&str>, click_count: u32) -> Vec<String> {
    vec![
        "mousemove".to_string(),
        "--sync".to_string(),
        "--".to_string(),
        x.to_string(),
        y.to_string(),
        "click".to_string(),
        "--repeat".to_string(),
        click_count.to_string(),
        "--delay".to_string(),
        "50".to_string(),
        xdotool_mouse_button(button).to_string(),
    ]
}

fn xdotool_scroll_args(
    target_point: Option<(i32, i32)>,
    direction: &str,
    units: i32,
) -> Vec<String> {
    let mut args = Vec::new();
    if let Some((x, y)) = target_point {
        args.extend([
            "mousemove".to_string(),
            "--sync".to_string(),
            "--".to_string(),
            x.to_string(),
            y.to_string(),
        ]);
    }
    let button = match direction.to_ascii_lowercase().as_str() {
        "up" => 4,
        "down" => 5,
        "left" => 6,
        "right" => 7,
        _ => 5,
    };
    args.extend([
        "click".to_string(),
        "--repeat".to_string(),
        units.max(1).to_string(),
        "--delay".to_string(),
        "25".to_string(),
        button.to_string(),
    ]);
    args
}

fn xdotool_drag_args(start_x: i32, start_y: i32, end_x: i32, end_y: i32) -> Vec<String> {
    vec![
        "mousemove".to_string(),
        "--sync".to_string(),
        "--".to_string(),
        start_x.to_string(),
        start_y.to_string(),
        "mousedown".to_string(),
        "1".to_string(),
        "mousemove".to_string(),
        "--sync".to_string(),
        "--".to_string(),
        end_x.to_string(),
        end_y.to_string(),
        "mouseup".to_string(),
        "1".to_string(),
    ]
}

fn xdotool_mouse_button(button: Option<&str>) -> u8 {
    match button.unwrap_or("left").to_ascii_lowercase().as_str() {
        "middle" => 2,
        "right" => 3,
        "side" | "back" => 8,
        "extra" | "forward" => 9,
        _ => 1,
    }
}

async fn run_ydotool_sequence(
    commands: &[Vec<String>],
) -> std::result::Result<Vec<Output>, String> {
    let mut outputs = Vec::new();
    for (index, args) in commands.iter().enumerate() {
        outputs.push(run_ydotool(args).await?);
        if index + 1 < commands.len() {
            sleep(Duration::from_millis(35)).await;
        }
    }
    Ok(outputs)
}

async fn run_xdotool(args: &[String]) -> std::result::Result<Output, String> {
    let mut command = TokioCommand::new("xdotool");
    command.args(args);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    match command.spawn() {
        Ok(child) => match wait_for_ydotool_output_with_timeout(child, XDOTOOL_TIMEOUT).await {
            Ok(output) if output.status.success() => Ok(output),
            Ok(output) => Err(command_output_error("xdotool", output)),
            Err(error) => Err(error.replace("ydotool", "xdotool")),
        },
        Err(error) => Err(format!("failed to run xdotool: {error}")),
    }
}

async fn run_ydotool(args: &[String]) -> std::result::Result<Output, String> {
    let mut command = TokioCommand::new("ydotool");
    command.args(args);
    if let Some(socket) = ydotool_socket() {
        command.env("YDOTOOL_SOCKET", socket);
    }
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    match command.spawn() {
        Ok(child) => match wait_for_ydotool_output(child).await {
            Ok(output) if output.status.success() => Ok(output),
            Ok(output) => Err(ydotool_output_error(output)),
            Err(error) => Err(error),
        },
        Err(error) => Err(format!("failed to run ydotool: {error}")),
    }
}

async fn run_ydotool_type_text(text: &str) -> std::result::Result<Output, String> {
    let mut command = TokioCommand::new("ydotool");
    command.args(["type", "--file", "-"]);
    if let Some(socket) = ydotool_socket() {
        command.env("YDOTOOL_SOCKET", socket);
    }
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    match command.spawn() {
        Ok(mut child) => {
            if let Some(mut stdin) = child.stdin.take() {
                if let Err(error) = stdin.write_all(text.as_bytes()).await {
                    let _ = child.kill().await;
                    return Err(format!("failed to write text to ydotool stdin: {error}"));
                }
            }
            let output =
                wait_for_ydotool_output_with_timeout(child, ydotool_type_timeout(text)).await?;
            if output.status.success() {
                Ok(output)
            } else {
                Err(ydotool_output_error(output))
            }
        }
        Err(error) => Err(format!("failed to run ydotool: {error}")),
    }
}

async fn wait_for_ydotool_output(child: TokioChild) -> std::result::Result<Output, String> {
    wait_for_ydotool_output_with_timeout(child, YDOTOOL_TIMEOUT).await
}

async fn wait_for_ydotool_output_with_timeout(
    mut child: TokioChild,
    timeout_duration: Duration,
) -> std::result::Result<Output, String> {
    let stdout_reader = read_child_pipe(child.stdout.take());
    let stderr_reader = read_child_pipe(child.stderr.take());
    let status = match timeout(timeout_duration, child.wait()).await {
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            stdout_reader.abort();
            stderr_reader.abort();
            return Err(format!(
                "ydotool timed out after {}s",
                timeout_duration.as_secs()
            ));
        }
        Ok(result) => result.map_err(|error| format!("failed to wait for ydotool: {error}"))?,
    };
    let stdout = stdout_reader.await.unwrap_or_default();
    let stderr = stderr_reader.await.unwrap_or_default();
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn read_child_pipe<R>(pipe: Option<R>) -> tokio::task::JoinHandle<Vec<u8>>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut output = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut output).await;
        }
        output
    })
}

fn ydotool_type_timeout(text: &str) -> Duration {
    let text_seconds = (text.chars().count() as u64).div_ceil(YDOTOOL_TYPE_CHARS_PER_SECOND);
    Duration::from_secs(YDOTOOL_TIMEOUT.as_secs().saturating_add(text_seconds))
}

fn prefer_x11_clipboard_text_backend(is_x11: bool, forced_ydotool_keyboard: bool) -> bool {
    is_x11 && !forced_ydotool_keyboard
}

fn clipboard_write_was_verified(expected: &str, actual: &str) -> bool {
    expected == actual
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipboardRestoreDecision {
    RestorePrevious,
    PreserveCurrent,
    Unreadable,
}

fn clipboard_restore_decision(
    temporary_owner: u32,
    current_owner: std::result::Result<u32, ()>,
) -> ClipboardRestoreDecision {
    match current_owner {
        Ok(current_owner) if current_owner == temporary_owner => {
            ClipboardRestoreDecision::RestorePrevious
        }
        Ok(_) => ClipboardRestoreDecision::PreserveCurrent,
        Err(_) => ClipboardRestoreDecision::Unreadable,
    }
}

fn clipboard_restore_decision_for_known_owner(
    temporary_owner: Option<u32>,
    current_owner: std::result::Result<u32, ()>,
) -> ClipboardRestoreDecision {
    match temporary_owner {
        Some(temporary_owner) => clipboard_restore_decision(temporary_owner, current_owner),
        None => match current_owner {
            Ok(_) => ClipboardRestoreDecision::PreserveCurrent,
            Err(()) => ClipboardRestoreDecision::Unreadable,
        },
    }
}

const X11_CLIPBOARD_MAX_TARGETS: usize = 256;
const X11_CLIPBOARD_MAX_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;
const X11_CLIPBOARD_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(5);

fn x11_clipboard_get_property_long_length(max_bytes: usize) -> u32 {
    let bounded_bytes = max_bytes.min(X11_CLIPBOARD_MAX_SNAPSHOT_BYTES);
    u32::try_from(bounded_bytes.div_ceil(4))
        .expect("the X11 clipboard snapshot byte limit always fits in u32 words")
}

fn x11_clipboard_snapshot_target_limit(
    accumulated_bytes: usize,
    owner_inline_limit: usize,
) -> std::result::Result<usize, String> {
    let remaining = X11_CLIPBOARD_MAX_SNAPSHOT_BYTES
        .checked_sub(accumulated_bytes)
        .filter(|remaining| *remaining > 0)
        .ok_or_else(|| {
            format!(
                "X11 clipboard snapshot reached its aggregate limit of {X11_CLIPBOARD_MAX_SNAPSHOT_BYTES} bytes"
            )
        })?;
    let target_limit = remaining.min(owner_inline_limit);
    if target_limit == 0 {
        return Err("X11 clipboard owner has no safe inline restore capacity".to_string());
    }
    Ok(target_limit)
}

#[derive(Debug)]
enum X11ClipboardSnapshot {
    Empty,
    Raw(Vec<X11RawClipboardTarget>),
}

#[derive(Debug, Clone)]
struct X11RawClipboardTarget {
    name: String,
    atom: u32,
    property: X11ClipboardProperty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct X11ClipboardProperty {
    type_atom: u32,
    format: u8,
    bytes: Vec<u8>,
}

fn x11_clipboard_property_from_reply_parts(
    requested_target: u32,
    type_atom: u32,
    format: u8,
    bytes: Vec<u8>,
) -> std::result::Result<X11ClipboardProperty, String> {
    if type_atom == x11rb::NONE {
        return Err(format!(
            "X11 clipboard target atom {requested_target} returned no property type"
        ));
    }
    x11_clipboard_property_item_count(format, bytes.len())?;
    Ok(X11ClipboardProperty {
        type_atom,
        format,
        bytes,
    })
}

fn x11_clipboard_property_item_count(
    format: u8,
    byte_len: usize,
) -> std::result::Result<u32, String> {
    let bytes_per_item = match format {
        8 => 1,
        16 => 2,
        32 => 4,
        _ => {
            return Err(format!(
                "X11 clipboard property uses unsupported format {format}"
            ));
        }
    };
    if !byte_len.is_multiple_of(bytes_per_item) {
        return Err(format!(
            "X11 clipboard property has {byte_len} bytes, which is not aligned to format {format}"
        ));
    }
    u32::try_from(byte_len / bytes_per_item)
        .map_err(|_| "X11 clipboard property contains too many items".to_string())
}

fn load_x11_clipboard_property(
    context: &x11_clipboard::Context,
    selection: u32,
    requested_target: u32,
    property: u32,
    deadline: Instant,
    max_inline_bytes: usize,
) -> std::result::Result<X11ClipboardProperty, String> {
    use x11rb::{
        connection::Connection,
        protocol::{
            xproto::{AtomEnum, ConnectionExt, Property},
            Event,
        },
    };

    let result = (|| {
        if Instant::now() >= deadline {
            return Err("X11 clipboard snapshot exceeded its total deadline".to_string());
        }
        let property_long_length = x11_clipboard_get_property_long_length(max_inline_bytes);
        context
            .connection
            .delete_property(context.window, property)
            .map_err(|error| format!("failed to clear the X11 clipboard property: {error}"))?
            .check()
            .map_err(|error| format!("X11 rejected clipboard property cleanup: {error}"))?;
        context
            .connection
            .convert_selection(
                context.window,
                selection,
                requested_target,
                property,
                x11rb::CURRENT_TIME,
            )
            .map_err(|error| format!("failed to request X11 clipboard conversion: {error}"))?
            .check()
            .map_err(|error| format!("X11 rejected clipboard conversion: {error}"))?;
        context
            .connection
            .flush()
            .map_err(|error| format!("failed to flush X11 clipboard conversion: {error}"))?;

        let mut incremental_transfer = false;
        let mut incremental: Option<X11ClipboardProperty> = None;
        let mut incremental_error: Option<String> = None;
        loop {
            if Instant::now() >= deadline {
                return Err("X11 clipboard conversion timed out".to_string());
            }
            let event = match context.connection.poll_for_event().map_err(|error| {
                format!("failed while waiting for X11 clipboard conversion: {error}")
            })? {
                Some(event) => event,
                None => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
            };

            match event {
                Event::SelectionNotify(event)
                    if event.requestor == context.window
                        && event.selection == selection
                        && event.target == requested_target =>
                {
                    if event.property == x11rb::NONE {
                        return Err(
                            "the X11 clipboard owner could not convert the target".to_string()
                        );
                    }
                    let reply = context
                        .connection
                        .get_property(
                            false,
                            context.window,
                            event.property,
                            AtomEnum::ANY,
                            0,
                            property_long_length,
                        )
                        .map_err(|error| {
                            format!("failed to read the converted X11 clipboard property: {error}")
                        })?
                        .reply()
                        .map_err(|error| {
                            format!(
                                "failed to receive the converted X11 clipboard property: {error}"
                            )
                        })?;
                    if reply.type_ == context.atoms.incr {
                        incremental_error = if reply.bytes_after != 0 {
                            Some(format!(
                                "X11 INCR clipboard header still has {} unread bytes",
                                reply.bytes_after
                            ))
                        } else {
                            match reply.value32().and_then(|mut values| values.next()) {
                                Some(lower_bound) if lower_bound as usize > max_inline_bytes => {
                                    Some(format!(
                                        "X11 clipboard target is at least {lower_bound} bytes, exceeding the safe inline restore limit of {max_inline_bytes} bytes"
                                    ))
                                }
                                Some(_) => None,
                                None => Some(format!(
                                    "X11 INCR clipboard header uses invalid format {} or contains no size lower bound",
                                    reply.format
                                )),
                            }
                        };
                        context
                            .connection
                            .delete_property(context.window, property)
                            .map_err(|error| {
                                format!(
                                    "failed to acknowledge X11 INCR clipboard transfer: {error}"
                                )
                            })?
                            .check()
                            .map_err(|error| {
                                format!("X11 rejected INCR clipboard acknowledgement: {error}")
                            })?;
                        context.connection.flush().map_err(|error| {
                            format!("failed to flush X11 INCR clipboard acknowledgement: {error}")
                        })?;
                        incremental_transfer = true;
                        incremental = None;
                        continue;
                    }
                    if reply.bytes_after != 0 {
                        return Err(format!(
                            "converted X11 clipboard property still has {} unread bytes",
                            reply.bytes_after
                        ));
                    }
                    if reply.value.len() > max_inline_bytes {
                        return Err(format!(
                            "X11 clipboard target is {} bytes, exceeding the safe inline restore limit of {} bytes",
                            reply.value.len(), max_inline_bytes
                        ));
                    }
                    return x11_clipboard_property_from_reply_parts(
                        requested_target,
                        reply.type_,
                        reply.format,
                        reply.value,
                    );
                }
                Event::PropertyNotify(event)
                    if incremental_transfer
                        && event.window == context.window
                        && event.atom == property
                        && event.state == Property::NEW_VALUE =>
                {
                    let reply = context
                        .connection
                        .get_property(
                            true,
                            context.window,
                            property,
                            AtomEnum::ANY,
                            0,
                            property_long_length,
                        )
                        .map_err(|error| {
                            format!("failed to read an X11 INCR clipboard chunk: {error}")
                        })?
                        .reply()
                        .map_err(|error| {
                            format!("failed to receive an X11 INCR clipboard chunk: {error}")
                        })?;
                    if reply.bytes_after != 0 {
                        incremental_error.get_or_insert_with(|| {
                            format!(
                                "X11 INCR clipboard chunk still has {} unread bytes",
                                reply.bytes_after
                            )
                        });
                        context
                            .connection
                            .delete_property(context.window, property)
                            .map_err(|error| {
                                format!("failed to discard an invalid X11 INCR chunk: {error}")
                            })?
                            .check()
                            .map_err(|error| {
                                format!("X11 rejected invalid INCR chunk cleanup: {error}")
                            })?;
                        incremental = None;
                        continue;
                    }
                    if reply.value.is_empty() {
                        if let Some(error) = incremental_error.take() {
                            return Err(error);
                        }
                        let terminator = x11_clipboard_property_from_reply_parts(
                            requested_target,
                            reply.type_,
                            reply.format,
                            reply.value,
                        )?;
                        return match incremental.take() {
                            Some(value)
                                if value.type_atom != terminator.type_atom
                                    || value.format != terminator.format =>
                            {
                                Err(
                                    "X11 INCR clipboard terminator changed property type or format"
                                        .to_string(),
                                )
                            }
                            Some(value) => Ok(value),
                            None => Ok(terminator),
                        };
                    }
                    if incremental_error.is_some() {
                        continue;
                    }
                    let chunk = match x11_clipboard_property_from_reply_parts(
                        requested_target,
                        reply.type_,
                        reply.format,
                        reply.value,
                    ) {
                        Ok(chunk) => chunk,
                        Err(error) => {
                            incremental_error = Some(error);
                            incremental = None;
                            continue;
                        }
                    };
                    if chunk.bytes.len() > max_inline_bytes {
                        incremental_error = Some(format!(
                            "X11 clipboard target exceeds the safe inline restore limit of {max_inline_bytes} bytes"
                        ));
                        incremental = None;
                        continue;
                    }
                    if let Some(value) = incremental.as_mut() {
                        if value.type_atom != chunk.type_atom || value.format != chunk.format {
                            incremental_error = Some(
                                "X11 INCR clipboard chunks changed property type or format"
                                    .to_string(),
                            );
                            incremental = None;
                            continue;
                        }
                        if value.bytes.len().saturating_add(chunk.bytes.len()) > max_inline_bytes {
                            incremental_error = Some(format!(
                                "X11 clipboard target exceeds the safe inline restore limit of {max_inline_bytes} bytes"
                            ));
                            incremental = None;
                            continue;
                        }
                        value.bytes.extend_from_slice(&chunk.bytes);
                    } else {
                        incremental = Some(chunk);
                    }
                }
                _ => {}
            }
        }
    })();

    let cleanup = context
        .connection
        .delete_property(context.window, property)
        .map_err(|error| error.to_string())
        .and_then(|cookie| cookie.check().map_err(|error| error.to_string()));
    match (result, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(error)) => Err(format!(
            "failed to clean up the X11 clipboard property: {error}"
        )),
        (Err(error), _) => Err(error),
    }
}

struct X11RawClipboard {
    loader: x11_clipboard::Clipboard,
    owner: X11MultiTargetOwner,
}

struct X11MultiTargetOwner {
    context: Arc<x11_clipboard::Context>,
    data: Arc<RwLock<HashMap<u32, X11ClipboardProperty>>>,
    max_inline_bytes: usize,
}

impl X11RawClipboard {
    fn new() -> std::result::Result<Self, String> {
        Ok(Self {
            loader: x11_clipboard::Clipboard::new().map_err(|error| {
                format!("failed to initialize raw X11 clipboard loader: {error}")
            })?,
            owner: X11MultiTargetOwner::new()?,
        })
    }

    fn snapshot_targets(
        &self,
        target_names: &[String],
        deadline: Instant,
    ) -> std::result::Result<Vec<X11RawClipboardTarget>, String> {
        if target_names.len() > X11_CLIPBOARD_MAX_TARGETS {
            return Err(format!(
                "X11 clipboard advertised {} targets, exceeding the limit of {X11_CLIPBOARD_MAX_TARGETS}",
                target_names.len()
            ));
        }
        let mut targets = Vec::new();
        let mut accumulated_bytes = 0usize;
        for name in target_names {
            if x11_clipboard_target_is_protocol(name) {
                continue;
            }
            if Instant::now() >= deadline {
                return Err("X11 clipboard snapshot exceeded its total deadline".to_string());
            }
            let atom = self.loader.getter.get_atom(name).map_err(|error| {
                format!("failed to resolve X11 clipboard target {name:?}: {error}")
            })?;
            let target_limit = x11_clipboard_snapshot_target_limit(
                accumulated_bytes,
                self.owner.max_inline_bytes,
            )?;
            let property = load_x11_clipboard_property(
                &self.loader.getter,
                self.loader.getter.atoms.clipboard,
                atom,
                self.loader.getter.atoms.property,
                deadline,
                target_limit,
            )
            .map_err(|error| {
                format!("failed to snapshot X11 clipboard target {name:?}: {error}")
            })?;
            accumulated_bytes = accumulated_bytes
                .checked_add(property.bytes.len())
                .filter(|total| *total <= X11_CLIPBOARD_MAX_SNAPSHOT_BYTES)
                .ok_or_else(|| {
                    format!(
                        "X11 clipboard snapshot exceeded its aggregate limit of {X11_CLIPBOARD_MAX_SNAPSHOT_BYTES} bytes"
                    )
                })?;
            targets.push(X11RawClipboardTarget {
                name: name.clone(),
                atom,
                property,
            });
        }
        Ok(targets)
    }
}

impl X11MultiTargetOwner {
    fn new() -> std::result::Result<Self, String> {
        use x11rb::connection::RequestConnection;

        let context =
            Arc::new(x11_clipboard::Context::new(None).map_err(|error| {
                format!("failed to initialize raw X11 clipboard owner: {error}")
            })?);
        let data = Arc::new(RwLock::new(HashMap::new()));
        let max_inline_bytes = context
            .connection
            .maximum_request_bytes()
            .saturating_sub(64);
        let thread_context = Arc::clone(&context);
        let thread_data = Arc::clone(&data);
        std::thread::spawn(move || serve_x11_multi_target_clipboard(thread_context, thread_data));
        Ok(Self {
            context,
            data,
            max_inline_bytes,
        })
    }

    fn set_targets(&self, targets: &[X11RawClipboardTarget]) -> std::result::Result<u32, String> {
        use x11rb::{connection::Connection, protocol::xproto::ConnectionExt};

        let mut data = self
            .data
            .write()
            .map_err(|_| "raw X11 clipboard data lock was poisoned".to_string())?;
        data.clear();
        for target in targets {
            if target.property.bytes.len() > self.max_inline_bytes {
                return Err(format!(
                    "X11 clipboard target {:?} exceeds the safe inline restore limit",
                    target.name
                ));
            }
            data.insert(target.atom, target.property.clone());
        }
        drop(data);
        self.context
            .connection
            .set_selection_owner(
                self.context.window,
                self.context.atoms.clipboard,
                x11rb::CURRENT_TIME,
            )
            .map_err(|error| format!("failed to restore X11 clipboard ownership: {error}"))?
            .check()
            .map_err(|error| format!("X11 rejected restored clipboard ownership: {error}"))?;
        self.context.connection.flush().map_err(|error| {
            format!("failed to flush restored X11 clipboard ownership: {error}")
        })?;
        let owner = self
            .context
            .connection
            .get_selection_owner(self.context.atoms.clipboard)
            .map_err(|error| format!("failed to verify restored X11 clipboard owner: {error}"))?
            .reply()
            .map_err(|error| format!("failed to read restored X11 clipboard owner: {error}"))?
            .owner;
        if owner == self.context.window {
            Ok(owner)
        } else {
            Err("raw X11 clipboard owner verification failed".to_string())
        }
    }
}

#[derive(Debug)]
struct PreparedX11Clipboard {
    snapshot: X11ClipboardSnapshot,
    temporary_owner: u32,
}

#[derive(Debug)]
struct X11ClipboardPasteError {
    message: String,
    can_fallback_to_xdotool: bool,
    dispatched: bool,
}

impl X11ClipboardPasteError {
    fn before_paste(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            can_fallback_to_xdotool: true,
            dispatched: false,
        }
    }

    fn preserving_clipboard(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            can_fallback_to_xdotool: false,
            dispatched: false,
        }
    }

    fn after_paste(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            can_fallback_to_xdotool: false,
            dispatched: true,
        }
    }
}

fn x11_clipboard_owner() -> std::result::Result<u32, String> {
    use x11rb::protocol::xproto::ConnectionExt;

    let (connection, _) = x11rb::connect(None)
        .map_err(|error| format!("failed to connect to X11 for clipboard ownership: {error}"))?;
    let clipboard = connection
        .intern_atom(false, b"CLIPBOARD")
        .map_err(|error| format!("failed to request the X11 CLIPBOARD atom: {error}"))?
        .reply()
        .map_err(|error| format!("failed to resolve the X11 CLIPBOARD atom: {error}"))?
        .atom;
    let owner = connection
        .get_selection_owner(clipboard)
        .map_err(|error| format!("failed to request the X11 clipboard owner: {error}"))?
        .reply()
        .map_err(|error| format!("failed to read the X11 clipboard owner: {error}"))?
        .owner;
    Ok(owner)
}

fn x11_clipboard_targets(deadline: Instant) -> std::result::Result<Vec<String>, String> {
    use x11rb::{
        connection::Connection,
        protocol::{xproto::ConnectionExt, Event},
    };

    if Instant::now() >= deadline {
        return Err("X11 clipboard snapshot exceeded its total deadline".to_string());
    }
    let (connection, screen_index) = x11rb::connect(None)
        .map_err(|error| format!("failed to connect to X11 for clipboard targets: {error}"))?;
    let screen = &connection.setup().roots[screen_index];
    let requestor = connection
        .generate_id()
        .map_err(|error| format!("failed to allocate an X11 clipboard request window: {error}"))?;
    connection
        .create_window(
            x11rb::COPY_FROM_PARENT as u8,
            requestor,
            screen.root,
            0,
            0,
            1,
            1,
            0,
            x11rb::protocol::xproto::WindowClass::INPUT_OUTPUT,
            0,
            &x11rb::protocol::xproto::CreateWindowAux::new(),
        )
        .map_err(|error| format!("failed to create an X11 clipboard request window: {error}"))?;
    let clipboard = connection
        .intern_atom(false, b"CLIPBOARD")
        .map_err(|error| format!("failed to request the X11 CLIPBOARD atom: {error}"))?
        .reply()
        .map_err(|error| format!("failed to resolve the X11 CLIPBOARD atom: {error}"))?
        .atom;
    let targets = connection
        .intern_atom(false, b"TARGETS")
        .map_err(|error| format!("failed to request the X11 TARGETS atom: {error}"))?
        .reply()
        .map_err(|error| format!("failed to resolve the X11 TARGETS atom: {error}"))?
        .atom;
    let property = connection
        .intern_atom(false, b"_CODEX_COMPUTER_USE_CLIPBOARD_TARGETS")
        .map_err(|error| format!("failed to request the X11 target property atom: {error}"))?
        .reply()
        .map_err(|error| format!("failed to resolve the X11 target property atom: {error}"))?
        .atom;
    connection
        .convert_selection(requestor, clipboard, targets, property, x11rb::CURRENT_TIME)
        .map_err(|error| format!("failed to request X11 clipboard TARGETS: {error}"))?;
    connection
        .flush()
        .map_err(|error| format!("failed to flush the X11 clipboard TARGETS request: {error}"))?;

    let selection_property = loop {
        if Instant::now() >= deadline {
            let _ = connection.destroy_window(requestor);
            return Err("X11 clipboard TARGETS request timed out".to_string());
        }
        match connection
            .poll_for_event()
            .map_err(|error| format!("failed while waiting for X11 clipboard TARGETS: {error}"))?
        {
            Some(Event::SelectionNotify(event)) if event.requestor == requestor => {
                break event.property;
            }
            Some(_) | None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    if selection_property == x11rb::NONE {
        let _ = connection.destroy_window(requestor);
        return Err("the X11 clipboard owner did not provide TARGETS".to_string());
    }
    let reply = connection
        .get_property(
            false,
            requestor,
            selection_property,
            x11rb::protocol::xproto::AtomEnum::ATOM,
            0,
            x11_clipboard_get_property_long_length(X11_CLIPBOARD_MAX_TARGETS * 4),
        )
        .map_err(|error| format!("failed to read X11 clipboard TARGETS: {error}"))?
        .reply()
        .map_err(|error| format!("failed to receive X11 clipboard TARGETS: {error}"))?;
    if reply.bytes_after != 0 {
        let _ = connection.destroy_window(requestor);
        return Err(format!(
            "X11 clipboard advertised more than {X11_CLIPBOARD_MAX_TARGETS} targets"
        ));
    }
    let atoms = reply
        .value32()
        .ok_or_else(|| "X11 clipboard TARGETS did not contain atoms".to_string())?;
    let mut names = Vec::new();
    for atom in atoms {
        if names.len() >= X11_CLIPBOARD_MAX_TARGETS {
            let _ = connection.destroy_window(requestor);
            return Err(format!(
                "X11 clipboard advertised more than {X11_CLIPBOARD_MAX_TARGETS} targets"
            ));
        }
        if Instant::now() >= deadline {
            let _ = connection.destroy_window(requestor);
            return Err("X11 clipboard snapshot exceeded its total deadline".to_string());
        }
        let name = connection
            .get_atom_name(atom)
            .map_err(|error| format!("failed to request an X11 clipboard target name: {error}"))?
            .reply()
            .map_err(|error| format!("failed to resolve an X11 clipboard target name: {error}"))?
            .name;
        names.push(String::from_utf8_lossy(&name).into_owned());
    }
    let _ = connection.destroy_window(requestor);
    Ok(names)
}

fn x11_clipboard_target_is_protocol(target: &str) -> bool {
    matches!(
        target,
        // ICCCM metadata, batching, transfer, and side-effect targets are not
        // static clipboard formats and must never be replayed during a snapshot.
        "TARGETS"
            | "MULTIPLE"
            | "TIMESTAMP"
            | "INCR"
            | "DELETE"
            | "INSERT_SELECTION"
            | "INSERT_PROPERTY"
            // freedesktop clipboard-manager operation target.
            | "SAVE_TARGETS"
            // Deepin's clipboard manager advertises this ownership marker in
            // TARGETS. It is protocol metadata, not restorable clipboard data.
            | "FROM_DEEPIN_CLIPBOARD_MANAGER"
    )
}

fn serve_x11_multi_target_clipboard(
    context: Arc<x11_clipboard::Context>,
    data: Arc<RwLock<HashMap<u32, X11ClipboardProperty>>>,
) {
    use x11rb::{
        connection::Connection,
        protocol::{
            xproto::{
                ConnectionExt, EventMask, PropMode, SelectionNotifyEvent, SELECTION_NOTIFY_EVENT,
            },
            Event,
        },
        wrapper::ConnectionExt as _,
    };

    while let Ok(event) = context.connection.wait_for_event() {
        let Event::SelectionRequest(event) = event else {
            continue;
        };
        if event.selection != context.atoms.clipboard {
            continue;
        }
        let property = if event.property == x11rb::NONE {
            event.target
        } else {
            event.property
        };
        let response_property = if event.target == context.atoms.targets {
            let targets = data
                .read()
                .map(|data| {
                    std::iter::once(context.atoms.targets)
                        .chain(data.keys().copied())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|_| vec![context.atoms.targets]);
            context
                .connection
                .change_property32(
                    PropMode::REPLACE,
                    event.requestor,
                    property,
                    x11rb::protocol::xproto::AtomEnum::ATOM,
                    &targets,
                )
                .ok()
                .and_then(|cookie| cookie.check().ok())
                .map(|()| property)
        } else {
            data.read()
                .ok()
                .and_then(|data| data.get(&event.target).cloned())
                .and_then(|value| {
                    let item_count =
                        x11_clipboard_property_item_count(value.format, value.bytes.len()).ok()?;
                    context
                        .connection
                        .change_property(
                            PropMode::REPLACE,
                            event.requestor,
                            property,
                            value.type_atom,
                            value.format,
                            item_count,
                            &value.bytes,
                        )
                        .ok()
                        .and_then(|cookie| cookie.check().ok())
                        .map(|()| property)
                })
        }
        .unwrap_or(x11rb::NONE);
        let _ = context.connection.send_event(
            false,
            event.requestor,
            EventMask::NO_EVENT,
            SelectionNotifyEvent {
                response_type: SELECTION_NOTIFY_EVENT,
                sequence: 0,
                time: event.time,
                requestor: event.requestor,
                selection: event.selection,
                target: event.target,
                property: response_property,
            },
        );
        let _ = context.connection.flush();
    }
}

fn with_x11_clipboard<T>(
    state: &Arc<Mutex<Option<arboard::Clipboard>>>,
    operation: impl FnOnce(&mut arboard::Clipboard) -> std::result::Result<T, String>,
) -> std::result::Result<T, String> {
    let mut guard = state
        .lock()
        .map_err(|_| "X11 clipboard state lock was poisoned".to_string())?;
    if guard.is_none() {
        *guard = Some(
            arboard::Clipboard::new()
                .map_err(|error| format!("failed to initialize the X11 clipboard: {error}"))?,
        );
    }
    operation(
        guard
            .as_mut()
            .expect("X11 clipboard is initialized before the operation"),
    )
}

fn with_x11_raw_clipboard<T>(
    state: &Arc<Mutex<Option<X11RawClipboard>>>,
    operation: impl FnOnce(&mut X11RawClipboard) -> std::result::Result<T, String>,
) -> std::result::Result<T, String> {
    let mut guard = state
        .lock()
        .map_err(|_| "raw X11 clipboard state lock was poisoned".to_string())?;
    if guard.is_none() {
        *guard = Some(X11RawClipboard::new()?);
    }
    operation(
        guard
            .as_mut()
            .expect("raw X11 clipboard is initialized before the operation"),
    )
}

fn restore_x11_clipboard_snapshot(
    clipboard: &mut arboard::Clipboard,
    raw_state: &Arc<Mutex<Option<X11RawClipboard>>>,
    snapshot: &X11ClipboardSnapshot,
) -> std::result::Result<(), String> {
    match snapshot {
        X11ClipboardSnapshot::Empty => clipboard
            .clear()
            .map_err(|error| format!("failed to restore an empty X11 clipboard: {error}")),
        X11ClipboardSnapshot::Raw(targets) => {
            with_x11_raw_clipboard(raw_state, |raw| raw.owner.set_targets(targets)).map(|_| ())
        }
    }
}

fn restore_x11_clipboard_snapshot_if_owner_matches(
    clipboard: &mut arboard::Clipboard,
    raw_state: &Arc<Mutex<Option<X11RawClipboard>>>,
    snapshot: &X11ClipboardSnapshot,
    expected_temporary_owner: u32,
) -> std::result::Result<ClipboardRestoreDecision, String> {
    let current_owner = x11_clipboard_owner().map_err(|error| {
        format!("failed to verify the temporary X11 clipboard owner before restoration: {error}")
    })?;
    let decision = clipboard_restore_decision_for_known_owner(
        Some(expected_temporary_owner),
        Ok(current_owner),
    );
    if decision == ClipboardRestoreDecision::RestorePrevious {
        restore_x11_clipboard_snapshot(clipboard, raw_state, snapshot)?;
    }
    Ok(decision)
}

fn x11_clipboard_error_after_failed_verification(
    clipboard: &mut arboard::Clipboard,
    raw_state: &Arc<Mutex<Option<X11RawClipboard>>>,
    snapshot: &X11ClipboardSnapshot,
    expected_temporary_owner: u32,
    message: impl Into<String>,
) -> X11ClipboardPasteError {
    let message = message.into();
    match restore_x11_clipboard_snapshot_if_owner_matches(
        clipboard,
        raw_state,
        snapshot,
        expected_temporary_owner,
    ) {
        Ok(ClipboardRestoreDecision::RestorePrevious) => {
            X11ClipboardPasteError::before_paste(message)
        }
        Ok(ClipboardRestoreDecision::PreserveCurrent) => {
            X11ClipboardPasteError::preserving_clipboard(format!(
                "{message}; the X11 clipboard owner changed, so the newer contents were preserved"
            ))
        }
        Ok(ClipboardRestoreDecision::Unreadable) => {
            X11ClipboardPasteError::preserving_clipboard(format!(
                "{message}; the X11 clipboard owner could not be verified, so it was left unchanged"
            ))
        }
        Err(restore_error) => {
            X11ClipboardPasteError::preserving_clipboard(format!("{message}; {restore_error}"))
        }
    }
}

fn prepare_x11_clipboard_text(
    state: &Arc<Mutex<Option<arboard::Clipboard>>>,
    raw_state: &Arc<Mutex<Option<X11RawClipboard>>>,
    temporary: &str,
) -> std::result::Result<PreparedX11Clipboard, X11ClipboardPasteError> {
    let previous_owner = x11_clipboard_owner().map_err(X11ClipboardPasteError::before_paste)?;
    let snapshot = if previous_owner == x11rb::NONE {
        X11ClipboardSnapshot::Empty
    } else {
        let snapshot_deadline = Instant::now() + X11_CLIPBOARD_SNAPSHOT_TIMEOUT;
        let target_names = x11_clipboard_targets(snapshot_deadline)
            .map_err(X11ClipboardPasteError::preserving_clipboard)?;
        let raw_targets = with_x11_raw_clipboard(raw_state, |raw| {
            raw.snapshot_targets(&target_names, snapshot_deadline)
        })
        .map_err(X11ClipboardPasteError::preserving_clipboard)?;
        X11ClipboardSnapshot::Raw(raw_targets)
    };
    let owner_after_snapshot =
        x11_clipboard_owner().map_err(X11ClipboardPasteError::preserving_clipboard)?;
    if owner_after_snapshot != previous_owner {
        return Err(X11ClipboardPasteError::preserving_clipboard(
            "the X11 clipboard owner changed while its contents were being snapshotted; preserving the newer clipboard",
        ));
    }
    let mut guard = state.lock().map_err(|_| {
        X11ClipboardPasteError::before_paste("X11 clipboard state lock was poisoned")
    })?;
    if guard.is_none() {
        *guard = Some(arboard::Clipboard::new().map_err(|error| {
            X11ClipboardPasteError::before_paste(format!(
                "failed to initialize the X11 clipboard: {error}"
            ))
        })?);
    }
    let clipboard = guard
        .as_mut()
        .expect("X11 clipboard is initialized before preparation");
    if let Err(error) = clipboard.set_text(temporary.to_string()) {
        let message = format!("failed to set temporary X11 clipboard text: {error}");
        let owner_after_write = x11_clipboard_owner();
        let temporary_visible = clipboard
            .get_text()
            .is_ok_and(|actual| clipboard_write_was_verified(temporary, &actual));
        return match owner_after_write {
            Ok(owner) if temporary_visible => Err(x11_clipboard_error_after_failed_verification(
                clipboard, raw_state, &snapshot, owner, message,
            )),
            Ok(owner) if owner == previous_owner => {
                Err(X11ClipboardPasteError::before_paste(message))
            }
            Ok(_) => Err(X11ClipboardPasteError::preserving_clipboard(format!(
                "{message}; another owner replaced the X11 clipboard, so its contents were preserved"
            ))),
            Err(owner_error) => Err(X11ClipboardPasteError::preserving_clipboard(format!(
                "{message}; failed to identify the current X11 clipboard owner: {owner_error}"
            ))),
        };
    }
    let temporary_owner_before_readback = match x11_clipboard_owner() {
        Ok(owner) => owner,
        Err(owner_error) => {
            return Err(X11ClipboardPasteError::preserving_clipboard(format!(
                "failed to identify the temporary X11 clipboard owner before verification: {owner_error}; the clipboard was left unchanged because ownership could not be verified"
            )));
        }
    };
    let read_back = clipboard
        .get_text()
        .map_err(|error| format!("failed to verify temporary X11 clipboard text: {error}"));
    match read_back {
        Ok(actual) if clipboard_write_was_verified(temporary, &actual) => {
            let temporary_owner_after_readback = x11_clipboard_owner().map_err(|owner_error| {
                X11ClipboardPasteError::preserving_clipboard(format!(
                    "failed to verify the temporary X11 clipboard owner after read-back: {owner_error}; the clipboard was left unchanged because ownership could not be verified"
                ))
            })?;
            if temporary_owner_after_readback != temporary_owner_before_readback {
                return Err(X11ClipboardPasteError::preserving_clipboard(
                    "the X11 clipboard owner changed while temporary text was being verified; preserving the newer clipboard",
                ));
            }
            Ok(PreparedX11Clipboard {
                snapshot,
                temporary_owner: temporary_owner_after_readback,
            })
        }
        Ok(_) => Err(x11_clipboard_error_after_failed_verification(
            clipboard,
            raw_state,
            &snapshot,
            temporary_owner_before_readback,
            "temporary X11 clipboard text did not match its read-back",
        )),
        Err(message) => Err(x11_clipboard_error_after_failed_verification(
            clipboard,
            raw_state,
            &snapshot,
            temporary_owner_before_readback,
            message,
        )),
    }
}

fn finish_x11_clipboard_text(
    state: &Arc<Mutex<Option<arboard::Clipboard>>>,
    raw_state: &Arc<Mutex<Option<X11RawClipboard>>>,
    prepared: &PreparedX11Clipboard,
) -> std::result::Result<String, String> {
    with_x11_clipboard(state, |clipboard| {
        match clipboard_restore_decision(
            prepared.temporary_owner,
            x11_clipboard_owner().map_err(|_| ()),
        ) {
            ClipboardRestoreDecision::RestorePrevious => {
                restore_x11_clipboard_snapshot(clipboard, raw_state, &prepared.snapshot)?;
                Ok("Previous X11 clipboard contents were restored.".to_string())
            }
            ClipboardRestoreDecision::PreserveCurrent => Ok(
                "The X11 clipboard changed after paste, so the newer contents were preserved."
                    .to_string(),
            ),
            ClipboardRestoreDecision::Unreadable => Ok(
                "Warning: the X11 clipboard became unreadable or non-text after paste; it was left unchanged to avoid overwriting newer contents."
                    .to_string(),
            ),
        }
    })
}

fn x11_paste_chord(terminal_target: bool) -> &'static str {
    if terminal_target {
        "ctrl+shift+v"
    } else {
        "ctrl+v"
    }
}

fn window_uses_terminal_paste(window: &WindowInfo) -> bool {
    if window.terminal.is_some() {
        return true;
    }
    [
        window.app_id.as_deref(),
        window.wm_class.as_deref(),
        window.title.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::to_ascii_lowercase)
    .any(|value| {
        [
            "terminal",
            "xterm",
            "konsole",
            "kitty",
            "alacritty",
            "wezterm",
            "tilix",
            "terminator",
        ]
        .iter()
        .any(|marker| value.contains(marker))
    })
}

async fn run_x11_clipboard_paste_text(
    state: &Arc<Mutex<Option<arboard::Clipboard>>>,
    raw_state: &Arc<Mutex<Option<X11RawClipboard>>>,
    text: &str,
    terminal_target: bool,
) -> std::result::Result<String, X11ClipboardPasteError> {
    let prepare_state = Arc::clone(state);
    let prepare_raw_state = Arc::clone(raw_state);
    let temporary = text.to_string();
    let prepare_text = temporary.clone();
    let prepared = tokio::task::spawn_blocking(move || {
        prepare_x11_clipboard_text(&prepare_state, &prepare_raw_state, &prepare_text)
    })
    .await
    .map_err(|error| {
        X11ClipboardPasteError::before_paste(format!(
            "X11 clipboard preparation task failed: {error}"
        ))
    })??;

    let expected_owner = prepared.temporary_owner;
    let current_owner = tokio::task::spawn_blocking(x11_clipboard_owner)
        .await
        .map_err(|error| {
            X11ClipboardPasteError::preserving_clipboard(format!(
                "X11 clipboard owner verification task failed: {error}"
            ))
        })?
        .map_err(X11ClipboardPasteError::preserving_clipboard)?;
    if current_owner != expected_owner {
        return Err(X11ClipboardPasteError::preserving_clipboard(
            "the X11 clipboard owner changed before paste; preserving the newer clipboard",
        ));
    }

    let paste_result = run_xdotool(&[
        "key".to_string(),
        "--clearmodifiers".to_string(),
        x11_paste_chord(terminal_target).to_string(),
    ])
    .await;

    if let Err(error) = paste_result {
        let restore_state = Arc::clone(state);
        let restore_raw_state = Arc::clone(raw_state);
        let restore = tokio::task::spawn_blocking(move || {
            finish_x11_clipboard_text(&restore_state, &restore_raw_state, &prepared)
        })
        .await
        .map_err(|join_error| format!("X11 clipboard restore task failed: {join_error}"))
        .and_then(|result| result);
        let message = match restore {
            Ok(note) => format!("{error}; {note}"),
            Err(restore_error) => format!("{error}; {restore_error}"),
        };
        return Err(if error.starts_with("failed to run xdotool:") {
            X11ClipboardPasteError::preserving_clipboard(message)
        } else {
            X11ClipboardPasteError::after_paste(message)
        });
    }

    sleep(kde_clipboard_restore_delay(text)).await;
    let finish_state = Arc::clone(state);
    let finish_raw_state = Arc::clone(raw_state);
    let finish = tokio::task::spawn_blocking(move || {
        finish_x11_clipboard_text(&finish_state, &finish_raw_state, &prepared)
    })
    .await
    .map_err(|error| format!("X11 clipboard restore task failed: {error}"))
    .and_then(|result| result);

    match finish {
        Ok(note) => Ok(format!(
            "Action pasted through the X11 clipboard using {}. {note}",
            x11_paste_chord(terminal_target)
        )),
        Err(error) => Ok(format!(
            "Action pasted through the X11 clipboard using {}. Warning: {error}",
            x11_paste_chord(terminal_target)
        )),
    }
}

const EVDEV_KEY_LEFTCTRL: i32 = 29;
const EVDEV_KEY_V: i32 = 47;
const KDE_CLIPBOARD_RESTORE_MIN_DELAY_MS: u64 = 1_500;
const KDE_CLIPBOARD_RESTORE_MAX_DELAY_MS: u64 = 5_000;
const KDE_CLIPBOARD_RESTORE_CHARS_PER_SECOND: u64 = 250;

fn kde_clipboard_restore_delay(text: &str) -> Duration {
    let text_delay_ms = (text.chars().count() as u64)
        .saturating_mul(1_000)
        .div_ceil(KDE_CLIPBOARD_RESTORE_CHARS_PER_SECOND);
    Duration::from_millis(text_delay_ms.clamp(
        KDE_CLIPBOARD_RESTORE_MIN_DELAY_MS,
        KDE_CLIPBOARD_RESTORE_MAX_DELAY_MS,
    ))
}

#[derive(Debug)]
struct KdeClipboardPasteError {
    message: String,
    can_fallback_to_ydotool: bool,
    clear_portal_keyboard_session: bool,
}

impl KdeClipboardPasteError {
    fn before_text_input(message: String) -> Self {
        Self {
            message,
            can_fallback_to_ydotool: true,
            clear_portal_keyboard_session: false,
        }
    }

    fn after_portal_input(message: String) -> Self {
        Self {
            message,
            can_fallback_to_ydotool: false,
            clear_portal_keyboard_session: true,
        }
    }
}

async fn run_kde_clipboard_paste_text(
    session: &PortalKeyboardSession,
    text: &str,
) -> std::result::Result<String, KdeClipboardPasteError> {
    let previous = kde_clipboard_contents()
        .await
        .map_err(KdeClipboardPasteError::before_text_input)?;
    kde_set_clipboard_contents(text)
        .await
        .map_err(KdeClipboardPasteError::before_text_input)?;

    let paste_result = press_keycode_chord(session, &[EVDEV_KEY_LEFTCTRL], EVDEV_KEY_V)
        .await
        .map_err(|error| format!("{error:#}"));

    sleep(kde_clipboard_restore_delay(text)).await;
    let restore_result = kde_set_clipboard_contents(&previous).await;

    match (paste_result, restore_result) {
        (Ok(_), Ok(_)) => Ok("Action pasted through KDE clipboard integration.".to_string()),
        (Err(error), Ok(_)) => Err(KdeClipboardPasteError::after_portal_input(error)),
        (Ok(_), Err(restore_error)) => Ok(format!(
            "Action pasted through KDE clipboard integration. Warning: previous KDE clipboard contents could not be restored: {restore_error}"
        )),
        (Err(error), Err(restore_error)) => Err(KdeClipboardPasteError::after_portal_input(
            format!("{error}; previous KDE clipboard contents could not be restored: {restore_error}"),
        )),
    }
}

async fn kde_clipboard_contents() -> std::result::Result<String, String> {
    let connection = kde_clipboard_connection().await?;
    let proxy = kde_clipboard_proxy(&connection).await?;
    let output: String = kde_clipboard_dbus_operation(
        "getClipboardContents",
        proxy.call("getClipboardContents", &()),
    )
    .await?;
    Ok(output)
}

async fn kde_set_clipboard_contents(text: &str) -> std::result::Result<(), String> {
    let connection = kde_clipboard_connection().await?;
    let proxy = kde_clipboard_proxy(&connection).await?;
    let _: () = kde_clipboard_dbus_operation(
        "setClipboardContents",
        proxy.call("setClipboardContents", &(text)),
    )
    .await?;
    Ok(())
}

async fn kde_clipboard_connection() -> std::result::Result<ZbusConnection, String> {
    ZbusConnection::session()
        .await
        .map_err(|error| format!("failed to connect to session bus for KDE clipboard: {error}"))
}

async fn kde_clipboard_proxy(
    connection: &ZbusConnection,
) -> std::result::Result<ZbusProxy<'_>, String> {
    kde_clipboard_dbus_operation(
        "proxy creation",
        ZbusProxy::new(
            connection,
            KDE_KLIPPER_SERVICE,
            KDE_KLIPPER_PATH,
            KDE_KLIPPER_INTERFACE,
        ),
    )
    .await
}

async fn kde_clipboard_dbus_operation<T, F>(
    operation: &'static str,
    future: F,
) -> std::result::Result<T, String>
where
    F: Future<Output = zbus::Result<T>>,
{
    kde_clipboard_dbus_operation_with_timeout(operation, future, KDE_CLIPBOARD_DBUS_TIMEOUT).await
}

async fn kde_clipboard_dbus_operation_with_timeout<T, F>(
    operation: &'static str,
    future: F,
    timeout_duration: Duration,
) -> std::result::Result<T, String>
where
    F: Future<Output = zbus::Result<T>>,
{
    timeout(timeout_duration, future)
        .await
        .map_err(|_| format!("KDE clipboard {operation} timed out"))?
        .map_err(|error| format!("KDE clipboard {operation} failed: {error}"))
}

fn ydotool_output_error(output: Output) -> String {
    command_output_error("ydotool", output)
}

fn command_output_error(command: &str, output: Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = if stderr.is_empty() { stdout } else { stderr };
    if detail.is_empty() {
        format!("{command} exited with {}", output.status)
    } else {
        detail
    }
}

fn ydotool_socket() -> Option<String> {
    if let Some(socket) = explicit_ydotool_socket() {
        return Some(socket);
    }

    connectable_ydotool_socket_from(fallback_ydotool_socket_candidates())
        .map(|path| path.display().to_string())
}

fn explicit_ydotool_socket() -> Option<String> {
    if let Ok(socket) = env::var("YDOTOOL_SOCKET") {
        let socket = socket.trim();
        if !socket.is_empty() {
            return Some(socket.to_string());
        }
    }
    None
}

fn fallback_ydotool_socket_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(runtime) = env::var("XDG_RUNTIME_DIR")
        .ok()
        .map(PathBuf::from)
        .or_else(|| user_id().map(|uid| PathBuf::from(format!("/run/user/{uid}"))))
    {
        candidates.push(runtime.join(".ydotool_socket"));
    }
    candidates.push(PathBuf::from("/tmp/.ydotool_socket"));
    candidates
}

fn connectable_ydotool_socket_from(candidates: Vec<PathBuf>) -> Option<PathBuf> {
    candidates.into_iter().find(ydotool_socket_connects)
}

fn ydotool_socket_connects(path: &PathBuf) -> bool {
    UnixStream::connect(path).is_ok()
        || UnixDatagram::unbound()
            .and_then(|socket| socket.connect(path))
            .is_ok()
}

fn mouse_button_code(button: Option<&str>) -> String {
    match button.unwrap_or("left").to_ascii_lowercase().as_str() {
        "right" => "0xC1",
        "middle" => "0xC2",
        "side" => "0xC3",
        "extra" => "0xC4",
        "forward" => "0xC5",
        "back" => "0xC6",
        _ => "0xC0",
    }
    .to_string()
}

fn key_sequence(key: &str) -> Option<Vec<String>> {
    let parts = key
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let (key_part, modifier_parts) = parts.split_last()?;
    if modifier_parts.is_empty() {
        if let Some(modifier) = modifier_keycode(key_part) {
            return Some(vec![format!("{modifier}:1"), format!("{modifier}:0")]);
        }
    }
    let mut modifiers = Vec::new();
    for part in modifier_parts {
        modifiers.push(modifier_keycode(part)?);
    }
    let keycode = keycode(key_part)?;

    let mut events = Vec::new();
    for modifier in &modifiers {
        events.push(format!("{modifier}:1"));
    }
    events.push(format!("{keycode}:1"));
    events.push(format!("{keycode}:0"));
    for modifier in modifiers.iter().rev() {
        events.push(format!("{modifier}:0"));
    }
    Some(events)
}

fn xdotool_key_chord(key: &str) -> Option<String> {
    let parts = key
        .split('+')
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .map(xdotool_key_name)
        .collect::<Option<Vec<_>>>()?;
    (!parts.is_empty()).then(|| parts.join("+"))
}

fn xdotool_key_name(key: &str) -> Option<String> {
    let normalized = key
        .chars()
        .filter(|character| !character.is_ascii_whitespace() && *character != '-')
        .collect::<String>()
        .to_ascii_lowercase();
    let name = match normalized.as_str() {
        "ctrl" | "control" => "ctrl",
        "alt" | "option" => "alt",
        "shift" => "shift",
        "meta" | "super" | "cmd" | "command" => "super",
        "enter" | "return" => "Return",
        "escape" | "esc" => "Escape",
        "tab" => "Tab",
        "backspace" => "BackSpace",
        "delete" | "del" => "Delete",
        "space" => "space",
        "home" => "Home",
        "end" => "End",
        "pageup" => "Page_Up",
        "pagedown" => "Page_Down",
        "arrowleft" | "left" => "Left",
        "arrowright" | "right" => "Right",
        "arrowup" | "up" => "Up",
        "arrowdown" | "down" => "Down",
        value
            if value.len() == 1
                && value
                    .as_bytes()
                    .first()
                    .is_some_and(|byte| byte.is_ascii_alphanumeric()) =>
        {
            return Some(value.to_string());
        }
        value
            if value.len() <= 3
                && value.starts_with('f')
                && value[1..]
                    .parse::<u8>()
                    .ok()
                    .is_some_and(|number| (1..=12).contains(&number)) =>
        {
            return Some(value.to_ascii_uppercase());
        }
        _ => return None,
    };
    Some(name.to_string())
}

fn modifier_keycode(key: &str) -> Option<u16> {
    match normalize_key(key).as_str() {
        "ctrl" | "control" => Some(29),
        "alt" | "option" => Some(56),
        "shift" => Some(42),
        "meta" | "super" | "cmd" | "command" => Some(125),
        _ => None,
    }
}

fn keycode(key: &str) -> Option<u16> {
    match normalize_key(key).as_str() {
        "enter" | "return" => Some(28),
        "escape" | "esc" => Some(1),
        "tab" => Some(15),
        "backspace" => Some(14),
        "delete" | "del" => Some(111),
        "space" => Some(57),
        "home" => Some(102),
        "end" => Some(107),
        "pageup" | "page_up" => Some(104),
        "pagedown" | "page_down" => Some(109),
        "arrowleft" | "left" => Some(105),
        "arrowright" | "right" => Some(106),
        "arrowup" | "up" => Some(103),
        "arrowdown" | "down" => Some(108),
        "f1" => Some(59),
        "f2" => Some(60),
        "f3" => Some(61),
        "f4" => Some(62),
        "f5" => Some(63),
        "f6" => Some(64),
        "f7" => Some(65),
        "f8" => Some(66),
        "f9" => Some(67),
        "f10" => Some(68),
        "f11" => Some(87),
        "f12" => Some(88),
        value if value.len() == 1 => keycode_for_ascii(value.as_bytes()[0] as char),
        _ => None,
    }
}

fn normalize_key(key: &str) -> String {
    key.trim().to_ascii_lowercase().replace(['-', ' '], "")
}

fn keycode_for_ascii(value: char) -> Option<u16> {
    match value {
        'a' => Some(30),
        'b' => Some(48),
        'c' => Some(46),
        'd' => Some(32),
        'e' => Some(18),
        'f' => Some(33),
        'g' => Some(34),
        'h' => Some(35),
        'i' => Some(23),
        'j' => Some(36),
        'k' => Some(37),
        'l' => Some(38),
        'm' => Some(50),
        'n' => Some(49),
        'o' => Some(24),
        'p' => Some(25),
        'q' => Some(16),
        'r' => Some(19),
        's' => Some(31),
        't' => Some(20),
        'u' => Some(22),
        'v' => Some(47),
        'w' => Some(17),
        'x' => Some(45),
        'y' => Some(21),
        'z' => Some(44),
        '1' => Some(2),
        '2' => Some(3),
        '3' => Some(4),
        '4' => Some(5),
        '5' => Some(6),
        '6' => Some(7),
        '7' => Some(8),
        '8' => Some(9),
        '9' => Some(10),
        '0' => Some(11),
        _ => None,
    }
}

fn user_id() -> Option<String> {
    let output = Command::new("id").arg("-u").output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
}

fn list_process_apps() -> Vec<AppCandidate> {
    let output = Command::new("ps")
        .args(["-eo", "pid=,comm=,args="])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_process_line)
        .filter(|app| looks_like_desktop_app(&app.name, &app.command))
        .take(50)
        .collect()
}

fn parse_process_line(line: &str) -> Option<AppCandidate> {
    let trimmed = line.trim();
    let mut parts = trimmed.splitn(3, char::is_whitespace);
    let pid = parts.next()?.parse().ok()?;
    let name = parts.next()?.to_string();
    let command = parts.next().unwrap_or("").trim().to_string();
    Some(AppCandidate { name, pid, command })
}

fn looks_like_desktop_app(name: &str, command: &str) -> bool {
    let haystack = format!("{name} {command}").to_ascii_lowercase();
    [
        "codex",
        "electron",
        "chrome",
        "chromium",
        "firefox",
        "brave",
        "code",
        "gnome-terminal",
        "ptyxis",
        "kgx",
        "nautilus",
        "slack",
        "discord",
        "spotify",
        "obsidian",
    ]
    .iter()
    .any(|needle| haystack.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atspi_tree::{AccessibilityAction, Bounds};
    use crate::windows::{WindowBounds, GNOME_SHELL_EXTENSION_BACKEND};

    struct EnvVarGuard {
        key: &'static str,
        original: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, original }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    fn node(index: u32, bounds: Option<Bounds>) -> AccessibilityNode {
        node_with_actions(index, bounds, Vec::new())
    }

    fn node_with_actions(
        index: u32,
        bounds: Option<Bounds>,
        actions: Vec<AccessibilityAction>,
    ) -> AccessibilityNode {
        AccessibilityNode {
            index,
            parent_index: None,
            depth: 0,
            object_ref: format!(":1.{index}/org/a11y/atspi/accessible/{index}"),
            role: "push button".to_string(),
            name: Some(format!("Button {index}")),
            description: None,
            child_count: 0,
            bounds,
            states: Vec::new(),
            actions,
            value: None,
            text: None,
            supports_editable_text: false,
        }
    }

    fn click_action() -> AccessibilityAction {
        AccessibilityAction {
            index: 0,
            name: "Click".to_string(),
            description: "Clicks the element".to_string(),
            keybinding: String::new(),
        }
    }

    fn solid_png(width: u32, height: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(width, height, image::Rgba([32, 128, 192, 255]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    fn solid_jpeg(width: u32, height: u32) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(width, height, image::Rgb([32, 128, 192]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(
                &mut std::io::Cursor::new(&mut out),
                image::ImageFormat::Jpeg,
            )
            .unwrap();
        out
    }

    #[test]
    fn get_app_state_returns_native_image_without_embedding_it_in_metadata() {
        let diagnostics = doctor_report();
        let capture = prepare_screenshot_payload(
            RawScreenshotCapture {
                bytes: solid_png(16, 16),
                source: "test".to_string(),
                width: 16,
                height: 16,
            },
            ScreenshotPayloadOptions::default(),
        )
        .unwrap();
        let screenshot_id = capture.screenshot_id.clone();
        let expected_image_data = data_url_payload(&capture.data_url);
        let output = GetAppStateOutput {
            app_name_or_bundle_identifier: Some("demo".to_string()),
            window_context: None,
            window_error: None,
            window_permissions_hint: None,
            backend: "linux-atspi".to_string(),
            screenshot: None,
            screenshot_error: None,
            accessibility_tree: Vec::new(),
            accessibility_tree_raw_count: 0,
            accessibility_error: None,
            readiness: diagnostics.readiness,
            diagnostics: None,
            message: "test".to_string(),
        };

        let mut output_without_screenshot = output.clone();
        output_without_screenshot.screenshot_error = Some("capture unavailable".to_string());
        let result_without_screenshot =
            get_app_state_call_result(output_without_screenshot, None).unwrap();
        assert_eq!(result_without_screenshot.content.len(), 1);
        assert!(result_without_screenshot
            .content
            .iter()
            .all(|content| content.raw.as_image().is_none()));
        let structured_without_screenshot = result_without_screenshot
            .structured_content
            .as_ref()
            .unwrap();
        assert!(structured_without_screenshot["screenshot"].is_null());
        assert_eq!(
            structured_without_screenshot["screenshot_error"],
            "capture unavailable"
        );

        let result = get_app_state_call_result(output, Some(capture)).unwrap();
        let images = result
            .content
            .iter()
            .filter_map(|content| content.raw.as_image())
            .collect::<Vec<_>>();
        let texts = result
            .content
            .iter()
            .filter_map(|content| content.raw.as_text())
            .collect::<Vec<_>>();
        let structured = result.structured_content.as_ref().unwrap();
        let structured_json = structured.to_string();

        assert_eq!(images.len(), 1);
        assert_eq!(images[0].mime_type, "image/png");
        assert_eq!(images[0].data, expected_image_data);
        assert_eq!(texts.len(), 1);
        let text_json: serde_json::Value = serde_json::from_str(&texts[0].text).unwrap();
        assert_eq!(&text_json, structured);
        assert!(!texts[0].text.contains("data:image"));
        assert!(!texts[0].text.contains("data_url"));
        assert!(!structured_json.contains("data:image"));
        assert!(!structured_json.contains("data_url"));
        assert!(!structured_json.contains(&expected_image_data));
        assert!(structured["screenshot"].get("format").is_none());
        assert!(structured["screenshot"].get("quality").is_none());
        assert!(structured["screenshot"].get("scale").is_none());
        assert_eq!(structured["screenshot"]["screenshot_id"], screenshot_id);
        assert_eq!(structured["screenshot"]["width"], 16);
        assert_eq!(structured["screenshot"]["coordinate_width"], 16);
        assert_eq!(structured["screenshot"]["coordinate_origin_x"], 0);
        assert!(structured.get("readiness").is_some());
        assert!(structured.get("accessibility_tree").is_some());
        assert!(structured_json.len() < 128 * 1024);

        let tools = ComputerUseLinux::tool_router().list_all();
        let get_app_state_tool = tools
            .iter()
            .find(|tool| tool.name.as_ref() == "get_app_state")
            .unwrap();
        let output_schema_json =
            serde_json::to_string(get_app_state_tool.output_schema.as_ref().unwrap()).unwrap();
        assert!(!output_schema_json.contains("data_url"));
        assert!(!output_schema_json.contains("dataUrl"));
    }

    #[test]
    fn screenshot_tool_schemas_expose_constraints_instead_of_encoding_choices() {
        let tools = ComputerUseLinux::tool_router().list_all();
        let input_schema = |name: &str| {
            let tool = tools
                .iter()
                .find(|tool| tool.name.as_ref() == name)
                .unwrap_or_else(|| panic!("missing tool {name}"));
            serde_json::Value::Object(tool.input_schema.as_ref().clone())
        };

        for name in ["get_app_state", "screenshot"] {
            let schema = input_schema(name);
            let properties = schema["properties"].as_object().unwrap();
            assert!(properties.contains_key("max_bytes"));
            assert!(properties.contains_key("max_width"));
            assert!(properties.contains_key("max_height"));
            assert!(!properties.contains_key("format"));
            assert!(!properties.contains_key("quality"));
            assert!(!properties.contains_key("scale"));
        }

        let compressed = input_schema("screenshot_compressed");
        let properties = compressed["properties"].as_object().unwrap();
        assert!(properties.contains_key("quality"));
        assert!(properties.contains_key("max_bytes"));
        assert!(properties.contains_key("max_width"));
        assert!(properties.contains_key("max_height"));
        assert!(!properties.contains_key("format"));
        assert!(!properties.contains_key("scale"));
    }

    #[test]
    fn coordinate_action_schemas_have_no_implicit_coordinate_space() {
        let tools = ComputerUseLinux::tool_router().list_all();
        let properties = |name: &str| {
            tools
                .iter()
                .find(|tool| tool.name.as_ref() == name)
                .unwrap_or_else(|| panic!("missing tool {name}"))
                .input_schema
                .get("properties")
                .and_then(serde_json::Value::as_object)
                .cloned()
                .unwrap()
        };

        let semantic = properties("click");
        assert!(!semantic.contains_key("x"));
        assert!(!semantic.contains_key("y"));
        assert!(!semantic.contains_key("coordinate_space"));
        assert!(!semantic.contains_key("screenshot_id"));

        for name in ["click_screenshot", "drag_screenshot", "scroll_screenshot"] {
            let schema = properties(name);
            assert!(schema.contains_key("screenshot_id"));
            assert!(!schema.contains_key("coordinate_space"));
        }
        for name in ["click_desktop", "drag_desktop", "scroll_desktop"] {
            let schema = properties(name);
            assert!(!schema.contains_key("screenshot_id"));
            assert!(!schema.contains_key("coordinate_space"));
        }
    }

    #[test]
    fn visual_locator_tools_require_a_screenshot_id() {
        let tools = ComputerUseLinux::tool_router().list_all();
        for name in ["locate_text", "locate_control"] {
            let tool = tools
                .iter()
                .find(|tool| tool.name.as_ref() == name)
                .unwrap_or_else(|| panic!("missing tool {name}"));
            let properties = tool.input_schema["properties"].as_object().unwrap();
            assert!(properties.contains_key("screenshot_id"));
            assert!(properties.contains_key("text"));
        }
    }

    #[test]
    fn verified_click_tools_accept_target_ids_not_coordinates() {
        let tools = ComputerUseLinux::tool_router().list_all();
        for name in ["click_target", "click_and_verify"] {
            let tool = tools
                .iter()
                .find(|tool| tool.name.as_ref() == name)
                .unwrap_or_else(|| panic!("missing tool {name}"));
            let properties = tool.input_schema["properties"].as_object().unwrap();
            assert!(properties.contains_key("target_id"));
            assert!(!properties.contains_key("x"));
            assert!(!properties.contains_key("y"));
            assert!(!properties.contains_key("screenshot_id"));
        }
    }

    #[test]
    fn visual_text_matching_ignores_case_and_ocr_whitespace() {
        assert!(normalize_visual_text("公益站 自 动签到")
            .contains(&normalize_visual_text("公益站自动签到")));
        assert_eq!(normalize_visual_text("Save NOW"), "savenow");
    }

    #[test]
    fn inferred_control_bounds_are_expanded_and_clipped() {
        assert_eq!(
            expand_visual_bounds(
                ImageBounds {
                    x: 2,
                    y: 3,
                    width: 20,
                    height: 10
                },
                100,
                50,
                Some("button"),
            ),
            Some(ImageBounds {
                x: 0,
                y: 0,
                width: 32,
                height: 19
            })
        );
    }

    #[test]
    fn window_crop_happens_before_screenshot_payload_resize() {
        let (cropped, width, height) =
            crop_image_to_png(&solid_png(400, 200), 50, 20, 200, 100).unwrap();
        let capture = prepare_screenshot_payload(
            RawScreenshotCapture {
                bytes: cropped,
                source: "test".to_string(),
                width,
                height,
            },
            ScreenshotPayloadOptions {
                max_width: Some(100),
                max_height: Some(100),
                max_bytes: Some(1024 * 1024),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            (capture.coordinate_width, capture.coordinate_height),
            (200, 100)
        );
        assert_eq!((capture.width, capture.height), (100, 50));
        assert!(capture.resized);
    }

    #[test]
    fn targeted_get_app_state_returns_only_the_strict_window_crop() {
        let mut window = window_with_bounds(7, 50, 20, 200, 100);
        window.backend = crate::windowing::XDOTOOL_BACKEND.to_string();
        let (capture, snapshot) = prepare_get_app_state_capture(
            RawScreenshotCapture {
                bytes: solid_png(400, 200),
                source: "test".to_string(),
                width: 400,
                height: 200,
            },
            Some(&window),
            ScreenshotPayloadOptions {
                max_width: Some(400),
                max_height: Some(200),
                max_bytes: Some(1024 * 1024),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!((capture.width, capture.height), (200, 100));
        assert_eq!(
            (capture.coordinate_origin_x, capture.coordinate_origin_y),
            (50, 20)
        );
        assert!(capture.cropped_to_window);
        assert_eq!(capture.target_window_id, Some(7));
        assert_eq!(snapshot.unwrap().window_id, 7);
    }

    #[test]
    fn targeted_get_app_state_never_falls_back_to_full_desktop() {
        let mut window = window_with_bounds(7, 50, 20, 200, 100);
        window.backend = "gnome-shell-extension".to_string();
        let error = prepare_get_app_state_capture(
            RawScreenshotCapture {
                bytes: solid_png(400, 200),
                source: "test".to_string(),
                width: 400,
                height: 200,
            },
            Some(&window),
            ScreenshotPayloadOptions::default(),
        )
        .unwrap_err();

        assert!(error.contains("cannot safely crop"));
    }

    #[test]
    fn targeted_window_crop_accepts_non_png_capture_sources() {
        let (cropped, width, height) =
            crop_image_to_png(&solid_jpeg(400, 200), 50, 20, 200, 100).unwrap();

        assert_eq!((width, height), (200, 100));
        assert_eq!(
            image::guess_format(&cropped).unwrap(),
            image::ImageFormat::Png
        );
    }

    #[test]
    fn targeted_window_crop_intersects_the_visible_capture() {
        let negative_origin =
            window_crop_rect_for_capture(&window_bounds(Some(-100), Some(-50), 300, 200), 800, 600)
                .unwrap();
        assert_eq!(
            negative_origin,
            WindowCropRect {
                x: 0,
                y: 0,
                width: 200,
                height: 150,
            }
        );

        let past_right_and_bottom =
            window_crop_rect_for_capture(&window_bounds(Some(700), Some(550), 200, 100), 800, 600)
                .unwrap();
        assert_eq!(
            past_right_and_bottom,
            WindowCropRect {
                x: 700,
                y: 550,
                width: 100,
                height: 50,
            }
        );
    }

    #[test]
    fn targeted_window_crop_rejects_missing_empty_and_fully_offscreen_bounds() {
        let missing_origin =
            window_crop_rect_for_capture(&window_bounds(None, Some(10), 100, 100), 800, 600)
                .unwrap_err();
        assert!(missing_origin.contains("origin"));

        let empty =
            window_crop_rect_for_capture(&window_bounds(Some(10), Some(10), 0, 100), 800, 600)
                .unwrap_err();
        assert!(empty.contains("non-empty"));

        let offscreen =
            window_crop_rect_for_capture(&window_bounds(Some(-300), Some(10), 200, 100), 800, 600)
                .unwrap_err();
        assert!(offscreen.contains("outside"));
    }

    #[test]
    fn targeted_window_crop_requires_capture_space_window_bounds() {
        let mut window = window_info(
            42,
            Some("Target"),
            Some("target-app"),
            Some("target-app"),
            Some(4242),
        );
        window.backend = GNOME_SHELL_EXTENSION_BACKEND.to_string();
        assert!(!window_bounds_match_capture_space(&window));

        window.backend = crate::windowing::XDOTOOL_BACKEND.to_string();
        assert!(window_bounds_match_capture_space(&window));
    }

    fn window_info(
        window_id: u64,
        title: Option<&str>,
        app_id: Option<&str>,
        wm_class: Option<&str>,
        pid: Option<u32>,
    ) -> WindowInfo {
        WindowInfo {
            window_id,
            title: title.map(str::to_string),
            app_id: app_id.map(str::to_string),
            wm_class: wm_class.map(str::to_string),
            pid,
            bounds: Some(WindowBounds {
                x: Some(10),
                y: Some(20),
                width: 800,
                height: 600,
            }),
            workspace: Some(0),
            focused: false,
            hidden: false,
            client_type: Some("wayland".to_string()),
            backend: GNOME_SHELL_EXTENSION_BACKEND.to_string(),
            terminal: None,
        }
    }

    fn focus_result_with_bounds(bounds: Option<WindowBounds>) -> WindowFocusResult {
        let mut requested_window = window_info(
            42,
            Some("Target"),
            Some("target-app"),
            Some("target-app"),
            Some(4242),
        );
        requested_window.bounds = bounds;
        let mut focused_window = requested_window.clone();
        focused_window.focused = true;
        WindowFocusResult {
            requested_window,
            focused_window: Some(focused_window),
            exact_window_focused: true,
            app_focused: true,
            backend: GNOME_SHELL_EXTENSION_BACKEND.to_string(),
            note: "test focus".to_string(),
        }
    }

    fn window_bounds(x: Option<i32>, y: Option<i32>, width: u32, height: u32) -> WindowBounds {
        WindowBounds {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn relative_click_coordinates_use_verified_window_bounds() {
        let focus = focus_result_with_bounds(Some(window_bounds(Some(100), Some(200), 800, 600)));
        let mut params = ClickParams {
            x: Some(7),
            y: Some(9),
            relative: Some(true),
            ..Default::default()
        };

        apply_window_relative_click_coordinates(&mut params, &focus).unwrap();

        assert_eq!((params.x, params.y), (Some(107), Some(209)));
    }

    #[test]
    fn relative_click_coordinates_prefer_focused_window_bounds() {
        let mut focus =
            focus_result_with_bounds(Some(window_bounds(Some(100), Some(200), 800, 600)));
        let focused_window = focus
            .focused_window
            .as_mut()
            .expect("test focus should include focused window");
        focused_window.bounds = Some(window_bounds(Some(300), Some(400), 800, 600));
        let mut params = ClickParams {
            x: Some(7),
            y: Some(9),
            relative: Some(true),
            ..Default::default()
        };

        apply_window_relative_click_coordinates(&mut params, &focus).unwrap();

        assert_eq!((params.x, params.y), (Some(307), Some(409)));
    }

    #[test]
    fn relative_click_coordinates_require_window_bounds_origin() {
        let focus = focus_result_with_bounds(Some(window_bounds(None, Some(200), 800, 600)));
        let mut params = ClickParams {
            x: Some(7),
            y: Some(9),
            relative: Some(true),
            ..Default::default()
        };

        let error = apply_window_relative_click_coordinates(&mut params, &focus).unwrap_err();

        assert!(error.contains("bounds with an origin"));
        assert_eq!((params.x, params.y), (Some(7), Some(9)));
    }

    #[test]
    fn relative_click_coordinates_require_xy() {
        let focus = focus_result_with_bounds(Some(window_bounds(Some(100), Some(200), 800, 600)));
        let mut params = ClickParams {
            x: Some(7),
            relative: Some(true),
            ..Default::default()
        };

        let error = apply_window_relative_click_coordinates(&mut params, &focus).unwrap_err();

        assert!(error.contains("both x and y"));
        assert_eq!((params.x, params.y), (Some(7), None));
    }

    #[test]
    fn relative_click_coordinates_must_stay_inside_bounds() {
        let focus = focus_result_with_bounds(Some(window_bounds(Some(100), Some(200), 800, 600)));

        for (x, y) in [(-1, 9), (7, -1), (800, 9), (7, 600)] {
            let mut params = ClickParams {
                x: Some(x),
                y: Some(y),
                relative: Some(true),
                ..Default::default()
            };

            let error = apply_window_relative_click_coordinates(&mut params, &focus).unwrap_err();

            assert!(error.contains("inside target-window bounds"));
            assert_eq!((params.x, params.y), (Some(x), Some(y)));
        }
    }

    #[test]
    fn accessibility_filter_candidates_prefer_title_and_skip_synthetic_app_id() {
        let window = window_info(
            42,
            Some("CU ATSPI GTK Test"),
            Some("window:46"),
            Some("cu_atspi_gtk_test.py"),
            Some(2914326),
        );

        let candidates = accessibility_filter_candidates(Some(&window));

        assert_eq!(
            candidates,
            vec![
                "CU ATSPI GTK Test".to_string(),
                "cu_atspi_gtk_test.py".to_string(),
            ]
        );
    }

    #[test]
    fn select_accessibility_object_ref_prefers_exact_pid_match() {
        let apps = vec![
            AccessibleAppSummary {
                object_ref: ":1.31/org/a11y/atspi/accessible/root".to_string(),
                name: Some("electron".to_string()),
                pid: Some(2774076),
                role: "application".to_string(),
                child_count: 1,
                bounds: None,
            },
            AccessibleAppSummary {
                object_ref: ":1.64/org/a11y/atspi/accessible/root".to_string(),
                name: Some("cu_atspi_gtk_test.py".to_string()),
                pid: Some(2914326),
                role: "application".to_string(),
                child_count: 1,
                bounds: None,
            },
        ];

        let object_ref = select_accessibility_object_ref(
            &apps,
            2914326,
            &[
                "CU ATSPI GTK Test".to_string(),
                "cu_atspi_gtk_test.py".to_string(),
            ],
        )
        .unwrap();

        assert_eq!(object_ref, ":1.64/org/a11y/atspi/accessible/root");
    }

    #[test]
    fn compact_accessibility_tree_reparents_actionable_descendants() {
        let nodes = vec![
            AccessibilityNode {
                index: 0,
                parent_index: None,
                depth: 0,
                object_ref: ":1.0/root".to_string(),
                role: "application".to_string(),
                name: Some("demo-app".to_string()),
                description: None,
                child_count: 1,
                bounds: None,
                states: Vec::new(),
                actions: Vec::new(),
                value: None,
                text: None,
                supports_editable_text: false,
            },
            AccessibilityNode {
                index: 1,
                parent_index: Some(0),
                depth: 1,
                object_ref: ":1.1/frame".to_string(),
                role: "frame".to_string(),
                name: Some("Demo Frame".to_string()),
                description: None,
                child_count: 1,
                bounds: None,
                states: Vec::new(),
                actions: Vec::new(),
                value: None,
                text: None,
                supports_editable_text: false,
            },
            AccessibilityNode {
                index: 2,
                parent_index: Some(1),
                depth: 2,
                object_ref: ":1.2/filler".to_string(),
                role: "filler".to_string(),
                name: None,
                description: None,
                child_count: 1,
                bounds: None,
                states: Vec::new(),
                actions: Vec::new(),
                value: None,
                text: None,
                supports_editable_text: false,
            },
            AccessibilityNode {
                index: 3,
                parent_index: Some(2),
                depth: 3,
                object_ref: ":1.3/button".to_string(),
                role: "button".to_string(),
                name: Some("Run".to_string()),
                description: None,
                child_count: 0,
                bounds: Some(Bounds {
                    x: 10,
                    y: 20,
                    width: 100,
                    height: 40,
                }),
                states: Vec::new(),
                actions: vec![AccessibilityAction {
                    index: 0,
                    name: "Click".to_string(),
                    description: "Clicks the button".to_string(),
                    keybinding: String::new(),
                }],
                value: None,
                text: None,
                supports_editable_text: false,
            },
        ];

        let compacted = compact_accessibility_tree(nodes);

        assert_eq!(compacted.len(), 3);
        assert_eq!(compacted[0].role, "application");
        assert_eq!(compacted[1].role, "frame");
        assert_eq!(compacted[2].role, "button");
        assert_eq!(compacted[2].parent_index, Some(1));
        assert_eq!(compacted[1].child_count, 1);
    }

    #[test]
    fn compact_accessibility_tree_drops_structural_noise() {
        let nodes = vec![
            AccessibilityNode {
                index: 0,
                parent_index: None,
                depth: 0,
                object_ref: ":1.0/root".to_string(),
                role: "application".to_string(),
                name: Some("demo-app".to_string()),
                description: None,
                child_count: 2,
                bounds: None,
                states: Vec::new(),
                actions: Vec::new(),
                value: None,
                text: None,
                supports_editable_text: false,
            },
            AccessibilityNode {
                index: 1,
                parent_index: Some(0),
                depth: 1,
                object_ref: ":1.1/frame".to_string(),
                role: "frame".to_string(),
                name: Some("Demo Frame".to_string()),
                description: None,
                child_count: 2,
                bounds: None,
                states: Vec::new(),
                actions: Vec::new(),
                value: None,
                text: None,
                supports_editable_text: false,
            },
            AccessibilityNode {
                index: 2,
                parent_index: Some(1),
                depth: 2,
                object_ref: ":1.2/tab".to_string(),
                role: "page tab".to_string(),
                name: Some("Hidden".to_string()),
                description: None,
                child_count: 0,
                bounds: None,
                states: Vec::new(),
                actions: Vec::new(),
                value: None,
                text: None,
                supports_editable_text: false,
            },
            AccessibilityNode {
                index: 3,
                parent_index: Some(1),
                depth: 2,
                object_ref: ":1.3/separator".to_string(),
                role: "separator".to_string(),
                name: None,
                description: None,
                child_count: 0,
                bounds: None,
                states: Vec::new(),
                actions: Vec::new(),
                value: None,
                text: None,
                supports_editable_text: false,
            },
        ];

        let compacted = compact_accessibility_tree(nodes);

        assert_eq!(compacted.len(), 3);
        assert_eq!(compacted[2].role, "page tab");
        assert_eq!(compacted[2].name.as_deref(), Some("Hidden"));
    }

    #[test]
    fn kde_clipboard_restore_delay_uses_minimum_for_short_text() {
        assert_eq!(
            kde_clipboard_restore_delay("short"),
            Duration::from_millis(KDE_CLIPBOARD_RESTORE_MIN_DELAY_MS)
        );
    }

    #[test]
    fn kde_clipboard_restore_delay_scales_and_caps_long_text() {
        let scaled_text = "x".repeat(1_000);
        assert_eq!(
            kde_clipboard_restore_delay(&scaled_text),
            Duration::from_millis(4_000)
        );

        let capped_text = "x".repeat(10_000);
        assert_eq!(
            kde_clipboard_restore_delay(&capped_text),
            Duration::from_millis(KDE_CLIPBOARD_RESTORE_MAX_DELAY_MS)
        );
    }

    #[test]
    fn x11_clipboard_backend_is_preferred_only_for_unforced_x11_keyboard_input() {
        assert!(prefer_x11_clipboard_text_backend(true, false));
        assert!(!prefer_x11_clipboard_text_backend(false, false));
        assert!(!prefer_x11_clipboard_text_backend(true, true));
    }

    #[test]
    fn x11_clipboard_write_requires_an_exact_read_back() {
        assert!(clipboard_write_was_verified("temporary", "temporary"));
        assert!(!clipboard_write_was_verified("temporary", "different"));
    }

    #[test]
    fn x11_clipboard_raw_snapshot_keeps_private_target_bytes() {
        let target = X11RawClipboardTarget {
            name: "chromium/x-web-custom-data".to_string(),
            atom: 123,
            property: X11ClipboardProperty {
                type_atom: 456,
                format: 8,
                bytes: vec![1, 2, 3, 4],
            },
        };
        let snapshot = X11ClipboardSnapshot::Raw(vec![target]);

        assert!(matches!(
            snapshot,
            X11ClipboardSnapshot::Raw(targets)
                if targets[0].name == "chromium/x-web-custom-data"
                    && targets[0].property.type_atom == 456
                    && targets[0].property.format == 8
                    && targets[0].property.bytes == vec![1, 2, 3, 4]
        ));
    }

    #[test]
    fn x11_clipboard_selection_accepts_actual_type_distinct_from_requested_target() {
        let property = x11_clipboard_property_from_reply_parts(
            417, // TEXT
            587, // COMPOUND_TEXT
            8,
            b"restorable text".to_vec(),
        )
        .unwrap();

        assert_eq!(property.type_atom, 587);
        assert_eq!(property.format, 8);
        assert_eq!(property.bytes, b"restorable text");
    }

    #[test]
    fn x11_clipboard_restore_item_count_uses_the_saved_property_format() {
        assert_eq!(x11_clipboard_property_item_count(8, 4).unwrap(), 4);
        assert_eq!(x11_clipboard_property_item_count(16, 4).unwrap(), 2);
        assert_eq!(x11_clipboard_property_item_count(32, 4).unwrap(), 1);
        assert!(x11_clipboard_property_item_count(16, 3).is_err());
        assert!(x11_clipboard_property_item_count(7, 4).is_err());
    }

    #[test]
    fn x11_clipboard_protocol_targets_are_regenerated_not_snapshotted() {
        assert!(x11_clipboard_target_is_protocol("TARGETS"));
        assert!(x11_clipboard_target_is_protocol("MULTIPLE"));
        assert!(x11_clipboard_target_is_protocol("TIMESTAMP"));
        assert!(x11_clipboard_target_is_protocol("SAVE_TARGETS"));
        assert!(x11_clipboard_target_is_protocol("DELETE"));
        assert!(x11_clipboard_target_is_protocol("INSERT_SELECTION"));
        assert!(x11_clipboard_target_is_protocol("INSERT_PROPERTY"));
        assert!(x11_clipboard_target_is_protocol("INCR"));
        assert!(x11_clipboard_target_is_protocol(
            "FROM_DEEPIN_CLIPBOARD_MANAGER"
        ));
        assert!(!x11_clipboard_target_is_protocol("text/rtf"));
        assert!(!x11_clipboard_target_is_protocol(
            "chromium/x-web-custom-data"
        ));
    }

    #[test]
    fn x11_clipboard_snapshot_reads_and_aggregate_size_are_bounded() {
        let long_length = x11_clipboard_get_property_long_length(X11_CLIPBOARD_MAX_SNAPSHOT_BYTES);
        assert_ne!(long_length, u32::MAX);
        assert!(long_length as usize * 4 <= X11_CLIPBOARD_MAX_SNAPSHOT_BYTES + 3);

        assert_eq!(
            x11_clipboard_snapshot_target_limit(0, usize::MAX).unwrap(),
            X11_CLIPBOARD_MAX_SNAPSHOT_BYTES
        );
        assert_eq!(
            x11_clipboard_snapshot_target_limit(X11_CLIPBOARD_MAX_SNAPSHOT_BYTES - 10, usize::MAX,)
                .unwrap(),
            10
        );
        assert!(
            x11_clipboard_snapshot_target_limit(X11_CLIPBOARD_MAX_SNAPSHOT_BYTES, usize::MAX,)
                .is_err()
        );
    }

    #[test]
    fn failed_clipboard_write_restores_only_for_a_known_current_owner() {
        assert_eq!(
            clipboard_restore_decision_for_known_owner(Some(100), Ok(100)),
            ClipboardRestoreDecision::RestorePrevious
        );
        assert_eq!(
            clipboard_restore_decision_for_known_owner(Some(100), Ok(200)),
            ClipboardRestoreDecision::PreserveCurrent
        );
        assert_eq!(
            clipboard_restore_decision_for_known_owner(None, Ok(100)),
            ClipboardRestoreDecision::PreserveCurrent
        );
        assert_eq!(
            clipboard_restore_decision_for_known_owner(Some(100), Err(())),
            ClipboardRestoreDecision::Unreadable
        );
    }

    #[test]
    fn x11_clipboard_restore_does_not_overwrite_a_concurrent_change() {
        assert_eq!(
            clipboard_restore_decision(100, Ok(100)),
            ClipboardRestoreDecision::RestorePrevious
        );
        assert_eq!(
            clipboard_restore_decision(100, Ok(200)),
            ClipboardRestoreDecision::PreserveCurrent
        );
        assert_eq!(
            clipboard_restore_decision(100, Err(())),
            ClipboardRestoreDecision::Unreadable
        );
    }

    #[test]
    fn x11_clipboard_same_text_from_a_new_owner_is_preserved() {
        assert_eq!(
            clipboard_restore_decision(100, Ok(200)),
            ClipboardRestoreDecision::PreserveCurrent
        );
    }

    #[test]
    fn x11_clipboard_paste_never_falls_back_after_paste_was_dispatched() {
        let before = X11ClipboardPasteError::before_paste("setup failed");
        assert!(before.can_fallback_to_xdotool);
        assert!(!before.dispatched);

        let after = X11ClipboardPasteError::after_paste("paste result unknown");
        assert!(!after.can_fallback_to_xdotool);
        assert!(after.dispatched);
    }

    #[test]
    fn x11_clipboard_uses_terminal_paste_chord_for_terminal_targets() {
        assert_eq!(x11_paste_chord(false), "ctrl+v");
        assert_eq!(x11_paste_chord(true), "ctrl+shift+v");

        let mut terminal = window_info(
            42,
            Some("Deepin Terminal"),
            Some("deepin-terminal"),
            Some("deepin-terminal"),
            Some(4242),
        );
        terminal.terminal = None;
        assert!(window_uses_terminal_paste(&terminal));
    }

    #[tokio::test]
    async fn kde_clipboard_dbus_operation_times_out_when_pending() {
        let error = kde_clipboard_dbus_operation_with_timeout(
            "proxy creation",
            std::future::pending::<zbus::Result<()>>(),
            Duration::from_millis(1),
        )
        .await
        .unwrap_err();

        assert_eq!(error, "KDE clipboard proxy creation timed out");
    }

    #[test]
    fn cached_element_index_resolves_to_bounds_center() {
        let backend = ComputerUseLinux::default();
        backend.cache_nodes(&[node(
            7,
            Some(Bounds {
                x: 10,
                y: 20,
                width: 100,
                height: 40,
            }),
        )]);

        let point = backend
            .resolve_optional_target_point(None, None, Some(7))
            .unwrap()
            .unwrap();

        assert_eq!(point, (60, 40));
    }

    #[test]
    fn screenshot_coordinate_cache_keeps_multiple_ids_and_crop_origins() {
        let backend = ComputerUseLinux::default();
        let make_capture = |id: &str, origin_x: i32, origin_y: i32| {
            let mut capture = prepare_screenshot_payload(
                RawScreenshotCapture {
                    bytes: solid_png(960, 540),
                    source: "test".to_string(),
                    width: 960,
                    height: 540,
                },
                ScreenshotPayloadOptions {
                    max_width: Some(960),
                    max_height: Some(540),
                    max_bytes: Some(4 * 1024 * 1024),
                    ..Default::default()
                },
            )
            .unwrap();
            capture.screenshot_id = id.to_string();
            capture.coordinate_width = 1280;
            capture.coordinate_height = 720;
            capture.coordinate_origin_x = origin_x;
            capture.coordinate_origin_y = origin_y;
            capture
        };
        let first = make_capture("shot-first", 100, 50);
        let second = make_capture("shot-second", -200, -100);
        backend.cache_screenshot_artifact(&first, "layout".to_string(), None);
        backend.cache_screenshot_artifact(&second, "layout".to_string(), None);

        assert_eq!(
            backend
                .screenshot_artifact("shot-first")
                .unwrap()
                .transform
                .map_pixel(480, 270)
                .unwrap(),
            (740, 410),
        );
        assert_eq!(
            backend
                .screenshot_artifact("shot-second")
                .unwrap()
                .transform
                .map_pixel(480, 270)
                .unwrap(),
            (440, 260),
        );
        assert!(backend
            .screenshot_artifact("shot-first")
            .unwrap()
            .transform
            .map_pixel(960, 100)
            .unwrap_err()
            .contains("outside image"));
    }

    #[test]
    fn atspi_bounds_scale_to_physical_x11_window_coordinates() {
        let backend = ComputerUseLinux::default();
        let mut frame = node(
            1,
            Some(Bounds {
                x: 0,
                y: 0,
                width: 1707,
                height: 922,
            }),
        );
        frame.role = "frame".to_string();
        let mut button = node(
            7,
            Some(Bounds {
                x: 750,
                y: 25,
                width: 100,
                height: 40,
            }),
        );
        button.depth = 1;
        button.parent_index = Some(1);
        let mut window = window_with_bounds(1, 0, 0, 2560, 1440);
        window.backend = crate::windowing::XDOTOOL_BACKEND.to_string();

        backend.cache_nodes_for_window(&[frame, button], Some(&window));

        assert_eq!(
            backend
                .resolve_optional_target_point(None, None, Some(7))
                .unwrap(),
            Some((1200, 67))
        );
    }

    #[test]
    fn atspi_primary_action_fallback_uses_physical_x11_coordinates() {
        let backend = ComputerUseLinux::default();
        let mut frame = node(
            1,
            Some(Bounds {
                x: 0,
                y: 0,
                width: 1707,
                height: 922,
            }),
        );
        frame.role = "frame".to_string();
        let mut button = node_with_actions(
            7,
            Some(Bounds {
                x: 750,
                y: 25,
                width: 100,
                height: 40,
            }),
            vec![click_action()],
        );
        button.depth = 1;
        button.parent_index = Some(1);
        let mut window = window_with_bounds(1, 0, 0, 2560, 1440);
        window.backend = crate::windowing::XDOTOOL_BACKEND.to_string();
        backend.cache_nodes_for_window(&[frame, button], Some(&window));

        let target = backend
            .resolve_click_target(&ClickParams {
                element_index: Some(7),
                ..Default::default()
            })
            .unwrap();

        assert!(matches!(
            target,
            ClickTarget::PrimaryAction {
                fallback_coordinates: Some((1200, 67)),
                ..
            }
        ));
    }

    #[test]
    fn coordinate_target_overrides_cached_element_index() {
        let backend = ComputerUseLinux::default();
        backend.cache_nodes(&[node(
            7,
            Some(Bounds {
                x: 10,
                y: 20,
                width: 100,
                height: 40,
            }),
        )]);

        let point = backend
            .resolve_optional_target_point(Some(200), Some(300), Some(7))
            .unwrap()
            .unwrap();

        assert_eq!(point, (200, 300));
    }

    #[test]
    fn cached_element_index_requires_positive_bounds() {
        let backend = ComputerUseLinux::default();
        backend.cache_nodes(&[node(
            7,
            Some(Bounds {
                x: 10,
                y: 20,
                width: 0,
                height: 40,
            }),
        )]);

        let error = backend
            .resolve_optional_target_point(None, None, Some(7))
            .unwrap_err();

        assert!(error.contains("No clickable bounds cached for element_index 7"));
    }

    #[test]
    fn cached_element_index_ignores_sentinel_bounds() {
        let backend = ComputerUseLinux::default();
        backend.cache_nodes(&[node(
            7,
            Some(Bounds {
                x: i32::MIN,
                y: i32::MIN,
                width: 1,
                height: 1,
            }),
        )]);

        let error = backend
            .resolve_optional_target_point(None, None, Some(7))
            .unwrap_err();

        assert!(error.contains("No clickable bounds cached for element_index 7"));
    }

    #[test]
    fn empty_node_cache_clears_stale_element_index() {
        let backend = ComputerUseLinux::default();
        backend.cache_nodes(&[node(
            7,
            Some(Bounds {
                x: 10,
                y: 20,
                width: 100,
                height: 40,
            }),
        )]);
        backend.cache_nodes(&[]);

        let error = backend
            .resolve_optional_target_point(None, None, Some(7))
            .unwrap_err();

        assert!(error.contains("No clickable bounds cached for element_index 7"));
    }

    #[test]
    fn click_target_falls_back_to_primary_action_without_bounds() {
        let backend = ComputerUseLinux::default();
        backend.cache_nodes(&[node_with_actions(
            7,
            None,
            vec![AccessibilityAction {
                index: 0,
                name: "Click".to_string(),
                description: "Clicks the button".to_string(),
                keybinding: String::new(),
            }],
        )]);

        let target = backend
            .resolve_click_target(&ClickParams {
                element_index: Some(7),
                ..Default::default()
            })
            .unwrap();

        match target {
            ClickTarget::PrimaryAction {
                object_ref,
                action_name,
                action_index,
                fallback_coordinates,
            } => {
                assert_eq!(object_ref, ":1.7/org/a11y/atspi/accessible/7");
                assert_eq!(action_name.as_deref(), Some("Click"));
                assert_eq!(action_index, 0);
                assert_eq!(fallback_coordinates, None);
            }
            ClickTarget::Coordinates(_, _) => {
                panic!("expected AT-SPI primary-action fallback")
            }
        }
    }

    #[test]
    fn click_target_falls_back_to_primary_action_with_sentinel_bounds() {
        let backend = ComputerUseLinux::default();
        backend.cache_nodes(&[node_with_actions(
            7,
            Some(Bounds {
                x: i32::MIN,
                y: i32::MIN,
                width: 1,
                height: 1,
            }),
            vec![AccessibilityAction {
                index: 0,
                name: "Click".to_string(),
                description: "Clicks the button".to_string(),
                keybinding: String::new(),
            }],
        )]);

        let target = backend
            .resolve_click_target(&ClickParams {
                element_index: Some(7),
                ..Default::default()
            })
            .unwrap();

        match target {
            ClickTarget::PrimaryAction {
                object_ref,
                action_name,
                action_index,
                fallback_coordinates,
            } => {
                assert_eq!(object_ref, ":1.7/org/a11y/atspi/accessible/7");
                assert_eq!(action_name.as_deref(), Some("Click"));
                assert_eq!(action_index, 0);
                assert_eq!(fallback_coordinates, None);
            }
            ClickTarget::Coordinates(_, _) => {
                panic!("expected AT-SPI primary-action fallback")
            }
        }
    }

    #[test]
    fn click_target_requires_bounds_for_non_plain_clicks() {
        let backend = ComputerUseLinux::default();
        backend.cache_nodes(&[node_with_actions(
            7,
            None,
            vec![AccessibilityAction {
                index: 0,
                name: "Click".to_string(),
                description: "Clicks the button".to_string(),
                keybinding: String::new(),
            }],
        )]);

        let error = backend
            .resolve_click_target(&ClickParams {
                element_index: Some(7),
                button: Some("right".to_string()),
                ..Default::default()
            })
            .unwrap_err();

        assert!(error.contains("No clickable bounds cached for element_index 7"));
    }

    #[test]
    fn absolute_mousemove_uses_coordinate_separator() {
        assert_eq!(
            absolute_mousemove_args(200, 300),
            vec![
                "mousemove".to_string(),
                "--absolute".to_string(),
                "--".to_string(),
                "200".to_string(),
                "300".to_string(),
            ]
        );
    }

    #[test]
    fn wheel_mousemove_uses_coordinate_separator_for_negative_values() {
        assert_eq!(
            wheel_mousemove_args(0, -3),
            vec![
                "mousemove".to_string(),
                "--wheel".to_string(),
                "--".to_string(),
                "0".to_string(),
                "-3".to_string(),
            ]
        );
    }

    #[test]
    fn pointer_actions_keep_pixel_coordinates_for_ydotool_absolute_moves() {
        assert_eq!(
            absolute_mousemove_args(1550, 930),
            vec![
                "mousemove".to_string(),
                "--absolute".to_string(),
                "--".to_string(),
                "1550".to_string(),
                "930".to_string(),
            ]
        );
    }

    #[test]
    fn xdotool_click_uses_absolute_coordinates_and_repeat_count() {
        assert_eq!(
            xdotool_click_args(80, 230, Some("right"), 2),
            vec![
                "mousemove",
                "--sync",
                "--",
                "80",
                "230",
                "click",
                "--repeat",
                "2",
                "--delay",
                "50",
                "3",
            ]
        );
    }

    #[test]
    fn xdotool_scroll_moves_before_wheel_clicks() {
        assert_eq!(
            xdotool_scroll_args(Some((400, 300)), "down", 5),
            vec![
                "mousemove",
                "--sync",
                "--",
                "400",
                "300",
                "click",
                "--repeat",
                "5",
                "--delay",
                "25",
                "5",
            ]
        );
    }

    #[test]
    fn xdotool_drag_holds_primary_button_between_points() {
        assert_eq!(
            xdotool_drag_args(10, 20, 30, 40),
            vec![
                "mousemove",
                "--sync",
                "--",
                "10",
                "20",
                "mousedown",
                "1",
                "mousemove",
                "--sync",
                "--",
                "30",
                "40",
                "mouseup",
                "1",
            ]
        );
    }

    #[test]
    fn xdotool_key_chord_matches_supported_key_grammar() {
        assert_eq!(
            xdotool_key_chord("Ctrl+Shift+P").as_deref(),
            Some("ctrl+shift+p")
        );
        assert_eq!(xdotool_key_chord("ArrowLeft").as_deref(), Some("Left"));
        assert_eq!(xdotool_key_chord("Page Down").as_deref(), Some("Page_Down"));
        assert_eq!(xdotool_key_chord("F12").as_deref(), Some("F12"));
        assert_eq!(xdotool_key_chord("not-a-key"), None);
    }

    #[test]
    fn key_sequence_presses_modifiers_around_key() {
        assert_eq!(
            key_sequence("Ctrl+Shift+P"),
            Some(vec![
                "29:1".to_string(),
                "42:1".to_string(),
                "25:1".to_string(),
                "25:0".to_string(),
                "42:0".to_string(),
                "29:0".to_string(),
            ])
        );
    }

    #[test]
    fn key_sequence_presses_bare_modifier() {
        assert_eq!(
            key_sequence("Super"),
            Some(vec!["125:1".to_string(), "125:0".to_string()])
        );
    }

    #[test]
    fn key_sequence_keeps_shortcuts_and_navigation_on_raw_events() {
        assert_eq!(
            key_sequence("Ctrl+L"),
            Some(vec![
                "29:1".to_string(),
                "38:1".to_string(),
                "38:0".to_string(),
                "29:0".to_string(),
            ])
        );
        assert_eq!(
            key_sequence("ArrowLeft"),
            Some(vec!["105:1".to_string(), "105:0".to_string()])
        );
        assert_eq!(
            key_sequence("Escape"),
            Some(vec!["1:1".to_string(), "1:0".to_string()])
        );
        assert_eq!(
            key_sequence("Enter"),
            Some(vec!["28:1".to_string(), "28:0".to_string()])
        );
    }

    #[test]
    fn ydotool_type_timeout_scales_with_text_length() {
        assert_eq!(ydotool_type_timeout("").as_secs(), 10);
        assert_eq!(ydotool_type_timeout("x").as_secs(), 11);
        assert_eq!(ydotool_type_timeout(&"x".repeat(200)).as_secs(), 20);
        assert_eq!(ydotool_type_timeout(&"x".repeat(500)).as_secs(), 35);
    }

    #[tokio::test]
    async fn ydotool_wait_drains_output_before_exit() {
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "yes noisy | head -c 200000 >&2; exit 7"]);
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let output = wait_for_ydotool_output_with_timeout(
            command.spawn().expect("spawn noisy child"),
            Duration::from_secs(5),
        )
        .await
        .expect("child should exit before timeout");

        assert_eq!(output.status.code(), Some(7));
        assert!(output.stderr.len() >= 100_000);
    }

    #[test]
    fn ydotool_socket_selection_skips_unconnectable_candidates() {
        let dir =
            std::env::temp_dir().join(format!("codex-computer-use-server-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp server dir");
        let stale_socket = dir.join("stale.sock");
        std::fs::write(&stale_socket, b"not a socket").expect("write stale socket placeholder");
        let usable_socket = dir.join("usable.sock");
        let listener =
            std::os::unix::net::UnixListener::bind(&usable_socket).expect("bind usable socket");

        let selected = connectable_ydotool_socket_from(vec![stale_socket, usable_socket.clone()])
            .expect("usable socket should be selected");

        assert_eq!(selected, usable_socket);
        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ydotool_socket_selection_accepts_datagram_socket() {
        let dir = std::env::temp_dir().join(format!(
            "codex-computer-use-server-dgram-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp server dir");
        let stale_socket = dir.join("stale.sock");
        std::fs::write(&stale_socket, b"not a socket").expect("write stale socket placeholder");
        let usable_socket = dir.join("usable.sock");
        let datagram =
            std::os::unix::net::UnixDatagram::bind(&usable_socket).expect("bind usable socket");

        let selected = connectable_ydotool_socket_from(vec![stale_socket, usable_socket.clone()])
            .expect("usable socket should be selected");

        assert_eq!(selected, usable_socket);
        drop(datagram);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn perform_action_defaults_to_primary_action_index() {
        assert_eq!(requested_or_primary_action(None), "0");
        assert_eq!(requested_or_primary_action(Some("   ")), "0");
        assert_eq!(
            requested_or_primary_action(Some(" show-menu ")),
            "show-menu"
        );
    }

    #[test]
    fn explicit_ydotool_socket_is_used_without_connectability_probe() {
        let _guard = EnvVarGuard::set("YDOTOOL_SOCKET", " /does/not/exist.sock ");

        let selected = explicit_ydotool_socket();

        assert_eq!(selected.as_deref(), Some("/does/not/exist.sock"));
    }

    #[test]
    fn element_identifier_overrides_cached_object_ref() {
        let backend = ComputerUseLinux::default();
        backend.cache_nodes(&[node(7, None)]);

        let object_ref = backend
            .resolve_object_ref(
                Some(7),
                Some(":1.99/org/a11y/atspi/accessible/3"),
                &ElementSelector::default(),
                ElementResolvePurpose::Action,
            )
            .unwrap();

        assert_eq!(object_ref, ":1.99/org/a11y/atspi/accessible/3");
    }

    #[test]
    fn element_index_resolves_to_cached_object_ref() {
        let backend = ComputerUseLinux::default();
        backend.cache_nodes(&[node(7, None)]);

        let object_ref = backend
            .resolve_object_ref(
                Some(7),
                None,
                &ElementSelector::default(),
                ElementResolvePurpose::Action,
            )
            .unwrap();

        assert_eq!(object_ref, ":1.7/org/a11y/atspi/accessible/7");
    }

    #[test]
    fn semantic_selector_resolves_unique_cached_node_by_role_and_name() {
        let backend = ComputerUseLinux::default();
        let mut search_entry = node(7, None);
        search_entry.role = "entry".to_string();
        search_entry.name = Some("Search files".to_string());
        search_entry.supports_editable_text = true;
        backend.cache_nodes(&[search_entry]);

        let object_ref = backend
            .resolve_object_ref(
                None,
                None,
                &ElementSelector {
                    role: Some("entry"),
                    name: Some("search"),
                    ..Default::default()
                },
                ElementResolvePurpose::SetValue,
            )
            .unwrap();

        assert_eq!(object_ref, ":1.7/org/a11y/atspi/accessible/7");
    }

    #[test]
    fn semantic_selector_prefers_actionable_match() {
        let backend = ComputerUseLinux::default();
        let mut label = node(4, None);
        label.role = "label".to_string();
        label.name = Some("Close".to_string());
        let mut button = node_with_actions(7, None, vec![click_action()]);
        button.role = "push button".to_string();
        button.name = Some("Close".to_string());
        backend.cache_nodes(&[label, button]);

        let object_ref = backend
            .resolve_object_ref(
                None,
                None,
                &ElementSelector {
                    name: Some("close"),
                    ..Default::default()
                },
                ElementResolvePurpose::Action,
            )
            .unwrap();

        assert_eq!(object_ref, ":1.7/org/a11y/atspi/accessible/7");
    }

    #[test]
    fn semantic_selector_prefers_editable_match() {
        let backend = ComputerUseLinux::default();
        let mut label = node(4, None);
        label.role = "label".to_string();
        label.name = Some("Search".to_string());
        let mut entry = node(7, None);
        entry.role = "entry".to_string();
        entry.name = Some("Search".to_string());
        entry.supports_editable_text = true;
        backend.cache_nodes(&[label, entry]);

        let object_ref = backend
            .resolve_object_ref(
                None,
                None,
                &ElementSelector {
                    name: Some("search"),
                    ..Default::default()
                },
                ElementResolvePurpose::SetValue,
            )
            .unwrap();

        assert_eq!(object_ref, ":1.7/org/a11y/atspi/accessible/7");
    }

    #[test]
    fn semantic_selector_reports_ambiguous_matches() {
        let backend = ComputerUseLinux::default();
        let mut first = node_with_actions(7, None, vec![click_action()]);
        first.name = Some("Close".to_string());
        let mut second = node_with_actions(9, None, vec![click_action()]);
        second.name = Some("Close".to_string());
        backend.cache_nodes(&[first, second]);

        let error = backend
            .resolve_object_ref(
                None,
                None,
                &ElementSelector {
                    name: Some("close"),
                    ..Default::default()
                },
                ElementResolvePurpose::Action,
            )
            .unwrap_err();

        assert!(error.contains("matched multiple cached nodes"));
        assert!(error.contains("element_index 7"));
        assert!(error.contains("element_index 9"));
    }

    #[test]
    fn semantic_click_selector_prefers_native_action_with_coordinate_fallback() {
        let backend = ComputerUseLinux::default();
        let mut button = node_with_actions(
            7,
            Some(Bounds {
                x: 10,
                y: 20,
                width: 100,
                height: 40,
            }),
            vec![click_action()],
        );
        button.name = Some("Run".to_string());
        backend.cache_nodes(&[button]);

        let target = backend
            .resolve_click_target(&ClickParams {
                role: Some("button".to_string()),
                name: Some("run".to_string()),
                ..Default::default()
            })
            .unwrap();

        assert!(matches!(
            target,
            ClickTarget::PrimaryAction {
                action_index: 0,
                fallback_coordinates: Some((60, 40)),
                ..
            }
        ));
    }

    #[test]
    fn describe_focused_element_editable() {
        let element = FocusedElementSummary {
            role: "text".to_string(),
            name: Some("Message".to_string()),
            editable: true,
            states: vec!["focused".to_string()],
        };
        let described = describe_focused_element(&element, true);
        assert!(described.contains("editable"));
        assert!(!described.contains("WARNING"));
    }

    #[test]
    fn describe_focused_element_warns_on_non_editable_when_typing() {
        let element = FocusedElementSummary {
            role: "push button".to_string(),
            name: Some("OK".to_string()),
            editable: false,
            states: vec!["focused".to_string()],
        };
        let described = describe_focused_element(&element, true);
        assert!(described.contains("WARNING"));
        assert!(described.contains("not editable"));
    }

    #[test]
    fn describe_focused_element_no_warning_for_press_key() {
        let element = FocusedElementSummary {
            role: "push button".to_string(),
            name: None,
            editable: false,
            states: vec![],
        };
        let described = describe_focused_element(&element, false);
        assert!(!described.contains("WARNING"));
    }

    #[test]
    fn backend_success_only_proves_dispatch() {
        let output = action_result_for_backend("type_text", Ok(Vec::new()), None, "xdotool");

        assert!(output.ok);
        assert!(output.dispatched);
        assert_eq!(output.landed, None);
        assert!(!output.verified);

        let serialized = serde_json::to_value(output).unwrap();
        assert_eq!(serialized["dispatched"], true);
        assert_eq!(serialized["landed"], serde_json::Value::Null);
        assert_eq!(serialized["verified"], false);
    }

    #[test]
    fn non_editable_focus_marks_typed_text_as_not_landed() {
        let element = FocusedElementSummary {
            role: "list".to_string(),
            name: Some("Extensions".to_string()),
            editable: false,
            states: vec!["focused".to_string()],
        };
        let feedback = focused_element_assessment(&element, true);
        let output = with_action_feedback(
            action_result_for_backend("type_text", Ok(Vec::new()), None, "xdotool"),
            feedback,
        );

        assert!(!output.ok);
        assert!(output.dispatched);
        assert_eq!(output.landed, Some(false));
        assert!(output.verified);
        assert!(output.message.contains("not editable"));
    }

    #[test]
    fn editable_focus_does_not_claim_the_text_effect_was_verified() {
        let element = FocusedElementSummary {
            role: "entry".to_string(),
            name: Some("Configuration".to_string()),
            editable: true,
            states: vec!["focused".to_string()],
        };
        let feedback = focused_element_assessment(&element, true);
        let output = with_action_feedback(
            action_result_for_backend("type_text", Ok(Vec::new()), None, "xdotool"),
            feedback,
        );

        assert!(output.ok);
        assert!(output.dispatched);
        assert_eq!(output.landed, None);
        assert!(!output.verified);
    }

    #[test]
    fn unavailable_focus_probe_does_not_claim_landing() {
        let output = with_action_feedback(
            action_result_for_backend("type_text", Ok(Vec::new()), None, "xdotool"),
            ActionFeedback::unverified(
                "Focused-element feedback unavailable (AT-SPI probe timed out).",
            ),
        );

        assert!(output.ok);
        assert!(output.dispatched);
        assert_eq!(output.landed, None);
        assert!(!output.verified);
    }

    #[test]
    fn known_offscreen_pointer_target_is_a_verified_landing_failure() {
        let output = with_action_feedback(
            action_result_for_backend("click", Ok(Vec::new()), None, "xdotool"),
            ActionFeedback::failed_landing(
                "WARNING: coordinate 3000,2000 is outside the captured desktop.",
            ),
        );

        assert!(!output.ok);
        assert!(output.dispatched);
        assert_eq!(output.landed, Some(false));
        assert!(output.verified);
    }

    #[test]
    fn relative_scroll_translates_coordinates() {
        let mut params = ScrollParams {
            element_index: None,
            x: Some(10),
            y: Some(20),
            direction: "down".to_string(),
            pages: None,
            window_id: Some(1),
            pid: None,
            app_id: None,
            wm_class: None,
            window_title: None,
            relative: Some(true),
        };
        let focus = WindowFocusResult {
            requested_window: window_with_bounds(1, 100, 200, 800, 600),
            focused_window: None,
            app_focused: true,
            exact_window_focused: true,
            backend: "test".to_string(),
            note: String::new(),
        };
        apply_window_relative_scroll_coordinates(&mut params, &focus).unwrap();
        assert_eq!(params.x, Some(110));
        assert_eq!(params.y, Some(220));
    }

    #[test]
    fn window_targeted_scroll_defaults_to_window_center() {
        let mut params = ScrollParams {
            element_index: None,
            x: None,
            y: None,
            direction: "down".to_string(),
            pages: None,
            window_id: Some(1),
            pid: None,
            app_id: None,
            wm_class: None,
            window_title: None,
            relative: None,
        };
        let focus = WindowFocusResult {
            requested_window: window_with_bounds(1, 100, 200, 800, 600),
            focused_window: None,
            app_focused: true,
            exact_window_focused: true,
            backend: "test".to_string(),
            note: String::new(),
        };
        apply_window_center_scroll_point(&mut params, &focus).unwrap();
        assert_eq!(params.x, Some(500));
        assert_eq!(params.y, Some(500));
    }

    #[test]
    fn window_targeted_scroll_without_bounds_errors() {
        let mut params = ScrollParams {
            element_index: None,
            x: None,
            y: None,
            direction: "down".to_string(),
            pages: None,
            window_id: Some(1),
            pid: None,
            app_id: None,
            wm_class: None,
            window_title: None,
            relative: None,
        };
        let mut window = window_with_bounds(1, 0, 0, 1, 1);
        window.bounds = None;
        let focus = WindowFocusResult {
            requested_window: window,
            focused_window: None,
            app_focused: true,
            exact_window_focused: true,
            backend: "test".to_string(),
            note: String::new(),
        };
        let error = apply_window_center_scroll_point(&mut params, &focus).unwrap_err();
        assert!(error.contains("pass x/y explicitly"));
        assert_eq!(params.x, None);
        assert_eq!(params.y, None);
    }

    #[test]
    fn relative_scroll_rejects_out_of_bounds() {
        let mut params = ScrollParams {
            element_index: None,
            x: Some(801),
            y: Some(20),
            direction: "down".to_string(),
            pages: None,
            window_id: Some(1),
            pid: None,
            app_id: None,
            wm_class: None,
            window_title: None,
            relative: Some(true),
        };
        let focus = WindowFocusResult {
            requested_window: window_with_bounds(1, 100, 200, 800, 600),
            focused_window: None,
            app_focused: true,
            exact_window_focused: true,
            backend: "test".to_string(),
            note: String::new(),
        };
        assert!(apply_window_relative_scroll_coordinates(&mut params, &focus).is_err());
    }

    fn window_with_bounds(id: u64, x: i32, y: i32, width: u32, height: u32) -> WindowInfo {
        WindowInfo {
            window_id: id,
            title: None,
            app_id: None,
            wm_class: None,
            pid: None,
            bounds: Some(crate::windowing::WindowBounds {
                x: Some(x),
                y: Some(y),
                width,
                height,
            }),
            workspace: None,
            focused: true,
            hidden: false,
            client_type: None,
            backend: "test".to_string(),
            terminal: None,
        }
    }
}
