//! Walk parsed-markdown events for one chapter and inject glossary term links.

mod matcher;
mod path;
mod render;

pub use path::calculate_relative_path;

use std::collections::HashSet;

use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, TagEnd};
use pulldown_cmark_to_cmark::cmark;

use crate::config::Config;
use crate::error::Result;
use crate::glossary::{Glossary, Term};

/// Adds glossary term links to a chapter's markdown content and returns the
/// result.
///
/// # Errors
///
/// Returns [`crate::error::TermlinkError::MarkdownSerialize`] if the processed
/// event stream cannot be reserialized.
pub fn add_term_links(
    content: &str,
    glossary: &Glossary,
    glossary_relative_path: &str,
    config: &Config,
) -> Result<String> {
    let terms: Vec<&Term> = glossary.iter().collect();
    let mut linked_terms: HashSet<String> = HashSet::new();

    // An empty `glossary_relative_path` is the orchestrator's signal that we
    // are *on* the glossary page itself. In that mode, definition-list titles
    // are a skip region so terms don't self-link to themselves, and the
    // rendered hrefs become bare `#anchor` (same-page) URLs.
    let on_glossary_page = glossary_relative_path.is_empty();

    // Sort-only path: on the glossary page with `sort-glossary = true` but
    // `process-glossary = false`. The preprocessor's gate let us through
    // because of the sort flag; respect the existing contract that the link
    // pass doesn't run on the glossary page unless `process-glossary` is on.
    if on_glossary_page && config.sort_glossary() && !config.process_glossary() {
        let mut events: Vec<Event> = Parser::new_ext(content, markdown_options()).collect();
        sort_definition_lists(&mut events);
        let mut output = String::new();
        cmark(events.into_iter(), &mut output)?;
        return Ok(output);
    }

    let parser = Parser::new_ext(content, markdown_options());
    let mut events: Vec<Event> = parser.collect();

    if on_glossary_page && config.sort_glossary() {
        sort_definition_lists(&mut events);
    }

    let processed_events = process_events(
        events,
        &terms,
        glossary_relative_path,
        config,
        &mut linked_terms,
        on_glossary_page,
    );

    let mut output = String::new();
    cmark(processed_events.into_iter(), &mut output)?;
    Ok(output)
}

fn markdown_options() -> Options {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_DEFINITION_LIST);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    // GFM enables blockquote alerts (`> [!NOTE]` etc.) so they round-trip
    // through parse → serialize as `Tag::BlockQuote(Some(kind))` instead of
    // being flattened to plain blockquote text. See issue #6.
    options.insert(Options::ENABLE_GFM);
    options
}

/// The kind of element currently surrounding the cursor in the event stream.
///
/// Only [`Context::Normal`] is safe to inject links into. Everything else
/// (code, links, headings, image alt-text, and on the glossary page itself
/// definition-list titles) is passed through verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Context {
    Normal,
    CodeBlock,
    Link,
    Heading,
    Image,
    /// The term title in a definition list — `API` on the line before
    /// `: A set of protocols…`. Only tracked when we are processing the
    /// glossary page itself, where linking the title would cause a term to
    /// self-link in its own definition.
    DefinitionTitle,
}

/// Returns the context that *opens* on this event, if any.
///
/// `on_glossary_page` toggles whether `Tag::DefinitionListTitle` is treated as
/// a skip region — only meaningful when the chapter being processed *is* the
/// glossary file.
const fn context_opened_by(event: &Event<'_>, on_glossary_page: bool) -> Option<Context> {
    match event {
        Event::Start(Tag::CodeBlock(_)) => Some(Context::CodeBlock),
        Event::Start(Tag::Link { .. }) => Some(Context::Link),
        Event::Start(Tag::Image { .. }) => Some(Context::Image),
        Event::Start(Tag::Heading { .. }) => Some(Context::Heading),
        Event::Start(Tag::DefinitionListTitle) if on_glossary_page => {
            Some(Context::DefinitionTitle)
        }
        _ => None,
    }
}

/// Whether this event closes one of the contexts [`context_opened_by`] opens.
const fn closes_context(event: &Event<'_>, on_glossary_page: bool) -> bool {
    matches!(
        event,
        Event::End(TagEnd::CodeBlock | TagEnd::Link | TagEnd::Image | TagEnd::Heading(_))
    ) || (on_glossary_page && matches!(event, Event::End(TagEnd::DefinitionListTitle)))
}

