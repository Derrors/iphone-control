//! NSKeyedArchiver archives: decoding into [`Obj`], with the classes remoteAXService uses kept as objects, and
//! encoding the few values the host sends.
//!
//! `ns-keyed-archive` (which idevice uses) returns instances of custom classes unresolved and turns integer
//! dictionary keys into strings, so this is a small reader and writer of its own.

use std::collections::HashMap;
use std::io::Cursor;

use plist::{Dictionary, Uid, Value};

use crate::{AxError, Result};

/// A decoded archive value
#[derive(Debug, Clone, PartialEq)]
pub enum Obj {
    Null,
    Bool(bool),
    /// Wide enough for both signed values and object IDs above `i64::MAX`
    Int(i128),
    Real(f64),
    Str(String),
    Data(Vec<u8>),
    Array(Vec<Obj>),
    /// Keys keep their decoded type (remoteAXService keys attributes by integer)
    Dict(Vec<(Obj, Obj)>),
    /// `NSValue` holding a CGRect: x, y, width, height
    Rect(f64, f64, f64, f64),
    /// `NSValue` holding a CGPoint
    Point(f64, f64),
    /// An instance of any other class, its fields resolved
    Object {
        class: String,
        fields: Vec<(String, Obj)>,
    },
}

impl Obj {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i128> {
        match self {
            Self::Int(i) => Some(*i),
            Self::Bool(b) => Some(i128::from(*b)),
            Self::Real(f) if f.fract() == 0.0 => Some(*f as i128),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            Self::Int(i) => Some(*i != 0),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Obj]> {
        match self {
            Self::Array(a) => Some(a),
            _ => None,
        }
    }

    /// A field of an object, or the value under a string key of a dictionary
    pub fn field(&self, name: &str) -> Option<&Obj> {
        match self {
            Self::Object { fields, .. } => fields.iter().find(|(k, _)| k == name).map(|(_, v)| v),
            Self::Dict(entries) => entries.iter().find(|(k, _)| k.as_str() == Some(name)).map(|(_, v)| v),
            _ => None,
        }
    }

    /// The value under an integer key of a dictionary (attribute numbers)
    pub fn int_key(&self, key: i128) -> Option<&Obj> {
        match self {
            Self::Dict(entries) => entries.iter().find(|(k, _)| k.as_int() == Some(key)).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn class(&self) -> Option<&str> {
        match self {
            Self::Object { class, .. } => Some(class),
            _ => None,
        }
    }
}

/// Deeper than this is garbage: such a value reads as [`Obj::Null`]
const MAX_DEPTH: usize = 200;

/// Values an archive may decode to, per byte of it (plus a floor). Each reference in an archive takes at least a
/// byte, so a real one stays well below this; one whose objects share children over and over (a DAG built to blow
/// up, `2^depth` values from a few hundred bytes) is refused instead of decoded for ever
const BUDGET_PER_BYTE: usize = 8;
const BUDGET_FLOOR: usize = 10_000;

/// Decodes one archive
pub fn decode(bytes: &[u8]) -> Result<Obj> {
    let top = Value::from_reader(Cursor::new(bytes)).map_err(|e| AxError::Archive(e.to_string()))?;
    let dict = top.as_dictionary().ok_or_else(|| AxError::Archive("not a dictionary".into()))?;
    let objects =
        dict.get("$objects").and_then(Value::as_array).ok_or_else(|| AxError::Archive("no $objects".into()))?;
    let root = dict
        .get("$top")
        .and_then(Value::as_dictionary)
        .and_then(|t| t.get("root").or_else(|| t.values().next()))
        .ok_or_else(|| AxError::Archive("no $top root".into()))?;
    let reader = Reader {
        objects,
        visiting: Default::default(),
        budget: std::cell::Cell::new(bytes.len().saturating_mul(BUDGET_PER_BYTE).saturating_add(BUDGET_FLOOR)),
    };
    let obj = reader.resolve(root, 0);
    if reader.budget.get() == 0 {
        return Err(AxError::Archive("too many values for its size".into()));
    }
    Ok(obj)
}

struct Reader<'a> {
    objects: &'a [Value],
    /// The objects being decoded on the way down: one that refers back to one of them is a cycle, read as
    /// [`Obj::Null`]
    visiting: std::cell::RefCell<std::collections::HashSet<u64>>,
    /// Values still to be decoded ([`BUDGET_PER_BYTE`]); at 0 everything reads as [`Obj::Null`] and the archive is
    /// refused
    budget: std::cell::Cell<usize>,
}

impl Reader<'_> {
    fn resolve(&self, v: &Value, depth: usize) -> Obj {
        if depth > MAX_DEPTH || self.budget.get() == 0 {
            return Obj::Null;
        }
        self.budget.set(self.budget.get() - 1);
        match v {
            Value::Uid(u) => match self.objects.get(u.get() as usize) {
                Some(Value::String(s)) if s == "$null" => Obj::Null,
                Some(o) => {
                    if !self.visiting.borrow_mut().insert(u.get()) {
                        return Obj::Null;
                    }
                    let r = self.resolve(o, depth + 1);
                    self.visiting.borrow_mut().remove(&u.get());
                    r
                }
                None => Obj::Null,
            },
            Value::Boolean(b) => Obj::Bool(*b),
            Value::Integer(i) => {
                Obj::Int(i.as_signed().map(i128::from).or_else(|| i.as_unsigned().map(i128::from)).unwrap_or(0))
            }
            Value::Real(f) => Obj::Real(*f),
            Value::String(s) => Obj::Str(s.clone()),
            Value::Data(d) => Obj::Data(d.clone()),
            Value::Array(a) => Obj::Array(a.iter().map(|x| self.resolve(x, depth + 1)).collect()),
            Value::Dictionary(d) => self.instance(d, depth),
            _ => Obj::Null,
        }
    }

