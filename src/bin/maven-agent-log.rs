use std::{collections::BTreeMap, path::PathBuf};

use anyhow::{Result, bail};
use clap::{Parser, ValueEnum};
use maven_mcp::log_analysis::{
    CommandCategory, CommandEvent, DEFAULT_MAX_INPUT_BYTES, DEFAULT_MAX_RECORD_BYTES,
    DEFAULT_MAX_STATE_BYTES, ENTRY_OVERHEAD_BYTES, EventFilter, LimitExceeded, LogSource,
    ResourceLimits, StateBudget, deduplicate_events_within, discover_default_sources,
    filter_events_within, normalize_timestamp, read_event_log_limited, redact_events_within,
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
    /// Skip a whole-document JSON input (VS Code) larger than this many bytes.
    #[arg(long, value_name = "BYTES", default_value_t = DEFAULT_MAX_INPUT_BYTES, value_parser = clap::value_parser!(u64).range(1..))]
    max_input_bytes: u64,
    /// Skip a JSONL/IntelliJ line or SQLite row larger than this many bytes.
    #[arg(long, value_name = "BYTES", default_value_t = DEFAULT_MAX_RECORD_BYTES as u64, value_parser = clap::value_parser!(u64).range(1..))]
    max_record_bytes: u64,
    /// Fail when the estimated retained state (events, session/message metadata,
    /// deduplication and grouping keys) would exceed this many bytes.
    #[arg(long, value_name = "BYTES", default_value_t = DEFAULT_MAX_STATE_BYTES as u64, value_parser = clap::value_parser!(u64).range(1..))]
    max_state_bytes: u64,
}

impl Cli {
    fn limits(&self) -> ResourceLimits {
        ResourceLimits {
            max_input_bytes: self.max_input_bytes,
            max_record_bytes: saturating_usize(self.max_record_bytes),
            max_state_bytes: saturating_usize(self.max_state_bytes),
        }
    }
}

