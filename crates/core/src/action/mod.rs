//! PDF actions and destinations (ISO 32000-1 §12.6 actions, §12.3.2
//! destinations) — a single model shared by links, the document open-action and
//! outline bookmarks. [`Action::from_json`] parses the SDK's JSON shape; the
//! `build_*` methods emit the corresponding PDF dictionaries/arrays.

use crate::headerfooter::ObjReader;
use crate::object::{Dictionary, Object, StringKind};

/// A view destination (ISO 32000-1 §12.3.2.2, Table 151). `page` is **1-based**;
/// the optional coordinates are in the destination page's user space, with
/// `None` meaning "leave that value unchanged" (PDF `null`).
#[derive(Debug, Clone, PartialEq)]
pub enum Destination {
    /// `/XYZ left top zoom` — position the given point at the upper-left, zoomed.
    Xyz {
        page: u32,
        left: Option<f64>,
        top: Option<f64>,
        zoom: Option<f64>,
    },
    /// `/Fit` — fit the whole page in the window.
    Fit { page: u32 },
    /// `/FitH top` — fit the page width, with `top` at the top edge.
    FitH { page: u32, top: Option<f64> },
    /// `/FitV left` — fit the page height, with `left` at the left edge.
    FitV { page: u32, left: Option<f64> },
    /// `/FitR left bottom right top` — fit the given rectangle.
    FitR { page: u32, rect: [f64; 4] },
    /// `/FitB` — fit the page's bounding box.
    FitB { page: u32 },
    /// `/FitBH top` — fit the bounding box width.
    FitBH { page: u32, top: Option<f64> },
    /// `/FitBV left` — fit the bounding box height.
    FitBV { page: u32, left: Option<f64> },
    /// A **named** destination (resolved through the catalog `/Dests`).
    Named(String),
}

/// A standard named action (ISO 32000-1 §12.6.4.4) — viewer navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamedAction {
    NextPage,
    PrevPage,
    FirstPage,
    LastPage,
}

impl NamedAction {
    fn pdf_name(self) -> &'static [u8] {
        match self {
            NamedAction::NextPage => b"NextPage",
            NamedAction::PrevPage => b"PrevPage",
            NamedAction::FirstPage => b"FirstPage",
            NamedAction::LastPage => b"LastPage",
        }
    }
}

/// A PDF action (ISO 32000-1 §12.6) usable on a link, a widget, an outline
/// bookmark, or the document `/OpenAction`.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Jump to a destination in this document.
    GoTo(Destination),
    /// Jump to a destination in a **remote** file (`/GoToR`). The destination's
    /// page is encoded as a 0-based integer (the remote objects aren't available).
    GoToR { file: String, dest: Destination },
    /// Open a URI (`/URI`).
    Uri(String),
    /// A standard viewer navigation action (`/Named`).
    Named(NamedAction),
    /// Launch an external application/document (`/Launch`).
    Launch(String),
    /// Run JavaScript (`/JavaScript`).
    JavaScript(String),
    /// Submit the form to `url` (`/SubmitForm`).
    SubmitForm { url: String },
    /// Reset the form fields (`/ResetForm`).
    ResetForm,
}

fn name(bytes: &[u8]) -> Object {
    Object::Name(bytes.to_vec())
}

fn lit(s: &str) -> Object {
    Object::String(s.as_bytes().to_vec(), StringKind::Literal)
}

fn opt_num(v: Option<f64>) -> Object {
    match v {
        Some(n) => Object::Real(n),
        None => Object::Null,
    }
}

impl Destination {
    /// The 1-based page this destination targets, or `None` for a named one.
    pub fn page(&self) -> Option<u32> {
        match self {
            Destination::Named(_) => None,
            Destination::Xyz { page, .. }
            | Destination::Fit { page }
            | Destination::FitH { page, .. }
            | Destination::FitV { page, .. }
            | Destination::FitR { page, .. }
            | Destination::FitB { page }
            | Destination::FitBH { page, .. }
            | Destination::FitBV { page, .. } => Some(*page),
        }
    }

    /// Build the `/D` value: a name string for a named destination, else an
    /// explicit array whose first element is `page_obj(page)` (a page reference
    /// for a local destination, or an integer for a remote one).
    pub fn build_d_value(&self, page_obj: &dyn Fn(u32) -> Object) -> Object {
        let arr = |first: u32, mut rest: Vec<Object>| {
            let mut v = vec![page_obj(first)];
            v.append(&mut rest);
            Object::Array(v)
        };
        match self {
            Destination::Named(n) => lit(n),
            Destination::Xyz {
                page,
                left,
                top,
                zoom,
            } => arr(
                *page,
                vec![name(b"XYZ"), opt_num(*left), opt_num(*top), opt_num(*zoom)],
            ),
            Destination::Fit { page } => arr(*page, vec![name(b"Fit")]),
            Destination::FitH { page, top } => arr(*page, vec![name(b"FitH"), opt_num(*top)]),
            Destination::FitV { page, left } => arr(*page, vec![name(b"FitV"), opt_num(*left)]),
            Destination::FitR { page, rect } => arr(
                *page,
                vec![
                    name(b"FitR"),
                    Object::Real(rect[0]),
                    Object::Real(rect[1]),
                    Object::Real(rect[2]),
                    Object::Real(rect[3]),
                ],
            ),
            Destination::FitB { page } => arr(*page, vec![name(b"FitB")]),
            Destination::FitBH { page, top } => arr(*page, vec![name(b"FitBH"), opt_num(*top)]),
            Destination::FitBV { page, left } => arr(*page, vec![name(b"FitBV"), opt_num(*left)]),
        }
    }
}

