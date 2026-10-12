//! The bundle dependency graph: which bundles a run includes, why, and in what order.
//!
//! Resolution, in order:
//!
//! 1. **Selection**: bundles named on the command line, else the profile's (or `dot.yml`'s)
//!    `bundles:`, plus every `implicit: true` bundle.
//! 2. **Drop non-matching `when:`**: a selected bundle whose `when:` does not match is excluded
//!    (and reported); a *required* one is an error naming the chain that required it.
//! 3. **Expand** transitively over `requires`, active `requires_if` entries and `cap:<name>`
//!    requirements (the profile's `providers:` picks the bundle; a single matching provider is
//!    picked automatically).
//! 4. **Topological sort**, dependencies first, ties broken alphabetically; `after:` orders two
//!    included bundles without pulling either in.
//! 5. **Cycles** are errors naming the whole loop.
//!
//! Errors are collected, not returned at the first one, so `dot check` and `dot plan` can report
//! all of them.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use petgraph::Direction;
use petgraph::graphmap::DiGraphMap;

use crate::config::Bundle;
use crate::facts::Facts;
use crate::vars::Vars;

/// `requires: [cap:node]` depends on whichever bundle provides `node`.
pub const CAP_PREFIX: &str = "cap:";

/// Why a bundle is part of a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// Named on the command line.
    Requested,
    /// In a bundle selection; the string names it, e.g. `profile mac` or `dot.yml`.
    Selected(String),
    /// `implicit: true`.
    Implicit,
    /// In the `requires` (or an active `requires_if`) of this bundle.
    RequiredBy(String),
    /// Chosen to provide capability `cap`, which `by` requires.
    Provides { cap: String, by: String },
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reason::Requested => f.write_str("requested"),
            Reason::Selected(by) => write!(f, "selected by {by}"),
            Reason::Implicit => f.write_str("implicit"),
            Reason::RequiredBy(b) => write!(f, "required by {b}"),
            Reason::Provides { cap, by } => write!(f, "provides {cap} for {by}"),
        }
    }
}

impl Reason {
    /// The bundle that pulled this one in, if any.
    pub fn parent(&self) -> Option<&str> {
        match self {
            Reason::RequiredBy(b) | Reason::Provides { by: b, .. } => Some(b),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EdgeKind {
    Requires,
    /// `requires_if` whose `when:` matched.
    RequiresIf,
    /// `cap:<name>` resolved to this provider.
    Capability(String),
    /// Ordering only.
    After,
}

impl EdgeKind {
    /// Whether the dependent must not run when the dependency failed.
    pub fn is_hard(&self) -> bool {
        !matches!(self, EdgeKind::After)
    }

    pub fn label(&self) -> String {
        match self {
            EdgeKind::Requires => "requires".into(),
            EdgeKind::RequiresIf => "requires (conditional)".into(),
            EdgeKind::Capability(c) => format!("requires {CAP_PREFIX}{c}"),
            EdgeKind::After => "after".into(),
        }
    }
}

/// `from` depends on (or is ordered after) `to`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
}

/// One included bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub name: String,
    /// Every reason it is included; the first is the one found first.
    pub reasons: Vec<Reason>,
}

/// A bundle left out because its `when:` did not match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Excluded {
    pub name: String,
    /// Why it would have been included.
    pub reason: Reason,
    /// Why it does not match, e.g. `when: role is server, not desktop`.
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolution {
    /// Dependencies first.
    pub order: Vec<Resolved>,
    pub excluded: Vec<Excluded>,
    /// Edges between included bundles.
    pub edges: Vec<Edge>,
}

impl Resolution {
    pub fn names(&self) -> Vec<&str> {
        self.order.iter().map(|r| r.name.as_str()).collect()
    }

    pub fn get(&self, name: &str) -> Option<&Resolved> {
        self.order.iter().find(|r| r.name == name)
    }

    /// For each bundle in `order`, the positions of the bundles it hard-depends on.
    pub fn dependency_indices(&self) -> Vec<Vec<usize>> {
        self.edge_indices(|k| k.is_hard())
    }

