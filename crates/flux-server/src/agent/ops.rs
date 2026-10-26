//! The operations ([`define`]) and what each does.

use std::collections::BTreeMap;

use base64::Engine as _;
use flux_catalog::agent_read::{self, FindQuery};
use flux_catalog::changes;
use flux_catalog::{fields, read};
use flux_core::agent::*;
use flux_core::api::{ArtSource, Id, Role};

use super::{announce, cat_err, err, op, Ctx, Op, ADMIN, ANYONE, EDITORS, SIGNED_IN};
use crate::agent::{fetch, files, lens, policy};

/// Most items one `items.edit` changes.
pub const MAX_EDITS: usize = 100;
/// Longest changeset summary.
pub const MAX_SUMMARY: usize = 500;

pub fn define() -> Vec<Op> {
    use Effect::{Act, Read, Write};
    vec![
        op(
            "status",
            "Server status",
            "Start here: what this flux server is, what it matches titles against by itself (often nothing: \
             then titles come from file names and you are the metadata source), its libraries, who you are, \
             what you may do, and the rules (what's withheld from agents, how to cite).",
            Read,
            ANYONE,
            true,
            false,
            status,
        ),
        op(
            "library.gaps",
            "What's missing",
            "Per library, how many items lack each thing: identity (no confirmed match), year, description, \
             poster, backdrop, episode titles, files on a drive that answers. Then list them with items.find \
             and its gap filter.",
            Read,
            ANYONE,
            true,
            false,
            gaps,
        ),
        op(
            "items.find",
            "Find items",
            "Items by library, kind, gap, match state, or text in the title or a file's path; paged by \
             cursor. Without parent or kind it lists top-level items (films, shows, videos); kind episode \
             finds episodes across shows; parent lists everything below a show, season or film.",
            Read,
            ANYONE,
            true,
            false,
            find,
        ),
        op(
            "item.get",
            "One item, with evidence",
            "One item as you may see it: every field you can set and where each value came from, what's \
             withheld (a provider's text, with its name), the files' own evidence (path, size, modified time, \
             duration, tracks and their languages, container title, chapters, subtitle files, what flux read \
             from the name), its images, its children and its recent changesets. Read this before editing.",
            Read,
            ANYONE,
            true,
            false,
            item_get,
        ),
        op(
            "items.edit",
            "Edit items",
            "Set or clear fields of up to 100 items as one changeset, with the pages that support them. \
             Apply (Owner, Admin) or propose (anyone may; a Viewer's edit is always a proposal). A field a \
             person set is refused unless force (only when they asked you). dry_run reports what would \
             change. Every applied changeset can be undone (changes.undo).",
            Write,
            EDITORS,
            false,
            true,
            items_edit,
        ),
        op(
            "files.fix",
            "Say what files are",
            "Correct what flux read from file names, for up to 200 files as one changeset: which film or show \
             a file belongs to (of), the title and year to group it under (two films in one item: give each \
             its own), an episode's season and number, or that it is an extra (with its kind) or nothing \
             (ignore). flux places the files again at once, and on every scan after; null clears a fix. \
             Apply (Owner, Admin) or propose; dry_run shows where each file would go. To say what a film \
             is (its title, year, ids), use items.edit instead.",
            Write,
            EDITORS,
            false,
            true,
            files_fix,
        ),
        op(
            "items.merge",
            "Merge items",
            "Films (or shows) that are one work: every file of each from item is fixed to belong to into \
             (episodes keep their numbers), and each goes once it's empty, with what people and agents set \
             on it, everyone's watched state and its collections carried over. One changeset; undo gives \
             them back. dry_run shows where each file would go.",
            Write,
            EDITORS,
            false,
            true,
            items_merge,
        ),
        op(
            "files.plan",
            "Plan renaming files",
            "Name items' files the way media servers read them (scheme standard: Title (Year)/Title \
             (Year).mkv; Show (Year)/Season 01/Show (Year) - S01E02 - Title.mkv; extras in Trailers/, \
             Featurettes/…), or move files where you say (moves). Files named like a video (subtitles, \
             .nfo, .flux.json, images) go with it; a film's or show's own folder is renamed. Nothing \
             moves: the plan is a proposed changeset with every step, and what it leaves alone and why. \
             Show it to the person; it runs when an Owner or Admin applies it (changes.apply, or in the \
             app), and changes.undo puts everything back. flux never replaces a file, never moves one \
             to another drive or out of its library folder.",
            Write,
            "anyone who may propose; applying it takes the Owner or an Admin",
            false,
            false,
            files_plan,
        ),
        op(
            "art.view",
            "Look at an image",
            "An item's image (by art id, from item.get), scaled to width (default 480), as base64. Over MCP \
             it arrives as an image you can look at: check a frame poster or one you added. Withheld \
             provider images can't be viewed.",
            Read,
            ANYONE,
            true,
            false,
            art_view,
        ),
        op(
            "art.add",
            "Add an image",
            "Add a poster, backdrop, thumb or logo from a URL (flux fetches it) or base64 bytes, and show it \
             (pin, default true). Say how it came to be: found (credit who made it and found_at, the page), \
             made (by you, without AI) or generated (by an AI model, named in generator; only when the person \
             asked). flux re-encodes it and journals it; changes.undo removes it.",
            Write,
            ADMIN,
            false,
            false,
            art_add,
        ),
        op(
            "art.pin",
            "Choose an image",
            "Show image art for its item and kind, or with item and kind and no art go back to flux's own \
             choice. Journaled; changes.undo restores the previous choice.",
            Write,
            ADMIN,
            true,
            true,
            art_pin,
        ),
        op(
            "changes.list",
            "Changesets",
            "Changesets newest first (filter by state or item), and how many proposals wait for a decision.",
            Read,
            ANYONE,
            true,
            false,
            changes_list,
        ),
        op(
            "changes.get",
            "One changeset",
            "A changeset with each change: the field, its value before and after, the sources, and what \
             became of it (applied, pending, skipped, reverted, kept).",
            Read,
            ANYONE,
            true,
            false,
            changes_get,
        ),
        op(
            "changes.undo",
            "Undo a changeset",
            "Revert an applied changeset: each field gets its value (and source) back, unless it changed \
             again since (then it is kept, and listed in not_done). An image it added is removed.",
            Write,
            ADMIN,
            false,
            true,
            changes_undo,
        ),
        op(
            "changes.apply",
            "Apply a proposal",
            "Apply a proposed changeset: each change whose field still holds what it held when proposed.",
            Write,
            ADMIN,
            false,
            true,
            changes_apply,
        ),
        op(
            "changes.reject",
            "Turn a proposal down",
            "Reject a proposed changeset; nothing changes.",
            Write,
            ADMIN,
            true,
            false,
            changes_reject,
        ),
        op(
            "lenses.list",
            "Lenses kept on the server",
            "Your lenses and everyone's, as the web client shows them on every device.",
            Read,
            SIGNED_IN,
            true,
            false,
            lenses_list,
        ),
        op(
            "lens.check",
            "Check a lens",
            "Check a lens kit lens (schema: lens) as the web client will: errors to fix, warnings for what \
             the browser removes (images, links, inline styles, url()), and the lens with defaults filled in. \
             Changes nothing.",
            Read,
            ANYONE,
            true,
            false,
            lens_check,
        ),
        op(
            "lens.add",
            "Add a lens",
            "Keep a lens on the server, for you or (Owner, Admin; everyone: true) for everyone: it shows in \
             the lens bar's gallery on every device at once. A lens with the same name replaces the old one.",
            Act,
            SIGNED_IN,
            true,
            true,
            lens_add,
        ),
        op(
            "lens.remove",
            "Remove a lens",
            "Forget a lens kept on the server (yours, or everyone's: Owner or Admin).",
            Act,
            SIGNED_IN,
            true,
            true,
            lens_remove,
        ),
        op(
            "collections.list",
            "Collections",
            "The collections you may see: everyone's, then yours. Each has its titles picked by hand and/or \
             a rule that takes matching titles as the library changes; the app shows them beside the \
             libraries.",
            Read,
            ANYONE,
            true,
            false,
            collections_list,
        ),
        op(
            "collection.get",
            "One collection",
            "A collection's titles: those picked, in order (with why each is there), then those its rule \
             takes, by title; paged.",
            Read,
            ANYONE,
            true,
            false,
            collection_get,
        ),
        op(
            "collection.put",
            "Make or change a collection",
            "Keep a collection on the server, for you or (Owner, Admin; everyone: true) for everyone: \
             \"Ghibli\", \"Rainy Sunday\", \"Kids' films\". Pick titles (items, in order, each with a note; \
             add, remove) and/or give a rule (genres, tags, people, studios, countries, languages, years, \
             text) that takes matching titles as the library changes. A collection of yours with the same \
             name is changed. It shows in the app at once.",
            Act,
            SIGNED_IN,
            true,
            true,
            collection_put,
        ),
        op(
            "collection.remove",
            "Remove a collection",
            "Forget a collection (yours, or everyone's: Owner or Admin). Its titles stay in the library.",
            Act,
            SIGNED_IN,
            true,
            true,
            collection_remove,
        ),
        op(
            "library.scan",
            "Scan a library",
            "Look for new, changed and removed files in a library now (flux also watches its folders). \
             Follow it with status.",
            Act,
            ADMIN,
            true,
            false,
            library_scan,
        ),
    ]
}