impl Action {
    /// Build the PDF `/A` action dictionary. `page_obj` maps a 1-based **local**
    /// page number to the object that goes first in a `/D` array (a page
    /// reference); remote (`GoToR`) destinations always use 0-based integers and
    /// ignore `page_obj`.
    pub fn build_dict(&self, page_obj: &dyn Fn(u32) -> Object) -> Dictionary {
        let mut d = Dictionary::new();
        d.set(b"Type".to_vec(), name(b"Action"));
        match self {
            Action::GoTo(dest) => {
                d.set(b"S".to_vec(), name(b"GoTo"));
                d.set(b"D".to_vec(), dest.build_d_value(page_obj));
            }
            Action::GoToR { file, dest } => {
                d.set(b"S".to_vec(), name(b"GoToR"));
                d.set(b"F".to_vec(), lit(file));
                let remote = |p: u32| Object::Integer(p as i64 - 1);
                d.set(b"D".to_vec(), dest.build_d_value(&remote));
            }
            Action::Uri(uri) => {
                d.set(b"S".to_vec(), name(b"URI"));
                d.set(b"URI".to_vec(), lit(uri));
            }
            Action::Named(n) => {
                d.set(b"S".to_vec(), name(b"Named"));
                d.set(b"N".to_vec(), name(n.pdf_name()));
            }
            Action::Launch(file) => {
                d.set(b"S".to_vec(), name(b"Launch"));
                d.set(b"F".to_vec(), lit(file));
            }
            Action::JavaScript(js) => {
                d.set(b"S".to_vec(), name(b"JavaScript"));
                d.set(b"JS".to_vec(), lit(js));
            }
            Action::SubmitForm { url } => {
                d.set(b"S".to_vec(), name(b"SubmitForm"));
                d.set(b"F".to_vec(), lit(url));
            }
            Action::ResetForm => {
                d.set(b"S".to_vec(), name(b"ResetForm"));
            }
        }
        d
    }

    /// Parse an [`Action`] from the SDK's JSON shape, e.g.
    /// `{"type":"goto","dest":{"fit":"xyz","page":3,"top":700,"zoom":1.5}}` or
    /// `{"type":"uri","uri":"https://…"}`. Returns `None` on malformed input.
    pub fn from_json(s: &str) -> Option<Action> {
        let mut p = ObjReader::new(s);
        let mut kind: Option<String> = None;
        let mut uri: Option<String> = None;
        let mut file: Option<String> = None;
        let mut js: Option<String> = None;
        let mut url: Option<String> = None;
        let mut named: Option<String> = None;
        let mut dest: Option<Destination> = None;
        p.object(|p, key| {
            match key {
                "type" => kind = Some(p.string()?),
                "uri" => uri = Some(p.string()?),
                "file" => file = Some(p.string()?),
                "js" | "javascript" => js = Some(p.string()?),
                "url" => url = Some(p.string()?),
                "action" | "named" => named = Some(p.string()?),
                "dest" | "destination" => dest = parse_destination(p),
                _ => p.skip_value()?,
            }
            Some(())
        })?;
        let kind = kind?;
        Some(match kind.as_str() {
            "goto" | "goTo" => Action::GoTo(dest?),
            "gotoR" | "goToR" | "gotor" => Action::GoToR {
                file: file?,
                dest: dest?,
            },
            "uri" | "url" => Action::Uri(uri.or(url)?),
            "named" => Action::Named(match named?.as_str() {
                "prevPage" | "prevpage" => NamedAction::PrevPage,
                "firstPage" | "firstpage" => NamedAction::FirstPage,
                "lastPage" | "lastpage" => NamedAction::LastPage,
                _ => NamedAction::NextPage,
            }),
            "launch" => Action::Launch(file?),
            "javascript" | "js" => Action::JavaScript(js?),
            "submitForm" | "submitform" => Action::SubmitForm { url: url? },
            "resetForm" | "resetform" => Action::ResetForm,
            _ => return None,
        })
    }
}

