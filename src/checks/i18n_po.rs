//! Slint & Rust dual-end `.po` translation catalog synchronizer, validator, and generator.
//!
//! Reconciles gettext `.po` catalogs across source code (`@tr(...)` / `tr(...)` / `tr!(...)`),
//! validates placeholder consistency and strict translation completeness, and generates a
//! sorted static Rust array for zero-cost runtime lookup.

use crate::checks::i18n_source::{SourceTranslationKey, collect_source_keys, placeholders_match};
use crate::report::{Diagnostic, Severity, normalize_rel_path};
use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Parsed `.po` entry supporting context (`msgctxt`), plurals, and `#, fuzzy` flags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PoEntry {
    pub msgctxt: Option<String>,
    pub msgid: String,
    pub msgid_plural: Option<String>,
    pub msgstr: String,
    pub msgstr_plural: Vec<String>,
    pub fuzzy: bool,
    pub line: usize,
}

impl PoEntry {
    pub fn lookup_key(&self) -> (Option<&str>, &str) {
        (self.msgctxt.as_deref(), self.msgid.as_str())
    }
}

#[derive(Clone, Copy)]
enum Field {
    None,
    Msgctxt,
    Msgid,
    MsgidPlural,
    Msgstr,
    MsgstrPlural(usize),
}

#[derive(Default)]
struct EntryBuilder {
    msgctxt: Option<String>,
    msgid: Option<String>,
    msgid_plural: Option<String>,
    msgstr: Option<String>,
    msgstr_plural: Vec<String>,
    fuzzy: bool,
    line: usize,
}

impl EntryBuilder {
    fn flush(&mut self, out: &mut Vec<PoEntry>) {
        if let Some(id) = self.msgid.take()
            && !id.is_empty()
        {
            out.push(PoEntry {
                msgctxt: self.msgctxt.take(),
                msgid: id,
                msgid_plural: self.msgid_plural.take(),
                msgstr: self.msgstr.take().unwrap_or_default(),
                msgstr_plural: std::mem::take(&mut self.msgstr_plural),
                fuzzy: self.fuzzy,
                line: self.line,
            });
        }
        self.msgctxt = None;
        self.msgid_plural = None;
        self.msgstr = None;
        self.msgstr_plural.clear();
        self.fuzzy = false;
        self.line = 0;
    }

    fn append_continuation(&mut self, field: Field, text: &str) {
        match field {
            Field::Msgctxt => append_opt(&mut self.msgctxt, text),
            Field::Msgid => append_opt(&mut self.msgid, text),
            Field::MsgidPlural => append_opt(&mut self.msgid_plural, text),
            Field::Msgstr => append_opt(&mut self.msgstr, text),
            Field::MsgstrPlural(idx) => {
                if let Some(slot) = self.msgstr_plural.get_mut(idx) {
                    slot.push_str(text);
                }
            }
            Field::None => {}
        }
    }
}

fn append_opt(slot: &mut Option<String>, text: &str) {
    if let Some(existing) = slot.as_mut() {
        existing.push_str(text);
    }
}

/// Parse a `.po` catalog file into rich `PoEntry` records.
pub fn parse_po_entries(path: &Path) -> Result<Vec<PoEntry>> {
    let content =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let mut entries = Vec::new();
    let mut builder = EntryBuilder::default();
    let mut field = Field::None;

    for (idx, raw_line) in content.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw_line.trim_end();
        if line.is_empty() {
            builder.flush(&mut entries);
            field = Field::None;
            continue;
        }
        if line.starts_with('#') {
            if builder.msgid.is_some() {
                builder.flush(&mut entries);
            }
            if let Some(flags) = line.strip_prefix("#,")
                && flags.split(',').any(|f| f.trim() == "fuzzy")
            {
                builder.fuzzy = true;
            }
            field = Field::None;
            continue;
        }
        field = parse_po_directive(line, line_no, path, &mut builder, &mut entries, field)?;
    }
    builder.flush(&mut entries);
    Ok(entries)
}