/// Walks the parser events, pushing/popping context as we cross protected
/// regions, and rewrites text in safe regions into a sequence of text + html
/// events that include glossary links.
fn process_events<'a>(
    events: Vec<Event<'a>>,
    terms: &[&Term],
    glossary_path: &str,
    config: &Config,
    linked_terms: &mut HashSet<String>,
    on_glossary_page: bool,
) -> Vec<Event<'a>> {
    let mut result = Vec::with_capacity(events.len());
    let mut context_stack: Vec<Context> = vec![Context::Normal];

    for event in events {
        if let Some(ctx) = context_opened_by(&event, on_glossary_page) {
            context_stack.push(ctx);
            result.push(event);
            continue;
        }
        if closes_context(&event, on_glossary_page) {
            context_stack.pop();
            result.push(event);
            continue;
        }

        if let Event::Text(text) = &event {
            let current = context_stack.last().copied().unwrap_or(Context::Normal);
            if current == Context::Normal {
                // Safe region — rewrite text into text + html events.
                result.extend(replace_terms_to_events(
                    text,
                    terms,
                    glossary_path,
                    config,
                    linked_terms,
                ));
                continue;
            }
        }

        // Inline code, protected text inside code/link/heading, or any other
        // event we don't transform: pass through unchanged.
        result.push(event);
    }

    result
}

/// Replaces term occurrences in `text` with HTML link events.
///
/// Returns a sequence of [`Event::Text`] and [`Event::Html`] events so that
/// HTML stays in its own event — wrapping mixed content in a single
/// `Event::Html` would confuse mdBook's renderer (see commit history for #4).
fn replace_terms_to_events(
    text: &str,
    terms: &[&Term],
    glossary_path: &str,
    config: &Config,
    linked_terms: &mut HashSet<String>,
) -> Vec<Event<'static>> {
    let mut matches: Vec<(usize, usize, String)> = Vec::new();

    for term in terms {
        if config.link_first_only() && linked_terms.contains(term.anchor()) {
            continue;
        }
        let Some(regex) = matcher::build_term_regex(term, config.case_sensitive()) else {
            continue;
        };
        if let Some(mat) = regex.find(text) {
            let matched_text = &text[mat.start()..mat.end()];
            let html = render::render_term_html(term, matched_text, glossary_path, config);
            matches.push((mat.start(), mat.end(), html));
            linked_terms.insert(term.anchor().to_string());
        }
    }

    matches.sort_by_key(|(start, _, _)| *start);

    let mut events = Vec::new();
    let mut last_end = 0;

    for (start, end, link) in matches {
        if start < last_end {
            // Overlapping match — skip.
            continue;
        }
        if start > last_end {
            events.push(Event::Text(CowStr::from(text[last_end..start].to_string())));
        }
        events.push(Event::Html(CowStr::from(link)));
        last_end = end;
    }

    if last_end < text.len() {
        events.push(Event::Text(CowStr::from(text[last_end..].to_string())));
    }
    if events.is_empty() {
        events.push(Event::Text(CowStr::from(text.to_string())));
    }
    events
}

/// Sorts every `<dl>` block in `events` in place so its title/definition
/// groups appear alphabetically by sort key. Outer structure — events between
/// definition lists, the number of lists, and each list's bounds — is
/// preserved.
fn sort_definition_lists(events: &mut Vec<Event<'_>>) {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut start: Option<usize> = None;
    for (i, ev) in events.iter().enumerate() {
        match ev {
            Event::Start(Tag::DefinitionList) => start = Some(i),
            Event::End(TagEnd::DefinitionList) => {
                if let Some(s) = start.take() {
                    ranges.push((s, i + 1));
                }
            }
            _ => {}
        }
    }
    // Reverse so earlier ranges stay valid as we splice replacements in.
    for (s, e) in ranges.into_iter().rev() {
        let sorted = sort_one_definition_list(&events[s..e]);
        events.splice(s..e, sorted);
    }
}

