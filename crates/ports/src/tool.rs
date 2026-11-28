//! The tool contract, and the authority one call runs under.
//!
//! A tool sees exactly what its [`Binding`] carries: a session id, a scratch
//! drawer partitioned for it, and the capability slots it **declared**. A slot it
//! did not declare reads as absent, which is how "a tool cannot be given authority
//! it did not ask for" is enforced without copying or nil-ing fields.

use std::sync::Arc;

use crate::sandbox::SessionStarter;
use async_trait::async_trait;
use nucleos_contract::{
    Content, Result, SessionKey, ToolCall, ToolDefinition, ToolName, ToolResult,
};

/// Advisory, execution-relevant metadata.
///
/// Every field is a claim, and zero means "no claim". `mutates_state: false` is
/// the conservative direction in the opposite sense: a caller that does not know
/// better should assume a tool **may** mutate state, so a retry is unsafe.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ToolMeta {
    /// Calls per minute this tool can sustain. Absent means no claim.
    ///
    /// An integer, not an `f32`: a claim that compares, serialises and crosses a
    /// language boundary is not a place for floating point.
    pub rate_limit_per_minute: Option<u32>,
    /// Whether it changes state beyond returning a result.
    pub mutates_state: bool,
    /// Whether it already bounds its own execution time, so a supervisor should
    /// not impose its default deadline on top.
    pub self_timeout: bool,
}

/// The session's file boundary: the roots a tool may read and write through.
///
/// A capability, not a path check: a tool that used `std::fs` instead would bypass
/// the boundary entirely, which is why the handle is the only way in.
pub trait FileTree: Send + Sync {
    /// Read one file, as bytes.
    fn read(&self, path: &str) -> Result<Vec<u8>>;

    /// Write one file.
    fn write(&self, path: &str, bytes: &[u8]) -> Result<()>;

    /// The readable roots, and the writable ones.
    fn roots(&self) -> (Vec<String>, Vec<String>);
}

/// The session's skill catalogue.
///
/// Two questions and no third: **what is there**, and **what matches**. Descriptions are one line
/// each, which is what lets a prompt carry the catalogue without carrying every body.
///
/// There is deliberately no `load`. A body is bytes, and the only way a tool reads bytes is
/// [`FileTree`] — inside the session's file boundary, with the same path checks and the same errors.
/// A `load` of its own would be a second door to the same files, and the one thing worse than a
/// boundary is a boundary with a side entrance.
pub trait SkillIndex: Send + Sync {
    /// Every mounted skill, in the order it should be presented.
    fn list(&self) -> Vec<nucleos_contract::Skill>;

    /// The skills matching a query.
    ///
    /// Defaulted to a case-insensitive match on the name and the description, because a deployment
    /// with five skills has no reason to write a ranker. An index that can do better overrides it.
    fn search(&self, query: &str) -> Vec<nucleos_contract::Skill> {
        let query = query.to_lowercase();
        self.list()
            .into_iter()
            .filter(|skill| {
                skill.name.to_lowercase().contains(&query)
                    || skill.description.to_lowercase().contains(&query)
            })
            .collect()
    }
}

/// One capability a host fills for a session.
///
/// An enum rather than a string-keyed bag of `Any`, and that is the whole point:
/// the identity of a capability is a **variant**, so a tool asks for the thing it
/// needs and a deployment cannot hand it a mistyped one. Adding a capability is a
/// new variant, which every `match` must then answer — the same discipline the fact
/// vocabulary gets.
#[derive(Clone)]
pub enum Capability {
    /// The file boundary.
    FileTree(Arc<dyn FileTree>),
    /// The ability to start a process.
    Process(Arc<dyn SessionStarter>),
    /// The skill catalogue.
    Skills(Arc<dyn SkillIndex>),
    /// Dispatch under the caller's binding.
    Nested(Arc<dyn Nested>),
    /// Models this session may call.
    Models(Arc<dyn Models>),
}

/// Which capability, without its value.
///
/// A `Copy` discriminant, so a tool's declaration list is a `&'static [Kind]` — no
/// allocation, and no chance of a name that no variant matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// [`Capability::FileTree`].
    FileTree,
    /// [`Capability::Process`].
    Process,
    /// [`Capability::Skills`].
    Skills,
    /// [`Capability::Nested`].
    Nested,
    /// [`Capability::Models`].
    Models,
}

impl Capability {
    /// Which capability this is.
    #[must_use]
    pub fn kind(&self) -> Kind {
        match self {
            Self::FileTree(_) => Kind::FileTree,
            Self::Process(_) => Kind::Process,
            Self::Skills(_) => Kind::Skills,
            Self::Nested(_) => Kind::Nested,
            Self::Models(_) => Kind::Models,
        }
    }
}

