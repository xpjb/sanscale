//! Example-only font discovery. The reusable adapter takes handles, not fontdb.
use super::preview::Faces;
use sanscale::{FontHandle, TextService};
const FALLBACK: &[&str] = &[
    "Noto Color Emoji",
    "Segoe UI Emoji",
    "Apple Color Emoji",
    "Noto Sans CJK SC",
    "Microsoft YaHei",
    "Noto Sans",
    "DejaVu Sans",
];
fn query(db: &fontdb::Database, name: &str, variant: usize) -> Option<fontdb::ID> {
    let id = db.query(&fontdb::Query {
        families: &[fontdb::Family::Name(name)],
        weight: if variant & 1 != 0 {
            fontdb::Weight::BOLD
        } else {
            fontdb::Weight::NORMAL
        },
        style: if variant & 2 != 0 {
            fontdb::Style::Italic
        } else {
            fontdb::Style::Normal
        },
        ..Default::default()
    })?;
    let f = db.face(id)?;
    if variant & 1 != 0 && f.weight < fontdb::Weight::SEMIBOLD {
        return None;
    }
    if variant & 2 != 0 && f.style == fontdb::Style::Normal {
        return None;
    }
    Some(id)
}
fn map(db: &mut fontdb::Database, text: &mut TextService, id: fontdb::ID) -> FontHandle {
    // SAFETY: as in common's font loader, system font files must not be
    // modified in place while their mapped bytes are in use.
    let (data, index) = unsafe { db.make_shared_face_data(id) }.expect("map system font");
    text.map_font(data, index).expect("parse system font")
}
pub fn load(text: &mut TextService, requested: Option<&str>) -> Faces {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    let fallback = FALLBACK
        .iter()
        .filter_map(|name| query(&db, name, 0))
        .collect::<Vec<_>>()
        .into_iter()
        .map(|id| map(&mut db, text, id))
        .collect::<Vec<_>>();
    let sans = if let Some(name) = requested {
        if query(&db, name, 0).is_some() {
            vec![name]
        } else {
            eprintln!("requested font {name} not found");
            vec!["DejaVu Sans", "Segoe UI", "Arial", "Liberation Sans"]
        }
    } else {
        vec!["DejaVu Sans", "Segoe UI", "Arial", "Liberation Sans"]
    };
    let mut load_family = |names: &[&str]| {
        let name = names
            .iter()
            .copied()
            .find(|name| (0..4).all(|v| query(&db, name, v).is_some()))
            .or_else(|| {
                names
                    .iter()
                    .copied()
                    .find(|name| query(&db, name, 0).is_some())
            })
            .expect("install DejaVu Sans + Sans Mono (including bold/oblique variants)");
        let normal = query(&db, name, 0).unwrap();
        std::array::from_fn(|v| {
            let id = query(&db, name, v).unwrap_or_else(|| {
                eprintln!(
                    "{name}: missing real face variant {v}, using regular; no synthetic styling"
                );
                normal
            });
            let face = map(&mut db, text, id);
            let mut handles = vec![face];
            handles.extend(fallback.iter().copied().filter(|&h| h != face));
            text.register_chain(&handles)
        })
    };

    Faces {
        prose: load_family(&sans),
        mono: load_family(&[
            "DejaVu Sans Mono",
            "Consolas",
            "Menlo",
            "Liberation Mono",
            "Courier New",
        ]),
    }
}