// ---- reading --------------------------------------------------------------------------

async fn status(ctx: Ctx, _: Nothing) -> Result<Status, OpError> {
    let st = ctx.st.clone();
    let mut libs = st.catalog.read(read::list_libraries).await.map_err(cat_err)?;
    for lib in &mut libs {
        if let Some(s) = st.scanner.status(lib.id) {
            lib.scan = s;
        }
        st.scanner.overlay_roots(lib);
    }
    let you = ctx.caller.identity.as_ref();
    Ok(Status {
        contract: CONTRACT.into(),
        server: ServerAbout {
            name: st.cfg.name.clone(),
            version: env!("CARGO_PKG_VERSION").into(),
        },
        metadata_source: crate::metadata::source(&st.cfg.tmdb_exports),
        you: You {
            login: ctx.login(),
            name: you.map_or_else(|| "Guest".into(), |i| i.display_name.clone()),
            role: ctx.role(),
            agent: ctx.agent.clone(),
            may: ctx.may(),
        },
        policy: policy::policy(),
        libraries: libs
            .into_iter()
            .map(|l| LibraryAbout {
                id: l.id,
                name: l.name,
                kind: l.kind,
                items: l.item_count,
                scanning: l.scan.state == flux_core::api::ScanState::Scanning,
                roots: l
                    .roots
                    .into_iter()
                    .map(|r| RootAbout {
                        path: r.path,
                        available: r.available,
                    })
                    .collect(),
            })
            .collect(),
    })
}

async fn gaps(ctx: Ctx, input: GapsInput) -> Result<GapsOutput, OpError> {
    let libraries = ctx
        .st
        .catalog
        .read(move |c| agent_read::gaps(c, input.library))
        .await
        .map_err(cat_err)?;
    Ok(GapsOutput { libraries })
}

fn cursor(c: Option<&str>) -> Result<Option<Id>, OpError> {
    match c.map(str::trim).filter(|c| !c.is_empty()) {
        None => Ok(None),
        Some(c) => c
            .parse::<Id>()
            .map(Some)
            .map_err(|_| err(ErrorCode::Invalid, "cursor must be the `next` of a previous page")),
    }
}

