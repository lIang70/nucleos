use crate::error::{Error, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The file boundary.
///
/// `read` and `write` list subtree roots; **empty means all**, which is the only
/// place a default can widen, so [`Files::normalized`] refuses the combinations
/// where that default would be an accident rather than a decision.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct Files {
    /// Readable subtrees. Empty means all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read: Vec<String>,
    /// Writable subtrees. Empty means all, unless `read_only` is set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub write: Vec<String>,
    /// Refuse every write. Mutually exclusive with `write`.
    #[serde(default)]
    pub read_only: bool,
}

impl Files {
    /// No reads beyond what the process needs to start, no writes at all.
    #[must_use]
    pub fn read_only() -> Self {
        Self {
            read: Vec::new(),
            write: Vec::new(),
            read_only: true,
        }
    }

    /// Reads everywhere, writes nowhere.
    #[must_use]
    pub fn read_everything() -> Self {
        Self::read_only()
    }

    /// The canonical form, and the three combinations that cannot be enforced.
    ///
    /// Normalisation is lexical — cleaned, sorted, deduplicated by subtree, with a
    /// filesystem root folding the whole list — so two spellings of one boundary
    /// compare equal.
    pub fn normalized(&self) -> Result<Self> {
        let read = normalize_roots(&self.read)?;
        let write = normalize_roots(&self.write)?;
        if self.read_only && !write.is_empty() {
            return Err(Error::invalid(
                "files: read_only and write are mutually exclusive",
            ));
        }
        if !read.is_empty() && write.is_empty() && !self.read_only {
            // The combination an author reaches for by omission and never means:
            // it lets a session that may look at one subtree write anywhere.
            return Err(Error::invalid(
                "files: read is narrowed but write is unrestricted; declare write or read_only",
            ));
        }
        for root in &write {
            if !read.iter().any(|parent| covers(parent, root)) {
                return Err(Error::invalid(format!(
                    "files: {root:?} is writable but not readable; write must be a subset of read"
                )));
            }
        }
        Ok(Self {
            read,
            write,
            read_only: self.read_only,
        })
    }

    /// Whether `next` grants no more than `self` does.
    ///
    /// Three axes, and turning writes off does **not** license widening reads: a
    /// read-only policy that says "read everything" is wider than one that reads
    /// a subtree, whichever way the write axis moved.
    #[must_use]
    pub fn narrows_to(&self, next: &Self) -> bool {
        let reads_no_more = subset_or_all(&self.read, &next.read);
        let writes_no_more = if next.read_only {
            true
        } else {
            !self.read_only && subset_or_all(&self.write, &next.write)
        };
        let stays_read_only = !self.read_only || next.read_only;
        reads_no_more && writes_no_more && stays_read_only
    }
}

fn normalize_roots(roots: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::with_capacity(roots.len());
    for root in roots {
        if !root.starts_with('/') {
            return Err(Error::invalid(format!(
                "a root must be absolute, got {root:?}"
            )));
        }
        let cleaned = clean(root);
        if cleaned == "/" {
            // A filesystem root folds the whole list: everything is covered.
            return Ok(Vec::new());
        }
        if !out.iter().any(|parent| covers(parent, &cleaned)) {
            out.retain(|existing| !covers(&cleaned, existing));
            out.push(cleaned);
        }
    }
    out.sort();
    Ok(out)
}

fn clean(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        "/".to_owned()
    } else {
        format!("/{}", parts.join("/"))
    }
}

/// Whether `parent` covers `child` as a subtree.
fn covers(parent: &str, child: &str) -> bool {
    child == parent
        || (child.starts_with(parent)
            && (parent.ends_with('/') || child.as_bytes().get(parent.len()) == Some(&b'/')))
}

/// Subset semantics where an empty list means "all".
pub(super) fn subset_or_all(allowed: &[String], narrower: &[String]) -> bool {
    if narrower.is_empty() && !allowed.is_empty() {
        // An absent list means everything, so it is not a subset of a list.
        return false;
    }
    narrower
        .iter()
        .all(|item| allowed.is_empty() || allowed.contains(item))
}
