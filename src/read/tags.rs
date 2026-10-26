//! Stateless validators for a returned read view, not for unseen file bytes.
use serde_json::{json, Value};
use std::io::Cursor;
use std::sync::LazyLock;

pub(crate) const AUTO_EMISSION_MIN_TOKENS: u64 = 1000;
// Maximum text envelope is < 128 bytes under the byte/4 estimator. Keep this
// reservation identical for issuance and validation; never budget the tag away.
pub(crate) const TAG_ENVELOPE_TOKENS: u64 = 32;
static WORDS: LazyLock<Vec<&'static str>> =
    LazyLock::new(|| include_str!("tag_words.txt").lines().collect());

#[derive(Debug)]
pub struct ReadResponse {
    pub output: String,
    pub tag: Option<String>,
    pub unchanged: bool,
}
impl ReadResponse {
    pub(crate) fn plain(output: String) -> Self {
        Self {
            output,
            tag: None,
            unchanged: false,
        }
    }
    pub fn text(&self) -> String {
        match &self.tag {
            Some(tag) if !self.unchanged => format!("view-tag: {tag}\n{}", self.output),
            _ => self.output.clone(),
        }
    }
    pub fn metadata(&self) -> Value {
        let mut value = json!({"status":if self.unchanged {"unchanged"} else {"content"}});
        if let Some(tag) = &self.tag {
            value["tag"] = json!(tag);
            value["tag_scope"] = json!("view");
        }
        value
    }
    pub fn json(&self) -> Value {
        let mut value = self.metadata();
        if !self.unchanged {
            value["output"] = json!(self.output);
        }
        value
    }
}

pub(crate) struct TagOptions<'a> {
    pub condition: Option<&'a str>,
    pub auto: bool,
    pub enabled: bool,
}
impl<'a> TagOptions<'a> {
    pub fn parse(args: &'a Value) -> Result<Self, String> {
        let policy = match args.get("tag") {
            None => "never",
            Some(value) => value.as_str().ok_or("tag must be auto, always, or never")?,
        };
        if !matches!(policy, "auto" | "always" | "never") {
            return Err("tag must be auto, always, or never".into());
        }
        let condition = args
            .get("if_tag")
            .map(|v| v.as_str().ok_or("if_tag must be a string"))
            .transpose()?;
        if let Some(tag) = condition {
            if args.get("tag").is_some() && policy == "never" {
                return Err("if_tag cannot be combined with tag=never".into());
            }
            validate(tag)?;
        }
        Ok(Self {
            condition,
            auto: policy == "auto" && condition.is_none(),
            enabled: policy != "never" || condition.is_some(),
        })
    }
}

fn validate(tag: &str) -> Result<(), String> {
    let invalid = || "invalid if_tag: expected a versioned five-word read tag".to_owned();
    if tag.len() > 256 || !tag.is_ascii() {
        return Err(invalid());
    }
    let (version, body) = tag.split_once(": ").ok_or_else(invalid)?;
    if !version
        .strip_prefix('r')
        .is_some_and(|v| !v.is_empty() && v.bytes().all(|c| c.is_ascii_digit()))
        || body.is_empty()
        || body.bytes().any(|b| b.is_ascii_control())
    {
        return Err(invalid());
    }
    // Unknown versions are ordinary misses, permitting protocol upgrades.
    if version != "r1" {
        return Ok(());
    }
    let words: Vec<_> = body.split(' ').collect();
    if words.len() != 5 || words.iter().any(|word| WORDS.binary_search(word).is_err()) {
        return Err(invalid());
    }
    Ok(())
}

pub(crate) fn encode(identity: &Value, view: &str) -> String {
    let identity = serde_json::to_vec(identity).expect("JSON identity serializes");
    let mut bytes = b"tilth/read-view/r1\0".to_vec();
    for field in [identity.as_slice(), view.as_bytes()] {
        bytes.extend_from_slice(&(field.len() as u64).to_le_bytes());
        bytes.extend_from_slice(field);
    }
    // MurmurHash3 x64_128, seed 0; low 65 bits of the crate's u128 result,
    // encoded most-significant 13-bit group first. This is not authentication.
    let hash = murmur3::murmur3_x64_128(&mut Cursor::new(bytes), 0).expect("in-memory hash read");
    let mut tag = String::from("r1:");
    for shift in [52, 39, 26, 13, 0] {
        tag.push(' ');
        tag.push_str(WORDS[((hash >> shift) & 8191) as usize]);
    }
    tag
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vocabulary_and_protocol() {
        assert_eq!(WORDS.len(), 8192);
        assert!(WORDS.windows(2).all(|w| w[0] < w[1]));
        assert!(WORDS
            .iter()
            .all(|w| (3..=12).contains(&w.len()) && w.bytes().all(|b| b.is_ascii_lowercase())));
        let tag = encode(&json!({"path":"/a.rs"}), "hello");
        // Independently generated with Python mmh3 x64_128 and the frozen
        // dictionary; pins domain framing, seed, output halves and bit order.
        assert_eq!(tag, "r1: drv compartment camping cheese changer");
        validate(&tag).unwrap();
        assert_eq!(tag, encode(&json!({"path":"/a.rs"}), "hello"));
        assert_ne!(tag, encode(&json!({"path":"/a.rs"}), "changed"));
        assert_ne!(tag, encode(&json!({"path":"/b.rs"}), "hello"));
        for bad in [
            "",
            "r1: aaa aaa aaa aaa",
            "r1: unknownword aaa aaa aaa aaa",
            "r1: aaa  aaa aaa aaa aaa",
            "r1: aaa aaa aaa aaa aaa\n",
        ] {
            assert!(validate(bad).is_err(), "{bad:?}");
        }
        validate("r2: future-format").unwrap();
    }
}
