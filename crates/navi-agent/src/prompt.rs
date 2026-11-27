//! System prompt assembly and prompt template argument expansion.
//!
//! The prompt layout follows the coding agent's structured system prompt: an
//! untagged preamble followed by tagged sections that a transcript can replace
//! independently. Placeholder expansion follows shell-style prompt templates:
//! replacements are taken from the template only and are never rescanned.

use std::collections::{BTreeMap, BTreeSet};

/// Preamble used when a context does not provide a base prompt.
const DEFAULT_BASE_PROMPT: &str = "You are a coding assistant running inside Navi, a coding agent harness. Help users inspect files, run commands, and modify code.";

/// Reserved section names are emitted once by the assembler itself.
const RESERVED_SECTIONS: [&str; 4] = ["preamble", "tools", "rules", "cwd"];

/// Inputs for [`assemble_system_prompt`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptContext {
    /// Untagged first paragraph. Empty uses the built-in default preamble.
    pub base_prompt: String,
    /// Working directory, normalized to forward slashes in the output.
    pub cwd: String,
    /// Names of tools available to the model.
    pub tools: Vec<String>,
    /// Additional tagged sections, keyed by their preferred tag name.
    pub extra_sections: BTreeMap<String, String>,
    /// Project-specific guidelines added to the rules section.
    pub project_rules: Vec<String>,
}