impl std::fmt::Debug for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Capability::{:?}", self.kind())
    }
}

impl Kind {
    /// Its name, for an error a human reads.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FileTree => "file tree",
            Self::Process => "process",
            Self::Skills => "skills",
            Self::Nested => "nested dispatch",
            Self::Models => "models",
        }
    }
}

/// The capabilities a host assembled for one session.
///
/// A builder: the host fills it, hands out a [`CapView`], and stops touching it.
#[derive(Clone, Default)]
pub struct CapSet {
    slots: Vec<Capability>,
}

impl CapSet {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fill one capability. Only the layer that assembled the session may call this.
    pub fn provide(&mut self, capability: Capability) -> &mut Self {
        self.slots.retain(|slot| slot.kind() != capability.kind());
        self.slots.push(capability);
        self
    }

    /// Whether the set carries a kind.
    #[must_use]
    pub fn has(&self, kind: Kind) -> bool {
        self.slots.iter().any(|slot| slot.kind() == kind)
    }

    /// The kinds a tool declared that this set cannot satisfy, sorted so a failed
    /// assembly reports the same list every run.
    ///
    /// It is the construction-time gate: a host validates a tool's needs against the
    /// set it built, so a process that cannot serve a tool says so once, at
    /// assembly.
    #[must_use]
    pub fn missing(&self, needs: &[Kind]) -> Vec<Kind> {
        let mut missing: Vec<Kind> = needs
            .iter()
            .copied()
            .filter(|kind| !self.has(*kind))
            .collect();
        missing.sort_unstable();
        missing.dedup();
        missing
    }

    /// A view narrowed to nothing: the caller must say what it may read.
    #[must_use]
    pub fn view(&self) -> CapView<'_> {
        CapView {
            slots: &self.slots,
            allow: None,
        }
    }
}

/// Which kinds a view may hand out.
#[derive(Debug, Clone, Default)]
pub struct Allow(Vec<Kind>);

impl Allow {
    /// Allow exactly these.
    #[must_use]
    pub fn of(needs: &[Kind]) -> Self {
        Self(needs.to_vec())
    }

    /// Whether a kind is allowed.
    #[must_use]
    pub fn permits(&self, kind: Kind) -> bool {
        self.0.contains(&kind)
    }
}

/// A borrowed, narrowed read view over a [`CapSet`].
///
/// Named accessors rather than a generic `get`: the compiler knows which capability
/// each one hands back, so reading the wrong type is not a `None` at runtime.
#[derive(Clone, Copy)]
pub struct CapView<'a> {
    slots: &'a [Capability],
    allow: Option<&'a Allow>,
}

static NO_SLOTS: &[Capability] = &[];

impl<'a> CapView<'a> {
    /// A view that carries nothing.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            slots: NO_SLOTS,
            allow: None,
        }
    }

    fn find(&self, kind: Kind) -> Option<&'a Capability> {
        if let Some(allow) = self.allow
            && !allow.permits(kind)
        {
            return None;
        }
        self.slots.iter().find(|slot| slot.kind() == kind)
    }

    /// The file boundary, when the tool declared it.
    #[must_use]
    pub fn file_tree(&self) -> Option<&'a Arc<dyn FileTree>> {
        match self.find(Kind::FileTree) {
            Some(Capability::FileTree(tree)) => Some(tree),
            _ => None,
        }
    }

    /// The process starter, when the tool declared it.
    ///
    /// `Err` rather than `None` because a tool that declared this and did not get it
    /// is a deployment gap, not a caller mistake.
    pub fn process(&self) -> Result<&'a Arc<dyn SessionStarter>> {
        match self.find(Kind::Process) {
            Some(Capability::Process(starter)) => Ok(starter),
            _ => Err(self.absent(Kind::Process)),
        }
    }

    /// The skill catalogue, when the tool declared it.
    #[must_use]
    pub fn skills(&self) -> Option<&'a Arc<dyn SkillIndex>> {
        match self.find(Kind::Skills) {
            Some(Capability::Skills(index)) => Some(index),
            _ => None,
        }
    }

    /// The models this session may call, when the tool declared them.
    ///
    /// `Err` for the same reason as [`Self::process`]: a tool that declared this and did not get it
    /// is a deployment gap, and a silent `None` would turn it into a mysterious empty answer.
    pub fn models(&self) -> Result<&'a Arc<dyn Models>> {
        match self.find(Kind::Models) {
            Some(Capability::Models(models)) => Ok(models),
            _ => Err(self.absent(Kind::Models)),
        }
    }

    /// Nested dispatch, when the tool declared it.
    pub fn nested(&self) -> Result<&'a Arc<dyn Nested>> {
        match self.find(Kind::Nested) {
            Some(Capability::Nested(nested)) => Ok(nested),
            _ => Err(self.absent(Kind::Nested)),
        }
    }

    fn absent(&self, kind: Kind) -> nucleos_contract::Error {
        nucleos_contract::Error::new(
            nucleos_contract::ErrorKind::Unsupported,
            format!(
                "this deployment does not provide the {kind:?} capability",
                kind = kind.as_str()
            ),
        )
    }

    /// A view narrowed to exactly these kinds.
    #[must_use]
    pub fn narrowed(&self, allow: &'a Allow) -> Self {
        Self {
            slots: self.slots,
            allow: Some(allow),
        }
    }

    /// How many capabilities are reachable.
    #[must_use]
    pub fn len(&self) -> usize {
        match self.allow {
            None => self.slots.len(),
            Some(allow) => self
                .slots
                .iter()
                .filter(|slot| allow.permits(slot.kind()))
                .count(),
        }
    }

    /// Whether nothing is reachable.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The models a session may call.