async fn find(ctx: Ctx, input: FindInput) -> Result<FindOutput, OpError> {
    let q = FindQuery {
        library: input.library,
        parent: input.parent,
        kind: input.kind,
        gap: input.gap,
        match_state: input.match_state,
        q: input.q.filter(|q| !q.trim().is_empty()),
        limit: input.limit.unwrap_or(50).clamp(1, 200),
        after: cursor(input.cursor.as_deref())?,
    };
    let hide = ctx.hide();
    let (items, next, total) = ctx
        .st
        .catalog
        .read(move |c| agent_read::find(c, &q, &hide))
        .await
        .map_err(cat_err)?;
    Ok(FindOutput {
        items,
        next: next.map(|n| n.to_string()),
        total,
    })
}

async fn item_get(ctx: Ctx, input: ItemRef) -> Result<AgentItemDetail, OpError> {
    let hide = ctx.hide();
    let id = input.item;
    ctx.st
        .catalog
        .read(move |c| agent_read::item(c, id, &hide))
        .await
        .map_err(cat_err)?
        .ok_or_else(|| err(ErrorCode::NotFound, format!("No item {id}")))
}

// ---- edits ------------------------------------------------------------------------------

/// What a caller may do with an edit: apply it, or only propose it.
fn edit_mode(ctx: &Ctx, asked: EditMode) -> Result<bool, OpError> {
    let may = ctx.may();
    if !may.propose {
        return Err(err(
            ErrorCode::Forbidden,
            "Your role can't change the library (Owners and Admins can; Viewers can propose)",
        ));
    }
    Ok(asked == EditMode::Propose || !may.apply)
}

fn check_sources(sources: &[Link]) -> Result<Vec<Link>, OpError> {
    let raw = serde_json::to_value(sources).map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
    fields::links("sources", &raw, 20).map_err(|e| err(ErrorCode::Invalid, e))
}

async fn items_edit(ctx: Ctx, input: EditInput) -> Result<EditOutput, OpError> {
    let propose = edit_mode(&ctx, input.mode)?;
    let summary = input.summary.split_whitespace().collect::<Vec<_>>().join(" ");
    if summary.is_empty() {
        return Err(err(
            ErrorCode::Invalid,
            "Give a summary: what these changes are and why, for the person reviewing them",
        ));
    }
    if summary.chars().count() > MAX_SUMMARY {
        return Err(err(
            ErrorCode::Invalid,
            format!("The summary is longer than {MAX_SUMMARY} characters"),
        ));
    }
    if input.edits.is_empty() {
        return Err(err(ErrorCode::Invalid, "There are no edits"));
    }
    if input.edits.len() > MAX_EDITS {
        return Err(err(
            ErrorCode::TooLarge,
            format!("One changeset edits at most {MAX_EDITS} items: split the work"),
        ));
    }
    // Each value checked against its item's kind, every name known.
    let ids: Vec<Id> = input.edits.iter().map(|e| e.item).collect();
    let kinds: BTreeMap<Id, String> = ctx
        .st
        .catalog
        .read(move |c| {
            let mut m = BTreeMap::new();
            for id in ids {
                if let Some(cur) = fields::read(c, id)? {
                    m.insert(id, cur.kind);
                }
            }
            Ok(m)
        })
        .await
        .map_err(cat_err)?;
    let mut edits = Vec::with_capacity(input.edits.len());
    for e in &input.edits {
        let kind = kinds
            .get(&e.item)
            .ok_or_else(|| err(ErrorCode::NotFound, format!("No item {}", e.item)))?;
        let raw = fields::from_fields(&e.set).map_err(|m| err(ErrorCode::Invalid, m))?;
        let mut set = fields::Values::new();
        for (f, v) in raw {
            let v = fields::check(&f, kind, &v)
                .map_err(|m| err(ErrorCode::Invalid, format!("item {}: {m}", e.item)))?;
            set.insert(f, v);
        }
        for f in &e.clear {
            if !fields::is_field(f) {
                return Err(err(
                    ErrorCode::Invalid,
                    format!("item {}: {f:?} is not a field", e.item),
                ));
            }
        }
        if set.is_empty() && e.clear.is_empty() {
            return Err(err(
                ErrorCode::Invalid,
                format!("item {}: nothing to set or clear", e.item),
            ));
        }
        edits.push(changes::ItemEdit {
            item: e.item,
            set,
            clear: e.clear.clone(),
            sources: check_sources(&e.sources)?,
        });
    }
    let hide = ctx.hide();
    let actor = ctx.actor();
    if input.dry_run {
        let force = input.force;
        let planned = ctx
            .st
            .catalog
            .read(move |c| changes::plan_edit(c, &actor, &edits, force))
            .await
            .map_err(cat_err)?;
        return Ok(EditOutput {
            changeset: None,
            items: planned.into_iter().map(|p| edit_result(p, &hide)).collect(),
        });
    }
    let (committed, planned) = ctx
        .st
        .catalog
        .writer()
        .commit_edit(actor, summary, edits, input.force, propose)
        .await
        .map_err(cat_err)?;
    announce(&ctx.st, &committed);
    if committed.done > 0 {
        ctx.st.activity.wake_jobs();
    }
    Ok(EditOutput {
        changeset: committed.changeset,
        items: planned.into_iter().map(|p| edit_result(p, &hide)).collect(),
    })
}

