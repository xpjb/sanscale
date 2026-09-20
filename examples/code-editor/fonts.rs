//! Resolve real faces once, sharing one fontdb and sanscale's global font pool.
use sanscale::{FontChainHandle, FontHandle, TextService};

#[derive(Clone, Copy)]
pub struct CodeFonts {
    pub normal: FontChainHandle,
    pub bold: FontChainHandle,
    pub italic: FontChainHandle,
}

pub fn load(text: &mut TextService, families: &[&str], requested: Option<&str>) -> CodeFonts {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    // Prefer a family with actual bold and italic/oblique faces. An explicit
    // family is honored even when variants are unavailable (with a warning).
    let primary = requested
        .and_then(|name| query(&db, name, 0).map(|id| (name, id)))
        .or_else(|| {
            families.iter().find_map(|&name| {
                let normal = query(&db, name, 0)?;
                query(&db, name, 1)?;
                query(&db, name, 2)?;
                Some((name, normal))
            })
        })
        .or_else(|| {
            families
                .iter()
                .find_map(|&name| query(&db, name, 0).map(|id| (name, id)))
        })
        .expect("install a monospace font (e.g. DejaVu Sans Mono) for the editor");
    if requested.is_some() && requested != Some(primary.0) {
        eprintln!("requested font not found; using {}", primary.0);
    }
    let bold = query(&db, primary.0, 1);
    let italic = query(&db, primary.0, 2);
    let normal = map(text, &mut db, primary.1).expect("load editor's primary font");
    let bold = bold
        .and_then(|id| map(text, &mut db, id))
        .unwrap_or_else(|| {
            eprintln!(
                "{} has no static bold face; bold roles use regular (no synthetic weight)",
                primary.0
            );
            normal
        });
    let italic = italic
        .and_then(|id| map(text, &mut db, id))
        .unwrap_or_else(|| {
            eprintln!(
                "{} has no italic/oblique face; comments use regular (no synthetic slant)",
                primary.0
            );
            normal
        });
    let mut fallback = Vec::new();
    for name in families {
        if let Some(id) = query(&db, name, 0) {
            if let Some(h) = map(text, &mut db, id) {
                if h != normal && !fallback.contains(&h) {
                    fallback.push(h);
                }
            }
        }
    }
    let mut register = |face| {
        let mut chain = vec![face];
        chain.extend(fallback.iter().copied());
        text.register_chain(&chain)
    };
    let regular = register(normal);
    CodeFonts {
        normal: regular,
        bold: if bold == normal {
            regular
        } else {
            register(bold)
        },
        italic: if italic == normal {
            regular
        } else {
            register(italic)
        },
    }
}
fn query(db: &fontdb::Database, name: &str, variant: u8) -> Option<fontdb::ID> {
    let families = [fontdb::Family::Name(name)];
    let id = db.query(&fontdb::Query {
        families: &families,
        weight: if variant == 1 {
            fontdb::Weight::BOLD
        } else {
            fontdb::Weight::NORMAL
        },
        style: if variant == 2 {
            fontdb::Style::Italic
        } else {
            fontdb::Style::Normal
        },
        ..Default::default()
    })?;
    let face = db.face(id)?;
    if variant == 1 && face.weight < fontdb::Weight::SEMIBOLD {
        return None;
    }
    if variant == 2 && face.style == fontdb::Style::Normal {
        return None;
    }
    Some(id)
}
fn map(text: &mut TextService, db: &mut fontdb::Database, id: fontdb::ID) -> Option<FontHandle> {
    // SAFETY: as in the other examples, mapped system font files must not be
    // replaced in place while the application is using them.
    let (data, index) = unsafe { db.make_shared_face_data(id) }?;
    text.map_font(data, index).ok()
}