fn saturating_usize(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

fn state_budget_error() -> anyhow::Error {
    anyhow::anyhow!(
        "retained state exceeds the --max-state-bytes budget; no report was produced. \
         Narrow the inputs or filters, or raise the budget"
    )
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
    let limits = cli.limits();
    let mut budget = StateBudget::new(limits.max_state_bytes);
    let mut events = Vec::new();
    let mut failed_files = 0;
    let mut skipped_records = 0;
    let mut oversized_files = 0;
    let mut oversized_records = 0;
    for path in paths {
        match read_event_log_limited(&path, cli.source.into(), &limits, &mut budget) {
            Ok(mut parsed) => {
                events.append(&mut parsed.events);
                skipped_records += parsed.skipped_records;
                oversized_records += parsed.oversized_records;
            }
            Err(error) => match error.downcast_ref::<LimitExceeded>() {
                Some(LimitExceeded::State) => return Err(state_budget_error()),
                Some(LimitExceeded::Input) => oversized_files += 1,
                None => failed_files += 1,
            },
        }
    }
    let limited = oversized_files + oversized_records;
    if failed_files > 0 || skipped_records > 0 {
        eprintln!(
            "Skipped {failed_files} unreadable/unsupported files and {skipped_records} malformed/incomplete records; report may be partial."
        );
    }
    if limited > 0 {
        eprintln!(
            "Skipped {oversized_files} inputs over --max-input-bytes and {oversized_records} records over --max-record-bytes; report is partial."
        );
    }
    if (failed_files > 0 || skipped_records > 0 || limited > 0) && events.is_empty() {
        bail!("no command records could be recovered from the incomplete input");
    }
    if cli.keep_duplicates {
        sort_events(&mut events);
    } else {
        events =
            deduplicate_events_within(events, &mut budget).map_err(|_| state_budget_error())?;
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
    let mut events = filter_events_within(events, &filter, &mut budget);
    // Grouping needs the original identities after redaction; keep only those keys.
    let raw_groups = cli
        .group_by
        .map(|group_by| RawGroups::collect(&events, group_by, &mut budget))
        .transpose()
        .map_err(|_| state_budget_error())?;
    if !cli.unsafe_no_redact {
        redact_events_within(
            &mut events,
            cli.workspace.as_deref(),
            &cli.redact_patterns,
            &mut budget,
        )
        .map_err(|_| state_budget_error())?;
    }
    let report = match (cli.group_by, raw_groups) {
        (Some(group_by), Some(raw_groups)) => {
            let groups = raw_groups
                .count(&events, group_by, &mut budget)
                .map_err(|_| state_budget_error())?;
            render_groups(&groups, cli.format)?
        }
        _ => render_events(&events, cli.format)?,
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

/// Original group identities and each event's index into them, kept in event
/// order so redaction can change labels but never merge or split groups.
struct RawGroups {
    identities: BTreeMap<String, usize>,
    event_identity: Vec<usize>,
    /// Budget charged for `identities` and `event_identity`.
    charged: usize,
}

/// Charge `bytes` and remember it in `charged` so it can be released later.
fn charge_tracked(
    budget: &mut StateBudget,
    charged: &mut usize,
    bytes: usize,
) -> Result<(), LimitExceeded> {
    budget.charge(bytes)?;
    *charged += bytes;
    Ok(())
}

impl RawGroups {
    fn collect(
        events: &[CommandEvent],
        group_by: GroupBy,
        budget: &mut StateBudget,
    ) -> Result<Self, LimitExceeded> {
        let mut charged = 0;
        charge_tracked(
            budget,
            &mut charged,
            events.len().saturating_mul(std::mem::size_of::<usize>()),
        )?;
        let mut identities: BTreeMap<String, usize> = BTreeMap::new();
        let mut event_identity = Vec::with_capacity(events.len());
        for event in events {
            let key = group_key(event, group_by);
            let next = identities.len();
            if let Some(&index) = identities.get(&key) {
                event_identity.push(index);
                continue;
            }
            charge_tracked(
                budget,
                &mut charged,
                key.len() + std::mem::size_of::<usize>() + ENTRY_OVERHEAD_BYTES,
            )?;
            identities.insert(key, next);
            event_identity.push(next);
        }
        Ok(Self {
            identities,
            event_identity,
            charged,
        })
    }

    /// Count per original identity; each label is the redacted key of its first event.
    /// Every workspace structure is charged before it is allocated; afterwards the
    /// budget keeps only the returned groups.
    fn count(
        self,
        redacted: &[CommandEvent],
        group_by: GroupBy,
        budget: &mut StateBudget,
    ) -> Result<Vec<GroupCount>, LimitExceeded> {
        let identity_count = self.identities.len();
        let mut workspace = 0;
        charge_tracked(
            budget,
            &mut workspace,
            identity_count.saturating_mul(
                std::mem::size_of::<Option<String>>() + std::mem::size_of::<usize>(),
            ),
        )?;
        let mut labels: Vec<Option<String>> = vec![None; identity_count];
        let mut counts = vec![0usize; identity_count];
        let mut kept = 0;
        for (&identity, event) in self.event_identity.iter().zip(redacted) {
            counts[identity] += 1;
            if labels[identity].is_none() {
                let label = group_key(event, group_by);
                charge_tracked(budget, &mut kept, label.len() + ENTRY_OVERHEAD_BYTES)?;
                labels[identity] = Some(label);
            }
        }
        charge_tracked(
            budget,
            &mut kept,
            identity_count.saturating_mul(std::mem::size_of::<GroupCount>()),
        )?;
        // BTreeMap order is the original-identity order of the previous implementation.
        let mut groups: Vec<GroupCount> = self
            .identities
            .into_values()
            .map(|identity| GroupCount {
                group: labels[identity].take().unwrap_or_default(),
                count: counts[identity],
            })
            .collect();
        drop((labels, counts, self.event_identity));
        budget.release(workspace + self.charged);
        disambiguate_labels(&mut groups, budget)?;
        Ok(groups)
    }
}

/// Give identical redacted labels numeric suffixes in group order. The suffix
/// tables borrow the labels and are charged while they exist.
fn disambiguate_labels(
    groups: &mut [GroupCount],
    budget: &mut StateBudget,
) -> Result<(), LimitExceeded> {
    let mut tables = 0;
    let suffixes = collect_suffixes(groups, budget, &mut tables);
    budget.release(tables);
    for (group, suffix) in groups.iter_mut().zip(suffixes?) {
        if let Some(index) = suffix {
            let label = format!("{} [{}]", group.group, index);
            budget.charge(label.len() - group.group.len())?;
            group.group = label;
        }
    }
    Ok(())
}

const SUFFIX_TABLE_ENTRY_BYTES: usize =
    std::mem::size_of::<&str>() + std::mem::size_of::<usize>() + ENTRY_OVERHEAD_BYTES;

/// Number each repeated label in group order; charges the tables to `tables`.
fn collect_suffixes(
    groups: &[GroupCount],
    budget: &mut StateBudget,
    tables: &mut usize,
) -> Result<Vec<Option<usize>>, LimitExceeded> {
    charge_tracked(
        budget,
        tables,
        groups
            .len()
            .saturating_mul(SUFFIX_TABLE_ENTRY_BYTES + std::mem::size_of::<Option<usize>>()),
    )?;
    let mut occurrences: BTreeMap<&str, usize> = BTreeMap::new();
    for group in groups {
        *occurrences.entry(group.group.as_str()).or_insert(0) += 1;
    }
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    let mut suffixes = Vec::with_capacity(groups.len());
    for group in groups {
        let label = group.group.as_str();
        if occurrences[label] < 2 {
            suffixes.push(None);
            continue;
        }
        if !seen.contains_key(label) {
            charge_tracked(budget, tables, SUFFIX_TABLE_ENTRY_BYTES)?;
        }
        let index = seen.entry(label).or_insert(0);
        *index += 1;
        suffixes.push(Some(*index));
    }
    Ok(suffixes)
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
