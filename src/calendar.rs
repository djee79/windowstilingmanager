//! Upcoming-meetings feed for the bar's calendar chip. A background thread
//! fetches the next 24 hours of events through a hidden PowerShell helper —
//! from classic Outlook (COM automation, fully offline, no cloud/OAuth) or
//! from any .ics file or URL (Google Calendar, Proton, Nextcloud, …).
//! Strictly opt-in: with no calendar_source configured, nothing here runs.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Default)]
pub struct Meeting {
    /// Unix epoch seconds.
    pub start: u64,
    pub end: u64,
    /// "HH:mm" in local time, pre-formatted by the helper.
    pub start_hm: String,
    pub end_hm: String,
    pub subject: String,
    pub location: String,
    /// Teams meeting link found in the body/description, if any.
    pub join_url: String,
}

static MEETINGS: Mutex<Vec<Meeting>> = Mutex::new(Vec::new());
static SOURCE: Mutex<String> = Mutex::new(String::new());
static REFRESH_MIN: AtomicU64 = AtomicU64::new(5);
static STARTED: AtomicBool = AtomicBool::new(false);

pub fn now_epoch() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Remember the configured source and start the refresher thread the first
/// time a source appears. Called at startup and on config hot-reload.
pub fn configure(source: &str, refresh_min: u32) {
    let source = source.trim().to_string();
    REFRESH_MIN.store(refresh_min.clamp(1, 120) as u64, Ordering::Relaxed);
    let changed = {
        let mut slot = SOURCE.lock().unwrap();
        let changed = *slot != source;
        *slot = source.clone();
        changed
    };
    if source.is_empty() {
        MEETINGS.lock().unwrap().clear();
        return;
    }
    if !STARTED.swap(true, Ordering::SeqCst) {
        std::thread::spawn(refresher);
    } else if changed {
        MEETINGS.lock().unwrap().clear(); // stale data from the old source
    }
}

/// Everything still relevant (ongoing or upcoming), soonest first.
pub fn agenda() -> Vec<Meeting> {
    let now = now_epoch();
    MEETINGS.lock().unwrap().iter().filter(|m| m.end >= now).cloned().collect()
}

/// What the bar chip shows: the ongoing meeting, or the next one within 12h.
pub fn next_meeting() -> Option<Meeting> {
    let now = now_epoch();
    agenda().into_iter().find(|m| m.start <= now + 12 * 3600)
}

fn refresher() {
    let mut last_err = String::new();
    loop {
        let source = SOURCE.lock().unwrap().clone();
        if !source.is_empty() {
            match fetch(&source) {
                Ok(list) => {
                    *MEETINGS.lock().unwrap() = list;
                    last_err.clear();
                }
                // Log each distinct failure once, not every refresh.
                Err(e) if e != last_err => {
                    crate::logln!("wtm: calendar: {e}");
                    last_err = e;
                }
                Err(_) => {}
            }
        }
        let mins = REFRESH_MIN.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(mins * 60));
    }
}

/// Run the PowerShell helper for `source` and parse its TSV output.
fn fetch(source: &str) -> Result<Vec<Meeting>, String> {
    let script = if source.eq_ignore_ascii_case("outlook") {
        OUTLOOK_PS.to_string()
    } else {
        ICS_PS.replace("__SRC__", &source.replace('\'', "''"))
    };
    let dir = std::env::var_os("LOCALAPPDATA")
        .map(|d| std::path::PathBuf::from(d).join("wtm"))
        .ok_or("LOCALAPPDATA not set")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join("calendar.ps1");
    std::fs::write(&path, script).map_err(|e| e.to_string())?;

    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&path)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("helper failed: {}", err.lines().next().unwrap_or("(no output)")));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut list: Vec<Meeting> = text.lines().filter_map(parse_line).collect();
    list.sort_by_key(|m| m.start);
    list.truncate(20);
    Ok(list)
}

fn parse_line(line: &str) -> Option<Meeting> {
    let mut f = line.trim_end().split('\t');
    let start: u64 = f.next()?.trim().parse().ok()?;
    let end: u64 = f.next()?.trim().parse().ok()?;
    let start_hm = f.next()?.to_string();
    let end_hm = f.next()?.to_string();
    let subject = f.next()?.trim().to_string();
    let location = f.next().unwrap_or("").trim().to_string();
    let join_url = f.next().unwrap_or("").trim().to_string();
    (!subject.is_empty())
        .then_some(Meeting { start, end, start_hm, end_hm, subject, location, join_url })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_helper_lines() {
        let m = parse_line("100\t200\t10:00\t10:30\tStandup\tRoom A\thttps://x").unwrap();
        assert_eq!((m.start, m.end), (100, 200));
        assert_eq!(m.subject, "Standup");
        assert_eq!(m.location, "Room A");
        assert_eq!(m.join_url, "https://x");
    }

    #[test]
    fn tolerates_missing_trailing_fields_and_garbage() {
        let m = parse_line("1\t2\t09:00\t09:30\tSolo").unwrap();
        assert!(m.location.is_empty() && m.join_url.is_empty());
        assert!(parse_line("garbage").is_none());
        assert!(parse_line("1\t2\t09:00\t09:30\t").is_none()); // empty subject
        assert!(parse_line("").is_none());
    }
}

