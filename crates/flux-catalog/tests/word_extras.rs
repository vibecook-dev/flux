//! Extras folders named by one of their words (`Bonus Clips)`, `Bonus
//! Trailers`) rather than exactly (`Featurettes`, which flux-names already
//! knows): their files are extras of the one film beside the folder, in any
//! scan order, and stay films of their own whenever that is not clear.

mod common;

use common::*;
use flux_catalog::group::extras_folder_by_word;
use flux_core::api::ItemKind;
use flux_core::{ExtraKind, LibraryKind};

const FILM: &str = "Some Film (2011)/1080P/Some.Film.2011.1080p.BluRay.mkv";
const CLIPS: [(&str, &str); 3] = [
    ("Some Film (2011)/1080P/Bonus Clips)/Song One.mkv", "Song One"),
    (
        "Some Film (2011)/1080P/Bonus Clips)/Song Two (Karaoke).mkv",
        "Song Two",
    ),
    ("Some Film (2011)/1080P/Bonus Clips)/Making, Of.mkv", "Making, Of"),
];

fn film() -> flux_catalog::FileUpsert {
    upsert(FILM, movie("Some Film", Some(2011)), Some(seed("h264_mkv")))
}

fn clips() -> Vec<flux_catalog::FileUpsert> {
    CLIPS
        .iter()
        .map(|(path, title)| upsert(path, movie(title, None), Some(seed("h264_mkv"))))
        .collect()
}

#[test]
fn folder_words_name_extras_folders_conservatively() {
    assert_eq!(
        extras_folder_by_word("Film (2011)/1080P/Bonus Clips)/x.mkv"),
        Some((ExtraKind::Other, "Film (2011)/1080P"))
    );
    assert_eq!(
        extras_folder_by_word("Film/Bonus Trailers/x.mkv"),
        Some((ExtraKind::Trailer, "Film"))
    );
    assert_eq!(
        extras_folder_by_word("Film/The Behind-the-Scenes Reel/x.mkv"),
        Some((ExtraKind::BehindTheScenes, "Film"))
    );
    assert_eq!(
        extras_folder_by_word("电影/花絮 Extras/x.mkv"),
        Some((ExtraKind::Other, "电影"))
    );
    for no in [
        // At the root: no film folder above.
        "More Clips/x.mkv",
        "x.mkv",
        // A year: a film's own folder.
        "Movies/Shorts (2009)/Shorts.2009.mkv",
        "Movies/Extras 2019/x.mkv",
        // Not a whole word, or singular.
        "Movies/Clipstone/x.mkv",
        "Movies/Extra Ordinary/x.mkv",
        "Movies/The Trailer Park/x.mkv",
        "Movies/Scenes from a Marriage/x.mkv",
    ] {
        assert_eq!(extras_folder_by_word(no), None, "{no}");
    }
}

/// The clips become extras of the film whichever batch brings which: the
/// film first, the clips first (an earlier batch placed them as films), or
/// both in one batch.
#[test]
fn clips_beside_a_film_are_its_extras_in_any_scan_order() {
    for order in ["film first", "clips first", "one batch"] {
        let e = env(LibraryKind::Movies);
        match order {
            "film first" => {
                e.apply(vec![film()]);
                e.apply(clips());
            }
            "clips first" => {
                e.apply(clips());
                assert_eq!(e.top().len(), 3, "no film yet: films of their own");
                e.apply(vec![film()]);
            }
            _ => {
                e.apply(clips().into_iter().chain([film()]).collect());
            }
        }
        let top = e.top();
        assert_eq!(top.len(), 1, "{order}: {top:#?}");
        assert_eq!(top[0].title, "Some Film");
        let kids = e.children(top[0].id);
        assert_eq!(kids.len(), 3, "{order}: {kids:#?}");
        assert!(kids.iter().all(|k| k.kind == ItemKind::Extra), "{order}");
        let mut titles: Vec<_> = kids.iter().map(|k| k.title.clone()).collect();
        titles.sort();
        assert_eq!(
            titles,
            ["Making, Of", "Song One", "Song Two (Karaoke)"],
            "{order}: extras are named after their files"
        );
        assert_eq!(e.detail(top[0].id).assets.len(), 1, "{order}");
        // A clip added later attaches at once.
        e.apply(vec![upsert(
            "Some Film (2011)/1080P/Bonus Clips)/Song Three.mkv",
            movie("Song Three", None),
            None,
        )]);
        assert_eq!(e.top().len(), 1, "{order}");
        assert_eq!(e.children(top[0].id).len(), 4, "{order}");
    }
}

/// Whenever the owner isn't clear, the files stay films of their own.
#[test]
fn unclear_extras_folders_stay_films() {
    // A folder of films directly (two films beside the clips folder).
    let e = env(LibraryKind::Movies);
    e.apply(vec![
        upsert("Collection/Film A.mkv", movie("Film A", None), None),
        upsert("Collection/Film B.mkv", movie("Film B", None), None),
        upsert("Collection/Fan Clips/Clip.mkv", movie("Clip", None), None),
    ]);
    assert_eq!(e.top().len(), 3);

    // No film beside the folder at all.
    let e = env(LibraryKind::Movies);
    e.apply(vec![upsert(
        "Concerts/Live Clips/Show.mkv",
        movie("Show", None),
        None,
    )]);
    assert_eq!(e.top().len(), 1);
    assert_eq!(e.top()[0].kind, ItemKind::Movie);

    // A film's own folder that merely has the word and a year.
    let e = env(LibraryKind::Movies);
    e.apply(vec![
        upsert("Movies/Feature.mkv", movie("Feature", None), None),
        upsert(
            "Movies/Shorts (2009)/Shorts.2009.mkv",
            movie("Shorts", Some(2009)),
            None,
        ),
    ]);
    assert_eq!(e.top().len(), 2);

    // An explicit provider id says it is a film.
    let e = env(LibraryKind::Movies);
    e.apply(vec![
        film(),
        upsert(
            "Some Film (2011)/1080P/Bonus Clips)/Other [tmdbid-603].mkv",
            movie_tmdb("Other", None, 603),
            None,
        ),
    ]);
    assert_eq!(e.top().len(), 2);
}
