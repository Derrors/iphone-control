//! Elements as remoteAXService describes them, and the trees the phone pushes.
//!
//! Attribute numbers are AccessibilityPlatformTranslation's, as iOS 27 answers them (not public; found in our own
//! rounds of experiments, `docs/dev/ios27-native.md`).

use std::collections::{HashMap, HashSet};

use crate::archive::{Obj, ObjectRef};

pub mod attr {
    pub const ACTIVATION_POINT: i128 = 1;
    pub const ATTRIBUTED_LABEL: i128 = 3;
    pub const CHILDREN: i128 = 8;
    /// Children in VoiceOver's order
    pub const NAVIGATION_ORDER: i128 = 9;
    pub const FRAME: i128 = 21;
    pub const IDENTIFIER: i128 = 25;
    pub const IS_ELEMENT: i128 = 26;
    pub const ENABLED: i128 = 27;
    pub const FOCUSED: i128 = 28;
    pub const SELECTED: i128 = 30;
    pub const VISIBLE: i128 = 32;
    pub const LABEL: i128 = 33;
    pub const PLACEHOLDER: i128 = 42;
    pub const ROLE: i128 = 45;
    pub const VALUE: i128 = 53;
    /// XCUIElementType
    pub const AUTOMATION_TYPE: i128 = 61;
    pub const IS_REMOTE: i128 = 66;
    pub const REMOTE_PID: i128 = 69;
    pub const TRAITS: i128 = 77;

    /// What a walk asks of each element
    pub const WALK: [i64; 16] = [
        CHILDREN as i64,
        NAVIGATION_ORDER as i64,
        ACTIVATION_POINT as i64,
        FRAME as i64,
        IDENTIFIER as i64,
        IS_ELEMENT as i64,
        ENABLED as i64,
        FOCUSED as i64,
        SELECTED as i64,
        VISIBLE as i64,
        LABEL as i64,
        PLACEHOLDER as i64,
        VALUE as i64,
        AUTOMATION_TYPE as i64,
        IS_REMOTE as i64,
        TRAITS as i64,
    ];
}

/// UIAccessibilityTraits bits, private ones included (observed on iOS 27)
pub mod trait_bit {
    pub const BUTTON: u64 = 1;
    pub const LINK: u64 = 1 << 1;
    pub const IMAGE: u64 = 1 << 2;
    pub const KEYBOARD_KEY: u64 = 1 << 5;
    pub const STATIC_TEXT: u64 = 1 << 6;
    pub const SEARCH_FIELD: u64 = 1 << 10;
    pub const ADJUSTABLE: u64 = 1 << 12;
    pub const HEADER: u64 = 1 << 16;
    pub const TEXT_FIELD: u64 = 1 << 18;
    /// SpringBoard's status bar items
    pub const STATUS_BAR: u64 = 1 << 23;
    /// A view showing another process's elements, which come right after it with frames relative to it
    pub const REMOTE_HOST: u64 = 1 << 34;
    pub const SWITCH: u64 = 1 << 53;
}

/// One element
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Element {
    pub object: Option<ObjectRef>,
    /// Index of the parent in the same list; None for the first element (the application)
    pub parent: Option<usize>,
    pub depth: usize,
    pub label: String,
    pub value: String,
    pub placeholder: String,
    pub identifier: String,
    /// x, y, width, height in screen points (for another process's elements: relative to their host, see
    /// [`crate::node`])
    pub frame: Option<(f64, f64, f64, f64)>,
    pub activation: Option<(f64, f64)>,
    /// XCUIElementType; 0 in the first tree pushed after a change
    pub xc_type: i64,
    pub traits: u64,
    pub is_element: bool,
    pub enabled: bool,
    pub selected: bool,
    pub focused: bool,
    pub visible: bool,
    pub remote: bool,
}

impl Element {
    pub fn pid(&self) -> i64 {
        self.object.map_or(0, |o| o.pid)
    }
}

pub(crate) fn object_ref(o: &Obj) -> Option<ObjectRef> {
    if o.class() != Some("AXPTranslationObject") {
        return None;
    }
    Some(ObjectRef {
        pid: o.field("pid").and_then(Obj::as_int).unwrap_or(0) as i64,
        id: o.field("objectID").and_then(Obj::as_int).unwrap_or(0) as u64,
        is_app: o.field("isApplicationElement").and_then(Obj::as_bool).unwrap_or(false),
    })
}

fn text(o: Option<&Obj>) -> String {
    match o {
        Some(Obj::Str(s)) => s.clone(),
        Some(Obj::Int(i)) => i.to_string(),
        Some(Obj::Real(f)) if f.is_finite() => {
            if f.fract() == 0.0 {
                format!("{f:.0}")
            } else {
                f.to_string()
            }
        }
        Some(Obj::Bool(b)) => u8::from(*b).to_string(),
        _ => String::new(),
    }
}