fn edit_result(p: changes::PlannedItem, hide: &[String]) -> EditResult {
    let title = match &p.title_provider {
        Some(tp) if hide.contains(tp) => p.file_title.clone().unwrap_or_else(|| format!("Item {}", p.item)),
        _ => p.title.clone(),
    };
    EditResult {
        item: p.item,
        title,
        changed: p
            .changes
            .into_iter()
            .map(|c| {
                let withheld = c
                    .before_source
                    .as_ref()
                    .filter(|s| s.source == fields::PROVIDER || s.source == fields::SIDECAR)
                    .and_then(|s| s.provider.clone())
                    .filter(|p| hide.contains(p) && !c.field.starts_with("ids."));
                FieldChange {
                    before: if withheld.is_some() { None } else { c.before },
                    after: c.after,
                    field: c.field,
                    withheld,
                }
            })
            .collect(),
        unchanged: p.unchanged,
        refused: p
            .refused
            .into_iter()
            .map(|(field, reason)| Refused { field, reason })
            .collect(),
    }
}

// ---- what files are ------------------------------------------------------------------------

/// Most files one `files.fix` fixes.
pub const MAX_FIXES: usize = flux_catalog::fixes::MAX_FIXES;

pub(crate) fn summary_of_input(summary: &str) -> Result<String, OpError> {
    let summary = summary.split_whitespace().collect::<Vec<_>>().join(" ");
    if summary.is_empty() {
        return Err(err(
            ErrorCode::Invalid,
            "Give a summary: what these changes are and why, for the person reviewing them",
        ));
    }
    if summary.chars().count() > MAX_SUMMARY {
        return Err(err(
            ErrorCode::Invalid,
            format!("The summary is longer than {MAX_SUMMARY} characters"),
        ));
    }
    Ok(summary)
}

fn fix_output(committed: changes::Committed, outcomes: Vec<flux_catalog::fixes::FixOutcome>) -> FixOutput {
    let place = |(item, title): (Option<Id>, Option<String>)| FilePlace { item, title };
    FixOutput {
        changeset: committed.changeset,
        files: outcomes
            .into_iter()
            .map(|o| FixResult {
                file: o.file,
                path: o.path,
                before: place(o.before),
                after: o.after.map(place),
                fix: o.fix,
                unchanged: o.unchanged,
            })
            .collect(),
    }
}

async fn commit_fixes(
    ctx: &Ctx,
    summary: String,
    edits: Vec<flux_catalog::fixes::FixEdit>,
    mode: EditMode,
    dry_run: bool,
) -> Result<FixOutput, OpError> {
    let propose = edit_mode(ctx, mode)?;
    let (committed, outcomes) = ctx
        .st
        .catalog
        .writer()
        .commit_fixes(ctx.actor(), summary, edits, propose, dry_run, ctx.hide())
        .await
        .map_err(cat_err)?;
    announce(&ctx.st, &committed);
    if committed.done > 0 {
        ctx.st.activity.wake_jobs();
    }
    Ok(fix_output(committed, outcomes))
}

async fn files_fix(ctx: Ctx, input: FixInput) -> Result<FixOutput, OpError> {
    edit_mode(&ctx, input.mode)?;
    let summary = summary_of_input(&input.summary)?;
    if input.fixes.is_empty() {
        return Err(err(ErrorCode::Invalid, "There are no fixes"));
    }
    if input.fixes.len() > MAX_FIXES {
        return Err(err(
            ErrorCode::TooLarge,
            format!("One changeset fixes at most {MAX_FIXES} files: split the work"),
        ));
    }
    let mut edits = Vec::with_capacity(input.fixes.len());
    for f in input.fixes {
        edits.push(flux_catalog::fixes::FixEdit {
            file: f.file,
            fix: f.fix,
            sources: check_sources(&f.sources)?,
        });
    }
    commit_fixes(&ctx, summary, edits, input.mode, input.dry_run).await
}

async fn files_plan(ctx: Ctx, input: PlanInput) -> Result<PlanOutput, OpError> {
    files::plan(&ctx, input).await
}

/// Whether changeset `cs` is a file plan (its steps move files).
async fn is_plan(ctx: &Ctx, cs: Id) -> Result<bool, OpError> {
    ctx.st
        .catalog
        .read(move |c| flux_catalog::organize::is_plan(c, cs))
        .await
        .map_err(cat_err)
}

async fn items_merge(ctx: Ctx, input: MergeInput) -> Result<FixOutput, OpError> {
    edit_mode(&ctx, input.mode)?;
    let summary = summary_of_input(&input.summary)?;
    let (into, from) = (input.into, input.from.clone());
    let edits = ctx
        .st
        .catalog
        .read(move |c| flux_catalog::fixes::merge_edits(c, into, &from, &[]))
        .await
        .map_err(cat_err)?;
    if edits.len() > MAX_FIXES {
        return Err(err(
            ErrorCode::TooLarge,
            format!(
                "Those items have {} files; one changeset fixes at most {MAX_FIXES}: merge fewer at once",
                edits.len()
            ),
        ));
    }
    commit_fixes(&ctx, summary, edits, input.mode, input.dry_run).await
}

// ---- art ----------------------------------------------------------------------------------

fn art_kind(k: ArtKind) -> flux_catalog::ArtKind {
    match k {
        ArtKind::Poster => flux_catalog::ArtKind::Poster,
        ArtKind::Backdrop => flux_catalog::ArtKind::Backdrop,
        ArtKind::Thumb => flux_catalog::ArtKind::Thumb,
        ArtKind::Logo => flux_catalog::ArtKind::Logo,
    }
}

/// The provider an image is credited to, as the policy names providers.
fn art_provider(row: &read::ArtRow) -> Option<String> {
    (row.source == ArtSource::Provider)
        .then(|| {
            row.attribution
                .as_deref()
                .map(|a| a.trim().to_ascii_lowercase().replace([' ', '.'], ""))
        })
        .flatten()
}