    /// How the engine may run the bundles in `order`: every edge, `after` included (see
    /// [`crate::engine::Schedule`]).
    pub fn schedule(&self) -> crate::engine::Schedule {
        crate::engine::Schedule {
            requires: self.dependency_indices(),
            after: self.edge_indices(|k| !k.is_hard()),
        }
    }

    /// For each bundle in `order`, the positions of the bundles it has an edge of a kind
    /// `keep` accepts to.
    fn edge_indices(&self, keep: impl Fn(&EdgeKind) -> bool) -> Vec<Vec<usize>> {
        let pos: BTreeMap<&str, usize> = self
            .order
            .iter()
            .enumerate()
            .map(|(i, r)| (r.name.as_str(), i))
            .collect();
        self.order
            .iter()
            .map(|r| {
                let mut deps: Vec<usize> = self
                    .edges
                    .iter()
                    .filter(|e| e.from == r.name && keep(&e.kind))
                    .filter_map(|e| pos.get(e.to.as_str()).copied())
                    .collect();
                deps.sort_unstable();
                deps.dedup();
                deps
            })
            .collect()
    }

    /// The chain from a selection root down to `name`, e.g. `["dev", "nodejs", "common"]`,
    /// following the first reason of each bundle.
    pub fn chain(&self, name: &str) -> Vec<String> {
        let mut out = vec![name.to_string()];
        let mut seen = BTreeSet::from([name.to_string()]);
        let mut cur = name.to_string();
        while let Some(parent) = self
            .get(&cur)
            .and_then(|r| r.reasons.first())
            .and_then(|r| r.parent())
        {
            if !seen.insert(parent.to_string()) {
                break;
            }
            out.push(parent.to_string());
            cur = parent.to_string();
        }
        out.reverse();
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphError {
    /// `chain` is the path of bundles that led here, root first (empty for a selected name).
    UnknownBundle { name: String, chain: Vec<String> },
    NotMatching {
        name: String,
        detail: String,
        chain: Vec<String>,
    },
    NoProvider {
        cap: String,
        detail: String,
        chain: Vec<String>,
    },
    AmbiguousProvider {
        cap: String,
        candidates: Vec<String>,
        chain: Vec<String>,
    },
    BadProvider {
        cap: String,
        bundle: String,
        detail: String,
    },
    /// `a -> b -> a`: each bundle depends on (or is ordered after) the next.
    Cycle(Vec<String>),
    /// A `when:` that cannot be evaluated (undefined variable, ...).
    Config(String),
}

fn via(chain: &[String]) -> String {
    if chain.is_empty() {
        String::new()
    } else {
        format!(" (required via {})", chain.join(" -> "))
    }
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GraphError::UnknownBundle { name, chain } if chain.is_empty() => {
                write!(f, "unknown bundle `{name}`")
            }
            GraphError::UnknownBundle { name, chain } => {
                write!(f, "unknown bundle `{name}`{}", via(chain))
            }
            GraphError::NotMatching {
                name,
                detail,
                chain,
            } => write!(
                f,
                "bundle `{name}`{} does not match this machine: {detail}",
                via(chain)
            ),
            GraphError::NoProvider { cap, detail, chain } => {
                write!(f, "capability `{cap}`{}: {detail}", via(chain))
            }
            GraphError::AmbiguousProvider {
                cap,
                candidates,
                chain,
            } => write!(
                f,
                "capability `{cap}`{} has several providers ({}); choose one with \
                 `providers: {{ {cap}: <bundle> }}` in the profile",
                via(chain),
                candidates.join(", ")
            ),
            GraphError::BadProvider {
                cap,
                bundle,
                detail,
            } => {
                write!(f, "providers: `{cap}: {bundle}`: {detail}")
            }
            GraphError::Cycle(path) => write!(f, "dependency cycle: {}", path.join(" -> ")),
            GraphError::Config(e) => f.write_str(e),
        }
    }
}

/// What to resolve.
#[derive(Debug, Clone, Default)]
pub struct Selection {
    pub roots: Vec<(String, Reason)>,
    /// Add every `implicit: true` bundle that matches.
    pub implicit: bool,
    /// Follow `requires` (false for `--no-deps`).
    pub expand: bool,
}

/// The bundles of a repo, and what resolution needs to evaluate `when:` and capabilities.
pub struct Graph<'a> {
    pub bundles: &'a BTreeMap<String, Bundle>,
    pub facts: &'a Facts,
    /// Variables for rendering templated `when:` values (each bundle adds its own layer).
    pub vars: &'a Vars,
    /// Capability -> chosen provider (the profile's, over `dot.yml`'s).
    pub providers: &'a BTreeMap<String, String>,
}