/// An element from its attributes (a dictionary keyed by attribute number), and its children
pub fn element(
    object: Option<ObjectRef>,
    attrs: &Obj,
    parent: Option<usize>,
    depth: usize,
) -> (Element, Vec<ObjectRef>) {
    let a = |k| attrs.int_key(k);
    let flag = |k, default| a(k).and_then(Obj::as_bool).unwrap_or(default);
    let mut label = text(a(attr::LABEL));
    if label.is_empty() {
        label = text(a(attr::ATTRIBUTED_LABEL));
    }
    let mut value = text(a(attr::VALUE));
    let mut identifier = text(a(attr::IDENTIFIER));
    // Some apps carry the value in the identifier (Calculator's display: "StandardInputView;value:144")
    if let Some((name, v)) = identifier.split_once(";value:")
        && !name.is_empty()
    {
        if value.is_empty() {
            value = v.to_owned();
        }
        identifier = name.to_owned();
    }
    let e = Element {
        object,
        parent,
        depth,
        label,
        value,
        placeholder: text(a(attr::PLACEHOLDER)),
        identifier,
        frame: match a(attr::FRAME) {
            Some(Obj::Rect(x, y, w, h)) => Some((*x, *y, *w, *h)),
            _ => None,
        },
        activation: match a(attr::ACTIVATION_POINT) {
            Some(Obj::Point(x, y)) => Some((*x, *y)),
            _ => None,
        },
        xc_type: a(attr::AUTOMATION_TYPE).and_then(Obj::as_int).unwrap_or(0) as i64,
        traits: a(attr::TRAITS).and_then(Obj::as_int).unwrap_or(0) as u64,
        is_element: flag(attr::IS_ELEMENT, false),
        enabled: flag(attr::ENABLED, true),
        selected: flag(attr::SELECTED, false),
        focused: flag(attr::FOCUSED, false),
        visible: flag(attr::VISIBLE, true),
        remote: flag(attr::IS_REMOTE, false),
    };
    let list = a(attr::NAVIGATION_ORDER).and_then(Obj::as_array).or_else(|| a(attr::CHILDREN).and_then(Obj::as_array));
    let kids = list.map(|l| l.iter().filter_map(object_ref).collect()).unwrap_or_default();
    (e, kids)
}

/// Deeper than this the walk stops (a malformed tree, or a cycle)
const MAX_DEPTH: usize = 60;

/// Elements in reading order from `root`, depth first, each with the attributes `attrs` holds for it. Elements
/// without attributes are left out with their descendants
pub fn assemble(root: ObjectRef, attrs: &HashMap<ObjectRef, Obj>) -> Vec<Element> {
    // an element is its process and ID: a reference to it may carry another `is_app` than the one its attributes came
    // under (looked up as it was, its whole subtree was left out without a word); it keeps the attributes' own
    let by_id: HashMap<(i64, u64), (&ObjectRef, &Obj)> = attrs.iter().map(|(k, v)| ((k.pid, k.id), (k, v))).collect();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = vec![(root, None, 0usize)];
    while let Some((o, parent, depth)) = stack.pop() {
        // the same element can be listed under two parents
        let key = (o.pid, o.id);
        let Some(&(o, a)) = by_id.get(&key) else { continue };
        if depth > MAX_DEPTH || !seen.insert(key) {
            continue;
        }
        let (e, kids) = element(Some(*o), a, parent, depth);
        out.push(e);
        let idx = out.len() - 1;
        for k in kids.into_iter().rev() {
            stack.push((k, Some(idx), depth + 1));
        }
    }
    out
}

/// A tree the phone pushed after the screen changed
#[derive(Debug, Clone)]
pub struct Pushed {
    /// The frontmost application's element first
    pub elements: Vec<Element>,
    /// The first tree after a change: it lacks some attributes (the element types); a second, complete one
    /// follows within about half a second
    pub initial: bool,
}

impl Pushed {
    /// The application's process
    pub fn app_pid(&self) -> i64 {
        self.elements.first().map_or(0, Element::pid)
    }
}

