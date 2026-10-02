use std::{collections::BTreeMap, path::PathBuf};

use anyhow::{Result, bail};
use clap::{Parser, ValueEnum};
use maven_mcp::log_analysis::{
    CommandCategory, CommandEvent, EventFilter, LogSource, deduplicate_events,
    discover_default_sources, filter_events, normalize_timestamp, read_event_log, redact_events,
    sort_events, source_paths,
};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Extract and analyze real agent terminal commands without an MCP server or LLM"
)]
struct Cli {
    /// Log files or directories to inspect.
    #[arg(value_name = "PATH")]
    inputs: Vec<PathBuf>,
    /// Discover common VS Code, Codex, Kilo Code, and IntelliJ storage roots.
    #[arg(long)]
    discover: bool,
    /// Keep mirrored/progressive records instead of deduplicating call identities.
    #[arg(long)]
    keep_duplicates: bool,
    /// Explicit source format; auto infers it from the file and parent directory.
    #[arg(long, value_enum, default_value_t = SourceArg::Auto)]
    source: SourceArg,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Terminal)]
    format: OutputFormat,
    /// Write the report to a file instead of stdout.
    #[arg(long)]
    output: Option<PathBuf>,
    #[arg(long)]
    since: Option<String>,
    #[arg(long)]
    until: Option<String>,
    #[arg(long)]
    ide: Option<String>,
    #[arg(long)]
    agent: Option<String>,
    #[arg(long)]
    project: Option<String>,
    #[arg(long)]
    session: Option<String>,
    #[arg(long, value_enum)]
    category: Option<CategoryArg>,
    /// Group counts by a normalized event field.
    #[arg(long, value_enum)]
    group_by: Option<GroupBy>,
    /// Project root replaced with <WORKSPACE> in reports.
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// Additional literal value to replace with <REDACTED>; repeatable.
    #[arg(long = "redact-pattern")]
    redact_patterns: Vec<String>,
    /// Emit paths and credentials without the default privacy redaction.
    #[arg(long)]
    unsafe_no_redact: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SourceArg {
    Auto,
    VsCode,
    VsCodeInsiders,
    Codex,
    Kilo,
    IntelliJ,
}