    fn class_name(&self, d: &Dictionary) -> String {
        d.get("$class")
            .and_then(|c| match c {
                Value::Uid(u) => self.objects.get(u.get() as usize),
                _ => None,
            })
            .and_then(Value::as_dictionary)
            .and_then(|c| c.get("$classname"))
            .and_then(Value::as_string)
            .unwrap_or("")
            .to_owned()
    }

    fn list(&self, d: &Dictionary, key: &str, depth: usize) -> Vec<Obj> {
        d.get(key)
            .and_then(Value::as_array)
            .map(|a| a.iter().map(|x| self.resolve(x, depth + 1)).collect())
            .unwrap_or_default()
    }

    fn instance(&self, d: &Dictionary, depth: usize) -> Obj {
        let class = self.class_name(d);
        match class.as_str() {
            "NSDictionary" | "NSMutableDictionary" => {
                let keys = self.list(d, "NS.keys", depth);
                let values = self.list(d, "NS.objects", depth);
                Obj::Dict(keys.into_iter().zip(values).collect())
            }
            "NSArray" | "NSMutableArray" | "NSSet" | "NSMutableSet" | "NSOrderedSet" | "NSMutableOrderedSet" => {
                Obj::Array(self.list(d, "NS.objects", depth))
            }
            "NSString" | "NSMutableString" => {
                d.get("NS.string").map(|s| self.resolve(s, depth + 1)).unwrap_or(Obj::Str(String::new()))
            }
            "NSAttributedString" | "NSMutableAttributedString" => {
                d.get("NSString").map(|s| self.resolve(s, depth + 1)).unwrap_or(Obj::Str(String::new()))
            }
            "NSData" | "NSMutableData" => d.get("NS.data").map(|s| self.resolve(s, depth + 1)).unwrap_or(Obj::Null),
            "NSValue" => {
                let text =
                    |key| d.get(key).map(|v| self.resolve(v, depth + 1)).and_then(|o| o.as_str().map(str::to_owned));
                if let Some(n) = text("NS.rectval").map(|s| numbers(&s)).filter(|n| n.len() == 4) {
                    Obj::Rect(n[0], n[1], n[2], n[3])
                } else if let Some(n) = text("NS.pointval").map(|s| numbers(&s)).filter(|n| n.len() == 2) {
                    Obj::Point(n[0], n[1])
                } else {
                    self.object(class, d, depth)
                }
            }
            _ => self.object(class, d, depth),
        }
    }