/// Assemble the complete system prompt from a context.
///
/// Each block after the preamble is wrapped in a tag of the same name so a
/// later update can replace a single section. Extra sections are emitted in
/// key order; names that are not valid tag names are normalized instead of
/// being rejected, so their content is never silently dropped.
pub fn assemble_system_prompt(ctx: &PromptContext) -> String {
    let mut sections = Vec::with_capacity(4 + ctx.extra_sections.len());

    if ctx.base_prompt.is_empty() {
        sections.push(DEFAULT_BASE_PROMPT.to_string());
    } else {
        sections.push(ctx.base_prompt.clone());
    }

    let tools = if ctx.tools.is_empty() {
        "(none)".to_string()
    } else {
        ctx.tools
            .iter()
            .map(|tool| format!("- {tool}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    sections.push(format!(
        "<tools>\n{tools}\n\nAdditional tools may be available.\n</tools>"
    ));

    sections.push(format!("<rules>\n{}\n</rules>", build_rules(ctx)));

    let cwd = ctx.cwd.replace('\\', "/");
    sections.push(format!("<cwd>\n{cwd}\n</cwd>"));

    let mut emitted: BTreeSet<String> = BTreeSet::new();
    for (name, content) in &ctx.extra_sections {
        if content.is_empty() {
            continue;
        }
        let tag = sanitize_section_name(name);
        if !emitted.insert(tag.clone()) {
            continue;
        }
        sections.push(format!("<{tag}>\n{content}\n</{tag}>"));
    }

    sections.join("\n\n")
}

/// Expand template placeholders using the supplied positional arguments.
///
/// Supported forms are `$1`, `$2`, ..., `$@`, `$ARGUMENTS`,
/// `${N:-default}`, `${@:-default}`, `${ARGUMENTS:-default}`, `${@:N}` and
/// `${@:N:L}`. Indices are one-based; missing or out-of-range arguments become
/// empty strings, and malformed or unclosed placeholders are kept verbatim.
/// Argument values are inserted as text and are never expanded again.
pub fn expand_template(template: &str, args: &[&str]) -> String {
    let all_args = args.join(" ");
    let mut out = String::with_capacity(template.len());
    let mut skip_until = 0usize;

    for (index, ch) in template.char_indices() {
        if index < skip_until {
            continue;
        }
        if ch == '$'
            && let Some((consumed, value)) = parse_placeholder(&template[index..], args, &all_args)
        {
            out.push_str(&value);
            skip_until = index + consumed;
            continue;
        }
        out.push(ch);
    }

    out
}

/// Build the deduplicated bullet list used in the `<rules>` section.
fn build_rules(ctx: &PromptContext) -> String {
    let mut seen = BTreeSet::new();
    let mut rules = Vec::new();

    let has = |name: &str| ctx.tools.iter().any(|tool| tool.as_str() == name);
    let has_bash = has("bash");
    let has_powershell = has("powershell");
    if (has_bash || has_powershell) && !has("grep") && !has("find") && !has("ls") {
        if has_bash && has_powershell {
            add_rule(
                &mut seen,
                &mut rules,
                "Use bash or PowerShell for file operations like listing, searching, and finding files",
            );
        } else if has_powershell {
            add_rule(
                &mut seen,
                &mut rules,
                "Use PowerShell for file operations like listing, searching, and finding files",
            );
        } else {
            add_rule(
                &mut seen,
                &mut rules,
                "Use bash for file operations like ls, rg, find",
            );
        }
    }

    for rule in &ctx.project_rules {
        add_rule(&mut seen, &mut rules, rule);
    }
    add_rule(&mut seen, &mut rules, "Be concise in your responses");
    add_rule(
        &mut seen,
        &mut rules,
        "Show file paths clearly when working with files",
    );

    rules
        .into_iter()
        .map(|rule| format!("- {rule}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Append a trimmed rule once, ignoring empty entries.
fn add_rule(seen: &mut BTreeSet<String>, rules: &mut Vec<String>, rule: &str) {
    let normalized = rule.trim();
    if normalized.is_empty() || !seen.insert(normalized.to_string()) {
        return;
    }
    rules.push(normalized.to_string());
}

/// Convert an arbitrary section name into a safe lowercase tag.
fn sanitize_section_name(name: &str) -> String {
    let mut tag: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();

    let starts_with_letter = tag
        .as_bytes()
        .first()
        .is_some_and(|byte| byte.is_ascii_lowercase());
    if !starts_with_letter || RESERVED_SECTIONS.contains(&tag.as_str()) {
        tag.insert_str(0, "section_");
    }
    tag
}

/// Parse a placeholder at the start of `rest`, returning its byte length and value.
fn parse_placeholder(rest: &str, args: &[&str], all_args: &str) -> Option<(usize, String)> {
    let bytes = rest.as_bytes();
    if bytes.first() != Some(&b'$') {
        return None;
    }

    if bytes.get(1) == Some(&b'{') {
        let mut closing = None;
        for (offset, byte) in bytes.iter().enumerate().skip(2) {
            if *byte == b'}' {
                closing = Some(offset);
                break;
            }
        }
        let closing = closing?;
        let value = expand_braced(&rest[2..closing], args, all_args)?;
        return Some((closing + 1, value));
    }

    if bytes.get(1) == Some(&b'@') {
        return Some((2, all_args.to_string()));
    }
    if rest[1..].starts_with("ARGUMENTS") {
        return Some((1 + "ARGUMENTS".len(), all_args.to_string()));
    }

    let mut end = 1usize;
    while bytes.get(end).is_some_and(|byte| byte.is_ascii_digit()) {
        end += 1;
    }
    if end == 1 {
        return None;
    }
    let value = parse_usize(&rest[1..end]).map_or("", |index| positional(args, index));
    Some((end, value.to_string()))
}

/// Expand the contents of a `${...}` placeholder.
fn expand_braced(inner: &str, args: &[&str], all_args: &str) -> Option<String> {
    if let Some((target, default)) = inner.split_once(":-") {
        if target == "@" || target == "ARGUMENTS" {
            return Some(if all_args.is_empty() {
                default.to_string()
            } else {
                all_args.to_string()
            });
        }
        let value = parse_usize(target).map_or("", |index| positional(args, index));
        return Some(if value.is_empty() {
            default.to_string()
        } else {
            value.to_string()
        });
    }

    let spec = inner.strip_prefix("@:")?;
    let (start_text, length_text) = match spec.split_once(':') {
        Some((start, length)) => (start, Some(length)),
        None => (spec, None),
    };
    let Some(start) = parse_usize(start_text) else {
        return Some(String::new());
    };
    let from = start.saturating_sub(1);
    let to = match length_text {
        Some(length) => from.saturating_add(parse_usize(length).unwrap_or(usize::MAX)),
        None => args.len(),
    };
    if from >= args.len() {
        return Some(String::new());
    }
    Some(
        args.get(from..to.min(args.len()))
            .map_or_else(String::new, |slice| slice.join(" ")),
    )
}

/// Parse a non-negative decimal index without overflowing or panicking.
fn parse_usize(text: &str) -> Option<usize> {
    if text.is_empty() {
        return None;
    }
    let mut value = 0usize;
    for byte in text.bytes() {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value
            .checked_mul(10)?
            .checked_add(usize::from(byte - b'0'))?;
    }
    Some(value)
}

/// Look up a one-based argument, returning an empty string when absent.
fn positional<'a>(args: &'a [&str], index: usize) -> &'a str {
    match index.checked_sub(1) {
        Some(offset) => args.get(offset).map_or("", |value| *value),
        None => "",
    }
}

#[cfg(test)]
mod tests {
    use super::{PromptContext, assemble_system_prompt, expand_template};
    use std::collections::BTreeMap;

    fn context() -> PromptContext {
        PromptContext {
            base_prompt: "You are Navi.".to_string(),
            cwd: "/work/navi".to_string(),
            tools: vec!["read".to_string(), "bash".to_string()],
            extra_sections: BTreeMap::from([("environment".to_string(), "Linux".to_string())]),
            project_rules: vec!["Keep diffs small".to_string()],
        }
    }

    #[test]
    fn assemble_includes_every_context_part() {
        let prompt = assemble_system_prompt(&context());

        assert!(prompt.starts_with("You are Navi."));
        assert!(prompt.contains("<cwd>\n/work/navi\n</cwd>"));
        assert!(prompt.contains("- read\n- bash"));
        assert!(prompt.contains("- Keep diffs small"));
        assert!(prompt.contains("<environment>\nLinux\n</environment>"));
        assert!(prompt.contains("Be concise in your responses"));
    }

    #[test]
    fn assemble_falls_back_to_default_preamble() {
        let ctx = PromptContext {
            base_prompt: String::new(),
            ..context()
        };
        let prompt = assemble_system_prompt(&ctx);

        assert!(prompt.starts_with("You are a coding assistant running inside Navi"));
        assert!(prompt.contains("<cwd>"));
    }

    #[test]
    fn assemble_deduplicates_trims_and_orders_rules() {
        let mut ctx = context();
        ctx.project_rules = vec![
            "  Keep diffs small  ".to_string(),
            "Keep diffs small".to_string(),
            String::new(),
            "Show file paths clearly when working with files".to_string(),
            "Use bash for file operations like ls, rg, find".to_string(),
        ];
        let prompt = assemble_system_prompt(&ctx);
        let rules = prompt.split_once("<rules>\n").map_or("", |(_, rest)| {
            rest.split_once("\n</rules>").map_or("", |(body, _)| body)
        });

        assert_eq!(
            rules,
            "- Use bash for file operations like ls, rg, find\n- Keep diffs small\n- Show file paths clearly when working with files\n- Be concise in your responses"
        );
    }

    #[test]
    fn assemble_handles_windows_cwd_and_no_tools() {
        let ctx = PromptContext {
            cwd: "C:\\Users\\navi".to_string(),
            tools: Vec::new(),
            extra_sections: BTreeMap::new(),
            ..context()
        };
        let prompt = assemble_system_prompt(&ctx);

        assert!(prompt.contains("<cwd>\nC:/Users/navi\n</cwd>"));
        assert!(prompt.contains("(none)"));
        assert!(!prompt.contains("<environment>"));
    }

    #[test]
    fn assemble_normalizes_unsafe_section_names() {
        let ctx = PromptContext {
            extra_sections: BTreeMap::from([
                ("Bad Name".to_string(), "kept".to_string()),
                ("tools".to_string(), "shadow".to_string()),
                ("Empty".to_string(), String::new()),
            ]),
            ..context()
        };
        let prompt = assemble_system_prompt(&ctx);

        assert!(prompt.contains("<bad_name>\nkept\n</bad_name>"));
        assert!(prompt.contains("<section_tools>\nshadow\n</section_tools>"));
        assert!(!prompt.contains("</empty>"));
    }

    #[test]
    fn expand_positional_and_all_argument_forms() {
        let args = ["alpha", "beta", "gamma"];
        assert_eq!(
            expand_template("$1 / $2 / $3", &args),
            "alpha / beta / gamma"
        );
        assert_eq!(
            expand_template("$@|$ARGUMENTS", &args),
            "alpha beta gamma|alpha beta gamma"
        );
        assert_eq!(expand_template("$1$1", &args), "alphaalpha");
        assert_eq!(expand_template("no placeholders", &args), "no placeholders");
    }

    #[test]
    fn expand_missing_and_overflowing_indices_are_safe() {
        let args = ["only"];
        assert_eq!(expand_template("$2", &args), "");
        assert_eq!(expand_template("$0", &args), "");
        assert_eq!(
            expand_template("$999999999999999999999999999999", &args),
            ""
        );
        assert_eq!(expand_template("$1 $9 $@", &[]), "  ");
        assert_eq!(expand_template("$- $x $1", &args), "$- $x only");
    }

    #[test]
    fn expand_argument_slices() {
        let args = ["a", "b", "c", "d"];
        assert_eq!(expand_template("${@:2}", &args), "b c d");
        assert_eq!(expand_template("${@:1:2}", &args), "a b");
        assert_eq!(expand_template("${@:0}", &args), "a b c d");
        assert_eq!(expand_template("${@:9}", &args), "");
        assert_eq!(expand_template("${@:2:100}", &args), "b c d");
        assert_eq!(expand_template("${@:3:0}", &args), "");
    }

    #[test]
    fn expand_default_forms() {
        let args = ["value", ""];
        assert_eq!(expand_template("${1:-fallback}", &args), "value");
        assert_eq!(expand_template("${2:-fallback}", &args), "fallback");
        assert_eq!(expand_template("${3:-fallback}", &args), "fallback");
        assert_eq!(expand_template("${@:-fallback}", &args), "value ");
        assert_eq!(expand_template("${ARGUMENTS:-fallback}", &[]), "fallback");
        assert_eq!(expand_template("${@:9}", &[]), "");
    }

    #[test]
    fn expand_does_not_rescan_argument_text() {
        let args = ["$1", "${@:2}", "$ARGUMENTS"];
        assert_eq!(expand_template("$1", &args), "$1");
        assert_eq!(expand_template("$@", &args), "$1 ${@:2} $ARGUMENTS");
        assert_eq!(expand_template("${1:-$2}", &args), "$1");
    }

    #[test]
    fn expand_preserves_unicode_and_malformed_placeholders() {
        let args = ["x"];
        assert_eq!(expand_template("héllo $1 ✓", &args), "héllo x ✓");
        assert_eq!(expand_template("${@:2", &args), "${@:2");
        assert_eq!(expand_template("${unknown}", &args), "${unknown}");
        assert_eq!(expand_template("cost $5", &args), "cost ");
        assert_eq!(expand_template("$", &args), "$");
    }
}