/// Classic Outlook via COM: works offline and inside locked-down networks —
/// no Graph API, no OAuth, no admin consent. Needs classic Outlook installed
/// (New Outlook has no COM interface).
const OUTLOOK_PS: &str = r#"$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$ol = New-Object -ComObject Outlook.Application
$items = $ol.GetNamespace('MAPI').GetDefaultFolder(9).Items
$items.IncludeRecurrences = $true
$items.Sort('[Start]')
$a = (Get-Date).AddMinutes(-15)
$b = (Get-Date).AddHours(24)
$filter = "[Start] >= '" + $a.ToString('g') + "' AND [Start] <= '" + $b.ToString('g') + "'"
$n = 0
foreach ($i in $items.Restrict($filter)) {
    if ($n -ge 20) { break }
    if ($i.AllDayEvent) { continue }
    $n++
    $s = [DateTimeOffset]::new($i.Start).ToUnixTimeSeconds()
    $e = [DateTimeOffset]::new($i.End).ToUnixTimeSeconds()
    $subject = ($i.Subject -replace "[`t`r`n]+", ' ').Trim()
    $loc = ($i.Location -replace "[`t`r`n]+", ' ').Trim()
    $url = ''
    try {
        if ($i.Body -match 'https://teams\.microsoft\.com/l/meetup-join/[^\s"<>]+') { $url = $Matches[0] }
    } catch {}
    [Console]::Out.WriteLine("$s`t$e`t" + $i.Start.ToString('HH:mm') + "`t" + $i.End.ToString('HH:mm') + "`t$subject`t$loc`t$url")
}
"#;

/// Any .ics file or URL. Recurring events are not expanded (only literal
/// DTSTART instances inside the window show up) — good enough for published
/// Google/Proton/Nextcloud calendars, which expand recurrences server-side.
const ICS_PS: &str = r#"$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$src = '__SRC__'
if ($src -match '^https?://') {
    $text = (Invoke-WebRequest -UseBasicParsing -Uri $src).Content
} else {
    $text = [System.IO.File]::ReadAllText($src)
}
# Unfold RFC 5545 folded lines before parsing.
$text = $text -replace "`r`n[ `t]", '' -replace "`n[ `t]", ''
$now = [DateTimeOffset]::Now.ToUnixTimeSeconds()
$lo = $now - 900
$hi = $now + 86400
function ParseStamp([string]$v) {
    if ($v -match '^(\d{8}T\d{6})(Z?)$') {
        $dt = [DateTime]::ParseExact($Matches[1], "yyyyMMdd'T'HHmmss", $null)
        if ($Matches[2] -eq 'Z') { $dt = [DateTime]::SpecifyKind($dt, 'Utc') }
        return [DateTimeOffset]::new($dt).ToUnixTimeSeconds()
    }
    return $null
}
$n = 0
foreach ($block in ($text -split 'BEGIN:VEVENT')) {
    if ($n -ge 20) { break }
    if ($block -notmatch 'END:VEVENT') { continue }
    $p = @{}
    foreach ($line in ($block -split "`r?`n")) {
        if ($line -match '^(DTSTART|DTEND|SUMMARY|LOCATION|DESCRIPTION)[^:]*:(.*)$') {
            if (-not $p.ContainsKey($Matches[1])) { $p[$Matches[1]] = $Matches[2] }
        }
    }
    if (-not $p['DTSTART']) { continue }
    $s = ParseStamp $p['DTSTART']
    if ($null -eq $s) { continue }
    $e = ParseStamp $p['DTEND']
    if ($null -eq $e) { $e = $s + 3600 }
    if ($e -lt $lo -or $s -gt $hi) { continue }
    $n++
    $subject = (("" + $p['SUMMARY']) -replace '\\,', ',' -replace "[`t`r`n]+", ' ').Trim()
    $loc = (("" + $p['LOCATION']) -replace '\\,', ',' -replace "[`t`r`n]+", ' ').Trim()
    $url = ''
    $body = ("" + $p['DESCRIPTION']) + ' ' + $loc
    if ($body -match 'https://teams\.microsoft\.com/l/meetup-join/[^\s"<>\\]+') { $url = $Matches[0] }
    $shm = [DateTimeOffset]::FromUnixTimeSeconds($s).ToLocalTime().ToString('HH:mm')
    $ehm = [DateTimeOffset]::FromUnixTimeSeconds($e).ToLocalTime().ToString('HH:mm')
    [Console]::Out.WriteLine("$s`t$e`t$shm`t$ehm`t$subject`t$loc`t$url")
}
"#;