/// Parse a destination object `{ "fit": "...", "page": n, … }`.
fn parse_destination(p: &mut ObjReader) -> Option<Destination> {
    let mut fit: Option<String> = None;
    let mut page: Option<u32> = None;
    let mut left: Option<f64> = None;
    let mut top: Option<f64> = None;
    let mut zoom: Option<f64> = None;
    let mut name_dest: Option<String> = None;
    let mut rect: Option<[f64; 4]> = None;
    p.object(|p, key| {
        match key {
            "fit" => fit = Some(p.string()?),
            "page" => page = Some(p.number()? as u32),
            "left" => left = Some(p.number()?),
            "top" => top = Some(p.number()?),
            "zoom" => zoom = Some(p.number()?),
            "name" => name_dest = Some(p.string()?),
            "rect" => {
                let n = p.number_array()?;
                if n.len() == 4 {
                    rect = Some([n[0], n[1], n[2], n[3]]);
                }
            }
            _ => p.skip_value()?,
        }
        Some(())
    })?;
    let fit = fit.unwrap_or_else(|| "fit".to_string());
    if fit == "named" {
        return Some(Destination::Named(name_dest?));
    }
    let page = page?;
    Some(match fit.as_str() {
        "xyz" => Destination::Xyz {
            page,
            left,
            top,
            zoom,
        },
        "fitH" | "fith" => Destination::FitH { page, top },
        "fitV" | "fitv" => Destination::FitV { page, left },
        "fitR" | "fitr" => Destination::FitR { page, rect: rect? },
        "fitB" | "fitb" => Destination::FitB { page },
        "fitBH" | "fitbh" => Destination::FitBH { page, top },
        "fitBV" | "fitbv" => Destination::FitBV { page, left },
        _ => Destination::Fit { page },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ref_resolver(p: u32) -> Object {
        Object::Integer(1000 + p as i64) // a stand-in "page reference"
    }

    #[test]
    fn parses_and_builds_goto_xyz() {
        let a = Action::from_json(
            r#"{"type":"goto","dest":{"fit":"xyz","page":3,"top":700,"zoom":1.5}}"#,
        )
        .unwrap();
        assert_eq!(
            a,
            Action::GoTo(Destination::Xyz {
                page: 3,
                left: None,
                top: Some(700.0),
                zoom: Some(1.5),
            })
        );
        let d = a.build_dict(&ref_resolver);
        assert_eq!(
            d.get(b"S").and_then(Object::as_name),
            Some(b"GoTo".as_slice())
        );
        let arr = d.get(b"D").and_then(Object::as_array).unwrap();
        assert_eq!(arr[0], Object::Integer(1003)); // resolver(3)
        assert_eq!(arr[1], Object::Name(b"XYZ".to_vec()));
        assert_eq!(arr[2], Object::Null); // left unset
        assert_eq!(arr[3], Object::Real(700.0));
        assert_eq!(arr[4], Object::Real(1.5));
    }

    #[test]
    fn parses_uri_named_js_resetform() {
        assert_eq!(
            Action::from_json(r#"{"type":"uri","uri":"https://x.test"}"#).unwrap(),
            Action::Uri("https://x.test".into())
        );
        assert_eq!(
            Action::from_json(r#"{"type":"named","action":"lastPage"}"#).unwrap(),
            Action::Named(NamedAction::LastPage)
        );
        assert_eq!(
            Action::from_json(r#"{"type":"javascript","js":"app.alert(1)"}"#).unwrap(),
            Action::JavaScript("app.alert(1)".into())
        );
        assert_eq!(
            Action::from_json(r#"{"type":"resetForm"}"#).unwrap(),
            Action::ResetForm
        );
    }

    #[test]
    fn gotor_uses_zero_based_integer_pages_and_file() {
        let a = Action::from_json(
            r#"{"type":"gotoR","file":"other.pdf","dest":{"fit":"fit","page":2}}"#,
        )
        .unwrap();
        // The resolver passed in is for local pages; GoToR must ignore it and use
        // a 0-based integer (page 2 → 1).
        let d = a.build_dict(&ref_resolver);
        assert_eq!(
            d.get(b"S").and_then(Object::as_name),
            Some(b"GoToR".as_slice())
        );
        let arr = d.get(b"D").and_then(Object::as_array).unwrap();
        assert_eq!(arr[0], Object::Integer(1));
        assert_eq!(arr[1], Object::Name(b"Fit".to_vec()));
    }

    #[test]
    fn named_destination_builds_a_string() {
        let a = Action::GoTo(Destination::Named("intro".into()));
        let d = a.build_dict(&ref_resolver);
        match d.get(b"D") {
            Some(Object::String(bytes, _)) => assert_eq!(bytes, b"intro"),
            other => panic!("expected /D string, got {other:?}"),
        }
    }

    #[test]
    fn from_json_rejects_garbage() {
        assert!(Action::from_json("not json").is_none());
        assert!(Action::from_json(r#"{"type":"goto"}"#).is_none()); // missing dest
    }
}
