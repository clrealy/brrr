//! Turns printer-speak into words a person would say.
use crate::ipp::{self, Group};

pub fn status(code: u16) -> String {
    let s = match code {
        0x0400 => "the printer didn't understand the request",
        0x0401 => "the printer said forbidden",
        0x0402 | 0x0403 => "the printer wants a login",
        0x0404 => "the printer can't do that right now",
        0x0405 => "the printer timed out waiting",
        0x0406 => "that job doesn't exist (anymore)",
        0x0407 => "that job is gone",
        0x040A => "the printer doesn't accept that file type",
        0x040B => "the printer rejected one of those settings",
        0x0500 => "the printer's firmware hit an internal error",
        0x0501 => "the printer doesn't support that",
        0x0502 => "the printer is unavailable right now",
        0x0504 => "the printer reported a hardware error",
        0x0505 => "the printer had a temporary error, try again",
        0x0506 => "the printer isn't accepting jobs",
        0x0507 => "the printer is busy",
        0x0508 => "the job got canceled",
        _ => return format!("IPP status 0x{code:04x}"),
    };
    s.to_string()
}

fn base(r: &str) -> &str {
    r.trim_end_matches("-error").trim_end_matches("-warning").trim_end_matches("-report")
}

pub fn reason(r: &str) -> Option<String> {
    let b = base(r);
    let s = match b {
        "none" | "paused" | "moving-to-paused" => return None,
        "media-empty" | "media-needed" => "out of paper",
        "media-low" => "paper is running low",
        "media-jam" => "paper jam (rip)",
        "input-tray-missing" => "the paper tray isn't in",
        "output-tray-missing" => "the output tray isn't in",
        "output-area-full" | "output-area-almost-full" => "the output tray is full",
        "toner-low" => "toner is low",
        "toner-empty" => "out of toner",
        "marker-supply-low" => "ink is low",
        "marker-supply-empty" => "out of ink",
        "marker-waste-almost-full" => "waste ink tank is almost full",
        "marker-waste-full" => "waste ink tank is full",
        "cover-open" => "a cover is open",
        "door-open" => "a door is open",
        "interlock-open" => "something's open on the printer",
        "fuser-over-temp" | "fuser-under-temp" => "the fuser temp is off, give it a minute",
        "offline" | "shutdown" => "the printer is offline",
        "connecting-to-device" => "still connecting to the printer",
        "timed-out" => "the printer stopped responding",
        "spool-area-full" => "the printer's memory is full",
        "stopped-partly" => "some of the printer's features are stopped",
        "cups-waiting-for-job-completed" => return None,
        other => return Some(other.replace('-', " ")),
    };
    Some(s.to_string())
}

/// Reasons that physically stop a print, as opposed to FYIs.
pub fn is_blocking(r: &str) -> bool {
    !r.ends_with("-warning") && !r.ends_with("-report") && !matches!(base(r), "none" | "paused" | "media-low" | "toner-low" | "marker-supply-low")
}

pub fn state(g: &Group) -> String {
    let s = match ipp::int(g, "printer-state") {
        Some(3) => "ready",
        Some(4) => "printing",
        Some(5) => "stopped",
        _ => "unknown",
    };
    let extra: Vec<String> = ipp::texts(g, "printer-state-reasons").iter().filter_map(|r| reason(r)).collect();
    if extra.is_empty() { s.to_string() } else { format!("{s} - {}", extra.join(", ")) }
}

pub fn ink(g: &Group) -> Vec<String> {
    let names = ipp::texts(g, "marker-names");
    let levels = ipp::ints(g, "marker-levels");
    names.iter().zip(levels.iter()).map(|(n, l)| {
        if *l < 0 { format!("{n}: ??") } else { format!("{n}: {l}%") }
    }).collect()
}

pub fn job_state(s: i32) -> &'static str {
    match s {
        3 => "waiting",
        4 => "held",
        5 => "printing",
        6 => "stopped",
        7 => "canceled",
        8 => "failed",
        9 => "done",
        _ => "unknown",
    }
}