/// Why a bundle is (not) applicable: `Err` is a config error, `Ok(Err)` a non-match.
type Match = Result<Result<(), String>, String>;

impl Graph<'_> {
    fn matches(&self, name: &str) -> Match {
        let Some(b) = self.bundles.get(name) else {
            return Ok(Ok(()));
        };
        let Some(when) = &b.manifest.when else {
            return Ok(Ok(()));
        };
        let vars = self.vars.with_bundle(b);
        let w = when
            .render(&vars)
            .map_err(|e| format!("{}: when: {e}", b.file.display()))?;
        Ok(w.eval(self.facts).map_err(|r| format!("when: {r}")))
    }

    /// Bundles providing `cap`, sorted.
    pub fn providers_of(&self, cap: &str) -> Vec<String> {
        self.bundles
            .values()
            .filter(|b| b.manifest.provides.iter().any(|p| p == cap))
            .map(|b| b.name.clone())
            .collect()
    }

    /// Picks the provider of `cap`.
    fn provider(&self, cap: &str, chain: &[String]) -> Result<String, GraphError> {
        let candidates = self.providers_of(cap);
        if let Some(chosen) = self.providers.get(cap) {
            if !self.bundles.contains_key(chosen) {
                return Err(GraphError::BadProvider {
                    cap: cap.into(),
                    bundle: chosen.clone(),
                    detail: format!("no bundle named `{chosen}`"),
                });
            }
            if !candidates.contains(chosen) {
                return Err(GraphError::BadProvider {
                    cap: cap.into(),
                    bundle: chosen.clone(),
                    detail: format!("bundle `{chosen}` does not provide `{cap}`"),
                });
            }
            return Ok(chosen.clone());
        }
        if candidates.is_empty() {
            return Err(GraphError::NoProvider {
                cap: cap.into(),
                detail: "no bundle provides it".into(),
                chain: chain.to_vec(),
            });
        }
        let mut matching = Vec::new();
        let mut reasons = Vec::new();
        for c in &candidates {
            match self.matches(c).map_err(GraphError::Config)? {
                Ok(()) => matching.push(c.clone()),
                Err(why) => reasons.push(format!("{c}: {why}")),
            }
        }
        match matching.len() {
            1 => Ok(matching.remove(0)),
            0 => Err(GraphError::NoProvider {
                cap: cap.into(),
                detail: format!("no provider matches this machine ({})", reasons.join("; ")),
                chain: chain.to_vec(),
            }),
            _ => Err(GraphError::AmbiguousProvider {
                cap: cap.into(),
                candidates: matching,
                chain: chain.to_vec(),
            }),
        }
    }

    /// The hard dependencies of `name` on this machine: `requires`, active `requires_if`, and
    /// capabilities resolved to their providers. Also returns the problems found on the way
    /// (the dependencies that could be resolved are still listed).
    fn hard_deps(
        &self,
        name: &str,
        chain: &[String],
    ) -> (Vec<(String, EdgeKind)>, Vec<GraphError>) {
        let Some(b) = self.bundles.get(name) else {
            return (Vec::new(), Vec::new());
        };
        let mut wanted: Vec<(String, EdgeKind)> = b
            .manifest
            .requires
            .iter()
            .map(|r| (r.clone(), EdgeKind::Requires))
            .collect();
        let mut errors = Vec::new();
        if !b.manifest.requires_if.is_empty() {
            let vars = self.vars.with_bundle(b);
            for (when, reqs) in &b.manifest.requires_if {
                match when.render(&vars) {
                    Err(e) => errors.push(GraphError::Config(format!(
                        "{}: requires_if: when: {e}",
                        b.file.display()
                    ))),
                    Ok(w) if w.matches(self.facts) => {
                        wanted.extend(reqs.iter().map(|r| (r.clone(), EdgeKind::RequiresIf)))
                    }
                    Ok(_) => {}
                }
            }
        }
        let mut out = Vec::new();
        for (req, kind) in wanted {
            match req.strip_prefix(CAP_PREFIX) {
                Some(cap) => match self.provider(cap, chain) {
                    Ok(p) => out.push((p, EdgeKind::Capability(cap.to_string()))),
                    Err(e) => errors.push(e),
                },
                None => out.push((req, kind)),
            }
        }
        (out, errors)
    }

    pub fn resolve(&self, sel: &Selection) -> Result<Resolution, Vec<GraphError>> {
        let mut errors = Vec::new();
        let mut excluded = Vec::new();
        let mut queue: VecDeque<(String, Reason, Vec<String>)> = VecDeque::new();

        let mut roots = sel.roots.clone();
        if sel.implicit {
            roots.extend(
                self.bundles
                    .values()
                    .filter(|b| b.manifest.implicit)
                    .map(|b| (b.name.clone(), Reason::Implicit)),
            );
        }
        for (name, reason) in roots {
            if !self.bundles.contains_key(&name) {
                errors.push(GraphError::UnknownBundle {
                    name,
                    chain: Vec::new(),
                });
                continue;
            }
            match self.matches(&name) {
                Err(e) => errors.push(GraphError::Config(e)),
                // Named on the command line: silently applying nothing would hide the mistake.
                Ok(Err(detail)) if reason == Reason::Requested => {
                    errors.push(GraphError::NotMatching {
                        name,
                        detail,
                        chain: Vec::new(),
                    })
                }
                Ok(Err(detail)) => {
                    if !excluded.iter().any(|e: &Excluded| e.name == name) {
                        excluded.push(Excluded {
                            name,
                            reason,
                            detail,
                        });
                    }
                }
                Ok(Ok(())) => queue.push_back((name, reason, Vec::new())),
            }
        }

        let mut included: BTreeMap<String, Vec<Reason>> = BTreeMap::new();
        let mut edges: Vec<Edge> = Vec::new();
        while let Some((name, reason, chain)) = queue.pop_front() {
            if let Some(reasons) = included.get_mut(&name) {
                if !reasons.contains(&reason) {
                    reasons.push(reason);
                }
                continue;
            }
            included.insert(name.clone(), vec![reason]);
            let mut next_chain = chain.clone();
            next_chain.push(name.clone());
            let (deps, problems) = self.hard_deps(&name, &next_chain);
            if sel.expand {
                errors.extend(problems);
            }
            for (dep, kind) in deps {
                edges.push(Edge {
                    from: name.clone(),
                    to: dep.clone(),
                    kind: kind.clone(),
                });
                if !sel.expand {
                    continue;
                }
                if !self.bundles.contains_key(&dep) {
                    errors.push(GraphError::UnknownBundle {
                        name: dep,
                        chain: next_chain.clone(),
                    });
                    continue;
                }
                match self.matches(&dep) {
                    Err(e) => errors.push(GraphError::Config(e)),
                    Ok(Err(detail)) => errors.push(GraphError::NotMatching {
                        name: dep,
                        detail,
                        chain: next_chain.clone(),
                    }),
                    Ok(Ok(())) => {
                        let reason = match &kind {
                            EdgeKind::Capability(cap) => Reason::Provides {
                                cap: cap.clone(),
                                by: name.clone(),
                            },
                            _ => Reason::RequiredBy(name.clone()),
                        };
                        queue.push_back((dep, reason, next_chain.clone()));
                    }
                }
            }
        }
        // Ordering edges and, without expansion, only edges between included bundles.
        edges.retain(|e| included.contains_key(&e.to));
        for name in included.keys() {
            if let Some(b) = self.bundles.get(name) {
                for a in &b.manifest.after {
                    // `after: [cap:x]` orders after every included bundle providing `x`.
                    let targets = match a.strip_prefix(CAP_PREFIX) {
                        Some(cap) => self.providers_of(cap),
                        None => vec![a.clone()],
                    };
                    for t in targets {
                        if t != *name && included.contains_key(&t) {
                            edges.push(Edge {
                                from: name.clone(),
                                to: t,
                                kind: EdgeKind::After,
                            });
                        }
                    }
                }
            }
        }
        excluded.retain(|e| !included.contains_key(&e.name));

        let names: Vec<&str> = included.keys().map(String::as_str).collect();
        let order = match topo_sort(&names, &edges) {
            Ok(o) => o,
            Err(cycles) => {
                errors.extend(cycles.into_iter().map(GraphError::Cycle));
                Vec::new()
            }
        };
        if !errors.is_empty() {
            dedup(&mut errors);
            return Err(errors);
        }
        let order = order
            .into_iter()
            .map(|n| Resolved {
                reasons: included.remove(&n).unwrap_or_default(),
                name: n,
            })
            .collect();
        Ok(Resolution {
            order,
            excluded,
            edges,
        })
    }
}