async fn art_view(ctx: Ctx, input: ArtViewInput) -> Result<ArtImage, OpError> {
    let id = input.art;
    let row = ctx
        .st
        .catalog
        .read(move |c| read::art(c, id))
        .await
        .map_err(cat_err)?
        .ok_or_else(|| err(ErrorCode::NotFound, format!("No image {id}")))?;
    if let Some(p) = art_provider(&row).filter(|p| ctx.hide().contains(p)) {
        return Err(err(
            ErrorCode::Forbidden,
            format!("Image {id} is {p}'s, and {p}'s terms keep its images from AI"),
        ));
    }
    let w = crate::art::bucket(Some(input.width.unwrap_or(480)));
    let (bytes, mime) = ctx.st.art.render(&row, w).await.map_err(super::api_err)?;
    Ok(ArtImage {
        art: id,
        mime: mime.into(),
        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

fn agent_art(row: &read::ArtRow, shown: bool) -> AgentArt {
    AgentArt {
        id: row.id,
        kind: match row.kind.as_str() {
            "poster" => ArtKind::Poster,
            "backdrop" => ArtKind::Backdrop,
            "thumb" => ArtKind::Thumb,
            _ => ArtKind::Logo,
        },
        source: row.source,
        shown,
        pinned: row.pinned,
        width: row.width,
        height: row.height,
        credit: row.attribution.clone(),
        found_at: row.found_at.clone(),
        generator: row.generator.clone(),
        url: Some(format!("/api/v1/art/{}", row.id)),
        withheld: None,
    }
}

async fn art_add(ctx: Ctx, input: ArtAddInput) -> Result<ArtAddOutput, OpError> {
    ctx.require_admin()?;
    let tidy = |s: Option<String>| {
        s.map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|s| !s.is_empty())
    };
    let credit = tidy(input.credit.clone());
    let generator = tidy(input.generator.clone());
    let found_at = match input.found_at.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(u) => Some(
            fields::link(u, None)
                .ok_or_else(|| err(ErrorCode::Invalid, "found_at must be an http or https address"))?
                .url,
        ),
        None => None,
    };
    match input.origin {
        ArtOrigin::Found if credit.is_none() => {
            return Err(err(
                ErrorCode::Invalid,
                "A found image needs credit: who made it (a studio, a photographer, a site)",
            ))
        }
        ArtOrigin::Generated if generator.is_none() => {
            return Err(err(
                ErrorCode::Invalid,
                "A generated image needs generator: the AI model that made it",
            ))
        }
        _ => {}
    }
    let bytes = match (&input.url, &input.data_base64) {
        (Some(u), None) => fetch::fetch(u).await?,
        (None, Some(b)) => {
            if b.len() > fetch::MAX_BYTES * 4 / 3 + 4 {
                return Err(err(ErrorCode::TooLarge, "Images are at most 20 MB"));
            }
            base64::engine::general_purpose::STANDARD
                .decode(b.trim())
                .map_err(|_| err(ErrorCode::Invalid, "data_base64 isn't base64"))?
        }
        _ => {
            return Err(err(
                ErrorCode::Invalid,
                "Give the image as url or as data_base64 (one of them)",
            ))
        }
    };
    let kind = input.kind;
    let prepared = tokio::task::spawn_blocking(move || fetch::prepare(&bytes, kind))
        .await
        .map_err(|e| err(ErrorCode::Internal, e.to_string()))??;
    let hash = xxhash_rust::xxh3::xxh3_64(&prepared.bytes);
    let name = format!(
        "{}-custom{}-{:08x}.{}",
        input.item,
        kind.as_str(),
        hash as u32,
        prepared.ext
    );
    let dir = ctx.st.art.dir.clone();
    let data = prepared.bytes.clone();
    let file = dir.join(&name);
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let tmp = file.with_extension("part");
        std::fs::write(&tmp, &data)?;
        std::fs::rename(&tmp, &file)
    })
    .await
    .map_err(|e| err(ErrorCode::Internal, e.to_string()))?
    .map_err(|e| err(ErrorCode::Internal, format!("Saving the image failed: {e}")))?;
    let (source, attribution) = match input.origin {
        ArtOrigin::Generated => (ArtSource::Generated, credit),
        ArtOrigin::Made => (
            ArtSource::Custom,
            credit.or_else(|| {
                Some(format!(
                    "Made by {}",
                    ctx.agent.clone().unwrap_or_else(|| ctx.login())
                ))
            }),
        ),
        ArtOrigin::Found => (ArtSource::Custom, credit),
    };
    let what = match kind {
        ArtKind::Poster => "a poster",
        ArtKind::Backdrop => "a backdrop",
        ArtKind::Thumb => "a thumb",
        ArtKind::Logo => "a logo",
    };
    let summary = tidy(input.summary.clone()).unwrap_or_else(|| match &attribution {
        Some(a) if input.origin == ArtOrigin::Found => format!("Added {what} ({a})"),
        _ if input.origin == ArtOrigin::Generated => {
            format!("Added {what} made with {}", generator.clone().unwrap_or_default())
        }
        _ => format!("Added {what}"),
    });
    let sources: Vec<Link> = found_at
        .iter()
        .map(|u| Link {
            url: u.clone(),
            title: None,
        })
        .collect();
    let art = flux_catalog::NewArt {
        kind: art_kind(kind),
        source,
        path: Some(crate::art::ArtStore::stored_path(&name)),
        url: None,
        width: Some(prepared.width),
        height: Some(prepared.height),
        blurhash: prepared.blurhash.clone(),
        attribution,
        generator,
        gen_version: None,
        gen_title: None,
    };
    let added = ctx
        .st
        .catalog
        .writer()
        .add_chosen_art(
            ctx.actor(),
            summary,
            input.item,
            art,
            found_at,
            input.pin,
            sources,
        )
        .await;
    let (id, committed) = match added {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_file(ctx.st.art.dir.join(&name));
            return Err(cat_err(e));
        }
    };
    announce(&ctx.st, &committed);
    let row = ctx
        .st
        .catalog
        .read(move |c| read::art(c, id))
        .await
        .map_err(cat_err)?
        .ok_or_else(|| err(ErrorCode::Internal, "the new image vanished"))?;
    Ok(ArtAddOutput {
        art: agent_art(&row, input.pin),
        changeset: committed
            .changeset
            .ok_or_else(|| err(ErrorCode::Internal, "no changeset"))?,
    })
}