    fn object(&self, class: String, d: &Dictionary, depth: usize) -> Obj {
        let fields =
            d.iter().filter(|(k, _)| *k != "$class").map(|(k, v)| (k.clone(), self.resolve(v, depth + 1))).collect();
        Obj::Object { class, fields }
    }
}

/// The numbers in NSStringFromRect's `{{x, y}, {w, h}}` or NSStringFromPoint's `{x, y}`
fn numbers(s: &str) -> Vec<f64> {
    s.split(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E')))
        .filter_map(|t| t.parse::<f64>().ok())
        .filter(|n| n.is_finite())
        .collect()
}

/// Builds one archive the way NSKeyedArchiver lays it out
struct Writer {
    objects: Vec<Value>,
    classes: HashMap<String, Uid>,
}

impl Writer {
    fn new() -> Self {
        Self { objects: vec![Value::String("$null".into())], classes: HashMap::new() }
    }

    fn add(&mut self, v: Value) -> Uid {
        self.objects.push(v);
        Uid::new(self.objects.len() as u64 - 1)
    }

    fn class(&mut self, name: &str) -> Uid {
        if let Some(u) = self.classes.get(name) {
            return *u;
        }
        let mut d = Dictionary::new();
        d.insert("$classname".into(), Value::String(name.into()));
        d.insert("$classes".into(), Value::Array(vec![Value::String(name.into()), Value::String("NSObject".into())]));
        let u = self.add(Value::Dictionary(d));
        self.classes.insert(name.into(), u);
        u
    }

    fn finish(self, root: Uid) -> Vec<u8> {
        let mut top = Dictionary::new();
        top.insert("root".into(), Value::Uid(root));
        let mut d = Dictionary::new();
        d.insert("$archiver".into(), Value::String("NSKeyedArchiver".into()));
        d.insert("$version".into(), Value::Integer(100_000u64.into()));
        d.insert("$top".into(), Value::Dictionary(top));
        d.insert("$objects".into(), Value::Array(self.objects));
        let mut out = Vec::new();
        // writing a plist into a Vec cannot fail
        plist::to_writer_binary(&mut out, &Value::Dictionary(d)).expect("plist into memory");
        out
    }
}

/// An archived scalar (a bool, an integer, a string) as the root
pub fn encode_scalar(v: Value) -> Vec<u8> {
    let mut w = Writer::new();
    let root = w.add(v);
    w.finish(root)
}

/// Archived bytes: an NSData
pub fn encode_data(bytes: &[u8]) -> Vec<u8> {
    encode_scalar(Value::Data(bytes.to_vec()))
}

/// An archived NSDictionary of string keys and integer values
pub fn encode_int_dict(entries: &[(&str, i64)]) -> Vec<u8> {
    let mut w = Writer::new();
    let keys: Vec<Value> = entries.iter().map(|(k, _)| Value::Uid(w.add(Value::String((*k).into())))).collect();
    let values: Vec<Value> = entries.iter().map(|(_, v)| Value::Uid(w.add(Value::Integer((*v).into())))).collect();
    let class = w.class("NSDictionary");
    let mut d = Dictionary::new();
    d.insert("NS.keys".into(), Value::Array(keys));
    d.insert("NS.objects".into(), Value::Array(values));
    d.insert("$class".into(), Value::Uid(class));
    let root = w.add(Value::Dictionary(d));
    w.finish(root)
}

/// A request parameter
#[derive(Debug, Clone, PartialEq)]
pub enum Param {
    Int(i64),
    Ints(Vec<i64>),
}

/// An element as the phone names it: its process and an ID unique within it
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectRef {
    pub pid: i64,
    pub id: u64,
    pub is_app: bool,
}

/// An archived `AXPTranslatorRequest`, with the keys its `encodeWithCoder:` writes
pub fn encode_request(
    request_type: i64,
    attribute: i64,
    object: Option<ObjectRef>,
    params: &[(&str, Param)],
    client_type: i64,
) -> Vec<u8> {
    let mut w = Writer::new();
    let root = w.add(Value::Boolean(false)); // placeholder, replaced below
    let mut d = Dictionary::new();
    d.insert("requestType".into(), Value::Integer(request_type.into()));
    d.insert("attributeType".into(), Value::Integer(attribute.into()));
    d.insert("actionType".into(), Value::Integer(0.into()));
    d.insert("clientType".into(), Value::Integer(client_type.into()));
    let translation = match object {
        Some(o) => {
            let id = w.add(Value::Integer(o.id.into()));
            let class = w.class("AXPTranslationObject");
            let mut t = Dictionary::new();
            t.insert("$class".into(), Value::Uid(class));
            t.insert("bridgeDelegateToken".into(), Value::Uid(Uid::new(0)));
            t.insert("didPopuldateAppInfo".into(), Value::Boolean(false));
            t.insert("isApplicationElement".into(), Value::Boolean(o.is_app));
            t.insert("objectID".into(), Value::Uid(id));
            t.insert("pid".into(), Value::Integer(o.pid.into()));
            t.insert("rawElementData".into(), Value::Uid(Uid::new(0)));
            w.add(Value::Dictionary(t))
        }
        None => Uid::new(0),
    };
    d.insert("translation".into(), Value::Uid(translation));
    let parameters = if params.is_empty() {
        Uid::new(0)
    } else {
        let keys: Vec<Value> = params.iter().map(|(k, _)| Value::Uid(w.add(Value::String((*k).into())))).collect();
        let values: Vec<Value> = params
            .iter()
            .map(|(_, p)| {
                Value::Uid(match p {
                    Param::Int(i) => w.add(Value::Integer((*i).into())),
                    Param::Ints(list) => {
                        let items: Vec<Value> =
                            list.iter().map(|i| Value::Uid(w.add(Value::Integer((*i).into())))).collect();
                        let class = w.class("NSArray");
                        let mut a = Dictionary::new();
                        a.insert("NS.objects".into(), Value::Array(items));
                        a.insert("$class".into(), Value::Uid(class));
                        w.add(Value::Dictionary(a))
                    }
                })
            })
            .collect();
        let class = w.class("NSDictionary");
        let mut p = Dictionary::new();
        p.insert("NS.keys".into(), Value::Array(keys));
        p.insert("NS.objects".into(), Value::Array(values));
        p.insert("$class".into(), Value::Uid(class));
        w.add(Value::Dictionary(p))
    };
    d.insert("parameters".into(), Value::Uid(parameters));
    let class = w.class("AXPTranslatorRequest");
    d.insert("$class".into(), Value::Uid(class));
    w.objects[root.get() as usize] = Value::Dictionary(d);
    w.finish(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An archive of `objects` (object 0 is `$null`, the root is object 1), as the phone would send one
    fn archive(objects: Vec<Value>) -> Vec<u8> {
        let mut all = vec![Value::String("$null".into())];
        all.extend(objects);
        let mut top = Dictionary::new();
        top.insert("root".into(), Value::Uid(plist::Uid::new(1)));
        let mut d = Dictionary::new();
        d.insert("$archiver".into(), Value::String("NSKeyedArchiver".into()));
        d.insert("$objects".into(), Value::Array(all));
        d.insert("$top".into(), Value::Dictionary(top));
        let mut out = Vec::new();
        Value::Dictionary(d).to_writer_binary(&mut out).unwrap();
        out
    }

    /// An `NSArray` instance of the given objects, its class at `class`
    fn ns_array(items: &[u64], class: u64) -> Value {
        let mut d = Dictionary::new();
        d.insert("$class".into(), Value::Uid(plist::Uid::new(class)));
        d.insert("NS.objects".into(), Value::Array(items.iter().map(|&u| Value::Uid(plist::Uid::new(u))).collect()));
        Value::Dictionary(d)
    }

    fn class(name: &str) -> Value {
        let mut d = Dictionary::new();
        d.insert("$classname".into(), Value::String(name.into()));
        Value::Dictionary(d)
    }

    /// An object that holds itself is a cycle: it reads as null there, not as a recursion 200 deep
    #[test]
    fn a_cycle_reads_as_null() {
        // 1: an array holding itself and a string; 2: its class; 3: the string
        let bytes = archive(vec![ns_array(&[1, 3], 2), class("NSArray"), Value::String("x".into())]);
        let Obj::Array(items) = decode(&bytes).unwrap() else { panic!("not an array") };
        assert!(matches!(items[0], Obj::Null), "{items:?}");
        assert_eq!(items[1].as_str(), Some("x"));
    }

    /// Objects that each hold the next one twice: 2^60 values from a few hundred bytes. Refused at once
    #[test]
    fn an_archive_built_to_blow_up_is_refused() {
        const LEVELS: u64 = 60;
        let class_at = LEVELS + 2;
        let mut objects: Vec<Value> = (1..=LEVELS).map(|i| ns_array(&[i + 1, i + 1], class_at)).collect();
        objects.push(Value::String("leaf".into()));
        objects.push(class("NSArray"));
        let bytes = archive(objects);
        let t0 = std::time::Instant::now();
        assert!(matches!(decode(&bytes), Err(AxError::Archive(_))));
        assert!(t0.elapsed() < std::time::Duration::from_secs(2), "{:?}", t0.elapsed());
        // shared, but small: decoded
        let mut objects: Vec<Value> = (1..=4).map(|i| ns_array(&[i + 1, i + 1], 6)).collect();
        objects.push(Value::String("leaf".into()));
        objects.push(class("NSArray"));
        let Obj::Array(top) = decode(&archive(objects)).unwrap() else { panic!("not an array") };
        assert_eq!(top.len(), 2);
    }

    #[test]
    fn scalars_round_trip() {
        assert_eq!(decode(&encode_scalar(Value::Boolean(true))).unwrap(), Obj::Bool(true));
        assert_eq!(decode(&encode_scalar(Value::Integer(7u64.into()))).unwrap(), Obj::Int(7));
        assert_eq!(decode(&encode_data(b"abc")).unwrap(), Obj::Data(b"abc".to_vec()));
    }

    #[test]
    fn dictionaries_keep_their_keys() {
        let d = decode(&encode_int_dict(&[("com.apple.private.DTXConnection", 1)])).unwrap();
        assert_eq!(d.field("com.apple.private.DTXConnection"), Some(&Obj::Int(1)));
    }

    #[test]
    fn a_request_decodes_as_the_object_it_describes() {
        let o = ObjectRef { pid: 39, id: u64::MAX - 5, is_app: true };
        let b = encode_request(5, 0, Some(o), &[("attributes", Param::Ints(vec![8, 21])), ("pid", Param::Int(39))], 2);
        let r = decode(&b).unwrap();
        assert_eq!(r.class(), Some("AXPTranslatorRequest"));
        assert_eq!(r.field("requestType"), Some(&Obj::Int(5)));
        let t = r.field("translation").unwrap();
        assert_eq!(t.class(), Some("AXPTranslationObject"));
        assert_eq!(t.field("objectID"), Some(&Obj::Int(i128::from(u64::MAX - 5))));
        assert_eq!(t.field("bridgeDelegateToken"), Some(&Obj::Null));
        let p = r.field("parameters").unwrap();
        assert_eq!(p.field("attributes"), Some(&Obj::Array(vec![Obj::Int(8), Obj::Int(21)])));
        assert_eq!(p.field("pid"), Some(&Obj::Int(39)));
        // no object, no parameters
        let r = decode(&encode_request(4, 0, None, &[], 2)).unwrap();
        assert_eq!(r.field("translation"), Some(&Obj::Null));
        assert_eq!(r.field("parameters"), Some(&Obj::Null));
    }

    #[test]
    fn ns_values_become_rects_and_points() {
        assert_eq!(numbers("{{20, 116.66666666666667}, {64.5, 40}}"), vec![20.0, 116.666_666_666_666_67, 64.5, 40.0]);
        assert_eq!(numbers("{215, -3e2}"), vec![215.0, -300.0]);
    }
}
