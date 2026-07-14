use crate::terminal::enrich_terminal_windows;
use crate::windowing::registry::BackendProbe;
use crate::windowing::types::{WindowBounds, WindowInfo};
use anyhow::{bail, Context, Result};
use std::{env, process::Command};

pub const XDOTOOL_BACKEND: &str = "xdotool-x11";

pub fn probe() -> BackendProbe {
    if !is_x11_session() {
        return BackendProbe {
            id: XDOTOOL_BACKEND,
            ok: false,
            can_list_windows: false,
            can_focus_apps: false,
            can_focus_windows: false,
            detail: "xdotool backend requires an X11 session".to_string(),
        };
    }

    match xdotool_ready().and_then(|()| root_window_state()) {
        Ok((window_ids, _, _)) if !window_ids.is_empty() => BackendProbe {
            id: XDOTOOL_BACKEND,
            ok: true,
            can_list_windows: true,
            can_focus_apps: true,
            can_focus_windows: true,
            detail: format!(
                "xdotool/xprop can access the X11 root window and {} client windows",
                window_ids.len()
            ),
        },
        Ok(_) => BackendProbe {
            id: XDOTOOL_BACKEND,
            ok: false,
            can_list_windows: false,
            can_focus_apps: false,
            can_focus_windows: false,
            detail: "X11 root window reported no managed client windows".to_string(),
        },
        Err(error) => BackendProbe {
            id: XDOTOOL_BACKEND,
            ok: false,
            can_list_windows: false,
            can_focus_apps: false,
            can_focus_windows: false,
            detail: format!("{error:#}"),
        },
    }
}

pub fn list_windows() -> Result<Vec<WindowInfo>> {
    if !is_x11_session() {
        bail!("xdotool backend requires an X11 session");
    }
    xdotool_ready()?;

    let (window_ids, active_window, current_workspace) = root_window_state()?;
    let mut windows = window_ids
        .into_iter()
        .filter_map(|window_id| window_info(window_id, active_window, current_workspace).ok())
        .collect::<Vec<_>>();
    enrich_terminal_windows(&mut windows);
    Ok(windows)
}

fn xdotool_ready() -> Result<()> {
    let output = Command::new("xdotool")
        .arg("getwindowfocus")
        .output()
        .context("failed to run xdotool getwindowfocus")?;
    if output.status.success() {
        Ok(())
    } else {
        bail!("xdotool getwindowfocus failed: {}", command_detail(&output))
    }
}

pub fn activate_window(window_id: u64) -> Result<()> {
    let output = Command::new("xdotool")
        .args(["windowactivate", "--sync", &window_id.to_string()])
        .output()
        .context("failed to run xdotool windowactivate")?;
    if output.status.success() {
        Ok(())
    } else {
        bail!("xdotool windowactivate failed: {}", command_detail(&output))
    }
}