async fn art_pin(ctx: Ctx, input: ArtPinInput) -> Result<ArtPinOutput, OpError> {
    ctx.require_admin()?;
    let (item, kind, art) = match (input.art, input.item, input.kind) {
        (Some(a), _, _) => {
            let row = ctx
                .st
                .catalog
                .read(move |c| read::art(c, a))
                .await
                .map_err(cat_err)?
                .ok_or_else(|| err(ErrorCode::NotFound, format!("No image {a}")))?;
            let kind = match row.kind.as_str() {
                "poster" => ArtKind::Poster,
                "backdrop" => ArtKind::Backdrop,
                "thumb" => ArtKind::Thumb,
                _ => ArtKind::Logo,
            };
            (row.item_id, kind, Some(a))
        }
        (None, Some(i), Some(k)) => (i, k, None),
        _ => {
            return Err(err(
                ErrorCode::Invalid,
                "Give art (to show it), or item and kind (to go back to flux's choice)",
            ))
        }
    };
    let summary = input
        .summary
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| match art {
            Some(_) => format!("Chose the {}", kind.as_str()),
            None => format!("Let flux choose the {}", kind.as_str()),
        });
    let committed = ctx
        .st
        .catalog
        .writer()
        .choose_art(ctx.actor(), summary, item, art_kind(kind), art)
        .await
        .map_err(cat_err)?;
    announce(&ctx.st, &committed);
    let k = kind.as_str();
    let shown = ctx
        .st
        .catalog
        .read(move |c| {
            let rows = read::item_art(c, item)?;
            Ok(rows.into_iter().find(|r| r.kind == k))
        })
        .await
        .map_err(cat_err)?;
    Ok(ArtPinOutput {
        changeset: committed.changeset,
        shown: shown.map(|r| agent_art(&r, true)),
    })
}

// ---- changesets ---------------------------------------------------------------------------

async fn changes_list(ctx: Ctx, input: ChangesListInput) -> Result<ChangesListOutput, OpError> {
    let limit = input.limit.unwrap_or(50).clamp(1, 200);
    let (changesets, proposed) = ctx
        .st
        .catalog
        .read(move |c| {
            Ok((
                changes::list(c, input.state, input.item, input.before, limit)?,
                changes::proposed_count(c)?,
            ))
        })
        .await
        .map_err(cat_err)?;
    let next = (changesets.len() as u32 == limit)
        .then(|| changesets.last().map(|c| c.id))
        .flatten();
    Ok(ChangesListOutput {
        changesets,
        next,
        proposed,
    })
}

async fn changes_get(ctx: Ctx, input: ChangesetRef) -> Result<Changeset, OpError> {
    let hide = ctx.hide();
    let id = input.changeset;
    ctx.st
        .catalog
        .read(move |c| {
            let Some((summary, rows)) = changes::get(c, id)? else {
                return Ok(None);
            };
            let mut out = Vec::with_capacity(rows.len());
            for r in rows {
                let title = match (&r.title, &r.title_provider) {
                    (Some(_), Some(p)) if hide.contains(p) => fields::file_title(c, r.item)?,
                    (t, _) => t.clone(),
                };
                let withheld = r
                    .before_provider
                    .clone()
                    .filter(|p| hide.contains(p) && !r.field.starts_with("ids."));
                out.push(Change {
                    item: r.item,
                    title,
                    before: if withheld.is_some() { None } else { r.before },
                    after: r.after,
                    field: r.field,
                    withheld,
                    sources: r.sources,
                    state: r.state,
                });
            }
            Ok(Some(Changeset {
                summary,
                changes: out,
            }))
        })
        .await
        .map_err(cat_err)?
        .ok_or_else(|| err(ErrorCode::NotFound, format!("No changeset {id}")))
}

fn decided(c: changes::Committed) -> Result<Decided, OpError> {
    Ok(Decided {
        changeset: c
            .changeset
            .ok_or_else(|| err(ErrorCode::Internal, "no changeset"))?,
        done: c.done,
        not_done: c
            .not_done
            .into_iter()
            .map(|(item, field, reason)| NotDone { item, field, reason })
            .collect(),
    })
}

/// A catalog refusal of a decision: the changeset isn't in that state.
fn decision_err(e: flux_catalog::CatalogError) -> OpError {
    match e {
        flux_catalog::CatalogError::Invalid(m) => err(ErrorCode::Conflict, m),
        e => cat_err(e),
    }
}

async fn changes_undo(ctx: Ctx, input: ChangesetRef) -> Result<Decided, OpError> {
    ctx.require_admin()?;
    if is_plan(&ctx, input.changeset).await? {
        let c = files::run(&ctx, input.changeset, true).await?;
        ctx.st.activity.wake_jobs();
        return decided(c);
    }
    let c = ctx
        .st
        .catalog
        .writer()
        .revert_changeset(input.changeset, ctx.login())
        .await
        .map_err(decision_err)?;
    if !c.art_removed.is_empty() {
        let st = ctx.st.clone();
        let (ids, paths): (Vec<Id>, Vec<Option<String>>) = c.art_removed.iter().cloned().unzip();
        let paths: Vec<String> = paths.into_iter().flatten().collect();
        let _ = tokio::task::spawn_blocking(move || st.art.remove(&paths, &ids)).await;
    }
    announce(&ctx.st, &c);
    ctx.st.activity.wake_jobs();
    decided(c)
}

