use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Token counts for one agent session, read incrementally from the agent's own log.
#[derive(Default)]
pub struct Usage {
    pub model: String,
    pub context: u64,
    pub input: u64,
    pub cached: u64,
    pub output: u64,
    pub credits: f64,
    pub premium: u64,
    /// output tokens per minute (minutes since the Unix epoch), from the log's timestamps
    pub timeline: BTreeMap<u64, u64>,
    prompt_limit: u64,
    copilot: bool,
    path: PathBuf,
    offset: u64,
    // claude: finished messages as (input, cache write, cache read, output), plus the latest
    // message (id, counts, minute), whose lines repeat with the same id until the next one starts
    done: [u64; 4],
    cur: Option<(String, [u64; 4], u64)>,
}

/// Where the agent logs a session: Claude's transcript or Copilot's event stream.
pub fn log_path(copilot: bool, sid: &str, cwd: &Path) -> Option<PathBuf> {
    if sid.is_empty() {
        return None;
    }
    let home = PathBuf::from(std::env::var("HOME").ok()?);
    Some(if copilot {
        home.join(".copilot/session-state").join(sid).join("events.jsonl")
    } else {
        let slug: String =
            cwd.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
        home.join(".claude/projects").join(slug).join(format!("{sid}.jsonl"))
    })
}

/// Minutes since the Unix epoch for an RFC 3339 UTC timestamp ("2026-09-14T02:04:52.692Z").
pub fn epoch_minute(ts: &str) -> Option<u64> {
    let n = |at: usize, len: usize| ts.get(at..at + len)?.parse::<i64>().ok();
    let (y, m, d, h, min) = (n(0, 4)?, n(5, 2)?, n(8, 2)?, n(11, 2)?, n(14, 2)?);
    // days from civil date (H. Hinnant)
    let y = if m <= 2 { y - 1 } else { y };
    let (era, yoe) = (y.div_euclid(400), y.rem_euclid(400));
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    u64::try_from(days * 1440 + h * 60 + min).ok()
}

impl Usage {
    /// Tokens the model accepts per request: what the progress bar measures against.
    /// Copilot logs it; current Claude models take 1M except Haiku (200K).
    pub fn limit(&self) -> u64 {
        match (self.prompt_limit, self.copilot) {
            (n, _) if n > 0 => n,
            (_, true) => 272_000,
            _ if self.model.contains("haiku") => 200_000,
            _ => 1_000_000,
        }
    }

    pub fn is_copilot(&self) -> bool {
        self.copilot
    }