impl From<SourceArg> for LogSource {
    fn from(value: SourceArg) -> Self {
        match value {
            SourceArg::Auto => Self::Auto,
            SourceArg::VsCode => Self::VsCode,
            SourceArg::VsCodeInsiders => Self::VsCodeInsiders,
            SourceArg::Codex => Self::Codex,
            SourceArg::Kilo => Self::Kilo,
            SourceArg::IntelliJ => Self::IntelliJ,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputFormat {
    Terminal,
    Json,
    Jsonl,
    Csv,
    Markdown,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CategoryArg {
    Maven,
    JarInspection,
    ClassInspection,
    PomInspection,
    ResourceInspection,
    Shell,
}

impl From<CategoryArg> for CommandCategory {
    fn from(value: CategoryArg) -> Self {
        match value {
            CategoryArg::Maven => Self::Maven,
            CategoryArg::JarInspection => Self::JarInspection,
            CategoryArg::ClassInspection => Self::ClassInspection,
            CategoryArg::PomInspection => Self::PomInspection,
            CategoryArg::ResourceInspection => Self::ResourceInspection,
            CategoryArg::Shell => Self::Shell,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum GroupBy {
    Day,
    Month,
    Ide,
    Agent,
    Project,
    Session,
    Category,
}

#[derive(Debug, Serialize)]
struct GroupCount {
    group: String,
    count: usize,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = collect_paths(&cli)?;
    if cli
        .output
        .as_ref()
        .and_then(|p| p.canonicalize().ok())
        .is_some_and(|p| paths.contains(&p))
    {
        bail!("output must not overwrite an input log");
    }
    let mut events = Vec::new();
    let mut failed_files = 0;
    let mut skipped_records = 0;
    for path in paths {
        match read_event_log(&path, cli.source.into()) {
            Ok(mut parsed) => {
                events.append(&mut parsed.events);
                skipped_records += parsed.skipped_records;
            }
            Err(_) => {
                failed_files += 1;
            }
        }
    }
    if failed_files > 0 || skipped_records > 0 {
        eprintln!(
            "Skipped {failed_files} unreadable/unsupported files and {skipped_records} malformed/incomplete records; report may be partial."
        );
        if events.is_empty() {
            bail!("no command records could be recovered from the incomplete input");
        }
    }
    if cli.keep_duplicates {
        sort_events(&mut events);
    } else {
        events = deduplicate_events(events);
    }
    let since = time_bound(cli.since.as_deref(), false)?;
    let until = time_bound(cli.until.as_deref(), true)?;
    if since
        .as_ref()
        .zip(until.as_ref())
        .is_some_and(|(s, u)| s > u)
    {
        bail!("--since must not be later than --until");
    }
    let filter = EventFilter {
        since,
        until,
        ide: cli.ide,
        agent: cli.agent,
        project: cli.project,
        session: cli.session,
        category: cli.category.map(Into::into),
    };
    let mut events = filter_events(events, &filter);
    let raw_events = events.clone();
    if !cli.unsafe_no_redact {
        redact_events(&mut events, cli.workspace.as_deref(), &cli.redact_patterns);
    }
    let report = if let Some(group_by) = cli.group_by {
        render_groups(&group_events(&raw_events, &events, group_by), cli.format)?
    } else {
        render_events(&events, cli.format)?
    };
    if let Some(output) = cli.output {
        std::fs::write(&output, report)
            .map_err(|_| anyhow::anyhow!("cannot write output report"))?;
    } else {
        print!("{report}");
    }
    Ok(())
}

fn collect_paths(cli: &Cli) -> Result<Vec<PathBuf>> {
    let mut roots = cli.inputs.clone();
    if cli.discover {
        roots.extend(discover_default_sources());
    }
    if roots.is_empty() && !cli.discover {
        bail!("provide at least one PATH or use --discover");
    }
    source_paths(&roots).map_err(|_| anyhow::anyhow!("cannot read input path or directory"))
}

fn time_bound(value: Option<&str>, upper: bool) -> Result<Option<String>> {
    value
        .map(|value| {
            let value = if upper && value.len() == 10 {
                format!("{value}T23:59:59.999Z")
            } else {
                value.to_owned()
            };
            normalize_timestamp(&value).ok_or_else(|| {
                anyhow::anyhow!("time filters require YYYY-MM-DD or an ISO 8601 timestamp")
            })
        })
        .transpose()
}

fn group_key(event: &CommandEvent, group_by: GroupBy) -> String {
    match group_by {
        GroupBy::Day => event
            .timestamp
            .as_ref()
            .and_then(|s| s.get(..10))
            .map(str::to_owned),
        GroupBy::Month => event
            .timestamp
            .as_ref()
            .and_then(|s| s.get(..7))
            .map(str::to_owned),
        GroupBy::Ide => Some(event.ide.clone()),
        GroupBy::Agent => event.agent.clone(),
        GroupBy::Project => event.project.clone(),
        GroupBy::Session => event.session.clone(),
        GroupBy::Category => Some(event.category.as_str().to_owned()),
    }
    .unwrap_or_else(|| "<unknown>".to_owned())
}

fn group_events(
    raw: &[CommandEvent],
    redacted: &[CommandEvent],
    group_by: GroupBy,
) -> Vec<GroupCount> {
    let mut counts = BTreeMap::new();
    for (original, event) in raw.iter().zip(redacted) {
        let group = counts
            .entry(group_key(original, group_by))
            .or_insert_with(|| GroupCount {
                group: group_key(event, group_by),
                count: 0,
            });
        group.count += 1;
    }
    let mut groups: Vec<GroupCount> = counts.into_values().collect();
    let mut occurrences = BTreeMap::new();
    for group in &groups {
        *occurrences.entry(group.group.clone()).or_insert(0) += 1;
    }
    let mut seen = BTreeMap::new();
    for group in &mut groups {
        if occurrences[&group.group] > 1 {
            let index = seen.entry(group.group.clone()).or_insert(0);
            *index += 1;
            group.group = format!("{} [{}]", group.group, index);
        }
    }
    groups
}

fn render_events(events: &[CommandEvent], format: OutputFormat) -> Result<String> {
    Ok(match format {
        OutputFormat::Terminal => events
            .iter()
            .map(|event| {
                format!(
                    "{} {:<17} {:<20} {}\n",
                    event.timestamp.as_deref().unwrap_or("-"),
                    event.category.as_str(),
                    event.ide,
                    event.command
                )
            })
            .collect(),
        OutputFormat::Json => format!("{}\n", serde_json::to_string_pretty(events)?),
        OutputFormat::Jsonl => {
            events
                .iter()
                .map(serde_json::to_string)
                .collect::<serde_json::Result<Vec<_>>>()?
                .join("\n")
                + "\n"
        }
        OutputFormat::Csv => {
            let mut output =
                "timestamp,ide,agent,project,session,tool,cwd,category,command,call_id,maven\n"
                    .to_owned();
            for event in events {
                output.push_str(
                    &[
                        event.timestamp.as_deref().unwrap_or(""),
                        &event.ide,
                        event.agent.as_deref().unwrap_or(""),
                        event.project.as_deref().unwrap_or(""),
                        event.session.as_deref().unwrap_or(""),
                        &event.tool,
                        event.cwd.as_deref().unwrap_or(""),
                        event.category.as_str(),
                        &event.command,
                        event.call_id.as_deref().unwrap_or(""),
                        &event
                            .maven
                            .as_ref()
                            .map(serde_json::to_string)
                            .transpose()?
                            .unwrap_or_default(),
                    ]
                    .into_iter()
                    .map(csv_field)
                    .collect::<Vec<_>>()
                    .join(","),
                );
                output.push('\n');
            }
            output
        }
        OutputFormat::Markdown => {
            let mut output =
                "| Time | IDE | Category | Command |\n| --- | --- | --- | --- |\n".to_owned();
            for event in events {
                output.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    markdown(event.timestamp.as_deref().unwrap_or("-")),
                    markdown(&event.ide),
                    event.category.as_str(),
                    markdown(&event.command)
                ));
            }
            output
        }
    })
}

fn render_groups(groups: &[GroupCount], format: OutputFormat) -> Result<String> {
    Ok(match format {
        OutputFormat::Json => format!("{}\n", serde_json::to_string_pretty(groups)?),
        OutputFormat::Jsonl => {
            groups
                .iter()
                .map(serde_json::to_string)
                .collect::<serde_json::Result<Vec<_>>>()?
                .join("\n")
                + "\n"
        }
        OutputFormat::Csv => {
            let mut output = "group,count\n".to_owned();
            for group in groups {
                output.push_str(&format!("{},{}\n", csv_field(&group.group), group.count));
            }
            output
        }
        OutputFormat::Markdown => {
            let mut output = "| Group | Count |\n| --- | ---: |\n".to_owned();
            for group in groups {
                output.push_str(&format!(
                    "| {} | {} |\n",
                    markdown(&group.group),
                    group.count
                ));
            }
            output
        }
        OutputFormat::Terminal => groups
            .iter()
            .map(|group| format!("{:<30} {}\n", group.group, group.count))
            .collect(),
    })
}

fn csv_field(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn markdown(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('|', "&#124;")
        .replace(['\n', '\r'], " ")
        .replace('`', "&#96;")
}