async fn changes_apply(ctx: Ctx, input: ChangesetRef) -> Result<Decided, OpError> {
    ctx.require_admin()?;
    if is_plan(&ctx, input.changeset).await? {
        let c = files::run(&ctx, input.changeset, false).await?;
        ctx.st.activity.wake_jobs();
        return decided(c);
    }
    let c = ctx
        .st
        .catalog
        .writer()
        .apply_proposal(input.changeset, ctx.login())
        .await
        .map_err(decision_err)?;
    announce(&ctx.st, &c);
    ctx.st.activity.wake_jobs();
    decided(c)
}

async fn changes_reject(ctx: Ctx, input: ChangesetRef) -> Result<Decided, OpError> {
    ctx.require_admin()?;
    let c = ctx
        .st
        .catalog
        .writer()
        .reject_proposal(input.changeset, ctx.login())
        .await
        .map_err(decision_err)?;
    announce(&ctx.st, &c);
    decided(c)
}

// ---- lenses -------------------------------------------------------------------------------

fn signed_in(ctx: &Ctx) -> Result<(), OpError> {
    match ctx.role() {
        Some(_) => Ok(()),
        None => Err(err(
            ErrorCode::Forbidden,
            "Lenses are kept per person: connect over the tailnet (or on this machine) to keep one",
        )),
    }
}

async fn lenses_list(ctx: Ctx, _: Nothing) -> Result<Lenses, OpError> {
    if ctx.role().is_none() {
        return Ok(Lenses { lenses: Vec::new() });
    }
    let user = ctx.user_id().await?;
    let lenses = ctx
        .st
        .catalog
        .read(move |c| flux_catalog::lenses::list(c, user))
        .await
        .map_err(cat_err)?;
    Ok(Lenses { lenses })
}

async fn lens_check(_: Ctx, input: LensCheckInput) -> Result<LensCheck, OpError> {
    Ok(lens::check(&input.spec))
}

async fn lens_add(ctx: Ctx, input: LensAddInput) -> Result<LensAdded, OpError> {
    signed_in(&ctx)?;
    if input.everyone {
        ctx.require_admin()?;
    }
    let checked = lens::check(&input.spec);
    let (Some(spec), Some(id)) = (checked.lens, checked.id) else {
        return Err(err(ErrorCode::Invalid, checked.errors.join(" ")));
    };
    let owner = if input.everyone {
        flux_catalog::lenses::EVERYONE
    } else {
        ctx.user_id().await?
    };
    let lens = ctx
        .st
        .catalog
        .writer()
        .put_lens(owner, id, spec, ctx.login(), ctx.agent.clone())
        .await
        .map_err(cat_err)?;
    ctx.st.events.send(flux_core::api::Event::Lenses);
    Ok(LensAdded {
        lens,
        warnings: checked.warnings,
    })
}

async fn lens_remove(ctx: Ctx, input: LensRemoveInput) -> Result<Removed, OpError> {
    signed_in(&ctx)?;
    let owner = if input.everyone {
        ctx.require_admin()?;
        flux_catalog::lenses::EVERYONE
    } else {
        ctx.user_id().await?
    };
    let removed = ctx
        .st
        .catalog
        .writer()
        .remove_lens(owner, input.id)
        .await
        .map_err(cat_err)?;
    if removed {
        ctx.st.events.send(flux_core::api::Event::Lenses);
    }
    Ok(Removed { removed })
}

// ---- collections ----------------------------------------------------------------------------

/// The caller's user id, if signed in.
async fn user_of(ctx: &Ctx) -> Result<Option<Id>, OpError> {
    match ctx.role() {
        Some(_) => Ok(Some(ctx.user_id().await?)),
        None => Ok(None),
    }
}

/// A collection the caller may see.
async fn visible_collection(ctx: &Ctx, id: Id) -> Result<flux_catalog::collections::Row, OpError> {
    let user = user_of(ctx).await?;
    let row = ctx
        .st
        .catalog
        .read(move |c| flux_catalog::collections::get(c, id))
        .await
        .map_err(cat_err)?;
    match row {
        Some(r) if flux_catalog::collections::visible(&r, user) => Ok(r),
        _ => Err(err(ErrorCode::NotFound, format!("No collection {id}"))),
    }
}

async fn collections_list(ctx: Ctx, _: Nothing) -> Result<Collections, OpError> {
    let user = user_of(&ctx).await?;
    let hide = ctx.hide();
    let collections = ctx
        .st
        .catalog
        .read(move |c| flux_catalog::collections::list(c, user, &hide))
        .await
        .map_err(cat_err)?;
    Ok(Collections { collections })
}

async fn collection_get(ctx: Ctx, input: CollectionGetInput) -> Result<CollectionItems, OpError> {
    let row = visible_collection(&ctx, input.collection).await?;
    let hide = ctx.hide();
    let limit = input.limit.unwrap_or(50).clamp(1, 200) as usize;
    let offset = cursor(input.cursor.as_deref())?.unwrap_or(0).max(0) as usize;
    ctx.st
        .catalog
        .read(move |c| {
            let info = flux_catalog::collections::info(c, &row, &hide)?;
            let members = flux_catalog::collections::members(c, &row, &hide)?;
            let notes = flux_catalog::collections::notes(c, &row)?;
            let mut items = Vec::new();
            for id in members.iter().skip(offset).take(limit) {
                if let Some(it) = agent_read::agent_item(c, *id, &hide)? {
                    items.push(it);
                }
            }
            let next = (offset + limit < members.len()).then(|| (offset + limit).to_string());
            Ok(CollectionItems {
                collection: info,
                notes: notes
                    .into_iter()
                    .filter(|(i, _)| members.contains(i))
                    .map(|(i, n)| (i.to_string(), n))
                    .collect(),
                items,
                next,
                total: members.len() as u64,
            })
        })
        .await
        .map_err(cat_err)
}