fn root_window_state() -> Result<(Vec<u64>, Option<u64>, Option<i32>)> {
    let output = Command::new("xprop")
        .args([
            "-root",
            "_NET_CLIENT_LIST_STACKING",
            "_NET_ACTIVE_WINDOW",
            "_NET_CURRENT_DESKTOP",
        ])
        .output()
        .context("failed to run xprop on the X11 root window")?;
    if !output.status.success() {
        bail!("xprop root query failed: {}", command_detail(&output));
    }
    Ok(parse_root_window_state(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

pub(crate) fn parse_root_window_state(output: &str) -> (Vec<u64>, Option<u64>, Option<i32>) {
    let mut window_ids = Vec::new();
    let mut active_window = None;
    let mut current_workspace = None;
    for line in output.lines() {
        if line.starts_with("_NET_CLIENT_LIST_STACKING") {
            window_ids = parse_window_ids(line);
        } else if line.starts_with("_NET_ACTIVE_WINDOW") {
            active_window = parse_window_ids(line).into_iter().next();
        } else if line.starts_with("_NET_CURRENT_DESKTOP") {
            current_workspace = line
                .split('=')
                .nth(1)
                .and_then(|value| value.trim().parse::<i32>().ok());
        }
    }
    (window_ids, active_window, current_workspace)
}

fn parse_window_ids(line: &str) -> Vec<u64> {
    line.split('#')
        .nth(1)
        .into_iter()
        .flat_map(|value| value.split(','))
        .filter_map(|value| parse_window_id(value.trim()))
        .filter(|window_id| *window_id != 0)
        .collect()
}

fn parse_window_id(value: &str) -> Option<u64> {
    value
        .strip_prefix("0x")
        .and_then(|hex| u64::from_str_radix(hex, 16).ok())
        .or_else(|| value.parse::<u64>().ok())
}

fn window_info(
    window_id: u64,
    active_window: Option<u64>,
    current_workspace: Option<i32>,
) -> Result<WindowInfo> {
    let id = window_id.to_string();
    let title = command_text("xdotool", &["getwindowname", &id]);
    let pid =
        command_text("xdotool", &["getwindowpid", &id]).and_then(|value| value.parse::<u32>().ok());
    let geometry = command_text("xdotool", &["getwindowgeometry", "--shell", &id]);
    let properties = command_text(
        "xprop",
        &["-id", &id, "WM_CLASS", "_NET_WM_DESKTOP", "_NET_WM_STATE"],
    )
    .unwrap_or_default();
    let (app_id, wm_class, workspace, hidden) = parse_window_properties(&properties);
    let bounds = geometry.as_deref().and_then(parse_geometry);

    Ok(WindowInfo {
        window_id,
        title,
        app_id,
        wm_class,
        pid,
        bounds,
        workspace: workspace.or(current_workspace),
        focused: active_window == Some(window_id),
        hidden,
        client_type: Some("x11".to_string()),
        backend: XDOTOOL_BACKEND.to_string(),
        terminal: None,
    })
}

pub(crate) fn parse_geometry(output: &str) -> Option<WindowBounds> {
    let value = |key: &str| {
        output.lines().find_map(|line| {
            line.strip_prefix(key)
                .and_then(|value| value.trim().parse::<i32>().ok())
        })
    };
    Some(WindowBounds {
        x: value("X="),
        y: value("Y="),
        width: u32::try_from(value("WIDTH=")?).ok()?,
        height: u32::try_from(value("HEIGHT=")?).ok()?,
    })
}

pub(crate) fn parse_window_properties(
    output: &str,
) -> (Option<String>, Option<String>, Option<i32>, bool) {
    let mut classes = Vec::new();
    let mut workspace = None;
    let mut hidden = false;
    for line in output.lines() {
        if line.starts_with("WM_CLASS") {
            classes = line
                .split('=')
                .nth(1)
                .into_iter()
                .flat_map(|value| value.split(','))
                .map(|value| value.trim().trim_matches('"'))
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .collect();
        } else if line.starts_with("_NET_WM_DESKTOP") {
            workspace = line
                .split('=')
                .nth(1)
                .and_then(|value| value.trim().parse::<u32>().ok())
                .and_then(|value| (value != u32::MAX).then_some(value as i32));
        } else if line.starts_with("_NET_WM_STATE") {
            hidden = line.contains("_NET_WM_STATE_HIDDEN");
        }
    }
    let instance = classes.first().cloned();
    let class = classes.get(1).cloned().or_else(|| instance.clone());
    (class.clone(), class.or(instance), workspace, hidden)
}

fn command_text(command: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(command).args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
}

fn command_detail(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stderr.is_empty() {
        stdout
    } else {
        stderr
    }
}

fn is_x11_session() -> bool {
    env::var("XDG_SESSION_TYPE")
        .ok()
        .is_some_and(|value| value.eq_ignore_ascii_case("x11"))
        || (env::var_os("DISPLAY").is_some() && env::var_os("WAYLAND_DISPLAY").is_none())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_root_window_state() {
        let output = "_NET_CLIENT_LIST_STACKING(WINDOW): window id # 0x6000004, 0x6400004\n_NET_ACTIVE_WINDOW(WINDOW): window id # 0x6000004\n_NET_CURRENT_DESKTOP(CARDINAL) = 2\n";
        let (windows, active, workspace) = parse_root_window_state(output);
        assert_eq!(windows, vec![0x6000004, 0x6400004]);
        assert_eq!(active, Some(0x6000004));
        assert_eq!(workspace, Some(2));
    }

    #[test]
    fn parses_geometry_shell_output() {
        let bounds =
            parse_geometry("WINDOW=100\nX=12\nY=34\nWIDTH=1280\nHEIGHT=900\nSCREEN=0\n").unwrap();
        assert_eq!(bounds.x, Some(12));
        assert_eq!(bounds.y, Some(34));
        assert_eq!(bounds.width, 1280);
        assert_eq!(bounds.height, 900);
    }

    #[test]
    fn parses_x11_class_workspace_and_hidden_state() {
        let properties = "WM_CLASS(STRING) = \"github desktop\", \"GitHub Desktop\"\n_NET_WM_DESKTOP(CARDINAL) = 0\n_NET_WM_STATE(ATOM) = _NET_WM_STATE_HIDDEN\n";
        let (app_id, wm_class, workspace, hidden) = parse_window_properties(properties);
        assert_eq!(app_id.as_deref(), Some("GitHub Desktop"));
        assert_eq!(wm_class.as_deref(), Some("GitHub Desktop"));
        assert_eq!(workspace, Some(0));
        assert!(hidden);
    }
}