fn dedup(errors: &mut Vec<GraphError>) {
    let mut seen = Vec::new();
    errors.retain(|e| {
        let s = e.to_string();
        if seen.contains(&s) {
            false
        } else {
            seen.push(s);
            true
        }
    });
}

/// Dependencies first, ties alphabetical. On failure, every cycle found, each as a closed path
/// (`[a, b, a]`) starting at its alphabetically first bundle.
pub fn topo_sort(nodes: &[&str], edges: &[Edge]) -> Result<Vec<String>, Vec<Vec<String>>> {
    // Edge dependency -> dependent, so a topological order lists dependencies first.
    let mut g: DiGraphMap<&str, ()> = DiGraphMap::new();
    for n in nodes {
        g.add_node(n);
    }
    for e in edges {
        if g.contains_node(&e.from) && g.contains_node(&e.to) {
            g.add_edge(e.to.as_str(), e.from.as_str(), ());
        }
    }
    let mut indegree: BTreeMap<&str, usize> = g
        .nodes()
        .map(|n| (n, g.neighbors_directed(n, Direction::Incoming).count()))
        .collect();
    let mut ready: BTreeSet<&str> = indegree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(n, _)| *n)
        .collect();
    let mut order = Vec::new();
    while let Some(n) = ready.pop_first() {
        order.push(n.to_string());
        for m in g.neighbors_directed(n, Direction::Outgoing) {
            let d = indegree.get_mut(m).expect("node has a degree");
            *d -= 1;
            if *d == 0 {
                ready.insert(m);
            }
        }
    }
    if order.len() == g.node_count() {
        return Ok(order);
    }
    Err(cycles(&g))
}