fn parse_po_directive(
    line: &str,
    line_no: usize,
    path: &Path,
    builder: &mut EntryBuilder,
    entries: &mut Vec<PoEntry>,
    current_field: Field,
) -> Result<Field> {
    if let Some(rest) = line.strip_prefix("msgctxt ") {
        if builder.msgid.is_some() {
            builder.flush(entries);
        }
        builder.line = line_no;
        builder.msgctxt = Some(po_string(rest, path)?);
        return Ok(Field::Msgctxt);
    }
    if let Some(rest) = line.strip_prefix("msgid ") {
        if builder.msgid.is_some() {
            builder.flush(entries);
        }
        if builder.line == 0 {
            builder.line = line_no;
        }
        builder.msgid = Some(po_string(rest, path)?);
        return Ok(Field::Msgid);
    }
    if let Some(rest) = line.strip_prefix("msgid_plural ") {
        builder.msgid_plural = Some(po_string(rest, path)?);
        return Ok(Field::MsgidPlural);
    }
    if let Some(rest) = line.strip_prefix("msgstr ") {
        builder.msgstr = Some(po_string(rest, path)?);
        return Ok(Field::Msgstr);
    }
    if let Some((idx, rest)) = parse_plural_msgstr_prefix(line) {
        if builder.msgstr_plural.len() <= idx {
            builder.msgstr_plural.resize_with(idx + 1, String::new);
        }
        builder.msgstr_plural[idx] = po_string(rest, path)?;
        return Ok(Field::MsgstrPlural(idx));
    }
    if line.starts_with('"') {
        let text = po_string(line, path)?;
        builder.append_continuation(current_field, &text);
        return Ok(current_field);
    }
    Ok(current_field)
}

fn parse_plural_msgstr_prefix(line: &str) -> Option<(usize, &str)> {
    let rest = line.strip_prefix("msgstr[")?;
    let close = rest.find(']')?;
    let idx: usize = rest[..close].parse().ok()?;
    let after = rest[close + 1..].trim_start();
    Some((idx, after))
}

/// Parse a `.po` catalog file into a simple `msgid -> msgstr` map.
pub fn parse_po(path: &Path) -> Result<BTreeMap<String, String>> {
    let entries = parse_po_entries(path)?;
    let mut map = BTreeMap::new();
    for entry in entries {
        if entry.msgctxt.is_none() {
            let value = if entry.msgstr.is_empty() && !entry.msgstr_plural.is_empty() {
                entry.msgstr_plural[0].clone()
            } else {
                entry.msgstr
            };
            map.insert(entry.msgid, value);
        }
    }
    Ok(map)
}

fn po_string(raw: &str, path: &Path) -> Result<String> {
    let raw = raw.trim();
    let inner = raw
        .strip_prefix('"')
        .and_then(|raw| raw.strip_suffix('"'))
        .with_context(|| format!("{}: malformed .po string: {raw}", path.display()))?;

    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => bail!(
                "{}: trailing backslash in .po string: {raw}",
                path.display()
            ),
        }
    }
    Ok(out)
}

fn rust_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

pub fn locale_po_path(lang_dir: &Path, locale: &str, domain: &str) -> PathBuf {
    lang_dir
        .join(locale)
        .join("LC_MESSAGES")
        .join(format!("{domain}.po"))
}

pub fn generate_catalog(lang_dir: &Path, domain: &str, locales: &[String]) -> Result<String> {
    if locales.is_empty() {
        bail!("At least one locale must be configured");
    }

    let catalogs: Vec<BTreeMap<String, String>> = locales
        .iter()
        .map(|locale| parse_po(&locale_po_path(lang_dir, locale, domain)))
        .collect::<Result<_>>()?;

    let mut msgids: BTreeMap<&str, ()> = BTreeMap::new();
    for catalog in &catalogs {
        for msgid in catalog.keys() {
            msgids.insert(msgid.as_str(), ());
        }
    }

    let num_locales = locales.len();
    let mut out = String::new();
    out.push_str(
        r#"//! Auto-generated by cargo-warden i18n — do not edit by hand.
//!
//! Mirrors gettext `.po` catalogs for high-performance binary search lookups.

pub(crate) struct Entry {
    pub(crate) msgid: &'static str,
    pub(crate) translations: [&'static str; "#,
    );
    out.push_str(&format!("{num_locales}],\n}}\n\n"));
    out.push_str("#[rustfmt::skip]\npub(crate) static CATALOG: &[Entry] = &[\n");

    for msgid in msgids.keys() {
        let mut trans = vec![*msgid; num_locales];
        for (idx, catalog) in catalogs.iter().enumerate() {
            if let Some(val) = catalog.get(*msgid).filter(|v| !v.is_empty()) {
                trans[idx] = val.as_str();
            }
        }
        out.push_str("    Entry { msgid: ");
        out.push_str(&rust_string(msgid));
        out.push_str(", translations: [");
        for (i, t) in trans.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&rust_string(t));
        }
        out.push_str("] },\n");
    }
    out.push_str("];\n");
    Ok(out)
}

fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n")
}

/// Options for running i18n catalog sync, quality, and source-coverage checks.
#[derive(Debug, Clone)]
pub struct I18nOptions<'a> {
    pub base_dir: &'a Path,
    pub lang_dir: &'a Path,
    pub domain: &'a str,
    pub locales: &'a [String],
    pub out_file: &'a Path,
    pub source_dirs: &'a [PathBuf],
    pub tr_macros: &'a [String],
    pub strict: bool,
    pub check_placeholders: bool,
    pub check_unused: bool,
    pub respect_gitignore: bool,
    pub changeset: Option<&'a BTreeSet<PathBuf>>,
    pub emit_text: bool,
}

fn should_run_i18n_changeset(opts: &I18nOptions<'_>, changeset: &BTreeSet<PathBuf>) -> bool {
    let lang_rel = normalize_rel_path(opts.base_dir, opts.lang_dir)
        .to_string_lossy()
        .replace('\\', "/");
    let out_rel = normalize_rel_path(opts.base_dir, opts.out_file)
        .to_string_lossy()
        .replace('\\', "/");
    let src_rels: Vec<String> = opts
        .source_dirs
        .iter()
        .map(|d| {
            normalize_rel_path(opts.base_dir, &opts.base_dir.join(d))
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();

    changeset.iter().any(|p| {
        let s = p.to_string_lossy().replace('\\', "/");
        s == out_rel
            || s == "warden.toml"
            || s == ".warden.toml"
            || s.starts_with(&format!("{lang_rel}/"))
            || src_rels
                .iter()
                .any(|sr| sr == "." || s == *sr || s.starts_with(&format!("{sr}/")))
    })
}

fn make_diag(
    rule_id: &str,
    message: String,
    file: Option<PathBuf>,
    line: Option<usize>,
    fingerprint: String,
) -> Diagnostic {
    Diagnostic {
        rule_id: rule_id.to_string(),
        check: "i18n".to_string(),
        severity: Severity::Error,
        message,
        file,
        line,
        column: None,
        fingerprint,
        metric_value: None,
        suppressed: false,
        suppression_reason: None,
    }
}

fn check_catalog_sync(opts: &I18nOptions<'_>, diagnostics: &mut Vec<Diagnostic>) -> Result<()> {
    let generated = generate_catalog(opts.lang_dir, opts.domain, opts.locales)?;
    let rel_out = normalize_rel_path(opts.base_dir, opts.out_file);
    let rel_str = rel_out.to_string_lossy().replace('\\', "/");
    let on_disk = match fs::read_to_string(opts.out_file) {
        Ok(content) => content,
        Err(_) => {
            diagnostics.push(make_diag(
                "warden/i18n",
                format!(
                    "Translation catalog {} does not exist. Run `cargo warden i18n generate` to create it.",
                    rel_out.display()
                ),
                Some(rel_out),
                None,
                format!("i18n:sync:{rel_str}"),
            ));
            return Ok(());
        }
    };

    if normalize(&generated) != normalize(&on_disk) {
        diagnostics.push(make_diag(
            "warden/i18n",
            format!(
                "Translation catalog {} is out of sync with .po catalogs. Run `cargo warden i18n generate` to refresh.",
                rel_out.display()
            ),
            Some(rel_out),
            None,
            format!("i18n:sync:{rel_str}"),
        ));
    }
    Ok(())
}

fn check_entry_quality(
    locale: &str,
    po_rel: &Path,
    entry: &PoEntry,
    opts: &I18nOptions<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let po_str = po_rel.to_string_lossy().replace('\\', "/");
    if opts.strict && entry.fuzzy {
        diagnostics.push(make_diag(
            "warden/i18n-fuzzy",
            format!(
                "[{locale}] Fuzzy translation for {:?} at {}:{}",
                entry.msgid,
                po_rel.display(),
                entry.line
            ),
            Some(po_rel.to_path_buf()),
            Some(entry.line),
            format!("i18n:fuzzy:{po_str}:{}", entry.msgid),
        ));
    }
    if opts.strict && is_entry_empty(entry) {
        diagnostics.push(make_diag(
            "warden/i18n-empty",
            format!(
                "[{locale}] Empty translation for {:?} at {}:{}",
                entry.msgid,
                po_rel.display(),
                entry.line
            ),
            Some(po_rel.to_path_buf()),
            Some(entry.line),
            format!("i18n:empty:{po_str}:{}", entry.msgid),
        ));
    }
    if opts.check_placeholders {
        check_entry_placeholders(locale, po_rel, &po_str, entry, diagnostics);
    }
}

fn is_entry_empty(entry: &PoEntry) -> bool {
    if entry.msgid_plural.is_some() {
        entry.msgstr_plural.is_empty() || entry.msgstr_plural.iter().any(|s| s.trim().is_empty())
    } else {
        entry.msgstr.trim().is_empty()
    }
}

fn check_entry_placeholders(
    locale: &str,
    po_rel: &Path,
    po_str: &str,
    entry: &PoEntry,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if !entry.msgstr.is_empty() && !placeholders_match(&entry.msgid, &entry.msgstr) {
        diagnostics.push(make_diag(
            "warden/i18n-placeholder",
            format!(
                "[{locale}] Placeholder mismatch for {:?} at {}:{}",
                entry.msgid,
                po_rel.display(),
                entry.line
            ),
            Some(po_rel.to_path_buf()),
            Some(entry.line),
            format!("i18n:placeholder:{po_str}:{}", entry.msgid),
        ));
    }
    if let Some(plural_src) = &entry.msgid_plural {
        for (idx, pl_tr) in entry.msgstr_plural.iter().enumerate() {
            if !pl_tr.is_empty()
                && !placeholders_match(plural_src, pl_tr)
                && !placeholders_match(&entry.msgid, pl_tr)
            {
                diagnostics.push(make_diag(
                    "warden/i18n-placeholder",
                    format!(
                        "[{locale}] Plural placeholder mismatch in msgstr[{idx}] for {:?} at {}:{}",
                        entry.msgid,
                        po_rel.display(),
                        entry.line
                    ),
                    Some(po_rel.to_path_buf()),
                    Some(entry.line),
                    format!("i18n:placeholder:{po_str}:{}:{}", entry.msgid, idx),
                ));
            }
        }
    }
}

fn check_source_coverage(
    opts: &I18nOptions<'_>,
    parsed: &[(&str, PathBuf, Vec<PoEntry>)],
    precomputed_keys: Option<Vec<SourceTranslationKey>>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<()> {
    if opts.source_dirs.is_empty() {
        return Ok(());
    }
    let source_keys = match precomputed_keys {
        Some(keys) => keys,
        None => {
            let resolved_dirs: Vec<PathBuf> = opts
                .source_dirs
                .iter()
                .map(|d| opts.base_dir.join(d))
                .filter(|d| d.exists())
                .collect();
            collect_source_keys(
                &resolved_dirs,
                opts.tr_macros,
                opts.out_file,
                opts.respect_gitignore,
                Some(opts.base_dir),
            )?
        }
    };

    for (locale, po_rel, entries) in parsed {
        let key_set: BTreeSet<(Option<&str>, &str)> =
            entries.iter().map(PoEntry::lookup_key).collect();
        for sk in &source_keys {
            if !key_set.contains(&(sk.ctx.as_deref(), sk.msgid.as_str())) {
                let src_rel = normalize_rel_path(opts.base_dir, &sk.file);
                let src_str = src_rel.to_string_lossy().replace('\\', "/");
                diagnostics.push(make_diag(
                    "warden/i18n-missing-key",
                    format!(
                        "[{locale}] Source key {:?} missing in {} (used at {}:{})",
                        sk.msgid,
                        po_rel.display(),
                        src_rel.display(),
                        sk.line
                    ),
                    Some(src_rel),
                    Some(sk.line),
                    format!("i18n:missing:{locale}:{src_str}:{}", sk.msgid),
                ));
            }
        }
    }

    if opts.check_unused {
        let src_set: BTreeSet<(Option<&str>, &str)> = source_keys
            .iter()
            .map(|k| (k.ctx.as_deref(), k.msgid.as_str()))
            .collect();
        for (locale, po_rel, entries) in parsed {
            let po_str = po_rel.to_string_lossy().replace('\\', "/");
            for entry in entries {
                if !src_set.contains(&entry.lookup_key()) {
                    diagnostics.push(make_diag(
                        "warden/i18n-unused-key",
                        format!(
                            "[{locale}] Unused .po key {:?} at {}:{}",
                            entry.msgid,
                            po_rel.display(),
                            entry.line
                        ),
                        Some(po_rel.clone()),
                        Some(entry.line),
                        format!("i18n:unused:{po_str}:{}", entry.msgid),
                    ));
                }
            }
        }
    }

    Ok(())
}

pub(crate) fn should_run_i18n(opts: &I18nOptions<'_>) -> bool {
    opts.changeset
        .is_none_or(|changed| should_run_i18n_changeset(opts, changed))
}

/// Collect structured i18n synchronization, quality, and source-coverage diagnostics.
pub fn collect_i18n(opts: &I18nOptions<'_>) -> Result<Vec<Diagnostic>> {
    collect_i18n_with_source_keys(opts, None)
}

pub(crate) fn collect_i18n_with_source_keys(
    opts: &I18nOptions<'_>,
    precomputed_keys: Option<Vec<SourceTranslationKey>>,
) -> Result<Vec<Diagnostic>> {
    if !should_run_i18n(opts) {
        tracing::info!("skipping i18n check (no .po, catalog, or i18n source changes)");
        return Ok(Vec::new());
    }

    tracing::debug!(
        lang_dir = %opts.lang_dir.display(),
        domain = opts.domain,
        locales = ?opts.locales,
        out_file = %opts.out_file.display(),
        "checking i18n catalog synchronization and quality"
    );
    if opts.emit_text {
        println!(
            "[i18n] checking catalog sync: {} against {}",
            opts.lang_dir.display(),
            opts.out_file.display()
        );
    }

    let mut diagnostics = Vec::new();
    check_catalog_sync(opts, &mut diagnostics)?;

    let mut parsed: Vec<(&str, PathBuf, Vec<PoEntry>)> = Vec::with_capacity(opts.locales.len());
    for locale in opts.locales {
        let po_path = locale_po_path(opts.lang_dir, locale, opts.domain);
        let po_rel = normalize_rel_path(opts.base_dir, &po_path);
        let entries = parse_po_entries(&po_path)?;
        for entry in &entries {
            check_entry_quality(locale, &po_rel, entry, opts, &mut diagnostics);
        }
        parsed.push((locale.as_str(), po_rel, entries));
    }

    if opts.strict && parsed.len() > 1 {
        check_cross_locale_parity(&parsed, &mut diagnostics);
    }

    check_source_coverage(opts, &parsed, precomputed_keys, &mut diagnostics)?;

    if diagnostics.is_empty() && opts.emit_text {
        println!(
            "[i18n] OK: {} is in sync with .po catalogs",
            opts.out_file.display()
        );
    }
    Ok(diagnostics)
}

fn check_cross_locale_parity(
    parsed: &[(&str, PathBuf, Vec<PoEntry>)],
    diagnostics: &mut Vec<Diagnostic>,
) {
    let all_keys: BTreeSet<(Option<&str>, &str)> = parsed
        .iter()
        .flat_map(|(_, _, entries)| entries.iter().map(PoEntry::lookup_key))
        .collect();
    for (locale, po_rel, entries) in parsed {
        let po_str = po_rel.to_string_lossy().replace('\\', "/");
        let local_set: BTreeSet<(Option<&str>, &str)> =
            entries.iter().map(PoEntry::lookup_key).collect();
        for (ctx, msgid) in &all_keys {
            if !local_set.contains(&(*ctx, *msgid)) {
                diagnostics.push(make_diag(
                    "warden/i18n-missing-key",
                    format!(
                        "[{locale}] Missing entry {:?} present in other locale(s) ({})",
                        msgid,
                        po_rel.display()
                    ),
                    Some(po_rel.clone()),
                    None,
                    format!("i18n:parity:{po_str}:{msgid}"),
                ));
            }
        }
    }
}

pub fn check_i18n(
    lang_dir: &Path,
    domain: &str,
    locales: &[String],
    out_file: &Path,
) -> Result<()> {
    let empty_dirs: Vec<PathBuf> = Vec::new();
    let empty_macros: Vec<String> = Vec::new();
    let diagnostics = collect_i18n(&I18nOptions {
        base_dir: Path::new("."),
        lang_dir,
        domain,
        locales,
        out_file,
        source_dirs: &empty_dirs,
        tr_macros: &empty_macros,
        strict: false,
        check_placeholders: true,
        check_unused: false,
        respect_gitignore: true,
        changeset: None,
        emit_text: true,
    })?;
    if let Some(first) = diagnostics.iter().find(|d| !d.suppressed) {
        bail!("{}", first.message);
    }
    Ok(())
}

pub fn sync_i18n(lang_dir: &Path, domain: &str, locales: &[String], out_file: &Path) -> Result<()> {
    let content = generate_catalog(lang_dir, domain, locales)?;
    if let Some(parent) = out_file.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(out_file, content)?;
    println!("[i18n] wrote Rust catalog -> {}", out_file.display());
    Ok(())
}