///
/// This is the runtime's handle, **not** a vendor's [`crate::model::Generator`], and the difference
/// is what the handle is for:
///
/// 1. the model a tool names is checked against the session's authority before anything is
///    dispatched, so a tool cannot reach a model the session never approved;
/// 2. the call is recorded when it returns, before the tool does — so a tool that generates an image
///    and then panics still leaves the spend in the log;
/// 3. the handle belongs to one session, so every entry it writes is attributed to that session
///    without the tool ever naming it.
///
/// A tool that generates media is therefore an ordinary tool whose work is an ordinary model call.
/// It reports content; the accounting is not its to report.
#[async_trait]
pub trait Models: Send + Sync {
    /// Generate, within this session's authority.
    ///
    /// # Errors
    ///
    /// The session may not use that model or mode, or the provider failed.
    async fn generate(
        &self,
        request: nucleos_contract::ModelRequest,
    ) -> Result<nucleos_contract::GenerateResponse>;
}

/// The conversation view a tool gets: which session, and its own drawer.
///
/// Two methods, and that is the whole answer to "what may a tool touch in a
/// session": an id and scratch space. No budget, no history, no confine, no
/// settings.
pub trait ToolSession: Send + Sync {
    /// The session's address.
    ///
    /// The application-scoped key, not a bare name: two applications may each own a session called
    /// `draft`, and a tool that received only the name could not tell them apart in a log line.
    fn key(&self) -> &SessionKey;

    /// Session-scoped storage, already partitioned for this tool, so two tools
    /// cannot collide on a key.
    fn scratch(&self) -> &dyn Scratch;
}

/// Per-session storage a tool owns.
pub trait Scratch: Send + Sync {
    /// The value under a key, when there is one.
    fn get(&self, key: &str) -> Option<&str>;

    /// Store a value.
    fn set(&self, key: &str, value: String);
}

/// Everything one call is bound to.
///
/// A parameter rather than a field on the tool, and nothing here is read from
/// ambient state: a tool that held a session could not be shared across sessions,
/// and one that read its authority from the environment could be handed authority
/// its caller never granted.
///
/// The zero value is **not** "unrestricted": [`Binding::valid`] is false and
/// dispatch refuses it. A binding is only ever assembled by a host.
#[derive(Clone, Copy)]
pub struct Binding<'a> {
    session: Option<&'a dyn ToolSession>,
    view: Option<&'a dyn View>,
    caps: Option<CapView<'a>>,
}

/// What may be shown, which a tool reads so a nested call sees the same set.
pub trait View: Send + Sync {
    /// The definitions the model may see this round.
    fn definitions(&self) -> Vec<ToolDefinition>;

    /// Whether a name may be shown or called.
    fn visible(&self, name: &ToolName) -> bool;

    /// Record that the model used a name.
    fn record_call(&self, call: &ToolCall);
}

impl<'a> Binding<'a> {
    /// Assemble the authority for one session. The only way to reach a valid
    /// binding: the one field no call can do without is the session.
    #[must_use]
    pub fn bind(session: &'a dyn ToolSession) -> Self {
        Self {
            session: Some(session),
            view: None,
            caps: None,
        }
    }