/// One closed path per strongly connected component that contains a cycle, written in the
/// "depends on" direction.
fn cycles(g: &DiGraphMap<&str, ()>) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for scc in petgraph::algo::tarjan_scc(g) {
        let members: BTreeSet<&str> = scc.iter().copied().collect();
        let start = *members.first().expect("non-empty component");
        let self_loop = g.contains_edge(start, start);
        if members.len() == 1 && !self_loop {
            continue;
        }
        // Walk "depends on" edges (reverse of the graph's) inside the component back to start.
        let mut path = vec![start];
        let mut visited = BTreeSet::from([start]);
        if find_back(g, &members, start, start, &mut path, &mut visited) {
            out.push(path.iter().map(|s| s.to_string()).collect());
        }
    }
    out.sort();
    out
}

fn find_back<'a>(
    g: &DiGraphMap<&'a str, ()>,
    members: &BTreeSet<&'a str>,
    start: &'a str,
    at: &'a str,
    path: &mut Vec<&'a str>,
    visited: &mut BTreeSet<&'a str>,
) -> bool {
    let mut next: Vec<&str> = g
        .neighbors_directed(at, Direction::Incoming)
        .filter(|n| members.contains(n))
        .collect();
    next.sort_unstable();
    for n in next {
        if n == start {
            path.push(n);
            return true;
        }
        if visited.insert(n) {
            path.push(n);
            if find_back(g, members, start, n, path, visited) {
                return true;
            }
            path.pop();
        }
    }
    false
}