/// Given the slice `[Start(DefinitionList), ..inner.., End(DefinitionList)]`,
/// returns a new vector with title+definition groups sorted by key.
fn sort_one_definition_list<'a>(slice: &[Event<'a>]) -> Vec<Event<'a>> {
    let inner = &slice[1..slice.len() - 1];

    let mut groups: Vec<(String, Vec<Event<'a>>)> = Vec::new();
    let mut leading: Vec<Event<'a>> = Vec::new();
    let mut current: Vec<Event<'a>> = Vec::new();
    let mut current_title = String::new();
    let mut title_text = String::new();
    let mut in_title = false;
    let mut group_started = false;

    for ev in inner {
        match ev {
            Event::Start(Tag::DefinitionListTitle) => {
                if !current.is_empty() {
                    let key = sort_key_for_title(&current_title);
                    groups.push((key, std::mem::take(&mut current)));
                    current_title.clear();
                }
                in_title = true;
                title_text.clear();
                current.push(ev.clone());
                group_started = true;
            }
            Event::End(TagEnd::DefinitionListTitle) => {
                in_title = false;
                current_title = title_text.trim().to_string();
                current.push(ev.clone());
            }
            Event::Text(t) | Event::Code(t) if in_title => {
                title_text.push_str(t);
                current.push(ev.clone());
            }
            _ => {
                if group_started {
                    current.push(ev.clone());
                } else {
                    leading.push(ev.clone());
                }
            }
        }
    }
    if !current.is_empty() {
        let key = sort_key_for_title(&current_title);
        groups.push((key, current));
    }

    // Stable sort: equal keys keep source order.
    groups.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = Vec::with_capacity(slice.len());
    out.push(slice[0].clone());
    out.extend(leading);
    for (_, g) in groups {
        out.extend(g);
    }
    out.push(slice[slice.len() - 1].clone());
    out
}