    /// Carry the session's tool projection.
    #[must_use]
    pub fn with_view(mut self, view: &'a dyn View) -> Self {
        self.view = Some(view);
        self
    }

    /// Carry the session's capability pocket.
    #[must_use]
    pub fn with_caps(mut self, caps: CapView<'a>) -> Self {
        self.caps = Some(caps);
        self
    }

    /// Whether a host assembled this.
    #[must_use]
    pub fn valid(&self) -> bool {
        self.session.is_some()
    }

    /// The session, when it was bound.
    pub fn session(&self) -> Result<&'a dyn ToolSession> {
        self.session
            .ok_or_else(|| nucleos_contract::Error::internal("a tool was called without a binding"))
    }

    /// The projection, when it was bound.
    pub fn view(&self) -> Option<&'a dyn View> {
        self.view
    }

    /// The capability pocket.
    #[must_use]
    pub fn caps(&self) -> CapView<'a> {
        self.caps.unwrap_or_else(CapView::empty)
    }

    /// The binding restricted to the needs one tool declared.
    ///
    /// Dispatch calls this before every execution, so a tool receives only what
    /// it asked for even though the session holds more.
    #[must_use]
    pub fn narrow(self, allow: &'a Allow) -> Self {
        Self {
            caps: self.caps.map(|caps| caps.narrowed(allow)),
            ..self
        }
    }
}

/// One callable capability.
#[async_trait]
pub trait Tool: Send + Sync {
    /// What the model sees.
    fn definition(&self) -> ToolDefinition;

    /// Which capabilities it will read. Declaring nothing is what makes a tool
    /// testable with no scaffolding at all.
    fn needs(&self) -> &'static [Kind] {
        &[]
    }

    /// Advisory metadata.
    fn meta(&self) -> ToolMeta {
        ToolMeta::default()
    }

    /// Run one call.
    ///
    /// `Err` is the natural authoring style: dispatch turns every failure —
    /// returned, panicked or timed out — into a [`ToolResult`] with `is_error`
    /// set, because the error text is what the model must read to recover.
    async fn execute(&self, binding: Binding<'_>, arguments: &str) -> Result<Content>;
}

/// A deferred tool a source contributes.
#[derive(Clone)]
pub struct LazyTool {
    /// The name it will have once loaded.
    pub name: ToolName,
    /// The definition served until it is loaded.
    pub placeholder: ToolDefinition,
    /// What it will need. Declared **here** rather than read after loading:
    /// registration happens before the first call, so a proxy that looked cheaper
    /// than its tool would be accepted by a process that cannot serve it.
    pub needs: Vec<Kind>,
    /// Load it.
    pub load: Arc<dyn Fn() -> Arc<dyn Tool> + Send + Sync>,
}

/// A contributor of tools.
pub trait Source: Send + Sync {
    /// Tools available immediately, with no I/O.
    fn tools(&self) -> Vec<Arc<dyn Tool>>;

    /// Tools registered as placeholders and loaded on first use.
    fn lazy_tools(&self) -> Vec<LazyTool> {
        Vec::new()
    }
}

/// Dispatch a tool call under the caller's binding.
///
/// A session-bound object behind a slot, rather than a process-wide table (which
/// would expose every definition, including the ones this session cannot see) or
/// a method on [`View`] (which answers "what may be shown" and has no binding).
pub trait Nested: Send + Sync {
    /// Run one call the way the model would see it.
    fn dispatch(&self, call: ToolCall) -> ToolResult;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::sync::Arc;

    struct Tree;