/// One problem found by [`check_all`]: the bundle it was found in (`None` for cycles) and the
/// manifest key holding the bad reference (`requires`, `requires_if` or `after`), so `dot check`
/// can point at the right line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckProblem {
    pub bundle: Option<String>,
    pub key: Option<&'static str>,
    pub error: GraphError,
}

/// Host-independent checks over every bundle, for `dot check`: references to unknown bundles
/// or capabilities nobody provides, and cycles (over every edge, whatever the `when:`s).
pub fn check_all(bundles: &BTreeMap<String, Bundle>) -> Vec<CheckProblem> {
    let mut out = Vec::new();
    let mut edges = Vec::new();
    let provides = |cap: &str| -> Vec<String> {
        bundles
            .values()
            .filter(|b| b.manifest.provides.iter().any(|p| p == cap))
            .map(|b| b.name.clone())
            .collect()
    };
    for b in bundles.values() {
        let mut refs: Vec<(&String, EdgeKind, &'static str)> = b
            .manifest
            .requires
            .iter()
            .map(|r| (r, EdgeKind::Requires, "requires"))
            .collect();
        for (_, reqs) in &b.manifest.requires_if {
            refs.extend(
                reqs.iter()
                    .map(|r| (r, EdgeKind::RequiresIf, "requires_if")),
            );
        }
        refs.extend(
            b.manifest
                .after
                .iter()
                .map(|r| (r, EdgeKind::After, "after")),
        );
        for (r, kind, key) in refs {
            let ordering = kind == EdgeKind::After;
            let chain = vec![b.name.clone()];
            let problem = |error: GraphError| CheckProblem {
                bundle: Some(b.name.clone()),
                key: Some(key),
                error,
            };
            match r.strip_prefix(CAP_PREFIX) {
                Some(cap) => {
                    let providers = provides(cap);
                    if providers.is_empty() {
                        out.push(problem(if ordering {
                            GraphError::Config(format!(
                                "after: capability `{cap}` (in {}): no bundle provides it",
                                b.name
                            ))
                        } else {
                            GraphError::NoProvider {
                                cap: cap.into(),
                                detail: "no bundle provides it".into(),
                                chain,
                            }
                        }));
                    }
                    for p in providers {
                        edges.push(Edge {
                            from: b.name.clone(),
                            to: p,
                            kind: if ordering {
                                EdgeKind::After
                            } else {
                                EdgeKind::Capability(cap.into())
                            },
                        });
                    }
                }
                None if !bundles.contains_key(r) => out.push(problem(if ordering {
                    GraphError::Config(format!("after: unknown bundle `{r}` (in {})", b.name))
                } else {
                    GraphError::UnknownBundle {
                        name: r.clone(),
                        chain,
                    }
                })),
                None => edges.push(Edge {
                    from: b.name.clone(),
                    to: r.clone(),
                    kind,
                }),
            }
        }
    }
    let names: Vec<&str> = bundles.keys().map(String::as_str).collect();
    if let Err(cycles) = topo_sort(&names, &edges) {
        out.extend(cycles.into_iter().map(|c| CheckProblem {
            bundle: None,
            key: None,
            error: GraphError::Cycle(c),
        }));
    }
    out
}

#[cfg(test)]
mod tests;