/// Builds the alphabetical sort key for a definition-list title: the term's
/// short form when the title matches `SHORT (Long Description)`, otherwise
/// the full title. Lowercased so the sort is case-insensitive.
fn sort_key_for_title(title: &str) -> String {
    let term = Term::new(title.to_string());
    term.short_name()
        .map_or_else(|| title.to_lowercase(), str::to_lowercase)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DisplayMode;
    use crate::test_support::{config_with_display_mode, sample_glossary};

    fn default_config() -> Config {
        Config::default()
    }

    #[test]
    fn snapshot_full_chapter_link_mode() {
        let input = "\
The API is great. We also use REST for transport.

```rust
let api = ApiClient::new();
```

Inline `API` should not link. Visit [the API docs](docs.html).

> [!NOTE]
> The API handles auth, while XPT is a legacy format.
";
        let output = add_term_links(
            input,
            &sample_glossary(),
            "glossary.html",
            &config_with_display_mode(DisplayMode::Link),
        )
        .unwrap();
        insta::assert_snapshot!(output);
    }

    #[test]
    fn snapshot_full_chapter_tooltip_mode() {
        let input = "The API and REST are linked in tooltip mode.";
        let output = add_term_links(
            input,
            &sample_glossary(),
            "glossary.html",
            &config_with_display_mode(DisplayMode::Tooltip),
        )
        .unwrap();
        insta::assert_snapshot!(output);
    }

    #[test]
    fn snapshot_full_chapter_both_mode() {
        let input = "The API and REST are linked in both mode.";
        let output = add_term_links(
            input,
            &sample_glossary(),
            "glossary.html",
            &config_with_display_mode(DisplayMode::Both),
        )
        .unwrap();
        insta::assert_snapshot!(output);
    }

    #[test]
    fn link_first_only_links_only_the_first_occurrence() {
        let term = Term::new("XPT");
        let terms = vec![&term];
        let config = default_config();
        let mut linked = HashSet::new();

        let events = replace_terms_to_events(
            "XPT is great. XPT is used.",
            &terms,
            "g.html",
            &config,
            &mut linked,
        );
        let rendered = events_to_string(&events);

        assert!(rendered.contains(r#"<a href="g.html#xpt""#));
        assert_eq!(rendered.matches("glossary-term").count(), 1);
    }

    #[test]
    fn alias_matches_and_links_to_canonical_anchor() {
        let term = Term::new("REST").with_aliases(vec!["RESTful".to_string()]);
        let terms = vec![&term];
        let mut linked = HashSet::new();

        let events = replace_terms_to_events(
            "This is a RESTful service.",
            &terms,
            "glossary.html",
            &default_config(),
            &mut linked,
        );
        let rendered = events_to_string(&events);

        assert!(rendered.contains(r#"<a href="glossary.html#rest""#));
        assert!(rendered.contains("RESTful</a>"));
    }

    #[test]
    fn admonition_marker_is_preserved_for_every_kind() {
        let term = Term::new("API");
        let glossary = Glossary::from_terms(vec![term]);
        let config = default_config();

        for kind in ["NOTE", "TIP", "IMPORTANT", "WARNING", "CAUTION"] {
            let input = format!("> [!{kind}]\n> Use the API carefully.\n");
            let out = add_term_links(&input, &glossary, "glossary.html", &config)
                .unwrap_or_else(|e| panic!("add_term_links failed for {kind}: {e}"));

            assert!(
                out.contains(&format!("[!{kind}]")),
                "alert marker [!{kind}] lost in output:\n{out}"
            );
            assert!(
                out.contains(r#"<a href="glossary.html#api""#),
                "termlink missing inside [!{kind}] body:\n{out}"
            );
        }
    }

    #[test]
    fn term_overlapping_admonition_marker_text_does_not_corrupt_marker() {
        let glossary = Glossary::from_terms(vec![Term::new("NOTE")]);
        let out = add_term_links(
            "> [!NOTE]\n> Read this NOTE.\n",
            &glossary,
            "g.html",
            &default_config(),
        )
        .unwrap();
        assert!(out.contains("[!NOTE]"), "alert marker dropped: {out}");
    }

    fn events_to_string(events: &[Event]) -> String {
        events
            .iter()
            .map(|e| match e {
                Event::Text(s) | Event::Html(s) => s.to_string(),
                _ => String::new(),
            })
            .collect()
    }

    // -----------------------------------------------------------------
    // process-glossary feature: empty `glossary_relative_path` signals
    // "this IS the glossary page". Definition-list titles become a skip
    // region, and hrefs collapse to bare `#anchor` (same-page) URLs.
    // -----------------------------------------------------------------

    #[test]
    fn glossary_page_skips_definition_titles_but_links_body_terms() {
        // "API" appears only as a title, "REST" only in the body — verify
        // the body link is emitted and the title is left alone.
        let glossary = Glossary::from_terms(vec![
            Term::with_definition("API", Some("Application Programming Interface".to_string())),
            Term::new("REST"),
        ]);
        let content = "API\n: A protocol. See also REST.\n";
        let out = add_term_links(content, &glossary, "", &default_config()).unwrap();

        assert!(
            out.contains(r##"<a href="#rest""##),
            "REST in the definition body must be linked: {out}"
        );
        assert!(
            !out.contains(r##"href="#api""##),
            "API as a definition title must not self-link: {out}"
        );
    }

    #[test]
    fn glossary_page_emits_same_page_anchor_hrefs() {
        let glossary = Glossary::from_terms(vec![Term::new("API")]);
        let out = add_term_links(
            "Some prose mentioning the API.",
            &glossary,
            "",
            &default_config(),
        )
        .unwrap();

        assert!(
            out.contains(r##"<a href="#api""##),
            "expected bare same-page href, got: {out}"
        );
        assert!(
            !out.contains("glossary.html"),
            "no file prefix should appear in same-page anchors: {out}"
        );
    }

    #[test]
    fn glossary_page_skips_short_form_inside_title() {
        // The title row "API (Application Programming Interface)" includes
        // the short form "API" as an alternation form. Both must be skipped.
        let glossary =
            Glossary::from_terms(vec![Term::new("API (Application Programming Interface)")]);
        let content = "API (Application Programming Interface)\n: The full definition here.\n";
        let out = add_term_links(content, &glossary, "", &default_config()).unwrap();

        assert!(
            !out.contains("href=\"#"),
            "no links should be emitted inside the title: {out}"
        );
    }

    #[test]
    fn non_glossary_page_still_links_definition_titles() {
        // Outside the glossary file, definition-list titles are NOT skipped —
        // a chapter that happens to use a definition list still gets its
        // titles linked. Guards against the skip leaking off the glossary
        // page.
        let glossary = Glossary::from_terms(vec![Term::new("API")]);
        let content = "API\n: Some local definition.\n";
        let out = add_term_links(content, &glossary, "glossary.html", &default_config()).unwrap();

        assert!(
            out.contains(r#"<a href="glossary.html#api""#),
            "definition-list title on a non-glossary page must still link: {out}"
        );
    }

    // -----------------------------------------------------------------
    // sort-glossary feature: when enabled, each definition list on the
    // glossary page is sorted alphabetically. Default is off.
    // -----------------------------------------------------------------

    fn config_with_sort_glossary(sort: bool, process: bool) -> Config {
        use mdbook_preprocessor::PreprocessorContext;
        use mdbook_preprocessor::config::Config as MdBookConf;
        use std::path::PathBuf;
        use std::str::FromStr;
        let toml = format!(
            "[book]\ntitle = 't'\n[preprocessor.termlink]\nsort-glossary = {sort}\nprocess-glossary = {process}\n"
        );
        let mdb_conf = MdBookConf::from_str(&toml).unwrap();
        let ctx = PreprocessorContext::new(PathBuf::new(), mdb_conf, String::new());
        Config::from_context(&ctx).unwrap()
    }

    #[test]
    fn sort_glossary_orders_definition_list_alphabetically_on_glossary_page() {
        let glossary =
            Glossary::from_terms(vec![Term::new("Zeta"), Term::new("Alpha"), Term::new("Mu")]);
        let content = "Zeta\n: last letter-ish.\n\nAlpha\n: first.\n\nMu\n: middle.\n";
        let out = add_term_links(
            content,
            &glossary,
            "",
            &config_with_sort_glossary(true, false),
        )
        .unwrap();

        let a = out.find("Alpha").expect("Alpha missing from output");
        let m = out.find("Mu").expect("Mu missing from output");
        let z = out.find("Zeta").expect("Zeta missing from output");
        assert!(a < m && m < z, "expected Alpha < Mu < Zeta in:\n{out}");
    }

    #[test]
    fn sort_glossary_disabled_preserves_source_order() {
        let glossary = Glossary::from_terms(vec![Term::new("Zeta"), Term::new("Alpha")]);
        let content = "Zeta\n: last.\n\nAlpha\n: first.\n";
        // Default config has sort-glossary=false. Need process_glossary=true
        // so the chapter is processed on the glossary page and we can observe
        // the (unsorted) result.
        let out = add_term_links(
            content,
            &glossary,
            "",
            &config_with_sort_glossary(false, true),
        )
        .unwrap();
        assert!(
            out.find("Zeta").unwrap() < out.find("Alpha").unwrap(),
            "source order should be preserved when sort is off:\n{out}"
        );
    }

    #[test]
    fn sort_glossary_uses_short_form_as_key() {
        // "API (Application Programming Interface)" should sort under 'A'
        // (short form 'API'), placing it before "Backend".
        let glossary = Glossary::from_terms(vec![
            Term::new("Backend"),
            Term::new("API (Application Programming Interface)"),
        ]);
        let content =
            "Backend\n: server side.\n\nAPI (Application Programming Interface)\n: a contract.\n";
        let out = add_term_links(
            content,
            &glossary,
            "",
            &config_with_sort_glossary(true, false),
        )
        .unwrap();
        assert!(
            out.find("API").unwrap() < out.find("Backend").unwrap(),
            "API should sort before Backend on its short form:\n{out}"
        );
    }

    #[test]
    fn sort_glossary_off_glossary_page_is_a_noop() {
        // Non-empty glossary_relative_path => not the glossary page => sort
        // must not run, even with sort-glossary=true.
        let glossary = Glossary::from_terms(vec![Term::new("Zeta"), Term::new("Alpha")]);
        let content = "Zeta\n: last.\n\nAlpha\n: first.\n";
        let out = add_term_links(
            content,
            &glossary,
            "glossary.html",
            &config_with_sort_glossary(true, false),
        )
        .unwrap();
        assert!(
            out.find("Zeta").unwrap() < out.find("Alpha").unwrap(),
            "non-glossary page should not be sorted:\n{out}"
        );
    }

    #[test]
    fn sort_only_mode_skips_term_linking_on_glossary_page() {
        // sort-glossary=true && process-glossary=false: sort runs but the
        // term-linking pass must not, preserving the existing contract that
        // the glossary page is otherwise untouched.
        let glossary = Glossary::from_terms(vec![Term::new("REST"), Term::new("API")]);
        let content = "REST\n: A protocol. See also API.\n\nAPI\n: A contract.\n";
        let out = add_term_links(
            content,
            &glossary,
            "",
            &config_with_sort_glossary(true, false),
        )
        .unwrap();
        assert!(
            !out.contains("<a "),
            "no anchor tags expected in sort-only mode:\n{out}"
        );
        // And the sort still happened.
        assert!(
            out.find("API").unwrap() < out.find("REST").unwrap(),
            "sort should still run in sort-only mode:\n{out}"
        );
    }
}