fn tidy_text(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

async fn collection_put(ctx: Ctx, input: CollectionPutInput) -> Result<CollectionInfo, OpError> {
    if ctx.role().is_none() {
        return Err(err(
            ErrorCode::Forbidden,
            "Collections are kept per person: connect over the tailnet (or on this machine) to keep one",
        ));
    }
    if input.everyone {
        ctx.require_admin()?;
    }
    let name = tidy_text(&input.name);
    if name.is_empty() || name.chars().count() > 60 {
        return Err(err(ErrorCode::Invalid, "Give a name of 1 to 60 characters"));
    }
    let description = input
        .description
        .as_deref()
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty());
    if description.as_ref().is_some_and(|d| d.chars().count() > 1000) {
        return Err(err(
            ErrorCode::Invalid,
            "The description is longer than 1,000 characters",
        ));
    }
    let tidy_picks = |v: &[CollectionPick]| -> Result<Vec<CollectionPick>, OpError> {
        v.iter()
            .map(|p| {
                let note = p.note.as_deref().map(tidy_text).filter(|n| !n.is_empty());
                if note.as_ref().is_some_and(|n| n.chars().count() > 300) {
                    return Err(err(
                        ErrorCode::Invalid,
                        format!("item {}: the note is longer than 300 characters", p.item),
                    ));
                }
                Ok(CollectionPick { item: p.item, note })
            })
            .collect()
    };
    let items = input.items.as_deref().map(tidy_picks).transpose()?;
    let add = tidy_picks(&input.add)?;
    if items.as_ref().map_or(0, Vec::len) + add.len() > flux_catalog::collections::MAX_PICKED {
        return Err(err(
            ErrorCode::TooLarge,
            format!(
                "A collection picks at most {} titles: use a rule for more",
                flux_catalog::collections::MAX_PICKED
            ),
        ));
    }
    if let Some(r) = &input.rule {
        let lists = [
            &r.genres,
            &r.tags,
            &r.people,
            &r.studios,
            &r.countries,
            &r.languages,
        ];
        if lists.iter().any(|l| l.len() > 50) || r.libraries.len() > 50 {
            return Err(err(ErrorCode::Invalid, "A rule lists at most 50 values for each"));
        }
    }
    let owner = if input.everyone {
        flux_catalog::collections::EVERYONE
    } else {
        ctx.user_id().await?
    };
    let put = flux_catalog::collections::Put {
        id: input.id,
        owner,
        name,
        description,
        items,
        add,
        remove: input.remove.clone(),
        rule: input.rule.clone(),
        sources: check_sources(&input.sources)?,
        by: ctx.login(),
        agent: ctx.agent.clone(),
    };
    let row = ctx
        .st
        .catalog
        .writer()
        .put_collection(put)
        .await
        .map_err(cat_err)?;
    ctx.st.events.send(flux_core::api::Event::Collections);
    let hide = ctx.hide();
    ctx.st
        .catalog
        .read(move |c| flux_catalog::collections::info(c, &row, &hide))
        .await
        .map_err(cat_err)
}

async fn collection_remove(ctx: Ctx, input: CollectionRef) -> Result<Removed, OpError> {
    let row = visible_collection(&ctx, input.collection).await?;
    if row.owner == flux_catalog::collections::EVERYONE {
        ctx.require_admin()?;
    } else if ctx.role().is_none() {
        return Err(err(ErrorCode::Forbidden, "Sign in to remove your collections"));
    }
    let removed = ctx
        .st
        .catalog
        .writer()
        .remove_collection(row.owner, row.id)
        .await
        .map_err(cat_err)?;
    if removed {
        ctx.st.events.send(flux_core::api::Event::Collections);
    }
    Ok(Removed { removed })
}

// ---- scans ------------------------------------------------------------------------------

async fn library_scan(ctx: Ctx, input: ScanInput) -> Result<ScanOutput, OpError> {
    ctx.require_admin()?;
    let id = input.library;
    let lib = ctx
        .st
        .catalog
        .library(id)
        .await
        .map_err(cat_err)?
        .ok_or_else(|| err(ErrorCode::NotFound, format!("No library {id}")))?;
    crate::api::start_scan(&ctx.st, id, crate::scan::Priority::Interactive)
        .map_err(|e| err(ErrorCode::Unavailable, e))?;
    let scan = ctx.st.scanner.status(id).unwrap_or(lib.scan);
    Ok(ScanOutput { library: id, scan })
}

/// Roles that may only propose (for docs and tests).
pub fn proposes_only(role: Option<Role>) -> bool {
    role == Some(Role::Viewer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn a_tmdb_value_before_is_withheld_from_agents() {
        let p = changes::PlannedItem {
            item: 1,
            title: "Parasite".into(),
            title_provider: Some("tmdb".into()),
            file_title: Some("Gisaengchung".into()),
            kind: "movie".into(),
            changes: vec![changes::Planned {
                field: "overview".into(),
                before: Some(Value::String("TMDB's".into())),
                after: Some(Value::String("Mine".into())),
                before_source: Some(fields::FieldRow {
                    source: fields::PROVIDER.into(),
                    provider: Some("tmdb".into()),
                    agent: None,
                    actor: None,
                    changeset: None,
                    sources: vec![],
                    at: 0,
                }),
                clear: false,
            }],
            unchanged: vec![],
            refused: vec![],
            sources: vec![],
        };
        let r = edit_result(p.clone(), &["tmdb".into()]);
        assert_eq!(
            (r.changed[0].before.as_ref(), r.changed[0].withheld.as_deref()),
            (None, Some("tmdb"))
        );
        assert_eq!(r.title, "Gisaengchung");
        let r = edit_result(p, &[]);
        assert!(r.changed[0].before.is_some());
        assert_eq!(r.title, "Parasite");
    }
}