    impl FileTree for Tree {
        fn read(&self, _path: &str) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }
        fn write(&self, _path: &str, _bytes: &[u8]) -> Result<()> {
            Ok(())
        }
        fn roots(&self) -> (Vec<String>, Vec<String>) {
            (vec!["/srv".to_owned()], Vec::new())
        }
    }

    struct Skills;

    impl SkillIndex for Skills {
        fn list(&self) -> Vec<nucleos_contract::Skill> {
            vec![
                nucleos_contract::Skill {
                    name: "review".to_owned(),
                    description: "Check a diff against the house rules".to_owned(),
                    path: "/srv/skills/review/SKILL.md".to_owned(),
                },
                nucleos_contract::Skill {
                    name: "release".to_owned(),
                    description: "Cut a version and write its notes".to_owned(),
                    path: "/srv/skills/release/SKILL.md".to_owned(),
                },
            ]
        }
    }

    #[test]
    fn a_catalogue_is_a_list_and_a_search_with_no_body() {
        let index = Skills;
        assert_eq!(index.list().len(), 2);
        assert_eq!(index.search("RELEASE").len(), 1);
        assert_eq!(index.search("house rules")[0].name, "review");
        assert!(index.search("nothing matches this").is_empty());
    }

    fn set() -> CapSet {
        let mut set = CapSet::new();
        set.provide(Capability::FileTree(Arc::new(Tree)));
        set.provide(Capability::Skills(Arc::new(Skills)));
        set
    }

    #[test]
    fn a_capability_is_found_by_its_variant_not_by_a_name() {
        let set = set();
        let view = set.view();
        assert!(view.file_tree().is_some());
        assert!(view.skills().is_some());
        assert!(view.process().is_err(), "undeclared reads as absent");
    }

    struct SilentModels;

    #[async_trait]
    impl Models for SilentModels {
        async fn generate(
            &self,
            _request: nucleos_contract::ModelRequest,
        ) -> Result<nucleos_contract::GenerateResponse> {
            Err(nucleos_contract::Error::internal("no"))
        }
    }

    #[test]
    fn a_tool_that_did_not_declare_models_cannot_reach_one() {
        let set = set();
        assert!(
            set.view().models().is_err(),
            "an undeclared model handle reads as absent"
        );

        let mut with_models = set.clone();
        with_models.provide(Capability::Models(Arc::new(SilentModels)));
        assert!(with_models.view().models().is_ok());

        let allow = Allow::of(&[Kind::FileTree]);
        assert!(
            with_models.view().narrowed(&allow).models().is_err(),
            "narrowing must hide the handle too"
        );
    }

    #[test]
    fn narrowing_hides_what_a_tool_did_not_declare() {
        let set = set();
        let allow = Allow::of(&[Kind::FileTree]);
        let narrowed = set.view().narrowed(&allow);
        assert!(narrowed.file_tree().is_some());
        assert!(narrowed.skills().is_none());
        assert_eq!(narrowed.len(), 1);
    }

    #[test]
    fn a_missing_capability_is_reported_at_assembly() {
        let set = set();
        assert_eq!(
            set.missing(&[Kind::FileTree, Kind::Process]),
            vec![Kind::Process]
        );
    }

    #[test]
    fn an_absent_capability_names_itself_in_the_error() {
        let set = set();
        let error = match set.view().process() {
            Ok(_) => panic!("an undeclared capability must not be handed out"),
            Err(error) => error,
        };
        assert_eq!(error.kind, nucleos_contract::ErrorKind::Unsupported);
        assert!(error.message.contains("process"), "{error:?}");
    }

    #[test]
    fn providing_the_same_kind_twice_replaces_it() {
        let mut set = CapSet::new();
        set.provide(Capability::FileTree(Arc::new(Tree)));
        set.provide(Capability::FileTree(Arc::new(Tree)));
        assert_eq!(set.view().len(), 1);
    }

    #[test]
    fn the_zero_binding_is_invalid() {
        let binding = Binding {
            session: None,
            view: None,
            caps: None,
        };
        assert!(!binding.valid());
        assert!(binding.session().is_err());
    }

    #[test]
    fn a_binding_without_caps_hands_out_nothing() {
        struct NoSession;
        impl ToolSession for NoSession {
            fn key(&self) -> &SessionKey {
                static KEY: std::sync::OnceLock<SessionKey> = std::sync::OnceLock::new();
                KEY.get_or_init(|| {
                    SessionKey::new(
                        nucleos_contract::AppId::new("writer").unwrap(),
                        nucleos_contract::SessionId::new("s1").unwrap(),
                    )
                })
            }
            fn scratch(&self) -> &dyn Scratch {
                struct Drawer;
                impl Scratch for Drawer {
                    fn get(&self, _key: &str) -> Option<&str> {
                        None
                    }
                    fn set(&self, _key: &str, _value: String) {}
                }
                static DRAWER: Drawer = Drawer;
                &DRAWER
            }
        }
        let session = NoSession;
        let binding = Binding::bind(&session);
        assert!(binding.valid());
        assert!(binding.caps().is_empty());
        assert!(binding.caps().file_tree().is_none());
    }

    #[test]
    fn a_tool_that_needs_nothing_declares_a_static_empty_slice() {
        struct Plain;
        #[async_trait]
        impl Tool for Plain {
            fn definition(&self) -> ToolDefinition {
                ToolDefinition {
                    name: ToolName::new("plain").unwrap(),
                    description: String::new(),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(&self, _binding: Binding<'_>, _arguments: &str) -> Result<Content> {
                Ok(Content::text("ok"))
            }
        }
        assert!(Plain.needs().is_empty());
    }
}