    /// Reads whatever the log gained since the last call; a partial last line waits for its newline.
    pub fn refresh(&mut self, path: &Path, copilot: bool) {
        let Ok(mut f) = File::open(path) else { return };
        if path != self.path || f.metadata().is_ok_and(|m| m.len() < self.offset) {
            *self = Usage { path: path.to_path_buf(), copilot, ..Default::default() };
        }
        if f.seek(SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut reader = BufReader::new(f);
        let mut buf = Vec::new();
        while matches!(reader.read_until(b'\n', &mut buf), Ok(1..)) && buf.ends_with(b"\n") {
            self.offset += buf.len() as u64;
            let line = String::from_utf8_lossy(&buf);
            if copilot { self.copilot(&line) } else { self.claude(&line) }
            buf.clear();
        }
        if !copilot {
            let t: [u64; 4] =
                std::array::from_fn(|i| self.done[i] + self.cur.as_ref().map_or(0, |(_, c, _)| c[i]));
            (self.input, self.cached, self.output) = (t[0] + t[1], t[2], t[3]);
        }
    }

    fn claude(&mut self, line: &str) {
        if !line.contains("\"usage\"") {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { return };
        let m = &v["message"];
        let (Some(id), Some(u)) = (m["id"].as_str(), m.get("usage")) else { return };
        let n = |k: &str| u[k].as_u64().unwrap_or(0);
        let now = [
            n("input_tokens"),
            n("cache_creation_input_tokens"),
            n("cache_read_input_tokens"),
            n("output_tokens"),
        ];
        let minute = v["timestamp"].as_str().and_then(epoch_minute).unwrap_or(0);
        match &mut self.cur {
            Some((cur_id, c, at)) if cur_id == id => {
                *self.timeline.entry(*at).or_default() += now[3].saturating_sub(c[3]);
                *c = now;
            }
            cur => {
                if let Some((_, c, _)) = cur.take() {
                    (0..4).for_each(|i| self.done[i] += c[i]);
                }
                *self.timeline.entry(minute).or_default() += now[3];
                *cur = Some((id.to_string(), now, minute));
            }
        }
        if v["isSidechain"] != true {
            self.context = now[0] + now[1] + now[2];
            if let Some(model) = m["model"].as_str().filter(|m| !m.starts_with('<')) {
                self.model = model.to_string();
            }
        }
    }

    fn copilot(&mut self, line: &str) {
        if let Some(n) = number_after(line, "\"max_prompt_tokens\":") {
            self.prompt_limit = n;
        }
        let head = line.get(..48).unwrap_or(line);
        if head.contains("\"assistant.message\"") {
            // these lines are large (full message + reasoning); skip the JSON parse
            let out = number_after(line, "\"outputTokens\":").unwrap_or(0);
            self.output += out;
            let at = line.rfind("\"timestamp\":\"").and_then(|i| epoch_minute(&line[i + 13..]));
            *self.timeline.entry(at.unwrap_or(0)).or_default() += out;
            return;
        }
        if !["session.start", "usage_checkpoint", "session.shutdown", "model_change"]
            .iter()
            .any(|t| head.contains(t))
        {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { return };
        let d = &v["data"];
        let model = d["currentModel"].as_str().or(d["selectedModel"].as_str()).or(d["newModel"].as_str());
        if let Some(m) = model.or(d["modelCacheState"][0]["modelId"].as_str()) {
            self.model = m.to_string();
        }
        if let Some(n) = d["totalNanoAiu"].as_u64() {
            self.credits = n as f64 / 1e9;
        }
        if let Some(n) = d["totalPremiumRequests"].as_u64() {
            self.premium = n;
        }
        let prompt = || d["promptCacheBreakState"][0]["models"].as_object()?.values().next()?["prompt_tokens"].as_u64();
        if let Some(n) = d["currentTokens"].as_u64().or_else(prompt) {
            self.context = n;
        }
        // only the shutdown summary carries input-side totals
        if d.get("tokenDetails").is_some() {
            let t = |k: &str| d["tokenDetails"][k]["tokenCount"].as_u64().unwrap_or(0);
            (self.input, self.cached) = (t("input") + t("cache_write"), t("cache_read"));
        }
    }
}

/// A plan usage window: percent used (0-100) and when it resets (Unix seconds, 0 if unknown).
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Window {
    pub used: f64,
    pub resets_at: u64,
}

/// Plan limits shown above a session: Claude's 5-hour and weekly windows, and Copilot's
/// monthly premium requests (with the month's entitlement).
#[derive(Default)]
pub struct Plan {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    pub copilot: Option<(Window, u64)>,
    pub copilot_checked: Option<Instant>,
}

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Where `chud --statusline` keeps the plan limits Claude Code hands its status line.
pub fn limits_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config/chud/claude-limits.json")
}

/// Claude's 5-hour and weekly windows from its status-line `rate_limits` object.
pub fn claude_windows(v: &Value, now: u64) -> [Option<Window>; 2] {
    ["five_hour", "seven_day"].map(|k| -> Option<Window> {
        let used = v[k]["used_percentage"].as_f64()?;
        let resets_at = v[k]["resets_at"].as_u64().unwrap_or(0);
        // past its reset a window starts empty again, until Claude reports the new one
        Some(Window { used: if resets_at > 0 && now >= resets_at { 0.0 } else { used }, resets_at })
    })
}

/// Copilot's monthly premium requests from `gh api /copilot_internal/user`.
pub fn copilot_quota(v: &Value) -> Option<(Window, u64)> {
    let q = &v["quota_snapshots"]["premium_interactions"];
    if q["unlimited"] == true {
        return None;
    }
    let used = 100.0 - q["percent_remaining"].as_f64()?;
    let date = v["quota_reset_date_utc"].as_str().or(v["quota_reset_date"].as_str()).unwrap_or("");
    let date = if date.len() == 10 { format!("{date}T00:00") } else { date.to_string() };
    let resets_at = epoch_minute(&date).map_or(0, |m| m * 60);
    Some((Window { used, resets_at }, q["entitlement"].as_u64().unwrap_or(0)))
}

fn number_after(s: &str, key: &str) -> Option<u64> {
    let rest = &s[s.find(key)? + key.len()..];
    rest[..rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len())].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp(name: &str, body: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("chud-{}-{name}", std::process::id()));
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn timestamps() {
        assert_eq!(epoch_minute("1970-01-01T00:01:00Z"), Some(1));
        assert_eq!(epoch_minute("2024-01-01T00:00:00.000Z"), Some(1_704_067_200 / 60));
        assert_eq!(epoch_minute("2026-09-14T02:04:52.692Z"), Some(epoch_minute("2026-09-14T00:00:00Z").unwrap() + 124));
        assert_eq!(epoch_minute("garbage"), None);
    }

    #[test]
    fn claude_counts_each_message_once() {
        let line = |id: &str, out: u64, min: u32| {
            format!(
                r#"{{"type":"assistant","timestamp":"2026-09-14T10:{min:02}:00.000Z","message":{{"id":"{id}","model":"claude-opus-5","usage":{{"input_tokens":10,"cache_creation_input_tokens":100,"cache_read_input_tokens":1000,"output_tokens":{out}}}}}}}"#
            ) + "\n"
        };
        let p = tmp("claude.jsonl", &(line("a", 5, 1) + &line("a", 7, 1) + &line("b", 3, 2)));
        let mut u = Usage::default();
        u.refresh(&p, false);
        assert_eq!((u.input, u.cached, u.output, u.context), (220, 2000, 10, 1110));
        assert_eq!((u.model.as_str(), u.limit()), ("claude-opus-5", 1_000_000));
        let m0 = epoch_minute("2026-09-14T10:00:00Z").unwrap();
        assert_eq!(u.timeline.get(&(m0 + 1)), Some(&7), "message a: its final count, once");

        // appended lines are picked up; a partial line waits for its newline
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        write!(f, "{}{}", line("b", 9, 3), &line("c", 1, 3)[..20]).unwrap();
        u.refresh(&p, false);
        assert_eq!(u.output, 16, "a:7 + b:9, c not complete yet");
        assert_eq!(u.timeline.get(&(m0 + 2)), Some(&9), "b's growth lands on b's first minute");
    }

    #[test]
    fn copilot_events() {
        let p = tmp(
            "events.jsonl",
            concat!(
                r#"{"type":"session.start","data":{"sessionId":"x","selectedModel":"gpt-5.6-terra"}}"#, "\n",
                r#"{"type":"model.turn_started","data":{"modelInfo":{"capabilities":{"limits":{"max_context_window_tokens":1050000,"max_prompt_tokens":272000}}}}}"#, "\n",
                r#"{"type":"assistant.message","data":{"messageId":"m1","content":"hi","outputTokens":486},"timestamp":"2026-09-14T10:01:30.000Z"}"#, "\n",
                r#"{"type":"assistant.message","data":{"messageId":"m2","content":"","outputTokens":14},"timestamp":"2026-09-14T10:01:59.000Z"}"#, "\n",
                r#"{"type":"session.usage_checkpoint","data":{"totalNanoAiu":135471690000,"totalPremiumRequests":1,"promptCacheBreakState":[{"models":{"gpt-5.6-terra":{"prompt_tokens":228076}}}]}}"#, "\n",
            ),
        );
        let mut u = Usage::default();
        u.refresh(&p, true);
        assert_eq!((u.output, u.context, u.premium, u.model.as_str()), (500, 228076, 1, "gpt-5.6-terra"));
        assert_eq!(u.limit(), 272_000, "the prompt limit, not the full window");
        assert!((u.credits - 135.47169).abs() < 1e-6);
        let m = epoch_minute("2026-09-14T10:01:00Z").unwrap();
        assert_eq!(u.timeline.get(&m), Some(&500));
    }

    #[test]
    fn plan_limits() {
        let v: Value = serde_json::from_str(
            r#"{"five_hour":{"used_percentage":23.5,"resets_at":2000},"seven_day":{"used_percentage":41.2,"resets_at":9000}}"#,
        )
        .unwrap();
        let [h, w] = claude_windows(&v, 1000);
        assert_eq!((h, w), (Some(Window { used: 23.5, resets_at: 2000 }), Some(Window { used: 41.2, resets_at: 9000 })));
        assert_eq!(claude_windows(&v, 3000)[0].unwrap().used, 0.0, "past its reset the 5h window is empty again");
        assert_eq!(claude_windows(&Value::Null, 0), [None, None]);

        let gh: Value = serde_json::from_str(
            r#"{"quota_reset_date":"2026-10-01","quota_snapshots":{"premium_interactions":{"entitlement":16100,"remaining":5044,"percent_remaining":31.3,"unlimited":false}}}"#,
        )
        .unwrap();
        let (q, total) = copilot_quota(&gh).unwrap();
        assert_eq!(total, 16100);
        assert!((q.used - 68.7).abs() < 1e-9);
        assert_eq!(q.resets_at, epoch_minute("2026-10-01T00:00Z").unwrap() * 60);
    }

    #[test]
    fn paths() {
        let home = std::env::var("HOME").unwrap();
        let claude = log_path(false, "abc", Path::new("/Users/me/Projects/chud")).unwrap();
        assert_eq!(claude, Path::new(&home).join(".claude/projects/-Users-me-Projects-chud/abc.jsonl"));
        let copilot = log_path(true, "abc", Path::new("/x")).unwrap();
        assert_eq!(copilot, Path::new(&home).join(".copilot/session-state/abc/events.jsonl"));
        assert_eq!(log_path(false, "", Path::new("/x")), None);
    }
}