/// The tree in a `treeDump` result (the list of responses the phone pushes): the frontmost app's element
/// (request type 4) and every element's attributes (type 5)
pub fn from_dump(list: &[Obj], kind: &str) -> Option<Pushed> {
    let mut app = None;
    let mut attrs = HashMap::new();
    for r in list {
        let request_type = r.field("associatedRequestType").and_then(Obj::as_int);
        let Some(result) = r.field("resultData") else { continue };
        match request_type {
            Some(4) => app = object_ref(result),
            Some(5) => {
                if let Some(o) = r.field("associatedTranslationObject").and_then(object_ref) {
                    attrs.insert(o, result.clone());
                }
            }
            _ => {}
        }
    }
    let elements = assemble(app?, &attrs);
    (!elements.is_empty()).then_some(Pushed { elements, initial: kind == "AXPTreeDumpTypeInitialDump" })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(pid: i64, id: u64) -> Obj {
        Obj::Object {
            class: "AXPTranslationObject".into(),
            fields: vec![("pid".into(), Obj::Int(pid.into())), ("objectID".into(), Obj::Int(id.into()))],
        }
    }

    pub(crate) fn attrs(entries: Vec<(i128, Obj)>) -> Obj {
        Obj::Dict(entries.into_iter().map(|(k, v)| (Obj::Int(k), v)).collect())
    }

    fn response(kind: i128, object: Option<Obj>, result: Obj) -> Obj {
        let mut fields = vec![("associatedRequestType".to_owned(), Obj::Int(kind)), ("resultData".to_owned(), result)];
        if let Some(o) = object {
            fields.push(("associatedTranslationObject".into(), o));
        }
        Obj::Object { class: "AXPTranslatorResponse".into(), fields }
    }

    #[test]
    fn a_dump_is_assembled_in_reading_order() {
        let list = vec![
            response(4, None, obj(10, 1)),
            response(
                5,
                Some(obj(10, 1)),
                attrs(vec![
                    (attr::LABEL, Obj::Str("设置".into())),
                    (attr::CHILDREN, Obj::Array(vec![obj(10, 2), obj(10, 3)])),
                    (attr::FRAME, Obj::Rect(0.0, 0.0, 430.0, 932.0)),
                ]),
            ),
            response(
                5,
                Some(obj(10, 3)),
                attrs(vec![
                    (attr::LABEL, Obj::Str("通用".into())),
                    (attr::AUTOMATION_TYPE, Obj::Int(9)),
                    (attr::TRAITS, Obj::Int(1)),
                    (attr::IDENTIFIER, Obj::Str("com.apple.settings.general".into())),
                ]),
            ),
            response(
                5,
                Some(obj(10, 2)),
                attrs(vec![(attr::LABEL, Obj::Str("飞行模式".into())), (attr::VALUE, Obj::Int(0))]),
            ),
        ];
        let p = from_dump(&list, "AXPTreeDumpTypeAdditionalData").unwrap();
        assert!(!p.initial);
        assert_eq!(p.app_pid(), 10);
        let labels: Vec<_> = p.elements.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(labels, ["设置", "飞行模式", "通用"]);
        assert_eq!(p.elements[1].parent, Some(0));
        assert_eq!(p.elements[1].value, "0");
        assert_eq!(p.elements[2].xc_type, 9);
        assert_eq!(p.elements[2].identifier, "com.apple.settings.general");
    }

    /// A child referenced with another `is_app` than its attributes were stored under is still found, with its
    /// subtree, and keeps the attributes' own reference
    #[test]
    fn a_child_is_found_whatever_its_reference_says_about_being_an_app() {
        let r = |id, is_app| ObjectRef { pid: 10, id, is_app };
        let mut stored = HashMap::new();
        stored.insert(r(1, true), attrs(vec![(attr::LABEL, Obj::Str("app".into()))]));
        stored.insert(r(2, true), attrs(vec![(attr::LABEL, Obj::Str("listed as an app".into()))]));
        stored.insert(r(3, false), attrs(vec![(attr::LABEL, Obj::Str("its child".into()))]));
        // the children as the parents name them: 2 without the app flag
        let mut with_kids = |of: ObjectRef, kids: Vec<Obj>| {
            if let Some(Obj::Dict(d)) = stored.get_mut(&of) {
                d.push((Obj::Int(attr::CHILDREN), Obj::Array(kids)));
            }
        };
        with_kids(r(1, true), vec![obj(10, 2)]);
        with_kids(r(2, true), vec![obj(10, 3)]);
        let els = assemble(r(1, true), &stored);
        let labels: Vec<_> = els.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(labels, ["app", "listed as an app", "its child"]);
        assert_eq!(els[1].object, Some(r(2, true)), "the stored reference");
    }

    #[test]
    fn a_value_kept_in_the_identifier_is_split_off() {
        let (e, _) =
            element(None, &attrs(vec![(attr::IDENTIFIER, Obj::Str("StandardInputView;value:144".into()))]), None, 0);
        assert_eq!((e.identifier.as_str(), e.value.as_str()), ("StandardInputView", "144"));
    }
}
